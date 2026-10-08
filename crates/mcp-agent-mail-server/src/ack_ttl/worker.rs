//! ACK scan scheduling and recoverable database admission.
//!
//! The server has already completed migrations. An unavailable mailbox must
//! defer a scan, not permanently disable the worker. Freeze the selected DB
//! and archive together and retain only the existing bounded scan state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, create_pool};
use tracing::{info, warn};

use super::{
    ACK_SCAN_SLICE_BUDGET, AckScanState, SHUTDOWN, next_ack_scan_delay, run_ack_ttl_slice,
};

fn selected_pool(config: &Config) -> DbPoolConfig {
    let mut selected = DbPoolConfig::from_env();
    selected.database_url.clone_from(&config.database_url);
    selected.storage_root = Some(config.storage_root.clone());
    selected.min_connections = 1;
    selected.max_connections = 1;
    selected.warmup_connections = 0;
    selected.run_migrations = false;
    selected
}

pub(super) fn run(config: &Config) {
    let interval = Duration::from_secs(config.ack_ttl_scan_interval_seconds.max(5));
    let startup_delay = interval.min(Duration::from_secs(8));
    let selected = selected_pool(config);
    let mut pool = None;
    let mut state = AckScanState::default();

    info!(
        interval_secs = interval.as_secs(),
        ttl_seconds = config.ack_ttl_seconds,
        escalation_enabled = config.ack_escalation_enabled,
        escalation_mode = %config.ack_escalation_mode,
        "ACK TTL scan worker started"
    );
    // Delay before opening anything, not after acquiring a connection family.
    if pause(startup_delay, &SHUTDOWN) {
        return;
    }
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        let result = run_pass(config, &selected, &mut pool, &mut state, &SHUTDOWN, || {
            started.elapsed() < ACK_SCAN_SLICE_BUDGET
        });
        let failed = result.is_err();
        match result {
            Ok((scanned, overdue)) => {
                if overdue > 0 {
                    info!(
                        event = "ack_ttl_scan",
                        scanned,
                        overdue,
                        lap_incomplete = state.cursor.is_some(),
                        "ACK TTL scan slice completed"
                    );
                }
            }
            Err(error) => {
                warn!(
                    %error,
                    "ACK TTL scan deferred; retaining continuation for the next scheduled attempt"
                );
            }
        }
        // Failed opens use the same normal failure cadence as failed reads.
        // In particular, a pending cursor must not cause a 250 ms retry storm.
        let delay = next_ack_scan_delay(interval, state.cursor.is_some(), failed);
        if pause(delay, &SHUTDOWN) {
            return;
        }
    }
}

fn run_pass(
    config: &Config,
    selected: &DbPoolConfig,
    pool: &mut Option<DbPool>,
    state: &mut AckScanState,
    stop: &AtomicBool,
    has_time: impl FnMut() -> bool,
) -> Result<(usize, usize), String> {
    if stop.load(Ordering::Acquire) {
        return Ok((0, 0));
    }
    if pool.is_none() {
        let opened = create_pool(selected)
            .map_err(|error| format!("ACK scan database admission failed: {error}"))?;
        *pool = Some(opened);
    }
    // The slice rechecks shutdown before any query and owns its generation
    // lease through all escalations. Opening a pool is not a scan permit.
    run_ack_ttl_slice(
        config,
        pool.as_ref().expect("successful admission installs the pool"),
        state,
        || stop.load(Ordering::Acquire),
        has_time,
    )
}

fn pause(duration: Duration, stop: &AtomicBool) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let step = remaining.min(Duration::from_millis(100));
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;
    use fastmcp_core::block_on;

    fn fixture() -> (tempfile::TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &temp.path().join("mailbox/mail.sqlite3"),
            ),
            storage_root: temp.path().join("archive"),
            ack_ttl_seconds: 0,
            ack_escalation_enabled: true,
            ack_escalation_mode: "file_reservation".into(),
            ack_escalation_claim_holder_name: String::new(),
            ..Config::default()
        };
        (temp, config)
    }

    fn seed(config: &Config) -> DbPool {
        let mut selected = selected_pool(config);
        selected.run_migrations = true;
        let pool = create_pool(&selected).unwrap();
        let cx = Cx::for_testing();
        let conn = block_on(pool.acquire(&cx)).into_result().unwrap();
        conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'ack-worker', '/ack-worker', 1)").unwrap();
        conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1), (82, 71, 'GreenStone', 'test', 'test', 1, 1)").unwrap();
        conn.execute_raw("INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) VALUES(1, 71, 81, 'worker admission', 'Body', 'normal', 1, 1000000, '[]', '{}')").unwrap();
        conn.execute_raw("INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(1, 82, 'to')").unwrap();
        drop(conn);
        pool
    }

    #[test]
    fn pool_selection_pins_both_authorities_and_never_runs_migrations() {
        let (_temp, config) = fixture();
        let selected = selected_pool(&config);
        assert_eq!(selected.database_url, config.database_url);
        assert_eq!(
            selected.storage_root.as_deref(),
            Some(config.storage_root.as_path())
        );
        assert_eq!((selected.min_connections, selected.max_connections), (1, 1));
        assert_eq!(selected.warmup_connections, 0);
        assert!(!selected.run_migrations);
    }

    #[test]
    fn failed_open_retries_the_restored_mailbox_and_publishes_a_real_escalation() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let (temp, config) = fixture();
            let mailbox = temp.path().join("mailbox");
            std::fs::create_dir_all(&mailbox).unwrap();
            drop(seed(&config));
            // Move the complete offline family together and restore the same
            // path later. No sidecar is deleted or opened with another engine.
            let retained = temp.path().join("retained-mailbox");
            std::fs::rename(&mailbox, &retained).unwrap();
            std::fs::write(&mailbox, b"admission unavailable").unwrap();
            let selected = selected_pool(&config);
            let mut pool = None;
            let mut state = AckScanState::default();
            let stop = AtomicBool::new(false);
            let error = run_pass(&config, &selected, &mut pool, &mut state, &stop, || true)
                .expect_err("a regular-file parent cannot admit a database");
            assert!(error.contains("ACK scan database admission failed"), "{error}");
            assert!(pool.is_none());
            assert!(state.cursor.is_none());
            assert!(state.identity.is_none());
            assert!(state.current.is_empty());
            assert!(state.warned.is_empty());
            assert_eq!(std::fs::read(&mailbox).unwrap(), b"admission unavailable");
            assert_eq!(
                next_ack_scan_delay(Duration::from_secs(60), true, true),
                Duration::from_secs(60)
            );

            std::fs::rename(&mailbox, temp.path().join("admission-evidence")).unwrap();
            std::fs::rename(&retained, &mailbox).unwrap();
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
                (1, 1)
            );
            let cx = Cx::for_testing();
            let live = pool.as_ref().unwrap();
            let conn = block_on(live.acquire(&cx)).into_result().unwrap();
            let rows = conn
                .query_sync("SELECT id, agent_id, reason FROM file_reservations", &[])
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get_named::<i64>("agent_id").unwrap(), 82);
            assert_eq!(rows[0].get_named::<String>("reason").unwrap(), "ack-overdue");
            let id = rows[0].get_named::<i64>("id").unwrap();
            let generation = conn
                .query_sync("SELECT generation_id FROM db_identity WHERE singleton = 0", &[])
                .unwrap()[0]
                .get_named::<String>("generation_id")
                .unwrap();
            drop(conn);
            let filename =
                mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                    Some(&generation),
                    id,
                );
            let artifact = config
                .storage_root
                .join("projects/ack-worker/file_reservations")
                .join(filename);
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
            assert_eq!(value["agent"], "GreenStone");
            assert_eq!(value["db_generation"], generation);
            assert_eq!(value["id"], id);
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
                (1, 1)
            );
            let conn = block_on(pool.as_ref().unwrap().acquire(&cx))
                .into_result()
                .unwrap();
            assert_eq!(
                conn.query_sync("SELECT id FROM file_reservations", &[])
                    .unwrap()
                    .len(),
                1
            );
        });
    }

    #[test]
    fn shutdown_before_admission_never_opens_the_mailbox() {
        let (temp, config) = fixture();
        let selected = selected_pool(&config);
        let mut pool = None;
        let mut state = AckScanState::default();
        let stop = AtomicBool::new(true);
        assert_eq!(
            run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
            (0, 0)
        );
        assert!(pool.is_none());
        assert!(state.identity.is_none());
        assert!(!temp.path().join("mailbox").exists());
        assert!(!config.storage_root.exists());
        assert!(pause(Duration::from_secs(60), &stop));
        assert!(pause(Duration::ZERO, &stop));
    }
}
