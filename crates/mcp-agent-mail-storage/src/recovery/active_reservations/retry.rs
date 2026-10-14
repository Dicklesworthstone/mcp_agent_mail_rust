//! Bounded scheduling debt for active reservation artifacts.
//!
//! Keep IDs, never a captured ACTIVE payload. Every retry must reread the live
//! source and repeat create-only publication checks. A full queue refuses new
//! debt rather than evicting an older contended lease behind the scan cursor.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_RETRY_IDS: usize = 1024;
const MAX_DUE_IDS: usize = 16;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct Entry {
    id: i64,
    attempts: u32,
    ready_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct RetryQueue {
    entries: VecDeque<Entry>,
}

impl RetryQueue {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn contains(&self, id: i64) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    pub(super) fn is_full(&self) -> bool {
        self.entries.len() >= MAX_RETRY_IDS
    }

    pub(super) fn due(&self, now: Instant) -> Vec<i64> {
        self.entries
            .iter()
            .filter(|entry| entry.ready_at <= now)
            .take(MAX_DUE_IDS)
            .map(|entry| entry.id)
            .collect()
    }

    pub(super) fn forget(&mut self, id: i64) {
        self.entries.retain(|entry| entry.id != id);
    }

    /// False leaves the queue unchanged: the caller must retain that scan
    /// position for the next pass rather than forget an unrecorded deferral.
    pub(super) fn defer(&mut self, id: i64, now: Instant) -> bool {
        let previous = self.entries.iter().position(|entry| entry.id == id);
        if previous.is_none() && self.is_full() {
            return false;
        }
        let attempts = previous
            .and_then(|index| self.entries.remove(index))
            .map_or(0, |entry| entry.attempts.saturating_add(1));
        // First contention gets another chance on the next worker pass.
        // Persistent contention backs off, but never beyond the idle cadence.
        let delay = if attempts == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(1_u64 << attempts.saturating_sub(1).min(5)).min(MAX_BACKOFF)
        };
        self.entries.push_back(Entry {
            id,
            attempts,
            ready_at: now + delay,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_deferral_is_due_without_waiting_for_a_complete_scan_round() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        assert!(queue.defer(401, now));
        assert_eq!(queue.due(now), [401]);
        assert_eq!(queue.len(), 1);
        assert!(queue.contains(401));
    }

    #[test]
    fn repeated_contention_backs_off_to_a_finite_ceiling() {
        let mut now = Instant::now();
        let mut queue = RetryQueue::default();
        assert!(queue.defer(401, now));
        for seconds in [1, 2, 4, 8, 16, 30, 30, 30] {
            assert!(queue.defer(401, now));
            let delay = Duration::from_secs(seconds);
            assert!(queue.due(now + delay - Duration::from_nanos(1)).is_empty());
            assert_eq!(queue.due(now + delay), [401]);
            assert_eq!(queue.len(), 1, "re-deferral must not duplicate an ID");
            now += delay;
        }
    }

    #[test]
    fn cooling_front_does_not_block_ready_ids_behind_it() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        assert!(queue.defer(401, now));
        assert!(queue.defer(401, now));
        assert!(queue.defer(402, now));
        assert_eq!(queue.due(now), [402]);
        assert_eq!(queue.due(now + Duration::from_secs(1)), [401, 402]);
    }

    #[test]
    fn due_snapshot_is_bounded_and_redeferral_rotates_fifo_order() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        for id in 1..=32 {
            assert!(queue.defer(id, now));
        }
        assert_eq!(queue.due(now), (1..=16).collect::<Vec<_>>());
        for id in 1..=16 {
            assert!(queue.defer(id, now));
        }
        assert_eq!(queue.due(now), (17..=32).collect::<Vec<_>>());
        // Even when the first group becomes due, later retained IDs keep their
        // place. Taking a scheduling snapshot itself never consumes any debt.
        assert_eq!(queue.due(now + MAX_BACKOFF), (17..=32).collect::<Vec<_>>());
        assert_eq!(queue.len(), 32);
    }

    #[test]
    fn full_queue_never_evicts_an_unrepaired_lease() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        for id in 1..=i64::try_from(MAX_RETRY_IDS).unwrap() {
            assert!(queue.defer(id, now));
        }
        assert!(queue.is_full());
        let before = queue.due(now);
        assert!(!queue.defer(50_000, now));
        assert_eq!(queue.len(), MAX_RETRY_IDS);
        assert_eq!(queue.due(now), before);
        assert!(!queue.contains(50_000));
        assert!(queue.defer(1, now), "an existing debt can still back off at capacity");
        assert_eq!(queue.len(), MAX_RETRY_IDS);
        queue.forget(2);
        assert!(!queue.is_full());
        assert!(queue.defer(50_000, now));
        assert!(queue.contains(1));
        assert!(!queue.contains(2));
    }

    #[test]
    fn retiring_a_released_expired_or_repaired_id_clears_its_backoff() {
        let now = Instant::now();
        let mut queue = RetryQueue::default();
        assert!(queue.defer(401, now));
        assert!(queue.defer(401, now));
        queue.forget(401);
        queue.forget(999);
        assert_eq!(queue.len(), 0);
        assert!(queue.due(now + MAX_BACKOFF).is_empty());
        assert!(queue.defer(401, now));
        assert_eq!(queue.due(now), [401]);
    }
}
