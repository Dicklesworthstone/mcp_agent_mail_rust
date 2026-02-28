//! Domain-specific search types for MCP Agent Mail.
//!
//! These types model the mail-specific search domain: documents, queries,
//! results, and the explain/scoring taxonomy. They are consumed by both the
//! search-core crate (which provides engine abstractions) and the db crate
//! (which wires everything together).

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

// ── Document types ──────────────────────────────────────────────────────────

/// Unique identifier for a document in the search index
pub type DocId = i64;

/// The kind of document (maps to different index schemas)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A message (subject + body)
    Message,
    /// An agent profile
    Agent,
    /// A project
    Project,
    /// A thread (aggregated from messages)
    Thread,
}

impl std::fmt::Display for DocKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => write!(f, "message"),
            Self::Agent => write!(f, "agent"),
            Self::Project => write!(f, "project"),
            Self::Thread => write!(f, "thread"),
        }
    }
}

/// A document to be indexed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Unique ID within the document kind
    pub id: DocId,
    /// What kind of entity this document represents
    pub kind: DocKind,
    /// Primary text content (e.g., message body, agent description)
    pub body: String,
    /// Secondary text content (e.g., message subject, agent name)
    pub title: String,
    /// The project this document belongs to (for scoping)
    pub project_id: Option<i64>,
    /// Timestamp in microseconds since epoch
    pub created_ts: i64,
    /// Structured metadata for faceted search
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Describes a change to a document for incremental index updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocChange {
    /// A new document was created or an existing one was updated
    Upsert(Document),
    /// A document was deleted
    Delete {
        /// The ID of the deleted document
        id: DocId,
        /// The kind of the deleted document
        kind: DocKind,
    },
}

impl DocChange {
    /// Returns the document ID affected by this change
    #[must_use]
    pub const fn doc_id(&self) -> DocId {
        match self {
            Self::Upsert(doc) => doc.id,
            Self::Delete { id, .. } => *id,
        }
    }

    /// Returns the document kind affected by this change
    #[must_use]
    pub const fn doc_kind(&self) -> DocKind {
        match self {
            Self::Upsert(doc) => doc.kind,
            Self::Delete { kind, .. } => *kind,
        }
    }
}

// ── Query types ─────────────────────────────────────────────────────────────

/// Which search algorithm to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Full-text lexical search (FTS5 or Tantivy)
    Lexical,
    /// Vector similarity search (embeddings)
    Semantic,
    /// Two-tier fusion: lexical candidates refined by semantic reranking
    Hybrid,
    /// Engine picks the best mode based on query characteristics
    #[default]
    Auto,
}

impl std::fmt::Display for SearchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lexical => write!(f, "lexical"),
            Self::Semantic => write!(f, "semantic"),
            Self::Hybrid => write!(f, "hybrid"),
            Self::Auto => write!(f, "auto"),
        }
    }
}

/// Date range filter (inclusive on both ends)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateRange {
    /// Start timestamp in microseconds since epoch (inclusive)
    pub start: Option<i64>,
    /// End timestamp in microseconds since epoch (inclusive)
    pub end: Option<i64>,
}

/// Importance level filter
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImportanceFilter {
    /// Match any importance level
    #[default]
    Any,
    /// Only urgent messages
    Urgent,
    /// Urgent or high importance
    High,
    /// Normal importance only
    Normal,
    /// Low importance only
    Low,
}

/// Structured filters applied to search results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchFilter {
    /// Filter by sender agent name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Filter by project ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i64>,
    /// Filter by date range
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_range: Option<DateRange>,
    /// Filter by importance level
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<ImportanceFilter>,
    /// Filter by thread ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Filter by document kind
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_kind: Option<DocKind>,
}

/// A search query with mode selection, filters, and pagination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// The raw query string
    pub raw_query: String,
    /// Which search mode to use
    #[serde(default)]
    pub mode: SearchMode,
    /// Structured filters
    #[serde(default)]
    pub filters: SearchFilter,
    /// Whether to include an explain report with scoring details
    #[serde(default)]
    pub explain: bool,
    /// Maximum number of results to return
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Offset for pagination
    #[serde(default)]
    pub offset: usize,
}

const fn default_limit() -> usize {
    20
}

impl SearchQuery {
    /// Create a new search query with default settings
    #[must_use]
    pub fn new(raw_query: impl Into<String>) -> Self {
        Self {
            raw_query: raw_query.into(),
            mode: SearchMode::default(),
            filters: SearchFilter::default(),
            explain: false,
            limit: default_limit(),
            offset: 0,
        }
    }

    /// Set the search mode
    #[must_use]
    pub const fn with_mode(mut self, mode: SearchMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the result limit
    #[must_use]
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the offset for pagination
    #[must_use]
    pub const fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Enable explain mode
    #[must_use]
    pub const fn with_explain(mut self) -> Self {
        self.explain = true;
        self
    }

    /// Set the search filters
    #[must_use]
    pub fn with_filters(mut self, filters: SearchFilter) -> Self {
        self.filters = filters;
        self
    }
}

// ── Results types ───────────────────────────────────────────────────────────

/// A byte range within a text field that should be highlighted
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HighlightRange {
    /// Field name (e.g., "body", "title")
    pub field: String,
    /// Start byte offset (inclusive)
    pub start: usize,
    /// End byte offset (exclusive)
    pub end: usize,
}

/// A single search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// Document ID
    pub doc_id: DocId,
    /// Document kind
    pub doc_kind: DocKind,
    /// Relevance score (higher is better, engine-specific scale)
    pub score: f64,
    /// Optional text snippet with matched terms highlighted
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Byte ranges to highlight in the original document
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub highlight_ranges: Vec<HighlightRange>,
    /// Additional metadata from the index (e.g., sender, subject, `thread_id`)
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Canonical explanation stage ordering for multi-stage ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplainStage {
    /// Lexical candidate generation (BM25 / keyword retrieval).
    Lexical,
    /// Semantic retrieval / similarity pass.
    Semantic,
    /// Fusion pass combining lexical + semantic evidence.
    Fusion,
    /// Final reranking pass (policy/business adjustments).
    Rerank,
}

impl ExplainStage {
    /// Canonical stage ordering used for deterministic explain output.
    #[must_use]
    pub const fn canonical_order() -> [Self; 4] {
        [Self::Lexical, Self::Semantic, Self::Fusion, Self::Rerank]
    }
}

/// Machine-stable reason codes for explainability across ranking stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplainReasonCode {
    /// Primary lexical BM25 signal.
    LexicalBm25,
    /// Lexical term overlap / coverage adjustment.
    LexicalTermCoverage,
    /// Semantic cosine (or equivalent vector) similarity.
    SemanticCosine,
    /// Semantic neighborhood / proximity adjustment.
    SemanticNeighborhood,
    /// Weighted fusion blend of multi-stage signals.
    FusionWeightedBlend,
    /// Positive reranking adjustment.
    RerankPolicyBoost,
    /// Negative reranking adjustment.
    RerankPolicyPenalty,
    /// Stage was not executed for this query/mode.
    StageNotExecuted,
    /// Stage details were redacted due to scope policy.
    ScopeRedacted,
    /// Hit denied by scope policy.
    ScopeDenied,
}

impl ExplainReasonCode {
    /// Human-readable summary string for operator-facing diagnostics.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::LexicalBm25 => "Lexical BM25 match",
            Self::LexicalTermCoverage => "Lexical term coverage adjustment",
            Self::SemanticCosine => "Semantic similarity contribution",
            Self::SemanticNeighborhood => "Semantic neighborhood contribution",
            Self::FusionWeightedBlend => "Weighted lexical/semantic fusion",
            Self::RerankPolicyBoost => "Policy rerank boost",
            Self::RerankPolicyPenalty => "Policy rerank penalty",
            Self::StageNotExecuted => "Stage not executed",
            Self::ScopeRedacted => "Explanation redacted by scope policy",
            Self::ScopeDenied => "Explanation denied by scope policy",
        }
    }
}

/// Verbosity controls for explanation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExplainVerbosity {
    /// High-level stage summaries only; no factor detail.
    Minimal,
    /// Stage summaries with truncated factor list.
    #[default]
    Standard,
    /// Full factor detail for debugging.
    Detailed,
}

/// A deterministic score factor used to compose stage explanations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreFactor {
    /// Canonical reason code for the factor.
    pub code: ExplainReasonCode,
    /// Stable key for machine/UI rendering (e.g. `bm25`, `term_coverage`).
    pub key: String,
    /// Numeric contribution to stage score.
    pub contribution: f64,
    /// Optional detailed note (only present in detailed verbosity).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A single stage-level explanation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageExplanation {
    /// Stage identifier (lexical/semantic/fusion/rerank).
    pub stage: ExplainStage,
    /// Canonical reason code for the stage outcome.
    pub reason_code: ExplainReasonCode,
    /// Human-readable stage summary.
    pub summary: String,
    /// Stage-local score before weighting.
    pub stage_score: f64,
    /// Stage weight used in final aggregation.
    pub stage_weight: f64,
    /// Weighted contribution to final score.
    pub weighted_score: f64,
    /// Truncated/sorted factors (shape depends on verbosity).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub score_factors: Vec<ScoreFactor>,
    /// Number of factors omitted by truncation or verbosity reduction.
    #[serde(default)]
    pub truncated_factor_count: usize,
    /// Whether this stage explanation was redacted by scope policy.
    #[serde(default)]
    pub redacted: bool,
}

/// Input used by the explain compositor for each stage.
#[derive(Debug, Clone)]
pub struct StageScoreInput {
    /// Stage identifier.
    pub stage: ExplainStage,
    /// Canonical reason code for the stage.
    pub reason_code: ExplainReasonCode,
    /// Optional human summary override.
    pub summary: Option<String>,
    /// Stage-local score before weighting.
    pub stage_score: f64,
    /// Stage weight in final score aggregation.
    pub stage_weight: f64,
    /// Raw factors to be deterministically sorted and truncated.
    pub score_factors: Vec<ScoreFactor>,
}

/// Configuration for deterministic explain composition.
#[derive(Debug, Clone)]
pub struct ExplainComposerConfig {
    /// Detail level to emit.
    pub verbosity: ExplainVerbosity,
    /// Maximum score factors retained per stage.
    pub max_factors_per_stage: usize,
}

impl Default for ExplainComposerConfig {
    fn default() -> Self {
        Self {
            verbosity: ExplainVerbosity::Standard,
            max_factors_per_stage: 4,
        }
    }
}

/// Scoring explanation for a single hit (when explain mode is on)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitExplanation {
    /// The document ID
    pub doc_id: DocId,
    /// Final fused score after all ranking stages
    pub final_score: f64,
    /// Per-stage explanations in canonical stage order
    pub stages: Vec<StageExplanation>,
    /// Canonical reason codes observed across this hit's stages
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reason_codes: Vec<ExplainReasonCode>,
}

/// Top-level explain report returned when `SearchQuery.explain` is true
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainReport {
    /// Per-hit scoring explanations
    pub hits: Vec<HitExplanation>,
    /// Which mode was actually used (relevant when mode=Auto)
    pub mode_used: SearchMode,
    /// Total candidate count before limit/offset
    pub candidates_evaluated: usize,
    /// Time spent in each search phase
    pub phase_timings: HashMap<String, Duration>,
    /// Stable taxonomy version for reason-code compatibility.
    #[serde(default = "default_taxonomy_version")]
    pub taxonomy_version: u32,
    /// Canonical stage order to guide clients/renderers.
    #[serde(default = "default_stage_order")]
    pub stage_order: Vec<ExplainStage>,
    /// Explain detail level used while composing this report.
    #[serde(default)]
    pub verbosity: ExplainVerbosity,
}

/// The complete result of a search query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    /// Matched documents, ordered by score descending
    pub hits: Vec<SearchHit>,
    /// Total number of matching documents (before limit/offset)
    pub total_count: usize,
    /// Which search mode was actually used
    pub mode_used: SearchMode,
    /// Optional explain report (only present when `SearchQuery.explain` is true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<ExplainReport>,
    /// Wall-clock time for the search operation
    pub elapsed: Duration,
}

const fn default_taxonomy_version() -> u32 {
    1
}

fn default_stage_order() -> Vec<ExplainStage> {
    ExplainStage::canonical_order().to_vec()
}

// ── Explain composition functions ───────────────────────────────────────────

/// Deterministic sort comparator for score factors (by |contribution| desc, then code, then key).
#[must_use]
pub fn factor_sort_cmp(a: &ScoreFactor, b: &ScoreFactor) -> Ordering {
    b.contribution
        .abs()
        .partial_cmp(&a.contribution.abs())
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.code.cmp(&b.code))
        .then_with(|| a.key.cmp(&b.key))
}

fn compose_stage(mut input: StageScoreInput, config: &ExplainComposerConfig) -> StageExplanation {
    input.score_factors.sort_by(factor_sort_cmp);
    let total_factor_count = input.score_factors.len();

    let mut score_factors =
        if config.verbosity == ExplainVerbosity::Minimal || config.max_factors_per_stage == 0 {
            Vec::new()
        } else {
            input
                .score_factors
                .into_iter()
                .take(config.max_factors_per_stage)
                .collect()
        };

    if config.verbosity != ExplainVerbosity::Detailed {
        for factor in &mut score_factors {
            factor.detail = None;
        }
    }

    let truncated_factor_count = total_factor_count.saturating_sub(score_factors.len());
    let summary = input
        .summary
        .unwrap_or_else(|| input.reason_code.summary().to_owned());

    StageExplanation {
        stage: input.stage,
        reason_code: input.reason_code,
        summary,
        stage_score: input.stage_score,
        stage_weight: input.stage_weight,
        weighted_score: input.stage_score * input.stage_weight,
        score_factors,
        truncated_factor_count,
        redacted: false,
    }
}

/// Create a placeholder explanation for a stage that was not executed.
#[must_use]
pub fn missing_stage(stage: ExplainStage) -> StageExplanation {
    StageExplanation {
        stage,
        reason_code: ExplainReasonCode::StageNotExecuted,
        summary: ExplainReasonCode::StageNotExecuted.summary().to_owned(),
        stage_score: 0.0,
        stage_weight: 0.0,
        weighted_score: 0.0,
        score_factors: Vec::new(),
        truncated_factor_count: 0,
        redacted: false,
    }
}

/// Compose a deterministic multi-stage explanation for a single hit.
///
/// - Stages are emitted in canonical order (lexical, semantic, fusion, rerank).
/// - Missing stages are represented with `stage_not_executed`.
/// - Factors are sorted deterministically and truncated by config.
#[must_use]
pub fn compose_hit_explanation(
    doc_id: DocId,
    final_score: f64,
    stage_inputs: Vec<StageScoreInput>,
    config: &ExplainComposerConfig,
) -> HitExplanation {
    let mut per_stage: HashMap<ExplainStage, StageScoreInput> = HashMap::new();

    for mut input in stage_inputs {
        if let Some(existing) = per_stage.get_mut(&input.stage) {
            existing.stage_score += input.stage_score;
            existing.stage_weight = existing.stage_weight.max(input.stage_weight);
            if existing.summary.is_none() {
                existing.summary = input.summary.take();
            }
            existing.score_factors.append(&mut input.score_factors);
            if existing.reason_code == ExplainReasonCode::StageNotExecuted {
                existing.reason_code = input.reason_code;
            }
        } else {
            per_stage.insert(input.stage, input);
        }
    }

    let stages: Vec<StageExplanation> = ExplainStage::canonical_order()
        .into_iter()
        .map(|stage| {
            per_stage.remove(&stage).map_or_else(
                || missing_stage(stage),
                |input| compose_stage(input, config),
            )
        })
        .collect();

    let reason_codes = stages
        .iter()
        .map(|stage| stage.reason_code)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    HitExplanation {
        doc_id,
        final_score,
        stages,
        reason_codes,
    }
}

/// Compose the top-level explain report with stable metadata.
#[must_use]
pub fn compose_explain_report(
    mode_used: SearchMode,
    candidates_evaluated: usize,
    phase_timings: HashMap<String, Duration, impl std::hash::BuildHasher>,
    hits: Vec<HitExplanation>,
    config: &ExplainComposerConfig,
) -> ExplainReport {
    ExplainReport {
        hits,
        mode_used,
        candidates_evaluated,
        phase_timings: phase_timings.into_iter().collect(),
        taxonomy_version: default_taxonomy_version(),
        stage_order: default_stage_order(),
        verbosity: config.verbosity,
    }
}

/// Redact stage-level details for a single hit explanation.
///
/// This is used for restricted-scope responses where ranking internals must be
/// hidden while preserving deterministic schema shape.
pub fn redact_hit_explanation(hit: &mut HitExplanation, reason_code: ExplainReasonCode) {
    hit.final_score = 0.0;
    hit.reason_codes = vec![reason_code];
    for stage in &mut hit.stages {
        stage.reason_code = reason_code;
        reason_code.summary().clone_into(&mut stage.summary);
        stage.stage_score = 0.0;
        stage.stage_weight = 0.0;
        stage.weighted_score = 0.0;
        stage.score_factors.clear();
        stage.truncated_factor_count = 0;
        stage.redacted = true;
    }
}

/// Redact explain details for selected documents in a report.
pub fn redact_report_for_docs(
    report: &mut ExplainReport,
    doc_ids: &BTreeSet<DocId>,
    reason_code: ExplainReasonCode,
) {
    for hit in &mut report.hits {
        if doc_ids.contains(&hit.doc_id) {
            redact_hit_explanation(hit, reason_code);
        }
    }
}

impl SearchResults {
    /// Create empty search results
    #[must_use]
    pub const fn empty(mode_used: SearchMode, elapsed: Duration) -> Self {
        Self {
            hits: Vec::new(),
            total_count: 0,
            mode_used,
            explain: None,
            elapsed,
        }
    }

    /// Returns true if no documents matched
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Document tests ──────────────────────────────────────────────────

    fn sample_doc() -> Document {
        Document {
            id: 1,
            kind: DocKind::Message,
            body: "Hello world".to_owned(),
            title: "Greetings".to_owned(),
            project_id: Some(42),
            created_ts: 1_700_000_000_000_000,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn doc_kind_display() {
        assert_eq!(DocKind::Message.to_string(), "message");
        assert_eq!(DocKind::Agent.to_string(), "agent");
        assert_eq!(DocKind::Project.to_string(), "project");
        assert_eq!(DocKind::Thread.to_string(), "thread");
    }

    #[test]
    fn doc_kind_serde_roundtrip() {
        for kind in [
            DocKind::Message,
            DocKind::Agent,
            DocKind::Project,
            DocKind::Thread,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let kind2: DocKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, kind2);
        }
    }

    #[test]
    fn document_serde_roundtrip() {
        let doc = sample_doc();
        let json = serde_json::to_string(&doc).unwrap();
        let doc2: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(doc2.id, doc.id);
        assert_eq!(doc2.kind, doc.kind);
        assert_eq!(doc2.body, doc.body);
    }

    #[test]
    fn doc_change_accessors() {
        let upsert = DocChange::Upsert(sample_doc());
        assert_eq!(upsert.doc_id(), 1);
        assert_eq!(upsert.doc_kind(), DocKind::Message);

        let delete = DocChange::Delete {
            id: 99,
            kind: DocKind::Agent,
        };
        assert_eq!(delete.doc_id(), 99);
        assert_eq!(delete.doc_kind(), DocKind::Agent);
    }

    // ── Query tests ─────────────────────────────────────────────────────

    #[test]
    fn query_builder_defaults() {
        let q = SearchQuery::new("hello world");
        assert_eq!(q.raw_query, "hello world");
        assert_eq!(q.mode, SearchMode::Auto);
        assert_eq!(q.limit, 20);
        assert_eq!(q.offset, 0);
        assert!(!q.explain);
    }

    #[test]
    fn query_builder_chained() {
        let q = SearchQuery::new("test")
            .with_mode(SearchMode::Lexical)
            .with_limit(50)
            .with_offset(10)
            .with_explain();
        assert_eq!(q.mode, SearchMode::Lexical);
        assert_eq!(q.limit, 50);
        assert_eq!(q.offset, 10);
        assert!(q.explain);
    }

    #[test]
    fn search_mode_display() {
        assert_eq!(SearchMode::Lexical.to_string(), "lexical");
        assert_eq!(SearchMode::Semantic.to_string(), "semantic");
        assert_eq!(SearchMode::Hybrid.to_string(), "hybrid");
        assert_eq!(SearchMode::Auto.to_string(), "auto");
    }

    #[test]
    fn importance_filter_default() {
        assert_eq!(ImportanceFilter::default(), ImportanceFilter::Any);
    }

    // ── Results tests ───────────────────────────────────────────────────

    fn sample_hit() -> SearchHit {
        SearchHit {
            doc_id: 42,
            doc_kind: DocKind::Message,
            score: 0.95,
            snippet: Some("...matched **term**...".to_owned()),
            highlight_ranges: vec![HighlightRange {
                field: "body".to_owned(),
                start: 11,
                end: 19,
            }],
            metadata: {
                let mut m = HashMap::new();
                m.insert("sender".to_owned(), serde_json::json!("BlueLake"));
                m
            },
        }
    }

    #[test]
    fn hit_serde_roundtrip() {
        let hit = sample_hit();
        let json = serde_json::to_string(&hit).unwrap();
        let hit2: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(hit2.doc_id, 42);
        assert!((hit2.score - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn search_results_empty() {
        let results = SearchResults::empty(SearchMode::Lexical, Duration::from_millis(1));
        assert!(results.is_empty());
        assert_eq!(results.total_count, 0);
    }

    #[test]
    fn compose_hit_explanation_canonical_order() {
        let config = ExplainComposerConfig::default();
        let explanation = compose_hit_explanation(
            1,
            0.9,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: Some("BM25".to_owned()),
                stage_score: 0.9,
                stage_weight: 1.0,
                score_factors: vec![],
            }],
            &config,
        );
        assert_eq!(explanation.stages.len(), 4); // all 4 canonical stages
        assert_eq!(explanation.stages[0].stage, ExplainStage::Lexical);
        assert_eq!(explanation.stages[1].stage, ExplainStage::Semantic);
        assert_eq!(explanation.stages[2].stage, ExplainStage::Fusion);
        assert_eq!(explanation.stages[3].stage, ExplainStage::Rerank);
    }

    #[test]
    fn redact_zeroes_scores() {
        let config = ExplainComposerConfig::default();
        let mut hit = compose_hit_explanation(
            1,
            0.9,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.9,
                stage_weight: 1.0,
                score_factors: vec![],
            }],
            &config,
        );
        redact_hit_explanation(&mut hit, ExplainReasonCode::ScopeRedacted);
        assert!((hit.final_score - 0.0).abs() < f64::EPSILON);
        assert!(hit.stages.iter().all(|s| s.redacted));
    }
}
