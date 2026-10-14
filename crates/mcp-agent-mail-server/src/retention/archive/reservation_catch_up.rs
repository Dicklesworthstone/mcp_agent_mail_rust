//! Finite catch-up turns for terminal reservation releases (br-kp1in.19).
//!
//! Retaining a contended ID is insufficient if the maintenance worker ignores
//! that debt and sleeps a minute. Keep pending retries on the short worker
//! cadence, and drain successful release batches without an intervening sleep.
//! The storage reconciler still owns all selection, cooldown, publication and
//! generation checks. This scheduler never writes a lease or changes its state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_storage::recovery::reservation_reconcile::{
    ReservationReconcileCursor, ReservationReconcileReport, reconcile_reservation_releases,
};

const MAX_BATCHES_PER_TURN: usize = 8;
const MAX_TURN_ACTIVE_TIME: Duration = Duration::from_millis(250);

/// Run existing bounded storage passes, retaining their individual reports.
/// Each pass still selects at most 32 IDs and attempts at most four releases.
/// The time target is cooperative: an individual DB/filesystem/Git operation
/// can exceed it, but another batch is not admitted after it has elapsed.
/// The returned hint controls only the worker pause, never retention/reporting.
pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReservationReconcileCursor,
    stop: &AtomicBool,
    observe: impl FnMut(&ReservationReconcileReport),
) -> Result<bool, String> {
    let started = Instant::now();
    run_with(
        || reconcile_reservation_releases(cx, pool, config, cursor, stop),
        observe,
        || started.elapsed(),
        || stop.load(Ordering::Acquire) || cx.checkpoint().is_err(),
    )
}

fn needs_follow_up(report: &ReservationReconcileReport) -> bool {
    !report.interrupted
        && (report.budget_exhausted || report.retry_len > 0 || report.deferred_transient > 0)
}

fn run_with(
    mut repair: impl FnMut() -> Result<ReservationReconcileReport, String>,
    mut observe: impl FnMut(&ReservationReconcileReport),
    mut elapsed: impl FnMut() -> Duration,
    mut cancelled: impl FnMut() -> bool,
) -> Result<bool, String> {
    for batches in 1..=MAX_BATCHES_PER_TURN {
        if cancelled() {
            return Ok(false);
        }
        if batches > 1 && elapsed() >= MAX_TURN_ACTIVE_TIME {
            return Ok(true);
        }
        let report = match repair() {
            Ok(report) => report,
            Err(error)
                if error
                    == "reservation reconciliation deferred: recovery promotion or admission contention" =>
            {
                // No source read or release was admitted. Retain the healthy
                // pool/cursor and yield to the short worker pause, rather than
                // sending this refusal through the pool-retirement path. An
                // observed generation/source failure remains an error below.
                if cancelled() {
                    return Ok(false);
                }
                tracing::warn!(
                    target: "maintenance",
                    event = "reservation_archive_reconcile_admission_deferred",
                    reason = %error,
                    "reservation archive admission busy; retaining source pool and retrying next short cycle"
                );
                return Ok(true);
            }
            Err(error) => return Err(error),
        };
        // Health/progress observations must describe each actual storage pass,
        // not an aggregate that appears to exceed storage's per-pass bounds.
        // Include observer I/O in the cooperative active-time target.
        observe(&report);
        if report.interrupted || cancelled() {
            return Ok(false);
        }
        let backlog = needs_follow_up(&report);
        if !backlog
            || report.repaired == 0
            || report.deferred > 0
            || report.deferred_transient > 0
            || batches == MAX_BATCHES_PER_TURN
            || elapsed() >= MAX_TURN_ACTIVE_TIME
        {
            return Ok(backlog);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    fn busy_report() -> ReservationReconcileReport {
        ReservationReconcileReport {
            scanned: 4,
            attempted: 4,
            repaired: 4,
            budget_exhausted: true,
            ..Default::default()
        }
    }

    #[test]
    fn successful_batches_drain_without_sleep_and_keep_individual_reports() {
        let mut reports = VecDeque::from([
            busy_report(),
            busy_report(),
            ReservationReconcileReport {
                scanned: 1,
                attempted: 1,
                repaired: 1,
                ..Default::default()
            },
        ]);
        let mut observed = Vec::new();
        let backlog = run_with(
            || Ok(reports.pop_front().expect("finished work must yield")),
            |report| observed.push((report.scanned, report.attempted, report.repaired)),
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(observed, [(4, 4, 4), (4, 4, 4), (1, 1, 1)]);
        assert!(reports.is_empty());
        assert!(!backlog);
    }

    #[test]
    fn continuously_arriving_releases_yield_at_the_batch_bound() {
        let calls = Cell::new(0);
        let mut observed = 0;
        let backlog = run_with(
            || {
                calls.set(calls.get() + 1);
                Ok(busy_report())
            },
            |_| observed += 1,
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls.get(), MAX_BATCHES_PER_TURN);
        assert_eq!(observed, MAX_BATCHES_PER_TURN);
    }

    #[test]
    fn batch_and_observer_time_prevent_admission_of_another_batch() {
        for observer_is_slow in [false, true] {
            let time = Cell::new(Duration::ZERO);
            let calls = Cell::new(0);
            let backlog = run_with(
                || {
                    calls.set(calls.get() + 1);
                    if !observer_is_slow {
                        time.set(MAX_TURN_ACTIVE_TIME);
                    }
                    Ok(busy_report())
                },
                |_| {
                    if observer_is_slow {
                        time.set(MAX_TURN_ACTIVE_TIME);
                    }
                },
                || time.get(),
                || false,
            )
            .unwrap();
            assert!(backlog);
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn transient_contention_yields_but_requests_short_follow_up() {
        let mut calls = 0;
        let backlog = run_with(
            || {
                calls += 1;
                Ok(ReservationReconcileReport {
                    scanned: 5,
                    deferred: 1,
                    deferred_transient: 1,
                    retry_len: 1,
                    ..busy_report()
                })
            },
            |_| {},
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!(calls, 1, "never retry a contended archive inside the turn");
        assert!(backlog);
    }

    #[test]
    fn admission_refusal_yields_without_a_fake_report_and_keeps_short_polling() {
        let mut calls = 0;
        let mut observed = 0;
        let backlog = run_with(
            || {
                calls += 1;
                Err("reservation reconciliation deferred: recovery promotion or admission contention"
                    .to_string())
            },
            |_| observed += 1,
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert_eq!((calls, observed), (1, 0));
        let poll = super::super::maintenance_poll_interval(Duration::from_secs(3600), true);
        assert_eq!(
            super::super::next_poll_interval(poll, backlog),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn admission_after_progress_preserves_observations_and_recovers_next_turn() {
        let mut calls = 0;
        let mut observed = Vec::new();
        let backlog = run_with(
            || {
                calls += 1;
                if calls == 1 {
                    Ok(busy_report())
                } else {
                    Err("reservation reconciliation deferred: recovery promotion or admission contention"
                        .to_string())
                }
            },
            |report| observed.push(report.repaired),
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls, 2);
        assert_eq!(observed, [4]);
        assert!(!run_with(
            || Ok(ReservationReconcileReport::default()),
            |_| {},
            || Duration::ZERO,
            || false,
        )
        .unwrap());
    }

    #[test]
    fn cancellation_during_admission_refusal_does_not_schedule_another_turn() {
        let stop = Cell::new(false);
        let backlog = run_with(
            || {
                stop.set(true);
                Err("reservation reconciliation deferred: recovery promotion or admission contention"
                    .to_string())
            },
            |_| panic!("refusal must not fabricate a report"),
            || Duration::ZERO,
            || stop.get(),
        )
        .unwrap();
        assert!(!backlog);
    }

    #[test]
    fn admission_classifier_does_not_hide_unrelated_source_failures() {
        for error in [
            "source generation changed",
            "source corruption breaker is open",
            "permission denied",
            "source error: reservation reconciliation deferred: recovery promotion or admission contention",
        ] {
            let result = run_with(
                || Err(error.to_string()),
                |_| panic!("failed storage has no report"),
                || Duration::ZERO,
                || false,
            );
            assert_eq!(result.unwrap_err(), error);
        }
    }

    #[test]
    fn cooldown_debt_requests_polling_without_spinning_or_fabricating_progress() {
        let mut calls = 0;
        let mut observed = 0;
        let backlog = run_with(
            || {
                calls += 1;
                Ok(ReservationReconcileReport {
                    retry_len: 7,
                    ..Default::default()
                })
            },
            |report| {
                observed += 1;
                assert_eq!((report.scanned, report.repaired), (0, 0));
            },
            || Duration::ZERO,
            || false,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!((calls, observed), (1, 1));
    }

    #[test]
    fn healthy_or_conflicting_rounds_without_retry_debt_return_to_idle() {
        for conflict in [false, true] {
            let mut calls = 0;
            let backlog = run_with(
                || {
                    calls += 1;
                    Ok(ReservationReconcileReport {
                        scanned: 16,
                        unchanged: if conflict { 0 } else { 16 },
                        deferred: if conflict { 16 } else { 0 },
                        ..Default::default()
                    })
                },
                |_| {},
                || Duration::ZERO,
                || false,
            )
            .unwrap();
            assert_eq!(calls, 1);
            assert!(!backlog);
        }
    }

    #[test]
    fn stop_before_admission_never_reads_or_publishes() {
        assert!(!run_with(
            || panic!("stopped turn must not invoke storage"),
            |_| panic!("no invented report"),
            || Duration::ZERO,
            || true,
        )
        .unwrap());
    }

    #[test]
    fn shutdown_after_completed_batch_preserves_report_and_skips_next_batch() {
        let stop = Cell::new(false);
        let mut calls = 0;
        let mut observed = Vec::new();
        let backlog = run_with(
            || {
                calls += 1;
                Ok(busy_report())
            },
            |report| {
                observed.push(report.repaired);
                stop.set(true);
            },
            || Duration::ZERO,
            || stop.get(),
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(observed, [4]);
        assert!(!backlog);
        assert!(!needs_follow_up(&ReservationReconcileReport {
            interrupted: true,
            retry_len: 7,
            deferred_transient: 1,
            ..busy_report()
        }));
    }

    #[test]
    fn source_failure_stops_turn_without_discarding_previous_observations() {
        let mut calls = 0;
        let mut observed = Vec::new();
        let error = run_with(
            || {
                calls += 1;
                if calls == 1 {
                    Ok(busy_report())
                } else {
                    Err("source generation changed".to_string())
                }
            },
            |report| observed.push(report.repaired),
            || Duration::ZERO,
            || false,
        )
        .unwrap_err();
        assert_eq!(error, "source generation changed");
        assert_eq!(calls, 2);
        assert_eq!(observed, [4]);
    }

    #[test]
    fn reservation_only_backlog_uses_existing_worker_pause_not_report_schedule() {
        let report = ReservationReconcileReport {
            retry_len: 1,
            ..Default::default()
        };
        let backlog = needs_follow_up(&report);
        let poll = super::super::maintenance_poll_interval(Duration::from_secs(3600), true);
        assert_eq!(
            super::super::next_poll_interval(poll, backlog),
            Duration::from_secs(1)
        );
        assert_eq!(
            super::super::next_poll_interval(poll, false),
            Duration::from_secs(60)
        );
        assert!(!super::super::report_is_due(
            Some(Duration::from_secs(1)),
            Duration::from_secs(3600)
        ));
    }
}
