//! Agent repair shares the message/reservation worker's source pool (GH#353).
//! Admission refusal is scheduling backpressure, not a failed source generation.
//! Productive finite scans catch up in bounded bursts; contention yields without
//! bypassing storage's per-ID backoff or delaying other maintenance phases.
//! Roster progress and retry IDs survive worker/server restarts through the
//! storage cursor's generation-bound checkpoint, never through cached profiles.
//! Critical disk pressure pauses before source/checkpoint admission. A paused
//! turn retains the same cursor and pool and resumes on the normal worker poll.

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbPool;
use mcp_agent_mail_storage::recovery::agent_reconcile::{
    AgentReconcileCursor, AgentReconcileReport,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ADMISSION_DEFERRED: &str =
    "agent reconciliation deferred: recovery promotion or admission contention";
const CHECKPOINT_OWNED: &str =
    "agent reconciliation deferred: checkpoint is owned by another worker";
const DISK_PRESSURE_DEFERRED: &str = "agent reconciliation deferred: critical disk pressure";
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
        || {
            with_disk_admission(
                mcp_agent_mail_core::global_metrics()
                    .system
                    .disk_pressure_level
                    .load(),
                || cursor.reconcile_checkpointed(cx, pool, config, stop),
            )
        },
        || stop.load(Ordering::Acquire) || cx.checkpoint().is_err(),
        || started.elapsed(),
        observe,
    )
}

fn with_disk_admission(
    pressure: u64,
    repair: impl FnOnce() -> Result<AgentReconcileReport, String>,
) -> Result<AgentReconcileReport, String> {
    if pressure >= mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64() {
        // Even an unchanged checkpointed batch can create a lock sidecar and
        // stage/fsync new hints. Refuse before checkout as well as publication,
        // not after the source has already competed for the remaining space.
        return Err(DISK_PRESSURE_DEFERRED.into());
    }
    repair()
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
        if !continue_turn || batches >= MAX_BATCHES_PER_TURN || elapsed() >= MAX_ACTIVE_PER_TURN {
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
        Err(error) if error == DISK_PRESSURE_DEFERRED => {
            // A resource pause is neither a source failure nor a completed
            // batch. Leave the pool, durable checkpoint and in-memory debt
            // alone. The normal lane deadline retries after disk recovery;
            // no one-second catch-up or repeated warning solely for pressure.
            Ok(false)
        }
        Err(error) if error == ADMISSION_DEFERRED || error == CHECKPOINT_OWNED => {
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
    fn disk_pressure_refuses_before_storage_and_does_not_mask_source_errors() {
        let critical = mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64();
        assert!(critical > 0);
        for level in [critical, critical.saturating_add(1), u64::MAX] {
            let refused = with_disk_admission(level, || {
                panic!("critical pressure must not enter the checkpointed reconciler")
            });
            assert_eq!(refused.unwrap_err(), DISK_PRESSURE_DEFERRED);
        }
        for level in [0, critical - 1] {
            let mut calls = 0;
            let result = with_disk_admission(level, || {
                calls += 1;
                Err("source unavailable".into())
            });
            assert_eq!(result.unwrap_err(), "source unavailable");
            assert_eq!(calls, 1);
        }
        for interrupted in [false, true] {
            assert!(
                !finish(
                    Err(DISK_PRESSURE_DEFERRED.into()),
                    || interrupted,
                    |_| panic!("pressure refusal is not a completed storage report"),
                )
                .unwrap()
            );
        }
        let other = format!("{DISK_PRESSURE_DEFERRED}: source also failed");
        assert_eq!(
            finish(Err(other.clone()), || false, |_| panic!("no report")),
            Err(other),
        );
    }

    #[test]
    fn pressure_between_batches_keeps_pending_work_and_resumes_without_replay() {
        let critical = mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64();
        let pressure = Cell::new(0);
        let mut pending = std::collections::VecDeque::from([
            AgentReconcileReport {
                scanned: 4,
                repaired: 4,
                budget_exhausted: true,
                ..Default::default()
            },
            AgentReconcileReport {
                scanned: 1,
                repaired: 1,
                ..Default::default()
            },
        ]);
        let mut observed = Vec::new();
        assert!(
            !run_with(
                || with_disk_admission(pressure.get(), || {
                    Ok(pending.pop_front().expect("no speculative storage batch"))
                }),
                || false,
                || Duration::ZERO,
                |report| {
                    observed.push(report.repaired);
                    pressure.set(critical);
                },
            )
            .unwrap()
        );
        assert_eq!(observed, [4]);
        assert_eq!(pending.len(), 1, "a pressure refusal cannot consume work");
        for _ in 0..60 {
            assert!(
                !run_with(
                    || with_disk_admission(pressure.get(), || {
                        panic!("sustained pressure cannot execute storage")
                    }),
                    || false,
                    || Duration::ZERO,
                    |_| panic!("no report for a paused turn"),
                )
                .unwrap()
            );
        }
        pressure.set(0);
        assert!(
            !run_with(
                || with_disk_admission(pressure.get(), || {
                    Ok(pending
                        .pop_front()
                        .expect("the remaining batch, not a replay"))
                }),
                || false,
                || Duration::ZERO,
                |report| observed.push(report.repaired),
            )
            .unwrap()
        );
        assert_eq!(observed, [4, 1]);
        assert!(pending.is_empty());
    }

    #[test]
    fn cancellation_before_a_pressure_turn_skips_even_the_batch_callback() {
        assert!(
            !run_with(
                || panic!("cancelled work must not enter disk or source admission"),
                || true,
                || Duration::ZERO,
                |_| panic!("cancelled work has no report"),
            )
            .unwrap()
        );
    }

    // Used only in isolated children, never concurrently with another test's
    // metrics. Restore on unwind too, rather than leaking pressure to teardown.
    struct RestoreDiskPressure(u64);

    impl Drop for RestoreDiskPressure {
        fn drop(&mut self) {
            mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .set(self.0);
        }
    }

    fn tree_bytes(
        root: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
        let mut entries = std::collections::BTreeMap::new();
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let kind = entry.file_type().unwrap();
                assert!(
                    !kind.is_symlink(),
                    "owned fixture must not follow a foreign path"
                );
                let value = if kind.is_dir() {
                    directories.push(path.clone());
                    None
                } else {
                    assert!(kind.is_file());
                    Some(std::fs::read(&path).unwrap())
                };
                entries.insert(path.strip_prefix(root).unwrap().to_path_buf(), value);
            }
        }
        entries
    }

    #[test]
    fn real_pressure_pause_preserves_source_checkpoint_and_lost_retirement_until_resume() {
        if isolated() {
            return;
        }
        let metric = &mcp_agent_mail_core::global_metrics()
            .system
            .disk_pressure_level;
        let _restore = RestoreDiskPressure(metric.load());
        metric.set(0);
        let temp = tempfile::tempdir().unwrap();
        let (config, _, pool) = mailbox(temp.path());
        let engine = pool.engine_identity_key();
        let cx = Cx::for_testing();
        let stop = AtomicBool::new(false);
        let mut cursor = AgentReconcileCursor::default();

        // First a never-started cursor, then a completed checkpoint whose
        // first agent has a lost retirement write. Both must be byte-neutral
        // under pressure, including SQLite sidecars and checkpoint namespace.
        for existing in [false, true] {
            if existing {
                fastmcp_core::block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
                    &cx,
                    &pool,
                    701,
                    Some(345_678_901),
                ))
                .into_result()
                .unwrap();
            }
            mcp_agent_mail_storage::flush_async_commits();
            let borrower = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            let before = tree_bytes(temp.path());
            let position = format!("{cursor:?}");
            metric.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
            for _ in 0..3 {
                assert!(
                    !run(&cx, &pool, &config, &mut cursor, &stop, |_| panic!(
                        "disk refusal cannot report a completed repair"
                    ),)
                    .unwrap()
                );
            }
            // With this one-connection pool exhausted, the old path waits or
            // errors at checkout. The pressure gate must return without it.
            assert_eq!(tree_bytes(temp.path()), before);
            assert_eq!(format!("{cursor:?}"), position);
            assert_eq!(pool.engine_identity_key(), engine);
            assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
            drop(borrower);
            if !existing {
                assert!(!config.storage_root.join(".git").exists());
            }
            metric.set(0);
            let mut repaired = 0;
            let mut backlog = true;
            for _ in 0..4 {
                if !backlog {
                    break;
                }
                backlog = run(&cx, &pool, &config, &mut cursor, &stop, |report| {
                    assert_eq!(report.deferred, 0);
                    repaired += report.repaired;
                })
                .unwrap();
            }
            assert!(
                !backlog,
                "disk recovery must resume the original finite scan"
            );
            assert_eq!(repaired, if existing { 1 } else { 5 });
            assert_eq!(pool.engine_identity_key(), engine);
            if existing {
                let relative = "projects/gh353/agents/BlueLake/profile.json";
                let bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
                let profile: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    profile["retired_at"],
                    mcp_agent_mail_db::micros_to_iso(345_678_901)
                );
                assert!(profile.get("registration_token").is_none());
                let repo = git2::Repository::open(&config.storage_root).unwrap();
                let tree = repo.head().unwrap().peel_to_tree().unwrap();
                let entry = tree.get_path(std::path::Path::new(relative)).unwrap();
                assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
            }
        }
    }

    #[test]
    fn checkpoint_ownership_refusal_yields_without_fabricating_a_batch() {
        for interrupted in [false, true] {
            assert_eq!(
                finish(
                    Err(CHECKPOINT_OWNED.into()),
                    || interrupted,
                    |_| panic!("a refused checkpoint has no completed storage report"),
                )
                .unwrap(),
                !interrupted,
            );
        }
        let mut calls = 0;
        let mut reports = Vec::new();
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
                        2 => Err(CHECKPOINT_OWNED.into()),
                        _ => panic!("checkpoint admission must not spin inside one turn"),
                    }
                },
                || false,
                || Duration::ZERO,
                |report| reports.push(report.repaired),
            )
            .unwrap()
        );
        assert_eq!(calls, 2);
        assert_eq!(reports, [4]);
        let reason = "agent checkpoint ownership changed; rescan required";
        assert_eq!(
            finish(Err(reason.into()), || false, |_| panic!("no report")),
            Err(reason.into()),
        );
    }

    #[test]
    fn real_worker_keeps_its_pool_on_checkpoint_contention_and_resumes_the_saved_scan() {
        if isolated() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (config, _, pool) = mailbox(temp.path());
        let engine = pool.engine_identity_key();
        let cx = Cx::for_testing();
        let stop = AtomicBool::new(false);
        let mut owner = AgentReconcileCursor::default();
        let first = owner
            .reconcile_checkpointed(&cx, &pool, &config, &stop)
            .unwrap();
        assert_eq!((first.repaired, first.deferred), (4, 0));
        assert!(first.budget_exhausted);
        let mut contender = AgentReconcileCursor::default();
        assert!(
            run(&cx, &pool, &config, &mut contender, &stop, |_| panic!(
                "another owner must refuse before any storage report"
            ),)
            .unwrap()
        );
        assert_eq!(pool.engine_identity_key(), engine);
        assert!(
            !config
                .storage_root
                .join("projects/gh353/agents/SilverCloud/profile.json")
                .exists()
        );
        drop(owner);
        let mut reports = Vec::new();
        assert!(
            !run(&cx, &pool, &config, &mut contender, &stop, |report| reports
                .push((report.scanned, report.repaired)),)
            .unwrap()
        );
        // A reset-to-zero scan would report five inspected profiles, not one.
        assert_eq!(reports, [(1, 1)]);
        assert_eq!(pool.engine_identity_key(), engine);
        assert_profiles_match_source_and_git(&cx, &pool, &config);
        assert_eq!(mcp_agent_mail_db::write_barrier::active_writer_count(), 0);
    }

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
                    || Ok(pending
                        .take()
                        .expect("a refusing pass must not be retried in this turn")),
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
        let conn = fastmcp_core::block_on(pool.acquire(cx))
            .into_result()
            .unwrap();
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
            assert!(
                Instant::now() < deadline,
                "selection never reached admission"
            );
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
        assert_eq!(
            reports, 0,
            "a partial refused pass is not a completed report"
        );
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
        let conn = fastmcp_core::block_on(pool.acquire(&cx))
            .into_result()
            .unwrap();
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
            })
            .unwrap();
        }
        assert!(!backlog, "initial profile materialization did not converge");
        assert_eq!(initial_repairs, 34);
        let relative = "projects/gh353/agents/Agent734/profile.json";
        let before = std::fs::read(config.storage_root.join(relative)).unwrap();
        fastmcp_core::block_on(mcp_agent_mail_db::queries::set_agent_retired_at(
            &cx,
            &pool,
            734,
            Some(345_678_901),
        ))
        .into_result()
        .unwrap();
        // Deliberately no lifecycle archive enqueue, client read, or resend.
        assert_eq!(
            std::fs::read(config.storage_root.join(relative)).unwrap(),
            before
        );
        let mut reports = Vec::new();
        let mut backlog = true;
        for _ in 0..2 {
            if !backlog {
                break;
            }
            backlog = run(&cx, &pool, &config, &mut cursor, &stop, |report| {
                assert_eq!(report.deferred, 0);
                reports.push((report.scanned, report.repaired));
            })
            .unwrap();
        }
        assert!(!backlog);
        assert_eq!(reports, [(32, 0), (2, 1)]);
        let bytes = std::fs::read(config.storage_root.join(relative)).unwrap();
        let profile: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            profile["retired_at"],
            mcp_agent_mail_db::micros_to_iso(345_678_901)
        );
        assert!(profile.get("registration_token").is_none());
        let repo = git2::Repository::open(&config.storage_root).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(std::path::Path::new(relative)).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), bytes);
        assert_profiles_match_source_and_git(&cx, &pool, &config);
    }
}
