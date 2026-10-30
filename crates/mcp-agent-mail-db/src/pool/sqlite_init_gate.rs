//! Single-initializer admission for a lazily opened mailbox database.
//!
//! Waiters run on blocking dispatch workers, so their timeout and cancellation
//! observation use an OS condvar rather than an async timer. The gate owns no
//! database connection or recovery authority. A refused waiter never changes
//! another initializer's state, and an initializer's completion guard resets
//! failed or unwinding work so a later caller can retry it.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

const LIVENESS_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqliteInitGateClaim {
    /// This caller must finish initialization under a completion guard.
    Initialize,
    /// Initialization already completed successfully.
    Ready,
    /// Another initializer still owns the gate after the original budget.
    TimedOut,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SqliteInitGateState {
    NotStarted,
    Running,
    Done,
}

pub(crate) struct SqliteInitGate {
    state: Mutex<SqliteInitGateState>,
    cv: Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

impl SqliteInitGate {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(SqliteInitGateState::NotStarted),
            cv: Condvar::new(),
            #[cfg(test)]
            waiters: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Claim initialization, or wait within one budget while the caller lives.
    ///
    /// The callback runs outside the state mutex before each claim/readiness
    /// check and after at most one bounded condvar wait. An error or panic
    /// leaves a foreign initializer untouched. No callback runs after claiming
    /// initialization: the caller must first construct its completion guard
    /// before performing further fallible work or cancellation checks.
    pub(crate) fn claim_or_wait_with<E>(
        &self,
        budget: Duration,
        mut check_live: impl FnMut() -> Result<(), E>,
    ) -> Result<SqliteInitGateClaim, E> {
        let deadline = Instant::now() + budget;
        loop {
            check_live()?;
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            match *state {
                SqliteInitGateState::Done => return Ok(SqliteInitGateClaim::Ready),
                SqliteInitGateState::NotStarted => {
                    *state = SqliteInitGateState::Running;
                    return Ok(SqliteInitGateClaim::Initialize);
                }
                SqliteInitGateState::Running => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(SqliteInitGateClaim::TimedOut);
                    }
                    #[cfg(test)]
                    self.waiters
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    let (guard, _) = self
                        .cv
                        .wait_timeout(state, remaining.min(LIVENESS_POLL_INTERVAL))
                        .unwrap_or_else(PoisonError::into_inner);
                    state = guard;
                    drop(state);
                    #[cfg(test)]
                    self.waiters
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
            }
        }
    }

    /// Publish the initializer's outcome and wake every bounded waiter.
    pub(crate) fn complete(&self, success: bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = if success {
            SqliteInitGateState::Done
        } else {
            SqliteInitGateState::NotStarted
        };
        drop(state);
        self.cv.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn waiter_count(&self) -> usize {
        self.waiters.load(std::sync::atomic::Ordering::Acquire)
    }

    // Keep the original non-cancelling controls and recovery fixtures on the
    // same implementation without adding an uncancellable production caller.
    #[cfg(test)]
    pub(crate) fn claim_or_wait(&self, budget: Duration) -> SqliteInitGateClaim {
        match self.claim_or_wait_with(budget, || Ok::<(), std::convert::Infallible>(())) {
            Ok(claim) => claim,
            Err(never) => match never {},
        }
    }
}

/// A failed or unwinding initializer makes the gate retryable on drop.
pub(crate) struct SqliteInitGateCompletion<'a> {
    pub(crate) gate: &'a SqliteInitGate,
    pub(crate) success: bool,
}

impl Drop for SqliteInitGateCompletion<'_> {
    fn drop(&mut self) {
        self.gate.complete(self.success);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn sqlite_init_gate_bounds_waiters_and_resets_on_failure() {
        let gate = SqliteInitGate::new();
        assert!(matches!(
            gate.claim_or_wait(Duration::from_millis(10)),
            SqliteInitGateClaim::Initialize
        ));

        let start = Instant::now();
        assert!(matches!(
            gate.claim_or_wait(Duration::from_millis(50)),
            SqliteInitGateClaim::TimedOut
        ));
        assert!(start.elapsed() >= Duration::from_millis(45));

        gate.complete(false);
        assert!(matches!(
            gate.claim_or_wait(Duration::from_millis(10)),
            SqliteInitGateClaim::Initialize
        ));

        gate.complete(true);
        assert!(matches!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Ready
        ));
    }

    #[test]
    fn sqlite_init_gate_completion_guard_resets_on_unwind_path() {
        let gate = SqliteInitGate::new();
        assert!(matches!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        ));
        drop(SqliteInitGateCompletion {
            gate: &gate,
            success: false,
        });
        assert!(matches!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        ));
    }

    #[test]
    fn sqlite_init_gate_wakes_waiters_when_initializer_completes() {
        let gate = Arc::new(SqliteInitGate::new());
        assert!(matches!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        ));
        let signaller = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            signaller.complete(true);
        });
        let start = Instant::now();
        let claim = gate.claim_or_wait(Duration::from_secs(5));
        let elapsed = start.elapsed();
        handle.join().expect("signaller thread");
        assert!(matches!(claim, SqliteInitGateClaim::Ready));
        assert!(
            elapsed < Duration::from_secs(4),
            "waiter must wake on completion, not run out its full budget"
        );
    }

    #[test]
    fn cancelled_waiter_returns_without_releasing_the_initializer() {
        let gate = Arc::new(SqliteInitGate::new());
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        );
        let owner = SqliteInitGateCompletion {
            gate: &gate,
            success: false,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::clone(&gate);
        let worker_cancelled = Arc::clone(&cancelled);
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut checks = 0;
            let result = worker_gate.claim_or_wait_with(Duration::from_secs(30), || {
                assert!(
                    worker_gate.state.try_lock().is_ok(),
                    "liveness holds the gate mutex"
                );
                checks += 1;
                if checks == 2 {
                    parked_tx.send(()).unwrap();
                }
                if worker_cancelled.load(Ordering::Acquire) {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });
            done_tx.send(result).unwrap();
        });
        let parked = parked_rx.recv_timeout(Duration::from_secs(2));
        cancelled.store(true, Ordering::Release);
        let completed = done_rx.recv_timeout(Duration::from_secs(2));
        let owner_retained = *gate.state.lock().unwrap() == SqliteInitGateState::Running;
        let waiters = gate.waiter_count();
        drop(owner);
        worker.join().expect("cancelled initialization waiter");
        assert!(parked.is_ok(), "waiter never observed a parked interval");
        assert_eq!(completed.unwrap(), Err("cancelled"));
        assert!(owner_retained, "a waiter released another initializer");
        assert_eq!(waiters, 0);
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        );
    }

    #[test]
    fn cancellation_before_claim_never_owns_or_reopens_the_gate() {
        let gate = SqliteInitGate::new();
        assert_eq!(
            gate.claim_or_wait_with(Duration::ZERO, || Err("cancelled")),
            Err("cancelled")
        );
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        );
        gate.complete(true);
        assert_eq!(
            gate.claim_or_wait_with(Duration::ZERO, || Err("cancelled")),
            Err("cancelled")
        );
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Ready
        );
    }

    #[test]
    fn liveness_panic_keeps_foreign_ownership_and_an_unpoisoned_mutex() {
        let gate = SqliteInitGate::new();
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        );
        for panic_at in [1, 2] {
            let mut checks = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gate.claim_or_wait_with(Duration::from_secs(5), || {
                    assert!(
                        gate.state.try_lock().is_ok(),
                        "liveness holds the gate mutex"
                    );
                    checks += 1;
                    assert!(checks != panic_at, "failed liveness observer");
                    Ok::<(), ()>(())
                })
            }));
            assert!(result.is_err());
            assert!(!gate.state.is_poisoned());
            assert!(*gate.state.lock().unwrap() == SqliteInitGateState::Running);
            assert_eq!(gate.waiter_count(), 0);
        }
        gate.complete(true);
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Ready
        );
    }

    #[test]
    fn cancellation_after_completion_does_not_erase_readiness() {
        let gate = Arc::new(SqliteInitGate::new());
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        );
        let worker_gate = Arc::clone(&gate);
        let (checking_tx, checking_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_gate.claim_or_wait_with(Duration::from_secs(5), || {
                checking_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Err("cancelled at completion")
            })
        });
        let checking = checking_rx.recv_timeout(Duration::from_secs(2));
        gate.complete(true);
        let _ = release_tx.send(());
        let completed = worker.join().expect("completion race waiter");
        assert!(checking.is_ok());
        assert_eq!(completed, Err("cancelled at completion"));
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Ready
        );
    }

    #[test]
    fn spurious_notifications_do_not_restart_the_wait_budget() {
        let gate = Arc::new(SqliteInitGate::new());
        assert_eq!(
            gate.claim_or_wait(Duration::ZERO),
            SqliteInitGateClaim::Initialize
        );
        let stop = Arc::new(AtomicBool::new(false));
        let signaller_gate = Arc::clone(&gate);
        let signaller_stop = Arc::clone(&stop);
        let signaller = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !signaller_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                signaller_gate.cv.notify_all();
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let started = Instant::now();
        let mut checks = 0;
        let result = gate.claim_or_wait_with(Duration::from_millis(60), || {
            checks += 1;
            Ok::<(), ()>(())
        });
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Release);
        signaller.join().expect("spurious notifier");
        assert_eq!(result, Ok(SqliteInitGateClaim::TimedOut));
        assert!(checks >= 3, "waiter never rechecked its liveness");
        assert!(elapsed >= Duration::from_millis(50));
        assert!(
            elapsed < Duration::from_secs(1),
            "notifications reset the original budget"
        );
        assert!(*gate.state.lock().unwrap() == SqliteInitGateState::Running);
        assert_eq!(gate.waiter_count(), 0);
        gate.complete(false);
    }
}
