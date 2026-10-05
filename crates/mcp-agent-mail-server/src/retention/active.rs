//! Missing active-lease recovery, independent of slow archive history scans.
//!
//! The storage pass is create-only and never changes database rows. This worker
//! shares the reservation archive-reconciliation switch, not retention settings.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use asupersync::Cx;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use mcp_agent_mail_storage::recovery::active_reservations::{
    ActiveReservationCursor, ActiveReservationReport, reconcile_active_reservations,
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
        .spawn(move || run(&config, &SHUTDOWN))
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
    if report.more && report.deferred == 0 && !report.interrupted {
        CATCH_UP_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

fn run(config: &Config, stop: &AtomicBool) {
    let mut pool: Option<DbPool> = None;
    let mut cursor = ActiveReservationCursor::default();
    if pause(CATCH_UP_INTERVAL, stop) {
        return;
    }
    loop {
        if stop.load(Ordering::Acquire) {
            return;
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
                |cx| reconcile_active_reservations(&cx, live, config, &mut cursor, stop),
            );
            match result {
                Ok(report) => {
                    delay = next_delay(&report);
                    if report.repaired > 0 || report.deferred > 0 || report.interrupted {
                        tracing::info!(target: "maintenance", event = "active_reservation_reconcile",
                            scanned = report.scanned, unchanged = report.unchanged,
                            attempted = report.attempted, repaired = report.repaired,
                            deferred = report.deferred, more = report.more,
                            interrupted = report.interrupted,
                            "create-only active reservation repair pass completed");
                    }
                    if report.scanned == 0 {
                        pool = None;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "active lease repair deferred; source and archive preserved");
                    // Recovery promotion may have retired this connection family.
                    pool = None;
                }
            }
        }
        if pause(delay, stop) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        report.deferred = 0;
        report.interrupted = true;
        assert_eq!(next_delay(&report), POLL_INTERVAL);
        assert!(pause(POLL_INTERVAL, &AtomicBool::new(true)));
    }

    struct TestWorker {
        stop: std::sync::Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
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
            drop(conn);
            drop(pool);
            let path = config
                .storage_root
                .join("projects/project/file_reservations/id-401-gaabb.json");
            assert!(!path.exists());
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker_config = config.clone();
            let worker = TestWorker {
                stop,
                thread: Some(std::thread::spawn(move || {
                    run(&worker_config, &worker_stop);
                })),
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !path.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Join before inspecting results, including on assertion failure.
            drop(worker);
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(value["id"], 401);
            assert_eq!(value["agent"], "BlueLake");
            assert_eq!(value["db_generation"], "aabb");
            assert!(value["released_ts"].is_null());
            let pool = DbPool::new(&selected_pool(&config)).unwrap();
            let conn = fastmcp_core::block_on(pool.acquire(&cx))
                .into_result()
                .unwrap();
            let rows = conn
                .query_sync(
                    "SELECT released_ts, expires_ts FROM file_reservations WHERE id=401",
                    &[],
                )
                .unwrap();
            assert_eq!(
                rows[0].get_named::<Option<i64>>("released_ts").unwrap(),
                None
            );
            assert_eq!(rows[0].get_named::<i64>("expires_ts").unwrap(), expires);
        });
    }
}
