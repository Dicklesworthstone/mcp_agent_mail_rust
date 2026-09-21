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
//!     [`acquire_promotion_barrier_draining`], which bounds both waiting for
//!     another promotion and draining in-flight writers with one timeout.
//!     Once acquired, it blocks new writers. A timeout is a fail-closed
//!     outcome: callers must drop the guard and defer recovery without
//!     renaming any member of the live SQLite generation.
//!
//! The promotion barrier is reentrant per thread (recovery code that re-enters
//! helper paths must not deadlock on itself). Exclusion ownership is distinct
//! from a successful drain: nested helpers cannot turn a timed-out guard into
//! permission to promote, even if its last writer subsequently finishes.
//! Write activity has no per-thread exemption from the writer count: async
//! tool futures are `Send` and may migrate between executor threads. Cold pool
//! bootstrap uses an explicit promotion-barrier to writer handoff; while that
//! writer is counted, nested promotion is unavailable.
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

/// Default bound on how long corruption recovery waits for promotion ownership
/// and for in-flight writers to drain before failing closed and deferring.
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

/// Bounded promotion-owner and writer-drain wait for corruption recovery.
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
    /// The owning attempt observed idleness or completed its drain in budget.
    /// A timeout leaves this false until the entire failed ownership ends.
    promotion_ready: bool,
}

impl BarrierState {
    const fn can_promote(&self) -> bool {
        self.promotion_active && self.promotion_ready && self.writers == 0
    }
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
            promotion_ready: false,
        }),
        writers_drained: Condvar::new(),
        promotion_released: Condvar::new(),
    })
}

thread_local! {
    /// Exclusion ownership, including a timed-out attempt awaiting drop.
    static THREAD_BARRIER_DEPTH: Cell<usize> = const { Cell::new(0) };
}

fn current_thread_holds_barrier() -> bool {
    THREAD_BARRIER_DEPTH.with(Cell::get) > 0
}

/// Whether this thread owns a successfully drained, currently write-idle gate.
///
/// Nested recovery may bypass standalone pacing only with this entitlement.
/// A timed-out guard still excludes new writers but returns false here, even
/// after a late writer drains. Drop that attempt before trying again. A counted
/// promotion-to-writer handoff also returns false until its writer is dropped.
#[must_use]
pub fn current_thread_holds_promotion_barrier() -> bool {
    if !current_thread_holds_barrier() {
        return false;
    }
    let state = barrier()
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    state.can_promote()
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
    begin_write_activity_with(
        WRITER_WAIT_WARN_INTERVAL,
        |waited| {
            tracing::warn!(
                waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                "write path is waiting on an in-progress database recovery promotion"
            );
        },
        |waited| {
            tracing::info!(
                waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                "write path resumed after recovery promotion released the barrier"
            );
        },
    )
}

/// Keep diagnostic callbacks outside the process-global admission lock.
/// Subscribers may inspect barrier metrics or unwind. Neither may strand the
/// mutex, leak a counted writer, or skip a successor promotion's exclusion.
fn begin_write_activity_with(
    warning_interval: Duration,
    mut on_wait: impl FnMut(Duration),
    on_resume: impl FnOnce(Duration),
) -> WriteActivityGuard {
    let b = barrier();
    let wait_started = Instant::now();
    let mut warned = false;
    let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    // Raw exclusion ownership avoids self-deadlock during the explicit
    // recovery-to-writer handoff. It does not exempt this writer from counting.
    if !current_thread_holds_barrier() {
        while state.promotion_active {
            let (next, _timeout) = b
                .promotion_released
                .wait_timeout(state, warning_interval)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if state.promotion_active && !warned && wait_started.elapsed() >= warning_interval {
                warned = true;
                drop(state);
                on_wait(wait_started.elapsed());
                state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
                // A different promotion may have acquired while diagnostics
                // ran. Recheck the predicate under the lock before admission.
            }
        }
    }
    state.writers = state.writers.saturating_add(1);
    let guard = WriteActivityGuard { _priv: () };
    drop(state);
    if warned {
        // The RAII guard already owns this count if the subscriber panics.
        on_resume(wait_started.elapsed());
    }
    guard
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
    /// Foreign writers drained within the shared acquisition timeout.
    Drained { waited: Duration },
    /// The budget expired or nested acquisition lacked a successful drain.
    /// The returned guard retains ownership only if this call acquired it;
    /// a queued or refused nested acquisition returns an inert guard.
    /// `remaining_writers` is diagnostic and may be zero after a late drain
    /// or when another promotion consumed the budget. None of these cases
    /// authorizes mutation: drop the guard and defer recovery.
    TimedOut { remaining_writers: usize },
}

/// RAII promotion barrier.
///
/// While held, new write activity blocks in [`begin_write_activity`].
/// Reentrant per thread: each acquired guard contributes a lease and only
/// the last release re-opens the write path, regardless of drop order.
/// Exclusion alone is not promotion permission: callers must check the drain
/// outcome. A refused queued or nested acquisition owns nothing.
#[must_use = "the promotion barrier releases when this guard drops"]
pub struct PromotionBarrierGuard {
    participating: bool,
    // Barrier nesting is intentionally thread-local and all promotion work is
    // synchronous. Prevent an executor or caller from moving this guard and
    // corrupting the depth/release contract.
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl PromotionBarrierGuard {
    fn new_held() -> Self {
        THREAD_BARRIER_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self {
            participating: true,
            _not_send: std::marker::PhantomData,
        }
    }

    fn new_unheld() -> Self {
        // In particular, do not grant the recovery-thread writer exemption.
        Self {
            participating: false,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for PromotionBarrierGuard {
    fn drop(&mut self) {
        if !self.participating {
            return;
        }
        let last_lease = THREAD_BARRIER_DEPTH.with(|depth| {
            let remaining = depth.get().saturating_sub(1);
            depth.set(remaining);
            remaining == 0
        });
        if !last_lease {
            return;
        }
        let b = barrier();
        let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.promotion_ready = false;
        state.promotion_active = false;
        drop(state);
        // Wake blocked writers and any queued promotion.
        b.writers_drained.notify_all();
        b.promotion_released.notify_all();
    }
}

/// Acquire the promotion barrier only if the process is write-idle.
///
/// Returns `None` when another promotion is active, any write is in flight,
/// or this thread still owns an unsuccessful drain. Nesting cannot promote
/// that unsuccessful attempt into permission; drop it before trying again.
#[must_use]
pub fn try_acquire_promotion_barrier_if_idle() -> Option<PromotionBarrierGuard> {
    let b = barrier();
    let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    if current_thread_holds_barrier() {
        let ready = state.can_promote();
        drop(state);
        return ready.then(PromotionBarrierGuard::new_held);
    }
    if state.promotion_active || state.writers > 0 {
        return None;
    }
    state.promotion_active = true;
    state.promotion_ready = true;
    drop(state);
    Some(PromotionBarrierGuard::new_held())
}

/// Decide whether a drain wait is complete, including a late release wakeup.
/// The original budget wins even if the last writer finished before we could
/// reacquire the mutex. Initial zero-budget idle admission is handled separately.
fn drain_progress(
    remaining_writers: usize,
    waited: Duration,
    timeout: Duration,
) -> Option<DrainOutcome> {
    if waited >= timeout {
        Some(DrainOutcome::TimedOut { remaining_writers })
    } else if remaining_writers == 0 {
        Some(DrainOutcome::Drained { waited })
    } else {
        None
    }
}

/// Acquire the promotion barrier for corruption recovery.
///
/// Shares one `timeout` between waiting for a previous promotion and draining
/// writers. Once ownership is obtained, new writers are blocked immediately.
/// A timeout after ownership retains exclusion until the guard is dropped but
/// never grants nested promotion permission, even after a late writer drains.
/// Queued timeouts and refused nested calls return inert guards. A nested call
/// cannot wait on a writer that its own caller may own: it refuses immediately.
/// Callers must treat every [`DrainOutcome::TimedOut`] as a refusal to promote,
/// including a timeout reporting zero writers. A zero budget can acquire an
/// immediately idle barrier, but never waits for another owner or a writer.
pub fn acquire_promotion_barrier_draining(
    timeout: Duration,
) -> (PromotionBarrierGuard, DrainOutcome) {
    let started = Instant::now();
    let b = barrier();
    let mut state = b.state.lock().unwrap_or_else(PoisonError::into_inner);
    if current_thread_holds_barrier() {
        let ready = state.can_promote();
        let remaining_writers = state.writers;
        drop(state);
        return if ready {
            (PromotionBarrierGuard::new_held(), DrainOutcome::Idle)
        } else {
            (
                PromotionBarrierGuard::new_unheld(),
                DrainOutcome::TimedOut { remaining_writers },
            )
        };
    }
    // Queueing is part of the same budget as draining. A timeout here must
    // not construct a held/nested guard: that would release another owner's
    // gate or let this thread bypass it on a subsequent write operation.
    let mut queued = false;
    while state.promotion_active {
        queued = true;
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let outcome = DrainOutcome::TimedOut {
                remaining_writers: state.writers,
            };
            drop(state);
            return (PromotionBarrierGuard::new_unheld(), outcome);
        }
        let (next, _timeout) = b
            .promotion_released
            .wait_timeout(state, remaining.min(WRITER_WAIT_WARN_INTERVAL))
            .unwrap_or_else(PoisonError::into_inner);
        state = next;
    }
    // Even a release wakeup must not reset an already exhausted queue budget.
    if queued && started.elapsed() >= timeout {
        let outcome = DrainOutcome::TimedOut {
            remaining_writers: state.writers,
        };
        drop(state);
        return (PromotionBarrierGuard::new_unheld(), outcome);
    }
    state.promotion_active = true;
    state.promotion_ready = false;

    let outcome = if state.writers == 0 {
        DrainOutcome::Idle
    } else {
        loop {
            let waited = started.elapsed();
            if let Some(outcome) = drain_progress(state.writers, waited, timeout) {
                break outcome;
            }
            let remaining = timeout.saturating_sub(waited);
            let (next, _timeout) = b
                .writers_drained
                .wait_timeout(state, remaining.min(WRITER_WAIT_WARN_INTERVAL))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            // Re-evaluate the same deadline before accepting the zero-writer
            // wakeup. A release after the budget cannot authorize promotion.
        }
    };
    state.promotion_ready = !matches!(outcome, DrainOutcome::TimedOut { .. });
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
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // A module-local mutex cannot exclude pool tests using the same global
    // barrier. Run each test in a fresh process instead of resetting live state.
    fn run_in_isolated_process() -> bool {
        const CHILD: &str = "AM_TEST_WRITE_BARRIER_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .expect("run isolated barrier test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated {name} did not pass exactly one test: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        true
    }

    #[test]
    fn drift_acquire_defers_while_foreign_writer_active() {
        if run_in_isolated_process() {
            return;
        }
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let guard = begin_write_activity();
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(guard);
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            try_acquire_promotion_barrier_if_idle().is_none(),
            "drift reconcile must defer while a foreign writer is in flight"
        );
        release_tx.send(()).unwrap();
        writer.join().expect("writer thread");
        let barrier = try_acquire_promotion_barrier_if_idle();
        assert!(barrier.is_some(), "idle process must grant the barrier");
    }

    #[test]
    fn write_activity_blocks_idle_promotion_even_on_same_thread() {
        if run_in_isolated_process() {
            return;
        }
        let _activity = begin_write_activity();
        let barrier = try_acquire_promotion_barrier_if_idle();
        assert!(
            barrier.is_none(),
            "every live writer must block promotion; cold bootstrap performs an explicit barrier-to-writer handoff"
        );
    }

    #[test]
    fn migrated_write_guard_cannot_leave_a_false_self_exemption() {
        if run_in_isolated_process() {
            return;
        }
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
        if run_in_isolated_process() {
            return;
        }
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
        if run_in_isolated_process() {
            return;
        }
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let guard = begin_write_activity();
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(guard);
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
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
        release_tx.send(()).unwrap();
        writer.join().expect("writer thread");
    }

    #[test]
    fn draining_acquire_observes_drain() {
        if run_in_isolated_process() {
            return;
        }
        // Register the writer before starting recovery. Release it only after
        // recovery has closed admission, so Idle cannot satisfy this proof.
        let writer = begin_write_activity();
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let recovery = std::thread::spawn(move || {
            let (barrier, outcome) = acquire_promotion_barrier_draining(Duration::from_secs(5));
            drained_tx.send(outcome).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(barrier);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !barrier().state.lock().unwrap().promotion_active {
            assert!(Instant::now() < deadline, "recovery never closed admission");
            std::thread::yield_now();
        }
        assert_eq!(active_writer_count(), 1);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let late_writer = std::thread::spawn(move || {
            let _guard = begin_write_activity();
            entered_tx.send(()).unwrap();
        });
        drop(writer);
        let outcome = drained_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(outcome, DrainOutcome::Drained { .. }),
            "expected drain before timeout, got {outcome:?}"
        );
        assert_eq!(active_writer_count(), 0);
        assert_eq!(
            entered_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "late writer entered during promotion"
        );
        release_tx.send(()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        late_writer.join().expect("late writer thread");
        recovery.join().expect("recovery thread");
    }

    #[test]
    fn nested_barrier_is_passthrough() {
        if run_in_isolated_process() {
            return;
        }
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
        if run_in_isolated_process() {
            return;
        }
        let path = Path::new("/tmp/write-barrier-test.sqlite3");
        assert!(time_since_last_promotion(path).is_none());
        record_promotion(path);
        let age = time_since_last_promotion(path).expect("recorded");
        assert!(age < Duration::from_secs(5));
    }

    #[test]
    fn queued_recovery_times_out_without_releasing_the_current_owner() {
        if run_in_isolated_process() {
            return;
        }
        for timeout in [Duration::ZERO, Duration::from_millis(50)] {
            let owner = try_acquire_promotion_barrier_if_idle().expect("owner");
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let queued = std::thread::spawn(move || {
                let (guard, outcome) = acquire_promotion_barrier_draining(timeout);
                let held_before_drop = current_thread_holds_promotion_barrier();
                drop(guard);
                done_tx
                    .send((
                        outcome,
                        held_before_drop,
                        current_thread_holds_promotion_barrier(),
                    ))
                    .unwrap();
            });
            let observed = done_rx.recv_timeout(Duration::from_secs(2));
            let owner_still_active = barrier().state.lock().unwrap().promotion_active;
            // Release even when the regression returned no result, so the test
            // can join its worker instead of leaving an orphan blocked forever.
            drop(owner);
            queued.join().expect("queued recovery");
            let (outcome, held_before_drop, held_after_drop) =
                observed.expect("bounded queue wait");
            assert_eq!(
                outcome,
                DrainOutcome::TimedOut {
                    remaining_writers: 0
                }
            );
            assert!(!held_before_drop && !held_after_drop);
            assert!(
                owner_still_active,
                "a timed-out waiter released the owner's barrier"
            );
        }
    }

    #[test]
    fn expired_queue_guard_cannot_release_a_later_acquisition_on_the_same_thread() {
        if run_in_isolated_process() {
            return;
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let guard = try_acquire_promotion_barrier_if_idle().expect("foreign owner");
            ready_tx.send(()).unwrap();
            let released = release_rx.recv_timeout(Duration::from_secs(5));
            drop(guard);
            released
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (expired, outcome) = acquire_promotion_barrier_draining(Duration::from_millis(50));
        let falsely_owned = current_thread_holds_promotion_barrier();
        let _ = release_tx.send(());
        let owner_released = owner.join().expect("foreign owner");
        assert!(owner_released.is_ok());
        assert_eq!(
            outcome,
            DrainOutcome::TimedOut {
                remaining_writers: 0
            }
        );
        assert!(!falsely_owned);

        let fresh = try_acquire_promotion_barrier_if_idle().expect("fresh owner");
        drop(expired);
        assert!(current_thread_holds_promotion_barrier());
        assert!(barrier().state.lock().unwrap().promotion_active);
        drop(fresh);
        assert!(!current_thread_holds_promotion_barrier());
        assert!(!barrier().state.lock().unwrap().promotion_active);
    }

    #[test]
    fn queued_recovery_can_acquire_after_the_previous_owner_releases() {
        if run_in_isolated_process() {
            return;
        }
        let owner = try_acquire_promotion_barrier_if_idle().expect("initial owner");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let queued = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let (guard, outcome) = acquire_promotion_barrier_draining(Duration::from_secs(5));
            acquired_tx
                .send((outcome, current_thread_holds_promotion_barrier()))
                .unwrap();
            let released = release_rx.recv_timeout(Duration::from_secs(5));
            drop(guard);
            released
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let before_release = acquired_rx.recv_timeout(Duration::from_millis(50));
        drop(owner);
        let acquired = acquired_rx.recv_timeout(Duration::from_secs(2));
        let held_by_successor = barrier().state.lock().unwrap().promotion_active;
        let _ = release_tx.send(());
        queued
            .join()
            .expect("queued recovery")
            .expect("release successor");
        assert_eq!(
            before_release,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        assert_eq!(acquired.unwrap(), (DrainOutcome::Idle, true));
        assert!(held_by_successor);
        assert!(!barrier().state.lock().unwrap().promotion_active);
    }

    #[test]
    fn zero_budget_preserves_idle_and_writer_timeout_semantics() {
        if run_in_isolated_process() {
            return;
        }
        let (idle, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(outcome, DrainOutcome::Idle);
        assert!(current_thread_holds_promotion_barrier());
        drop(idle);

        let writer = begin_write_activity();
        let (timed_out, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(
            outcome,
            DrainOutcome::TimedOut {
                remaining_writers: 1
            }
        );
        assert!(
            current_thread_holds_barrier(),
            "timeout retains exclusion ownership"
        );
        assert!(
            !current_thread_holds_promotion_barrier(),
            "timeout grants no promotion entitlement"
        );
        assert!(barrier().state.lock().unwrap().promotion_active);
        drop(timed_out);
        assert!(!current_thread_holds_promotion_barrier());
        assert_eq!(active_writer_count(), 1);
        drop(writer);
    }

    #[test]
    fn non_lifo_promotion_drops_hold_admission_until_the_last_lease() {
        if run_in_isolated_process() {
            return;
        }
        let first = try_acquire_promotion_barrier_if_idle().expect("first lease");
        let second = try_acquire_promotion_barrier_if_idle().expect("second lease");
        let (third, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(outcome, DrainOutcome::Idle);
        drop(first);
        drop(second);
        assert!(current_thread_holds_promotion_barrier());
        assert!(barrier().state.lock().unwrap().promotion_active);
        let foreign_denied =
            std::thread::spawn(|| try_acquire_promotion_barrier_if_idle().is_none())
                .join()
                .expect("foreign acquisition");
        assert!(foreign_denied, "a live nested lease lost exclusion");
        drop(third);
        assert!(!current_thread_holds_promotion_barrier());
        assert!(!barrier().state.lock().unwrap().promotion_active);
        assert!(try_acquire_promotion_barrier_if_idle().is_some());
    }

    #[test]
    fn nested_promotion_to_writer_handoff_keeps_the_writer_counted() {
        if run_in_isolated_process() {
            return;
        }
        let first = try_acquire_promotion_barrier_if_idle().expect("first lease");
        let second = try_acquire_promotion_barrier_if_idle().expect("nested lease");
        let writer = begin_write_activity();
        assert_eq!(active_writer_count(), 1);
        drop(first);
        assert!(barrier().state.lock().unwrap().promotion_active);
        drop(second);
        assert!(!current_thread_holds_promotion_barrier());
        assert!(!barrier().state.lock().unwrap().promotion_active);
        assert!(try_acquire_promotion_barrier_if_idle().is_none());
        std::thread::spawn(move || drop(writer))
            .join()
            .expect("migrated writer");
        assert_eq!(active_writer_count(), 0);
        assert!(try_acquire_promotion_barrier_if_idle().is_some());
    }

    fn assert_nested_promotion_refused(remaining_writers: usize) {
        let depth = THREAD_BARRIER_DEPTH.with(Cell::get);
        assert!(depth > 0);
        assert!(!current_thread_holds_promotion_barrier());
        assert!(try_acquire_promotion_barrier_if_idle().is_none());
        let (nested, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(outcome, DrainOutcome::TimedOut { remaining_writers });
        assert!(
            !nested.participating,
            "refused nesting must not gain a lease"
        );
        drop(nested);
        assert_eq!(THREAD_BARRIER_DEPTH.with(Cell::get), depth);
        assert!(barrier().state.lock().unwrap().promotion_active);
    }

    #[test]
    fn failed_owned_drain_cannot_be_upgraded_by_nested_recovery() {
        if run_in_isolated_process() {
            return;
        }
        let writer = begin_write_activity();
        let (failed, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(
            outcome,
            DrainOutcome::TimedOut {
                remaining_writers: 1
            }
        );
        assert!(failed.participating);
        assert_nested_promotion_refused(1);
        drop(writer);
        // Even observing zero writers later cannot extend the expired attempt.
        assert_nested_promotion_refused(0);
        drop(failed);
        let fresh = try_acquire_promotion_barrier_if_idle().expect("fresh idle admission");
        assert!(current_thread_holds_promotion_barrier());
        drop(fresh);
    }

    #[test]
    fn handoff_writer_suspends_nested_promotion_until_it_finishes() {
        if run_in_isolated_process() {
            return;
        }
        let outer = try_acquire_promotion_barrier_if_idle().expect("outer");
        let writer = begin_write_activity();
        assert_nested_promotion_refused(1);
        drop(writer);
        assert!(current_thread_holds_promotion_barrier());
        let inner = try_acquire_promotion_barrier_if_idle().expect("handoff finished");
        drop(outer);
        assert!(current_thread_holds_promotion_barrier());
        drop(inner);
        assert!(!barrier().state.lock().unwrap().promotion_active);
    }

    #[test]
    fn migrated_handoff_writer_cannot_be_hidden_by_promotion_thread_ownership() {
        if run_in_isolated_process() {
            return;
        }
        let outer = try_acquire_promotion_barrier_if_idle().expect("outer");
        let writer = begin_write_activity();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let released = release_rx.recv_timeout(Duration::from_secs(5));
            drop(writer);
            released
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_nested_promotion_refused(1);
        release_tx.send(()).unwrap();
        worker.join().unwrap().expect("release migrated writer");
        let (inner, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(outcome, DrainOutcome::Idle);
        drop(outer);
        assert!(current_thread_holds_promotion_barrier());
        drop(inner);
    }

    #[test]
    fn refused_nested_guard_cannot_release_a_later_fresh_attempt() {
        if run_in_isolated_process() {
            return;
        }
        let writer = begin_write_activity();
        let (failed, _) = acquire_promotion_barrier_draining(Duration::ZERO);
        let (refused, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        assert_eq!(
            outcome,
            DrainOutcome::TimedOut {
                remaining_writers: 1
            }
        );
        assert!(!refused.participating);
        drop(writer);
        drop(failed);
        let fresh = try_acquire_promotion_barrier_if_idle().expect("new attempt");
        drop(refused);
        assert!(current_thread_holds_promotion_barrier());
        assert!(barrier().state.lock().unwrap().promotion_active);
        drop(fresh);
    }

    #[test]
    fn failed_drain_still_excludes_new_writers_until_its_guard_drops() {
        if run_in_isolated_process() {
            return;
        }
        let first_writer = begin_write_activity();
        let (failed, _) = acquire_promotion_barrier_draining(Duration::ZERO);
        drop(first_writer);
        assert_nested_promotion_refused(0);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let late = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _writer = begin_write_activity();
            entered_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let before_drop = entered_rx.recv_timeout(Duration::from_millis(50));
        drop(failed);
        let after_drop = entered_rx.recv_timeout(Duration::from_secs(5));
        late.join().expect("late writer");
        assert_eq!(before_drop, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        assert!(after_drop.is_ok());
        assert_eq!(active_writer_count(), 0);
    }

    #[test]
    fn drain_wakeup_must_precede_the_original_deadline() {
        let budget = Duration::from_millis(100);
        assert_eq!(
            drain_progress(0, Duration::from_millis(99), budget),
            Some(DrainOutcome::Drained {
                waited: Duration::from_millis(99)
            })
        );
        for waited in [budget, budget + Duration::from_nanos(1), Duration::MAX] {
            for remaining_writers in [0, 1] {
                assert_eq!(
                    drain_progress(remaining_writers, waited, budget),
                    Some(DrainOutcome::TimedOut { remaining_writers })
                );
            }
        }
        assert_eq!(drain_progress(1, Duration::from_millis(99), budget), None);
        assert_eq!(
            drain_progress(0, Duration::ZERO, Duration::ZERO),
            Some(DrainOutcome::TimedOut {
                remaining_writers: 0
            })
        );
    }

    #[test]
    fn denied_nested_recovery_preserves_the_open_writer_file_generation() {
        if run_in_isolated_process() {
            return;
        }
        use std::io::Write;

        // Real file handles and renames exercise the admission boundary, not
        // SQLite reconstruction. Preserve both generations for inspection.
        let root = tempfile::tempdir().unwrap().keep();
        let live = root.join("live-generation");
        let candidate = root.join("candidate-generation");
        let saved = root.join("saved-generation");
        std::fs::write(&live, b"original;").unwrap();
        std::fs::write(&candidate, b"replacement;").unwrap();
        let writer_guard = begin_write_activity();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .unwrap();
        let (failed, _) = acquire_promotion_barrier_draining(Duration::ZERO);
        let (nested, outcome) = acquire_promotion_barrier_draining(Duration::ZERO);
        if matches!(outcome, DrainOutcome::Idle | DrainOutcome::Drained { .. }) {
            std::fs::rename(&live, &saved).unwrap();
            std::fs::rename(&candidate, &live).unwrap();
        }
        writer.write_all(b"acknowledged;").unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        drop(writer_guard);
        drop(nested);
        assert_eq!(std::fs::read(&live).unwrap(), b"original;acknowledged;");
        assert_eq!(std::fs::read(&candidate).unwrap(), b"replacement;");
        assert!(!saved.exists());
        assert_nested_promotion_refused(0);
        drop(failed);

        // Positive control: a fresh, genuinely idle admission can replace the
        // file without stranding any writer, retaining the original contents.
        let fresh = try_acquire_promotion_barrier_if_idle().expect("verified idle gate");
        std::fs::rename(&live, &saved).unwrap();
        std::fs::rename(&candidate, &live).unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), b"original;acknowledged;");
        assert_eq!(std::fs::read(&live).unwrap(), b"replacement;");
        drop(fresh);
    }

    fn assert_admission_observers_available(expected_writers: usize) {
        // Fail immediately instead of hanging the test if a callback is
        // accidentally invoked under the mutex, then exercise the real reader.
        let state = barrier()
            .state
            .try_lock()
            .expect("diagnostic holds no admission lock");
        assert_eq!(state.writers, expected_writers);
        drop(state);
        assert_eq!(active_writer_count(), expected_writers);
        assert!(!current_thread_holds_promotion_barrier());
    }

    #[test]
    fn writer_wait_diagnostics_can_reenter_admission_observers() {
        if run_in_isolated_process() {
            return;
        }
        let owner = try_acquire_promotion_barrier_if_idle().expect("owner");
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut warnings = 0;
            let mut resumes = 0;
            let writer = begin_write_activity_with(
                Duration::from_millis(5),
                |_| {
                    assert_admission_observers_available(0);
                    warnings += 1;
                    waiting_tx.send(()).unwrap();
                },
                |_| {
                    assert_admission_observers_available(1);
                    resumes += 1;
                },
            );
            drop(writer);
            (warnings, resumes)
        });
        let waiting = waiting_rx.recv_timeout(Duration::from_secs(5));
        drop(owner);
        let counts = worker.join().expect("diagnostic observer");
        assert!(waiting.is_ok());
        assert_eq!(counts, (1, 1));
        assert_eq!(active_writer_count(), 0);
    }

    #[test]
    fn wait_diagnostic_panic_neither_poisons_gate_nor_counts_a_writer() {
        if run_in_isolated_process() {
            return;
        }
        let owner = try_acquire_promotion_barrier_if_idle().expect("owner");
        let worker = std::thread::spawn(|| {
            std::panic::catch_unwind(|| {
                begin_write_activity_with(
                    Duration::from_millis(5),
                    |_| {
                        assert_admission_observers_available(0);
                        panic!("wait subscriber failed");
                    },
                    |_| panic!("a failed wait callback cannot resume"),
                )
            })
            .is_err()
        });
        assert!(worker.join().expect("caught subscriber panic"));
        assert!(!barrier().state.is_poisoned());
        assert_eq!(active_writer_count(), 0);
        assert!(current_thread_holds_promotion_barrier());
        drop(owner);
        let writer = begin_write_activity();
        assert_eq!(active_writer_count(), 1);
        drop(writer);
    }

    #[test]
    fn resume_diagnostic_panic_releases_its_already_counted_writer() {
        if run_in_isolated_process() {
            return;
        }
        let owner = try_acquire_promotion_barrier_if_idle().expect("owner");
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                begin_write_activity_with(
                    Duration::from_millis(5),
                    |_| {
                        assert_admission_observers_available(0);
                        waiting_tx.send(()).unwrap();
                    },
                    |_| {
                        assert_admission_observers_available(1);
                        panic!("resume subscriber failed");
                    },
                )
            }))
            .is_err()
        });
        let waiting = waiting_rx.recv_timeout(Duration::from_secs(5));
        drop(owner);
        let panicked = worker.join().expect("caught resume panic");
        assert!(waiting.is_ok() && panicked);
        assert!(!barrier().state.is_poisoned());
        assert_eq!(active_writer_count(), 0);
        assert!(try_acquire_promotion_barrier_if_idle().is_some());
    }

    #[test]
    fn admission_rechecks_a_successor_promotion_after_unlocked_diagnostics() {
        if run_in_isolated_process() {
            return;
        }
        let first = try_acquire_promotion_barrier_if_idle().expect("first owner");
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let writer = begin_write_activity_with(
                Duration::from_millis(5),
                |_| {
                    assert_admission_observers_available(0);
                    waiting_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                },
                |_| assert_admission_observers_available(1),
            );
            entered_tx.send(()).unwrap();
            drop(writer);
        });
        waiting_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(first);
        let successor = try_acquire_promotion_barrier_if_idle().expect("successor owner");
        resume_tx.send(()).unwrap();
        let early = entered_rx.recv_timeout(Duration::from_millis(50));
        drop(successor);
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        worker.join().expect("writer after successor");
        assert_eq!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        assert!(entered.is_ok());
        assert_eq!(active_writer_count(), 0);
    }

    #[test]
    fn uncontended_and_owner_handoff_admission_emit_no_wait_diagnostics() {
        if run_in_isolated_process() {
            return;
        }
        let writer = begin_write_activity_with(
            Duration::from_millis(5),
            |_| panic!("uncontended writer must not warn"),
            |_| panic!("uncontended writer must not report resuming"),
        );
        assert_eq!(active_writer_count(), 1);
        drop(writer);
        let owner = try_acquire_promotion_barrier_if_idle().expect("bootstrap owner");
        let writer = begin_write_activity_with(
            Duration::from_millis(5),
            |_| panic!("bootstrap handoff must not wait on itself"),
            |_| panic!("bootstrap handoff must not report resuming"),
        );
        assert_nested_promotion_refused(1);
        drop(writer);
        assert!(current_thread_holds_promotion_barrier());
        drop(owner);
        assert_eq!(active_writer_count(), 0);
    }
}
