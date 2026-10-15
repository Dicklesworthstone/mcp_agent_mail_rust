//! Bounded message-repair turns above the existing four-repair storage batches.
//!
//! A busy mailbox must not pay a one-second sleep after every four recovered
//! messages or every healthy page hiding an older crash orphan. Each batch still
//! owns its source checks, retry fairness, payload limit and durable checkpoint.
//! This scheduler removes sleeps only while bounded repair or scan progress is
//! reported. Storage ends startup catch-up after the current history round.

#[cfg(test)]
mod integration_tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_storage::recovery::message_reconcile::database::{
    ReconcileCursor, ReconcileReport, reconcile_message_batch,
};

const MAX_BATCHES_PER_TURN: usize = 8;
const MAX_TURN_ACTIVE_TIME: Duration = Duration::from_millis(250);
// Admission target, checked BETWEEN batches. A final batch retains storage's
// independent 16 MiB bound, so a turn admits less than 32 MiB in total. This is
// not a resident-memory allocation or permission to enlarge a storage batch.
const TURN_PAYLOAD_TARGET: usize = 16 * 1024 * 1024;
const DISK_PRESSURE_DEFERRED: &str =
    "message reconciliation deferred: critical disk pressure";

/// Run one finite repair turn, observing each real storage report separately.
/// The returned backlog hint controls only the worker's next sleep, never
/// reporting/pruning cadence or authority to publish an archive artifact.
/// The time target is cooperative: a single filesystem/SQL/Git call can exceed
/// it, but no further batch is started once it has elapsed.
pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut ReconcileCursor,
    stop: &AtomicBool,
    observe: impl FnMut(&ReconcileReport),
) -> Result<bool, String> {
    let started = Instant::now();
    let result = run_with(
        || {
            with_disk_admission(
                mcp_agent_mail_core::global_metrics()
                    .system
                    .disk_pressure_level
                    .load(),
                || reconcile_message_batch(cx, pool, config, cursor, stop),
            )
        },
        observe,
        || started.elapsed(),
    );
    finish_turn(result, || {
        stop.load(Ordering::Acquire) || cx.checkpoint().is_err()
    })
}

fn with_disk_admission(
    pressure: u64,
    repair: impl FnOnce() -> Result<ReconcileReport, String>,
) -> Result<ReconcileReport, String> {
    if pressure >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64() {
        // Check before the checkpointed entrypoint: even an unchanged pass
        // writes/fsyncs scheduling hints beside the database. On a critically
        // full disk, preserve the cursor and source rather than competing for
        // the remaining space with the mailbox's foreground recovery path.
        return Err(DISK_PRESSURE_DEFERRED.into());
    }
    repair()
}

/// Admission refusal is not a broken source pool. Let the worker retain its
/// pool and cursor and use the ordinary one-second follow-up instead of
/// retiring the pool and sleeping for the full idle interval. Do not retry
/// inside this turn, synthesize a successful storage report, or hide source,
/// generation, corruption, or checkpoint-I/O failures behind this hint.
fn finish_turn(
    result: Result<bool, String>,
    interrupted: impl FnOnce() -> bool,
) -> Result<bool, String> {
    match result {
        Err(error) if error == DISK_PRESSURE_DEFERRED => {
            // Resource pressure is neither a completed pass nor a bad source.
            // Retain the live pool/cursor and use the normal maintenance poll;
            // do not manufacture a report or create a one-second retry loop
            // while the filesystem is still critically full.
            Ok(false)
        }
        Err(error)
            if matches!(
                error.as_str(),
                "message reconciliation deferred: recovery promotion or admission contention"
                    | "message reconciliation deferred: checkpoint is owned by another worker"
            ) =>
        {
            if interrupted() {
                return Ok(false);
            }
            tracing::warn!(
                target: "maintenance",
                event = "message_archive_reconcile_admission_deferred",
                reason = %error,
                "message archive admission busy; retaining source pool and retrying next short cycle"
            );
            Ok(true)
        }
        outcome => outcome,
    }
}

fn needs_follow_up(report: &ReconcileReport) -> bool {
    // A deferred ID can be the only item in a pass: no work budget need be
    // exhausted. Sleeping a full minute in that case defeats the retry lane's
    // immediate first retry. Keep polling, but do not force another same-turn
    // batch or bypass storage's per-ID backoff. Conflict-only scans stay idle.
    if report.interrupted {
        return false;
    }
    report.budget_exhausted || report.retry_len > 0 || report.deferred_transient > 0
}

fn continue_turn(
    batches: usize,
    payload_bytes: usize,
    elapsed: Duration,
    report: &ReconcileReport,
) -> bool {
    // A healthy page advances the history cursor even when nothing needed a
    // repair. Requiring repaired > 0 would repeatedly sleep before reaching
    // old orphans. Only count a fully non-deferred scan as that progress;
    // errors, conflict-only pages and empty/cooldown-only passes still yield.
    let progressed = report.repaired > 0 || (report.unchanged > 0 && report.deferred == 0);
    batches < MAX_BATCHES_PER_TURN
        && payload_bytes < TURN_PAYLOAD_TARGET
        && elapsed < MAX_TURN_ACTIVE_TIME
        && report.budget_exhausted
        && progressed
        && report.deferred_transient == 0
        && !report.interrupted
}

fn run_with(
    mut repair: impl FnMut() -> Result<ReconcileReport, String>,
    mut observe: impl FnMut(&ReconcileReport),
    mut elapsed: impl FnMut() -> Duration,
) -> Result<bool, String> {
    let mut payload_bytes = 0usize;
    for batches in 1..=MAX_BATCHES_PER_TURN {
        let report = repair()?;
        payload_bytes = payload_bytes.saturating_add(report.payload_bytes);
        // Preserve the existing per-batch diagnostics and checkpoint counts;
        // aggregating them into a single ReconcileReport would misrepresent
        // storage's per-pass limits. Observer time counts against this turn.
        observe(&report);
        if !continue_turn(batches, payload_bytes, elapsed(), &report) {
            return Ok(needs_follow_up(&report));
        }
    }
    // The finite batch bound above always ends the turn on its final report.
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    fn busy_report() -> ReconcileReport {
        ReconcileReport {
            scanned: 4,
            repaired: 4,
            budget_exhausted: true,
            ..ReconcileReport::default()
        }
    }

    #[test]
    fn disk_admission_stops_before_checkpoint_or_archive_work() {
        let critical = mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64();
        for pressure in [critical, critical.saturating_add(1), u64::MAX] {
            assert_eq!(
                with_disk_admission(pressure, || {
                    panic!("critical pressure must not enter message reconciliation")
                })
                .unwrap_err(),
                DISK_PRESSURE_DEFERRED,
            );
        }
        assert!(critical > 0);
        for pressure in [0, critical - 1] {
            assert_eq!(
                with_disk_admission(pressure, || Ok(busy_report()))
                    .unwrap()
                    .repaired,
                4,
            );
        }
    }

    #[test]
    fn critical_pressure_after_progress_yields_without_fabricating_a_pass() {
        let critical = mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64();
        let pressure = Cell::new(0_u64);
        let calls = Cell::new(0_usize);
        let mut observed = Vec::new();
        let result = run_with(
            || {
                with_disk_admission(pressure.get(), || {
                    calls.set(calls.get() + 1);
                    Ok(busy_report())
                })
            },
            |report| {
                observed.push(report.repaired);
                pressure.set(critical);
            },
            || Duration::ZERO,
        );
        assert!(!finish_turn(result, || false).unwrap());
        assert_eq!(calls.get(), 1);
        assert_eq!(observed, [4]);
        pressure.set(0);
        let resumed = run_with(
            || {
                with_disk_admission(pressure.get(), || {
                    calls.set(calls.get() + 1);
                    Ok(ReconcileReport {
                        scanned: 1,
                        repaired: 1,
                        ..ReconcileReport::default()
                    })
                })
            },
            |_| {},
            || Duration::ZERO,
        );
        assert!(!finish_turn(resumed, || false).unwrap());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn drains_multiple_successful_batches_before_sleeping() {
        let mut reports = VecDeque::from([
            busy_report(),
            ReconcileReport {
                scanned: 2,
                repaired: 2,
                ..ReconcileReport::default()
            },
        ]);
        let mut observed = Vec::new();
        let backlog = run_with(
            || Ok(reports.pop_front().expect("unexpected extra batch")),
            |report| observed.push(report.repaired),
            || Duration::ZERO,
        )
        .unwrap();
        assert_eq!(observed, [4, 2]);
        assert!(!backlog);
        assert!(reports.is_empty(), "both repair batches must execute");
    }

    #[test]
    fn continuously_growing_backlog_yields_at_the_batch_bound() {
        let calls = Cell::new(0);
        let mut observed = 0;
        let backlog = run_with(
            || {
                calls.set(calls.get() + 1);
                Ok(busy_report())
            },
            |_| observed += 1,
            || Duration::ZERO,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls.get(), MAX_BATCHES_PER_TURN);
        assert_eq!(observed, MAX_BATCHES_PER_TURN);
    }

    #[test]
    fn observer_and_batch_time_are_included_before_another_batch() {
        let elapsed = Cell::new(Duration::ZERO);
        let calls = Cell::new(0);
        let backlog = run_with(
            || {
                calls.set(calls.get() + 1);
                Ok(busy_report())
            },
            |_| elapsed.set(MAX_TURN_ACTIVE_TIME),
            || elapsed.get(),
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn large_payloads_yield_between_original_bounded_batches() {
        let mut calls = 0;
        let backlog = run_with(
            || {
                calls += 1;
                let mut report = busy_report();
                report.payload_bytes = TURN_PAYLOAD_TARGET / 2;
                Ok(report)
            },
            |_| {},
            || Duration::ZERO,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls, 2);
        assert!(!continue_turn(
            1,
            usize::MAX,
            Duration::ZERO,
            &busy_report()
        ));
    }

    #[test]
    fn interruption_contention_and_no_scan_progress_never_spin_in_a_turn() {
        for mode in ["interrupted", "contended", "conflict_only", "empty", "complete"] {
            let mut calls = 0;
            run_with(
                || {
                    calls += 1;
                    let mut report = busy_report();
                    match mode {
                        "interrupted" => report.interrupted = true,
                        "contended" => report.deferred_transient = 1,
                        "conflict_only" => {
                            report.repaired = 0;
                            report.deferred = report.scanned;
                            report.deferred_conflict = report.scanned;
                        }
                        "empty" => {
                            report.repaired = 0;
                            report.scanned = 0;
                        }
                        _ => report.budget_exhausted = false,
                    }
                    Ok(report)
                },
                |_| {},
                || Duration::ZERO,
            )
            .unwrap();
            assert_eq!(calls, 1, "{mode}");
        }
    }

    #[test]
    fn source_errors_stop_the_turn_without_hiding_prior_reports() {
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
        )
        .unwrap_err();
        assert_eq!(error, "source generation changed");
        assert_eq!(calls, 2);
        assert_eq!(observed, [4]);
    }

    #[test]
    fn admission_refusal_preserves_follow_up_without_fabricating_reports() {
        for error in [
            "message reconciliation deferred: recovery promotion or admission contention",
            "message reconciliation deferred: checkpoint is owned by another worker",
        ] {
            let mut calls = 0;
            let mut observed = 0;
            let result = run_with(
                || {
                    calls += 1;
                    Err(error.to_string())
                },
                |_| observed += 1,
                || Duration::ZERO,
            );
            assert!(finish_turn(result, || false).unwrap());
            assert_eq!(calls, 1, "admission must not be retried inside a turn");
            assert_eq!(observed, 0, "a refusal is not a completed storage batch");
            assert!(!finish_turn(Err(error.to_string()), || true).unwrap());
        }
    }

    #[test]
    fn admission_after_progress_preserves_reports_and_next_turn_recovers() {
        let mut calls = 0;
        let mut observed = Vec::new();
        let result = run_with(
            || {
                calls += 1;
                if calls == 1 {
                    Ok(busy_report())
                } else {
                    Err("message reconciliation deferred: checkpoint is owned by another worker"
                        .to_string())
                }
            },
            |report| observed.push(report.repaired),
            || Duration::ZERO,
        );
        assert!(finish_turn(result, || false).unwrap());
        assert_eq!(calls, 2);
        assert_eq!(observed, [4]);
        let recovered = run_with(
            || Ok(ReconcileReport::default()),
            |_| {},
            || Duration::ZERO,
        );
        assert!(!finish_turn(recovered, || false).unwrap());
    }

    #[test]
    fn only_exact_admission_refusals_are_rescheduled() {
        for error in [
            "message reconciliation source changed after selection; rescan required",
            "message reconciliation refused: source corruption breaker is open",
            "permission denied",
            "checkpoint source changed during reconciliation",
            "source error: message reconciliation deferred: recovery promotion or admission contention",
            "source error: message reconciliation deferred: critical disk pressure",
        ] {
            let result = finish_turn(Err(error.to_string()), || {
                panic!("source failures must not be mistaken for admission contention")
            });
            assert_eq!(result.unwrap_err(), error);
        }
        for backlog in [false, true] {
            assert_eq!(
                finish_turn(Ok(backlog), || panic!("successful turn needs no refusal check")),
                Ok(backlog)
            );
        }
    }

    #[test]
    fn cooperative_targets_do_not_expand_a_storage_batch() {
        let report = busy_report();
        assert!(continue_turn(1, 0, Duration::ZERO, &report));
        assert!(!continue_turn(MAX_BATCHES_PER_TURN, 0, Duration::ZERO, &report));
        assert!(!continue_turn(1, TURN_PAYLOAD_TARGET, Duration::ZERO, &report));
        assert!(!continue_turn(1, 0, MAX_TURN_ACTIVE_TIME, &report));
    }

    fn healthy_page() -> ReconcileReport {
        ReconcileReport {
            scanned: 16,
            unchanged: 16,
            budget_exhausted: true,
            ..ReconcileReport::default()
        }
    }

    #[test]
    fn healthy_startup_pages_reach_older_repairs_and_yield_when_the_round_finishes() {
        let mut reports = VecDeque::from([
            healthy_page(),
            healthy_page(),
            ReconcileReport {
                scanned: 8,
                unchanged: 7,
                repaired: 1,
                budget_exhausted: true,
                ..ReconcileReport::default()
            },
            ReconcileReport::default(),
        ]);
        let mut observed = Vec::new();
        let backlog = run_with(
            || Ok(reports.pop_front().expect("finished round must yield")),
            |report| observed.push((report.scanned, report.repaired)),
            || Duration::ZERO,
        )
        .unwrap();
        assert_eq!(observed, [(16, 0), (16, 0), (8, 1), (0, 0)]);
        assert!(reports.is_empty());
        assert!(!backlog);
        let mut idle = healthy_page();
        idle.budget_exhausted = false;
        assert!(!continue_turn(1, 0, Duration::ZERO, &idle));
        let mut conflicting = healthy_page();
        conflicting.deferred = 1;
        conflicting.deferred_conflict = 1;
        assert!(!continue_turn(1, 0, Duration::ZERO, &conflicting));
    }

    #[test]
    fn healthy_scan_progress_obeys_the_same_time_payload_and_batch_targets() {
        let calls = Cell::new(0);
        let mut observed = 0;
        let backlog = run_with(
            || {
                calls.set(calls.get() + 1);
                Ok(healthy_page())
            },
            |_| observed += 1,
            || Duration::ZERO,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls.get(), MAX_BATCHES_PER_TURN);
        assert_eq!(observed, MAX_BATCHES_PER_TURN);
        let report = healthy_page();
        assert!(!continue_turn(1, TURN_PAYLOAD_TARGET, Duration::ZERO, &report));
        assert!(!continue_turn(1, 0, MAX_TURN_ACTIVE_TIME, &report));
        assert!(!continue_turn(MAX_BATCHES_PER_TURN, 0, Duration::ZERO, &report));
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn follow_up_distinguishes_retry_debt_from_conflicts_and_shutdown() {
        for (mode, expected) in [
            ("idle", false),
            ("budget", true),
            ("retained", true),
            ("transient", true),
            ("conflict", false),
            ("interrupted", false),
        ] {
            let mut report = ReconcileReport::default();
            match mode {
                "budget" => report.budget_exhausted = true,
                "retained" => report.retry_len = 1,
                "transient" => report.deferred_transient = 1,
                "conflict" => {
                    report.deferred = 1;
                    report.deferred_conflict = 1;
                }
                "interrupted" => {
                    report.interrupted = true;
                    report.budget_exhausted = true;
                    report.retry_len = 1;
                    report.deferred_transient = 1;
                }
                _ => {}
            }
            assert_eq!(needs_follow_up(&report), expected, "{mode}");
        }
    }

    #[test]
    fn one_deferred_orphan_requests_follow_up_without_a_same_turn_retry() {
        let mut calls = 0;
        let backlog = run_with(
            || {
                calls += 1;
                Ok(ReconcileReport {
                    scanned: 1,
                    deferred: 1,
                    deferred_transient: 1,
                    retry_len: 1,
                    ..ReconcileReport::default()
                })
            },
            |_| {},
            || Duration::ZERO,
        )
        .unwrap();
        assert!(backlog, "a non-exhausted pass still has known repair debt");
        assert_eq!(calls, 1, "retry backoff belongs to the next worker turn");
    }

    #[test]
    fn cooling_retry_ids_keep_follow_up_alive_without_hot_looping() {
        let mut calls = 0;
        let backlog = run_with(
            || {
                calls += 1;
                Ok(ReconcileReport {
                    retry_len: 3,
                    ..ReconcileReport::default()
                })
            },
            |_| {},
            || Duration::ZERO,
        )
        .unwrap();
        assert!(backlog, "zero scanned is not proof that retained debt vanished");
        assert_eq!(calls, 1);
    }

    #[test]
    fn recovered_last_retry_returns_to_steady_state() {
        let backlog = run_with(
            || {
                Ok(ReconcileReport {
                    scanned: 1,
                    repaired: 1,
                    ..ReconcileReport::default()
                })
            },
            |_| {},
            || Duration::ZERO,
        )
        .unwrap();
        assert!(!backlog);
    }

    #[test]
    fn productive_but_contended_batch_yields_with_retry_debt_visible() {
        let mut calls = 0;
        let backlog = run_with(
            || {
                calls += 1;
                Ok(ReconcileReport {
                    scanned: 5,
                    repaired: 4,
                    deferred: 1,
                    deferred_transient: 1,
                    retry_len: 1,
                    budget_exhausted: true,
                    ..ReconcileReport::default()
                })
            },
            |_| {},
            || Duration::ZERO,
        )
        .unwrap();
        assert!(backlog);
        assert_eq!(calls, 1, "do not hammer a contended publication fence");
    }
}
