//! Agent repair shares the message/reservation worker's source pool (GH#353).
//! Admission refusal is scheduling backpressure, not a failed source generation.

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_storage::recovery::agent_reconcile::{
    AgentReconcileCursor, AgentReconcileReport, reconcile_agent_batch,
};
use std::sync::atomic::{AtomicBool, Ordering};

const ADMISSION_DEFERRED: &str =
    "agent reconciliation deferred: recovery promotion or admission contention";

/// Run one original, bounded storage batch. Only a real source failure asks
/// the owning worker to retire the pool also used by message/release repair.
/// A refused attempt neither reports success nor retries inside this turn.
pub(super) fn run(
    cx: &Cx,
    pool: &DbPool,
    config: &Config,
    cursor: &mut AgentReconcileCursor,
    stop: &AtomicBool,
    observe: impl FnOnce(&AgentReconcileReport),
) -> Result<bool, String> {
    finish(
        reconcile_agent_batch(cx, pool, config, cursor, stop),
        || stop.load(Ordering::Acquire) || cx.checkpoint().is_err(),
        observe,
    )
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
            let follow_up = report.budget_exhausted && !report.interrupted;
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

        // Ordinary writers do not exclude archive repair. Keep one counted
        // throughout the two bounded batches, then inspect actual Git blobs.
        let writer = write_barrier::begin_write_activity();
        let mut repaired = Vec::new();
        assert!(
            run(
                &cx,
                &pool,
                &config,
                &mut cursor,
                &AtomicBool::new(false),
                |report| repaired.push(report.repaired),
            )
            .unwrap()
        );
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
}
