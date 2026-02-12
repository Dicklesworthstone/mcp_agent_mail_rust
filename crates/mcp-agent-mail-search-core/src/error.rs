//! Error types for the search subsystem

use thiserror::Error;

/// Result type alias for search operations
pub type SearchResult<T> = std::result::Result<T, SearchError>;

/// Errors that can occur during search operations
#[derive(Debug, Error)]
pub enum SearchError {
    /// The search index is not ready (still building or corrupted)
    #[error("Index not ready: {0}")]
    IndexNotReady(String),

    /// The index is corrupted and needs a full rebuild
    #[error("Index corrupted: {0}")]
    IndexCorrupted(String),

    /// Query syntax error (malformed FTS query, invalid filter, etc.)
    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    /// The requested search mode is not available (feature not compiled)
    #[error("Search mode unavailable: {0}")]
    ModeUnavailable(String),

    /// Document not found in the index
    #[error("Document not found: {0}")]
    DocumentNotFound(String),

    /// Timeout during search or indexing
    #[error("Search timeout: {0}")]
    Timeout(String),

    /// I/O error during index operations
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Internal/unexpected error
    #[error("Internal search error: {0}")]
    Internal(String),
}

impl SearchError {
    /// Returns the error type string (for JSON responses)
    #[must_use]
    pub const fn error_type(&self) -> &'static str {
        match self {
            Self::IndexNotReady(_) => "INDEX_NOT_READY",
            Self::IndexCorrupted(_) => "INDEX_CORRUPTED",
            Self::InvalidQuery(_) => "INVALID_QUERY",
            Self::ModeUnavailable(_) => "MODE_UNAVAILABLE",
            Self::DocumentNotFound(_) => "DOCUMENT_NOT_FOUND",
            Self::Timeout(_) => "TIMEOUT",
            Self::Io(_) => "IO_ERROR",
            Self::Serialization(_) => "SERIALIZATION_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    /// Returns whether the error is transient and can be retried
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::IndexNotReady(_) | Self::Timeout(_) | Self::Io(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_type_mapping() {
        let cases: Vec<(SearchError, &str)> = vec![
            (
                SearchError::IndexNotReady("building".into()),
                "INDEX_NOT_READY",
            ),
            (
                SearchError::IndexCorrupted("bad segment".into()),
                "INDEX_CORRUPTED",
            ),
            (
                SearchError::InvalidQuery("missing term".into()),
                "INVALID_QUERY",
            ),
            (
                SearchError::ModeUnavailable("semantic".into()),
                "MODE_UNAVAILABLE",
            ),
            (
                SearchError::DocumentNotFound("doc-1".into()),
                "DOCUMENT_NOT_FOUND",
            ),
            (SearchError::Timeout("5s".into()), "TIMEOUT"),
            (
                SearchError::Io(std::io::Error::other("disk full")),
                "IO_ERROR",
            ),
            (SearchError::Internal("unexpected".into()), "INTERNAL_ERROR"),
        ];
        for (err, expected) in &cases {
            assert_eq!(
                err.error_type(),
                *expected,
                "Error {err:?} should map to {expected}"
            );
        }
    }

    #[test]
    fn retryable_classification() {
        // Retryable
        assert!(SearchError::IndexNotReady("x".into()).is_retryable());
        assert!(SearchError::Timeout("x".into()).is_retryable());
        assert!(SearchError::Io(std::io::Error::other("x")).is_retryable());

        // Not retryable
        assert!(!SearchError::IndexCorrupted("x".into()).is_retryable());
        assert!(!SearchError::InvalidQuery("x".into()).is_retryable());
        assert!(!SearchError::ModeUnavailable("x".into()).is_retryable());
        assert!(!SearchError::DocumentNotFound("x".into()).is_retryable());
        assert!(!SearchError::Internal("x".into()).is_retryable());
    }

    #[test]
    fn display_all_non_empty() {
        let all_errors: Vec<SearchError> = vec![
            SearchError::IndexNotReady(String::new()),
            SearchError::IndexCorrupted(String::new()),
            SearchError::InvalidQuery(String::new()),
            SearchError::ModeUnavailable(String::new()),
            SearchError::DocumentNotFound(String::new()),
            SearchError::Timeout(String::new()),
            SearchError::Io(std::io::Error::other("")),
            SearchError::Serialization(serde_json::from_str::<i32>("x").unwrap_err()),
            SearchError::Internal(String::new()),
        ];
        for err in &all_errors {
            assert!(
                !err.to_string().is_empty(),
                "Error {err:?} should have non-empty Display"
            );
        }
    }

    #[test]
    fn io_error_from_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let search_err: SearchError = io_err.into();
        assert!(matches!(search_err, SearchError::Io(_)));
        assert_eq!(search_err.error_type(), "IO_ERROR");
    }

    #[test]
    fn serde_error_from_conversion() {
        let json_err = serde_json::from_str::<i32>("nope").unwrap_err();
        let search_err: SearchError = json_err.into();
        assert!(matches!(search_err, SearchError::Serialization(_)));
        assert_eq!(search_err.error_type(), "SERIALIZATION_ERROR");
    }
}
