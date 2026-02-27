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
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Number of independent shards for the coalesce map.
///
/// Each shard has its own mutex, so operations on keys that hash to
/// different shards never contend. 16 is a good default: it is small
/// enough that `inflight_count()` (which sums all shards) stays fast,
/// and large enough that contention is negligible for typical workloads.
const NUM_SHARDS: usize = 16;

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&str>() {
        return format!("leader panicked: {msg}");
    }
    if let Some(msg) = payload.downcast_ref::<String>() {
        return format!("leader panicked: {msg}");
    }
    "leader panicked".to_string()
}

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

/// A concurrent map that deduplicates in-flight read operations using 16
/// independent shards to minimise lock contention.
///
/// When [`execute_or_join`](Self::execute_or_join) is called:
/// - If no other thread is executing the same key: this thread becomes the
///   "leader", executes the closure, broadcasts the result, and removes the entry.
/// - If another thread is already executing the same key: this thread "joins"
///   and blocks (with timeout) until the leader finishes, then clones the result.
///
/// Keys are routed to shards via `DefaultHasher` (FNV-quality distribution).
/// Operations on keys in different shards never contend on the same mutex.
///
/// # Type Parameters
///
/// - `K`: The cache key (typically a tuple of query parameters). Must be
///   `Hash + Eq + Clone + Send + Sync`.
/// - `V`: The result value. Must be `Clone + Send + Sync` (cloned to joiners).
pub struct ShardedCoalesceMap<K, V> {
    shards: [Mutex<HashMap<K, Arc<Slot<V>>>>; NUM_SHARDS],
    max_entries_per_shard: usize,
    join_timeout: Duration,
    // Metrics (lock-free atomics).
    leader_count: AtomicU64,
    joined_count: AtomicU64,
    timeout_count: AtomicU64,
    leader_failed_count: AtomicU64,
}

/// Backward-compatible alias. All existing code continues to use `CoalesceMap`.
pub type CoalesceMap<K, V> = ShardedCoalesceMap<K, V>;

impl<K: Hash + Eq + Clone, V: Clone> ShardedCoalesceMap<K, V> {
    /// Create a new `ShardedCoalesceMap`.
    ///
    /// - `max_entries`: maximum number of concurrent in-flight operations
    ///   (divided equally across shards). When a shard exceeds its share,
    ///   one arbitrary entry is evicted (best-effort).
    /// - `join_timeout`: maximum time a joiner will wait for the leader.
    ///   On timeout, the joiner falls through and the closure is called
    ///   independently.
    #[must_use]
    pub fn new(max_entries: usize, join_timeout: Duration) -> Self {
        let per_shard = max_entries.saturating_add(NUM_SHARDS - 1) / NUM_SHARDS;
        let cap = per_shard.min(8);
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashMap::with_capacity(cap))),
            max_entries_per_shard: per_shard,
            join_timeout,
            leader_count: AtomicU64::new(0),
            joined_count: AtomicU64::new(0),
            timeout_count: AtomicU64::new(0),
            leader_failed_count: AtomicU64::new(0),
        }
    }

    /// Compute the shard index for a key using `DefaultHasher`.
    #[allow(clippy::cast_possible_truncation)] // modulo 16 fits in any pointer width
    fn shard_index(key: &K) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % NUM_SHARDS
    }

    /// Execute `f` or join an existing in-flight operation for the same key.
    ///
    /// Returns `Ok(CoalesceOutcome::Executed(v))` if this thread was the leader,
    /// or `Ok(CoalesceOutcome::Joined(v))` if it joined an existing operation.
    ///
    /// If joining fails (timeout or leader error), the closure `f` is called
    /// directly as a fallback.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)] // key is cloned into the map; owned is correct
    pub fn execute_or_join<F, E>(&self, key: K, f: F) -> Result<CoalesceOutcome<V>, E>
    where
        F: FnOnce() -> Result<V, E>,
        E: fmt::Display,
    {
        enum Role<V> {
            Leader(Arc<Slot<V>>),
            Joiner(Arc<Slot<V>>),
        }

        let shard_idx = Self::shard_index(&key);

        let role = {
            let mut map = self.shards[shard_idx]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            #[allow(clippy::option_if_let_else)] // map_or_else can't work: else branch mutates map
            if let Some(slot) = map.get(&key).map(Arc::clone) {
                drop(map);
                Role::Joiner(slot)
            } else {
                // We are the leader. Insert our slot.
                let slot = Arc::new(Slot::new());
                if map.len() >= self.max_entries_per_shard {
                    // Eviction strategy: try to find an entry with no active joiners (strong_count == 2).
                    // (1 reference in the map, 1 held by the currently executing leader).
                    // If none found, fall back to arbitrary eviction (best-effort).
                    let key_to_evict = map
                        .iter()
                        .find(|(_, slot)| Arc::strong_count(slot) == 2)
                        .map(|(k, _)| k.clone())
                        .or_else(|| map.keys().next().cloned());
                    if let Some(k) = key_to_evict {
                        map.remove(&k);
                    }
                }
                map.insert(key.clone(), Arc::clone(&slot));
                drop(map);
                Role::Leader(slot)
            }
        };

        let inflight_count = self.shards[shard_idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();

        match role {
            Role::Joiner(slot) => {
                mcp_agent_mail_core::evidence_ledger().record(
                    "coalesce.outcome",
                    serde_json::json!({ "inflight_count": inflight_count }),
                    "joined",
                    Some("join_rate >= 0.3".into()),
                    0.8,
                    "coalesce_v1",
                );
                match slot.wait(self.join_timeout) {
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
                }
            }
            Role::Leader(slot) => {
                mcp_agent_mail_core::evidence_ledger().record(
                    "coalesce.outcome",
                    serde_json::json!({ "inflight_count": inflight_count }),
                    "executed",
                    Some("join_rate >= 0.3".into()),
                    0.8,
                    "coalesce_v1",
                );
                self.leader_count.fetch_add(1, Ordering::Relaxed);

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

                match result {
                    Ok(result) => {
                        // Broadcast result to any joiners.
                        match &result {
                            Ok(v) => slot.complete_ok(v),
                            Err(e) => slot.complete_err(e.to_string()),
                        }

                        // Remove from in-flight map only if it hasn't been replaced.
                        let mut map = self.shards[shard_idx]
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(existing) = map.get(&key)
                            && Arc::ptr_eq(existing, &slot)
                        {
                            map.remove(&key);
                        }
                        drop(map);

                        result.map(CoalesceOutcome::Executed)
                    }
                    Err(payload) => {
                        slot.complete_err(panic_payload_message(payload.as_ref()));
                        let mut map = self.shards[shard_idx]
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(existing) = map.get(&key)
                            && Arc::ptr_eq(existing, &slot)
                        {
                            map.remove(&key);
                        }
                        drop(map);
                        std::panic::resume_unwind(payload);
                    }
                }
            }
        }
    }

    /// Number of currently in-flight operations (sum across all shards).
    #[must_use]
    pub fn inflight_count(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                s.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
            })
            .sum()
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
    use std::sync::mpsc;
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
    fn leader_error_metrics_exact_for_single_joiner_fallback() {
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(2)));
        let barrier = Arc::new(Barrier::new(2));

        let map1 = Arc::clone(&map);
        let barrier1 = Arc::clone(&barrier);
        let leader = thread::spawn(move || {
            barrier1.wait();
            map1.execute_or_join("metric-leader-fail".to_string(), || {
                thread::sleep(Duration::from_millis(50));
                Err::<i32, String>("leader-failed".into())
            })
        });

        let map2 = Arc::clone(&map);
        let barrier2 = Arc::clone(&barrier);
        let joiner = thread::spawn(move || {
            barrier2.wait();
            thread::sleep(Duration::from_millis(5));
            map2.execute_or_join("metric-leader-fail".to_string(), || Ok::<_, String>(7))
        });

        let leader_res = leader.join().unwrap();
        let joiner_res = joiner.join().unwrap();
        assert!(leader_res.is_err());
        assert_eq!(joiner_res.unwrap().into_inner(), 7);

        let metrics = map.metrics();
        assert_eq!(metrics.leader_count, 2);
        assert_eq!(metrics.joined_count, 0);
        assert_eq!(metrics.timeout_count, 0);
        assert_eq!(metrics.leader_failed_count, 1);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn leader_panic_unblocks_joiner_and_cleans_up_slot() {
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            100,
            Duration::from_millis(400),
        ));
        let (leader_ready_tx, leader_ready_rx) = mpsc::sync_channel::<()>(1);
        let (panic_now_tx, panic_now_rx) = mpsc::sync_channel::<()>(1);

        let map1 = Arc::clone(&map);
        let leader = thread::spawn(move || {
            map1.execute_or_join("panic-key".to_string(), || {
                leader_ready_tx
                    .send(())
                    .expect("leader should signal readiness");
                let _ = panic_now_rx.recv();
                panic!("boom");
                #[allow(unreachable_code)]
                Ok::<_, String>(1)
            })
        });

        let map2 = Arc::clone(&map);
        let joiner = thread::spawn(move || {
            leader_ready_rx
                .recv()
                .expect("joiner should wait for leader readiness");
            map2.execute_or_join("panic-key".to_string(), || Ok::<_, String>(77))
        });

        thread::sleep(Duration::from_millis(10));
        panic_now_tx
            .send(())
            .expect("leader should receive panic signal");

        assert!(leader.join().is_err(), "leader should panic");
        let joiner_res = joiner.join().unwrap().unwrap();
        assert_eq!(joiner_res.into_inner(), 77);
        assert_eq!(map.inflight_count(), 0);

        let after = map
            .execute_or_join("panic-key".to_string(), || Ok::<_, String>(123))
            .unwrap();
        assert_eq!(after.into_inner(), 123);
        assert_eq!(map.inflight_count(), 0);

        let metrics = map.metrics();
        assert_eq!(metrics.timeout_count, 0);
        assert_eq!(metrics.leader_failed_count, 1);
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

    // ---- WBQ error path tests (br-3h13.3.1) ----

    #[test]
    fn queue_overflow_evicts_and_still_works() {
        // With max_entries=1, inserting a second concurrent key should evict the first
        // slot from the map. Verify the map does not panic and the new operation succeeds.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(1, Duration::from_millis(200));

        // First operation completes normally (leader, inserts then removes).
        let r1 = map.execute_or_join("a", || Ok::<_, String>(10)).unwrap();
        assert_eq!(r1.into_inner(), 10);
        assert_eq!(map.inflight_count(), 0);

        // Second operation also succeeds.
        let r2 = map.execute_or_join("b", || Ok::<_, String>(20)).unwrap();
        assert_eq!(r2.into_inner(), 20);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn queue_overflow_under_concurrency() {
        // max_entries=2 with 10 concurrent operations on distinct keys.
        // The map should evict aggressively but all operations must still complete
        // (evicted slots lose their broadcast, but leaders still return their result).
        let map = Arc::new(CoalesceMap::<String, usize>::new(
            2,
            Duration::from_millis(500),
        ));
        let barrier = Arc::new(Barrier::new(10));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let key = format!("overflow-key-{i}");
                    map.execute_or_join(key, || {
                        thread::sleep(Duration::from_millis(10));
                        Ok::<_, String>(i)
                    })
                    .unwrap()
                    .into_inner()
                })
            })
            .collect();

        let results: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Each thread should get its own value back (they use distinct keys).
        assert_eq!(results.len(), 10);
        // After all complete, the map should be empty.
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn post_completion_reuse() {
        // After all operations complete (simulating "post-shutdown" of a batch),
        // enqueuing new operations should still work normally.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        // First "batch"
        for i in 0..5 {
            let key = if i == 0 {
                "x"
            } else if i == 1 {
                "y"
            } else if i == 2 {
                "z"
            } else if i == 3 {
                "w"
            } else {
                "v"
            };
            let r = map.execute_or_join(key, || Ok::<_, String>(i)).unwrap();
            assert_eq!(r.into_inner(), i);
        }
        assert_eq!(map.inflight_count(), 0);

        let m1 = map.metrics();
        assert_eq!(m1.leader_count, 5);

        // "Post-shutdown" batch: the map should still accept new work.
        for i in 10..15 {
            let key = if i == 10 {
                "a"
            } else if i == 11 {
                "b"
            } else if i == 12 {
                "c"
            } else if i == 13 {
                "d"
            } else {
                "e"
            };
            let r = map.execute_or_join(key, || Ok::<_, String>(i)).unwrap();
            assert_eq!(r.into_inner(), i);
        }
        assert_eq!(map.inflight_count(), 0);

        let m2 = map.metrics();
        assert_eq!(m2.leader_count, 10);
    }

    #[test]
    fn error_propagation_all_callers_get_error() {
        // When the leader's closure returns Err, the leader gets the original error
        // and the map is cleaned up so subsequent calls work fine.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        let r = map.execute_or_join("err-key", || Err::<i32, String>("disk full".into()));
        assert!(r.is_err());
        assert_eq!(r.unwrap_err(), "disk full");
        assert_eq!(map.inflight_count(), 0);

        // A follow-up call with the same key should work (slot was cleaned up).
        let r2 = map
            .execute_or_join("err-key", || Ok::<_, String>(42))
            .unwrap();
        assert_eq!(r2.into_inner(), 42);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn error_propagation_typed_error() {
        // Test with a custom error type to verify generic E: Display constraint.
        #[derive(Debug)]
        struct DiskFullError;
        impl fmt::Display for DiskFullError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "disk full")
            }
        }

        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        let r = map.execute_or_join("typed-err", || Err::<i32, DiskFullError>(DiskFullError));
        assert!(r.is_err());
        let err_msg = format!("{}", r.unwrap_err());
        assert_eq!(err_msg, "disk full");
    }

    #[test]
    fn empty_map_execute_works() {
        // Executing on a completely fresh map with no pending items should work
        // (this is the "empty flush" case: no prior state to interfere).
        let map: CoalesceMap<String, Vec<i32>> = CoalesceMap::new(100, Duration::from_millis(100));

        assert_eq!(map.inflight_count(), 0);
        let m = map.metrics();
        assert_eq!(m.leader_count, 0);
        assert_eq!(m.joined_count, 0);

        let r = map
            .execute_or_join("first".to_string(), || Ok::<_, String>(vec![1, 2, 3]))
            .unwrap();
        assert!(!r.was_joined());
        assert_eq!(r.into_inner(), vec![1, 2, 3]);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn concurrent_enqueue_during_slow_leader() {
        // A leader takes a long time. While it executes, new operations with the
        // SAME key arrive and join. Also, operations with DIFFERENT keys arrive
        // and execute independently in parallel.
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let exec_count = Arc::new(AtomicUsize::new(0));

        // Phase 1: start a slow leader for "slow-key".
        let map_leader = Arc::clone(&map);
        let exec_count_leader = Arc::clone(&exec_count);
        let leader_started = Arc::new(Barrier::new(2));
        let leader_started_clone = Arc::clone(&leader_started);

        let h_leader = thread::spawn(move || {
            map_leader
                .execute_or_join("slow-key".to_string(), || {
                    exec_count_leader.fetch_add(1, Ordering::SeqCst);
                    // Signal that leader has started executing.
                    leader_started_clone.wait();
                    // Simulate slow work.
                    thread::sleep(Duration::from_millis(200));
                    Ok::<_, String>(100)
                })
                .unwrap()
                .into_inner()
        });

        // Wait for leader to start executing.
        leader_started.wait();

        // Phase 2: spawn joiners for the same key and independent workers for different keys.
        let mut handles: Vec<_> = Vec::new();

        // 3 joiners for "slow-key"
        for _ in 0..3 {
            let map = Arc::clone(&map);
            let exec_count = Arc::clone(&exec_count);
            handles.push(thread::spawn(move || {
                let r = map
                    .execute_or_join("slow-key".to_string(), || {
                        exec_count.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(999) // fallback value if join fails
                    })
                    .unwrap();
                (r.was_joined(), r.into_inner())
            }));
        }

        // 2 independent keys that should execute in parallel
        for i in 0..2 {
            let map = Arc::clone(&map);
            handles.push(thread::spawn(move || {
                let key = format!("independent-{i}");
                let r = map
                    .execute_or_join(key, || Ok::<_, String>(i + 200))
                    .unwrap();
                (r.was_joined(), r.into_inner())
            }));
        }

        let leader_val = h_leader.join().unwrap();
        assert_eq!(leader_val, 100);

        let results: Vec<(bool, i32)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Joiners for "slow-key" should get 100 (the leader's result).
        // They might have joined OR timed out and fallen back.
        // Either way, the values should be either 100 (joined) or 999 (fallback).
        for (idx, (was_joined, val)) in results.iter().enumerate() {
            if idx < 3 {
                // Joiner for slow-key
                assert!(
                    *val == 100 || *val == 999,
                    "joiner got unexpected value {val} (joined={was_joined})"
                );
            }
        }
        // Independent keys should get their own values
        for &(_, val) in &results[3..] {
            assert!(val == 200 || val == 201, "independent got {val}");
        }

        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn double_reset_metrics_is_safe() {
        // Calling reset_metrics multiple times (analogous to "double shutdown")
        // should be safe and idempotent.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        let _ = map.execute_or_join("a", || Ok::<_, String>(1));
        let _ = map.execute_or_join("b", || Ok::<_, String>(2));

        let m = map.metrics();
        assert_eq!(m.leader_count, 2);

        map.reset_metrics();
        let m = map.metrics();
        assert_eq!(m.leader_count, 0);
        assert_eq!(m.joined_count, 0);

        // Second reset should also be fine.
        map.reset_metrics();
        let m = map.metrics();
        assert_eq!(m.leader_count, 0);
        assert_eq!(m.joined_count, 0);

        // Map is still usable after double-reset.
        let r = map.execute_or_join("c", || Ok::<_, String>(3)).unwrap();
        assert_eq!(r.into_inner(), 3);
        let m = map.metrics();
        assert_eq!(m.leader_count, 1);
    }

    #[test]
    fn double_use_same_key_sequential() {
        // Using the same key twice sequentially (after the first operation finishes)
        // should work fine -- the slot is removed after leader completes.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        let r1 = map
            .execute_or_join("reused", || Ok::<_, String>(1))
            .unwrap();
        assert_eq!(r1.into_inner(), 1);

        let r2 = map
            .execute_or_join("reused", || Ok::<_, String>(2))
            .unwrap();
        assert_eq!(r2.into_inner(), 2);

        // Both should have been leaders (no overlap).
        let m = map.metrics();
        assert_eq!(m.leader_count, 2);
        assert_eq!(m.joined_count, 0);
    }

    #[test]
    fn error_then_success_same_key() {
        // A failed operation on a key should not prevent a subsequent successful
        // operation on the same key.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        let r1 = map.execute_or_join("retry-key", || {
            Err::<i32, String>("transient failure".into())
        });
        assert!(r1.is_err());

        let r2 = map
            .execute_or_join("retry-key", || Ok::<_, String>(42))
            .unwrap();
        assert_eq!(r2.into_inner(), 42);

        let m = map.metrics();
        assert_eq!(m.leader_count, 2);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn leader_error_metrics_tracked() {
        // When a leader fails and joiners fall back, the leader_failed_count metric
        // should be incremented.
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let barrier = Arc::new(Barrier::new(2));

        // Thread 1: leader that fails.
        let map1 = Arc::clone(&map);
        let barrier1 = Arc::clone(&barrier);
        let h1 = thread::spawn(move || {
            barrier1.wait();
            map1.execute_or_join("fail-metrics-key".to_string(), || {
                thread::sleep(Duration::from_millis(50));
                Err::<i32, String>("boom".into())
            })
        });

        // Thread 2: joiner that will see the leader's failure and fall back.
        let map2 = Arc::clone(&map);
        let barrier2 = Arc::clone(&barrier);
        let h2 = thread::spawn(move || {
            barrier2.wait();
            thread::sleep(Duration::from_millis(5)); // ensure thread 1 becomes leader
            map2.execute_or_join("fail-metrics-key".to_string(), || Ok::<_, String>(77))
        });

        let _r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();

        // Joiner should have succeeded via fallback.
        assert_eq!(r2.unwrap().into_inner(), 77);

        let m = map.metrics();
        // At least the joiner should have recorded a leader_failed event.
        // (Timing-dependent: joiner may or may not have seen the inflight slot.)
        // We check that metrics are consistent: leader_count >= 1.
        assert!(m.leader_count >= 1, "at least one leader expected");
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn zero_capacity_still_works() {
        // Edge case: max_entries=0. Every insert triggers an eviction, but
        // the leader should still complete because it retrieves the slot before
        // any other thread can cause eviction.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(0, Duration::from_millis(100));

        // The eviction code runs on every insert when len >= 0 (always true),
        // but the leader still holds an Arc to its slot so it can complete.
        let r = map
            .execute_or_join("zero-cap", || Ok::<_, String>(7))
            .unwrap();
        assert_eq!(r.into_inner(), 7);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn coalesce_join_error_display() {
        // Verify Display implementations for error types.
        let timeout_err = CoalesceJoinError::Timeout;
        assert_eq!(format!("{timeout_err}"), "coalesce join timed out");

        let leader_err = CoalesceJoinError::LeaderFailed("kaboom".to_string());
        assert_eq!(format!("{leader_err}"), "coalesce leader failed: kaboom");

        // Also verify Error trait is implemented (dyn Error should work).
        let _: &dyn std::error::Error = &CoalesceJoinError::Timeout;
    }

    // ---- Singleflight edge case tests (br-3h13.3.2) ----

    #[test]
    fn joiner_timeout_falls_back_to_independent_execution() {
        // Leader sleeps much longer than the join timeout. Joiners should
        // timeout and execute their own closure independently.
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            100,
            Duration::from_millis(30), // very short join timeout
        ));
        let barrier = Arc::new(Barrier::new(2));

        // Leader: holds the slot for 500ms (far exceeding 30ms timeout).
        let map1 = Arc::clone(&map);
        let barrier1 = Arc::clone(&barrier);
        let leader = thread::spawn(move || {
            barrier1.wait();
            map1.execute_or_join("timeout-key".to_string(), || {
                thread::sleep(Duration::from_millis(500));
                Ok::<_, String>(100)
            })
        });

        // Joiner: arrives shortly after leader, should timeout and fall back.
        let map2 = Arc::clone(&map);
        let barrier2 = Arc::clone(&barrier);
        let joiner = thread::spawn(move || {
            barrier2.wait();
            thread::sleep(Duration::from_millis(5)); // ensure leader starts first
            map2.execute_or_join("timeout-key".to_string(), || Ok::<_, String>(200))
        });

        let r_leader = leader.join().unwrap().unwrap();
        let r_joiner = joiner.join().unwrap().unwrap();

        assert_eq!(r_leader.into_inner(), 100);
        // Joiner timed out and ran its own closure, returning 200.
        assert_eq!(r_joiner.into_inner(), 200);

        let m = map.metrics();
        assert!(
            m.timeout_count >= 1,
            "expected at least 1 timeout, got {}",
            m.timeout_count
        );
        // Both the leader and the timed-out joiner count as leaders in metrics.
        assert!(
            m.leader_count >= 2,
            "expected at least 2 leader executions (leader + fallback), got {}",
            m.leader_count
        );
    }

    #[test]
    fn timeout_metrics_exact_for_single_joiner_fallback() {
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            100,
            Duration::from_millis(20),
        ));
        let barrier = Arc::new(Barrier::new(2));

        let map1 = Arc::clone(&map);
        let barrier1 = Arc::clone(&barrier);
        let leader = thread::spawn(move || {
            barrier1.wait();
            map1.execute_or_join("metric-timeout".to_string(), || {
                thread::sleep(Duration::from_millis(120));
                Ok::<_, String>(10)
            })
        });

        let map2 = Arc::clone(&map);
        let barrier2 = Arc::clone(&barrier);
        let joiner = thread::spawn(move || {
            barrier2.wait();
            thread::sleep(Duration::from_millis(5));
            map2.execute_or_join("metric-timeout".to_string(), || Ok::<_, String>(20))
        });

        let leader_res = leader.join().unwrap().unwrap();
        let joiner_res = joiner.join().unwrap().unwrap();
        assert_eq!(leader_res.into_inner(), 10);
        assert_eq!(joiner_res.into_inner(), 20);

        let metrics = map.metrics();
        assert_eq!(metrics.leader_count, 2);
        assert_eq!(metrics.joined_count, 0);
        assert_eq!(metrics.timeout_count, 1);
        assert_eq!(metrics.leader_failed_count, 0);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn zero_timeout_forces_all_joiners_to_fall_back() {
        // With zero join timeout, no joiner can ever successfully wait for the
        // leader. All threads that detect an in-flight slot immediately timeout
        // and execute independently.
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            100,
            Duration::from_millis(0), // zero timeout
        ));
        let barrier = Arc::new(Barrier::new(4));
        let exec_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                let exec_count = Arc::clone(&exec_count);
                thread::spawn(move || {
                    barrier.wait();
                    map.execute_or_join("zero-to-key".to_string(), || {
                        exec_count.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(30));
                        Ok::<_, String>(42)
                    })
                    .unwrap()
                    .into_inner()
                })
            })
            .collect();

        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(results.iter().all(|&v| v == 42));

        // With zero timeout, most/all joiners should have fallen back.
        let execs = exec_count.load(Ordering::SeqCst);
        assert!(
            execs >= 2,
            "with zero timeout, expected most threads to execute independently, got {execs}"
        );
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn many_concurrent_leaders_same_key_all_complete() {
        // 15 threads race to become leader for the same key. At most one
        // becomes leader; the rest join. All must receive a valid result.
        let n = 15;
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let barrier = Arc::new(Barrier::new(n));
        let exec_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                let exec_count = Arc::clone(&exec_count);
                thread::spawn(move || {
                    barrier.wait();
                    let r = map
                        .execute_or_join("leader-race".to_string(), || {
                            exec_count.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(60));
                            Ok::<_, String>(555)
                        })
                        .unwrap();
                    r.into_inner()
                })
            })
            .collect();

        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.len(), n);
        assert!(results.iter().all(|&v| v == 555));

        // The closure should run far fewer than n times due to coalescing.
        let execs = exec_count.load(Ordering::SeqCst);
        assert!(
            execs < n,
            "expected coalescing to reduce executions below {n}, got {execs}"
        );

        let m = map.metrics();
        // joined + leader_count together should account for all n threads
        // (some might have timed out and retried as leader, but
        // joined_count should be positive with a 5s timeout).
        assert!(
            m.joined_count > 0,
            "with 5s timeout and 60ms work, at least one joiner expected, got 0"
        );
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn max_entries_eviction_during_inflight_concurrent_keys() {
        // max_entries=2 with 6 concurrent distinct keys that all hold their
        // slot in-flight simultaneously. The map evicts entries when capacity
        // is exceeded. Evicted leaders lose their map entry but still hold
        // Arc<Slot>, so their joiners (if any) still get notified. All
        // leaders must still complete successfully.
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            2,
            Duration::from_millis(200),
        ));
        let barrier = Arc::new(Barrier::new(6));

        let handles: Vec<_> = (0..6_i32)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let key = format!("evict-inflight-{i}");
                    map.execute_or_join(key, || {
                        thread::sleep(Duration::from_millis(40));
                        Ok::<_, String>(i)
                    })
                    .unwrap()
                    .into_inner()
                })
            })
            .collect();

        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.len(), 6);
        // Each thread should get its own value (distinct keys, all leaders).
        for (i, &val) in results.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let expected = i as i32;
            assert_eq!(val, expected);
        }
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn evicted_slot_leader_still_broadcasts_to_joiner() {
        // Verify that even if a slot is evicted from the HashMap by a
        // subsequent insert, the leader still holds the Arc<Slot> and can
        // broadcast to any joiner that also holds an Arc.
        //
        // We do this by: (1) starting a slow leader for key "A" with
        // max_entries=1, (2) having a joiner also wait on key "A",
        // (3) starting a separate leader for key "B" which evicts "A"
        // from the map. The joiner for "A" still succeeds because it
        // holds the Arc<Slot>.
        let map = Arc::new(CoalesceMap::<String, i32>::new(1, Duration::from_secs(5)));
        let leader_a_started = Arc::new(Barrier::new(2));

        // Leader for key "A": starts slowly.
        let map_a = Arc::clone(&map);
        let leader_a_started_clone = Arc::clone(&leader_a_started);
        let h_leader_a = thread::spawn(move || {
            map_a
                .execute_or_join("A".to_string(), || {
                    leader_a_started_clone.wait(); // signal we've started
                    thread::sleep(Duration::from_millis(150));
                    Ok::<_, String>(111)
                })
                .unwrap()
                .into_inner()
        });

        // Wait for leader A to begin executing.
        leader_a_started.wait();

        // Joiner for key "A": should find A's slot in the map.
        let map_j = Arc::clone(&map);
        let h_joiner_a = thread::spawn(move || {
            map_j
                .execute_or_join("A".to_string(), || {
                    // Fallback if join fails.
                    Ok::<_, String>(999)
                })
                .unwrap()
                .into_inner()
        });

        // Give joiner time to grab the Arc<Slot> for A.
        thread::sleep(Duration::from_millis(10));

        // Leader for key "B": with max_entries=1, inserting B evicts A from map.
        let map_b = Arc::clone(&map);
        let h_leader_b = thread::spawn(move || {
            map_b
                .execute_or_join("B".to_string(), || Ok::<_, String>(222))
                .unwrap()
                .into_inner()
        });

        let val_a = h_leader_a.join().unwrap();
        let val_joiner = h_joiner_a.join().unwrap();
        let val_b = h_leader_b.join().unwrap();

        assert_eq!(val_a, 111, "leader A should return its own result");
        assert_eq!(val_b, 222, "leader B should return its own result");
        // Joiner should have received leader A's result (111) via the
        // Arc<Slot>, OR fallen back (999) if the slot was evicted before
        // the joiner grabbed the Arc. Both are acceptable outcomes.
        assert!(
            val_joiner == 111 || val_joiner == 999,
            "joiner A got unexpected value: {val_joiner}"
        );
    }

    #[test]
    fn error_propagation_joiner_fallback_also_errors() {
        // When the leader fails AND the joiner's fallback also fails, the
        // joiner should return the fallback error (not the leader's error).
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let barrier = Arc::new(Barrier::new(2));

        let map1 = Arc::clone(&map);
        let barrier1 = Arc::clone(&barrier);
        let h_leader = thread::spawn(move || {
            barrier1.wait();
            map1.execute_or_join("double-fail".to_string(), || {
                thread::sleep(Duration::from_millis(50));
                Err::<i32, String>("leader-error".into())
            })
        });

        let map2 = Arc::clone(&map);
        let barrier2 = Arc::clone(&barrier);
        let h_joiner = thread::spawn(move || {
            barrier2.wait();
            thread::sleep(Duration::from_millis(5));
            map2.execute_or_join("double-fail".to_string(), || {
                // Fallback also fails.
                Err::<i32, String>("joiner-fallback-error".into())
            })
        });

        let r1 = h_leader.join().unwrap();
        let r2 = h_joiner.join().unwrap();

        assert!(r1.is_err());
        assert_eq!(r1.unwrap_err(), "leader-error");

        // The joiner saw the leader fail, then executed its own fallback
        // which also failed. It should return ITS error, not the leader's.
        // (Note: timing may cause the joiner to become a leader itself if
        // it arrives before the leader inserts the slot.)
        assert!(r2.is_err());
        let joiner_err = r2.unwrap_err();
        assert!(
            joiner_err == "joiner-fallback-error" || joiner_err == "leader-error",
            "joiner error should be its own fallback or leader error (if it became leader): {joiner_err}"
        );
    }

    #[test]
    fn reentry_after_eviction_under_concurrency() {
        // After a key is evicted from a small-capacity map, inserting the
        // same key again should work as a fresh leader.
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            2,
            Duration::from_millis(200),
        ));

        // Fill and drain two keys.
        let r1 = map
            .execute_or_join("k1".to_string(), || Ok::<_, String>(1))
            .unwrap();
        let r2 = map
            .execute_or_join("k2".to_string(), || Ok::<_, String>(2))
            .unwrap();
        assert_eq!(r1.into_inner(), 1);
        assert_eq!(r2.into_inner(), 2);
        assert_eq!(map.inflight_count(), 0);

        // Add a third key (capacity is 2, but since prior keys completed,
        // map is empty so no eviction needed).
        let r3 = map
            .execute_or_join("k3".to_string(), || Ok::<_, String>(3))
            .unwrap();
        assert_eq!(r3.into_inner(), 3);

        // Re-use k1 which was previously used.
        let r4 = map
            .execute_or_join("k1".to_string(), || Ok::<_, String>(10))
            .unwrap();
        assert!(!r4.was_joined());
        assert_eq!(r4.into_inner(), 10);

        assert_eq!(map.inflight_count(), 0);
        let m = map.metrics();
        assert_eq!(m.leader_count, 4);
        assert_eq!(m.joined_count, 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn mixed_success_failure_concurrent_same_key() {
        // Leader succeeds. Joiners that successfully join get the leader's
        // result (success). Their own closures are NOT called.
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let barrier = Arc::new(Barrier::new(4));
        let joiner_closure_invoked = Arc::new(AtomicUsize::new(0));

        // Leader succeeds.
        let map0 = Arc::clone(&map);
        let barrier0 = Arc::clone(&barrier);
        let h_leader = thread::spawn(move || {
            barrier0.wait();
            map0.execute_or_join("mix-key".to_string(), || {
                thread::sleep(Duration::from_millis(60));
                Ok::<_, String>(42)
            })
        });

        // Three joiners whose closures would FAIL if called.
        let joiner_handles: Vec<_> = (0..3)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                let joiner_closure_invoked = Arc::clone(&joiner_closure_invoked);
                thread::spawn(move || {
                    barrier.wait();
                    thread::sleep(Duration::from_millis(5));
                    map.execute_or_join("mix-key".to_string(), || {
                        joiner_closure_invoked.fetch_add(1, Ordering::SeqCst);
                        Err::<i32, String>("joiner-would-fail".into())
                    })
                })
            })
            .collect();

        let r_leader = h_leader.join().unwrap();
        assert!(r_leader.is_ok());
        assert_eq!(r_leader.unwrap().into_inner(), 42);

        let mut join_success = 0_usize;
        let mut fallback_fail = 0_usize;
        for h in joiner_handles {
            match h.join().unwrap() {
                Ok(outcome) => {
                    // Joiner successfully shared leader's result.
                    assert_eq!(outcome.into_inner(), 42);
                    join_success += 1;
                }
                Err(e) => {
                    // Joiner's fallback was called and it failed.
                    assert_eq!(e, "joiner-would-fail");
                    fallback_fail += 1;
                }
            }
        }

        // With 5s timeout and 60ms work, most joiners should share leader's result.
        assert!(
            join_success > 0,
            "at least one joiner should share the leader's success"
        );
        // Total should be 3.
        assert_eq!(join_success + fallback_fail, 3);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn multiple_timeouts_each_gets_independent_result() {
        // Leader sleeps 500ms, join timeout is 10ms. Multiple joiners all
        // timeout and each runs their own closure. Each should get a unique
        // result from their closure.
        let map = Arc::new(CoalesceMap::<String, i32>::new(
            100,
            Duration::from_millis(10), // very short timeout
        ));
        let barrier = Arc::new(Barrier::new(5));

        // Leader: very slow.
        let map0 = Arc::clone(&map);
        let barrier0 = Arc::clone(&barrier);
        let h_leader = thread::spawn(move || {
            barrier0.wait();
            map0.execute_or_join("multi-to".to_string(), || {
                thread::sleep(Duration::from_millis(500));
                Ok::<_, String>(0)
            })
            .unwrap()
            .into_inner()
        });

        // 4 joiners: each should timeout and return its own value.
        let handles: Vec<_> = (1..5)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    thread::sleep(Duration::from_millis(5));
                    map.execute_or_join("multi-to".to_string(), || Ok::<_, String>(i))
                        .unwrap()
                        .into_inner()
                })
            })
            .collect();

        let leader_val = h_leader.join().unwrap();
        assert_eq!(leader_val, 0);

        let joiner_vals: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Each joiner should have gotten some value (either 0 from
        // joining or their own i from fallback). All should be valid.
        assert_eq!(joiner_vals.len(), 4);
        for &v in &joiner_vals {
            assert!((0..=4).contains(&v), "unexpected value: {v}");
        }

        let m = map.metrics();
        assert!(
            m.timeout_count >= 1,
            "expected at least 1 timeout with 10ms window and 500ms leader, got {}",
            m.timeout_count
        );
    }

    #[test]
    fn coalesce_join_error_implements_std_error() {
        // Verify that CoalesceJoinError satisfies the std::error::Error bound.
        let err_timeout: Box<dyn std::error::Error> = Box::new(CoalesceJoinError::Timeout);
        assert!(err_timeout.source().is_none());
        assert_eq!(err_timeout.to_string(), "coalesce join timed out");

        let err_leader: Box<dyn std::error::Error> =
            Box::new(CoalesceJoinError::LeaderFailed("oom".to_string()));
        assert!(err_leader.source().is_none());
        assert_eq!(err_leader.to_string(), "coalesce leader failed: oom");
    }

    #[test]
    fn coalesce_join_error_debug_format() {
        // Verify Debug output is useful.
        let err = CoalesceJoinError::Timeout;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Timeout"), "Debug should show variant: {dbg}");

        let err = CoalesceJoinError::LeaderFailed("crash".to_string());
        let dbg = format!("{err:?}");
        assert!(
            dbg.contains("LeaderFailed"),
            "Debug should show variant: {dbg}"
        );
        assert!(dbg.contains("crash"), "Debug should show message: {dbg}");
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn large_burst_20_threads_coalescing() {
        // 20 threads on the same key. High contention stress test.
        let n = 20;
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let barrier = Arc::new(Barrier::new(n));
        let exec_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                let exec_count = Arc::clone(&exec_count);
                thread::spawn(move || {
                    barrier.wait();
                    let r = map
                        .execute_or_join("burst-20".to_string(), || {
                            exec_count.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(80));
                            Ok::<_, String>(999)
                        })
                        .unwrap();
                    r.into_inner()
                })
            })
            .collect();

        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(results.iter().all(|&v| v == 999));
        assert_eq!(results.len(), n);

        let execs = exec_count.load(Ordering::SeqCst);
        assert!(
            execs < n,
            "coalescing should reduce execs below {n}, got {execs}"
        );

        let m = map.metrics();
        assert!(m.joined_count >= 1, "expected joins with 5s timeout");
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn sequential_rapid_fire_same_key_no_stale_state() {
        // Rapidly execute the same key 50 times sequentially. Each
        // execution should be a fresh leader (no stale slot lingering).
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        for i in 0..50 {
            let r = map.execute_or_join("rapid", || Ok::<_, String>(i)).unwrap();
            assert!(!r.was_joined());
            assert_eq!(r.into_inner(), i);
            assert_eq!(map.inflight_count(), 0);
        }

        let m = map.metrics();
        assert_eq!(m.leader_count, 50);
        assert_eq!(m.joined_count, 0);
    }

    #[test]
    fn alternating_success_failure_sequence() {
        // Alternate success/failure on the same key to verify no state
        // leaks between executions.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        for i in 0..10 {
            if i % 2 == 0 {
                let r = map
                    .execute_or_join("alt-key", || Ok::<_, String>(i))
                    .unwrap();
                assert_eq!(r.into_inner(), i);
            } else {
                let r = map.execute_or_join("alt-key", || Err::<i32, String>(format!("fail-{i}")));
                assert!(r.is_err());
                assert_eq!(r.unwrap_err(), format!("fail-{i}"));
            }
            assert_eq!(map.inflight_count(), 0);
        }

        let m = map.metrics();
        assert_eq!(m.leader_count, 10);
    }

    #[test]
    fn coalesce_metrics_default_is_zero() {
        let m = CoalesceMetrics::default();
        assert_eq!(m.leader_count, 0);
        assert_eq!(m.joined_count, 0);
        assert_eq!(m.timeout_count, 0);
        assert_eq!(m.leader_failed_count, 0);
    }

    #[test]
    fn complex_value_type_clones_correctly() {
        // Verify coalescing works with a complex Clone type.
        #[derive(Clone, Debug, PartialEq)]
        struct ComplexResult {
            ids: Vec<i64>,
            label: String,
        }

        let map: CoalesceMap<&str, ComplexResult> =
            CoalesceMap::new(100, Duration::from_millis(100));

        let expected = ComplexResult {
            ids: vec![1, 2, 3, 4, 5],
            label: "test-result".to_string(),
        };

        let r = map
            .execute_or_join("complex", || Ok::<_, String>(expected.clone()))
            .unwrap();
        assert_eq!(r.into_inner(), expected);
    }

    // ─── Sharded coalescer tests (br-2fttz) ────────────────────────────────────

    #[test]
    fn sharded_single_thread_executes() {
        // Basic smoke test: single-threaded execution uses the shard correctly.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));
        let result = map
            .execute_or_join("shard-key-1", || Ok::<_, String>(42))
            .unwrap();
        assert!(!result.was_joined());
        assert_eq!(result.into_inner(), 42);
        assert_eq!(map.inflight_count(), 0);

        let m = map.metrics();
        assert_eq!(m.leader_count, 1);
        assert_eq!(m.joined_count, 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn sharded_joiners_receive_result() {
        // Multiple threads on the same key: joiners should share the leader's result.
        let map = Arc::new(CoalesceMap::<String, i32>::new(100, Duration::from_secs(5)));
        let exec_count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(5));

        let handles: Vec<_> = (0..5)
            .map(|_| {
                let map = Arc::clone(&map);
                let exec_count = Arc::clone(&exec_count);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let result = map
                        .execute_or_join("shard-shared".to_string(), || {
                            exec_count.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(50));
                            Ok::<_, String>(99)
                        })
                        .unwrap();
                    result.into_inner()
                })
            })
            .collect();

        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(results.iter().all(|&v| v == 99));

        let actual_execs = exec_count.load(Ordering::SeqCst);
        assert!(
            actual_execs < 5,
            "expected fewer than 5 executions, got {actual_execs}"
        );

        let m = map.metrics();
        assert!(m.joined_count > 0, "at least one thread should have joined");
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    #[allow(clippy::needless_collect)]
    fn sharded_different_keys_no_contention() {
        // Keys that hash to different shards should execute independently
        // without contention.
        let map = Arc::new(CoalesceMap::<String, usize>::new(
            100,
            Duration::from_millis(100),
        ));
        let exec_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let map = Arc::clone(&map);
                let exec_count = Arc::clone(&exec_count);
                thread::spawn(move || {
                    let key = format!("independent-shard-key-{i}");
                    let result = map
                        .execute_or_join(key, || {
                            exec_count.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(i)
                        })
                        .unwrap();
                    result.into_inner()
                })
            })
            .collect();

        let results: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.len(), 16);

        // All 16 distinct keys should have executed independently.
        assert_eq!(exec_count.load(Ordering::SeqCst), 16);
        assert_eq!(map.inflight_count(), 0);
    }

    #[test]
    fn sharded_hash_distribution() {
        // Verify that hashing distributes keys across multiple shards.
        // With 100 distinct keys, we expect most of the 16 shards to be hit.
        let mut shard_hits = [0u32; NUM_SHARDS];
        for i in 0..100 {
            let key = format!("distribution-test-key-{i}");
            let idx = ShardedCoalesceMap::<String, ()>::shard_index(&key);
            shard_hits[idx] += 1;
        }

        let shards_used = shard_hits.iter().filter(|&&c| c > 0).count();
        assert!(
            shards_used >= 10,
            "expected at least 10 of 16 shards used, got {shards_used} (distribution: {shard_hits:?})"
        );

        // No single shard should have more than 25% of keys (extreme imbalance).
        let max_per_shard = *shard_hits.iter().max().unwrap();
        assert!(
            max_per_shard <= 25,
            "expected no shard with >25 of 100 keys, got {max_per_shard} (distribution: {shard_hits:?})"
        );
    }

    #[test]
    fn sharded_inflight_count_accurate() {
        // Verify `inflight_count()` correctly sums across all shards.
        let map: CoalesceMap<&str, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        // After a completed operation, inflight count is 0.
        let _ = map.execute_or_join("a", || Ok::<_, String>(1));
        let _ = map.execute_or_join("b", || Ok::<_, String>(2));
        let _ = map.execute_or_join("c", || Ok::<_, String>(3));
        assert_eq!(map.inflight_count(), 0);

        // Metrics should reflect 3 leader executions.
        let m = map.metrics();
        assert_eq!(m.leader_count, 3);
    }

    #[test]
    fn sharded_metrics_consistent() {
        // Run a batch of operations and verify metrics are internally consistent.
        let map: CoalesceMap<String, i32> = CoalesceMap::new(100, Duration::from_millis(100));

        for i in 0..50 {
            let _ = map.execute_or_join(format!("metric-key-{i}"), || Ok::<_, String>(i));
        }

        let m = map.metrics();
        // All sequential single-threaded calls should be leaders.
        assert_eq!(m.leader_count, 50);
        assert_eq!(m.joined_count, 0);
        assert_eq!(m.timeout_count, 0);
        assert_eq!(m.leader_failed_count, 0);
        assert_eq!(map.inflight_count(), 0);

        // Reset and verify.
        map.reset_metrics();
        let m = map.metrics();
        assert_eq!(m.leader_count, 0);
        assert_eq!(m.joined_count, 0);

        // Map still works after reset.
        let r = map
            .execute_or_join("post-reset".to_string(), || Ok::<_, String>(7))
            .unwrap();
        assert_eq!(r.into_inner(), 7);
        assert_eq!(map.metrics().leader_count, 1);
    }

    // ─── Property tests ───────────────────────────────────────────────────────

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    mod proptest_coalesce {
        use super::*;
        use proptest::prelude::*;

        fn pt_config() -> ProptestConfig {
            ProptestConfig {
                cases: 500, // lower for thread-based tests
                max_shrink_iters: 2000,
                ..ProptestConfig::default()
            }
        }

        proptest! {
            #![proptest_config(pt_config())]

            /// All N callers with the same key get a valid result.
            #[test]
            #[allow(clippy::needless_collect)]
            fn prop_coalesce_all_callers_get_result(n in 2..=8usize) {
                let map = Arc::new(CoalesceMap::<String, i32>::new(
                    100,
                    Duration::from_secs(5),
                ));
                let barrier = Arc::new(Barrier::new(n));

                let handles: Vec<_> = (0..n)
                    .map(|_| {
                        let map = Arc::clone(&map);
                        let barrier = Arc::clone(&barrier);
                        thread::spawn(move || {
                            barrier.wait();
                            map.execute_or_join("shared".to_string(), || {
                                thread::sleep(Duration::from_millis(20));
                                Ok::<_, String>(99)
                            })
                        })
                    })
                    .collect();

                for h in handles {
                    let r = h.join().unwrap();
                    prop_assert!(r.is_ok(), "all callers must get Ok");
                    prop_assert_eq!(r.unwrap().into_inner(), 99);
                }
            }

            /// After all threads complete, inflight is zero.
            #[test]
            fn prop_coalesce_inflight_zero_after_completion(n in 1..=10usize) {
                let map = Arc::new(CoalesceMap::<String, i32>::new(
                    100,
                    Duration::from_secs(5),
                ));
                let barrier = Arc::new(Barrier::new(n));

                let handles: Vec<_> = (0..n)
                    .map(|i| {
                        let map = Arc::clone(&map);
                        let barrier = Arc::clone(&barrier);
                        thread::spawn(move || {
                            barrier.wait();
                            let _ = map.execute_or_join(
                                format!("k-{}", i % 3),
                                || Ok::<_, String>(i as i32),
                            );
                        })
                    })
                    .collect();

                for h in handles {
                    h.join().unwrap();
                }
                prop_assert_eq!(
                    map.inflight_count(),
                    0,
                    "inflight must be 0 after all threads complete"
                );
            }

            /// leader + joined + timeout >= total calls.
            #[test]
            fn prop_coalesce_metrics_sum_consistent(n in 1..=20usize) {
                let map: CoalesceMap<&str, i32> =
                    CoalesceMap::new(100, Duration::from_millis(100));
                for i in 0..n {
                    let _ = map.execute_or_join("seq", || Ok::<_, String>(i as i32));
                }
                let m = map.metrics();
                let sum = m.leader_count + m.joined_count + m.timeout_count;
                prop_assert!(
                    sum >= n as u64,
                    "metrics sum {sum} < total calls {n}"
                );
            }

            /// N distinct keys each produce a leader execution.
            #[test]
            fn prop_coalesce_different_keys_independent(n in 1..=20usize) {
                let map: CoalesceMap<String, i32> =
                    CoalesceMap::new(100, Duration::from_millis(100));
                for i in 0..n {
                    let r = map
                        .execute_or_join(format!("unique-{i}"), || Ok::<_, String>(i as i32))
                        .unwrap();
                    prop_assert!(!r.was_joined(), "distinct key should not join");
                }
                let m = map.metrics();
                prop_assert!(
                    m.leader_count >= n as u64,
                    "expected {} leaders, got {}",
                    n,
                    m.leader_count
                );
            }
        }
    }
}
