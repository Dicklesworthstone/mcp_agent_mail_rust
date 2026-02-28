//! Core search engine traits
//!
//! These traits define the pluggable interface for search backends.
//! Implementations live in separate crates/modules gated behind feature flags.

use serde::{Deserialize, Serialize};

use mcp_agent_mail_core::{DocChange, DocId, Document};
use crate::search_error::SearchResult;
use mcp_agent_mail_core::SearchQuery;
use mcp_agent_mail_core::SearchResults;

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
    use mcp_agent_mail_core::SearchMode;
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

    // ── IndexHealth serde ───────────────────────────────────────────────

    #[test]
    fn index_health_serde_roundtrip() {
        let health = IndexHealth {
            ready: true,
            doc_count: 1000,
            size_bytes: Some(4096),
            last_updated_ts: Some(1_700_000_000_000_000),
            status_message: "all good".to_owned(),
        };
        let json = serde_json::to_string(&health).unwrap();
        let back: IndexHealth = serde_json::from_str(&json).unwrap();
        assert!(back.ready);
        assert_eq!(back.doc_count, 1000);
        assert_eq!(back.size_bytes, Some(4096));
        assert_eq!(back.last_updated_ts, Some(1_700_000_000_000_000));
        assert_eq!(back.status_message, "all good");
    }

    #[test]
    fn index_health_not_ready() {
        let health = IndexHealth {
            ready: false,
            doc_count: 0,
            size_bytes: None,
            last_updated_ts: None,
            status_message: "building".to_owned(),
        };
        assert!(!health.ready);
        assert!(health.size_bytes.is_none());
    }

    // ── IndexStats serde ────────────────────────────────────────────────

    #[test]
    fn index_stats_serde_roundtrip() {
        let stats = IndexStats {
            docs_indexed: 500,
            docs_removed: 10,
            elapsed_ms: 1234,
            warnings: vec!["slow batch".to_owned()],
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: IndexStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.docs_indexed, 500);
        assert_eq!(back.docs_removed, 10);
        assert_eq!(back.elapsed_ms, 1234);
        assert_eq!(back.warnings, vec!["slow batch"]);
    }

    #[test]
    fn index_stats_empty_warnings() {
        let stats = IndexStats {
            docs_indexed: 0,
            docs_removed: 0,
            elapsed_ms: 0,
            warnings: vec![],
        };
        assert!(stats.warnings.is_empty());
    }

    // ── Trait object safety ─────────────────────────────────────────────

    #[test]
    fn search_engine_trait_object_safe() {
        let engine: Box<dyn SearchEngine> = Box::new(StubEngine);
        let query = SearchQuery::new("test");
        let results = engine.search(&query).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn index_lifecycle_trait_object_safe() {
        let lifecycle: Box<dyn IndexLifecycle> = Box::new(StubLifecycle);
        assert!(lifecycle.health().ready);
    }

    #[test]
    fn document_source_trait_object_safe() {
        let source: Box<dyn DocumentSource> = Box::new(StubSource);
        assert_eq!(source.total_count().unwrap(), 0);
    }

    // ── StubLifecycle incremental with changes ──────────────────────────

    #[test]
    fn stub_lifecycle_incremental_with_changes() {
        use mcp_agent_mail_core::{DocKind, Document};
        use std::collections::HashMap;

        let lifecycle = StubLifecycle;
        let doc = Document {
            id: 1,
            kind: DocKind::Message,
            body: "test body".to_owned(),
            title: "test title".to_owned(),
            project_id: Some(1),
            created_ts: 1_000_000,
            metadata: HashMap::new(),
        };
        let changes = vec![
            DocChange::Upsert(doc),
            DocChange::Delete {
                id: 2,
                kind: DocKind::Message,
            },
        ];
        let count = lifecycle.update_incremental(&changes).unwrap();
        assert_eq!(count, 2);
    }

    // ── StubSource batch with IDs ───────────────────────────────────────

    #[test]
    fn stub_source_fetch_batch_with_ids_returns_empty() {
        let source = StubSource;
        let docs = source.fetch_batch(&[1, 2, 3]).unwrap();
        // Stub returns empty regardless of input
        assert!(docs.is_empty());
    }

    #[test]
    fn stub_source_fetch_all_batched_with_offset() {
        let source = StubSource;
        let docs = source.fetch_all_batched(50, 100).unwrap();
        assert!(docs.is_empty());
    }

    // ── IndexHealth trait coverage ─────────────────────────────────────

    #[test]
    fn index_health_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let health = IndexHealth {
            ready: true,
            doc_count: 100,
            size_bytes: None,
            last_updated_ts: None,
            status_message: "ok".to_owned(),
        };
        let debug = format!("{health:?}");
        assert!(debug.contains("IndexHealth"));
        assert_clone(&health);
    }

    #[test]
    fn index_health_optional_fields_present() {
        let health = IndexHealth {
            ready: true,
            doc_count: 500,
            size_bytes: Some(1_048_576),
            last_updated_ts: Some(1_700_000_000_000_000),
            status_message: "indexed".to_owned(),
        };
        let json = serde_json::to_string(&health).unwrap();
        assert!(json.contains("1048576"));
        assert!(json.contains("1700000000000000"));
    }

    // ── IndexStats trait coverage ──────────────────────────────────────

    #[test]
    fn index_stats_debug_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let stats = IndexStats {
            docs_indexed: 10,
            docs_removed: 2,
            elapsed_ms: 50,
            warnings: vec!["warn1".to_owned()],
        };
        let debug = format!("{stats:?}");
        assert!(debug.contains("IndexStats"));
        assert_clone(&stats);
    }

    #[test]
    fn index_stats_multiple_warnings() {
        let stats = IndexStats {
            docs_indexed: 100,
            docs_removed: 0,
            elapsed_ms: 999,
            warnings: vec![
                "slow batch 1".to_owned(),
                "missing field".to_owned(),
                "truncated body".to_owned(),
            ],
        };
        assert_eq!(stats.warnings.len(), 3);
        let json = serde_json::to_string(&stats).unwrap();
        let back: IndexStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.warnings.len(), 3);
    }

    // ── Send + Sync trait bounds verified ──────────────────────────────

    #[test]
    fn trait_bounds_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StubEngine>();
        assert_send_sync::<StubLifecycle>();
        assert_send_sync::<StubSource>();
    }
}
