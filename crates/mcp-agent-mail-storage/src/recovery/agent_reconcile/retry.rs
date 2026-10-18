//! Bounded, ID-only retry debt for lifecycle profile recovery.
//!
//! A transient archive failure must not disappear behind the roster watermark.
//! Retaining a profile payload would be unsafe: retirement or deregistration
//! can change before the next attempt. Every scheduled ID is read afresh by
//! the ordinary source and publication checks in the parent module.

mod checkpoint;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::StorageError;

const MAX_RETRY_IDS: usize = 1024;
const MAX_READY_PER_BATCH: usize = 16;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(super) struct Failure {
    pub(super) detail: String,
    pub(super) transient: bool,
}

impl Failure {
    pub(super) fn source_unavailable(error: impl std::fmt::Display) -> Self {
        Self::transient(super::source_error(error))
    }

    pub(super) fn transient(detail: String) -> Self {
        Self {
            detail,
            transient: true,
        }
    }
}

impl From<String> for Failure {
    fn from(detail: String) -> Self {
        Self {
            detail,
            transient: false,
        }
    }
}

impl From<&str> for Failure {
    fn from(detail: &str) -> Self {
        detail.to_string().into()
    }
}

impl From<StorageError> for Failure {
    fn from(error: StorageError) -> Self {
        // Classify the typed error before rendering it. An identity conflict
        // mentioning a lock is not permission to put it on the fast lane.
        let transient = matches!(
            &error,
            StorageError::LockContention { .. }
                | StorageError::LockTimeout(_)
                | StorageError::GitIndexLock { .. }
        ) || matches!(&error, StorageError::Io(error)
            if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        Self {
            detail: error.to_string(),
            transient,
        }
    }
}

#[derive(Debug)]
struct Entry {
    id: i64,
    attempts: u32,
    ready_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct RetryQueue {
    entries: VecDeque<Entry>,
    // The roster watermark and retry debt are one atomic scheduling snapshot.
    // Keep its lifetime lock and restore-once state with the private queue.
    persistence: checkpoint::Persistence,
}

impl RetryQueue {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn contains(&self, id: i64) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    /// Reserve capacity logically before attempting a previously unseen ID.
    /// The single cursor owner cannot consume this slot concurrently. A full
    /// queue stops new scan work, not retries, and never evicts unresolved debt.
    pub(super) fn can_admit(&self, id: i64) -> bool {
        self.entries.len() < MAX_RETRY_IDS || self.contains(id)
    }

    /// Select without removing. Cancellation, exhausted repair budgets, and
    /// failed admission leave every unattempted obligation in the queue.
    pub(super) fn ready_ids(&self, now: Instant) -> Vec<i64> {
        self.entries
            .iter()
            .filter(|entry| entry.ready_at <= now)
            .take(MAX_READY_PER_BATCH)
            .map(|entry| entry.id)
            .collect()
    }

    pub(super) fn forget(&mut self, id: i64) {
        self.entries.retain(|entry| entry.id != id);
    }

    /// First refusal is eligible in the next batch, never a second attempt in
    /// the current batch. Repeated failures rotate behind other ready IDs and
    /// back off exponentially, capped at the normal one-minute worker cadence.
    pub(super) fn defer(&mut self, id: i64, now: Instant) {
        assert!(
            self.can_admit(id),
            "profile retry capacity must precede admission"
        );
        let previous = self
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .and_then(|index| self.entries.remove(index));
        let attempts = previous.map_or(0, |entry| entry.attempts.saturating_add(1));
        let delay = if attempts == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(1_u64 << attempts.saturating_sub(1).min(6)).min(MAX_BACKOFF)
        };
        self.entries.push_back(Entry {
            id,
            attempts,
            ready_at: now + delay,
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) id: i64,
    pub(super) retry: bool,
}

/// Both inputs have already been bounded so their sum is at most the original
/// 32-candidate batch. Alternate lanes rather than letting a four-repair budget
/// be consumed exclusively by retries or by continuously registered identities.
pub(super) fn interleave(
    scan: Vec<i64>,
    retries: Vec<i64>,
    mut prefer_retry: bool,
) -> Vec<Candidate> {
    let capacity = scan.len().saturating_add(retries.len());
    let mut scan = scan.into_iter();
    let mut retries = retries.into_iter();
    let mut result = Vec::with_capacity(capacity);
    loop {
        let candidate = if prefer_retry {
            retries
                .next()
                .map(|id| Candidate { id, retry: true })
                .or_else(|| scan.next().map(|id| Candidate { id, retry: false }))
        } else {
            scan.next()
                .map(|id| Candidate { id, retry: false })
                .or_else(|| retries.next().map(|id| Candidate { id, retry: true }))
        };
        let Some(candidate) = candidate else { break };
        prefer_retry = !candidate.retry;
        result.push(candidate);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_bounded_and_selection_does_not_consume_debt() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        queue.defer(7, now);
        assert_eq!(queue.ready_ids(now), [7]);
        assert_eq!(queue.ready_ids(now), [7]);
        queue.defer(7, now);
        assert!(queue.ready_ids(now).is_empty());
        assert_eq!(queue.ready_ids(now + Duration::from_secs(1)), [7]);
        for _ in 0..100 {
            queue.defer(7, now);
        }
        assert_eq!(queue.len(), 1);
        assert!(
            queue
                .ready_ids(now + MAX_BACKOFF - Duration::from_nanos(1))
                .is_empty()
        );
        assert_eq!(queue.ready_ids(now + MAX_BACKOFF), [7]);
        queue.forget(7);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn full_retry_queue_preserves_every_obligation_and_still_admits_retries() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        for id in 1..=i64::try_from(MAX_RETRY_IDS).unwrap() {
            assert!(queue.can_admit(id));
            queue.defer(id, now);
        }
        assert!(!queue.can_admit(10_000));
        assert!(queue.can_admit(1));
        queue.defer(1, now);
        assert_eq!(queue.len(), MAX_RETRY_IDS);
        assert!(queue.contains(1));
        assert!(queue.contains(i64::try_from(MAX_RETRY_IDS).unwrap()));
        assert_eq!(queue.ready_ids(now), (2..=17).collect::<Vec<_>>());
        queue.forget(2);
        assert!(queue.can_admit(10_000));
    }

    #[test]
    fn retry_and_history_lanes_both_progress_under_small_repair_budgets() {
        for prefer_retry in [false, true] {
            let candidates = interleave(vec![100, 101, 102], vec![1, 2, 3], prefer_retry);
            assert_eq!(candidates.len(), 6);
            for pair in candidates.chunks_exact(2) {
                assert_ne!(pair[0].retry, pair[1].retry);
            }
            assert_eq!(
                candidates
                    .iter()
                    .filter(|item| item.retry)
                    .map(|item| item.id)
                    .collect::<Vec<_>>(),
                [1, 2, 3]
            );
            assert_eq!(
                candidates
                    .iter()
                    .filter(|item| !item.retry)
                    .map(|item| item.id)
                    .collect::<Vec<_>>(),
                [100, 101, 102]
            );
        }
        assert!(interleave(Vec::new(), Vec::new(), true).is_empty());
        assert_eq!(
            interleave(vec![9], Vec::new(), true),
            [Candidate {
                id: 9,
                retry: false
            }]
        );
        assert_eq!(
            interleave(Vec::new(), vec![9], false),
            [Candidate { id: 9, retry: true }]
        );
    }

    #[test]
    fn only_typed_transient_storage_errors_enter_the_fast_lane() {
        for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
            let failure = Failure::from(StorageError::Io(std::io::Error::new(kind, "busy")));
            assert!(failure.transient);
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
        ] {
            let failure = Failure::from(StorageError::Io(std::io::Error::new(kind, "evidence")));
            assert!(!failure.transient);
        }
        assert!(
            !Failure::from(StorageError::InvalidPath(
                "publication fence busy is not identity authority".into()
            ))
            .transient
        );
        assert!(!Failure::from("agent source is ambiguous").transient);
    }
}
