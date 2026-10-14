//! Bounded message-repair turns above the existing four-repair storage batches.
//!
//! A busy mailbox must not pay a one-second sleep after every four recovered
//! messages or every healthy page hiding an older crash orphan. Each batch still
//! owns its source checks, retry fairness, payload limit and durable checkpoint.
//! This scheduler removes sleeps only while bounded repair or scan progress is
//! reported. Storage ends startup catch-up after the current history round.

#[cfg(test)]
mod integration_tests;

use std::sync::atomic::AtomicBool;
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
    run_with(
        || reconcile_message_batch(cx, pool, config, cursor, stop),
        observe,
        || started.elapsed(),
    )
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
