//! Restart the owned archive-maintenance thread after an unwinding run.
//!
//! Only a reporting-clock hint survives a failed run. Its database pool,
//! transactions, source cursors and archive guards belong to the attempt and
//! must unwind before restart. The caller captures the original process-start
//! grace anchor, never a new anchor that could treat in-flight writes as settled.
//! This does not recover process aborts or preempt blocked native operations.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MIN_RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub(super) struct Restart {
    pub(super) panicked: bool,
    pub(super) delay: Duration,
}

#[must_use = "keep the guard alive through the reporting/pruning sweep"]
pub(super) struct ReportAttempt<'a> {
    last_report: &'a mut Option<Instant>,
}

impl<'a> ReportAttempt<'a> {
    pub(super) fn begin(last_report: &'a mut Option<Instant>) -> Self {
        *last_report = Some(Instant::now());
        Self { last_report }
    }
}

impl Drop for ReportAttempt<'_> {
    fn drop(&mut self) {
        *self.last_report = Some(Instant::now());
    }
}

fn restart_delay(previous: Duration, lived: Duration) -> Duration {
    if lived >= MAX_RESTART_DELAY {
        MIN_RESTART_DELAY
    } else {
        previous
            .saturating_mul(2)
            .clamp(MIN_RESTART_DELAY, MAX_RESTART_DELAY)
    }
}

pub(super) fn run(
    stop: &AtomicBool,
    attempt: impl FnMut(&mut Option<Instant>),
    wait: impl FnMut(Duration) -> bool,
    observe_restart: impl FnMut(Restart),
) {
    run_with(stop, attempt, wait, observe_restart, |started| {
        started.elapsed()
    });
}

fn run_with(
    stop: &AtomicBool,
    mut attempt: impl FnMut(&mut Option<Instant>),
    mut wait: impl FnMut(Duration) -> bool,
    mut observe_restart: impl FnMut(Restart),
    mut lived: impl FnMut(Instant) -> Duration,
) {
    let mut previous_delay = Duration::ZERO;
    let mut last_report = None;
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            attempt(&mut last_report);
        }))
        .is_err();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let delay = restart_delay(previous_delay, lived(started));
        previous_delay = delay;
        observe_restart(Restart { panicked, delay });
        if stop.load(Ordering::Acquire) || wait(delay) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn restart_backoff_is_bounded_and_resets_only_after_a_stable_run() {
        let mut previous = Duration::ZERO;
        for expected in [1, 2, 4, 8, 16, 32, 60, 60] {
            previous = restart_delay(previous, Duration::ZERO);
            assert_eq!(previous, Duration::from_secs(expected));
        }
        assert_eq!(
            restart_delay(Duration::MAX, Duration::ZERO),
            MAX_RESTART_DELAY
        );
        assert_eq!(
            restart_delay(
                MAX_RESTART_DELAY,
                MAX_RESTART_DELAY
                    .checked_sub(Duration::from_nanos(1))
                    .unwrap()
            ),
            MAX_RESTART_DELAY
        );
        assert_eq!(
            restart_delay(MAX_RESTART_DELAY, MAX_RESTART_DELAY),
            MIN_RESTART_DELAY
        );
    }

    #[test]
    fn panics_and_unexpected_returns_both_restart_on_the_same_thread() {
        let stop = AtomicBool::new(false);
        let calls = Cell::new(0);
        let original_thread = std::thread::current().id();
        let same_thread = Cell::new(true);
        let mut waits = Vec::new();
        let mut events = Vec::new();
        run_with(
            &stop,
            |_| {
                calls.set(calls.get() + 1);
                same_thread
                    .set(same_thread.get() && std::thread::current().id() == original_thread);
                match calls.get() {
                    1 => panic!("injected archive run failure"),
                    2 => {}
                    _ => stop.store(true, Ordering::Release),
                }
            },
            |delay| {
                waits.push(delay);
                false
            },
            |event| events.push((event.panicked, event.delay)),
            |_| Duration::ZERO,
        );
        assert_eq!(calls.get(), 3);
        assert!(same_thread.get());
        assert_eq!(waits, [Duration::from_secs(1), Duration::from_secs(2)]);
        assert_eq!(events, [(true, waits[0]), (false, waits[1])]);
    }

    #[test]
    fn stopped_worker_never_enters_an_attempt_or_backoff() {
        let stop = AtomicBool::new(true);
        let calls = Cell::new(0);
        let waits = Cell::new(0);
        let restarts = Cell::new(0);
        run(
            &stop,
            |_| calls.set(calls.get() + 1),
            |_| {
                waits.set(waits.get() + 1);
                true
            },
            |_| restarts.set(restarts.get() + 1),
        );
        assert_eq!((calls.get(), waits.get(), restarts.get()), (0, 0, 0));
    }

    #[test]
    fn shutdown_wins_over_an_unwinding_attempt() {
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        run(
            &stop,
            |_| {
                calls += 1;
                stop.store(true, Ordering::Release);
                panic!("unwind races shutdown");
            },
            |_| panic!("shutdown must not enter backoff"),
            |_| panic!("shutdown must not announce restart"),
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_completed_backoff_still_checks_shutdown_before_reentry() {
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        run(
            &stop,
            |_| calls += 1,
            |_| {
                stop.store(true, Ordering::Release);
                false
            },
            |_| {},
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn interrupted_backoff_does_not_start_another_attempt() {
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        run(&stop, |_| calls += 1, |_| true, |_| {});
        assert_eq!(calls, 1);
    }

    #[test]
    fn failed_run_resources_drop_before_observation_backoff_and_reentry() {
        struct Lease<'a>(&'a Cell<usize>);
        impl Drop for Lease<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        let stop = AtomicBool::new(false);
        let calls = Cell::new(0);
        let active = Cell::new(0);
        let clean = Cell::new(true);
        run(
            &stop,
            |_| {
                clean.set(clean.get() && active.get() == 0);
                active.set(active.get() + 1);
                let _lease = Lease(&active);
                calls.set(calls.get() + 1);
                assert_ne!(calls.get(), 1, "unwind while holding run-local resources");
                stop.store(true, Ordering::Release);
            },
            |_| {
                clean.set(clean.get() && active.get() == 0);
                false
            },
            |_| clean.set(clean.get() && active.get() == 0),
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(active.get(), 0);
        assert!(clean.get());
    }

    #[test]
    fn report_guard_records_completion_on_success_and_unwind() {
        let mut last_report = None;
        for fail in [false, true] {
            let before = Instant::now();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _attempt = ReportAttempt::begin(&mut last_report);
                assert!(!fail, "injected reporting failure");
            }));
            let after = Instant::now();
            assert_eq!(outcome.is_err(), fail);
            let completed = last_report.expect("both exits record the reporting clock");
            assert!(completed >= before && completed <= after);
        }
    }

    #[test]
    fn failed_report_schedule_survives_restart_instead_of_restarting_pruning() {
        let stop = AtomicBool::new(false);
        let calls = Cell::new(0);
        let remembered = Cell::new(None);
        let observed = Cell::new(None);
        run(
            &stop,
            |last_report| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    let _report = ReportAttempt::begin(last_report);
                    panic!("failure during retention reporting");
                }
                observed.set(*last_report);
                stop.store(true, Ordering::Release);
            },
            |_| false,
            |_| remembered.set(Some(Instant::now())),
        );
        assert_eq!(calls.get(), 2);
        assert!(observed.get().is_some());
        assert!(observed.get().unwrap() <= remembered.get().unwrap());
    }

    #[test]
    fn a_stable_attempt_resets_the_running_supervisors_backoff() {
        let stop = AtomicBool::new(false);
        let calls = Cell::new(0);
        let mut delays = Vec::new();
        run_with(
            &stop,
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 4 {
                    stop.store(true, Ordering::Release);
                }
            },
            |_| false,
            |event| delays.push(event.delay.as_secs()),
            |_| {
                if calls.get() == 3 {
                    MAX_RESTART_DELAY
                } else {
                    Duration::ZERO
                }
            },
        );
        assert_eq!(calls.get(), 4);
        assert_eq!(delays, [1, 2, 1]);
    }
}
