//! Cooperative catch-up turns for the active-lease repair worker.
//!
//! Storage owns all source, generation, expiry, conflict and publication checks.
//! This scheduler only removes intervening sleeps while bounded passes make
//! progress. It never manufactures a storage report for refused admission.
//! Critical disk pressure yields without consuming a lease or its checkpoint.

use std::time::{Duration, Instant};

use mcp_agent_mail_storage::recovery::active_reservations::ActiveReservationReport;

use super::{CATCH_UP_INTERVAL, POLL_INTERVAL, next_delay};

const MAX_BATCHES_PER_TURN: usize = 8;
const MAX_ACTIVE_TIME: Duration = Duration::from_millis(250);
const ADMISSION_REFUSAL: &str =
    "active reservation repair deferred: recovery promotion or admission contention";
const CHECKPOINT_ADMISSION_REFUSAL: &str =
    "active reservation repair deferred: checkpoint is owned by another worker";
const DISK_PRESSURE_REFUSAL: &str = "active reservation repair deferred: critical disk pressure";

pub(super) fn disk_is_critical() -> bool {
    pressure_is_critical(
        mcp_agent_mail_core::global_metrics()
            .system
            .disk_pressure_level
            .load(),
    )
}

fn pressure_is_critical(pressure: u64) -> bool {
    pressure >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64()
}

fn with_disk_admission(
    pressure: u64,
    repair: impl FnOnce() -> Result<ActiveReservationReport, String>,
) -> Result<ActiveReservationReport, String> {
    if pressure_is_critical(pressure) {
        return Err(DISK_PRESSURE_REFUSAL.into());
    }
    repair()
}

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
    mut repair: impl FnMut() -> Result<ActiveReservationReport, String>,
    observe: impl FnMut(&ActiveReservationReport),
    cancelled: impl FnMut() -> bool,
) -> Result<Turn, String> {
    let started = Instant::now();
    run_with(
        || {
            with_disk_admission(
                mcp_agent_mail_core::global_metrics()
                    .system
                    .disk_pressure_level
                    .load(),
                &mut repair,
            )
        },
        observe,
        || started.elapsed(),
        cancelled,
    )
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
            Err(error) if error == DISK_PRESSURE_REFUSAL => {
                // No source or checkpoint was admitted. This is not empty
                // storage, nor a failed source family: retain the pool and
                // cursor and wait on the ordinary poll, without a hot retry.
                return Ok(stopped());
            }
            Err(error) if error == ADMISSION_REFUSAL || error == CHECKPOINT_ADMISSION_REFUSAL => {
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

    #[test]
    fn critical_disk_refusal_never_enters_storage_or_reports_idle_success() {
        let critical = mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64();
        assert!(critical > 0);
        for pressure in [critical, critical.saturating_add(1), u64::MAX] {
            assert_eq!(
                with_disk_admission(pressure, || panic!("no source or checkpoint access"))
                    .unwrap_err(),
                DISK_PRESSURE_REFUSAL,
            );
            let turn = run_with(
                || with_disk_admission(pressure, || panic!("no repair")),
                |_| panic!("pressure has no storage report"),
                || Duration::ZERO,
                || false,
            )
            .unwrap();
            assert_eq!(turn, stopped());
            assert!(!turn.idle);
        }
        for pressure in [0, critical - 1] {
            assert_eq!(
                with_disk_admission(pressure, || Ok(busy()))
                    .unwrap()
                    .repaired,
                4,
            );
            assert_eq!(
                with_disk_admission(pressure, || Err("source unavailable".into())).unwrap_err(),
                "source unavailable",
            );
        }
    }

    #[test]
    fn pressure_after_progress_preserves_the_remaining_lease_batch_until_resume() {
        let pressure = Cell::new(0);
        let mut reports = VecDeque::from([
            busy(),
            ActiveReservationReport {
                scanned: 1,
                attempted: 1,
                repaired: 1,
                ..Default::default()
            },
        ]);
        let mut observed = Vec::new();
        let paused = run_with(
            || {
                with_disk_admission(pressure.get(), || {
                    Ok(reports
                        .pop_front()
                        .expect("only pending work may be consumed"))
                })
            },
            |report| {
                observed.push(report.repaired);
                pressure.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
            },
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(paused, stopped());
        assert_eq!(observed, [4]);
        assert_eq!(reports.len(), 1);
        for _ in 0..60 {
            assert_eq!(
                run_with(
                    || with_disk_admission(pressure.get(), || panic!("no storage while critical")),
                    |_| panic!("no fabricated batch"),
                    || Duration::ZERO,
                    || false,
                )
                .unwrap(),
                stopped()
            );
        }
        pressure.set(0);
        let resumed = run_with(
            || {
                with_disk_admission(pressure.get(), || {
                    Ok(reports.pop_front().expect("resume, not replay"))
                })
            },
            |report| observed.push(report.repaired),
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(resumed.delay, POLL_INTERVAL);
        assert!(!resumed.idle);
        assert_eq!(observed, [4, 1]);
        assert!(reports.is_empty());
    }

    #[test]
    fn pressure_does_not_swallow_other_failures_or_override_shutdown() {
        let other = format!("{DISK_PRESSURE_REFUSAL}: unexpected source error");
        let result = run_with(
            || Err(other.clone()),
            |_| panic!("failed source has no report"),
            || Duration::ZERO,
            || false,
        );
        assert_eq!(result.unwrap_err(), other);
        let stopped = run_with(
            || panic!("shutdown precedes pressure and source admission"),
            |_| panic!("no report"),
            || Duration::ZERO,
            || true,
        )
        .unwrap();
        assert_eq!(stopped.delay, POLL_INTERVAL);
        assert!(!stopped.idle);
    }

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
            || {
                Ok(reports
                    .pop_front()
                    .expect("empty completed round must yield"))
            },
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
        for refusal in [ADMISSION_REFUSAL, CHECKPOINT_ADMISSION_REFUSAL] {
            let mut calls = 0;
            let turn = run_with(
                || {
                    calls += 1;
                    Err(refusal.to_string())
                },
                |_| panic!("refusal cannot produce a storage report"),
                || Duration::ZERO,
                || false,
            )
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(turn, pending());
        }
        for error in [
            "active reservation source changed after selection; rescan required",
            "active reservation repair refused: corruption breaker open",
            "permission denied",
            "source error: active reservation repair deferred: recovery promotion or admission contention",
            "source error: active reservation repair deferred: checkpoint is owned by another worker",
            "active reservation checkpoint: permission denied",
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
    fn checkpoint_contention_after_progress_preserves_reports_and_respects_shutdown() {
        for cancel in [false, true] {
            let stop = Cell::new(false);
            let mut calls = 0;
            let mut observed = Vec::new();
            let turn = run_with(
                || {
                    calls += 1;
                    if calls == 1 {
                        return Ok(busy());
                    }
                    stop.set(cancel);
                    Err(CHECKPOINT_ADMISSION_REFUSAL.to_string())
                },
                |report| observed.push(report.repaired),
                || Duration::ZERO,
                || stop.get(),
            )
            .unwrap();
            assert_eq!(calls, 2);
            assert_eq!(observed, [4]);
            assert_eq!(turn, if cancel { stopped() } else { pending() });
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
                        let error = if admission {
                            ADMISSION_REFUSAL
                        } else {
                            "source changed"
                        };
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
