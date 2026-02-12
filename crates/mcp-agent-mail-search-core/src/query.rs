//! Search query model
//!
//! [`SearchQuery`] is the primary input to [`SearchEngine::search`]. It supports
//! multiple search modes, filters, pagination, and optional explain output.

use serde::{Deserialize, Serialize};

use crate::document::DocKind;

/// Which search algorithm to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
}
