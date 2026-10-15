//! Finite catch-up turns for terminal reservation releases (br-kp1in.19).
//!
//! Retaining a contended ID is insufficient if the maintenance worker ignores
//! that debt and sleeps a minute. Keep pending retries on the short worker
//! cadence, and drain successful releases or healthy unfinished history pages
//! without an intervening sleep. Reaching a finite round's end returns to idle.
//! The storage reconciler checkpoints scan positions and retry debt together.
//! It still owns all selection, cooldown, publication and generation checks.
//! This scheduler never writes a lease or changes its state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_storage::recovery::reservation_reconcile::{
    ReservationReconcileCursor, ReservationReconcileReport,
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
        || cursor.reconcile_checkpointed(cx, pool, config, stop),
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
                if matches!(
                    error.as_str(),
                    "reservation reconciliation deferred: recovery promotion or admission contention"
                        | "reservation reconciliation deferred: checkpoint owned by another worker"
                ) =>
            {
                // No reservation scan or release was admitted. Retain the
                // healthy pool/cursor and yield to the short worker pause,
                // rather than retiring the pool or discarding checkpoint
                // ownership. Source/generation failures remain errors below.
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
        // A healthy page can advance towards a missing release without
        // publishing anything itself. Storage alone decides whether its
        // captured round remains unfinished; an empty or completed round
        // must never be turned into speculative work by this scheduler.
        let progressed = report.repaired > 0 || report.unchanged > 0;
        if !backlog
            || !progressed
            || report.deferred > 0
            || report.deferred_transient > 0
            || report.retry_len > 0
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

    fn healthy_page() -> ReservationReconcileReport {
        ReservationReconcileReport {
            scanned: 32,
            unchanged: 32,
            budget_exhausted: true,
            ..Default::default()
        }
    }

    #[test]
    fn healthy_pages_reach_missing_releases_in_the_same_bounded_turn() {
        let mut reports = VecDeque::from([
            healthy_page(),
            healthy_page(),
            ReservationReconcileReport {
                scanned: 18,
                unchanged: 17,
                attempted: 1,
                repaired: 1,
                ..Default::default()
            },
        ]);
        let mut observed = Vec::new();
        assert!(!run_with(
            || Ok(reports.pop_front().expect("a completed round must yield")),
            |report| observed.push((report.scanned, report.unchanged, report.repaired)),
            || Duration::ZERO,
            || false,
        )
        .unwrap());
        assert_eq!(observed, [(32, 32, 0), (32, 32, 0), (18, 17, 1)]);
        assert!(reports.is_empty());
    }

    #[test]
    fn healthy_discovery_obeys_batch_and_active_time_bounds() {
        let mut observed = 0;
        assert!(run_with(
            || Ok(healthy_page()),
            |_| observed += 1,
            || Duration::ZERO,
            || false,
        )
        .unwrap());
        assert_eq!(observed, MAX_BATCHES_PER_TURN);
        for observer_is_slow in [false, true] {
            let time = Cell::new(Duration::ZERO);
            let calls = Cell::new(0);
            assert!(run_with(
                || {
                    calls.set(calls.get() + 1);
                    if !observer_is_slow {
                        time.set(MAX_TURN_ACTIVE_TIME);
                    }
                    Ok(healthy_page())
                },
                |_| {
                    if observer_is_slow {
                        time.set(MAX_TURN_ACTIVE_TIME);
                    }
                },
                || time.get(),
                || false,
            )
            .unwrap());
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn healthy_work_cannot_spin_on_cooling_debt_or_ignore_cancellation() {
        let mut calls = 0;
        assert!(run_with(
            || {
                calls += 1;
                Ok(ReservationReconcileReport {
                    retry_len: 1,
                    ..healthy_page()
                })
            },
            |_| {},
            || Duration::ZERO,
            || false,
        )
        .unwrap());
        assert_eq!(calls, 1);
        let stop = Cell::new(false);
        let mut observed = 0;
        assert!(!run_with(
            || Ok(healthy_page()),
            |_| {
                observed += 1;
                stop.set(true);
            },
            || Duration::ZERO,
            || stop.get(),
        )
        .unwrap());
        assert_eq!(observed, 1);
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
    fn checkpoint_owner_refusal_preserves_completed_reports_and_yields() {
        for completed_first in [false, true] {
            let mut calls = 0;
            let mut observed = Vec::new();
            let backlog = run_with(
                || {
                    calls += 1;
                    if completed_first && calls == 1 {
                        Ok(healthy_page())
                    } else {
                        Err("reservation reconciliation deferred: checkpoint owned by another worker".into())
                    }
                },
                |report| observed.push(report.scanned),
                || Duration::ZERO,
                || false,
            ).unwrap();
            assert_eq!(calls, if completed_first { 2 } else { 1 });
            assert_eq!(observed, if completed_first { vec![32] } else { vec![] });
            assert!(backlog);
            assert_eq!(
                super::super::next_poll_interval(Duration::from_secs(60), backlog),
                Duration::from_secs(1)
            );
        }
    }

    #[test]
    fn cancellation_during_checkpoint_owner_refusal_does_not_rearm_worker() {
        let stop = Cell::new(false);
        let mut observed = 0;
        let backlog = run_with(
            || {
                stop.set(true);
                Err("reservation reconciliation deferred: checkpoint owned by another worker".into())
            },
            |_| observed += 1,
            || Duration::ZERO,
            || stop.get(),
        ).unwrap();
        assert!(!backlog);
        assert_eq!(observed, 0);
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
            "source error: reservation reconciliation deferred: checkpoint owned by another worker",
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

    fn isolated() -> bool {
        use std::io::{Read, Seek, SeekFrom};

        const CHILD: &str = "AM_TEST_RESERVATION_DISCOVERY_CHILD";
        let thread = std::thread::current();
        let name = thread.name().expect("named libtest thread");
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let mut log = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .expect("spawn isolated reservation discovery test");
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut expired = false;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                expired = true;
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait().unwrap();
        log.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        log.read_to_string(&mut output).unwrap();
        assert!(
            !expired && status.success() && output.contains("1 passed; 0 failed"),
            "isolated {name} failed (timeout={expired}): {output}"
        );
        true
    }

    #[test]
    fn worker_discovers_and_commits_a_release_behind_healthy_live_rows() {
        // The production publication fence is global. Use a separate process
        // rather than weakening assertions when unrelated libtests hold it.
        if isolated() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &temp.path().join("mail.sqlite3"),
            ),
            storage_root: temp.path().join("archive"),
            ..Default::default()
        };
        std::fs::create_dir_all(&config.storage_root).unwrap();
        let pool = mcp_agent_mail_db::create_pool(&mcp_agent_mail_db::DbPoolConfig {
            database_url: config.database_url.clone(),
            storage_root: Some(config.storage_root.clone()),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        })
        .unwrap();
        let cx = Cx::for_testing();
        let conn = fastmcp_core::block_on(pool.acquire(&cx)).into_result().unwrap();
        conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'project', '/project', 1)").unwrap();
        conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
        conn.execute_raw("INSERT OR REPLACE INTO db_identity(singleton, generation_id) VALUES(0, 'aabb')").unwrap();
        for id in 401..483 {
            conn.execute_raw(&format!(
                "INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'worker/{id}.rs', 1, 'unchanged', 1000000, 9000000, NULL)"
            )).unwrap();
        }
        // Only the durable release ledger changes. Recovery must not rewrite
        // the hot row or manufacture active artifacts for the preceding rows.
        conn.execute_raw("INSERT INTO file_reservation_releases(reservation_id, released_ts) VALUES(482, 5000000)").unwrap();
        drop(conn);
        let mut cursor = ReservationReconcileCursor::default();
        let stop = AtomicBool::new(false);
        let mut observed = Vec::new();
        let mut completed = false;
        // A real filesystem operation may exhaust a turn's time target. Three
        // finite pages fit in at most three turns without altering that target.
        for _ in 0..3 {
            let backlog = run(&cx, &pool, &config, &mut cursor, &stop, |report| {
                assert!(report.scanned <= 32);
                assert!(report.attempted <= 4);
                assert_eq!((report.deferred, report.retry_len), (0, 0));
                observed.push((report.scanned, report.unchanged, report.repaired));
            })
            .unwrap();
            if !backlog {
                completed = true;
                break;
            }
        }
        assert!(completed, "finite discovery never returned to idle");
        assert_eq!(observed, [(32, 32, 0), (32, 32, 0), (18, 17, 1)]);
        let relative = "projects/project/file_reservations/id-482-gaabb.json";
        let bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
        let artifact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(artifact["id"], 482);
        assert_eq!(artifact["db_generation"], "aabb");
        assert_eq!(artifact["path_pattern"], "worker/482.rs");
        assert_eq!(
            mcp_agent_mail_core::iso_to_micros(artifact["released_ts"].as_str().unwrap()),
            Some(5_000_000),
        );
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(std::path::Path::new(relative)).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        assert!(!config.storage_root.join("projects/project/file_reservations/id-401-gaabb.json").exists());
        let conn = fastmcp_core::block_on(pool.acquire(&cx)).into_result().unwrap();
        assert_eq!(
            conn.query_sync("SELECT COUNT(*) FROM file_reservations WHERE released_ts IS NULL", &[])
                .unwrap()[0]
                .get_as::<i64>(0)
                .unwrap(),
            82,
        );
        assert_eq!(
            conn.query_sync("SELECT released_ts FROM file_reservation_releases WHERE reservation_id=482", &[])
                .unwrap()[0]
                .get_as::<i64>(0)
                .unwrap(),
            5_000_000,
        );
    }
}
