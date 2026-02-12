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

use asupersync::{Cx, Outcome};
use mcp_agent_mail_search_core::{
    CandidateBudget, CandidateBudgetConfig, CandidateHit, CandidateMode, QueryAssistance,
    QueryClass, parse_query_assistance, prepare_candidates,
};
#[cfg(feature = "hybrid")]
use mcp_agent_mail_search_core::{
    DocKind as SearchDocKind, ModelRegistry, ModelTier, RegistryConfig, VectorFilter, VectorIndex,
    VectorIndexConfig,
};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row as SqlRow, Value};
use sqlmodel_query::raw_query;
#[cfg(feature = "hybrid")]
use std::sync::{Arc, OnceLock, RwLock};
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

/// Bridge to the semantic search infrastructure (vector index + embedder).
#[cfg(feature = "hybrid")]
pub struct SemanticBridge {
    /// The vector index holding document embeddings.
    index: RwLock<VectorIndex>,
    /// The model registry for obtaining embedders.
    registry: RwLock<ModelRegistry>,
}

#[cfg(feature = "hybrid")]
impl SemanticBridge {
    /// Create a new semantic bridge with the given configuration.
    #[must_use]
    pub fn new(config: VectorIndexConfig) -> Self {
        Self {
            index: RwLock::new(VectorIndex::new(config)),
            registry: RwLock::new(ModelRegistry::new(RegistryConfig::default())),
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
        self.registry().has_real_embedder()
    }

    /// Search for semantically similar documents.
    ///
    /// Embeds the query text and performs vector similarity search.
    pub fn search(&self, query: &SearchQuery, limit: usize) -> Vec<SearchResult> {
        let registry = self.registry();

        // Get the best available embedder (will fallback to hash if no real model)
        let Ok(embedder) = registry.get_embedder(ModelTier::Fast) else {
            return Vec::new();
        };

        // If only hash embedder is available, semantic search won't produce
        // meaningful similarity scores, so return empty to degrade gracefully
        if embedder.model_info().tier == ModelTier::Hash {
            tracing::debug!(
                target: "search.semantic",
                "no real embedder available, skipping semantic search"
            );
            return Vec::new();
        }

        // Embed the query text
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
        drop(registry);

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
                redacted: false,
                redaction_reason: None,
            })
            .collect()
    }
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

/// Initialize the global semantic search bridge.
///
/// Should be called once at startup when hybrid search is enabled.
#[cfg(feature = "hybrid")]
pub fn init_semantic_bridge(config: VectorIndexConfig) -> Result<(), String> {
    let bridge = SemanticBridge::new(config);
    let _ = SEMANTIC_BRIDGE.set(Some(Arc::new(bridge)));
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

/// Try executing semantic candidate retrieval for hybrid mode.
///
/// Semantic retrieval is intentionally optional at this stage: if no semantic
/// backend is initialized yet, we return `None` and the orchestration stage
/// degrades to lexical-only while preserving deterministic behavior.
#[cfg(feature = "hybrid")]
fn try_semantic_search(query: &SearchQuery, limit: usize) -> Option<Vec<SearchResult>> {
    let bridge = get_semantic_bridge()?;
    let results = bridge.search(query, limit);
    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Fallback for when hybrid feature is not enabled.
#[cfg(not(feature = "hybrid"))]
const fn try_semantic_search(_query: &SearchQuery, _limit: usize) -> Option<Vec<SearchResult>> {
    None
}

fn orchestrate_hybrid_results(
    query: &SearchQuery,
    engine: SearchEngine,
    lexical_results: Vec<SearchResult>,
    semantic_results: Vec<SearchResult>,
) -> Vec<SearchResult> {
    let requested_limit = query.effective_limit();
    let mode = match engine {
        SearchEngine::Hybrid => CandidateMode::Hybrid,
        SearchEngine::Auto => CandidateMode::Auto,
        _ => CandidateMode::LexicalFallback,
    };
    let query_class = QueryClass::classify(&query.text);
    let budget = CandidateBudget::derive(
        requested_limit,
        mode,
        query_class,
        CandidateBudgetConfig::default(),
    );

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
        mode = ?mode,
        query_class = ?query_class,
        lexical_considered = prepared.counts.lexical_considered,
        semantic_considered = prepared.counts.semantic_considered,
        lexical_selected = prepared.counts.lexical_selected,
        semantic_selected = prepared.counts.semantic_selected,
        deduped_selected = prepared.counts.deduped_selected,
        duplicates_removed = prepared.counts.duplicates_removed,
        "hybrid candidate orchestration completed"
    );

    merged
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
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            if options.track_telemetry {
                record_query("search_service_tantivy", latency_us);
            }
            // Record V3 metrics
            global_metrics().search.record_v3_query(latency_us, false);
            return finish_scoped_response(raw_results, query, options, assistance.clone());
        }
        // Bridge not initialized → fall through to FTS5
    }

    // ── Hybrid candidate orchestration path ─────────────────────────
    //
    // Stage order:
    // 1) lexical candidate retrieval (Tantivy bridge)
    // 2) semantic candidate retrieval (optional; may be unavailable)
    // 3) deterministic dedupe + merge prep (mode-aware budgets)
    // Fusion/rerank stages are implemented by follow-up Search V3 tracks.
    if matches!(engine, SearchEngine::Hybrid | SearchEngine::Auto) {
        if let Some(lexical_results) = try_tantivy_search(query) {
            let semantic_results =
                try_semantic_search(query, query.effective_limit()).unwrap_or_default();
            let raw_results =
                orchestrate_hybrid_results(query, engine, lexical_results, semantic_results);
            let latency_us = u64::try_from(timer.elapsed().as_micros()).unwrap_or(u64::MAX);
            if options.track_telemetry {
                record_query("search_service_hybrid_candidates", latency_us);
            }
            global_metrics().search.record_v3_query(latency_us, false);
            return finish_scoped_response(raw_results, query, options, assistance.clone());
        }
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
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    // Step 3: Execute SQL
    let values: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
    let rows_out = map_sql_outcome(raw_query(cx, &*conn, &plan.sql, &values).await);

    let rows = match rows_out {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => return Outcome::Err(e),
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
    let audit = if scope_ctx.viewer.is_some() {
        Some(audit_summary)
    } else {
        None
    };
    Outcome::Ok(ScopedSearchResponse {
        results: scoped_results,
        next_cursor,
        explain: None,
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
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let values: Vec<Value> = plan.params.iter().map(plan_param_to_value).collect();
    let rows_out = map_sql_outcome(raw_query(cx, &*conn, &plan.sql, &values).await);

    let rows = match rows_out {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => return Outcome::Err(e),
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
            redacted: false,
            redaction_reason: None,
        }
    }

    #[test]
    fn hybrid_orchestration_keeps_lexical_ordering_deterministic() {
        let query = SearchQuery::messages("incident rollback plan", 1);
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

        let merged = orchestrate_hybrid_results(&query, SearchEngine::Hybrid, lexical, semantic);
        let ids = merged.iter().map(|result| result.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![10, 20, 40, 30]);
    }

    #[test]
    fn hybrid_orchestration_respects_requested_limit() {
        let mut query = SearchQuery::messages("search", 1);
        query.limit = Some(2);
        let lexical = vec![
            result_with_score(1, 0.9),
            result_with_score(2, 0.8),
            result_with_score(3, 0.7),
        ];

        let merged = orchestrate_hybrid_results(&query, SearchEngine::Hybrid, lexical, Vec::new());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[1].id, 2);
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
}
