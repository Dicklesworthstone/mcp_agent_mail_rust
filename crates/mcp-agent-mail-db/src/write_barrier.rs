//! In-process mutual exclusion between live write activity and database
//! recovery promotion (mcp_agent_mail_rust#219).
//!
//! `promote_recovery_candidate` replaces the live SQLite file by rename while
//! the owning `serve-http` process keeps dispatching MCP write tools. The
//! cross-process ownership guard (`refuse_mutating_mailbox_when_owned`)
//! deliberately excludes the current PID, so nothing stopped the server from
//! swapping the database underneath its own in-flight writes. Writes that
//! straddled a promotion resolved numeric project/agent ids against one file
//! generation and landed rows in another, minting cross-generation orphans
//! (`agent_link references unmapped origin agent N`) and — via roll-forward /
//! roll-back sequences — physically diverged b-trees.
//!
//! This module provides the missing in-process lease:
//!
//! - Every MCP write tool call holds a [`WriteActivityGuard`] for its full
//!   duration (bracketed by the tools crate's `WriteGuard`).
//! - Recovery paths bracket build+promotion in a [`PromotionBarrierGuard`]:
//!   * **Archive-drift reconciles** (healthy DB, archive ahead) acquire via
//!     [`try_acquire_promotion_barrier_if_idle`] and simply *defer* when any
//!     writer is active — a drift catch-up is an optimization, never worth
//!     racing a live write. Holding the barrier across the candidate build
//!     also freezes the archive (writers are what advance it), so the
//!     promoted database is exactly current and the drift predicate cannot
//!     immediately re-arm (the "3 reconstructions in 40 s" loop).
//!   * **Corruption recovery** acquires via
//!     [`acquire_promotion_barrier_draining`], which blocks new writers,
//!     and waits a bounded time for in-flight writers to drain. A timeout is a
//!     fail-closed outcome: callers must release the barrier and defer recovery
//!     without renaming any member of the live SQLite generation.
//!
//! The promotion barrier is reentrant per thread (recovery code that re-enters
//! helper paths must not deadlock on itself). Write activity has no per-thread
//! exemption: async tool futures are `Send` and may migrate between executor
//! threads. Cold pool bootstrap therefore uses an explicit promotion-barrier
//! to writer handoff before returning the live pool.
//!
//! The barrier is process-global rather than per-path: promotions are rare,
//! and a global gate keeps the lock graph trivial.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// How long a blocked writer waits between "promotion still active" warnings.
const WRITER_WAIT_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// Default bound on how long corruption recovery waits for in-flight writers
/// to drain before failing closed and deferring recovery.
const DEFAULT_WRITER_DRAIN_TIMEOUT_SECS: u64 = 10;

/// Default minimum interval between archive-drift reconciles of the same
/// path after a durable promotion. Belt-and-suspenders against any residual
/// re-trigger loop: even a converged promotion re-runs the drift predicate on
/// the next pool bootstrap, and if anything (clock skew, a lagging archive
/// scanner) still reports drift, this floor keeps the mailbox from thrashing.
const DEFAULT_ARCHIVE_RECONCILE_MIN_INTERVAL_SECS: u64 = 60;

fn parse_positive_u64(raw: Option<String>, default: u64) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Bounded writer-drain wait for corruption recovery.
///
/// `AM_PROMOTION_WRITER_DRAIN_TIMEOUT_SECS`, default 10; zero/garbage fall
/// back to the default so an override can never disable draining entirely.
#[must_use]
pub fn writer_drain_timeout() -> Duration {
    Duration::from_secs(parse_positive_u64(
        std::env::var("AM_PROMOTION_WRITER_DRAIN_TIMEOUT_SECS").ok(),
        DEFAULT_WRITER_DRAIN_TIMEOUT_SECS,
    ))
}

/// Minimum spacing between archive-drift reconciles per path.
///
/// `AM_ARCHIVE_RECONCILE_MIN_INTERVAL_SECS`, default 60. Unlike the
/// recovery breaker's thresholds, an explicit `0` here is honored and
/// disables the cooldown — it is a pacing knob, not a safety floor.
#[must_use]
pub fn archive_reconcile_min_interval() -> Duration {
    std::env::var("AM_ARCHIVE_RECONCILE_MIN_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(
            Duration::from_secs(DEFAULT_ARCHIVE_RECONCILE_MIN_INTERVAL_SECS),
            Duration::from_secs,
        )
}

struct BarrierState {
    writers: usize,
    promotion_active: bool,
}

struct Barrier {
    state: Mutex<BarrierState>,
    /// Signalled when `writers` decreases (drain progress).
    writers_drained: Condvar,
    /// Signalled when `promotion_active` clears.
    promotion_released: Condvar,
}

static BARRIER: OnceLock<Barrier> = OnceLock::new();

fn barrier() -> &'static Barrier {
    BARRIER.get_or_init(|| Barrier {
        state: Mutex::new(BarrierState {
            writers: 0,
            promotion_active: false,
        }),
        writers_drained: Condvar::new(),
        promotion_released: Condvar::new(),
    })
}

thread_local! {
    /// Whether the current thread already holds the promotion barrier.
    static THREAD_BARRIER_DEPTH: Cell<usize> = const { Cell::new(0) };
}

fn current_thread_holds_barrier() -> bool {
    THREAD_BARRIER_DEPTH.with(Cell::get) > 0
}

/// Whether the calling thread is already inside a promotion barrier.
///
/// True while executing as part of an ongoing recovery operation. Pacing
/// gates (cooldowns, idle checks) apply only to standalone drift reconciles,
/// never to steps nested inside a recovery that already owns the barrier.
#[must_use]
pub fn current_thread_holds_promotion_barrier() -> bool {
    current_thread_holds_barrier()
}

/// RAII lease marking one in-flight write-path operation.
///
/// Acquisition blocks while a promotion holds the barrier (warning once
/// after five seconds). The stall is bounded by the promotion itself, and a
/// stalled tool call is strictly better than a write landing across a file
/// swap.
///
/// This guard is safely `Send`: its ownership is process-global and dropping
/// it on a different executor thread decrements the same global count. No
/// thread-local writer exemption is used.
#[must_use = "write activity ends when this guard drops"]
pub struct WriteActivityGuard {
    _priv: (),
}

/// Begin one unit of write activity.
pub fn begin_write_activity() -> WriteActivityGuard {
    let b = barrier();
    let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    // A thread already inside the promotion barrier is the recovery path
    // itself re-entering a helper; it must not block on its own barrier.
    if !current_thread_holds_barrier() {
        let wait_started = Instant::now();
        let mut warned = false;
        while state.promotion_active {
            let (next, _timeout) = b
                .promotion_released
                .wait_timeout(state, WRITER_WAIT_WARN_INTERVAL)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if state.promotion_active
                && !warned
                && wait_started.elapsed() >= WRITER_WAIT_WARN_INTERVAL
            {
                warned = true;
                tracing::warn!(
                    waited_ms =
                        u64::try_from(wait_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "write path is waiting on an in-progress database recovery promotion"
                );
            }
        }
        if warned {
            tracing::info!(
                waited_ms = u64::try_from(wait_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "write path resumed after recovery promotion released the barrier"
            );
        }
    }
    state.writers = state.writers.saturating_add(1);
    drop(state);
    WriteActivityGuard { _priv: () }
}

impl Drop for WriteActivityGuard {
    fn drop(&mut self) {
        let b = barrier();
        let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.writers = state.writers.saturating_sub(1);
        drop(state);
        b.writers_drained.notify_all();
    }
}

/// Number of write-path operations currently in flight in this process.
#[must_use]
pub fn active_writer_count() -> usize {
    let b = barrier();
    let state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    state.writers
}

/// Outcome of a draining barrier acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// No writers were active at acquisition.
    Idle,
    /// Foreign writers drained within the timeout.
    Drained { waited: Duration },
    /// The timeout elapsed with writers still in flight. The barrier is still
    /// held when this outcome is returned so the caller can observe a stable
    /// count, but the caller must release it and abort before any mutation.
    TimedOut { remaining_writers: usize },
}

/// RAII promotion barrier.
///
/// While held, new write activity blocks in [`begin_write_activity`].
/// Reentrant per thread: nested acquisitions are passthrough and only the
/// outermost release re-opens the write path.
#[must_use = "the promotion barrier releases when this guard drops"]
pub struct PromotionBarrierGuard {
    passthrough: bool,
    // Barrier nesting is intentionally thread-local and all promotion work is
    // synchronous. Prevent an executor or caller from moving this guard and
    // corrupting the depth/release contract.
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl PromotionBarrierGuard {
    fn new_held() -> Self {
        THREAD_BARRIER_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self {
            passthrough: false,
            _not_send: std::marker::PhantomData,
        }
    }

    fn new_passthrough() -> Self {
        THREAD_BARRIER_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self {
            passthrough: true,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for PromotionBarrierGuard {
    fn drop(&mut self) {
        THREAD_BARRIER_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        if self.passthrough {
            return;
        }
        debug_assert_eq!(
            THREAD_BARRIER_DEPTH.with(Cell::get),
            0,
            "outer promotion barrier dropped while a passthrough guard is still alive; \
             barrier guards must be strictly LIFO-scoped"
        );
        let b = barrier();
        let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.promotion_active = false;
        drop(state);
        // Wake blocked writers and any queued promotion.
        b.writers_drained.notify_all();
        b.promotion_released.notify_all();
    }
}

/// Acquire the promotion barrier only if the process is write-idle.
///
/// Returns `None` when another promotion is active or when any write
/// is in flight — the caller (an archive-drift reconcile) should defer and
/// let the admission machinery retry later.
#[must_use]
pub fn try_acquire_promotion_barrier_if_idle() -> Option<PromotionBarrierGuard> {
    if current_thread_holds_barrier() {
        return Some(PromotionBarrierGuard::new_passthrough());
    }
    let b = barrier();
    let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    if state.promotion_active {
        return None;
    }
    if state.writers > 0 {
        return None;
    }
    state.promotion_active = true;
    drop(state);
    Some(PromotionBarrierGuard::new_held())
}

/// Acquire the promotion barrier for corruption recovery: block new writers
/// immediately, wait up to `timeout` for every in-flight writer to drain,
/// then return [`DrainOutcome::TimedOut`] if any remain. A timed-out guard must
/// only be used to fail closed; it does not authorize promotion.
pub fn acquire_promotion_barrier_draining(
    timeout: Duration,
) -> (PromotionBarrierGuard, DrainOutcome) {
    if current_thread_holds_barrier() {
        return (PromotionBarrierGuard::new_passthrough(), DrainOutcome::Idle);
    }
    let b = barrier();
    let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    // Serialize with any promotion already in flight.
    while state.promotion_active {
        let (next, _timeout) = b
            .promotion_released
            .wait_timeout(state, WRITER_WAIT_WARN_INTERVAL)
            .unwrap_or_else(PoisonError::into_inner);
        state = next;
    }
    state.promotion_active = true;

    let started = Instant::now();
    let outcome = if state.writers == 0 {
        DrainOutcome::Idle
    } else {
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break DrainOutcome::TimedOut {
                    remaining_writers: state.writers,
                };
            }
            let (next, _timeout) = b
                .writers_drained
                .wait_timeout(state, remaining.min(WRITER_WAIT_WARN_INTERVAL))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if state.writers == 0 {
                break DrainOutcome::Drained {
                    waited: started.elapsed(),
                };
            }
        }
    };
    drop(state);
    (PromotionBarrierGuard::new_held(), outcome)
}

// ---------------------------------------------------------------------------
// Promotion recency bookkeeping (archive-drift reconcile cooldown)
// ---------------------------------------------------------------------------

static LAST_PROMOTIONS: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();

/// Cap on retained promotion-recency entries; oldest are evicted first.
/// Promotions are rare and roots are few — this only bounds pathological
/// many-mailbox processes.
const MAX_PROMOTION_RECENCY_ENTRIES: usize = 64;

/// Monotonic count of durable promotions in this process. Long-lived raw
/// connections (e.g. the tool-metrics worker) compare this against a cached
/// value to detect that their fd now points at a quarantined generation and
/// must be reopened (mcp_agent_mail_rust#219).
static PROMOTION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Current process-wide promotion epoch.
#[must_use]
pub fn promotion_epoch() -> u64 {
    PROMOTION_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

fn last_promotions() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    LAST_PROMOTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that `primary_path` just went through a durable promotion.
pub fn record_promotion(primary_path: &Path) {
    PROMOTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut guard = last_promotions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    guard.insert(primary_path.to_path_buf(), Instant::now());
    if guard.len() > MAX_PROMOTION_RECENCY_ENTRIES {
        let mut entries: Vec<(PathBuf, Instant)> =
            guard.iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort_by_key(|(_, at)| *at);
        for (stale, _) in entries
            .iter()
            .take(guard.len() - MAX_PROMOTION_RECENCY_ENTRIES)
        {
            guard.remove(stale);
        }
    }
}

/// Time since the last durable promotion of `primary_path`, if any.
#[must_use]
pub fn time_since_last_promotion(primary_path: &Path) -> Option<Duration> {
    let guard = last_promotions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    guard.get(primary_path).map(Instant::elapsed)
}

/// Test-only: clear the promotion-recency map WITHOUT touching live barrier
/// state. Safe to call from tests that run in parallel with other tests
/// holding write-activity or promotion guards.
#[cfg(test)]
pub(crate) fn clear_promotion_recency_for_test() {
    if let Some(map) = LAST_PROMOTIONS.get() {
        map.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    if let Some(barrier) = BARRIER.get() {
        let mut state = barrier.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.writers = 0;
        state.promotion_active = false;
    }
    if let Some(map) = LAST_PROMOTIONS.get() {
        map.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // The barrier is process-global, so these tests serialize on a local
    // mutex to avoid cross-test interference within this module.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn drift_acquire_defers_while_foreign_writer_active() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let writer = std::thread::spawn(|| {
            let guard = begin_write_activity();
            std::thread::sleep(Duration::from_millis(200));
            drop(guard);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            try_acquire_promotion_barrier_if_idle().is_none(),
            "drift reconcile must defer while a foreign writer is in flight"
        );
        writer.join().expect("writer thread");
        let barrier = try_acquire_promotion_barrier_if_idle();
        assert!(barrier.is_some(), "idle process must grant the barrier");
    }

    #[test]
    fn write_activity_blocks_idle_promotion_even_on_same_thread() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let _activity = begin_write_activity();
        let barrier = try_acquire_promotion_barrier_if_idle();
        assert!(
            barrier.is_none(),
            "every live writer must block promotion; cold bootstrap performs an explicit barrier-to-writer handoff"
        );
    }

    #[test]
    fn migrated_write_guard_cannot_leave_a_false_self_exemption() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let migrated = begin_write_activity();
        std::thread::spawn(move || drop(migrated))
            .join()
            .expect("drop migrated writer");

        let _fresh = begin_write_activity();
        assert!(
            try_acquire_promotion_barrier_if_idle().is_none(),
            "dropping a Send guard on another thread must not leave ghost ownership that hides a real writer"
        );
    }

    #[test]
    fn new_writers_block_until_promotion_releases() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let barrier = try_acquire_promotion_barrier_if_idle().expect("idle acquire");
        let entered = Arc::new(AtomicBool::new(false));
        let entered_clone = Arc::clone(&entered);
        let writer = std::thread::spawn(move || {
            let guard = begin_write_activity();
            entered_clone.store(true, Ordering::SeqCst);
            drop(guard);
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !entered.load(Ordering::SeqCst),
            "writer must stay parked while the barrier is held"
        );
        drop(barrier);
        writer.join().expect("writer thread");
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn draining_acquire_times_out_but_holds_barrier() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let writer_started = Arc::new(AtomicBool::new(false));
        let started_clone = Arc::clone(&writer_started);
        let writer = std::thread::spawn(move || {
            let guard = begin_write_activity();
            started_clone.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            drop(guard);
        });
        while !writer_started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let (barrier, outcome) = acquire_promotion_barrier_draining(Duration::from_millis(50));
        assert!(
            matches!(
                outcome,
                DrainOutcome::TimedOut {
                    remaining_writers: 1
                }
            ),
            "expected timeout with one straggler, got {outcome:?}"
        );
        drop(barrier);
        writer.join().expect("writer thread");
    }

    #[test]
    fn draining_acquire_observes_drain() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let writer = std::thread::spawn(|| {
            let guard = begin_write_activity();
            std::thread::sleep(Duration::from_millis(100));
            drop(guard);
        });
        std::thread::sleep(Duration::from_millis(30));
        let (barrier, outcome) = acquire_promotion_barrier_draining(Duration::from_secs(5));
        assert!(
            matches!(outcome, DrainOutcome::Drained { .. } | DrainOutcome::Idle),
            "expected drain before timeout, got {outcome:?}"
        );
        drop(barrier);
        writer.join().expect("writer thread");
    }

    #[test]
    fn nested_barrier_is_passthrough() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let outer = try_acquire_promotion_barrier_if_idle().expect("outer");
        let (inner, outcome) = acquire_promotion_barrier_draining(Duration::from_millis(10));
        assert_eq!(outcome, DrainOutcome::Idle);
        drop(inner);
        // Barrier must still be held by the outer guard.
        assert!(
            active_writer_count() == 0,
            "sanity: no writers involved in this test"
        );
        let b = barrier();
        {
            let promotion_active = b
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .promotion_active;
            assert!(
                promotion_active,
                "inner passthrough drop must not release the outer barrier"
            );
        }
        drop(outer);
        let promotion_active = b
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .promotion_active;
        assert!(!promotion_active, "outer drop must release");
    }

    #[test]
    fn promotion_recency_is_recorded() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_test();
        let path = Path::new("/tmp/write-barrier-test.sqlite3");
        assert!(time_since_last_promotion(path).is_none());
        record_promotion(path);
        let age = time_since_last_promotion(path).expect("recorded");
        assert!(age < Duration::from_secs(5));
    }
}
