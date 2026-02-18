#![allow(clippy::permissions_set_readonly_false, clippy::missing_const_for_fn)]
//! Fault-injection tests for degraded and corrupt states in Search V3.
//!
//! br-2tnl.7.6: Implement fault-injection tests for index/model/daemon failure paths
//!
//! Validates robust behavior under common operational failures:
//! - Corrupted lexical/vector index files (checkpoint corruption, missing files)
//! - Missing/unavailable embedding or reranker models (fallback semantics)
//! - Index lifecycle failures (rebuild crashes, incremental update failures)
//! - Consistency check behavior under fault conditions
//! - Recovery semantics (`repair_if_needed`, reindex after corruption)

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mcp_agent_mail_search_core::consistency::{
    ConsistencyConfig, NoProgress, ReindexConfig, Severity, check_consistency, full_reindex,
    repair_if_needed,
};
use mcp_agent_mail_search_core::document::{DocChange, DocKind, Document};
use mcp_agent_mail_search_core::engine::{DocumentSource, IndexHealth, IndexLifecycle, IndexStats};
use mcp_agent_mail_search_core::error::{SearchError, SearchResult};
use mcp_agent_mail_search_core::index_layout::{
    IndexCheckpoint, IndexLayout, IndexScope, SchemaField, SchemaHash,
};

// ═══════════════════════════════════════════════════════════════════════
// Mock infrastructure — fault-injectable variants
// ═══════════════════════════════════════════════════════════════════════

/// A document source that can be configured to fail on specific operations.
struct FaultySource {
    docs: Vec<Document>,
    /// If set, `total_count()` returns this error.
    fail_total_count: AtomicBool,
    /// If set, `fetch_all_batched()` fails after this many batches.
    fail_after_batches: Option<usize>,
    batch_calls: AtomicUsize,
}

impl FaultySource {
    fn healthy(count: usize) -> Self {
        let docs = (0..count)
            .map(|i| {
                let idx = i64::try_from(i).expect("count fits i64");
                Document {
                    id: idx + 1,
                    kind: DocKind::Message,
                    body: format!("body {i}"),
                    title: format!("title {i}"),
                    project_id: Some(1),
                    created_ts: 1_700_000_000_000_000 + idx,
                    metadata: HashMap::new(),
                }
            })
            .collect();
        Self {
            docs,
            fail_total_count: AtomicBool::new(false),
            fail_after_batches: None,
            batch_calls: AtomicUsize::new(0),
        }
    }

    fn with_failing_count(mut self) -> Self {
        self.fail_total_count = AtomicBool::new(true);
        self
    }

    fn with_failing_batches(mut self, after: usize) -> Self {
        self.fail_after_batches = Some(after);
        self
    }
}

impl DocumentSource for FaultySource {
    fn fetch_batch(&self, ids: &[i64]) -> SearchResult<Vec<Document>> {
        Ok(self
            .docs
            .iter()
            .filter(|d| ids.contains(&d.id))
            .cloned()
            .collect())
    }

    fn fetch_all_batched(&self, batch_size: usize, offset: usize) -> SearchResult<Vec<Document>> {
        let call_num = self.batch_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(limit) = self.fail_after_batches
            && call_num >= limit
        {
            return Err(SearchError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "simulated batch read failure",
            )));
        }
        Ok(self
            .docs
            .iter()
            .skip(offset)
            .take(batch_size)
            .cloned()
            .collect())
    }

    fn total_count(&self) -> SearchResult<usize> {
        if self.fail_total_count.load(Ordering::Relaxed) {
            Err(SearchError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "simulated DB connection failure",
            )))
        } else {
            Ok(self.docs.len())
        }
    }
}

/// An index lifecycle that can simulate failures at specific points.
struct FaultyLifecycle {
    doc_count: AtomicUsize,
    ready: AtomicBool,
    /// If set, `rebuild()` returns an error.
    fail_rebuild: AtomicBool,
    /// If set, `update_incremental()` returns an error.
    fail_incremental: AtomicBool,
    /// Count of incremental calls before failure (0 = fail immediately).
    incremental_fail_after: Option<usize>,
    incremental_calls: AtomicUsize,
    /// Track all changes applied (for verification).
    applied_changes: Mutex<Vec<usize>>,
}

impl FaultyLifecycle {
    fn healthy(doc_count: usize) -> Self {
        Self {
            doc_count: AtomicUsize::new(doc_count),
            ready: AtomicBool::new(true),
            fail_rebuild: AtomicBool::new(false),
            fail_incremental: AtomicBool::new(false),
            incremental_fail_after: None,
            incremental_calls: AtomicUsize::new(0),
            applied_changes: Mutex::new(Vec::new()),
        }
    }

    fn not_ready() -> Self {
        Self {
            doc_count: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            fail_rebuild: AtomicBool::new(false),
            fail_incremental: AtomicBool::new(false),
            incremental_fail_after: None,
            incremental_calls: AtomicUsize::new(0),
            applied_changes: Mutex::new(Vec::new()),
        }
    }

    fn with_failing_rebuild(self) -> Self {
        self.fail_rebuild.store(true, Ordering::Relaxed);
        self
    }

    fn with_failing_incremental(self) -> Self {
        self.fail_incremental.store(true, Ordering::Relaxed);
        self
    }

    fn with_incremental_fail_after(mut self, after: usize) -> Self {
        self.incremental_fail_after = Some(after);
        self
    }
}

impl IndexLifecycle for FaultyLifecycle {
    fn rebuild(&self) -> SearchResult<IndexStats> {
        if self.fail_rebuild.load(Ordering::Relaxed) {
            return Err(SearchError::IndexCorrupted(
                "simulated rebuild failure: index segment corrupted".to_owned(),
            ));
        }
        self.doc_count.store(0, Ordering::Relaxed);
        Ok(IndexStats {
            docs_indexed: 0,
            docs_removed: 0,
            elapsed_ms: 0,
            warnings: Vec::new(),
        })
    }

    fn update_incremental(&self, changes: &[DocChange]) -> SearchResult<usize> {
        if self.fail_incremental.load(Ordering::Relaxed) {
            return Err(SearchError::Internal(
                "simulated incremental update failure".to_owned(),
            ));
        }

        let call_num = self.incremental_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(limit) = self.incremental_fail_after
            && call_num >= limit
        {
            return Err(SearchError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "simulated OOM during incremental update",
            )));
        }

        let count = changes.len();
        self.doc_count.fetch_add(count, Ordering::Relaxed);
        self.applied_changes.lock().expect("lock").push(count);
        Ok(count)
    }

    fn health(&self) -> IndexHealth {
        let ready = self.ready.load(Ordering::Relaxed);
        IndexHealth {
            ready,
            doc_count: self.doc_count.load(Ordering::Relaxed),
            size_bytes: None,
            last_updated_ts: None,
            status_message: if ready {
                "healthy".to_owned()
            } else {
                "not ready: simulated unavailability".to_owned()
            },
        }
    }
}

/// Mutable in-memory source for CRUD transition integration tests.
struct MutableSource {
    docs: Mutex<Vec<Document>>,
}

impl MutableSource {
    fn new(docs: Vec<Document>) -> Self {
        Self {
            docs: Mutex::new(docs),
        }
    }

    fn upsert(&self, doc: Document) {
        let mut docs = self.docs.lock().expect("lock");
        if let Some(existing) = docs
            .iter_mut()
            .find(|existing| existing.id == doc.id && existing.kind == doc.kind)
        {
            *existing = doc;
            return;
        }
        docs.push(doc);
    }

    fn delete(&self, id: i64, kind: DocKind) -> bool {
        let mut docs = self.docs.lock().expect("lock");
        let before = docs.len();
        docs.retain(|d| !(d.id == id && d.kind == kind));
        docs.len() != before
    }

    fn snapshot_keys(&self) -> Vec<String> {
        let mut keys = self
            .docs
            .lock()
            .expect("lock")
            .iter()
            .map(|doc| doc_key(doc.id, doc.kind))
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }
}

impl DocumentSource for MutableSource {
    fn fetch_batch(&self, ids: &[i64]) -> SearchResult<Vec<Document>> {
        Ok(self
            .docs
            .lock()
            .expect("lock")
            .iter()
            .filter(|doc| ids.contains(&doc.id))
            .cloned()
            .collect())
    }

    fn fetch_all_batched(&self, batch_size: usize, offset: usize) -> SearchResult<Vec<Document>> {
        Ok(self
            .docs
            .lock()
            .expect("lock")
            .iter()
            .skip(offset)
            .take(batch_size)
            .cloned()
            .collect())
    }

    fn total_count(&self) -> SearchResult<usize> {
        Ok(self.docs.lock().expect("lock").len())
    }
}

/// Stateful lifecycle that applies upsert/delete changes by `(kind,id)` key.
struct CrudLifecycle {
    docs: Mutex<HashMap<(DocKind, i64), Document>>,
    ready: AtomicBool,
}

impl CrudLifecycle {
    fn new() -> Self {
        Self {
            docs: Mutex::new(HashMap::new()),
            ready: AtomicBool::new(true),
        }
    }

    fn with_docs(docs: Vec<Document>) -> Self {
        let mut seeded = HashMap::new();
        for doc in docs {
            seeded.insert((doc.kind, doc.id), doc);
        }
        Self {
            docs: Mutex::new(seeded),
            ready: AtomicBool::new(true),
        }
    }

    fn get_doc(&self, id: i64, kind: DocKind) -> Option<Document> {
        self.docs.lock().expect("lock").get(&(kind, id)).cloned()
    }

    fn snapshot_keys(&self) -> Vec<String> {
        let mut keys = self
            .docs
            .lock()
            .expect("lock")
            .keys()
            .map(|(kind, id)| doc_key(*id, *kind))
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }
}

impl IndexLifecycle for CrudLifecycle {
    fn rebuild(&self) -> SearchResult<IndexStats> {
        self.docs.lock().expect("lock").clear();
        Ok(IndexStats {
            docs_indexed: 0,
            docs_removed: 0,
            elapsed_ms: 0,
            warnings: Vec::new(),
        })
    }

    fn update_incremental(&self, changes: &[DocChange]) -> SearchResult<usize> {
        {
            let mut docs = self.docs.lock().expect("lock");
            for change in changes {
                match change {
                    DocChange::Upsert(doc) => {
                        docs.insert((doc.kind, doc.id), doc.clone());
                    }
                    DocChange::Delete { id, kind } => {
                        docs.remove(&(*kind, *id));
                    }
                }
            }
        }
        Ok(changes.len())
    }

    fn health(&self) -> IndexHealth {
        let docs = self.docs.lock().expect("lock");
        IndexHealth {
            ready: self.ready.load(Ordering::Relaxed),
            doc_count: docs.len(),
            size_bytes: None,
            last_updated_ts: None,
            status_message: "healthy".to_owned(),
        }
    }
}

fn sample_doc(id: i64, kind: DocKind, title: &str, body: &str) -> Document {
    Document {
        id,
        kind,
        body: body.to_owned(),
        title: title.to_owned(),
        project_id: Some(1),
        created_ts: 1_700_000_000_000_000 + id,
        metadata: HashMap::new(),
    }
}

fn doc_key(id: i64, kind: DocKind) -> String {
    format!("{kind}:{id}")
}

/// Helper to create a temp layout with a valid checkpoint
fn setup_healthy_index(
    tmp: &tempfile::TempDir,
    scope: &IndexScope,
    schema: &SchemaHash,
    doc_count: usize,
) -> IndexLayout {
    let layout = IndexLayout::new(tmp.path());
    layout.ensure_dirs(scope, schema).unwrap();
    let cp = IndexCheckpoint {
        schema_hash: schema.clone(),
        docs_indexed: doc_count,
        started_ts: 1_700_000_000_000_000,
        completed_ts: Some(1_700_000_001_000_000),
        max_version: 1_700_000_000_000_000,
        success: true,
    };
    cp.write_to(&layout.lexical_dir(scope, schema)).unwrap();
    layout
}

fn test_scope() -> IndexScope {
    IndexScope::Global
}

fn test_schema() -> SchemaHash {
    SchemaHash("abcdef123456".to_owned())
}

// ═══════════════════════════════════════════════════════════════════════
// Section 1: Corrupted index files
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn corrupted_checkpoint_json_triggers_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());
    layout.ensure_dirs(&scope, &schema).unwrap();

    // Write garbage to the checkpoint file
    let checkpoint_path = layout
        .lexical_dir(&scope, &schema)
        .join(IndexCheckpoint::FILENAME);
    std::fs::write(&checkpoint_path, "{ this is not valid json !!!").unwrap();

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(10);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    // Should find a missing/corrupt checkpoint warning
    let checkpoint_finding = report
        .findings
        .iter()
        .find(|f| f.category == "missing_checkpoint");
    assert!(
        checkpoint_finding.is_some(),
        "Expected missing_checkpoint finding for corrupted JSON, got: {:?}",
        report
            .findings
            .iter()
            .map(|f| &f.category)
            .collect::<Vec<_>>()
    );
}

#[test]
fn missing_checkpoint_file_produces_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());
    layout.ensure_dirs(&scope, &schema).unwrap();

    // No checkpoint written — directory exists but no checkpoint.json
    let source = FaultySource::healthy(5);
    let lifecycle = FaultyLifecycle::healthy(5);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    let finding = report
        .findings
        .iter()
        .find(|f| f.category == "missing_checkpoint");
    assert!(finding.is_some());
    assert_eq!(finding.unwrap().severity, Severity::Warning);
}

#[test]
fn incomplete_build_checkpoint_triggers_error_and_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());
    layout.ensure_dirs(&scope, &schema).unwrap();

    // Write a checkpoint that indicates failure (success=false)
    let cp = IndexCheckpoint {
        schema_hash: schema.clone(),
        docs_indexed: 5,
        started_ts: 1_700_000_000_000_000,
        completed_ts: None,
        max_version: 1_700_000_000_000_000,
        success: false,
    };
    cp.write_to(&layout.lexical_dir(&scope, &schema)).unwrap();

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(5);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    assert!(report.rebuild_recommended);
    let finding = report
        .findings
        .iter()
        .find(|f| f.category == "incomplete_build" && f.severity == Severity::Error);
    assert!(
        finding.is_some(),
        "Expected error-severity incomplete_build finding"
    );
}

#[test]
fn checkpoint_missing_completion_ts_produces_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());
    layout.ensure_dirs(&scope, &schema).unwrap();

    // Success=true but no completed_ts (interrupted before writing timestamp)
    let cp = IndexCheckpoint {
        schema_hash: schema.clone(),
        docs_indexed: 10,
        started_ts: 1_700_000_000_000_000,
        completed_ts: None,
        max_version: 1_700_000_000_000_000,
        success: true,
    };
    cp.write_to(&layout.lexical_dir(&scope, &schema)).unwrap();

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(10);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    let finding = report
        .findings
        .iter()
        .find(|f| f.category == "incomplete_build" && f.severity == Severity::Warning);
    assert!(
        finding.is_some(),
        "Expected warning for missing completion timestamp"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Section 2: Index not ready / unavailability
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn index_not_ready_triggers_rebuild_recommendation() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(0);
    let lifecycle = FaultyLifecycle::not_ready();

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    assert!(!report.healthy);
    assert!(report.rebuild_recommended);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.category == "index_not_ready")
    );
}

#[test]
fn index_not_ready_with_docs_in_db_flags_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    // DB has 100 docs, but index reports not ready with 0 docs
    let source = FaultySource::healthy(100);
    let lifecycle = FaultyLifecycle::not_ready();

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    assert!(!report.healthy);
    assert!(report.rebuild_recommended);
    assert!(report.error_count() > 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 3: Rebuild failures
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn full_reindex_fails_on_rebuild_error() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(0).with_failing_rebuild();

    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig::default(),
        &NoProgress,
    );

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.error_type(), "INDEX_CORRUPTED");
    assert!(!err.is_retryable());
}

#[test]
fn full_reindex_fails_when_source_count_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(10).with_failing_count();
    let lifecycle = FaultyLifecycle::healthy(0);

    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig::default(),
        &NoProgress,
    );

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.error_type(), "IO_ERROR");
    assert!(err.is_retryable());
}

#[test]
fn full_reindex_fails_midway_on_batch_error() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    // Source will fail after 1 successful batch
    let source = FaultySource::healthy(100).with_failing_batches(2); // batch 0 = count, batch 1 = first data, batch 2 = fail
    let lifecycle = FaultyLifecycle::healthy(0);

    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig {
            batch_size: 10,
            write_checkpoint: true,
        },
        &NoProgress,
    );

    // Should fail with IO error from the second data batch
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.is_retryable(), "batch read errors should be retryable");
}

#[test]
fn full_reindex_fails_on_incremental_update_error() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(0).with_failing_incremental();

    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig::default(),
        &NoProgress,
    );

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().error_type(), "INTERNAL_ERROR");
}

#[test]
fn full_reindex_fails_on_incremental_oom_after_partial_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    // Source with 50 docs; lifecycle fails after 2 successful incremental calls
    let source = FaultySource::healthy(50);
    let lifecycle = FaultyLifecycle::healthy(0).with_incremental_fail_after(2);

    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig {
            batch_size: 10,
            write_checkpoint: true,
        },
        &NoProgress,
    );

    assert!(result.is_err());
    // Some docs were applied before the failure
    let applied: Vec<usize> = lifecycle.applied_changes.lock().unwrap().clone();
    assert_eq!(applied.len(), 2, "Expected 2 successful batch applies");
    assert_eq!(applied[0], 10);
    assert_eq!(applied[1], 10);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 3.5: Incremental CRUD + rebuild consistency
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn incremental_crud_transitions_stay_consistent_with_source() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = MutableSource::new(vec![
        sample_doc(1, DocKind::Message, "m1", "message-1"),
        sample_doc(2, DocKind::Agent, "agent-a", "agent profile"),
        sample_doc(3, DocKind::Project, "proj-a", "project profile"),
    ]);
    let lifecycle = CrudLifecycle::new();

    // Seed index from source.
    let initial = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig {
            batch_size: 2,
            write_checkpoint: true,
        },
        &NoProgress,
    )
    .unwrap();
    assert_eq!(initial.stats.docs_indexed, 3);
    assert!(initial.checkpoint_written);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();
    assert!(report.healthy);
    assert!(!report.rebuild_recommended);

    // CREATE
    let created = sample_doc(4, DocKind::Message, "m4", "message-4");
    source.upsert(created.clone());
    lifecycle
        .update_incremental(&[DocChange::Upsert(created)])
        .unwrap();

    // UPDATE (same id/kind with changed payload)
    let updated_agent = sample_doc(2, DocKind::Agent, "agent-a-v2", "agent profile updated");
    source.upsert(updated_agent.clone());
    lifecycle
        .update_incremental(&[DocChange::Upsert(updated_agent.clone())])
        .unwrap();
    let indexed_agent = lifecycle.get_doc(2, DocKind::Agent).expect("agent present");
    assert_eq!(indexed_agent.title, updated_agent.title);
    assert_eq!(indexed_agent.body, updated_agent.body);

    // DELETE
    assert!(source.delete(3, DocKind::Project));
    lifecycle
        .update_incremental(&[DocChange::Delete {
            id: 3,
            kind: DocKind::Project,
        }])
        .unwrap();

    let post_crud = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();
    assert!(post_crud.healthy);
    assert!(!post_crud.rebuild_recommended);
    assert_eq!(source.total_count().unwrap(), lifecycle.health().doc_count);
    assert_eq!(source.snapshot_keys(), lifecycle.snapshot_keys());

    // Compare incremental state against a fresh full rebuild from the same source.
    let rebuild_lifecycle = CrudLifecycle::new();
    let rebuild = full_reindex(
        &source,
        &rebuild_lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig {
            batch_size: 2,
            write_checkpoint: false,
        },
        &NoProgress,
    )
    .unwrap();
    assert_eq!(rebuild.stats.docs_indexed, source.total_count().unwrap());
    assert_eq!(lifecycle.snapshot_keys(), rebuild_lifecycle.snapshot_keys());

    let rebuilt_agent = rebuild_lifecycle
        .get_doc(2, DocKind::Agent)
        .expect("rebuilt agent present");
    assert_eq!(rebuilt_agent.title, "agent-a-v2");
    assert_eq!(rebuilt_agent.body, "agent profile updated");
}

#[test]
fn repair_recovers_from_failed_checkpoint_and_drifted_index_state() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());
    layout.ensure_dirs(&scope, &schema).unwrap();

    let source = MutableSource::new(vec![
        sample_doc(1, DocKind::Message, "m1", "message-1"),
        sample_doc(2, DocKind::Agent, "agent-a", "agent profile"),
        sample_doc(3, DocKind::Project, "proj-a", "project profile"),
        sample_doc(4, DocKind::Thread, "thread-a", "thread summary"),
    ]);

    // Seed lifecycle with stale subset: source has 4 docs, index has only 2.
    let lifecycle = CrudLifecycle::with_docs(vec![
        sample_doc(1, DocKind::Message, "m1", "message-1"),
        sample_doc(2, DocKind::Agent, "agent-a", "agent profile"),
    ]);

    // Force checkpoint failure state to require recovery.
    let failed = IndexCheckpoint {
        schema_hash: schema.clone(),
        docs_indexed: 2,
        started_ts: 1_700_000_000_000_000,
        completed_ts: None,
        max_version: 1_700_000_000_000_002,
        success: false,
    };
    let lexical_dir = layout.lexical_dir(&scope, &schema);
    failed.write_to(&lexical_dir).unwrap();

    let pre_repair = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();
    assert!(pre_repair.rebuild_recommended);
    assert!(
        pre_repair
            .findings
            .iter()
            .any(|f| f.category == "incomplete_build")
    );
    assert!(
        pre_repair
            .findings
            .iter()
            .any(|f| f.category == "count_mismatch")
    );

    let (_, reindex_result) =
        repair_if_needed(&source, &lifecycle, &layout, &scope, &schema, &NoProgress).unwrap();
    let reindex_result = reindex_result.expect("repair should run reindex");
    assert!(reindex_result.checkpoint_written);
    assert_eq!(
        reindex_result.stats.docs_indexed,
        source.total_count().unwrap()
    );

    let checkpoint = IndexCheckpoint::read_from(&lexical_dir).unwrap();
    assert!(checkpoint.success);
    assert!(checkpoint.completed_ts.is_some());
    assert_eq!(checkpoint.docs_indexed, source.total_count().unwrap());

    let post_repair = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();
    assert!(post_repair.healthy);
    assert!(!post_repair.rebuild_recommended);
    assert_eq!(source.snapshot_keys(), lifecycle.snapshot_keys());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 4: Consistency check under fault conditions
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn consistency_check_fails_when_source_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = setup_healthy_index(&tmp, &scope, &schema, 10);

    // Source total_count() fails (simulating DB connection failure)
    let source = FaultySource::healthy(10).with_failing_count();
    let lifecycle = FaultyLifecycle::healthy(10);

    let result = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    );

    // The doc count check will fail, propagating the IO error
    assert!(result.is_err());
}

#[test]
fn consistency_severe_drift_recommends_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = setup_healthy_index(&tmp, &scope, &schema, 10);

    // DB has 100, index has 10 — 90% drift
    let source = FaultySource::healthy(100);
    let lifecycle = FaultyLifecycle::healthy(10);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    assert!(report.rebuild_recommended);
    let mismatch = report
        .findings
        .iter()
        .find(|f| f.category == "count_mismatch");
    assert!(mismatch.is_some());
    assert_eq!(mismatch.unwrap().severity, Severity::Error);
}

#[test]
fn consistency_minor_drift_does_not_recommend_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = setup_healthy_index(&tmp, &scope, &schema, 99);

    // DB has 100, index has 99 — 1% drift
    let source = FaultySource::healthy(100);
    let lifecycle = FaultyLifecycle::healthy(99);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    assert!(!report.rebuild_recommended);
    let mismatch = report
        .findings
        .iter()
        .find(|f| f.category == "count_mismatch");
    assert!(mismatch.is_some());
    assert_eq!(mismatch.unwrap().severity, Severity::Warning);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 5: Recovery semantics (repair_if_needed)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn repair_if_needed_triggers_reindex_when_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(5);
    let lifecycle = FaultyLifecycle::not_ready();

    let (report, reindex_result) =
        repair_if_needed(&source, &lifecycle, &layout, &scope, &schema, &NoProgress).unwrap();

    assert!(report.rebuild_recommended);
    assert!(reindex_result.is_some());
    let result = reindex_result.unwrap();
    assert!(result.elapsed_ms < 60_000); // sanity: should complete quickly
}

#[test]
fn repair_if_needed_skips_reindex_when_healthy() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = setup_healthy_index(&tmp, &scope, &schema, 10);

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(10);

    let (report, reindex_result) =
        repair_if_needed(&source, &lifecycle, &layout, &scope, &schema, &NoProgress).unwrap();

    assert!(report.healthy);
    assert!(reindex_result.is_none());
}

#[test]
fn repair_if_needed_fails_when_rebuild_crashes() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(5);
    let lifecycle = FaultyLifecycle::not_ready().with_failing_rebuild();

    let result = repair_if_needed(&source, &lifecycle, &layout, &scope, &schema, &NoProgress);

    // repair_if_needed should propagate the rebuild error
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 6: Index layout fault handling
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn checkpoint_read_from_nonexistent_dir_returns_io_error() {
    let result = IndexCheckpoint::read_from(std::path::Path::new("/nonexistent/path/to/index"));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().error_type(), "IO_ERROR");
}

#[test]
fn checkpoint_read_from_corrupted_file_returns_serialization_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(IndexCheckpoint::FILENAME),
        "NOT VALID JSON AT ALL {{{",
    )
    .unwrap();

    let result = IndexCheckpoint::read_from(tmp.path());
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().error_type(), "SERIALIZATION_ERROR");
}

#[test]
fn checkpoint_write_to_readonly_dir_returns_io_error() {
    let tmp = tempfile::tempdir().unwrap();
    let readonly_dir = tmp.path().join("readonly");
    std::fs::create_dir(&readonly_dir).unwrap();

    // Make directory read-only
    let mut perms = std::fs::metadata(&readonly_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&readonly_dir, perms.clone()).unwrap();

    let cp = IndexCheckpoint {
        schema_hash: SchemaHash("test123456789".to_owned()),
        docs_indexed: 1,
        started_ts: 0,
        completed_ts: Some(1),
        max_version: 0,
        success: true,
    };

    let result = cp.write_to(&readonly_dir);

    // Restore permissions for cleanup
    perms.set_readonly(false);
    std::fs::set_permissions(&readonly_dir, perms).unwrap();

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().error_type(), "IO_ERROR");
}

#[test]
fn ensure_dirs_on_readonly_root_returns_io_error() {
    let tmp = tempfile::tempdir().unwrap();
    let readonly_root = tmp.path().join("readonly_root");
    std::fs::create_dir(&readonly_root).unwrap();

    let mut perms = std::fs::metadata(&readonly_root).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&readonly_root, perms.clone()).unwrap();

    let layout = IndexLayout::new(&readonly_root);
    let result = layout.ensure_dirs(&test_scope(), &test_schema());

    // Restore permissions for cleanup
    perms.set_readonly(false);
    std::fs::set_permissions(&readonly_root, perms).unwrap();

    assert!(result.is_err());
}

#[test]
fn activate_unknown_engine_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = IndexLayout::new(tmp.path());
    let scope = test_scope();
    let schema = test_schema();

    let result = layout.activate(&scope, "quantum_search", &schema);
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 7: Schema hash and layout utilities under edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn schema_hash_empty_fields_is_deterministic() {
    let hash1 = SchemaHash::compute(&[]);
    let hash2 = SchemaHash::compute(&[]);
    assert_eq!(hash1, hash2);
}

#[test]
fn schema_hash_order_independence() {
    let fields_a = vec![
        SchemaField {
            name: "body".to_owned(),
            field_type: "text".to_owned(),
            indexed: true,
        },
        SchemaField {
            name: "title".to_owned(),
            field_type: "text".to_owned(),
            indexed: true,
        },
    ];
    let fields_b = vec![fields_a[1].clone(), fields_a[0].clone()];
    assert_eq!(
        SchemaHash::compute(&fields_a),
        SchemaHash::compute(&fields_b)
    );
}

#[test]
fn active_schema_returns_none_for_missing_link() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = IndexLayout::new(tmp.path());
    assert!(layout.active_schema(&test_scope(), "lexical").is_none());
}

#[test]
fn index_scope_dir_names_distinct() {
    let project = IndexScope::Project { project_id: 1 };
    let product = IndexScope::Product { product_id: 1 };
    let global = IndexScope::Global;
    assert_ne!(project.dir_name(), product.dir_name());
    assert_ne!(project.dir_name(), global.dir_name());
    assert_ne!(product.dir_name(), global.dir_name());
}

// ═══════════════════════════════════════════════════════════════════════
// Section 8: Error classification under fault conditions
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn io_errors_are_retryable() {
    let err = SearchError::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "db down",
    ));
    assert!(err.is_retryable());
    assert_eq!(err.error_type(), "IO_ERROR");
}

#[test]
fn corrupted_index_is_not_retryable() {
    let err = SearchError::IndexCorrupted("CRC mismatch".into());
    assert!(!err.is_retryable());
    assert_eq!(err.error_type(), "INDEX_CORRUPTED");
}

#[test]
fn timeout_is_retryable() {
    let err = SearchError::Timeout("5s budget exhausted".into());
    assert!(err.is_retryable());
    assert_eq!(err.error_type(), "TIMEOUT");
}

#[test]
fn mode_unavailable_is_not_retryable() {
    let err = SearchError::ModeUnavailable("semantic feature not compiled".into());
    assert!(!err.is_retryable());
    assert_eq!(err.error_type(), "MODE_UNAVAILABLE");
}

#[test]
fn index_not_ready_is_retryable() {
    let err = SearchError::IndexNotReady("still building".into());
    assert!(err.is_retryable());
    assert_eq!(err.error_type(), "INDEX_NOT_READY");
}

// ═══════════════════════════════════════════════════════════════════════
// Section 9: Reindex with checkpoint write failures
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn reindex_succeeds_even_when_checkpoint_dir_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(3);
    let lifecycle = FaultyLifecycle::healthy(0);

    // ensure_dirs is called inside full_reindex, so this should work
    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig {
            batch_size: 100,
            write_checkpoint: true,
        },
        &NoProgress,
    );

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.checkpoint_written);
    assert_eq!(result.stats.docs_indexed, 3);
}

#[test]
fn reindex_without_checkpoint_still_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(5);
    let lifecycle = FaultyLifecycle::healthy(0);

    let result = full_reindex(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ReindexConfig {
            batch_size: 100,
            write_checkpoint: false,
        },
        &NoProgress,
    )
    .unwrap();

    assert!(!result.checkpoint_written);
    assert_eq!(result.stats.docs_indexed, 5);
}

// ═══════════════════════════════════════════════════════════════════════
// Section 10: Consistency report structure invariants
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn consistency_report_findings_sorted_by_severity() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    // Not ready + count mismatch = multiple severity levels
    let source = FaultySource::healthy(100);
    let lifecycle = FaultyLifecycle::not_ready();

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    // Verify findings are sorted: errors first, then warnings, then info
    let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
    for window in severities.windows(2) {
        let a = match window[0] {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        };
        let b = match window[1] {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        };
        assert!(
            a <= b,
            "Findings should be sorted by severity: {severities:?}"
        );
    }
}

#[test]
fn consistency_report_error_count_matches_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = IndexLayout::new(tmp.path());

    let source = FaultySource::healthy(100);
    let lifecycle = FaultyLifecycle::not_ready();

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    let manual_error_count = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    assert_eq!(report.error_count(), manual_error_count);
}

#[test]
fn consistency_report_warning_count_matches_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = setup_healthy_index(&tmp, &scope, &schema, 99);

    let source = FaultySource::healthy(100);
    let lifecycle = FaultyLifecycle::healthy(99);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    let manual_warning_count = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .count();
    assert_eq!(report.warning_count(), manual_warning_count);
}

#[test]
fn healthy_report_has_zero_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = test_scope();
    let schema = test_schema();
    let layout = setup_healthy_index(&tmp, &scope, &schema, 10);

    let source = FaultySource::healthy(10);
    let lifecycle = FaultyLifecycle::healthy(10);

    let report = check_consistency(
        &source,
        &lifecycle,
        &layout,
        &scope,
        &schema,
        &ConsistencyConfig::default(),
    )
    .unwrap();

    assert!(report.healthy);
    assert_eq!(report.error_count(), 0);
    assert!(!report.rebuild_recommended);
}
