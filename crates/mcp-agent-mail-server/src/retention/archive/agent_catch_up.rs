//! Agent repair shares the message/reservation worker's source pool (GH#353).
//! Admission refusal is scheduling backpressure, not a failed source generation.
//! Productive finite scans catch up in bounded bursts; contention yields without
//! bypassing storage's per-ID backoff or delaying other maintenance phases.

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_storage::recovery::agent_reconcile::{
    AgentReconcileCursor, AgentReconcileReport, reconcile_agent_batch,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ADMISSION_DEFERRED: &str =
    "agent reconciliation deferred: recovery promotion or admission contention";
const MAX_BATCHES_PER_TURN: usize = 8;
const MAX_ACTIVE_PER_TURN: Duration = Duration::from_millis(250);

/// Run productive storage batches in a bounded turn. Every batch retains its
/// original four-repair/32-candidate bounds. The time target is checked between
/// batches, not a hard deadline on a filesystem, SQL, or Git operation.
///
/// A refusal neither reports success nor retries inside this turn. Only a real
/// source failure asks the owner to retire the shared message/release pool.
pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut AgentReconcileCursor,
    stop: &AtomicBool,
    observe: impl FnMut(&AgentReconcileReport),
) -> Result<bool, String> {
    let started = Instant::now();
    run_with(
        || reconcile_agent_batch(cx, pool, config, cursor, stop),
        || stop.load(Ordering::Acquire) || cx.checkpoint().is_err(),
        || started.elapsed(),
        observe,
    )
}

fn run_with(
    mut batch: impl FnMut() -> Result<AgentReconcileReport, String>,
    mut interrupted: impl FnMut() -> bool,
    mut elapsed: impl FnMut() -> Duration,
    mut observe: impl FnMut(&AgentReconcileReport),
) -> Result<bool, String> {
    let mut batches = 0;
    loop {
        if interrupted() {
            return Ok(false);
        }
        let mut continue_turn = false;
        let follow_up = finish(batch(), &mut interrupted, |report| {
            batches += 1;
            // Healthy pages matter as well as repairs: a lost retirement may
            // sit beyond thousands of already-correct profiles. A transient,
            // conflicting, or zero-progress pass always hands back the thread.
            continue_turn = !report.interrupted
                && report.budget_exhausted
                && report.deferred == 0
                && report.deferred_transient == 0
                && report.retry_len == 0
                && (report.repaired > 0 || report.unchanged > 0);
            observe(report);
        })?;
        if interrupted() || !follow_up {
            return Ok(false);
        }
        if !continue_turn
            || batches >= MAX_BATCHES_PER_TURN
            || elapsed() >= MAX_ACTIVE_PER_TURN
        {
            return Ok(true);
        }
    }
}

fn finish(
    result: Result<AgentReconcileReport, String>,
    interrupted: impl FnOnce() -> bool,
    observe: impl FnOnce(&AgentReconcileReport),
) -> Result<bool, String> {
    match result {
        Ok(report) => {
            // Preserve storage's four-repair/32-candidate bounds. Conflicts
            // alone must not turn into repeated whole-root scans every second.
            // Retained transient debt needs short polls even when no repair
            // budget was exhausted; storage still owns every per-ID cooldown.
            let follow_up = !report.interrupted
                && (report.budget_exhausted
                    || report.retry_len > 0
                    || report.deferred_transient > 0);
            observe(&report);
            Ok(follow_up)
        }
        Err(error) if error == ADMISSION_DEFERRED => {
            if interrupted() {
                return Ok(false);
            }
            tracing::warn!(
                target: "maintenance",
                event = "agent_archive_reconcile_admission_deferred",
                reason = %error,
                "agent archive admission busy; retaining shared source pool and cursor"
            );
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::time::{Duration, Instant};

    #[test]
    fn exact_admission_refusal_never_fabricates_a_storage_report() {
        let observed = Cell::new(false);
        assert!(
            finish(
                Err(ADMISSION_DEFERRED.into()),
                || false,
                |_| observed.set(true),
            )
            .unwrap()
        );
        assert!(!observed.get());
    }

    #[test]
    fn refusal_during_shutdown_does_not_rearm_the_worker() {
        assert!(
            !finish(
                Err(ADMISSION_DEFERRED.into()),
                || true,
                |_| panic!("refused work has no completed report"),
            )
            .unwrap()
        );
    }

    #[test]
    fn source_and_generation_errors_remain_errors() {
        for reason in [
            "agent reconciliation refused: source corruption breaker is open",
            "agent reconciliation pool is not the configured live database",
            "agent reconciliation source changed after selection; rescan required",
            "agent reconciliation source query failed: database pool is closed",
            "agent reconciliation deferred: recovery promotion or admission contention: unknown",
        ] {
            assert_eq!(
                finish(Err(reason.into()), || false, |_| panic!("no report")),
                Err(reason.into()),
            );
        }
    }

    #[test]
    fn productive_budget_exhaustion_follows_up_but_conflicts_stay_paced() {
        for (report, expected) in [
            (
                AgentReconcileReport {
                    repaired: 4,
                    budget_exhausted: true,
                    ..Default::default()
                },
                true,
            ),
            (
                AgentReconcileReport {
                    deferred: 32,
                    ..Default::default()
                },
                false,
            ),
            (
                AgentReconcileReport {
                    interrupted: true,
                    budget_exhausted: true,
                    ..Default::default()
                },
                false,
            ),
            (AgentReconcileReport::default(), false),
        ] {
            let observed = Cell::new(0);
            assert_eq!(
                finish(Ok(report), || false, |_| observed.set(observed.get() + 1)).unwrap(),
                expected,
            );
            assert_eq!(observed.get(), 1);
        }
    }

    #[test]
    fn retry_debt_follows_up_without_repair_budget_exhaustion() {
        for interrupted in [false, true] {
            for retained in [false, true] {
                let report = AgentReconcileReport {
                    deferred_transient: usize::from(!retained),
                    retry_len: usize::from(retained),
                    interrupted,
                    ..Default::default()
                };
                let observed = Cell::new(0);
                assert_eq!(
                    finish(Ok(report), || false, |_| observed.set(observed.get() + 1)).unwrap(),
                    !interrupted,
                );
                assert_eq!(observed.get(), 1);
            }
        }
    }

    #[test]
    fn healthy_roster_pages_continue_without_requiring_a_repair() {
        let mut calls = 0;
        let mut reports = Vec::new();
        let follow_up = run_with(
            || {
                calls += 1;
                Ok(match calls {
                    1 => AgentReconcileReport {
                        scanned: 32,
                        unchanged: 32,
                        budget_exhausted: true,
                        ..Default::default()
                    },
                    2 => AgentReconcileReport {
                        scanned: 1,
                        repaired: 1,
                        ..Default::default()
                    },
                    _ => panic!("a completed finite scan must end the turn"),
                })
            },
            || false,
            || Duration::ZERO,
            |report| reports.push((report.scanned, report.repaired)),
        )
        .unwrap();
        assert!(!follow_up);
        assert_eq!(calls, 2);
        assert_eq!(reports, [(32, 0), (1, 1)]);
    }

    #[test]
    fn catch_up_turn_has_independent_batch_and_elapsed_limits() {
        for (elapsed, expected_calls) in [(Duration::ZERO, 8), (Duration::from_millis(250), 1)] {
            let mut calls = 0;
            let mut observed = 0;
            assert!(
                run_with(
                    || {
                        calls += 1;
                        Ok(AgentReconcileReport {
                            scanned: 4,
                            repaired: 4,
                            budget_exhausted: true,
                            ..Default::default()
                        })
                    },
                    || false,
                    || elapsed,
                    |_| observed += 1,
                )
                .unwrap()
            );
            assert_eq!(calls, expected_calls);
            assert_eq!(observed, expected_calls);
        }
    }

    #[test]
    fn transient_conflicting_and_zero_progress_batches_yield_the_shared_worker() {
        for report in [
            AgentReconcileReport {
                scanned: 32,
                unchanged: 31,
                deferred: 1,
                deferred_transient: 1,
                retry_len: 1,
                budget_exhausted: true,
                ..Default::default()
            },
            AgentReconcileReport {
                scanned: 32,
                unchanged: 31,
                deferred: 1,
                budget_exhausted: true,
                ..Default::default()
            },
            AgentReconcileReport {
                retry_len: 1024,
                budget_exhausted: true,
                ..Default::default()
            },
        ] {
            let mut pending = Some(report);
            let mut observed = 0;
            assert!(
                run_with(
                    || Ok(pending.take().expect("a refusing pass must not be retried in this turn")),
                    || false,
                    || Duration::ZERO,
                    |_| observed += 1,
                )
                .unwrap()
            );
            assert_eq!(observed, 1);
        }
    }

    #[test]
    fn admission_refusal_after_progress_is_not_reported_as_a_completed_batch() {
        let mut calls = 0;
        let mut observed = 0;
        assert!(
            run_with(
                || {
                    calls += 1;
                    match calls {
                        1 => Ok(AgentReconcileReport {
                            repaired: 4,
                            budget_exhausted: true,
                            ..Default::default()
                        }),
                        2 => Err(ADMISSION_DEFERRED.into()),
                        _ => panic!("no repeated admission in a single turn"),
                    }
                },
                || false,
                || Duration::ZERO,
                |_| observed += 1,
            )
            .unwrap()
        );
        assert_eq!((calls, observed), (2, 1));
    }

    #[test]
    fn cancellation_between_batches_stops_without_rearming() {
        let stopped = Cell::new(false);
        let mut calls = 0;
        assert!(
            !run_with(
                || {
                    calls += 1;
                    Ok(AgentReconcileReport {
                        repaired: 4,
                        budget_exhausted: true,
                        ..Default::default()
                    })
                },
                || stopped.get(),
                || Duration::ZERO,
                |_| stopped.set(true),
            )
            .unwrap()
        );
        assert_eq!(calls, 1);
        assert!(
            !run_with(
                || panic!("cancelled work must not touch storage"),
                || true,
                || Duration::ZERO,
                |_| panic!("cancelled work has no report"),
            )
            .unwrap()
        );
    }

    // Isolate the real process-global promotion gate from unrelated libtests.
    // File-backed logs and a parent deadline also cover an admission deadlock.
    fn isolated() -> bool {
        const CHILD: &str = "AM_TEST_AGENT_CATCH_UP_ADMISSION_CHILD";
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
            .expect("spawn isolated agent catch-up test");
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

    const NAMES: [&str; 5] = ["BlueLake", "GoldFox", "RedOak", "GreenHill", "SilverCloud"];

    fn mailbox(root: &std::path::Path) -> (Config, mcp_agent_mail_db::DbPoolConfig, DbPool) {
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &root.join("mail.sqlite3"),
            ),
            storage_root: root.join("archive"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.storage_root).unwrap();
        let pool_config = mcp_agent_mail_db::DbPoolConfig {
            database_url: config.database_url.clone(),
            storage_root: Some(config.storage_root.clone()),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = mcp_agent_mail_db::create_pool(&pool_config).unwrap();
        let cx = Cx::for_testing();
        let conn = fastmcp_core::block_on(pool.acquire(&cx))
            .into_result()
            .unwrap();
        let project_key = root.join("project").to_string_lossy().replace('\'', "''");
        conn.execute_raw(&format!(
            "INSERT INTO projects(id, slug, human_key, created_at) \
             VALUES(701, 'gh353', '{project_key}', 1)"
        ))
        .unwrap();
        for (index, name) in NAMES.iter().enumerate() {
            let id = 701 + index;
            conn.execute_raw(&format!(
                "INSERT INTO agents(id, project_id, name, program, model, task_description, \
                 inception_ts, last_active_ts) \
                 VALUES({id}, 701, '{name}', 'test', 'test', 'GH353', 1000000, 2000000)"
            ))
            .unwrap();
        }
        drop(conn);
        (config, pool_config, pool)
    }

    fn assert_profiles_match_source_and_git(cx: &Cx, pool: &DbPool, config: &Config) {
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let conn = fastmcp_core::block_on(pool.acquire(cx)).into_result().unwrap();
        for (index, name) in NAMES.iter().enumerate() {
            let id = 701 + index;
            let rows = conn
                .query_sync(
                    &format!("SELECT last_active_ts FROM agents WHERE id = {id}"),
                    &[],
                )
                .unwrap();
            let timestamp = rows[0].get_named::<i64>("last_active_ts").unwrap();
            let relative = format!("projects/gh353/agents/{name}/profile.json");
            let bytes = std::fs::read(config.storage_root.join(&relative)).unwrap();
            let profile: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                profile["last_active_ts"],
                mcp_agent_mail_db::micros_to_iso(timestamp)
            );
            let entry = tree.get_path(std::path::Path::new(&relative)).unwrap();
            assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        }
    }

    #[test]
    fn failed_drain_mid_selection_resumes_the_same_pool_and_cursor() {
        if isolated() {
            return;
        }
        use mcp_agent_mail_db::write_barrier::{self, DrainOutcome};
        let temp = tempfile::tempdir().unwrap();
        let (config, _, pool) = mailbox(temp.path());
        let engine = pool.engine_identity_key();
        let cx = Cx::for_testing();
        // Exhaust the single real engine connection. The reconciler must hold
        // selection admission while it waits; no fake storage report or sleep
        // is used to manufacture that overlap.
        let held = fastmcp_core::block_on(pool.acquire(&cx))
            .into_result()
            .unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let pool = pool.clone();
            let config = config.clone();
            move || {
                let mut cursor = AgentReconcileCursor::default();
                let mut reports = 0;
                let outcome = run(
                    &Cx::for_testing(),
                    &pool,
                    &config,
                    &mut cursor,
                    &AtomicBool::new(false),
                    |_| reports += 1,
                );
                send.send((cursor, outcome, reports)).unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while write_barrier::active_writer_count() == 0 {
            assert!(Instant::now() < deadline, "selection never reached admission");
            std::thread::yield_now();
        }
        let (promotion, drain) =
            write_barrier::acquire_promotion_barrier_draining(Duration::from_millis(2));
        assert!(matches!(drain, DrainOutcome::TimedOut { .. }));
        drop(held);
        let (mut cursor, outcome, reports) = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("refusal must return before the failed promotion guard is released");
        worker.join().unwrap();
        assert_eq!(outcome, Ok(true));
        assert_eq!(reports, 0, "a partial refused pass is not a completed report");
        assert!(!write_barrier::current_thread_holds_promotion_barrier());
        assert!(write_barrier::try_begin_write_activity().is_none());
        assert!(!config.storage_root.join("projects/gh353/agents").exists());
        let position = format!("{cursor:?}");
        assert!(
            run(
                &cx,
                &pool,
                &config,
                &mut cursor,
                &AtomicBool::new(false),
                |_| panic!("closed admission must not report success"),
            )
            .unwrap()
        );
        assert_eq!(format!("{cursor:?}"), position);
        drop(promotion);

        // Ordinary writers do not exclude archive repair. The exact same two
        // bounded batches may fit in one turn or require a second time slice.
        let writer = write_barrier::begin_write_activity();
        let mut repaired = Vec::new();
        let follow_up = run(
            &cx,
            &pool,
            &config,
            &mut cursor,
            &AtomicBool::new(false),
            |report| repaired.push(report.repaired),
        )
        .unwrap();
        if follow_up {
            assert!(
                !run(
                    &cx,
                    &pool,
                    &config,
                    &mut cursor,
                    &AtomicBool::new(false),
                    |report| repaired.push(report.repaired),
                )
                .unwrap()
            );
        }
        drop(writer);
        assert_eq!(repaired, [4, 1]);
        assert_eq!(pool.engine_identity_key(), engine);
        assert_profiles_match_source_and_git(&cx, &pool, &config);

        // Do not mistake a genuinely retired source for admission backpressure.
        let (promotion, drain) =
            write_barrier::acquire_promotion_barrier_draining(Duration::from_secs(2));
        assert!(!matches!(drain, DrainOutcome::TimedOut { .. }));
        assert!(
            pool.try_recover_from_corruption("GH353 healthy-file retirement regression")
                .is_err()
        );
        drop(promotion);
        assert!(
            run(
                &cx,
                &pool,
                &config,
                &mut cursor,
                &AtomicBool::new(false),
                |_| panic!("retired source cannot complete a pass"),
            )
            .is_err()
        );
    }

    #[test]
    fn real_worker_reaches_lost_retirement_beyond_a_full_healthy_roster_page() {
        if isolated() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (config, _, pool) = mailbox(temp.path());
        let cx = Cx::for_testing();
        let conn = fastmcp_core::block_on(pool.acquire(&cx)).into_result().unwrap();
        for id in 706..=734 {
            conn.execute_sync(
                "INSERT INTO agents(id, project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES(?, 701, ?, 'test', 'test', '', 1000000, 2000000)",
                &[id.into(), format!("Agent{id}").into()],
            ).unwrap();
        }
        drop(conn);
        let mut cursor = AgentReconcileCursor::default();
        let stop = AtomicBool::new(false);
        let mut initial_repairs = 0;
        let mut backlog = true;
        for _ in 0..16 {
            if !backlog {
                break;
            }
            backlog = run(&cx, &pool, &config, &mut cursor, &stop, |report| {
                assert_eq!(report.deferred, 0);
                initial_repairs += report.repaired;
            }).unwrap();
        }
        assert!(!backlog, "initial profile materialization did not converge");
        assert_eq!(initial_repairs, 34);
        let relative = "projects/gh353/agents/Agent734/profile.json";
        let before = std::fs::read(config.storage_root.join(relative)).unwrap();
        fastmcp_core::block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
            &cx, &pool, 734, Some(345_678_901),
        )).into_result().unwrap();
        // Deliberately no lifecycle archive enqueue, client read, or resend.
        assert_eq!(std::fs::read(config.storage_root.join(relative)).unwrap(), before);
        let mut reports = Vec::new();
        let mut backlog = true;
        for _ in 0..2 {
            if !backlog {
                break;
            }
            backlog = run(&cx, &pool, &config, &mut cursor, &stop, |report| {
                assert_eq!(report.deferred, 0);
                reports.push((report.scanned, report.repaired));
            }).unwrap();
        }
        assert!(!backlog);
        assert_eq!(reports, [(32, 0), (2, 1)]);
        let bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
        let profile: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(profile["retired_at"], mcp_agent_mail_db::micros_to_iso(345_678_901));
        assert!(profile.get("registration_token").is_none());
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(std::path::Path::new(relative)).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        assert_profiles_match_source_and_git(&cx, &pool, &config);
    }
}
