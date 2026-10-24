//! Missing active-lease recovery, independent of slow archive history scans.
//!
//! The storage pass is create-only and never changes database rows. This worker
//! shares the reservation archive-reconciliation switch, not retention settings.
//! Its owned thread restarts failed runs with bounded backoff. A failed run's
//! database pool and scan cursor are dropped before a fresh run is admitted.
//! Productive passes run in finite catch-up turns; transient retry debt keeps
//! the short poll without bypassing storage cooldown or spinning on contention.
//! Each pass checkpoints source-bound scheduling hints beside SQLite. Restart
//! restores pending IDs, not captured lease state or a failed DB transaction.
//! Critical disk pressure pauses before initial pool creation and before each
//! storage batch. The same pool/cursor survive the pause without extra retries.

mod turn;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use mcp_agent_mail_storage::recovery::active_reservations::{
    ActiveReservationCursor, ActiveReservationReport,
};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const CATCH_UP_INTERVAL: Duration = Duration::from_secs(1);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(None));

pub(super) fn start(config: &Config) {
    if !mcp_agent_mail_storage::recovery::reservation_reconcile::enabled(config) {
        return;
    }
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
        && let Some(stale) = worker.take()
    {
        let _ = stale.join();
    }
    if worker.is_some() {
        return;
    }
    let config = config.clone();
    SHUTDOWN.store(false, Ordering::Release);
    match std::thread::Builder::new()
        .name("active-lease-repair".into())
        .stack_size(mcp_agent_mail_core::worker_stack_size())
        .spawn(move || supervise(&config, &SHUTDOWN))
    {
        Ok(handle) => *worker = Some(handle),
        Err(error) => tracing::warn!(%error, "could not start active lease repair worker"),
    }
}

pub(super) fn shutdown() {
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    SHUTDOWN.store(true, Ordering::Release);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn selected_pool(config: &Config) -> DbPoolConfig {
    let mut selected = DbPoolConfig::from_env();
    selected.database_url.clone_from(&config.database_url);
    selected.storage_root = Some(config.storage_root.clone());
    selected.min_connections = 1;
    selected.max_connections = 1;
    selected.warmup_connections = 0;
    // The serving path completes migrations before starting maintenance.
    selected.run_migrations = false;
    selected
}

fn pause(duration: Duration, stop: &AtomicBool) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let slice = remaining.min(Duration::from_millis(100));
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
    stop.load(Ordering::Acquire)
}

fn next_delay(report: &ActiveReservationReport) -> Duration {
    if !report.interrupted
        && (report.retry_len > 0
            || report.deferred_transient > 0
            || (report.more && report.deferred == 0))
    {
        CATCH_UP_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

fn restart_delay(previous: Duration, lived: Duration) -> Duration {
    if lived >= POLL_INTERVAL {
        CATCH_UP_INTERVAL
    } else {
        previous
            .saturating_mul(2)
            .clamp(CATCH_UP_INTERVAL, POLL_INTERVAL)
    }
}

fn supervise(config: &Config, stop: &AtomicBool) {
    supervise_with(stop, || run(config, stop), |delay| pause(delay, stop));
}

/// A restart begins from authoritative storage, not a failed run's cursor or
/// connection. The production closure captures only immutable configuration and
/// the stop flag; all mutable recovery state belongs to `run` and unwinds before
/// this boundary returns. No failed transaction is resumed or marked successful.
/// A fresh cursor may restore validated scheduling hints from its own source's
/// checkpoint; every pending lease is still reread from the live database.
///
/// This handles unwinding/early exit, not a blocked syscall or process abort.
/// The original thread remains owned and joined; retries never spawn or detach
/// another worker, and shutdown is checked before every attempt and after exit.
fn supervise_with(
    stop: &AtomicBool,
    mut attempt: impl FnMut(),
    mut wait: impl FnMut(Duration) -> bool,
) {
    let mut previous_delay = Duration::ZERO;
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        // Drop the panic payload too before backoff; it may own run resources.
        let panicked =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut attempt)).is_err();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let delay = restart_delay(previous_delay, started.elapsed());
        previous_delay = delay;
        tracing::warn!(
            panicked,
            retry_in_secs = delay.as_secs(),
            "active lease repair exited unexpectedly; restarting from live database state"
        );
        if wait(delay) {
            return;
        }
    }
}

fn apply_successful_turn_pool_policy(pool: Option<&DbPool>, turn: &turn::Turn) {
    if turn.idle && pool.is_some() {
        // Empty is healthy, not a generation failure. Keep the worker's
        // one-connection source pool across the normal idle interval. The
        // error path below still retires it after source/generation failures.
        tracing::debug!(
            target: "maintenance",
            event = "active_reservation_reconcile_idle",
            "active lease repair is idle; retaining its source pool"
        );
    }
}

fn run(config: &Config, stop: &AtomicBool) {
    run_with(config, stop, turn::disk_is_critical, |delay| {
        pause(delay, stop)
    });
}

fn run_with(
    config: &Config,
    stop: &AtomicBool,
    mut disk_critical: impl FnMut() -> bool,
    mut wait: impl FnMut(Duration) -> bool,
) {
    let mut pool: Option<DbPool> = None;
    let mut cursor = ActiveReservationCursor::default();
    if wait(CATCH_UP_INTERVAL) {
        return;
    }
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Pool creation itself can open/create SQLite sidecars. Do not wait
        // until turn admission to guard the first run after startup/recovery.
        // A pressure pause neither drops a healthy source nor resets its debt.
        if disk_critical() {
            if wait(POLL_INTERVAL) {
                return;
            }
            continue;
        }
        if pool.is_none() {
            match mcp_agent_mail_db::create_pool(&selected_pool(config)) {
                Ok(created) => pool = Some(created),
                Err(error) => tracing::warn!(%error,
                    "active lease repair awaits live database admission"),
            }
        }
        let mut delay = POLL_INTERVAL;
        if let Some(live) = pool.as_ref() {
            let cx = fastmcp_core::block_on(async { Cx::current() });
            let result = cx.map_or_else(
                || Err("active lease repair has no runtime context".to_string()),
                |cx| {
                    turn::run(
                        || cursor.reconcile_checkpointed(&cx, live, config, stop),
                        |report| {
                            if report.repaired > 0 || report.deferred > 0 || report.interrupted {
                                tracing::info!(target: "maintenance", event = "active_reservation_reconcile",
                                    scanned = report.scanned, unchanged = report.unchanged,
                                    attempted = report.attempted, repaired = report.repaired,
                                    deferred = report.deferred,
                                    deferred_transient = report.deferred_transient,
                                    retry_len = report.retry_len, more = report.more,
                                    interrupted = report.interrupted,
                                    "create-only active reservation repair pass completed");
                            }
                        },
                        || stop.load(Ordering::Acquire) || cx.checkpoint().is_err(),
                    )
                },
            );
            match result {
                Ok(turn) => {
                    delay = turn.delay;
                    apply_successful_turn_pool_policy(pool.as_ref(), &turn);
                }
                Err(error) => {
                    tracing::warn!(%error, "active lease repair deferred; source and archive preserved");
                    // Recovery promotion may have retired this connection family.
                    pool = None;
                }
            }
        }
        if wait(delay) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn pressure_paused_worker_does_not_create_a_source_or_checkpoint_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("mail.sqlite3");
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&database),
            storage_root: temp.path().join("archive"),
            ..Default::default()
        };
        let probes = Cell::new(0);
        let mut delays = Vec::new();
        // Run the actual worker loop with a deterministic pressure observation
        // and wait. Neither callback substitutes the pool or storage code.
        run_with(
            &config,
            &AtomicBool::new(false),
            || {
                probes.set(probes.get() + 1);
                true
            },
            |delay| {
                delays.push(delay);
                delays.len() == 4
            },
        );
        assert_eq!(probes.get(), 3);
        assert_eq!(
            delays,
            [
                CATCH_UP_INTERVAL,
                POLL_INTERVAL,
                POLL_INTERVAL,
                POLL_INTERVAL
            ]
        );
        assert!(!database.exists());
        assert!(!config.storage_root.exists());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn shutdown_during_pressure_wait_prevents_any_later_source_admission() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &temp.path().join("mail.sqlite3"),
            ),
            storage_root: temp.path().join("archive"),
            ..Default::default()
        };
        let stop = AtomicBool::new(false);
        let probes = Cell::new(0);
        let mut waits = 0;
        run_with(
            &config,
            &stop,
            || {
                probes.set(probes.get() + 1);
                assert_eq!(
                    probes.get(),
                    1,
                    "shutdown must precede the next disk/source probe"
                );
                true
            },
            |delay| {
                waits += 1;
                if waits == 2 {
                    assert_eq!(delay, POLL_INTERVAL);
                    stop.store(true, Ordering::Release);
                }
                // Even a completed wait must respect the now-set stop flag.
                false
            },
        );
        assert_eq!(waits, 2);
        assert_eq!(probes.get(), 1);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn restart_backoff_is_bounded_and_recovers_after_a_stable_run() {
        let mut previous = Duration::ZERO;
        for seconds in [1, 2, 4, 8, 16, 30, 30, 30] {
            previous = restart_delay(previous, Duration::ZERO);
            assert_eq!(previous, Duration::from_secs(seconds));
        }
        assert_eq!(restart_delay(Duration::MAX, Duration::ZERO), POLL_INTERVAL);
        assert_eq!(
            restart_delay(
                POLL_INTERVAL,
                POLL_INTERVAL.saturating_sub(Duration::from_nanos(1))
            ),
            POLL_INTERVAL
        );
        assert_eq!(
            restart_delay(POLL_INTERVAL, POLL_INTERVAL),
            CATCH_UP_INTERVAL
        );
    }

    #[test]
    #[allow(clippy::manual_assert)] // the panic is the injected fault
    fn supervisor_restarts_panics_and_unexpected_returns() {
        let stop = AtomicBool::new(false);
        let attempts = Cell::new(0);
        let mut delays = Vec::new();
        supervise_with(
            &stop,
            || {
                attempts.set(attempts.get() + 1);
                assert!(attempts.get() != 1, "injected active lease repair failure");
                if attempts.get() >= 3 {
                    stop.store(true, Ordering::Release);
                }
            },
            |delay| {
                delays.push(delay);
                false
            },
        );
        assert_eq!(attempts.get(), 3);
        assert_eq!(delays, [Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[test]
    fn supervisor_does_not_start_or_restart_after_shutdown() {
        let stop = AtomicBool::new(true);
        let attempts = Cell::new(0);
        let waits = Cell::new(0);
        supervise_with(
            &stop,
            || attempts.set(attempts.get() + 1),
            |_| {
                waits.set(waits.get() + 1);
                true
            },
        );
        assert_eq!(attempts.get(), 0);
        assert_eq!(waits.get(), 0);
        stop.store(false, Ordering::Release);
        supervise_with(
            &stop,
            || {
                attempts.set(attempts.get() + 1);
                stop.store(true, Ordering::Release);
                panic!("failure races shutdown");
            },
            |_| {
                waits.set(waits.get() + 1);
                true
            },
        );
        assert_eq!(attempts.get(), 1);
        assert_eq!(waits.get(), 0, "shutdown must win over restart backoff");
    }

    #[test]
    fn shutdown_during_backoff_prevents_the_next_attempt() {
        let stop = AtomicBool::new(false);
        let attempts = Cell::new(0);
        supervise_with(
            &stop,
            || attempts.set(attempts.get() + 1),
            |_| {
                stop.store(true, Ordering::Release);
                // Even a completed wait must recheck shutdown before a restart.
                false
            },
        );
        assert_eq!(attempts.get(), 1);
        assert!(pause(POLL_INTERVAL, &stop));
    }

    #[test]
    #[allow(clippy::manual_assert)] // the panic is the injected fault
    fn failed_run_resources_are_dropped_before_backoff_and_restart() {
        struct Lease<'a>(&'a Cell<usize>);
        impl Drop for Lease<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        let stop = AtomicBool::new(false);
        let active = Cell::new(0);
        let attempts = Cell::new(0);
        let clean_on_entry = Cell::new(true);
        let clean_during_backoff = Cell::new(true);
        supervise_with(
            &stop,
            || {
                clean_on_entry.set(clean_on_entry.get() && active.get() == 0);
                active.set(active.get() + 1);
                let _lease = Lease(&active);
                attempts.set(attempts.get() + 1);
                assert!(attempts.get() != 1, "unwind while owning run-local state");
                stop.store(true, Ordering::Release);
            },
            |_| {
                clean_during_backoff.set(clean_during_backoff.get() && active.get() == 0);
                false
            },
        );
        assert_eq!(attempts.get(), 2);
        assert_eq!(active.get(), 0);
        assert!(
            clean_on_entry.get(),
            "restart retained failed run resources"
        );
        assert!(
            clean_during_backoff.get(),
            "backoff retained failed run resources"
        );
    }

    #[test]
    fn empty_successful_turn_keeps_the_healthy_source_pool() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: temp.path().join("archive"),
                ..Default::default()
            };
            std::fs::create_dir_all(&config.storage_root).unwrap();
            let mut selected = selected_pool(&config);
            selected.run_migrations = true;
            let source = DbPool::new(&selected).unwrap();
            let engine = source.engine_identity_key();
            let pool = Some(source);
            let turn = turn::Turn {
                delay: POLL_INTERVAL,
                idle: true,
            };

            apply_successful_turn_pool_policy(pool.as_ref(), &turn);

            let retained = pool
                .as_ref()
                .expect("healthy idle turn retired source pool");
            assert_eq!(retained.engine_identity_key(), engine);
            assert!(!retained.is_closed());
        });
    }

    #[test]
    fn pool_and_backoff_follow_the_selected_mailbox_without_enabling_retention() {
        let config = Config {
            database_url: "sqlite:///chosen/mail.sqlite3".into(),
            storage_root: "/chosen/archive".into(),
            ..Default::default()
        };
        let selected = selected_pool(&config);
        assert_eq!(selected.database_url, config.database_url);
        assert_eq!(
            selected.storage_root.as_deref(),
            Some(config.storage_root.as_path())
        );
        assert_eq!((selected.min_connections, selected.max_connections), (1, 1));
        assert_eq!(selected.warmup_connections, 0);
        assert!(!selected.run_migrations);
        assert!(!config.retention_report_enabled);
        assert_eq!(config.messages_retention_days, 0);
        let mut report = ActiveReservationReport::default();
        assert_eq!(next_delay(&report), POLL_INTERVAL);
        report.more = true;
        assert_eq!(next_delay(&report), CATCH_UP_INTERVAL);
        report.deferred = 1;
        assert_eq!(next_delay(&report), POLL_INTERVAL);
        report.retry_len = 1;
        assert_eq!(next_delay(&report), CATCH_UP_INTERVAL);
        report.more = false;
        assert_eq!(next_delay(&report), CATCH_UP_INTERVAL);
        report.retry_len = 0;
        report.deferred_transient = 1;
        assert_eq!(next_delay(&report), CATCH_UP_INTERVAL);
        report.deferred = 0;
        report.interrupted = true;
        assert_eq!(next_delay(&report), POLL_INTERVAL);
        assert!(pause(POLL_INTERVAL, &AtomicBool::new(true)));
    }

    struct TestWorker {
        stop: std::sync::Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestWorker {
        #[allow(clippy::manual_assert)] // the panic is the injected fault
        fn spawn(config: Config, fail_first: bool) -> Self {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let worker_attempts = attempts.clone();
            let thread = std::thread::spawn(move || {
                supervise_with(
                    &worker_stop,
                    || {
                        let attempt = worker_attempts.fetch_add(1, Ordering::AcqRel);
                        assert!(
                            !(fail_first && attempt == 0),
                            "injected failure before active reservation repair"
                        );
                        run(&config, &worker_stop);
                    },
                    |delay| pause(delay, &worker_stop),
                );
            });
            Self {
                stop,
                thread: Some(thread),
                attempts,
            }
        }
    }

    impl Drop for TestWorker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn worker_restores_an_active_guard_artifact_without_another_client_request() {
        assert_worker_restores_artifact(false, 1, false);
    }

    #[test]
    fn failed_worker_restarts_and_restores_the_real_guard_artifact() {
        assert_worker_restores_artifact(true, 1, false);
    }

    #[test]
    fn worker_commits_a_multi_batch_active_backlog_without_client_requests() {
        assert_worker_restores_artifact(false, 13, false);
    }

    #[test]
    fn worker_restart_restores_checkpointed_backlog_without_client_requests() {
        // Hold the actual global archive fence in an isolated libtest process,
        // not alongside unrelated tests trying to publish their own archives.
        const CHILD: &str = "AM_TEST_ACTIVE_WORKER_CHECKPOINT_CHILD";
        let thread = std::thread::current();
        let name = thread.name().unwrap();
        if std::env::var(CHILD).as_deref() != Ok(name) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env(CHILD, name)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut expired = false;
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    expired = true;
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                !expired && output.status.success(),
                "{name}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            return;
        }
        assert_worker_restores_artifact(false, 13, true);
    }

    fn checkpoint_debt_before_restart(config: &Config, parent: &std::path::Path, count: usize) {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            mcp_agent_mail_storage::with_archive_snapshot_publication_fence(|| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(20))
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let worker = TestWorker::spawn(config.clone(), false);
        let deadline = Instant::now() + Duration::from_secs(10);
        let observed = loop {
            let path =
                std::fs::read_dir(parent).ok().and_then(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .find(|path| {
                            path.extension().is_some_and(|ext| ext == "json")
                                && path.file_name().and_then(|name| name.to_str()).is_some_and(
                                    |name| name.starts_with(".am-active-reservation-reconcile-"),
                                )
                        })
                });
            if let Some(path) = path {
                let snapshot = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
                if snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot["retries"]
                        .as_array()
                        .is_some_and(|ids| ids.len() == count)
                }) {
                    break Some(path);
                }
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        // Join while publication is still refused. The next worker must not
        // inherit either this run's cursor or its lifetime checkpoint lock.
        drop(worker);
        let _ = release_tx.send(());
        assert!(
            owner.join().unwrap().is_ok(),
            "worker waited for fence-owner expiry"
        );
        let path = observed.expect("worker never checkpointed its deferred active leases");
        let snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut ids = snapshot["retries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_i64().unwrap())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(
            ids,
            (401..=400 + i64::try_from(count).unwrap()).collect::<Vec<_>>()
        );
        assert_eq!(snapshot["binding"]["generation"], "aabb");
        assert_eq!(snapshot["version"], 1);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("json.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock)
            .expect("stopped worker retained checkpoint ownership");
        drop(lock);
        for id in ids {
            assert!(
                !config
                    .storage_root
                    .join(format!(
                        "projects/project/file_reservations/id-{id}-gaabb.json"
                    ))
                    .exists(),
                "a held publication fence must not be bypassed"
            );
        }
    }

    fn assert_worker_restores_artifact(fail_first: bool, count: usize, restart_with_debt: bool) {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let temp = tempfile::tempdir().unwrap();
            let config = Config {
                database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                    &temp.path().join("mail.sqlite3"),
                ),
                storage_root: temp.path().join("archive"),
                retention_report_enabled: false,
                quota_enabled: false,
                messages_retention_days: 0,
                ..Default::default()
            };
            std::fs::create_dir_all(&config.storage_root).unwrap();
            let mut selected = selected_pool(&config);
            selected.run_migrations = true;
            let pool = DbPool::new(&selected).unwrap();
            let cx = Cx::for_testing();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'project', '/project', 1)").unwrap();
            conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
            conn.execute_raw(
                "INSERT OR REPLACE INTO db_identity(singleton, generation_id) VALUES(0, 'aabb')",
            )
            .unwrap();
            let expires = mcp_agent_mail_db::now_micros() + 3_600_000_000;
            conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES(401, 71, 81, 'src/*.rs', 1, 'worker-test', 1000000, {expires}, NULL)")).unwrap();
            let last_id = 400 + i64::try_from(count).unwrap();
            for id in 402..=last_id {
                conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) SELECT {id}, project_id, agent_id, 'src/{id}.rs', \"exclusive\", reason, created_ts, expires_ts, NULL FROM file_reservations WHERE id=401")).unwrap();
            }
            drop(conn);
            drop(pool);
            if restart_with_debt {
                checkpoint_debt_before_restart(&config, temp.path(), count);
            }
            let last_relative =
                format!("projects/project/file_reservations/id-{last_id}-gaabb.json");
            assert!(!config.storage_root.join(&last_relative).exists());
            let worker = TestWorker::spawn(config.clone(), fail_first);
            let attempts = worker.attempts.clone();
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            // A file can appear before its commit. Wait for the actual final
            // Git blob before shutting the worker down, not just path.exists().
            let complete = loop {
                let committed = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&config.storage_root)
                    .args(["cat-file", "-e"])
                    .arg(format!("HEAD:{last_relative}"))
                    .output()
                    .expect("Git is required for archive worker integration tests");
                if committed.status.success() {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            // Join before inspecting results, including on assertion failure.
            drop(worker);
            assert!(
                complete,
                "the worker did not commit the complete active backlog"
            );
            assert_eq!(
                attempts.load(Ordering::Acquire),
                if fail_first { 2 } else { 1 }
            );
            for id in 401..=last_id {
                let relative = format!("projects/project/file_reservations/id-{id}-gaabb.json");
                let bytes = std::fs::read(config.storage_root.join(&relative)).unwrap();
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["id"], id);
                assert_eq!(value["agent"], "BlueLake");
                assert_eq!(value["reason"], "worker-test");
                assert_eq!(value["db_generation"], "aabb");
                assert!(value["released_ts"].is_null());
                let committed = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&config.storage_root)
                    .args(["cat-file", "blob"])
                    .arg(format!("HEAD:{relative}"))
                    .output()
                    .unwrap();
                assert!(committed.status.success());
                assert_eq!(
                    committed.stdout, bytes,
                    "working tree and committed lease must agree"
                );
            }
            let pool = DbPool::new(&selected_pool(&config)).unwrap();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            let rows = conn
                .query_sync(
                    &format!("SELECT id, released_ts, expires_ts FROM file_reservations WHERE id >= 401 AND id <= {last_id} ORDER BY id"),
                    &[],
                )
                .unwrap();
            assert_eq!(
                rows.len(),
                count,
                "repair must not create or remove source leases"
            );
            for (offset, row) in rows.iter().enumerate() {
                assert_eq!(
                    row.get_named::<i64>("id").unwrap(),
                    401 + i64::try_from(offset).unwrap()
                );
                assert_eq!(row.get_named::<Option<i64>>("released_ts").unwrap(), None);
                assert_eq!(row.get_named::<i64>("expires_ts").unwrap(), expires);
            }
        });
    }
}
