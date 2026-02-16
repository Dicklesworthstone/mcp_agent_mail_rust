//! Unified search service bridging the query planner, SQL execution, and scope enforcement.
//!
//! This module provides [`execute_search`] — the single entry point for all search
//! operations across TUI, MCP tools, and web surfaces. It:
//!
//! 1. Plans the query via [`plan_search`]
//! 2. Executes the resulting SQL against the database
//! 3. Applies scope and redaction via [`apply_scope`]
//! 4. Tracks query telemetry
//! 5. Returns a rich [`SearchResponse`] with pagination, explain, and audit

use crate::error::DbError;
use crate::pool::DbPool;
use crate::search_planner::{
    DocKind, PlanMethod, PlanParam, SearchCursor, SearchQuery, SearchResponse, SearchResult,
    plan_search,
};
use crate::search_scope::{
    RedactionPolicy, ScopeAuditSummary, ScopeContext, ScopedSearchResult, apply_scope,
};
use crate::tracking::record_query;
use mcp_agent_mail_core::config::SearchEngine;
use mcp_agent_mail_core::metrics::global_metrics;
use mcp_agent_mail_core::{EvidenceLedgerEntry, append_evidence_entry_if_configured};

use asupersync::{Budget, Cx, Outcome, Time};
#[cfg(feature = "hybrid")]
use half::f16;
use mcp_agent_mail_search_core::{
    CandidateBudget, CandidateBudgetConfig, CandidateBudgetDecision, CandidateBudgetDerivation,
    CandidateHit, CandidateMode, CandidateStageCounts, QueryAssistance, QueryClass,
    parse_query_assistance, prepare_candidates,
};
#[cfg(feature = "hybrid")]
use mcp_agent_mail_search_core::{
    DocKind as SearchDocKind, Embedder, EmbeddingJobConfig, EmbeddingJobRunner, EmbeddingQueue,
    EmbeddingRequest, EmbeddingResult, FsScoredResult, HashEmbedder, JobMetricsSnapshot, ModelInfo,
    ModelRegistry, ModelTier, QueueStats, RefreshWorkerConfig, RegistryConfig, ScoredResult,
    SearchPhase, TwoTierAvailability, TwoTierConfig, TwoTierEntry, TwoTierIndex, VectorFilter,
    VectorIndex, VectorIndexConfig, fs, get_two_tier_context,
};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row as SqlRow, Value};
use sqlmodel_query::raw_query;
#[cfg(feature = "hybrid")]
use std::collections::BTreeMap;
#[cfg(feature = "hybrid")]
use std::path::PathBuf;
#[cfg(feature = "hybrid")]
use std::sync::{Arc, Mutex, OnceLock, RwLock};
// ────────────────────────────────────────────────────────────────────
// Search service options
// ────────────────────────────────────────────────────────────────────

/// Options for search execution beyond what `SearchQuery` carries.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Scope context for permission enforcement. `None` = operator mode.
    pub scope_ctx: Option<ScopeContext>,
    /// Redaction policy for scope-filtered results. Defaults to standard.
    pub redaction_policy: Option<RedactionPolicy>,
    /// Whether to emit telemetry events for this query.
    pub track_telemetry: bool,
    /// Search engine override. `None` = use global config default.
    pub search_engine: Option<SearchEngine>,
}

// ────────────────────────────────────────────────────────────────────
// Search response types
// ────────────────────────────────────────────────────────────────────

/// Full search response including scope audit information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopedSearchResponse {
    /// Visible results (after scope filtering + redaction).
    pub results: Vec<ScopedSearchResult>,
    /// Pagination cursor for next page.
    pub next_cursor: Option<String>,
    /// Query explain metadata (when requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<crate::search_planner::QueryExplain>,
    /// Audit summary of scope enforcement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_summary: Option<ScopeAuditSummary>,
    /// Total rows returned from SQL before scope filtering.
    pub sql_row_count: usize,
    /// Query-assistance metadata (`did_you_mean`, parsed hint tokens, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistance: Option<QueryAssistance>,
}

/// Lightweight response for simple (unscoped) searches.
pub type SimpleSearchResponse = SearchResponse;

// ────────────────────────────────────────────────────────────────────
// SQL parameter conversion
// ────────────────────────────────────────────────────────────────────

fn plan_param_to_value(param: &PlanParam) -> Value {
    match param {
        PlanParam::Int(v) => Value::BigInt(*v),
        PlanParam::Text(s) => Value::Text(s.clone()),
        PlanParam::Float(f) => Value::Double(*f),
    }
}

// ────────────────────────────────────────────────────────────────────
// Internal helpers
// ────────────────────────────────────────────────────────────────────

fn map_sql_error(e: &sqlmodel_core::Error) -> DbError {
    DbError::Sqlite(e.to_string())
}

fn map_sql_outcome<T>(out: Outcome<T, sqlmodel_core::Error>) -> Outcome<T, DbError> {
    match out {
        Outcome::Ok(v) => Outcome::Ok(v),
        Outcome::Err(e) => Outcome::Err(map_sql_error(&e)),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

fn query_assistance_payload(query: &SearchQuery) -> Option<QueryAssistance> {
    let assistance = parse_query_assistance(&query.text);
    if assistance.applied_filter_hints.is_empty() && assistance.did_you_mean.is_empty() {
        None
    } else {
        Some(assistance)
    }
}

async fn acquire_conn(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<sqlmodel_pool::PooledConnection<crate::DbConn>, DbError> {
    map_sql_outcome(pool.acquire(cx).await)
}

// ────────────────────────────────────────────────────────────────────
// Tantivy routing helpers
// ────────────────────────────────────────────────────────────────────

/// Try executing a search via the Tantivy bridge. Returns `None` if the
/// bridge is not initialized (`init_bridge` not called).
fn try_tantivy_search(query: &SearchQuery) -> Option<Vec<SearchResult>> {
    let bridge = crate::search_v3::get_bridge()?;
    Some(bridge.search(query))
}

// ────────────────────────────────────────────────────────────────────
// Semantic search bridge (vector index + embedder)
// ────────────────────────────────────────────────────────────────────

#[cfg(feature = "hybrid")]
static SEMANTIC_BRIDGE: OnceLock<Option<Arc<SemanticBridge>>> = OnceLock::new();

#[cfg(feature = "hybrid")]
#[derive(Debug)]
struct AutoInitSemanticEmbedder {
    info: ModelInfo,
    hash_fallback: HashEmbedder,
}

#[cfg(feature = "hybrid")]
impl AutoInitSemanticEmbedder {
    fn new() -> Self {
        let dimension = get_two_tier_context().config().fast_dimension;
        Self {
            info: ModelInfo::new(
                "auto-init-semantic-fast",
                "Auto-Init Semantic Fast",
                ModelTier::Fast,
                dimension,
                4096,
            )
            .with_available(true),
            hash_fallback: HashEmbedder::new(),
        }
    }
}

#[cfg(feature = "hybrid")]
impl Embedder for AutoInitSemanticEmbedder {
    fn embed(
        &self,
        text: &str,
    ) -> mcp_agent_mail_search_core::error::SearchResult<EmbeddingResult> {
        let ctx = get_two_tier_context();
        let start = std::time::Instant::now();
        if let Ok(vector) = ctx.embed_fast(text) {
            return Ok(EmbeddingResult::new(
                vector,
                self.info.id.clone(),
                ModelTier::Fast,
                start.elapsed(),
                mcp_agent_mail_search_core::canonical::content_hash(text),
            ));
        }
        if let Ok(vector) = ctx.embed_quality(text) {
            return Ok(EmbeddingResult::new(
                vector,
                "auto-init-semantic-quality".to_string(),
                ModelTier::Quality,
                start.elapsed(),
                mcp_agent_mail_search_core::canonical::content_hash(text),
            ));
        }
        self.hash_fallback.embed(text)
    }

    fn model_info(&self) -> &ModelInfo {
        &self.info
    }
}

/// Bridge to the semantic search infrastructure (vector index + embedder).
#[cfg(feature = "hybrid")]
pub struct SemanticBridge {
    /// The vector index holding document embeddings.
    index: Arc<RwLock<VectorIndex>>,
    /// The model registry for obtaining embedders.
    registry: Arc<RwLock<ModelRegistry>>,
    /// Queue of pending embedding work.
    queue: Arc<EmbeddingQueue>,
    /// Batch runner for embedding/index updates.
    runner: Arc<EmbeddingJobRunner>,
    /// Background refresh worker.
    refresh_worker: Arc<mcp_agent_mail_search_core::IndexRefreshWorker>,
    /// Background refresh worker handle.
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[cfg(feature = "hybrid")]
impl SemanticBridge {
    /// Create a new semantic bridge with the given configuration.
    #[must_use]
    pub fn new(config: VectorIndexConfig) -> Self {
        Self::new_with_embedder(config, Arc::new(AutoInitSemanticEmbedder::new()))
    }

    #[must_use]
    fn new_with_embedder(config: VectorIndexConfig, embedder: Arc<dyn Embedder>) -> Self {
        let index = Arc::new(RwLock::new(VectorIndex::new(config)));
        let registry = Arc::new(RwLock::new(ModelRegistry::new(RegistryConfig::default())));
        let job_config = EmbeddingJobConfig::default();
        let queue = Arc::new(EmbeddingQueue::with_config(job_config.clone()));
        let runner = Arc::new(EmbeddingJobRunner::new(
            job_config,
            queue.clone(),
            embedder,
            index.clone(),
        ));
        let worker_cfg = RefreshWorkerConfig {
            refresh_interval_ms: 250,
            rebuild_on_startup: false,
            max_docs_per_cycle: 256,
        };
        let refresh_worker = Arc::new(mcp_agent_mail_search_core::IndexRefreshWorker::new(
            worker_cfg,
            runner.clone(),
        ));
        let worker = {
            let worker = refresh_worker.clone();
            std::thread::Builder::new()
                .name("semantic-index-refresh".to_string())
                .spawn(move || worker.run())
                .ok()
        };

        Self {
            index,
            registry,
            queue,
            runner,
            refresh_worker,
            worker: Mutex::new(worker),
        }
    }

    /// Create a semantic bridge with default configuration (384-dim for `MiniLM`).
    #[must_use]
    pub fn default_config() -> Self {
        Self::new(VectorIndexConfig::default())
    }

    /// Get a reference to the vector index (for reads).
    pub fn index(&self) -> std::sync::RwLockReadGuard<'_, VectorIndex> {
        self.index.read().expect("vector index lock poisoned")
    }

    /// Get a mutable reference to the vector index (for writes).
    pub fn index_mut(&self) -> std::sync::RwLockWriteGuard<'_, VectorIndex> {
        self.index.write().expect("vector index lock poisoned")
    }

    /// Get a reference to the model registry.
    pub fn registry(&self) -> std::sync::RwLockReadGuard<'_, ModelRegistry> {
        self.registry.read().expect("model registry lock poisoned")
    }

    /// Get a mutable reference to the model registry (for registering embedders).
    pub fn registry_mut(&self) -> std::sync::RwLockWriteGuard<'_, ModelRegistry> {
        self.registry.write().expect("model registry lock poisoned")
    }

    /// Check if the bridge has any real embedder (beyond hash fallback).
    #[must_use]
    pub fn has_real_embedder(&self) -> bool {
        self.registry().has_real_embedder() || get_two_tier_context().is_available()
    }

    /// Search for semantically similar documents.
    ///
    /// Embeds the query text and performs vector similarity search.
    pub fn search(&self, query: &SearchQuery, limit: usize) -> Vec<SearchResult> {
        let embedder = AutoInitSemanticEmbedder::new();
        let embedding = match embedder.embed(&query.text) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                    target: "search.semantic",
                    error = %e,
                    "failed to embed query"
                );
                return Vec::new();
            }
        };
        if embedding.is_hash_only() {
            tracing::debug!(
                target: "search.semantic",
                "no real embedder available, skipping semantic search"
            );
            return Vec::new();
        }

        // Build filter from query
        let filter = build_vector_filter(query);

        // Search the index
        let index = self.index();
        let hits = match index.search(&embedding.vector, limit, Some(&filter)) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    target: "search.semantic",
                    error = %e,
                    "vector search failed"
                );
                return Vec::new();
            }
        };
        drop(index);

        // Convert to SearchResult
        hits.into_iter()
            .map(|hit| SearchResult {
                doc_kind: convert_doc_kind(hit.doc_kind),
                id: hit.doc_id,
                project_id: hit.project_id,
                title: String::new(), // Vector index doesn't store full docs
                body: String::new(),
                score: Some(f64::from(hit.score)),
                importance: None,
                ack_required: None,
                created_ts: None,
                thread_id: None,
                from_agent: None,
                reason_codes: Vec::new(),
                score_factors: Vec::new(),
                redacted: false,
                redaction_reason: None,
            })
            .collect()
    }

    /// Enqueue a document for background semantic indexing.
    pub fn enqueue_document(
        &self,
        doc_id: i64,
        doc_kind: SearchDocKind,
        project_id: Option<i64>,
        title: &str,
        body: &str,
    ) -> bool {
        self.queue.enqueue(EmbeddingRequest::new(
            doc_id,
            doc_kind,
            project_id,
            title,
            body,
            ModelTier::Fast,
        ))
    }

    #[must_use]
    pub fn queue_stats(&self) -> QueueStats {
        self.queue.stats()
    }

    #[must_use]
    pub fn metrics_snapshot(&self) -> JobMetricsSnapshot {
        self.runner.metrics().snapshot()
    }
}

#[cfg(feature = "hybrid")]
impl Drop for SemanticBridge {
    fn drop(&mut self) {
        self.refresh_worker.shutdown();
        let join = self.worker.lock().expect("worker lock poisoned").take();
        if let Some(join) = join {
            let _ = join.join();
        }
    }
}

#[cfg(feature = "hybrid")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticIndexingSnapshot {
    pub queue: QueueStats,
    pub metrics: JobMetricsSnapshot,
}

#[cfg(feature = "hybrid")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticIndexingHealth {
    pub queue: QueueStats,
    pub metrics: JobMetricsSnapshot,
}

#[cfg(not(feature = "hybrid"))]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SemanticIndexingHealth {}

#[cfg(feature = "hybrid")]
fn get_or_init_semantic_bridge() -> Option<Arc<SemanticBridge>> {
    // Use OnceLock::get_or_init for atomic, race-free initialization.
    // Only one SemanticBridge is created even under concurrent access.
    SEMANTIC_BRIDGE
        .get_or_init(|| Some(Arc::new(SemanticBridge::default_config())))
        .clone()
}

/// Build a `VectorFilter` from a `SearchQuery`.
#[cfg(feature = "hybrid")]
fn build_vector_filter(query: &SearchQuery) -> VectorFilter {
    let mut filter = VectorFilter::new();

    if let Some(pid) = query.project_id {
        filter = filter.with_project(pid);
    }

    let doc_kinds = vec![match query.doc_kind {
        DocKind::Message => SearchDocKind::Message,
        DocKind::Agent => SearchDocKind::Agent,
        DocKind::Project => SearchDocKind::Project,
        DocKind::Thread => SearchDocKind::Thread,
    }];
    filter = filter.with_doc_kinds(doc_kinds);
    filter
}

/// Convert search-core `DocKind` to planner `DocKind`.
#[cfg(feature = "hybrid")]
const fn convert_doc_kind(kind: SearchDocKind) -> DocKind {
    match kind {
        SearchDocKind::Message => DocKind::Message,
        SearchDocKind::Agent => DocKind::Agent,
        SearchDocKind::Project => DocKind::Project,
        SearchDocKind::Thread => DocKind::Thread,
    }
}

#[cfg(feature = "hybrid")]
fn scored_results_to_search_results(hits: Vec<ScoredResult>) -> Vec<SearchResult> {
    hits.into_iter()
        .map(|hit| SearchResult {
            doc_kind: convert_doc_kind(hit.doc_kind),
            id: i64::try_from(hit.doc_id).unwrap_or(i64::MAX),
            project_id: hit.project_id,
            title: String::new(),
            body: String::new(),
            score: Some(f64::from(hit.score)),
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        })
        .collect()
}

#[cfg(feature = "hybrid")]
fn select_best_two_tier_results<I>(phases: I) -> Option<Vec<ScoredResult>>
where
    I: IntoIterator<Item = SearchPhase>,
{
    let mut best: Option<Vec<ScoredResult>> = None;

    for phase in phases {
        match phase {
            SearchPhase::Initial { results, .. } => {
                if best.is_none() {
                    best = Some(results);
                }
            }
            SearchPhase::Refined { results, .. } => {
                // Keep the initial phase when refinement yields an empty set.
                if !results.is_empty() || best.is_none() {
                    best = Some(results);
                }
            }
            SearchPhase::RefinementFailed { error } => {
                tracing::debug!(
                    target: "search.semantic",
                    error = %error,
                    "two-tier refinement failed; keeping the best available phase"
                );
            }
        }
    }

    best
}

#[cfg(feature = "hybrid")]
fn select_fast_first_two_tier_results<I>(phases: I) -> Option<Vec<ScoredResult>>
where
    I: IntoIterator<Item = SearchPhase>,
{
    for phase in phases {
        match phase {
            SearchPhase::Initial { results, .. } | SearchPhase::Refined { results, .. } => {
                if !results.is_empty() {
                    return Some(results);
                }
            }
            SearchPhase::RefinementFailed { error } => {
                tracing::debug!(
                    target: "search.semantic",
                    error = %error,
                    "two-tier refinement failed during fast-first selection"
                );
            }
        }
    }

    None
}

#[cfg(feature = "hybrid")]
const fn convert_planner_doc_kind(kind: DocKind) -> SearchDocKind {
    match kind {
        DocKind::Message => SearchDocKind::Message,
        DocKind::Agent => SearchDocKind::Agent,
        DocKind::Project => SearchDocKind::Project,
        DocKind::Thread => SearchDocKind::Thread,
    }
}

/// Initialize the global semantic search bridge.
///
/// Should be called once at startup when hybrid search is enabled.
#[cfg(feature = "hybrid")]
pub fn init_semantic_bridge(config: VectorIndexConfig) -> Result<(), String> {
    let bridge = SemanticBridge::new(config);
    if SEMANTIC_BRIDGE.set(Some(Arc::new(bridge))).is_err() {
        return Err(
            "semantic bridge is already initialized; restart process to apply a new config"
                .to_string(),
        );
    }
    Ok(())
}

/// Initialize the global semantic bridge with default configuration.
#[cfg(feature = "hybrid")]
pub fn init_semantic_bridge_default() -> Result<(), String> {
    init_semantic_bridge(VectorIndexConfig::default())
}

/// Get the global semantic bridge, if initialized.
#[cfg(feature = "hybrid")]
pub fn get_semantic_bridge() -> Option<Arc<SemanticBridge>> {
    SEMANTIC_BRIDGE.get().and_then(std::clone::Clone::clone)
}

/// Enqueue a document for background semantic indexing.
#[cfg(feature = "hybrid")]
#[must_use]
pub fn enqueue_semantic_document(
    doc_kind: DocKind,
    doc_id: i64,
    project_id: Option<i64>,
    title: &str,
    body: &str,
) -> bool {
    let Some(bridge) = get_or_init_semantic_bridge() else {
        return false;
    };
    bridge.enqueue_document(
        doc_id,
        convert_planner_doc_kind(doc_kind),
        project_id,
        title,
        body,
    )
}

#[cfg(not(feature = "hybrid"))]
#[must_use]
pub fn enqueue_semantic_document(
    _doc_kind: DocKind,
    _doc_id: i64,
    _project_id: Option<i64>,
    _title: &str,
    _body: &str,
) -> bool {
    false
}

/// Snapshot current semantic indexing queue + metrics.
#[cfg(feature = "hybrid")]
#[must_use]
pub fn semantic_indexing_snapshot() -> Option<SemanticIndexingSnapshot> {
    let bridge = get_or_init_semantic_bridge()?;
    Some(SemanticIndexingSnapshot {
        queue: bridge.queue_stats(),
        metrics: bridge.metrics_snapshot(),
    })
}

#[cfg(not(feature = "hybrid"))]
#[must_use]
pub const fn semantic_indexing_snapshot() -> Option<()> {
    None
}

/// Snapshot current semantic indexing queue + metrics in a stable health format.
#[cfg(feature = "hybrid")]
#[must_use]
pub fn semantic_indexing_health() -> Option<SemanticIndexingHealth> {
    semantic_indexing_snapshot().map(|snapshot| SemanticIndexingHealth {
        queue: snapshot.queue,
        metrics: snapshot.metrics,
    })
}

#[cfg(not(feature = "hybrid"))]
#[must_use]
pub const fn semantic_indexing_health() -> Option<SemanticIndexingHealth> {
    None
}

// ────────────────────────────────────────────────────────────────────
// Two-Tier Semantic Bridge (auto-initialized, no manual setup)
// ────────────────────────────────────────────────────────────────────

#[cfg(feature = "hybrid")]
static TWO_TIER_BRIDGE: OnceLock<Option<Arc<TwoTierBridge>>> = OnceLock::new();
#[cfg(feature = "hybrid")]
static HYBRID_RERANKER: OnceLock<Option<Arc<fs::FlashRankReranker>>> = OnceLock::new();

/// Bridge to the two-tier progressive semantic search system.
///
/// Uses automatic embedder detection and initialization:
/// - Fast tier: potion-128M (sub-ms, from `HuggingFace` cache)
/// - Quality tier: `MiniLM-L6-v2` (128ms, via `FastEmbed`)
///
/// No manual setup required - embedders are auto-detected on first use.
#[cfg(feature = "hybrid")]
pub struct TwoTierBridge {
    /// The two-tier index holding document embeddings.
    index: RwLock<TwoTierIndex>,
    /// Configuration (derived from auto-detected embedders).
    config: TwoTierConfig,
}

#[cfg(feature = "hybrid")]
impl TwoTierBridge {
    /// Create a new two-tier bridge using auto-detected configuration.
    ///
    /// This automatically detects available embedders and creates appropriate
    /// configuration. No manual model loading required.
    #[must_use]
    pub fn new() -> Self {
        let ctx = get_two_tier_context();
        let config = ctx.config().clone();
        let index = ctx.create_index();

        tracing::info!(
            availability = %ctx.availability(),
            fast_model = ?ctx.fast_info().map(|i| &i.id),
            quality_model = ?ctx.quality_info().map(|i| &i.id),
            "Two-tier semantic bridge initialized"
        );

        Self {
            index: RwLock::new(index),
            config,
        }
    }

    /// Get the two-tier index (for reads).
    pub fn index(&self) -> std::sync::RwLockReadGuard<'_, TwoTierIndex> {
        self.index.read().expect("two-tier index lock poisoned")
    }

    /// Get a mutable reference to the index (for writes).
    pub fn index_mut(&self) -> std::sync::RwLockWriteGuard<'_, TwoTierIndex> {
        self.index.write().expect("two-tier index lock poisoned")
    }

    /// Check if two-tier search is available (at least fast embedder).
    #[must_use]
    pub fn is_available(&self) -> bool {
        get_two_tier_context().fast_info().is_some()
    }

    /// Check if full two-tier search is available (both tiers).
    #[must_use]
    pub fn is_full(&self) -> bool {
        get_two_tier_context().is_full()
    }

    /// Get the availability status.
    #[must_use]
    pub fn availability(&self) -> TwoTierAvailability {
        get_two_tier_context().availability()
    }

    /// Search for semantically similar documents using two-tier progressive search.
    ///
    /// This returns the best available phase: refined results when quality
    /// refinement succeeds, otherwise the initial fast phase.
    pub fn search(&self, query: &SearchQuery, limit: usize) -> Vec<SearchResult> {
        self.search_with_policy(query, limit, false)
    }

    /// Budget-aware two-tier search that can prefer fast-first selection.
    ///
    /// When remaining request budget is tight, this path keeps latency bounded by
    /// selecting the earliest non-empty phase instead of waiting for refinement.
    pub fn search_with_cx(&self, cx: &Cx, query: &SearchQuery, limit: usize) -> Vec<SearchResult> {
        if cx.checkpoint().is_err() {
            tracing::debug!(
                target: "search.semantic",
                "two-tier search cancelled before dispatch"
            );
            return Vec::new();
        }

        let remaining_ms = request_budget_remaining_ms(cx).unwrap_or(u64::MAX);
        let fast_first_budget_ms = two_tier_fast_first_budget_ms();
        let prefer_fast_first = remaining_ms <= fast_first_budget_ms;

        let results = self.search_with_policy(query, limit, prefer_fast_first);
        if cx.checkpoint().is_err() {
            tracing::debug!(
                target: "search.semantic",
                "two-tier search cancelled after dispatch"
            );
            return Vec::new();
        }

        results
    }

    fn search_with_policy(
        &self,
        query: &SearchQuery,
        limit: usize,
        prefer_fast_first: bool,
    ) -> Vec<SearchResult> {
        let ctx = get_two_tier_context();

        // Two-tier bridge requires fast embeddings for both query and indexed docs.
        if ctx.fast_info().is_none() {
            tracing::debug!(
                target: "search.semantic",
                "fast embedder unavailable, skipping two-tier search"
            );
            return Vec::new();
        }

        let (selected_results, had_searcher) = {
            let index = self.index();
            ctx.create_searcher(&index)
                .map_or((None, false), |searcher| {
                    (
                        if prefer_fast_first {
                            select_fast_first_two_tier_results(searcher.search(&query.text, limit))
                        } else {
                            select_best_two_tier_results(searcher.search(&query.text, limit))
                        },
                        true,
                    )
                })
        };

        if let Some(results) = selected_results {
            return scored_results_to_search_results(results);
        }

        if had_searcher {
            tracing::debug!(
                target: "search.semantic",
                "two-tier search yielded no phases; falling back to fast tier"
            );
        } else {
            tracing::debug!(
                target: "search.semantic",
                "failed to create two-tier searcher; falling back to fast tier"
            );
        }

        // Deterministic fallback: run plain fast-tier search if progressive path
        // is unavailable or yields no phases.
        let embedding = match ctx.embed_fast(&query.text) {
            Ok(emb) => emb,
            Err(e) => {
                tracing::warn!(
                    target: "search.semantic",
                    error = %e,
                    "failed to embed query with fast tier"
                );
                return Vec::new();
            }
        };

        let hits = self.index().search_fast(&embedding, limit);
        scored_results_to_search_results(hits)
    }

    /// Add a document to the two-tier index.
    ///
    /// Automatically embeds using available tiers.
    pub fn add_document(
        &self,
        doc_id: i64,
        doc_kind: DocKind,
        project_id: Option<i64>,
        text: &str,
    ) -> Result<(), String> {
        let ctx = get_two_tier_context();

        if ctx.fast_info().is_none() {
            return Err("fast embedder unavailable".to_string());
        }

        // Embed with fast tier
        let fast_embedding = ctx
            .embed_fast(text)
            .map_err(|e| format!("fast embed failed: {e}"))?;

        // Embed with quality tier if available
        let quality_embedding = if ctx.is_full() {
            ctx.embed_quality(text).ok()
        } else {
            None
        };

        let doc_id = u64::try_from(doc_id).map_err(|_| "doc_id overflow".to_string())?;

        let fast_embedding_f16 = fast_embedding
            .into_iter()
            .map(f16::from_f32)
            .collect::<Vec<_>>();
        let has_quality = quality_embedding.is_some();
        let quality_embedding_f16 = quality_embedding
            .unwrap_or_else(|| vec![0.0; self.config.quality_dimension])
            .into_iter()
            .map(f16::from_f32)
            .collect::<Vec<_>>();

        let search_doc_kind = match doc_kind {
            DocKind::Message => SearchDocKind::Message,
            DocKind::Agent => SearchDocKind::Agent,
            DocKind::Project => SearchDocKind::Project,
            DocKind::Thread => SearchDocKind::Thread,
        };

        let entry = TwoTierEntry {
            doc_id,
            doc_kind: search_doc_kind,
            project_id,
            fast_embedding: fast_embedding_f16,
            quality_embedding: quality_embedding_f16,
            has_quality,
        };

        // Add to index
        let mut index = self.index_mut();
        index
            .add_entry(entry)
            .map_err(|e| format!("two-tier index add_entry failed: {e}"))?;
        drop(index);

        Ok(())
    }
}

#[cfg(feature = "hybrid")]
impl Default for TwoTierBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// Initialize the global two-tier semantic bridge.
///
/// This automatically detects available embedders. No configuration needed.
///
/// # Deprecation
///
/// Prefer using the atomic `get_or_init_two_tier_bridge()` function instead,
/// which handles concurrent initialization safely. This function is retained
/// for backward compatibility but may create duplicate bridges under concurrent
/// access (the extras are silently dropped by `OnceLock::set`).
#[cfg(feature = "hybrid")]
#[deprecated(
    since = "0.1.0",
    note = "Use get_or_init_two_tier_bridge() for thread-safe initialization"
)]
pub fn init_two_tier_bridge() -> Result<(), String> {
    let bridge = TwoTierBridge::new();
    let _ = TWO_TIER_BRIDGE.set(Some(Arc::new(bridge)));
    Ok(())
}

/// Get the global two-tier bridge, if initialized.
#[cfg(feature = "hybrid")]
pub fn get_two_tier_bridge() -> Option<Arc<TwoTierBridge>> {
    TWO_TIER_BRIDGE.get().and_then(std::clone::Clone::clone)
}

/// Get or atomically initialize the global two-tier bridge.
///
/// This is safe for concurrent calls - only one `TwoTierBridge` will ever be created,
/// avoiding the race condition where multiple threads could each create an expensive
/// bridge instance before `OnceLock::set` silently drops the extras.
#[cfg(feature = "hybrid")]
fn get_or_init_two_tier_bridge_with<F>(
    slot: &OnceLock<Option<Arc<TwoTierBridge>>>,
    init: F,
) -> Option<Arc<TwoTierBridge>>
where
    F: FnOnce() -> Option<Arc<TwoTierBridge>>,
{
    slot.get_or_init(init).clone()
}

#[cfg(feature = "hybrid")]
fn get_or_init_two_tier_bridge() -> Option<Arc<TwoTierBridge>> {
    get_or_init_two_tier_bridge_with(&TWO_TIER_BRIDGE, || Some(Arc::new(TwoTierBridge::new())))
}

/// Try executing semantic candidate retrieval using two-tier system.
///
/// Uses the two-tier bridge when available. When unavailable, callers
/// deterministically degrade to lexical-only candidate orchestration.
#[cfg(feature = "hybrid")]
#[allow(dead_code)]
fn try_two_tier_search(query: &SearchQuery, limit: usize) -> Option<Vec<SearchResult>> {
    // Use atomic get_or_init pattern to avoid race condition on initialization.
    // Only one TwoTierBridge instance is ever created under concurrent access.
    let bridge = get_or_init_two_tier_bridge()?;
    if bridge.is_available() {
        Some(bridge.search(query, limit))
    } else {
        None
    }
}

#[cfg(feature = "hybrid")]
fn try_two_tier_search_with_cx(
    cx: &Cx,
    query: &SearchQuery,
    limit: usize,
) -> Option<Vec<SearchResult>> {
    let bridge = get_or_init_two_tier_bridge()?;
    if bridge.is_available() {
        Some(bridge.search_with_cx(cx, query, limit))
    } else {
        None
    }
}

#[cfg(feature = "hybrid")]
const AM_SEARCH_RERANK_ENABLED_ENV: &str = "AM_SEARCH_RERANK_ENABLED";
#[cfg(feature = "hybrid")]
const AM_SEARCH_RERANK_TOP_K_ENV: &str = "AM_SEARCH_RERANK_TOP_K";
#[cfg(feature = "hybrid")]
const AM_SEARCH_RERANK_MIN_CANDIDATES_ENV: &str = "AM_SEARCH_RERANK_MIN_CANDIDATES";
#[cfg(feature = "hybrid")]
const AM_SEARCH_RERANK_BLEND_POLICY_ENV: &str = "AM_SEARCH_RERANK_BLEND_POLICY";
#[cfg(feature = "hybrid")]
const AM_SEARCH_RERANK_BLEND_WEIGHT_ENV: &str = "AM_SEARCH_RERANK_BLEND_WEIGHT";
#[cfg(feature = "hybrid")]
const AM_SEARCH_RERANK_MODEL_DIR_ENV: &str = "AM_SEARCH_RERANK_MODEL_DIR";
#[cfg(feature = "hybrid")]
const FRANKENSEARCH_MODEL_DIR_ENV: &str = "FRANKENSEARCH_MODEL_DIR";
#[cfg(feature = "hybrid")]
const DEFAULT_RERANK_MODEL_NAME: &str = "flashrank";
#[cfg(feature = "hybrid")]
const AM_SEARCH_TWO_TIER_FAST_FIRST_BUDGET_MS_ENV: &str = "AM_SEARCH_TWO_TIER_FAST_FIRST_BUDGET_MS";
#[cfg(feature = "hybrid")]
const DEFAULT_TWO_TIER_FAST_FIRST_BUDGET_MS: u64 = 150;
const AM_SEARCH_HYBRID_BUDGET_GOVERNOR_ENABLED_ENV: &str =
    "AM_SEARCH_HYBRID_BUDGET_GOVERNOR_ENABLED";
const AM_SEARCH_HYBRID_BUDGET_GOVERNOR_TIGHT_MS_ENV: &str =
    "AM_SEARCH_HYBRID_BUDGET_GOVERNOR_TIGHT_MS";
const AM_SEARCH_HYBRID_BUDGET_GOVERNOR_CRITICAL_MS_ENV: &str =
    "AM_SEARCH_HYBRID_BUDGET_GOVERNOR_CRITICAL_MS";
const AM_SEARCH_HYBRID_BUDGET_GOVERNOR_TIGHT_SCALE_BPS_ENV: &str =
    "AM_SEARCH_HYBRID_BUDGET_GOVERNOR_TIGHT_SCALE_BPS";
const AM_SEARCH_HYBRID_BUDGET_GOVERNOR_CRITICAL_SCALE_BPS_ENV: &str =
    "AM_SEARCH_HYBRID_BUDGET_GOVERNOR_CRITICAL_SCALE_BPS";
const AM_SEARCH_HYBRID_BUDGET_GOVERNOR_RESULT_FLOOR_ENV: &str =
    "AM_SEARCH_HYBRID_BUDGET_GOVERNOR_RESULT_FLOOR";
const DEFAULT_HYBRID_BUDGET_GOVERNOR_TIGHT_MS: u64 = 250;
const DEFAULT_HYBRID_BUDGET_GOVERNOR_CRITICAL_MS: u64 = 120;
const DEFAULT_HYBRID_BUDGET_GOVERNOR_TIGHT_SCALE_BPS: u32 = 70;
const DEFAULT_HYBRID_BUDGET_GOVERNOR_CRITICAL_SCALE_BPS: u32 = 40;
const DEFAULT_HYBRID_BUDGET_GOVERNOR_RESULT_FLOOR: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HybridBudgetGovernorTier {
    Unlimited,
    Normal,
    Tight,
    Critical,
}

impl HybridBudgetGovernorTier {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unlimited => "unlimited",
            Self::Normal => "normal",
            Self::Tight => "tight",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HybridBudgetGovernorConfig {
    enabled: bool,
    tight_ms: u64,
    critical_ms: u64,
    tight_scale_bps: u32,
    critical_scale_bps: u32,
    result_floor: usize,
}

impl Default for HybridBudgetGovernorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tight_ms: DEFAULT_HYBRID_BUDGET_GOVERNOR_TIGHT_MS,
            critical_ms: DEFAULT_HYBRID_BUDGET_GOVERNOR_CRITICAL_MS,
            tight_scale_bps: DEFAULT_HYBRID_BUDGET_GOVERNOR_TIGHT_SCALE_BPS,
            critical_scale_bps: DEFAULT_HYBRID_BUDGET_GOVERNOR_CRITICAL_SCALE_BPS,
            result_floor: DEFAULT_HYBRID_BUDGET_GOVERNOR_RESULT_FLOOR,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HybridBudgetGovernorState {
    remaining_budget_ms: Option<u64>,
    tier: HybridBudgetGovernorTier,
    rerank_enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct HybridExecutionPlan {
    derivation: CandidateBudgetDerivation,
    governor: HybridBudgetGovernorState,
}

fn wall_clock_now_time() -> Time {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let nanos_u64 = u64::try_from(nanos).unwrap_or(u64::MAX);
    Time::from_nanos(nanos_u64)
}

fn saturating_duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn deadline_remaining_budget_ms(budget: Budget) -> Option<u64> {
    budget.deadline?;
    let now = wall_clock_now_time();
    Some(
        budget
            .remaining_time(now)
            .map_or(0, saturating_duration_millis),
    )
}

fn request_budget_remaining_ms(cx: &Cx) -> Option<u64> {
    let budget = cx.budget();
    let deadline_remaining_ms = deadline_remaining_budget_ms(budget);
    let cost_remaining_ms = budget.remaining_cost();

    match (deadline_remaining_ms, cost_remaining_ms) {
        (Some(deadline), Some(cost)) => Some(deadline.min(cost)),
        (Some(deadline), None) => Some(deadline),
        (None, Some(cost)) => Some(cost),
        (None, None) => None,
    }
}

#[cfg(feature = "hybrid")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerankBlendPolicy {
    Weighted,
    Replace,
}

#[cfg(feature = "hybrid")]
impl RerankBlendPolicy {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "replace" | "rerank_only" | "rerank-only" => Self::Replace,
            _ => Self::Weighted,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::Replace => "replace",
        }
    }
}

#[cfg(feature = "hybrid")]
#[derive(Debug, Clone, Copy, PartialEq)]
struct HybridRerankConfig {
    enabled: bool,
    top_k: usize,
    min_candidates: usize,
    blend_policy: RerankBlendPolicy,
    blend_weight: f64,
}

#[cfg(feature = "hybrid")]
impl Default for HybridRerankConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            top_k: 100,
            min_candidates: 5,
            blend_policy: RerankBlendPolicy::Weighted,
            blend_weight: 0.35,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct HybridRerankAudit {
    enabled: bool,
    attempted: bool,
    outcome: String,
    candidate_count: usize,
    top_k: usize,
    min_candidates: usize,
    blend_policy: Option<String>,
    blend_weight: Option<f64>,
    applied_count: usize,
}

fn parse_env_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_env_bool(&value))
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn env_u32(name: &str, default: u32, min: u32, max: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

#[cfg(feature = "hybrid")]
fn env_f64(name: &str, default: f64, min: f64, max: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

#[cfg(feature = "hybrid")]
fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn hybrid_budget_governor_config_from_env() -> HybridBudgetGovernorConfig {
    let defaults = HybridBudgetGovernorConfig::default();
    let tight_ms = env_u64(
        AM_SEARCH_HYBRID_BUDGET_GOVERNOR_TIGHT_MS_ENV,
        defaults.tight_ms,
        1,
        60_000,
    );
    let critical_ms = env_u64(
        AM_SEARCH_HYBRID_BUDGET_GOVERNOR_CRITICAL_MS_ENV,
        defaults.critical_ms,
        1,
        tight_ms,
    );

    HybridBudgetGovernorConfig {
        enabled: env_bool(
            AM_SEARCH_HYBRID_BUDGET_GOVERNOR_ENABLED_ENV,
            defaults.enabled,
        ),
        tight_ms,
        critical_ms,
        tight_scale_bps: env_u32(
            AM_SEARCH_HYBRID_BUDGET_GOVERNOR_TIGHT_SCALE_BPS_ENV,
            defaults.tight_scale_bps,
            1,
            100,
        ),
        critical_scale_bps: env_u32(
            AM_SEARCH_HYBRID_BUDGET_GOVERNOR_CRITICAL_SCALE_BPS_ENV,
            defaults.critical_scale_bps,
            1,
            100,
        ),
        result_floor: env_usize(
            AM_SEARCH_HYBRID_BUDGET_GOVERNOR_RESULT_FLOOR_ENV,
            defaults.result_floor,
            1,
            200,
        ),
    }
}

const fn classify_hybrid_budget_tier(
    remaining_budget_ms: Option<u64>,
    config: HybridBudgetGovernorConfig,
) -> HybridBudgetGovernorTier {
    match remaining_budget_ms {
        None => HybridBudgetGovernorTier::Unlimited,
        Some(remaining) if remaining <= config.critical_ms => HybridBudgetGovernorTier::Critical,
        Some(remaining) if remaining <= config.tight_ms => HybridBudgetGovernorTier::Tight,
        Some(_) => HybridBudgetGovernorTier::Normal,
    }
}

fn scale_limit_by_bps(limit: usize, scale_bps: u32) -> usize {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    let scaled = limit_u64.saturating_mul(u64::from(scale_bps)).div_ceil(100);
    usize::try_from(scaled).unwrap_or(usize::MAX).max(1)
}

fn apply_hybrid_budget_governor(
    requested_limit: usize,
    base_budget: CandidateBudget,
    remaining_budget_ms: Option<u64>,
    config: HybridBudgetGovernorConfig,
) -> (CandidateBudget, HybridBudgetGovernorState) {
    let tier = classify_hybrid_budget_tier(remaining_budget_ms, config);
    if !config.enabled {
        return (
            base_budget,
            HybridBudgetGovernorState {
                remaining_budget_ms,
                tier,
                rerank_enabled: true,
            },
        );
    }

    let result_floor = requested_limit.clamp(1, config.result_floor);
    let (budget, rerank_enabled) = match tier {
        HybridBudgetGovernorTier::Unlimited | HybridBudgetGovernorTier::Normal => {
            (base_budget, true)
        }
        HybridBudgetGovernorTier::Tight => {
            let lexical_limit =
                scale_limit_by_bps(base_budget.lexical_limit, config.tight_scale_bps)
                    .max(result_floor);
            let semantic_limit = if base_budget.semantic_limit == 0 {
                0
            } else {
                scale_limit_by_bps(base_budget.semantic_limit, config.tight_scale_bps).max(1)
            };
            let combined_limit = base_budget
                .combined_limit
                .min(lexical_limit.saturating_add(semantic_limit))
                .max(lexical_limit.max(result_floor));
            (
                CandidateBudget {
                    lexical_limit,
                    semantic_limit,
                    combined_limit,
                },
                false,
            )
        }
        HybridBudgetGovernorTier::Critical => {
            let lexical_limit =
                scale_limit_by_bps(base_budget.lexical_limit, config.critical_scale_bps)
                    .max(result_floor);
            let combined_limit = lexical_limit.max(result_floor);
            (
                CandidateBudget {
                    lexical_limit,
                    semantic_limit: 0,
                    combined_limit,
                },
                false,
            )
        }
    };

    (
        budget,
        HybridBudgetGovernorState {
            remaining_budget_ms,
            tier,
            rerank_enabled,
        },
    )
}

fn derive_hybrid_execution_plan(
    cx: &Cx,
    query: &SearchQuery,
    engine: SearchEngine,
) -> HybridExecutionPlan {
    let requested_limit = query.effective_limit();
    let mode = match engine {
        SearchEngine::Hybrid => CandidateMode::Hybrid,
        SearchEngine::Auto => CandidateMode::Auto,
        _ => CandidateMode::LexicalFallback,
    };
    let query_class = QueryClass::classify(&query.text);
    let mut derivation = CandidateBudget::derive_with_decision(
        requested_limit,
        mode,
        query_class,
        CandidateBudgetConfig::default(),
    );
    let governor_config = hybrid_budget_governor_config_from_env();
    let remaining_budget_ms = request_budget_remaining_ms(cx);
    let (governed_budget, governor) = apply_hybrid_budget_governor(
        requested_limit,
        derivation.budget,
        remaining_budget_ms,
        governor_config,
    );
    derivation.budget = governed_budget;
    HybridExecutionPlan {
        derivation,
        governor,
    }
}

#[cfg(feature = "hybrid")]
fn hybrid_rerank_config_from_env() -> HybridRerankConfig {
    let default = HybridRerankConfig::default();
    let blend_policy = std::env::var(AM_SEARCH_RERANK_BLEND_POLICY_ENV)
        .ok()
        .map_or(default.blend_policy, |value| {
            RerankBlendPolicy::parse(&value)
        });

    HybridRerankConfig {
        enabled: env_bool(AM_SEARCH_RERANK_ENABLED_ENV, default.enabled),
        top_k: env_usize(AM_SEARCH_RERANK_TOP_K_ENV, default.top_k, 1, 500),
        min_candidates: env_usize(
            AM_SEARCH_RERANK_MIN_CANDIDATES_ENV,
            default.min_candidates,
            1,
            500,
        ),
        blend_policy,
        blend_weight: env_f64(
            AM_SEARCH_RERANK_BLEND_WEIGHT_ENV,
            default.blend_weight,
            0.0,
            1.0,
        ),
    }
}

#[cfg(feature = "hybrid")]
fn two_tier_fast_first_budget_ms() -> u64 {
    env_u64(
        AM_SEARCH_TWO_TIER_FAST_FIRST_BUDGET_MS_ENV,
        DEFAULT_TWO_TIER_FAST_FIRST_BUDGET_MS,
        1,
        30_000,
    )
}

#[cfg(feature = "hybrid")]
fn resolve_rerank_model_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(AM_SEARCH_RERANK_MODEL_DIR_ENV) {
        return Some(PathBuf::from(path));
    }

    std::env::var(FRANKENSEARCH_MODEL_DIR_ENV)
        .ok()
        .map(|path| PathBuf::from(path).join(DEFAULT_RERANK_MODEL_NAME))
}

#[cfg(feature = "hybrid")]
fn get_or_init_hybrid_reranker() -> Option<Arc<fs::FlashRankReranker>> {
    HYBRID_RERANKER
        .get_or_init(|| {
            let Some(model_dir) = resolve_rerank_model_dir() else {
                tracing::debug!(
                    target: "search.metrics",
                    "rerank model dir not configured; skipping reranker init"
                );
                return None;
            };

            match fs::FlashRankReranker::load(&model_dir) {
                Ok(reranker) => Some(Arc::new(reranker)),
                Err(error) => {
                    tracing::warn!(
                        target: "search.metrics",
                        model_dir = %model_dir.display(),
                        error = %error,
                        "failed to initialize reranker; degrading to fusion-only"
                    );
                    None
                }
            }
        })
        .clone()
}

#[cfg(feature = "hybrid")]
fn blend_rerank_score(
    baseline_score: f64,
    rerank_score: f64,
    policy: RerankBlendPolicy,
    weight: f64,
) -> f64 {
    match policy {
        RerankBlendPolicy::Replace => rerank_score,
        RerankBlendPolicy::Weighted => {
            let w = weight.clamp(0.0, 1.0);
            (1.0 - w).mul_add(baseline_score, w * rerank_score)
        }
    }
}

#[cfg(feature = "hybrid")]
fn apply_rerank_scores_and_sort(
    merged: &mut [SearchResult],
    rerank_scores: &BTreeMap<i64, f64>,
    policy: RerankBlendPolicy,
    weight: f64,
) -> usize {
    let mut applied = 0usize;
    for result in merged.iter_mut() {
        let Some(&rerank_score) = rerank_scores.get(&result.id) else {
            continue;
        };
        let baseline = result.score.unwrap_or(0.0);
        result.score = Some(blend_rerank_score(baseline, rerank_score, policy, weight));
        applied = applied.saturating_add(1);
    }

    merged.sort_by(|left, right| {
        right
            .score
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&left.score.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| left.id.cmp(&right.id))
    });
    applied
}

#[cfg(feature = "hybrid")]
#[allow(clippy::too_many_lines)]
async fn maybe_apply_hybrid_rerank(
    cx: &Cx,
    query: &SearchQuery,
    merged: &mut [SearchResult],
) -> HybridRerankAudit {
    let config = hybrid_rerank_config_from_env();
    let candidate_count = merged.len();
    let top_k = config.top_k.min(candidate_count);
    let min_candidates = config.min_candidates.min(top_k.max(1));
    let mut audit = HybridRerankAudit {
        enabled: config.enabled,
        attempted: false,
        outcome: if config.enabled {
            "not_attempted".to_string()
        } else {
            "disabled".to_string()
        },
        candidate_count,
        top_k,
        min_candidates,
        blend_policy: Some(config.blend_policy.as_str().to_string()),
        blend_weight: Some(config.blend_weight),
        applied_count: 0,
    };

    if !config.enabled {
        return audit;
    }
    if candidate_count < config.min_candidates {
        audit.outcome = "insufficient_candidates".to_string();
        tracing::debug!(
            target: "search.metrics",
            candidate_count,
            min_candidates = config.min_candidates,
            "skipping rerank due to insufficient candidates"
        );
        return audit;
    }

    let Some(reranker) = get_or_init_hybrid_reranker() else {
        audit.outcome = "reranker_unavailable".to_string();
        return audit;
    };

    audit.attempted = true;

    let text_by_doc = merged
        .iter()
        .map(|result| {
            let text = if result.body.is_empty() {
                result.title.clone()
            } else {
                format!("{}\n\n{}", result.title, result.body)
            };
            (result.id.to_string(), text)
        })
        .collect::<BTreeMap<_, _>>();

    #[allow(clippy::cast_possible_truncation)]
    let mut fs_candidates = merged
        .iter()
        .map(|result| FsScoredResult {
            doc_id: result.id.to_string(),
            score: result.score.unwrap_or(0.0) as f32,
            source: fs::core::types::ScoreSource::Hybrid,
            fast_score: None,
            quality_score: None,
            lexical_score: result.score.map(|score| score as f32),
            rerank_score: None,
            explanation: None,
            metadata: None,
        })
        .collect::<Vec<_>>();

    let rerank_outcome = fs::rerank_step(
        cx,
        reranker.as_ref(),
        &query.text,
        &mut fs_candidates,
        |doc_id| text_by_doc.get(doc_id).cloned(),
        top_k,
        min_candidates,
    )
    .await;

    if let Err(error) = rerank_outcome {
        audit.outcome = "rerank_error".to_string();
        tracing::warn!(
            target: "search.metrics",
            error = %error,
            "rerank step failed; degrading to fusion-only ranking"
        );
        return audit;
    }

    let rerank_scores = fs_candidates
        .iter()
        .filter_map(|candidate| {
            let score = candidate.rerank_score?;
            let doc_id = candidate.doc_id.parse::<i64>().ok()?;
            Some((doc_id, f64::from(score)))
        })
        .collect::<BTreeMap<_, _>>();
    if rerank_scores.is_empty() {
        audit.outcome = "no_scores".to_string();
        tracing::debug!(
            target: "search.metrics",
            "rerank step produced no scores; preserving fusion order"
        );
        return audit;
    }

    let applied = apply_rerank_scores_and_sort(
        merged,
        &rerank_scores,
        config.blend_policy,
        config.blend_weight,
    );
    audit.applied_count = applied;
    audit.outcome = if applied > 0 {
        "applied".to_string()
    } else {
        "no_matching_scores".to_string()
    };
    tracing::debug!(
        target: "search.metrics",
        applied_count = applied,
        top_k,
        min_candidates,
        blend_weight = config.blend_weight,
        blend_policy = ?config.blend_policy,
        "hybrid rerank applied"
    );
    audit
}

fn orchestrate_hybrid_results(
    query: &SearchQuery,
    derivation: &CandidateBudgetDerivation,
    governor: HybridBudgetGovernorState,
    lexical_results: Vec<SearchResult>,
    semantic_results: Vec<SearchResult>,
) -> Vec<SearchResult> {
    let requested_limit = query.effective_limit();
    let budget = derivation.budget;

    let lexical_hits = lexical_results
        .iter()
        .map(|result| CandidateHit::new(result.id, result.score.unwrap_or(0.0)))
        .collect::<Vec<_>>();
    let semantic_hits = semantic_results
        .iter()
        .map(|result| CandidateHit::new(result.id, result.score.unwrap_or(0.0)))
        .collect::<Vec<_>>();
    let prepared = prepare_candidates(&lexical_hits, &semantic_hits, budget);

    let lexical_map = lexical_results
        .into_iter()
        .map(|result| (result.id, result))
        .collect::<std::collections::BTreeMap<_, _>>();
    let semantic_map = semantic_results
        .into_iter()
        .map(|result| (result.id, result))
        .collect::<std::collections::BTreeMap<_, _>>();

    let ordered_candidates = prepared
        .candidates
        .iter()
        .take(requested_limit)
        .collect::<Vec<_>>();

    let merged = ordered_candidates
        .iter()
        .filter_map(|candidate| {
            lexical_map
                .get(&candidate.doc_id)
                .cloned()
                .or_else(|| semantic_map.get(&candidate.doc_id).cloned())
        })
        .collect::<Vec<_>>();

    tracing::debug!(
        target: "search.metrics",
        query = %query.text,
        mode = ?derivation.decision.mode,
        query_class = ?derivation.decision.query_class,
        decision_action = ?derivation.decision.chosen_action,
        decision_expected_loss = derivation.decision.chosen_expected_loss,
        decision_confidence = decision_confidence(&derivation.decision),
        governor_tier = governor.tier.as_str(),
        governor_remaining_budget_ms = governor.remaining_budget_ms.unwrap_or(u64::MAX),
        governor_rerank_enabled = governor.rerank_enabled,
        lexical_considered = prepared.counts.lexical_considered,
        semantic_considered = prepared.counts.semantic_considered,
        lexical_selected = prepared.counts.lexical_selected,
        semantic_selected = prepared.counts.semantic_selected,
        deduped_selected = prepared.counts.deduped_selected,
        duplicates_removed = prepared.counts.duplicates_removed,
        "hybrid candidate orchestration completed"
    );
    emit_hybrid_budget_evidence(query, derivation, &prepared.counts, governor);

    merged
}

#[cfg(feature = "hybrid")]
fn rerank_skip_audit_for_governor(
    governor: HybridBudgetGovernorState,
    candidate_count: usize,
) -> HybridRerankAudit {
    HybridRerankAudit {
        enabled: false,
        attempted: false,
        outcome: format!("skipped_by_budget_governor_{}", governor.tier.as_str()),
        candidate_count,
        top_k: 0,
        min_candidates: 0,
        blend_policy: None,
        blend_weight: None,
        applied_count: 0,
    }
}

async fn orchestrate_hybrid_results_with_optional_rerank(
    cx: &Cx,
    query: &SearchQuery,
    plan: &HybridExecutionPlan,
    lexical_results: Vec<SearchResult>,
    semantic_results: Vec<SearchResult>,
) -> (Vec<SearchResult>, Option<HybridRerankAudit>) {
    let mut merged = orchestrate_hybrid_results(
        query,
        &plan.derivation,
        plan.governor,
        lexical_results,
        semantic_results,
    );

    #[cfg(feature = "hybrid")]
    let rerank_audit = Some(if plan.governor.rerank_enabled {
        maybe_apply_hybrid_rerank(cx, query, merged.as_mut_slice()).await
    } else {
        rerank_skip_audit_for_governor(plan.governor, merged.len())
    });
    #[cfg(not(feature = "hybrid"))]
    let rerank_audit = None;

    (merged, rerank_audit)
}

fn build_v3_query_explain(
    query: &SearchQuery,
    engine: SearchEngine,
    rerank_audit: Option<&HybridRerankAudit>,
) -> crate::search_planner::QueryExplain {
    let mut facets_applied = vec![format!("engine:{engine}")];
    if let Some(audit) = rerank_audit {
        facets_applied.push(format!("rerank_enabled:{}", audit.enabled));
        facets_applied.push(format!("rerank_attempted:{}", audit.attempted));
        facets_applied.push(format!("rerank_outcome:{}", audit.outcome));
        facets_applied.push(format!("rerank_candidates:{}", audit.candidate_count));
        facets_applied.push(format!("rerank_top_k:{}", audit.top_k));
        facets_applied.push(format!("rerank_min_candidates:{}", audit.min_candidates));
        facets_applied.push(format!("rerank_applied_count:{}", audit.applied_count));
        if let Some(policy) = &audit.blend_policy {
            facets_applied.push(format!("rerank_blend_policy:{policy}"));
        }
        if let Some(weight) = audit.blend_weight {
            facets_applied.push(format!("rerank_blend_weight:{weight:.3}"));
        }
    }

    crate::search_planner::QueryExplain {
        method: format!("{engine}_v3"),
        normalized_query: if query.text.is_empty() {
            None
        } else {
            Some(query.text.clone())
        },
        used_like_fallback: false,
        facet_count: facets_applied.len(),
        facets_applied,
        sql: "-- v3 pipeline (non-SQL result assembly)".to_string(),
        scope_policy: "unrestricted".to_string(),
        denied_count: 0,
        redacted_count: 0,
    }
}

fn decision_confidence(decision: &CandidateBudgetDecision) -> f64 {
    let mut losses = decision
        .action_losses
        .iter()
        .map(|entry| entry.expected_loss)
        .collect::<Vec<_>>();
    losses.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let Some(best) = losses.first().copied() else {
        return 0.0;
    };
    let Some(second_best) = losses.get(1).copied() else {
        return 1.0;
    };
    let denom = (best + second_best).max(f64::EPSILON);
    ((second_best - best) / denom).clamp(0.0, 1.0)
}

const fn mode_label(mode: CandidateMode) -> &'static str {
    match mode {
        CandidateMode::Hybrid => "hybrid",
        CandidateMode::Auto => "auto",
        CandidateMode::LexicalFallback => "lexical_fallback",
    }
}

fn emit_hybrid_budget_evidence(
    query: &SearchQuery,
    derivation: &CandidateBudgetDerivation,
    counts: &CandidateStageCounts,
    governor: HybridBudgetGovernorState,
) {
    let confidence = decision_confidence(&derivation.decision);
    let action_label = match derivation.decision.chosen_action {
        mcp_agent_mail_search_core::CandidateBudgetAction::LexicalDominant => "lexical_dominant",
        mcp_agent_mail_search_core::CandidateBudgetAction::Balanced => "balanced",
        mcp_agent_mail_search_core::CandidateBudgetAction::SemanticDominant => "semantic_dominant",
        mcp_agent_mail_search_core::CandidateBudgetAction::LexicalOnly => "lexical_only",
    };
    let mode = derivation.decision.mode;
    let decision_id = format!(
        "search.hybrid_budget:{}:{}:{}",
        chrono::Utc::now().timestamp_micros(),
        mode_label(mode),
        query.effective_limit()
    );
    let mut entry = EvidenceLedgerEntry::new(
        decision_id,
        "search.hybrid_budget",
        action_label,
        confidence,
        serde_json::json!({
            "query_text": query.text,
            "query_class": derivation.decision.query_class,
            "mode": mode_label(mode),
            "requested_limit": query.effective_limit(),
            "budget": derivation.budget,
            "posterior": derivation.decision.posterior,
            "action_losses": derivation.decision.action_losses,
            "counts": counts,
            "governor": {
                "tier": governor.tier.as_str(),
                "remaining_budget_ms": governor.remaining_budget_ms,
                "rerank_enabled": governor.rerank_enabled,
            },
        }),
    );
    entry.expected_loss = Some(derivation.decision.chosen_expected_loss);
    entry.expected = Some("budgeted hybrid retrieval with deterministic fusion input".to_string());
    entry.trace_id.clone_from(&query.thread_id);

    if let Err(error) = append_evidence_entry_if_configured(&entry) {
        tracing::debug!(
            target: "search.metrics",
            error = %error,
            "failed to append hybrid budget evidence entry"
        );
    }
}

/// Log a comparison between FTS5 and Tantivy results in shadow mode.
fn log_shadow_comparison(
    fts5: &[SearchResult],
    tantivy: &[SearchResult],
    query: &SearchQuery,
    fts5_latency_us: u64,
    tantivy_latency_us: u64,
    v3_had_error: bool,
) {
    let fts5_ids: Vec<i64> = fts5.iter().map(|r| r.id).collect();
    let tantivy_ids: Vec<i64> = tantivy.iter().map(|r| r.id).collect();
    let overlap = fts5_ids
        .iter()
        .filter(|id| tantivy_ids.contains(id))
        .count();

    // Compute overlap percentage (0.0 - 1.0)
    let max_count = fts5.len().max(tantivy.len()).max(1);
    #[allow(clippy::cast_precision_loss)]
    let overlap_pct = overlap as f64 / max_count as f64;

    // Compute latency delta (V3 - legacy) in microseconds
    #[allow(clippy::cast_possible_wrap)]
    let latency_delta_us = tantivy_latency_us as i64 - fts5_latency_us as i64;

    // Equivalent if ≥80% overlap and no V3 errors
    let is_equivalent = overlap_pct >= 0.8 && !v3_had_error;

    // Record to global metrics
    global_metrics()
        .search
        .record_shadow_comparison(is_equivalent, v3_had_error, latency_delta_us);

    tracing::info!(
        target: "search.metrics",
        query = %query.text,
        fts5_count = fts5.len(),
        tantivy_count = tantivy.len(),
        overlap_count = overlap,
        overlap_pct = %format!("{:.1}%", overlap_pct * 100.0),
        latency_delta_us = latency_delta_us,
        is_equivalent = is_equivalent,
        v3_had_error = v3_had_error,
        "shadow search comparison"
    );
}

fn record_legacy_error_metrics(metric_key: &str, latency_us: u64, track_telemetry: bool) {
    if track_telemetry {
        record_query(metric_key, latency_us);
    }
    global_metrics()
        .search
        .record_legacy_query(latency_us, true);
}

fn maybe_record_v3_fallback(engine: SearchEngine, query: &SearchQuery) {
    if matches!(
        engine,
        SearchEngine::Lexical | SearchEngine::Hybrid | SearchEngine::Auto
    ) {
        global_metrics().search.record_fallback();
        tracing::warn!(
            target: "search.metrics",
            engine = ?engine,
            query = %query.text,
            "v3 search unavailable; falling back to legacy FTS5"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Core execution
// ────────────────────────────────────────────────────────────────────

/// Execute a search query with full plan → SQL → scope pipeline.
///
/// This is the primary entry point for all search operations.
///
/// # Errors
///
/// Returns `DbError` on database or pool errors.
#[allow(clippy::too_many_lines)]
pub async fn execute_search(
    cx: &Cx,
    pool: &DbPool,
    query: &SearchQuery,
    options: &SearchOptions,
) -> Outcome<ScopedSearchResponse, DbError> {
    let timer = std::time::Instant::now();
    let engine = options.search_engine.unwrap_or_default();
    let assistance = query_assistance_payload(query);

    // ── Tantivy-only fast path ──────────────────────────────────────
    if engine == SearchEngine::Lexical {
        if let Some(raw_results) = try_tantivy_search(query) {
            let explain = if query.explain {
                Some(build_v3_query_explain(query, engine, None))
            } else {
                None
            };
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            if options.track_telemetry {
                record_query("search_service_tantivy", latency_us);
            }
            // Record V3 metrics
            global_metrics().search.record_v3_query(latency_us, false);
            return finish_scoped_response(
                raw_results,
                query,
                options,
                assistance.clone(),
                explain,
            );
        }
        maybe_record_v3_fallback(engine, query);
        // Bridge not initialized → fall through to FTS5.
    }

    // ── Hybrid candidate orchestration path ─────────────────────────
    //
    // Stage order:
    // 1) lexical candidate retrieval (Tantivy bridge)
    // 2) semantic candidate retrieval (two-tier with auto-init)
    // 3) deterministic dedupe + merge prep (mode-aware budgets)
    // 4) optional rerank refinement with graceful fallback.
    if matches!(engine, SearchEngine::Hybrid | SearchEngine::Auto) {
        let plan = derive_hybrid_execution_plan(cx, query, engine);
        let mut lexical_query = query.clone();
        lexical_query.limit = Some(plan.derivation.budget.lexical_limit);

        if let Some(lexical_results) = try_tantivy_search(&lexical_query) {
            // Two-tier semantic candidates are optional; missing bridge degrades
            // to lexical-only while preserving deterministic fusion behavior.
            #[cfg(feature = "hybrid")]
            let semantic_results = if plan.derivation.budget.semantic_limit == 0 {
                Vec::new()
            } else {
                try_two_tier_search_with_cx(cx, query, plan.derivation.budget.semantic_limit)
                    .unwrap_or_default()
            };
            #[cfg(not(feature = "hybrid"))]
            let semantic_results = Vec::new();

            let (raw_results, rerank_audit) = orchestrate_hybrid_results_with_optional_rerank(
                cx,
                query,
                &plan,
                lexical_results,
                semantic_results,
            )
            .await;
            let explain = if query.explain {
                Some(build_v3_query_explain(query, engine, rerank_audit.as_ref()))
            } else {
                None
            };
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            if options.track_telemetry {
                record_query("search_service_hybrid_candidates", latency_us);
            }
            global_metrics().search.record_v3_query(latency_us, false);
            return finish_scoped_response(
                raw_results,
                query,
                options,
                assistance.clone(),
                explain,
            );
        }
        maybe_record_v3_fallback(engine, query);
        // No lexical bridge available yet → fall through to legacy FTS.
    }

    // ── Shadow: pre-fetch Tantivy results for comparison ────────────
    #[allow(deprecated)]
    let (shadow_tantivy, shadow_tantivy_latency_us) = if engine.is_shadow() {
        let tantivy_timer = std::time::Instant::now();
        let results = try_tantivy_search(query);
        let latency = u64::try_from(tantivy_timer.elapsed().as_micros()).unwrap_or(u64::MAX);
        (results, latency)
    } else {
        (None, 0)
    };

    // ── FTS5 path (default + Shadow primary) ────────────────────────

    // Step 1: Plan the query
    let plan = plan_search(query);

    if plan.method == PlanMethod::Empty {
        let explain = if query.explain {
            Some(plan.explain())
        } else {
            None
        };
        return Outcome::Ok(ScopedSearchResponse {
            results: Vec::new(),
            next_cursor: None,
            explain,
            audit_summary: None,
            sql_row_count: 0,
            assistance,
        });
    }

    // Step 2: Acquire connection
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => {
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            record_legacy_error_metrics("search_service", latency_us, options.track_telemetry);
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    // Step 3: Execute SQL
    let values: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
    let rows_out = map_sql_outcome(raw_query(cx, &*conn, &plan.sql, &values).await);

    let rows = match rows_out {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => {
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            record_legacy_error_metrics("search_service", latency_us, options.track_telemetry);
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let fts5_latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
    if options.track_telemetry {
        record_query("search_service", fts5_latency_us);
    }
    // Record legacy FTS5 metrics
    global_metrics()
        .search
        .record_legacy_query(fts5_latency_us, false);

    // Step 4: Map rows to SearchResult
    let raw_results = map_rows_to_results(&rows, query.doc_kind);

    // Shadow comparison logging
    if let Some(ref tantivy_results) = shadow_tantivy {
        let v3_had_error = tantivy_results.is_empty() && !raw_results.is_empty();
        log_shadow_comparison(
            &raw_results,
            tantivy_results,
            query,
            fts5_latency_us,
            shadow_tantivy_latency_us,
            v3_had_error,
        );
    }

    let sql_row_count = raw_results.len();

    // Step 5: Compute pagination cursor
    let next_cursor = compute_next_cursor(&raw_results, query.effective_limit());

    // Step 6: Apply scope enforcement
    let redaction = options.redaction_policy.clone().unwrap_or_default();
    let scope_ctx = options.scope_ctx.clone().unwrap_or_else(|| ScopeContext {
        viewer: None,
        approved_contacts: Vec::new(),
        viewer_project_ids: Vec::new(),
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    });

    let (scoped_results, audit_summary) = apply_scope(raw_results, &scope_ctx, &redaction);

    // Step 7: Build explain
    let explain = if query.explain {
        let mut e = plan.explain();
        e.denied_count = audit_summary.denied_count;
        e.redacted_count = audit_summary.redacted_count;
        e.scope_policy.clone_from(&plan.scope_label);
        Some(e)
    } else {
        None
    };

    let audit = if scope_ctx.viewer.is_some() {
        Some(audit_summary)
    } else {
        None
    };

    Outcome::Ok(ScopedSearchResponse {
        results: scoped_results,
        next_cursor,
        explain,
        audit_summary: audit,
        sql_row_count,
        assistance,
    })
}

/// Apply scope enforcement and build a `ScopedSearchResponse` from raw results.
///
/// Shared by both the Tantivy and FTS5 paths to avoid duplicating scope logic.
fn finish_scoped_response(
    raw_results: Vec<SearchResult>,
    query: &SearchQuery,
    options: &SearchOptions,
    assistance: Option<QueryAssistance>,
    explain: Option<crate::search_planner::QueryExplain>,
) -> Outcome<ScopedSearchResponse, DbError> {
    let sql_row_count = raw_results.len();
    let next_cursor = compute_next_cursor(&raw_results, query.effective_limit());
    let redaction = options.redaction_policy.clone().unwrap_or_default();
    let scope_ctx = options.scope_ctx.clone().unwrap_or_else(|| ScopeContext {
        viewer: None,
        approved_contacts: Vec::new(),
        viewer_project_ids: Vec::new(),
        sender_policies: Vec::new(),
        recipient_map: Vec::new(),
    });
    let (scoped_results, audit_summary) = apply_scope(raw_results, &scope_ctx, &redaction);
    let explain = if query.explain {
        explain.map(|mut value| {
            value.denied_count = audit_summary.denied_count;
            value.redacted_count = audit_summary.redacted_count;
            if scope_ctx.viewer.is_some() {
                value.scope_policy = "caller_scoped".to_string();
            }
            value
        })
    } else {
        None
    };
    let audit = if scope_ctx.viewer.is_some() {
        Some(audit_summary)
    } else {
        None
    };
    Outcome::Ok(ScopedSearchResponse {
        results: scoped_results,
        next_cursor,
        explain,
        audit_summary: audit,
        sql_row_count,
        assistance,
    })
}

/// Execute a simple (unscoped) search — for backward compatibility with existing tools.
///
/// Always uses the FTS5 engine regardless of global config. Callers that want
/// Tantivy routing should use [`execute_search`] with [`SearchOptions::search_engine`].
///
/// Returns a `SearchResponse` without scope enforcement or audit.
///
/// # Errors
///
/// Returns `DbError` on database or pool errors.
pub async fn execute_search_simple(
    cx: &Cx,
    pool: &DbPool,
    query: &SearchQuery,
) -> Outcome<SimpleSearchResponse, DbError> {
    let timer = std::time::Instant::now();
    let assistance = query_assistance_payload(query);

    let plan = plan_search(query);

    if plan.method == PlanMethod::Empty {
        let explain = if query.explain {
            Some(plan.explain())
        } else {
            None
        };
        return Outcome::Ok(SearchResponse {
            results: Vec::new(),
            next_cursor: None,
            explain,
            assistance,
            audit: Vec::new(),
        });
    }

    // Acquire connection
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => {
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            record_legacy_error_metrics("search_service_simple", latency_us, true);
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let values: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
    let rows_out = map_sql_outcome(raw_query(cx, &*conn, &plan.sql, &values).await);

    let rows = match rows_out {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => {
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            record_legacy_error_metrics("search_service_simple", latency_us, true);
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
    record_query("search_service_simple", latency_us);
    // Record legacy FTS5 metrics (simple search always uses FTS5)
    global_metrics()
        .search
        .record_legacy_query(latency_us, false);

    let raw_results = map_rows_to_results(&rows, query.doc_kind);
    let next_cursor = compute_next_cursor(&raw_results, query.effective_limit());

    let explain = if query.explain {
        Some(plan.explain())
    } else {
        None
    };

    Outcome::Ok(SearchResponse {
        results: raw_results,
        next_cursor,
        explain,
        assistance,
        audit: Vec::new(),
    })
}

// ────────────────────────────────────────────────────────────────────
// Row mapping
// ────────────────────────────────────────────────────────────────────

/// Map database rows to `SearchResult` based on document kind.
fn map_rows_to_results(rows: &[SqlRow], doc_kind: DocKind) -> Vec<SearchResult> {
    rows.iter()
        .filter_map(|row| match doc_kind {
            DocKind::Message | DocKind::Thread => map_message_row(row),
            DocKind::Agent => map_agent_row(row),
            DocKind::Project => map_project_row(row),
        })
        .collect()
}

fn map_message_row(row: &SqlRow) -> Option<SearchResult> {
    let id: i64 = row.get_named("id").ok()?;
    let subject: String = row.get_named("subject").unwrap_or_default();
    let body: String = row.get_named("body_md").unwrap_or_default();
    let importance: Option<String> = row.get_named("importance").ok();
    let ack_required: Option<bool> = row.get_named::<i64>("ack_required").ok().map(|v| v != 0);
    let created_ts: Option<i64> = row.get_named("created_ts").ok();
    let thread_id: Option<String> = row.get_named("thread_id").ok();
    let from_agent: Option<String> = row.get_named("from_name").ok();
    let project_id: Option<i64> = row.get_named("project_id").ok();
    let score: Option<f64> = row.get_named("score").ok();

    Some(SearchResult {
        doc_kind: DocKind::Message,
        id,
        project_id,
        title: subject,
        body,
        score,
        importance,
        ack_required,
        created_ts,
        thread_id,
        from_agent,
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
        redacted: false,
        redaction_reason: None,
    })
}

fn map_agent_row(row: &SqlRow) -> Option<SearchResult> {
    let id: i64 = row.get_named("id").ok()?;
    let name: String = row.get_named("name").unwrap_or_default();
    let task_desc: String = row.get_named("task_description").unwrap_or_default();
    let project_id: Option<i64> = row.get_named("project_id").ok();
    let score: Option<f64> = row.get_named("score").ok();

    Some(SearchResult {
        doc_kind: DocKind::Agent,
        id,
        project_id,
        title: name,
        body: task_desc,
        score,
        importance: None,
        ack_required: None,
        created_ts: None,
        thread_id: None,
        from_agent: None,
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
        redacted: false,
        redaction_reason: None,
    })
}

fn map_project_row(row: &SqlRow) -> Option<SearchResult> {
    let id: i64 = row.get_named("id").ok()?;
    let slug: String = row.get_named("slug").unwrap_or_default();
    let human_key: String = row.get_named("human_key").unwrap_or_default();
    let score: Option<f64> = row.get_named("score").ok();

    Some(SearchResult {
        doc_kind: DocKind::Project,
        id,
        project_id: Some(id),
        title: slug,
        body: human_key,
        score,
        importance: None,
        ack_required: None,
        created_ts: None,
        thread_id: None,
        from_agent: None,
        reason_codes: Vec::new(),
        score_factors: Vec::new(),
        redacted: false,
        redaction_reason: None,
    })
}

// ────────────────────────────────────────────────────────────────────
// Pagination
// ────────────────────────────────────────────────────────────────────

/// Compute the next cursor if there are more results.
fn compute_next_cursor(results: &[SearchResult], limit: usize) -> Option<String> {
    if results.len() < limit {
        return None; // fewer than limit means no more pages
    }
    // Use the last result's (score, id) as cursor
    results.last().map(|r| {
        let cursor = SearchCursor {
            score: r.score.unwrap_or(0.0),
            id: r.id,
        };
        cursor.encode()
    })
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_planner::SearchCursor;
    use mcp_agent_mail_core::metrics::global_metrics;
    #[cfg(feature = "hybrid")]
    use std::time::Duration;

    #[test]
    fn plan_param_conversion() {
        assert!(matches!(
            plan_param_to_value(&PlanParam::Int(42)),
            Value::BigInt(42)
        ));
        assert!(matches!(
            plan_param_to_value(&PlanParam::Float(1.5)),
            Value::Double(_)
        ));
        if let Value::Text(s) = plan_param_to_value(&PlanParam::Text("hello".to_string())) {
            assert_eq!(s, "hello");
        } else {
            panic!("expected Text");
        }
    }

    #[test]
    fn next_cursor_none_when_underfull() {
        let results = vec![SearchResult {
            doc_kind: DocKind::Message,
            id: 1,
            project_id: Some(1),
            title: "t".to_string(),
            body: "b".to_string(),
            score: Some(-1.0),
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        }];
        assert!(compute_next_cursor(&results, 50).is_none());
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn next_cursor_present_when_full() {
        let results: Vec<SearchResult> = (0..50)
            .map(|i| SearchResult {
                doc_kind: DocKind::Message,
                id: i,
                project_id: Some(1),
                title: format!("t{i}"),
                body: String::new(),
                score: Some(-(i as f64)),
                importance: None,
                ack_required: None,
                created_ts: None,
                thread_id: None,
                from_agent: None,
                reason_codes: Vec::new(),
                score_factors: Vec::new(),
                redacted: false,
                redaction_reason: None,
            })
            .collect();
        let cursor = compute_next_cursor(&results, 50).unwrap();
        let decoded = SearchCursor::decode(&cursor).unwrap();
        assert_eq!(decoded.id, 49);
    }

    #[test]
    fn next_cursor_empty_results() {
        assert!(compute_next_cursor(&[], 50).is_none());
    }

    #[test]
    fn search_options_default() {
        let opts = SearchOptions::default();
        assert!(opts.scope_ctx.is_none());
        assert!(opts.redaction_policy.is_none());
        assert!(!opts.track_telemetry);
    }

    fn result_with_score(id: i64, score: f64) -> SearchResult {
        SearchResult {
            doc_kind: DocKind::Message,
            id,
            project_id: Some(1),
            title: format!("doc-{id}"),
            body: String::new(),
            score: Some(score),
            importance: None,
            ack_required: None,
            created_ts: None,
            thread_id: None,
            from_agent: None,
            reason_codes: Vec::new(),
            score_factors: Vec::new(),
            redacted: false,
            redaction_reason: None,
        }
    }

    fn passthrough_governor() -> HybridBudgetGovernorState {
        HybridBudgetGovernorState {
            remaining_budget_ms: None,
            tier: HybridBudgetGovernorTier::Unlimited,
            rerank_enabled: true,
        }
    }

    #[test]
    fn hybrid_orchestration_keeps_lexical_ordering_deterministic() {
        let query = SearchQuery::messages("incident rollback plan", 1);
        let derivation = CandidateBudget::derive_with_decision(
            query.effective_limit(),
            CandidateMode::Hybrid,
            QueryClass::classify(&query.text),
            CandidateBudgetConfig::default(),
        );
        let lexical = vec![
            result_with_score(10, 0.9),
            result_with_score(20, 0.8),
            result_with_score(30, 0.7),
        ];
        let semantic = vec![
            result_with_score(20, 0.99),
            result_with_score(40, 0.75),
            result_with_score(30, 0.6),
        ];

        let merged = orchestrate_hybrid_results(
            &query,
            &derivation,
            passthrough_governor(),
            lexical,
            semantic,
        );
        let ids = merged.iter().map(|result| result.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![10, 20, 40, 30]);
    }

    #[test]
    fn hybrid_orchestration_respects_requested_limit() {
        let mut query = SearchQuery::messages("search", 1);
        query.limit = Some(2);
        let derivation = CandidateBudget::derive_with_decision(
            query.effective_limit(),
            CandidateMode::Hybrid,
            QueryClass::classify(&query.text),
            CandidateBudgetConfig::default(),
        );
        let lexical = vec![
            result_with_score(1, 0.9),
            result_with_score(2, 0.8),
            result_with_score(3, 0.7),
        ];

        let merged = orchestrate_hybrid_results(
            &query,
            &derivation,
            passthrough_governor(),
            lexical,
            Vec::new(),
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[1].id, 2);
    }

    #[test]
    fn request_budget_remaining_ms_uses_cost_quota_without_deadline() {
        let cx = Cx::for_request_with_budget(Budget::new().with_cost_quota(87));
        assert_eq!(request_budget_remaining_ms(&cx), Some(87));
    }

    #[test]
    fn request_budget_remaining_ms_reports_expired_deadline_as_zero() {
        let cx = Cx::for_request_with_budget(Budget::new().with_deadline(Time::ZERO));
        assert_eq!(request_budget_remaining_ms(&cx), Some(0));
    }

    #[test]
    fn hybrid_budget_governor_critical_disables_semantic_and_rerank() {
        let base = CandidateBudget {
            lexical_limit: 120,
            semantic_limit: 80,
            combined_limit: 200,
        };
        let config = HybridBudgetGovernorConfig::default();
        let (budget, governor) = apply_hybrid_budget_governor(50, base, Some(100), config);

        assert_eq!(governor.tier, HybridBudgetGovernorTier::Critical);
        assert!(!governor.rerank_enabled);
        assert_eq!(budget.semantic_limit, 0);
        assert!(budget.lexical_limit >= 10);
        assert!(budget.lexical_limit <= base.lexical_limit);
        assert_eq!(budget.combined_limit, budget.lexical_limit);
    }

    #[test]
    fn hybrid_budget_governor_tight_scales_limits_deterministically() {
        let base = CandidateBudget {
            lexical_limit: 120,
            semantic_limit: 80,
            combined_limit: 200,
        };
        let config = HybridBudgetGovernorConfig::default();
        let (budget, governor) = apply_hybrid_budget_governor(50, base, Some(200), config);

        assert_eq!(governor.tier, HybridBudgetGovernorTier::Tight);
        assert!(!governor.rerank_enabled);
        assert_eq!(budget.lexical_limit, 84);
        assert_eq!(budget.semantic_limit, 56);
        assert_eq!(budget.combined_limit, 140);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn blend_rerank_score_replace_policy_uses_rerank_score() {
        let blended = blend_rerank_score(0.91, 0.27, RerankBlendPolicy::Replace, 0.8);
        assert!((blended - 0.27).abs() < f64::EPSILON);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn blend_rerank_score_weighted_policy_respects_weight() {
        let blended = blend_rerank_score(0.8, 0.2, RerankBlendPolicy::Weighted, 0.25);
        assert!((blended - 0.65).abs() < 1e-12);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn apply_rerank_scores_replace_policy_reorders_and_tie_breaks_by_id() {
        let mut merged = vec![
            result_with_score(11, 0.95),
            result_with_score(22, 0.85),
            result_with_score(33, 0.75),
        ];
        let rerank_scores = BTreeMap::from([(11_i64, 0.4_f64), (22_i64, 0.9_f64), (33_i64, 0.9)]);

        let applied = apply_rerank_scores_and_sort(
            merged.as_mut_slice(),
            &rerank_scores,
            RerankBlendPolicy::Replace,
            0.5,
        );

        assert_eq!(applied, 3);
        let ids = merged.iter().map(|result| result.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![22, 33, 11]);
        assert!((merged[0].score.unwrap_or_default() - 0.9).abs() < 1e-12);
        assert!((merged[1].score.unwrap_or_default() - 0.9).abs() < 1e-12);
        assert!((merged[2].score.unwrap_or_default() - 0.4).abs() < 1e-12);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn apply_rerank_scores_with_no_matches_preserves_scores() {
        let mut merged = vec![result_with_score(1, 0.8), result_with_score(2, 0.7)];
        let rerank_scores = BTreeMap::from([(10_i64, 0.3_f64)]);

        let applied = apply_rerank_scores_and_sort(
            merged.as_mut_slice(),
            &rerank_scores,
            RerankBlendPolicy::Weighted,
            0.5,
        );

        assert_eq!(applied, 0);
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[1].id, 2);
        assert!((merged[0].score.unwrap_or_default() - 0.8).abs() < 1e-12);
        assert!((merged[1].score.unwrap_or_default() - 0.7).abs() < 1e-12);
    }

    #[test]
    fn build_v3_query_explain_includes_engine_and_rerank_facets() {
        let query = SearchQuery {
            text: "outage rollback".to_string(),
            explain: true,
            ..SearchQuery::messages("outage rollback", 1)
        };
        let rerank_audit = HybridRerankAudit {
            enabled: true,
            attempted: true,
            outcome: "applied".to_string(),
            candidate_count: 24,
            top_k: 12,
            min_candidates: 5,
            blend_policy: Some("weighted".to_string()),
            blend_weight: Some(0.35),
            applied_count: 9,
        };

        let explain = build_v3_query_explain(&query, SearchEngine::Hybrid, Some(&rerank_audit));
        assert_eq!(explain.method, "hybrid_v3");
        assert_eq!(explain.facet_count, explain.facets_applied.len());
        assert!(
            explain
                .facets_applied
                .iter()
                .any(|facet| facet == "engine:hybrid")
        );
        assert!(
            explain
                .facets_applied
                .iter()
                .any(|facet| facet == "rerank_outcome:applied")
        );
        assert!(
            explain
                .facets_applied
                .iter()
                .any(|facet| facet == "rerank_applied_count:9")
        );
    }

    #[test]
    fn shadow_comparison_logging_updates_metrics_hook() {
        let before = global_metrics().snapshot();
        let query = SearchQuery::messages("shadow-hook", 1);
        let fts5 = vec![
            result_with_score(1, 0.9),
            result_with_score(2, 0.8),
            result_with_score(3, 0.7),
        ];
        let tantivy = vec![
            result_with_score(1, 0.88),
            result_with_score(2, 0.77),
            result_with_score(9, 0.66),
        ];

        log_shadow_comparison(&fts5, &tantivy, &query, 1500, 1100, false);

        let after = global_metrics().snapshot();
        assert!(
            after.search.shadow_comparisons_total > before.search.shadow_comparisons_total,
            "expected shadow comparison counter to increase (before={}, after={})",
            before.search.shadow_comparisons_total,
            after.search.shadow_comparisons_total
        );
    }

    #[test]
    fn v3_fallback_records_metric() {
        let before = global_metrics().snapshot();
        let query = SearchQuery::messages("fallback-check", 1);

        maybe_record_v3_fallback(SearchEngine::Lexical, &query);

        let after = global_metrics().snapshot();
        assert!(
            after.search.fallback_to_legacy_total > before.search.fallback_to_legacy_total,
            "expected fallback counter to increase (before={}, after={})",
            before.search.fallback_to_legacy_total,
            after.search.fallback_to_legacy_total
        );
    }

    #[test]
    fn legacy_error_metrics_record_error_counter() {
        let before = global_metrics().snapshot();

        record_legacy_error_metrics("search_service_test_error", 321, false);

        let after = global_metrics().snapshot();
        assert!(
            after.search.queries_errors_total > before.search.queries_errors_total,
            "expected error counter to increase (before={}, after={})",
            before.search.queries_errors_total,
            after.search.queries_errors_total
        );
    }

    #[test]
    fn query_assistance_payload_empty_for_plain_text() {
        let query = SearchQuery::messages("plain text query", 1);
        assert!(query_assistance_payload(&query).is_none());
    }

    #[test]
    fn query_assistance_payload_contains_hints_and_suggestions() {
        let query = SearchQuery::messages("form:BlueLake thread:br-123 migration", 1);
        let assistance = query_assistance_payload(&query).expect("assistance should be populated");
        assert_eq!(assistance.applied_filter_hints.len(), 1);
        assert_eq!(assistance.applied_filter_hints[0].field, "thread");
        assert_eq!(assistance.applied_filter_hints[0].value, "br-123");
        assert_eq!(assistance.did_you_mean.len(), 1);
        assert_eq!(assistance.did_you_mean[0].suggested_field, "from");
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn two_tier_entry_contract() {
        let config = TwoTierConfig::default();
        let mut index = TwoTierIndex::new(&config);

        let entry = TwoTierEntry {
            doc_id: 9,
            doc_kind: SearchDocKind::Message,
            project_id: Some(42),
            fast_embedding: vec![half::f16::from_f32(0.01); config.fast_dimension],
            quality_embedding: vec![half::f16::from_f32(0.02); config.quality_dimension],
            has_quality: true,
        };

        index
            .add_entry(entry)
            .expect("two-tier entry should be accepted with matching dimensions");

        let hits = index.search_fast(&vec![0.01_f32; config.fast_dimension], 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, 9);
        assert_eq!(hits[0].doc_kind, SearchDocKind::Message);
        assert_eq!(hits[0].project_id, Some(42));
    }

    #[cfg(feature = "hybrid")]
    #[allow(clippy::cast_possible_truncation)]
    fn make_scored(doc_id: u64, score: f32) -> ScoredResult {
        ScoredResult {
            idx: doc_id as usize,
            doc_id,
            doc_kind: SearchDocKind::Message,
            project_id: Some(7),
            score,
        }
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_best_two_tier_results_prefers_refined_phase() {
        let phases = vec![
            SearchPhase::Initial {
                results: vec![make_scored(1, 0.1)],
                latency_ms: 5,
            },
            SearchPhase::Refined {
                results: vec![make_scored(2, 0.9)],
                latency_ms: 21,
            },
        ];

        let selected =
            select_best_two_tier_results(phases).expect("expected at least one usable phase");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].doc_id, 2);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_best_two_tier_results_keeps_initial_on_refinement_failure() {
        let phases = vec![
            SearchPhase::Initial {
                results: vec![make_scored(11, 0.7)],
                latency_ms: 6,
            },
            SearchPhase::RefinementFailed {
                error: "quality embedder unavailable".to_string(),
            },
        ];

        let selected =
            select_best_two_tier_results(phases).expect("initial phase should be preserved");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].doc_id, 11);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_best_two_tier_results_keeps_initial_when_refined_is_empty() {
        let phases = vec![
            SearchPhase::Initial {
                results: vec![make_scored(17, 0.8)],
                latency_ms: 4,
            },
            SearchPhase::Refined {
                results: Vec::new(),
                latency_ms: 12,
            },
        ];

        let selected =
            select_best_two_tier_results(phases).expect("initial phase should be preserved");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].doc_id, 17);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_best_two_tier_results_none_for_empty_iterator() {
        let phases: Vec<SearchPhase> = Vec::new();
        assert!(select_best_two_tier_results(phases).is_none());
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_fast_first_two_tier_results_prefers_initial_phase() {
        let phases = vec![
            SearchPhase::Initial {
                results: vec![make_scored(11, 0.7)],
                latency_ms: 4,
            },
            SearchPhase::Refined {
                results: vec![make_scored(99, 0.99)],
                latency_ms: 18,
            },
        ];

        let selected = select_fast_first_two_tier_results(phases)
            .expect("fast-first selection should return initial phase");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].doc_id, 11);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_fast_first_two_tier_results_falls_back_to_refined_when_initial_empty() {
        let phases = vec![
            SearchPhase::Initial {
                results: Vec::new(),
                latency_ms: 3,
            },
            SearchPhase::Refined {
                results: vec![make_scored(7, 0.91)],
                latency_ms: 14,
            },
        ];

        let selected = select_fast_first_two_tier_results(phases)
            .expect("fast-first selection should use refined phase when initial is empty");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].doc_id, 7);
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn select_fast_first_two_tier_results_none_for_empty_iterator() {
        let phases: Vec<SearchPhase> = Vec::new();
        assert!(select_fast_first_two_tier_results(phases).is_none());
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn semantic_enqueue_auto_initializes_bridge_and_tracks_dedup() {
        assert!(enqueue_semantic_document(
            DocKind::Message,
            4242,
            Some(7),
            "Initial subject",
            "Initial body"
        ));
        assert!(enqueue_semantic_document(
            DocKind::Message,
            4242,
            Some(7),
            "Updated subject",
            "Updated body"
        ));

        let snapshot =
            semantic_indexing_snapshot().expect("semantic indexing bridge should be initialized");
        assert!(snapshot.queue.total_enqueued >= 1);
        assert!(snapshot.queue.total_deduped >= 1);
        let health =
            semantic_indexing_health().expect("semantic indexing health snapshot should exist");
        assert!(health.queue.total_enqueued >= 1);
    }

    #[cfg(feature = "hybrid")]
    #[derive(Debug)]
    struct FixedSemanticTestEmbedder {
        info: ModelInfo,
    }

    #[cfg(feature = "hybrid")]
    impl FixedSemanticTestEmbedder {
        fn new(dimension: usize) -> Self {
            Self {
                info: ModelInfo::new(
                    "fixed-semantic-test",
                    "Fixed Semantic Test",
                    ModelTier::Fast,
                    dimension,
                    4096,
                )
                .with_available(true),
            }
        }
    }

    #[cfg(feature = "hybrid")]
    impl Embedder for FixedSemanticTestEmbedder {
        fn embed(
            &self,
            text: &str,
        ) -> mcp_agent_mail_search_core::error::SearchResult<EmbeddingResult> {
            Ok(EmbeddingResult::new(
                vec![0.42_f32; self.info.dimension],
                self.info.id.clone(),
                ModelTier::Fast,
                Duration::from_millis(1),
                mcp_agent_mail_search_core::canonical::content_hash(text),
            ))
        }

        fn model_info(&self) -> &ModelInfo {
            &self.info
        }
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn semantic_bridge_pipeline_runs_enqueue_process_and_index_search() {
        let bridge = SemanticBridge::new_with_embedder(
            VectorIndexConfig {
                dimension: 4,
                ..Default::default()
            },
            Arc::new(FixedSemanticTestEmbedder::new(4)),
        );

        assert!(bridge.enqueue_document(
            7001,
            SearchDocKind::Message,
            Some(77),
            "Bridge Subject",
            "Bridge Body"
        ));
        let before = bridge.queue_stats();
        assert_eq!(before.pending_count, 1);

        let processed = bridge.refresh_worker.run_cycle();
        assert_eq!(processed, 1);

        let after = bridge.queue_stats();
        assert_eq!(after.pending_count, 0);
        assert_eq!(after.retry_count, 0);

        let metrics = bridge.metrics_snapshot();
        assert_eq!(metrics.total_succeeded, 1);
        assert_eq!(metrics.total_retryable, 0);
        assert_eq!(metrics.total_failed, 0);

        let hits = bridge
            .index()
            .search(&[0.42_f32; 4], 8, None)
            .expect("vector index search should succeed");
        assert!(
            hits.iter().any(|hit| hit.doc_id == 7001),
            "indexed document should be retrievable from vector index"
        );
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn try_two_tier_search_lazy_initializes_bridge() {
        let query = SearchQuery::messages("auto-init semantic bridge", 1);
        let _ = try_two_tier_search(&query, query.effective_limit());
        assert!(get_two_tier_bridge().is_some());
    }

    #[cfg(feature = "hybrid")]
    fn two_tier_test_bridge() -> Arc<TwoTierBridge> {
        let config = mcp_agent_mail_search_core::TwoTierConfig::default();
        Arc::new(TwoTierBridge {
            index: std::sync::RwLock::new(mcp_agent_mail_search_core::TwoTierIndex::new(&config)),
            config,
        })
    }

    #[cfg(feature = "hybrid")]
    #[test]
    #[allow(clippy::needless_collect)]
    fn get_or_init_two_tier_bridge_initializes_once_under_contention() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let slot = Arc::new(OnceLock::<Option<Arc<TwoTierBridge>>>::new());
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let init_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let barrier = Arc::clone(&barrier);
                let init_count = Arc::clone(&init_count);
                thread::spawn(move || {
                    barrier.wait();
                    get_or_init_two_tier_bridge_with(slot.as_ref(), || {
                        init_count.fetch_add(1, Ordering::Relaxed);
                        Some(two_tier_test_bridge())
                    })
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread should not panic"))
            .collect();

        assert!(
            results.iter().all(Option::is_some),
            "all threads should observe initialized bridge"
        );

        let first = results[0]
            .as_ref()
            .expect("first result should contain the shared bridge");
        for maybe_bridge in &results[1..] {
            let bridge = maybe_bridge
                .as_ref()
                .expect("every thread should see the shared bridge");
            assert!(
                Arc::ptr_eq(first, bridge),
                "all threads should share one Arc"
            );
        }

        assert_eq!(
            init_count.load(Ordering::Relaxed),
            1,
            "initializer path (and init log) must run exactly once"
        );
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn get_or_init_two_tier_bridge_caches_init_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let slot = OnceLock::<Option<Arc<TwoTierBridge>>>::new();
        let init_count = AtomicUsize::new(0);

        let first = get_or_init_two_tier_bridge_with(&slot, || {
            init_count.fetch_add(1, Ordering::Relaxed);
            None
        });
        let second = get_or_init_two_tier_bridge_with(&slot, || {
            init_count.fetch_add(1, Ordering::Relaxed);
            Some(two_tier_test_bridge())
        });

        assert!(first.is_none(), "first init failure should return None");
        assert!(
            second.is_none(),
            "cached failure should remain None without rerunning initialization"
        );
        assert_eq!(
            init_count.load(Ordering::Relaxed),
            1,
            "failure initializer should be executed once"
        );
    }

    #[cfg(feature = "hybrid")]
    #[test]
    #[ignore = "slow: requires ML embedder initialization (60+ seconds)"]
    fn get_or_init_two_tier_bridge_is_thread_safe() {
        // Verify that concurrent calls to get_or_init_two_tier_bridge all return
        // the same Arc instance (pointer equality), proving no duplicate bridges
        // are created under concurrent access.
        use std::thread;

        let barrier = Arc::new(std::sync::Barrier::new(10));
        let results: Vec<_> = (0..10)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    // All threads wait here, then race to initialize
                    barrier.wait();
                    get_or_init_two_tier_bridge().map(|arc| Arc::as_ptr(&arc) as usize)
                })
            })
            .filter_map(|h| h.join().ok())
            .collect();

        // All 10 threads should complete (no panics).
        assert_eq!(results.len(), 10, "all 10 threads should complete");

        // All threads should have gotten Some(bridge)
        assert!(
            results.iter().all(std::option::Option::is_some),
            "all threads should get a bridge"
        );

        // All pointers should be equal (same Arc instance)
        let first_ptr = results[0].unwrap();
        assert!(
            results.iter().all(|r| r.unwrap() == first_ptr),
            "all threads should get the same Arc<TwoTierBridge> instance"
        );
    }

    #[cfg(feature = "hybrid")]
    #[test]
    fn get_or_init_two_tier_bridge_cached_access_is_fast() {
        // First call initializes the bridge (may take time due to embedder init)
        let _ = get_or_init_two_tier_bridge();

        // Subsequent calls should be nearly instant (just Arc clone)
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = get_or_init_two_tier_bridge();
        }
        let elapsed = start.elapsed();

        // 1000 cached accesses should complete in <10ms (avg <10µs each)
        assert!(
            elapsed.as_millis() < 10,
            "1000 cached accesses took {elapsed:?}, expected <10ms"
        );
    }

    #[cfg(feature = "hybrid")]
    #[test]
    #[ignore = "slow: requires ML embedder initialization (60+ seconds), high thread count"]
    fn get_or_init_two_tier_bridge_stress_100_threads() {
        // High-contention stress test with 100 threads
        use std::thread;

        let barrier = Arc::new(std::sync::Barrier::new(100));
        let results: Vec<_> = (0..100)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    get_or_init_two_tier_bridge().map(|arc| Arc::as_ptr(&arc) as usize)
                })
            })
            .filter_map(|h| h.join().ok())
            .collect();

        // All 100 threads should succeed
        assert_eq!(results.len(), 100, "all 100 threads should complete");
        assert!(
            results.iter().all(std::option::Option::is_some),
            "all threads should get a bridge"
        );

        // All should point to the same Arc
        let first_ptr = results[0].unwrap();
        let all_same = results.iter().all(|r| r.unwrap() == first_ptr);
        assert!(
            all_same,
            "all 100 threads should get the same Arc<TwoTierBridge>"
        );
    }
}
