//! Search results model
//!
//! [`SearchResults`] is the output of [`SearchEngine::search`]. Each result
//! is a [`SearchHit`] with score, optional snippet, and highlight ranges.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use crate::document::{DocId, DocKind};
use crate::query::SearchMode;

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

fn factor_sort_cmp(a: &ScoreFactor, b: &ScoreFactor) -> Ordering {
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

fn missing_stage(stage: ExplainStage) -> StageExplanation {
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
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sample_explain_hit(config: &ExplainComposerConfig) -> HitExplanation {
        compose_hit_explanation(
            42,
            0.95,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: Some("BM25 dominant".to_owned()),
                stage_score: 0.95,
                stage_weight: 1.0,
                score_factors: vec![
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalBm25,
                        key: "bm25".to_owned(),
                        contribution: 0.90,
                        detail: Some("raw_bm25=12.5000".to_owned()),
                    },
                    ScoreFactor {
                        code: ExplainReasonCode::LexicalTermCoverage,
                        key: "term_coverage".to_owned(),
                        contribution: 0.05,
                        detail: Some("matched=2/2".to_owned()),
                    },
                ],
            }],
            config,
        )
    }

    #[test]
    fn search_results_empty() {
        let results = SearchResults::empty(SearchMode::Auto, Duration::from_millis(1));
        assert!(results.is_empty());
        assert_eq!(results.total_count, 0);
        assert_eq!(results.mode_used, SearchMode::Auto);
        assert!(results.explain.is_none());
    }

    #[test]
    fn search_results_with_hits() {
        let results = SearchResults {
            hits: vec![sample_hit()],
            total_count: 1,
            mode_used: SearchMode::Lexical,
            explain: None,
            elapsed: Duration::from_millis(5),
        };
        assert!(!results.is_empty());
        assert_eq!(results.hits[0].doc_id, 42);
        assert!((results.hits[0].score - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn search_hit_serde_roundtrip() {
        let hit = sample_hit();
        let json = serde_json::to_string(&hit).unwrap();
        let hit2: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(hit2.doc_id, hit.doc_id);
        assert_eq!(hit2.doc_kind, hit.doc_kind);
        assert!((hit2.score - hit.score).abs() < f64::EPSILON);
        assert_eq!(hit2.snippet, hit.snippet);
        assert_eq!(hit2.highlight_ranges.len(), 1);
        assert_eq!(hit2.highlight_ranges[0].field, "body");
        assert_eq!(hit2.highlight_ranges[0].start, 11);
        assert_eq!(hit2.highlight_ranges[0].end, 19);
    }

    #[test]
    fn search_results_serde_roundtrip() {
        let results = SearchResults {
            hits: vec![sample_hit()],
            total_count: 100,
            mode_used: SearchMode::Hybrid,
            explain: None,
            elapsed: Duration::from_millis(42),
        };
        let json = serde_json::to_string(&results).unwrap();
        let results2: SearchResults = serde_json::from_str(&json).unwrap();
        assert_eq!(results2.total_count, 100);
        assert_eq!(results2.mode_used, SearchMode::Hybrid);
        assert_eq!(results2.hits.len(), 1);
    }

    #[test]
    fn explain_report_serde_roundtrip() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 8,
        };
        let report = ExplainReport {
            hits: vec![sample_explain_hit(&config)],
            mode_used: SearchMode::Lexical,
            candidates_evaluated: 500,
            phase_timings: {
                let mut m = HashMap::new();
                m.insert("retrieval".to_owned(), Duration::from_millis(3));
                m.insert("scoring".to_owned(), Duration::from_millis(1));
                m
            },
            taxonomy_version: 1,
            stage_order: ExplainStage::canonical_order().to_vec(),
            verbosity: ExplainVerbosity::Detailed,
        };
        let json = serde_json::to_string(&report).unwrap();
        let report2: ExplainReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report2.hits.len(), 1);
        assert_eq!(report2.hits[0].doc_id, 42);
        assert_eq!(report2.candidates_evaluated, 500);
        assert_eq!(report2.mode_used, SearchMode::Lexical);
        assert_eq!(report2.taxonomy_version, 1);
        assert_eq!(
            report2.stage_order,
            ExplainStage::canonical_order().to_vec()
        );
    }

    #[test]
    fn explain_composer_emits_canonical_stage_shape() {
        let config = ExplainComposerConfig::default();
        let hit = sample_explain_hit(&config);
        assert_eq!(
            hit.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
            ExplainStage::canonical_order().to_vec()
        );
        assert_eq!(
            hit.stages[1].reason_code,
            ExplainReasonCode::StageNotExecuted
        );
        assert_eq!(
            hit.stages[2].reason_code,
            ExplainReasonCode::StageNotExecuted
        );
        assert_eq!(
            hit.stages[3].reason_code,
            ExplainReasonCode::StageNotExecuted
        );
    }

    #[test]
    fn explain_composer_deterministic_factor_sort_and_truncation() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 2,
        };

        let factors_a = vec![
            ScoreFactor {
                code: ExplainReasonCode::LexicalTermCoverage,
                key: "zeta".to_owned(),
                contribution: 0.10,
                detail: Some("z".to_owned()),
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalBm25,
                key: "alpha".to_owned(),
                contribution: 0.80,
                detail: Some("a".to_owned()),
            },
            ScoreFactor {
                code: ExplainReasonCode::LexicalTermCoverage,
                key: "beta".to_owned(),
                contribution: 0.10,
                detail: Some("b".to_owned()),
            },
        ];
        let mut factors_b = factors_a.clone();
        factors_b.reverse();

        let hit_a = compose_hit_explanation(
            7,
            0.80,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.80,
                stage_weight: 1.0,
                score_factors: factors_a,
            }],
            &config,
        );
        let hit_b = compose_hit_explanation(
            7,
            0.80,
            vec![StageScoreInput {
                stage: ExplainStage::Lexical,
                reason_code: ExplainReasonCode::LexicalBm25,
                summary: None,
                stage_score: 0.80,
                stage_weight: 1.0,
                score_factors: factors_b,
            }],
            &config,
        );

        assert_eq!(
            serde_json::to_value(&hit_a).unwrap(),
            serde_json::to_value(&hit_b).unwrap()
        );
        let lexical = &hit_a.stages[0];
        assert_eq!(lexical.score_factors.len(), 2);
        assert_eq!(lexical.truncated_factor_count, 1);
    }

    #[test]
    fn explain_composer_aggregates_duplicate_stage_inputs() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 8,
        };
        let hit = compose_hit_explanation(
            10,
            0.84,
            vec![
                StageScoreInput {
                    stage: ExplainStage::Lexical,
                    reason_code: ExplainReasonCode::LexicalBm25,
                    summary: None,
                    stage_score: 0.60,
                    stage_weight: 0.8,
                    score_factors: vec![ScoreFactor {
                        code: ExplainReasonCode::LexicalBm25,
                        key: "bm25".to_owned(),
                        contribution: 0.60,
                        detail: None,
                    }],
                },
                StageScoreInput {
                    stage: ExplainStage::Lexical,
                    reason_code: ExplainReasonCode::LexicalTermCoverage,
                    summary: None,
                    stage_score: 0.24,
                    stage_weight: 1.0,
                    score_factors: vec![ScoreFactor {
                        code: ExplainReasonCode::LexicalTermCoverage,
                        key: "term_coverage".to_owned(),
                        contribution: 0.24,
                        detail: None,
                    }],
                },
            ],
            &config,
        );
        let lexical = &hit.stages[0];
        assert!((lexical.stage_score - 0.84).abs() < f64::EPSILON);
        assert!((lexical.stage_weight - 1.0).abs() < f64::EPSILON);
        assert!((lexical.weighted_score - 0.84).abs() < f64::EPSILON);
        assert_eq!(lexical.score_factors.len(), 2);
    }

    #[test]
    fn explain_verbosity_minimal_hides_factor_details() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Minimal,
            max_factors_per_stage: 4,
        };
        let hit = sample_explain_hit(&config);
        assert!(hit.stages[0].score_factors.is_empty());
        assert_eq!(hit.stages[0].truncated_factor_count, 2);
    }

    #[test]
    fn redact_report_for_docs_scrubs_stage_details() {
        let config = ExplainComposerConfig {
            verbosity: ExplainVerbosity::Detailed,
            max_factors_per_stage: 4,
        };
        let mut report = compose_explain_report(
            SearchMode::Lexical,
            1,
            HashMap::new(),
            vec![sample_explain_hit(&config)],
            &config,
        );
        let mut redacted_ids = BTreeSet::new();
        redacted_ids.insert(42);
        redact_report_for_docs(&mut report, &redacted_ids, ExplainReasonCode::ScopeRedacted);

        let hit = &report.hits[0];
        assert!((hit.final_score - 0.0).abs() < f64::EPSILON);
        assert_eq!(hit.reason_codes, vec![ExplainReasonCode::ScopeRedacted]);
        assert!(hit.stages.iter().all(|s| s.redacted));
        assert!(hit.stages.iter().all(|s| s.score_factors.is_empty()));
        assert!(
            hit.stages
                .iter()
                .all(|s| s.reason_code == ExplainReasonCode::ScopeRedacted)
        );
    }

    #[test]
    fn highlight_range_serde() {
        let range = HighlightRange {
            field: "title".to_owned(),
            start: 0,
            end: 5,
        };
        let json = serde_json::to_string(&range).unwrap();
        let range2: HighlightRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range2.field, "title");
        assert_eq!(range2.start, 0);
        assert_eq!(range2.end, 5);
    }

    #[test]
    fn hit_metadata_empty_skipped_in_json() {
        let hit = SearchHit {
            doc_id: 1,
            doc_kind: DocKind::Agent,
            score: 0.5,
            snippet: None,
            highlight_ranges: Vec::new(),
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&hit).unwrap();
        // Empty metadata and highlight_ranges should be skipped
        assert!(!json.contains("metadata"));
        assert!(!json.contains("highlight_ranges"));
        // snippet is None so should also be skipped
        assert!(!json.contains("snippet"));
    }
}
