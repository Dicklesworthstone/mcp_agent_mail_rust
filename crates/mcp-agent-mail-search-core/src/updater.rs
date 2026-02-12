//! Incremental index updater for search
//!
//! Bridges DB mutations (message/agent/project create/update/delete) to the
//! search index via [`DocChange`] batches. Supports backpressure to avoid
//! blocking the critical write path.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::document::{DocChange, DocKind};
use crate::engine::IndexLifecycle;
use crate::envelope::{
    AgentRow, MessageRow, ProjectRow, agent_to_envelope, message_to_envelope, project_to_envelope,
};
use crate::error::SearchResult;

/// Configuration for the incremental index updater
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// Maximum number of pending changes before applying a batch
    pub batch_size: usize,
    /// Maximum time to wait before flushing pending changes
    pub flush_interval: Duration,
    /// Maximum number of pending changes before dropping low-priority updates
    pub backpressure_threshold: usize,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            flush_interval: Duration::from_secs(5),
            backpressure_threshold: 1000,
        }
    }
}

/// Statistics about the updater's current state
#[derive(Debug, Clone, Default)]
pub struct UpdaterStats {
    /// Number of changes currently pending
    pub pending_count: usize,
    /// Total changes applied since start
    pub total_applied: u64,
    /// Total changes dropped due to backpressure
    pub total_dropped: u64,
    /// Number of flush cycles completed
    pub flush_count: u64,
    /// Last flush duration
    pub last_flush_duration: Option<Duration>,
}

/// Tracks pending changes and applies them to an [`IndexLifecycle`] backend.
///
/// This is intentionally synchronous — async integration with the server event
/// loop will be done at the wiring layer, not here.
pub struct IncrementalUpdater {
    config: UpdaterConfig,
    pending: Mutex<PendingState>,
}

struct PendingState {
    changes: VecDeque<DocChange>,
    last_flush: Instant,
    stats: UpdaterStats,
}

impl IncrementalUpdater {
    /// Create a new updater with default configuration
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(UpdaterConfig::default())
    }

    /// Create a new updater with custom configuration
    #[must_use]
    pub fn with_config(config: UpdaterConfig) -> Self {
        Self {
            config,
            pending: Mutex::new(PendingState {
                changes: VecDeque::new(),
                last_flush: Instant::now(),
                stats: UpdaterStats::default(),
            }),
        }
    }

    /// Enqueue a raw document change for later application.
    ///
    /// Returns `true` if the change was accepted, `false` if dropped due to
    /// backpressure.
    pub fn enqueue(&self, change: DocChange) -> bool {
        let mut state = self.pending.lock().expect("updater lock poisoned");
        if state.changes.len() >= self.config.backpressure_threshold {
            state.stats.total_dropped += 1;
            return false;
        }
        state.changes.push_back(change);
        true
    }

    /// Convenience: enqueue a message upsert from a DB row
    pub fn on_message_upsert(&self, row: &MessageRow) -> bool {
        let envelope = message_to_envelope(row);
        self.enqueue(DocChange::Upsert(envelope.document))
    }

    /// Convenience: enqueue a message deletion
    pub fn on_message_delete(&self, message_id: i64) -> bool {
        self.enqueue(DocChange::Delete {
            id: message_id,
            kind: DocKind::Message,
        })
    }

    /// Convenience: enqueue an agent upsert from a DB row
    pub fn on_agent_upsert(&self, row: &AgentRow) -> bool {
        let envelope = agent_to_envelope(row);
        self.enqueue(DocChange::Upsert(envelope.document))
    }

    /// Convenience: enqueue an agent deletion
    pub fn on_agent_delete(&self, agent_id: i64) -> bool {
        self.enqueue(DocChange::Delete {
            id: agent_id,
            kind: DocKind::Agent,
        })
    }

    /// Convenience: enqueue a project upsert from a DB row
    pub fn on_project_upsert(&self, row: &ProjectRow) -> bool {
        let envelope = project_to_envelope(row);
        self.enqueue(DocChange::Upsert(envelope.document))
    }

    /// Check if a flush is needed (batch full or interval elapsed)
    #[must_use]
    pub fn should_flush(&self) -> bool {
        let state = self.pending.lock().expect("updater lock poisoned");
        if state.changes.is_empty() {
            return false;
        }
        state.changes.len() >= self.config.batch_size
            || state.last_flush.elapsed() >= self.config.flush_interval
    }

    /// Get current statistics
    #[must_use]
    pub fn stats(&self) -> UpdaterStats {
        let guard = self.pending.lock().expect("updater lock poisoned");
        let mut result = guard.stats.clone();
        result.pending_count = guard.changes.len();
        result
    }

    /// Drain all pending changes and apply them to the given lifecycle backend.
    ///
    /// Returns the number of changes successfully applied.
    ///
    /// # Errors
    /// Returns `SearchError` if the backend fails to apply changes.
    pub fn flush(&self, backend: &dyn IndexLifecycle) -> SearchResult<usize> {
        let changes: Vec<DocChange> = {
            let mut state = self.pending.lock().expect("updater lock poisoned");
            state.changes.drain(..).collect()
        };

        if changes.is_empty() {
            return Ok(0);
        }

        let start = Instant::now();
        let applied = backend.update_incremental(&changes)?;
        let duration = start.elapsed();

        {
            let mut state = self.pending.lock().expect("updater lock poisoned");
            state.last_flush = Instant::now();
            state.stats.total_applied += applied as u64;
            state.stats.flush_count += 1;
            state.stats.last_flush_duration = Some(duration);
        }

        Ok(applied)
    }

    /// Drain pending changes without applying them (for testing or shutdown)
    pub fn drain(&self) -> Vec<DocChange> {
        let mut state = self.pending.lock().expect("updater lock poisoned");
        state.changes.drain(..).collect()
    }
}

impl Default for IncrementalUpdater {
    fn default() -> Self {
        Self::new()
    }
}

/// Deduplicate a batch of changes, keeping only the latest change per document.
///
/// This is useful before applying a batch: if a document was updated 5 times
/// in the batch, we only need to apply the last update.
#[must_use]
pub fn deduplicate_changes(changes: Vec<DocChange>) -> Vec<DocChange> {
    use std::collections::HashMap;

    // Key: (kind, id), Value: index in the output
    let mut seen: HashMap<(DocKind, i64), usize> = HashMap::new();
    let mut result: Vec<DocChange> = Vec::with_capacity(changes.len());

    for change in changes {
        let key = (change.doc_kind(), change.doc_id());
        if let Some(&idx) = seen.get(&key) {
            // Replace the earlier change with this one
            result[idx] = change;
        } else {
            seen.insert(key, result.len());
            result.push(change);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::engine::{IndexHealth, IndexStats};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockLifecycle {
        applied: AtomicUsize,
    }

    impl MockLifecycle {
        fn new() -> Self {
            Self {
                applied: AtomicUsize::new(0),
            }
        }
    }

    impl IndexLifecycle for MockLifecycle {
        fn rebuild(&self) -> SearchResult<IndexStats> {
            Ok(IndexStats {
                docs_indexed: 0,
                docs_removed: 0,
                elapsed_ms: 0,
                warnings: Vec::new(),
            })
        }

        fn update_incremental(&self, changes: &[DocChange]) -> SearchResult<usize> {
            self.applied.fetch_add(changes.len(), Ordering::Relaxed);
            Ok(changes.len())
        }

        fn health(&self) -> IndexHealth {
            IndexHealth {
                ready: true,
                doc_count: self.applied.load(Ordering::Relaxed),
                size_bytes: None,
                last_updated_ts: None,
                status_message: "mock".to_owned(),
            }
        }
    }

    fn sample_message_row() -> MessageRow {
        MessageRow {
            id: 1,
            project_id: 1,
            sender_id: 1,
            sender_name: Some("BlueLake".to_owned()),
            thread_id: None,
            subject: "test".to_owned(),
            body_md: "test body".to_owned(),
            importance: "normal".to_owned(),
            ack_required: false,
            created_ts: 1_700_000_000_000_000,
            product_ids: vec![],
        }
    }

    fn sample_agent_row() -> AgentRow {
        AgentRow {
            id: 1,
            project_id: 1,
            name: "BlueLake".to_owned(),
            program: "claude-code".to_owned(),
            model: "opus-4.6".to_owned(),
            task_description: "testing".to_owned(),
            inception_ts: 1_700_000_000_000_000,
            last_active_ts: 1_700_000_000_000_000,
            product_ids: vec![],
        }
    }

    fn sample_project_row() -> ProjectRow {
        ProjectRow {
            id: 1,
            slug: "test-project".to_owned(),
            human_key: "/tmp/test".to_owned(),
            created_at: 1_700_000_000_000_000,
            product_ids: vec![],
        }
    }

    #[test]
    fn updater_default_config() {
        let updater = IncrementalUpdater::new();
        assert_eq!(updater.config.batch_size, 100);
        assert_eq!(updater.config.flush_interval, Duration::from_secs(5));
        assert_eq!(updater.config.backpressure_threshold, 1000);
    }

    #[test]
    fn enqueue_and_flush() {
        let updater = IncrementalUpdater::new();
        let backend = MockLifecycle::new();

        assert!(updater.on_message_upsert(&sample_message_row()));
        assert!(updater.on_agent_upsert(&sample_agent_row()));
        assert!(updater.on_project_upsert(&sample_project_row()));

        let stats = updater.stats();
        assert_eq!(stats.pending_count, 3);

        let applied = updater.flush(&backend).unwrap();
        assert_eq!(applied, 3);
        assert_eq!(backend.applied.load(Ordering::Relaxed), 3);

        let stats = updater.stats();
        assert_eq!(stats.pending_count, 0);
        assert_eq!(stats.total_applied, 3);
        assert_eq!(stats.flush_count, 1);
    }

    #[test]
    fn flush_empty_is_noop() {
        let updater = IncrementalUpdater::new();
        let backend = MockLifecycle::new();
        let applied = updater.flush(&backend).unwrap();
        assert_eq!(applied, 0);
    }

    #[test]
    fn delete_operations() {
        let updater = IncrementalUpdater::new();
        assert!(updater.on_message_delete(42));
        assert!(updater.on_agent_delete(7));

        let changes = updater.drain();
        assert_eq!(changes.len(), 2);
        assert!(matches!(
            &changes[0],
            DocChange::Delete {
                id: 42,
                kind: DocKind::Message
            }
        ));
        assert!(matches!(
            &changes[1],
            DocChange::Delete {
                id: 7,
                kind: DocKind::Agent
            }
        ));
    }

    #[test]
    fn backpressure_drops_changes() {
        let updater = IncrementalUpdater::with_config(UpdaterConfig {
            backpressure_threshold: 3,
            ..UpdaterConfig::default()
        });

        assert!(updater.on_message_upsert(&sample_message_row()));
        assert!(updater.on_message_upsert(&sample_message_row()));
        assert!(updater.on_message_upsert(&sample_message_row()));
        // 4th should be dropped
        assert!(!updater.on_message_upsert(&sample_message_row()));

        let stats = updater.stats();
        assert_eq!(stats.pending_count, 3);
        assert_eq!(stats.total_dropped, 1);
    }

    #[test]
    fn should_flush_batch_full() {
        let updater = IncrementalUpdater::with_config(UpdaterConfig {
            batch_size: 2,
            ..UpdaterConfig::default()
        });

        assert!(!updater.should_flush()); // Empty
        updater.on_message_upsert(&sample_message_row());
        assert!(!updater.should_flush()); // 1 < batch_size
        updater.on_message_upsert(&sample_message_row());
        assert!(updater.should_flush()); // 2 >= batch_size
    }

    #[test]
    fn drain_returns_all_pending() {
        let updater = IncrementalUpdater::new();
        updater.on_message_upsert(&sample_message_row());
        updater.on_agent_upsert(&sample_agent_row());

        let changes = updater.drain();
        assert_eq!(changes.len(), 2);
        assert_eq!(updater.stats().pending_count, 0);
    }

    #[test]
    fn deduplicate_keeps_last() {
        let doc1 = Document {
            id: 1,
            kind: DocKind::Message,
            body: "v1".to_owned(),
            title: "title".to_owned(),
            project_id: Some(1),
            created_ts: 100,
            metadata: HashMap::new(),
        };
        let doc2 = Document {
            id: 1,
            kind: DocKind::Message,
            body: "v2".to_owned(),
            title: "title".to_owned(),
            project_id: Some(1),
            created_ts: 200,
            metadata: HashMap::new(),
        };
        let doc3 = Document {
            id: 2,
            kind: DocKind::Agent,
            body: "agent".to_owned(),
            title: "name".to_owned(),
            project_id: Some(1),
            created_ts: 300,
            metadata: HashMap::new(),
        };

        let changes = vec![
            DocChange::Upsert(doc1),
            DocChange::Upsert(doc2),
            DocChange::Upsert(doc3),
        ];

        let deduped = deduplicate_changes(changes);
        assert_eq!(deduped.len(), 2); // message:1 (v2) + agent:2
        if let DocChange::Upsert(ref doc) = deduped[0] {
            assert_eq!(doc.body, "v2"); // Last version kept
        } else {
            panic!("Expected upsert");
        }
    }

    #[test]
    fn deduplicate_delete_overrides_upsert() {
        let doc = Document {
            id: 1,
            kind: DocKind::Message,
            body: "content".to_owned(),
            title: "title".to_owned(),
            project_id: Some(1),
            created_ts: 100,
            metadata: HashMap::new(),
        };

        let changes = vec![
            DocChange::Upsert(doc),
            DocChange::Delete {
                id: 1,
                kind: DocKind::Message,
            },
        ];

        let deduped = deduplicate_changes(changes);
        assert_eq!(deduped.len(), 1);
        assert!(matches!(deduped[0], DocChange::Delete { .. }));
    }

    #[test]
    fn stats_update_after_multiple_flushes() {
        let updater = IncrementalUpdater::new();
        let backend = MockLifecycle::new();

        updater.on_message_upsert(&sample_message_row());
        updater.flush(&backend).unwrap();

        updater.on_message_upsert(&sample_message_row());
        updater.on_message_upsert(&sample_message_row());
        updater.flush(&backend).unwrap();

        let stats = updater.stats();
        assert_eq!(stats.total_applied, 3);
        assert_eq!(stats.flush_count, 2);
        assert!(stats.last_flush_duration.is_some());
    }
}
