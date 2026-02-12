//! Core search engine traits
//!
//! These traits define the pluggable interface for search backends.
//! Implementations live in separate crates/modules gated behind feature flags.

use serde::{Deserialize, Serialize};

use crate::document::{DocChange, DocId, Document};
use crate::error::SearchResult;
use crate::query::SearchQuery;
use crate::results::SearchResults;

/// Health status of a search index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexHealth {
    /// Whether the index is ready to serve queries
    pub ready: bool,
    /// Number of documents currently indexed
    pub doc_count: usize,
    /// Index size on disk in bytes (if applicable)
    pub size_bytes: Option<u64>,
    /// Timestamp of the last successful index update (micros since epoch)
    pub last_updated_ts: Option<i64>,
    /// Human-readable status message
    pub status_message: String,
}

/// Statistics returned after an index rebuild or update
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    /// Number of documents indexed
    pub docs_indexed: usize,
    /// Number of documents removed
    pub docs_removed: usize,
    /// Wall-clock time for the operation
    pub elapsed_ms: u64,
    /// Any warnings generated during indexing
    pub warnings: Vec<String>,
}

/// The primary search trait that all engine backends implement.
///
/// Implementations:
/// - FTS5 (`SQLite` built-in, always available)
/// - Tantivy (behind `tantivy` feature flag)
/// - Semantic (behind `semantic` feature flag)
/// - Hybrid fusion (behind `hybrid` feature flag)
pub trait SearchEngine: Send + Sync {
    /// Execute a search query and return ranked results.
    ///
    /// # Errors
    /// Returns `SearchError` if the query is invalid, the index is not ready,
    /// or an internal error occurs.
    fn search(&self, query: &SearchQuery) -> SearchResult<SearchResults>;
}

/// Manages the lifecycle of a search index: build, rebuild, incremental update.
pub trait IndexLifecycle: Send + Sync {
    /// Perform a full rebuild of the index from scratch.
    ///
    /// This is a potentially expensive operation that should be run in the
    /// background. Returns statistics about what was indexed.
    ///
    /// # Errors
    /// Returns `SearchError` on I/O errors or corruption.
    fn rebuild(&self) -> SearchResult<IndexStats>;

    /// Apply incremental changes to the index.
    ///
    /// Returns the number of changes successfully applied.
    ///
    /// # Errors
    /// Returns `SearchError` if the index is not ready or changes are invalid.
    fn update_incremental(&self, changes: &[DocChange]) -> SearchResult<usize>;

    /// Check the current health of the index.
    fn health(&self) -> IndexHealth;
}

/// Abstract source of documents to be indexed.
///
/// The DB layer implements this trait so the search engine doesn't depend
/// directly on the database crate.
pub trait DocumentSource: Send + Sync {
    /// Fetch a batch of documents by their IDs.
    ///
    /// Missing documents are silently omitted from the result.
    ///
    /// # Errors
    /// Returns `SearchError` on data access failures.
    fn fetch_batch(&self, ids: &[DocId]) -> SearchResult<Vec<Document>>;

    /// Fetch all documents (for full index rebuild).
    ///
    /// Returns an iterator-like batched interface to avoid loading everything
    /// into memory at once. Each call returns a batch; empty batch signals end.
    ///
    /// # Errors
    /// Returns `SearchError` on data access failures.
    fn fetch_all_batched(&self, batch_size: usize, offset: usize) -> SearchResult<Vec<Document>>;

    /// Return the total document count (for progress reporting)
    ///
    /// # Errors
    /// Returns `SearchError` on data access failures.
    fn total_count(&self) -> SearchResult<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::SearchMode;
    use std::time::Duration;

    /// Stub implementation to verify traits compile
    struct StubEngine;

    impl SearchEngine for StubEngine {
        fn search(&self, query: &SearchQuery) -> SearchResult<SearchResults> {
            Ok(SearchResults::empty(query.mode, Duration::ZERO))
        }
    }

    struct StubLifecycle;

    impl IndexLifecycle for StubLifecycle {
        fn rebuild(&self) -> SearchResult<IndexStats> {
            Ok(IndexStats {
                docs_indexed: 0,
                docs_removed: 0,
                elapsed_ms: 0,
                warnings: Vec::new(),
            })
        }

        fn update_incremental(&self, changes: &[DocChange]) -> SearchResult<usize> {
            Ok(changes.len())
        }

        fn health(&self) -> IndexHealth {
            IndexHealth {
                ready: true,
                doc_count: 0,
                size_bytes: None,
                last_updated_ts: None,
                status_message: "stub".to_owned(),
            }
        }
    }

    struct StubSource;

    impl DocumentSource for StubSource {
        fn fetch_batch(&self, _ids: &[DocId]) -> SearchResult<Vec<Document>> {
            Ok(Vec::new())
        }

        fn fetch_all_batched(
            &self,
            _batch_size: usize,
            _offset: usize,
        ) -> SearchResult<Vec<Document>> {
            Ok(Vec::new())
        }

        fn total_count(&self) -> SearchResult<usize> {
            Ok(0)
        }
    }

    #[test]
    fn stub_engine_returns_empty_results() {
        let engine = StubEngine;
        let query = SearchQuery::new("hello");
        let results = engine.search(&query).unwrap();
        assert!(results.is_empty());
        assert_eq!(results.total_count, 0);
        assert_eq!(results.mode_used, SearchMode::Auto);
    }

    #[test]
    fn stub_lifecycle_rebuild() {
        let lifecycle = StubLifecycle;
        let stats = lifecycle.rebuild().unwrap();
        assert_eq!(stats.docs_indexed, 0);
        assert!(stats.warnings.is_empty());
    }

    #[test]
    fn stub_lifecycle_health() {
        let lifecycle = StubLifecycle;
        let health = lifecycle.health();
        assert!(health.ready);
        assert_eq!(health.doc_count, 0);
    }

    #[test]
    fn stub_lifecycle_incremental_empty() {
        let lifecycle = StubLifecycle;
        let count = lifecycle.update_incremental(&[]).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn stub_source_fetch_batch_empty() {
        let source = StubSource;
        let docs = source.fetch_batch(&[]).unwrap();
        assert!(docs.is_empty());
    }

    #[test]
    fn stub_source_total_count() {
        let source = StubSource;
        assert_eq!(source.total_count().unwrap(), 0);
    }

    #[test]
    fn stub_source_fetch_all_batched_empty() {
        let source = StubSource;
        let docs = source.fetch_all_batched(100, 0).unwrap();
        assert!(docs.is_empty());
    }
}
