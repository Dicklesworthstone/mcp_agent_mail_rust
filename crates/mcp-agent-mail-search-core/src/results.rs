//! Search results model
//!
//! [`SearchResults`] is the output of [`SearchEngine::search`]. Each result
//! is a [`SearchHit`] with score, optional snippet, and highlight ranges.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Scoring explanation for a single hit (when explain mode is on)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitExplanation {
    /// The document ID
    pub doc_id: DocId,
    /// Term frequency / inverse document frequency breakdown
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tf_idf: Option<f64>,
    /// BM25 score component
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25: Option<f64>,
    /// Semantic similarity score component
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_similarity: Option<f64>,
    /// Final fused score
    pub final_score: f64,
    /// Free-form explanation text
    pub explanation: String,
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
        let report = ExplainReport {
            hits: vec![HitExplanation {
                doc_id: 42,
                tf_idf: Some(0.8),
                bm25: Some(12.5),
                semantic_similarity: None,
                final_score: 0.95,
                explanation: "BM25 dominant".to_owned(),
            }],
            mode_used: SearchMode::Lexical,
            candidates_evaluated: 500,
            phase_timings: {
                let mut m = HashMap::new();
                m.insert("retrieval".to_owned(), Duration::from_millis(3));
                m.insert("scoring".to_owned(), Duration::from_millis(1));
                m
            },
        };
        let json = serde_json::to_string(&report).unwrap();
        let report2: ExplainReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report2.hits.len(), 1);
        assert_eq!(report2.hits[0].doc_id, 42);
        assert_eq!(report2.candidates_evaluated, 500);
        assert_eq!(report2.mode_used, SearchMode::Lexical);
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
