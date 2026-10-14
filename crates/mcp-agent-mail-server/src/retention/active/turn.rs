//! Cooperative catch-up turns for the active-lease repair worker.
//!
//! Storage owns all source, generation, expiry, conflict and publication checks.
//! This scheduler only removes intervening sleeps while bounded passes make
//! progress. It never manufactures a storage report for refused admission.

use std::time::{Duration, Instant};

use mcp_agent_mail_storage::recovery::active_reservations::ActiveReservationReport;

use super::{CATCH_UP_INTERVAL, POLL_INTERVAL, next_delay};

const MAX_BATCHES_PER_TURN: usize = 8;
const MAX_ACTIVE_TIME: Duration = Duration::from_millis(250);
const ADMISSION_REFUSAL: &str =
    "active reservation repair deferred: recovery promotion or admission contention";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Turn {
    pub(super) delay: Duration,
    /// Only an actual empty, complete pass permits retiring an idle pool.
    /// Cooling retry debt, cancellation and admission refusal do not.
    pub(super) idle: bool,
}

fn stopped() -> Turn {
    Turn {
        delay: POLL_INTERVAL,
        idle: false,
    }
}

fn pending() -> Turn {
    Turn {
        delay: CATCH_UP_INTERVAL,
        idle: false,
    }
}

pub(super) fn run(
    repair: impl FnMut() -> Result<ActiveReservationReport, String>,
    observe: impl FnMut(&ActiveReservationReport),
    cancelled: impl FnMut() -> bool,
) -> Result<Turn, String> {
    let started = Instant::now();
    run_with(repair, observe, || started.elapsed(), cancelled)
}

fn run_with(
    mut repair: impl FnMut() -> Result<ActiveReservationReport, String>,
    mut observe: impl FnMut(&ActiveReservationReport),
    mut elapsed: impl FnMut() -> Duration,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Turn, String> {
    for batch in 1..=MAX_BATCHES_PER_TURN {
        if cancelled() {
            return Ok(stopped());
        }
        if batch > 1 && elapsed() >= MAX_ACTIVE_TIME {
            return Ok(pending());
        }
        let report = match repair() {
            Ok(report) => report,
            Err(error) if error == ADMISSION_REFUSAL => {
                if cancelled() {
                    return Ok(stopped());
                }
                // The source was not admitted, not found broken. Preserve its
                // pool/cursor and retry after a pause; never loop on the fence.
                tracing::warn!(target: "maintenance",
                    event = "active_reservation_reconcile_admission_deferred",
                    reason = %error,
                    "active lease repair admission busy; retaining source for the next short cycle");
                return Ok(pending());
            }
            Err(error) => return Err(error),
        };
        // Observer I/O counts against the cooperative time target. Preserve
        // one report per real pass, including a partial pass before failure.
        observe(&report);
        if report.interrupted || cancelled() {
            return Ok(stopped());
        }
        let delay = next_delay(&report);
        let progressed = report.repaired > 0 || (report.unchanged > 0 && report.deferred == 0);
        if delay != CATCH_UP_INTERVAL
            || !report.more
            || !progressed
            || report.deferred > 0
            || report.deferred_transient > 0
            || batch == MAX_BATCHES_PER_TURN
            || elapsed() >= MAX_ACTIVE_TIME
        {
            return Ok(Turn {
                delay,
                idle: report.scanned == 0 && report.retry_len == 0 && !report.more,
            });
        }
    }
    Ok(pending())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    fn busy() -> ActiveReservationReport {
        ActiveReservationReport {
            scanned: 4,
            attempted: 4,
            repaired: 4,
            more: true,
            ..Default::default()
        }
    }

    #[test]
    fn productive_batches_drain_without_sleeps_and_keep_separate_reports() {
        let mut reports = VecDeque::from([
            busy(),
            busy(),
            ActiveReservationReport {
                scanned: 1,
                attempted: 1,
                repaired: 1,
                ..Default::default()
            },
        ]);
        let mut observed = Vec::new();
        let turn = run_with(
            || Ok(reports.pop_front().expect("finished discovery must yield")),
            |report| observed.push(report.repaired),
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(observed, [4, 4, 1]);
        assert!(reports.is_empty());
        assert_eq!(
            turn,
            Turn {
                delay: POLL_INTERVAL,
                idle: false,
            }
        );
    }

    #[test]
    fn healthy_pages_make_bounded_progress_towards_missing_artifacts() {
        let mut reports = VecDeque::from([
            ActiveReservationReport {
                scanned: 32,
                unchanged: 32,
                more: true,
                ..Default::default()
            },
            busy(),
            ActiveReservationReport::default(),
        ]);
        let mut observed = Vec::new();
        let turn = run_with(
            || Ok(reports.pop_front().expect("empty completed round must yield")),
            |report| observed.push((report.scanned, report.repaired)),
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(observed, [(32, 0), (4, 4), (0, 0)]);
        assert_eq!(
            turn,
            Turn {
                delay: POLL_INTERVAL,
                idle: true,
            }
        );
    }

    #[test]
    fn endless_productive_work_yields_at_the_original_batch_multiple() {
        let calls = Cell::new(0);
        let mut observed = 0;
        let turn = run_with(
            || {
                calls.set(calls.get() + 1);
                Ok(busy())
            },
            |_| observed += 1,
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(calls.get(), MAX_BATCHES_PER_TURN);
        assert_eq!(observed, MAX_BATCHES_PER_TURN);
        assert_eq!(turn, pending());
    }

    #[test]
    fn batch_or_observer_time_prevents_admitting_another_pass() {
        for slow_observer in [false, true] {
            let time = Cell::new(Duration::ZERO);
            let calls = Cell::new(0);
            let turn = run_with(
                || {
                    calls.set(calls.get() + 1);
                    if !slow_observer {
                        time.set(MAX_ACTIVE_TIME);
                    }
                    Ok(busy())
                },
                |_| {
                    if slow_observer {
                        time.set(MAX_ACTIVE_TIME);
                    }
                },
                || time.get(),
                || false,
            )
            .unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(turn, pending());
        }
    }

    #[test]
    fn contention_and_cooling_debt_keep_the_pool_without_a_same_turn_retry() {
        for cooling in [false, true] {
            let mut calls = 0;
            let turn = run_with(
                || {
                    calls += 1;
                    Ok(ActiveReservationReport {
                        scanned: usize::from(!cooling),
                        deferred: usize::from(!cooling),
                        deferred_transient: usize::from(!cooling),
                        retry_len: 1,
                        more: true,
                        ..Default::default()
                    })
                },
                |_| {},
                || Duration::ZERO,
                || false,
            )
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(turn, pending());
        }
    }

    #[test]
    fn conflicts_do_not_become_a_hot_catch_up_loop() {
        let mut calls = 0;
        let turn = run_with(
            || {
                calls += 1;
                Ok(ActiveReservationReport {
                    scanned: 5,
                    deferred: 1,
                    ..busy()
                })
            },
            |_| {},
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            turn,
            Turn {
                delay: POLL_INTERVAL,
                idle: false,
            }
        );
    }

    #[test]
    fn admission_refusal_is_not_an_empty_pass_or_a_failed_source_pool() {
        let mut calls = 0;
        let turn = run_with(
            || {
                calls += 1;
                Err(ADMISSION_REFUSAL.to_string())
            },
            |_| panic!("refusal cannot produce a storage report"),
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(turn, pending());
        for error in [
            "active reservation source changed after selection; rescan required",
            "active reservation repair refused: corruption breaker open",
            "permission denied",
            "source error: active reservation repair deferred: recovery promotion or admission contention",
        ] {
            let failed = run_with(
                || Err(error.to_string()),
                |_| panic!("source failure cannot produce a storage report"),
                || Duration::ZERO,
                || false,
            );
            assert_eq!(failed.unwrap_err(), error);
        }
    }

    #[test]
    fn failure_after_progress_keeps_the_completed_pass_observation() {
        for admission in [false, true] {
            let mut calls = 0;
            let mut observed = Vec::new();
            let result = run_with(
                || {
                    calls += 1;
                    if calls == 1 {
                        Ok(busy())
                    } else {
                        let error = if admission { ADMISSION_REFUSAL } else { "source changed" };
                        Err(error.to_string())
                    }
                },
                |report| observed.push(report.repaired),
                || Duration::ZERO,
                || false,
            );
            assert_eq!(calls, 2);
            assert_eq!(observed, [4]);
            if admission {
                assert_eq!(result.unwrap(), pending());
            } else {
                assert_eq!(result.unwrap_err(), "source changed");
            }
        }
    }

    #[test]
    fn shutdown_wins_before_admission_and_after_completed_observation() {
        assert_eq!(
            run_with(
                || panic!("cancelled worker must not admit storage"),
                |_| panic!("no fake report"),
                || Duration::ZERO,
                || true,
            )
            .unwrap(),
            stopped(),
        );
        let stop = Cell::new(false);
        let mut calls = 0;
        let turn = run_with(
            || {
                calls += 1;
                Ok(busy())
            },
            |_| stop.set(true),
            || Duration::ZERO,
            || stop.get(),
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(turn, stopped());
    }

    #[test]
    fn cancellation_during_admission_refusal_does_not_request_another_turn() {
        let stop = Cell::new(false);
        let turn = run_with(
            || {
                stop.set(true);
                Err(ADMISSION_REFUSAL.to_string())
            },
            |_| panic!("refusal has no report"),
            || Duration::ZERO,
            || stop.get(),
        )
        .unwrap();
        assert_eq!(turn, stopped());
    }
}
