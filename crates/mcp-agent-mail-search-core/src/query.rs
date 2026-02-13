//! Search query model
//!
//! [`SearchQuery`] is the primary input to [`SearchEngine::search`]. It supports
//! multiple search modes, filters, pagination, and optional explain output.

use serde::{Deserialize, Serialize};

use crate::document::DocKind;

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn query_builder_with_filters() {
        let filter = SearchFilter {
            sender: Some("BlueLake".to_owned()),
            project_id: Some(42),
            doc_kind: Some(DocKind::Message),
            ..SearchFilter::default()
        };
        let q = SearchQuery::new("plan").with_filters(filter);
        assert_eq!(q.filters.sender.as_deref(), Some("BlueLake"));
        assert_eq!(q.filters.project_id, Some(42));
        assert_eq!(q.filters.doc_kind, Some(DocKind::Message));
        assert!(q.filters.thread_id.is_none());
    }

    #[test]
    fn search_mode_display() {
        assert_eq!(SearchMode::Lexical.to_string(), "lexical");
        assert_eq!(SearchMode::Semantic.to_string(), "semantic");
        assert_eq!(SearchMode::Hybrid.to_string(), "hybrid");
        assert_eq!(SearchMode::Auto.to_string(), "auto");
    }

    #[test]
    fn search_mode_default_is_auto() {
        assert_eq!(SearchMode::default(), SearchMode::Auto);
    }

    #[test]
    fn query_serde_roundtrip() {
        let q = SearchQuery::new("migration plan")
            .with_mode(SearchMode::Hybrid)
            .with_limit(5)
            .with_offset(2)
            .with_explain();
        let json = serde_json::to_string(&q).unwrap();
        let q2: SearchQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(q2.raw_query, "migration plan");
        assert_eq!(q2.mode, SearchMode::Hybrid);
        assert_eq!(q2.limit, 5);
        assert_eq!(q2.offset, 2);
        assert!(q2.explain);
    }

    #[test]
    fn search_filter_serde_skip_none() {
        let filter = SearchFilter::default();
        let json = serde_json::to_string(&filter).unwrap();
        // All fields are None/default, should be empty object
        assert_eq!(json, "{}");
    }

    #[test]
    fn importance_filter_default() {
        assert_eq!(ImportanceFilter::default(), ImportanceFilter::Any);
    }

    #[test]
    fn date_range_serde() {
        let range = DateRange {
            start: Some(1_000_000),
            end: Some(2_000_000),
        };
        let json = serde_json::to_string(&range).unwrap();
        let range2: DateRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range2.start, Some(1_000_000));
        assert_eq!(range2.end, Some(2_000_000));
    }

    // ── SearchMode serde ────────────────────────────────────────────────

    #[test]
    fn search_mode_serde_all_variants() {
        for mode in [
            SearchMode::Lexical,
            SearchMode::Semantic,
            SearchMode::Hybrid,
            SearchMode::Auto,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: SearchMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn search_mode_serde_snake_case() {
        let json = serde_json::to_string(&SearchMode::Lexical).unwrap();
        assert_eq!(json, "\"lexical\"");
        let json = serde_json::to_string(&SearchMode::Auto).unwrap();
        assert_eq!(json, "\"auto\"");
    }

    #[test]
    fn search_mode_hash_distinct() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SearchMode::Lexical);
        set.insert(SearchMode::Semantic);
        set.insert(SearchMode::Hybrid);
        set.insert(SearchMode::Auto);
        assert_eq!(set.len(), 4);
    }

    // ── ImportanceFilter serde ──────────────────────────────────────────

    #[test]
    fn importance_filter_serde_all_variants() {
        for filter in [
            ImportanceFilter::Any,
            ImportanceFilter::Urgent,
            ImportanceFilter::High,
            ImportanceFilter::Normal,
            ImportanceFilter::Low,
        ] {
            let json = serde_json::to_string(&filter).unwrap();
            let back: ImportanceFilter = serde_json::from_str(&json).unwrap();
            assert_eq!(back, filter);
        }
    }

    // ── DateRange edge cases ────────────────────────────────────────────

    #[test]
    fn date_range_start_only() {
        let range = DateRange {
            start: Some(100),
            end: None,
        };
        let json = serde_json::to_string(&range).unwrap();
        let back: DateRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back.start, Some(100));
        assert!(back.end.is_none());
    }

    #[test]
    fn date_range_end_only() {
        let range = DateRange {
            start: None,
            end: Some(200),
        };
        let json = serde_json::to_string(&range).unwrap();
        let back: DateRange = serde_json::from_str(&json).unwrap();
        assert!(back.start.is_none());
        assert_eq!(back.end, Some(200));
    }

    #[test]
    fn date_range_both_none() {
        let range = DateRange {
            start: None,
            end: None,
        };
        let json = serde_json::to_string(&range).unwrap();
        let back: DateRange = serde_json::from_str(&json).unwrap();
        assert!(back.start.is_none());
        assert!(back.end.is_none());
    }

    // ── SearchFilter populated ──────────────────────────────────────────

    #[test]
    fn search_filter_all_fields_set() {
        let filter = SearchFilter {
            sender: Some("Agent".to_owned()),
            project_id: Some(1),
            date_range: Some(DateRange {
                start: Some(100),
                end: Some(200),
            }),
            importance: Some(ImportanceFilter::Urgent),
            thread_id: Some("thread-1".to_owned()),
            doc_kind: Some(DocKind::Message),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sender.as_deref(), Some("Agent"));
        assert_eq!(back.project_id, Some(1));
        assert_eq!(back.importance, Some(ImportanceFilter::Urgent));
        assert_eq!(back.thread_id.as_deref(), Some("thread-1"));
    }

    // ── SearchQuery deserialization defaults ─────────────────────────────

    #[test]
    fn query_deserialize_minimal_json() {
        let json = r#"{"raw_query": "test"}"#;
        let q: SearchQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.raw_query, "test");
        assert_eq!(q.mode, SearchMode::Auto); // default
        assert_eq!(q.limit, 20); // default_limit
        assert_eq!(q.offset, 0); // default
        assert!(!q.explain); // default
    }

    #[test]
    fn query_with_mode_returns_correct_mode() {
        let q = SearchQuery::new("x").with_mode(SearchMode::Semantic);
        assert_eq!(q.mode, SearchMode::Semantic);
    }

    #[test]
    fn query_with_limit_returns_correct_limit() {
        let q = SearchQuery::new("x").with_limit(100);
        assert_eq!(q.limit, 100);
    }

    #[test]
    fn query_with_offset_returns_correct_offset() {
        let q = SearchQuery::new("x").with_offset(42);
        assert_eq!(q.offset, 42);
    }

    #[test]
    fn query_with_explain_sets_true() {
        let q = SearchQuery::new("x").with_explain();
        assert!(q.explain);
    }

    // ── SearchFilter doc_kind variants ──────────────────────────────────

    #[test]
    fn search_filter_doc_kind_agent() {
        let filter = SearchFilter {
            doc_kind: Some(DocKind::Agent),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_kind, Some(DocKind::Agent));
    }

    #[test]
    fn search_filter_doc_kind_project() {
        let filter = SearchFilter {
            doc_kind: Some(DocKind::Project),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_kind, Some(DocKind::Project));
    }

    // ── SearchMode trait coverage ─────────────────────────────────────

    #[test]
    fn search_mode_debug() {
        let debug = format!("{:?}", SearchMode::Lexical);
        assert!(debug.contains("Lexical"));
    }

    #[test]
    fn search_mode_clone_copy_eq() {
        let a = SearchMode::Hybrid;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(a, SearchMode::Lexical);
    }

    // ── ImportanceFilter trait coverage ────────────────────────────────

    #[test]
    fn importance_filter_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ImportanceFilter::Urgent).unwrap(),
            "\"urgent\""
        );
        assert_eq!(
            serde_json::to_string(&ImportanceFilter::Any).unwrap(),
            "\"any\""
        );
    }

    #[test]
    fn importance_filter_debug_clone_copy() {
        let a = ImportanceFilter::High;
        let b = a; // Copy
        assert_eq!(a, b);
        let debug = format!("{a:?}");
        assert!(debug.contains("High"));
    }

    #[test]
    fn importance_filter_eq_ne() {
        assert_eq!(ImportanceFilter::Low, ImportanceFilter::Low);
        assert_ne!(ImportanceFilter::Low, ImportanceFilter::Normal);
    }

    // ── DateRange trait coverage ──────────────────────────────────────

    #[test]
    fn date_range_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let range = DateRange {
            start: Some(100),
            end: Some(200),
        };
        let debug = format!("{range:?}");
        assert!(debug.contains("DateRange"));
        assert_clone(&range);
    }

    // ── SearchFilter trait coverage ──────────────────────────────────

    #[test]
    fn search_filter_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let filter = SearchFilter::default();
        let debug = format!("{filter:?}");
        assert!(debug.contains("SearchFilter"));
        assert_clone(&filter);
    }

    #[test]
    fn search_filter_importance_field() {
        let filter = SearchFilter {
            importance: Some(ImportanceFilter::Low),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.importance, Some(ImportanceFilter::Low));
    }

    // ── SearchQuery trait coverage ───────────────────────────────────

    #[test]
    fn search_query_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let q = SearchQuery::new("test");
        let debug = format!("{q:?}");
        assert!(debug.contains("SearchQuery"));
        assert_clone(&q);
    }

    #[test]
    fn search_query_new_from_string() {
        let q = SearchQuery::new(String::from("owned string"));
        assert_eq!(q.raw_query, "owned string");
    }

    #[test]
    fn search_query_default_limit_is_20() {
        let q = SearchQuery::new("x");
        assert_eq!(q.limit, 20);
    }

    #[test]
    fn search_query_chained_all_builders() {
        let filter = SearchFilter {
            sender: Some("Agent".to_owned()),
            ..SearchFilter::default()
        };
        let q = SearchQuery::new("hello")
            .with_mode(SearchMode::Semantic)
            .with_limit(10)
            .with_offset(5)
            .with_explain()
            .with_filters(filter);
        assert_eq!(q.mode, SearchMode::Semantic);
        assert_eq!(q.limit, 10);
        assert_eq!(q.offset, 5);
        assert!(q.explain);
        assert_eq!(q.filters.sender.as_deref(), Some("Agent"));
    }

    // ── SearchMode invalid deserialize ───────────────────────────────

    #[test]
    fn search_mode_invalid_deserialize() {
        let result = serde_json::from_str::<SearchMode>("\"invalid\"");
        assert!(result.is_err());
    }

    #[test]
    fn importance_filter_invalid_deserialize() {
        let result = serde_json::from_str::<ImportanceFilter>("\"critical\"");
        assert!(result.is_err());
    }

    // ── SearchFilter doc_kind thread variant ─────────────────────────

    #[test]
    fn search_filter_doc_kind_thread() {
        let filter = SearchFilter {
            doc_kind: Some(DocKind::Thread),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_kind, Some(DocKind::Thread));
    }

    // ── SearchFilter thread_id field ─────────────────────────────────

    #[test]
    fn search_filter_thread_id_field() {
        let filter = SearchFilter {
            thread_id: Some("br-42".to_owned()),
            ..SearchFilter::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        assert!(json.contains("br-42"));
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back.thread_id.as_deref(), Some("br-42"));
    }

    // ── SearchQuery empty raw_query ──────────────────────────────────

    #[test]
    fn search_query_empty_raw_query() {
        let q = SearchQuery::new("");
        assert!(q.raw_query.is_empty());
        let json = serde_json::to_string(&q).unwrap();
        let back: SearchQuery = serde_json::from_str(&json).unwrap();
        assert!(back.raw_query.is_empty());
    }
}
