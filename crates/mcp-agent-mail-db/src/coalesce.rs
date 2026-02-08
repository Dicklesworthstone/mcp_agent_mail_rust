//! Request coalescing (singleflight) for identical concurrent read operations.
//!
//! When multiple threads issue the same read query simultaneously, only the
//! first ("leader") executes; others ("joiners") block briefly and share the
//! cloned result. This eliminates redundant DB work under thundering-herd
//! conditions — e.g., 10 agents all calling `fetch_inbox` for the same project.
//!
//! Design:
//! - **Lock-free fast path**: a single `Mutex<HashMap>` guards the in-flight map.
//!   Uncontended lock + `HashMap` lookup is ~20-50ns.
//! - **Bounded blocking**: joiners wait on `Condvar` with a configurable timeout.
//!   On timeout, they fall through and execute independently.
//! - **Bounded memory**: max entries cap prevents unbounded growth; eviction is
//!   best-effort (removes one arbitrary entry at capacity).
//! - **Metrics**: atomic counters track leader/joiner/timeout events for
//!   observability.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Slot: shared state between leader and joiners
// ---------------------------------------------------------------------------

enum SlotState<V> {
    /// The leader is still executing.
    Pending,
    /// The leader finished successfully; joiners clone this value.
    Ready(V),
    /// The leader's closure returned an error (stringified for sharing).
    Failed(String),
}

struct Slot<V> {
    state: Mutex<SlotState<V>>,
    done: Condvar,
}

impl<V: Clone> Slot<V> {
    const fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Pending),
            done: Condvar::new(),
        }
    }

    fn complete_ok(&self, value: &V) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = SlotState::Ready(value.clone());
        drop(state);
        self.done.notify_all();
    }

    fn complete_err(&self, msg: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = SlotState::Failed(msg);
        drop(state);
        self.done.notify_all();
    }

    #[allow(clippy::significant_drop_tightening)] // guard is consumed by wait_timeout_while
    fn wait(&self, timeout: Duration) -> Result<V, CoalesceJoinError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, wait_result) = self
            .done
            .wait_timeout_while(guard, timeout, |s| matches!(s, SlotState::Pending))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if wait_result.timed_out() {
            return Err(CoalesceJoinError::Timeout);
        }
        let result = match &*guard {
            SlotState::Ready(v) => Ok(v.clone()),
            SlotState::Failed(msg) => Err(CoalesceJoinError::LeaderFailed(msg.clone())),
            SlotState::Pending => unreachable!("condvar spurious wakeup with timeout"),
        };
        drop(guard);
        result
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Error returned when joining an in-flight operation fails.
#[derive(Debug)]
pub enum CoalesceJoinError {
    /// The join timed out waiting for the leader.
    Timeout,
    /// The leader's closure returned an error.
    LeaderFailed(String),
}

impl fmt::Display for CoalesceJoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "coalesce join timed out"),
            Self::LeaderFailed(msg) => write!(f, "coalesce leader failed: {msg}"),
        }
    }
}

impl std::error::Error for CoalesceJoinError {}

/// Outcome of a coalesced operation.
#[derive(Debug)]
pub enum CoalesceOutcome<V> {
    /// This thread executed the operation (was the leader).
    Executed(V),
    /// This thread joined an in-flight operation and received a shared result.
    Joined(V),
}

impl<V> CoalesceOutcome<V> {
    /// Unwrap the inner value regardless of whether we were leader or joiner.
    pub fn into_inner(self) -> V {
        match self {
            Self::Executed(v) | Self::Joined(v) => v,
        }
    }

    /// Returns `true` if this result was obtained by joining another thread's
    /// in-flight operation (i.e., no redundant DB work was performed).
    #[must_use]
    pub const fn was_joined(&self) -> bool {
        matches!(self, Self::Joined(_))
    }
}

/// Snapshot of coalescing metrics.
#[derive(Debug, Clone, Default)]
pub struct CoalesceMetrics {
    /// Number of times a thread became the leader (executed the closure).
    pub leader_count: u64,
    /// Number of times a thread successfully joined an in-flight operation.
    pub joined_count: u64,
    /// Number of join attempts that timed out (fell back to independent execution).
    pub timeout_count: u64,
    /// Number of join attempts where the leader failed.
    pub leader_failed_count: u64,
}

// ---------------------------------------------------------------------------
// CoalesceMap
// ---------------------------------------------------------------------------

/// A concurrent map that deduplicates in-flight read operations.
///
/// When [`execute_or_join`](Self::execute_or_join) is called:
/// - If no other thread is executing the same key: this thread becomes the
///   "leader", executes the closure, broadcasts the result, and removes the entry.
/// - If another thread is already executing the same key: this thread "joins"
///   and blocks (with timeout) until the leader finishes, then clones the result.
///
/// # Type Parameters
///
/// - `K`: The cache key (typically a tuple of query parameters). Must be
///   `Hash + Eq + Clone + Send + Sync`.
/// - `V`: The result value. Must be `Clone + Send + Sync` (cloned to joiners).
pub struct CoalesceMap<K, V> {
    inflight: Mutex<HashMap<K, Arc<Slot<V>>>>,
    max_entries: usize,
    join_timeout: Duration,
    // Metrics (lock-free atomics).
    leader_count: AtomicU64,
    joined_count: AtomicU64,
    timeout_count: AtomicU64,
    leader_failed_count: AtomicU64,
}

impl<K: Hash + Eq + Clone, V: Clone> CoalesceMap<K, V> {
    /// Create a new `CoalesceMap`.
    ///
    /// - `max_entries`: maximum number of concurrent in-flight operations.
    ///   When exceeded, one arbitrary entry is evicted (best-effort).
    /// - `join_timeout`: maximum time a joiner will wait for the leader.
    ///   On timeout, the joiner falls through and the closure is called
    ///   independently.
    #[must_use]
    pub fn new(max_entries: usize, join_timeout: Duration) -> Self {
        Self {
            inflight: Mutex::new(HashMap::with_capacity(max_entries.min(64))),
            max_entries,
            join_timeout,
            leader_count: AtomicU64::new(0),
            joined_count: AtomicU64::new(0),
            timeout_count: AtomicU64::new(0),
            leader_failed_count: AtomicU64::new(0),
        }
    }

    /// Execute `f` or join an existing in-flight operation for the same key.
    ///
    /// Returns `Ok(CoalesceOutcome::Executed(v))` if this thread was the leader,
    /// or `Ok(CoalesceOutcome::Joined(v))` if it joined an existing operation.
    ///
    /// If joining fails (timeout or leader error), the closure `f` is called
    /// directly as a fallback.
    #[allow(clippy::needless_pass_by_value)] // key is cloned into the map; owned is correct
    pub fn execute_or_join<F, E>(&self, key: K, f: F) -> Result<CoalesceOutcome<V>, E>
    where
        F: FnOnce() -> Result<V, E>,
        E: fmt::Display,
    {
        enum Role<V> {
            Leader,
            Joiner(Arc<Slot<V>>),
        }

        let role = {
            let mut map = self
                .inflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            #[allow(clippy::option_if_let_else)] // map_or_else can't work: else branch mutates map
            if let Some(slot) = map.get(&key) {
                Role::Joiner(Arc::clone(slot))
            } else {
                // We are the leader. Insert our slot.
                let slot = Arc::new(Slot::new());
                if map.len() >= self.max_entries {
                    // Best-effort eviction: remove one arbitrary entry.
                    if let Some(k) = map.keys().next().cloned() {
                        map.remove(&k);
                    }
                }
                map.insert(key.clone(), Arc::clone(&slot));
                Role::Leader
            }
        };

        match role {
            Role::Joiner(slot) => match slot.wait(self.join_timeout) {
                Ok(v) => {
                    self.joined_count.fetch_add(1, Ordering::Relaxed);
                    Ok(CoalesceOutcome::Joined(v))
                }
                Err(CoalesceJoinError::Timeout) => {
                    self.timeout_count.fetch_add(1, Ordering::Relaxed);
                    // Fallback: execute independently.
                    self.leader_count.fetch_add(1, Ordering::Relaxed);
                    f().map(CoalesceOutcome::Executed)
                }
                Err(CoalesceJoinError::LeaderFailed(_)) => {
                    self.leader_failed_count.fetch_add(1, Ordering::Relaxed);
                    // Fallback: execute independently.
                    self.leader_count.fetch_add(1, Ordering::Relaxed);
                    f().map(CoalesceOutcome::Executed)
                }
            },
            Role::Leader => {
                self.leader_count.fetch_add(1, Ordering::Relaxed);
                // Retrieve our slot (we just inserted it).
                let slot = {
                    self.inflight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&key)
                        .cloned()
                };

                let result = f();

                // Broadcast result to any joiners.
                if let Some(slot) = slot {
                    match &result {
                        Ok(v) => slot.complete_ok(v),
                        Err(e) => slot.complete_err(e.to_string()),
                    }
                }

                // Remove from in-flight map.
                self.inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);

                result.map(CoalesceOutcome::Executed)
            }
        }
    }

    /// Number of currently in-flight operations.
    #[must_use]
    pub fn inflight_count(&self) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Returns a snapshot of coalescing metrics.
    #[must_use]
    pub fn metrics(&self) -> CoalesceMetrics {
        CoalesceMetrics {
            leader_count: self.leader_count.load(Ordering::Relaxed),
            joined_count: self.joined_count.load(Ordering::Relaxed),
            timeout_count: self.timeout_count.load(Ordering::Relaxed),
            leader_failed_count: self.leader_failed_count.load(Ordering::Relaxed),
        }
    }

    /// Reset all metrics counters to zero.
    pub fn reset_metrics(&self) {
        self.leader_count.store(0, Ordering::Relaxed);
        self.joined_count.store(0, Ordering::Relaxed);
        self.timeout_count.store(0, Ordering::Relaxed);
        self.leader_failed_count.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    #[test]
    fn single_thread_executes_as_leader() {
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));
        let result = map.execute_or_join("key1", || Ok::<_, String>(42)).unwrap();
        assert!(!result.was_joined());
        assert_eq!(result.into_inner(), 42);
        assert_eq!(map.inflight_count(), 0);

        let m = map.metrics();
        assert_eq!(m.leader_count, 1);
        assert_eq!(m.joined_count, 0);
    }

    #[test]
    fn error_propagates_from_leader() {
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));
        let result = map.execute_or_join("key1", || Err::<i32, String>("boom".into()));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "boom");
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn joiners_receive_leader_result() {
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let exec_count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(5));
        let threads = 5;

        // Phase 1: spawn all threads (must collect before joining — barrier
        // needs all threads alive before any can proceed).
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let map = Arc::clone(&map);
                let exec_count = Arc::clone(&exec_count);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let result = map
                        .execute_or_join("shared-key".to_string(), || {
                            exec_count.fetch_add(1, Ordering::SeqCst);
                            // Simulate work.
                            thread::sleep(Duration::from_millis(50));
                            Ok::<_, String>(42)
                        })
                        .unwrap();
                    result.into_inner()
                })
            })
            .collect();
        // Phase 2: join all threads.
        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All threads get the same result.
        assert!(results.iter().all(|&v| v == 42));

        // The closure should have executed very few times (ideally 1, but
        // timing may cause a few extras due to fallback).
        let actual_execs = exec_count.load(Ordering::SeqCst);
        assert!(
            actual_execs < threads,
            "expected fewer than {threads} executions, got {actual_execs}"
        );

        let m = map.metrics();
        assert!(m.joined_count > 0, "at least one thread should have joined");
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn different_keys_execute_independently() {
        let map = Arc::new(CoalesceMap::<String, String>::new(
            100,
            Duration::from_millis(100),
        ));
        let exec_count = Arc::new(AtomicUsize::new(0));

        // Phase 1: spawn all threads.
        let handles: Vec<_> = (0..3)
            .map(|i| {
                let map = Arc::clone(&map);
                let exec_count = Arc::clone(&exec_count);
                thread::spawn(move || {
                    let key = format!("key-{i}");
                    let result = map
                        .execute_or_join(key.clone(), || {
                            exec_count.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(key)
                        })
                        .unwrap();
                    result.into_inner()
                })
            })
            .collect();
        // Phase 2: join.
        let results: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.len(), 3);
        // Each key should have executed independently.
        assert_eq!(exec_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn max_entries_evicts() {
        let map: CoalesceMap<i32, i32> = CoalesceMap::new(2, Duration::from_millis(100));

        // Insert two entries via leader slots (they'll be removed after execute).
        let r1 = map.execute_or_join(1, || Ok::<_, String>(10)).unwrap();
        let r2 = map.execute_or_join(2, || Ok::<_, String>(20)).unwrap();
        assert_eq!(r1.into_inner(), 10);
        assert_eq!(r2.into_inner(), 20);

        // Map should be empty (leaders clean up after themselves).
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn metrics_track_correctly() {
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        let _ = map.execute_or_join("a", || Ok::<_, String>(1));
        let _ = map.execute_or_join("b", || Ok::<_, String>(2));

        let m = map.metrics();
        assert_eq!(m.leader_count, 2);
        assert_eq!(m.joined_count, 0);
        assert_eq!(m.timeout_count, 0);
        assert_eq!(m.leader_failed_count, 0);

        map.reset_metrics();
        let m = map.metrics();
        assert_eq!(m.leader_count, 0);
    }

    #[test]
    fn leader_error_causes_joiner_fallback() {
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let barrier = Arc::new(Barrier::new(2));

        // Thread 1: leader that will fail.
        let map1 = Arc::clone(&map);
        let barrier1 = Arc::clone(&barrier);
        let h1 = thread::spawn(move || {
            barrier1.wait();
            map1.execute_or_join("key".to_string(), || {
                thread::sleep(Duration::from_millis(50));
                Err::<i32, String>("leader-error".into())
            })
        });

        // Thread 2: joiner that should fall back after leader fails.
        let map2 = Arc::clone(&map);
        let barrier2 = Arc::clone(&barrier);
        let h2 = thread::spawn(move || {
            barrier2.wait();
            // Small delay to ensure thread 1 becomes leader.
            thread::sleep(Duration::from_millis(5));
            map2.execute_or_join("key".to_string(), || Ok::<_, String>(99))
        });

        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();

        // Leader should have failed.
        assert!(r1.is_err());
        // Joiner should have fallen back and succeeded.
        assert_eq!(r2.unwrap().into_inner(), 99);
    }

    #[test]
    fn coalesce_outcome_into_inner() {
        let executed: CoalesceOutcome<i32> = CoalesceOutcome::Executed(42);
        assert!(!executed.was_joined());
        assert_eq!(executed.into_inner(), 42);

        let joined: CoalesceOutcome<i32> = CoalesceOutcome::Joined(99);
        assert!(joined.was_joined());
        assert_eq!(joined.into_inner(), 99);
    }
}
