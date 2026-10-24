//! ACK scan scheduling and recoverable database admission.
//!
//! The server has already completed migrations. An unavailable mailbox must
//! defer a scan, not permanently disable the worker. Freeze the selected DB
//! and archive together and retain only the existing bounded scan state.
//! An unwinding attempt drops its pool and observations before restarting on
//! the same owned thread. Committed grants are revalidated, never replayed from
//! a captured payload. Process aborts and blocked native calls are not preempted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, DbPoolConfig, create_pool};
use tracing::{info, warn};

use super::{
    ACK_SCAN_SLICE_BUDGET, AckScanState, SHUTDOWN, next_ack_scan_delay, run_ack_ttl_slice,
};

const MIN_RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);

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

    info!(
        interval_secs = interval.as_secs(),
        ttl_seconds = config.ack_ttl_seconds,
        escalation_enabled = config.ack_escalation_enabled,
        escalation_mode = %config.ack_escalation_mode,
        "ACK TTL scan worker started"
    );
    // Delay before opening anything, not after acquiring a connection family.
    // This is a process-start delay, not an additional delay on every restart.
    if pause(startup_delay, &SHUTDOWN) {
        return;
    }
    supervise(
        &SHUTDOWN,
        || scan_loop(config, &selected, interval, &SHUTDOWN),
        |delay| pause(delay, &SHUTDOWN),
        |started| started.elapsed(),
    );
}

fn restart_delay(previous: Duration, lived: Duration) -> Duration {
    if lived >= MAX_RESTART_DELAY {
        MIN_RESTART_DELAY
    } else {
        previous
            .saturating_mul(2)
            .clamp(MIN_RESTART_DELAY, MAX_RESTART_DELAY)
    }
}

/// The attempt owns every pool, cursor and admission guard. Unwinding it must
/// finish before logging, sleeping or starting another attempt. A new scan
/// rereads the source; the transactional grant API handles already-committed
/// effects. Neither a panic nor an unexpected return silently disables ACKs.
fn supervise(
    stop: &AtomicBool,
    mut attempt: impl FnMut(),
    mut wait: impl FnMut(Duration) -> bool,
    mut lived: impl FnMut(Instant) -> Duration,
) {
    let mut previous_delay = Duration::ZERO;
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            attempt();
        }))
        .is_err();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let delay = restart_delay(previous_delay, lived(started));
        previous_delay = delay;
        warn!(
            event = "ack_ttl_worker_restarting",
            panicked,
            retry_after_secs = delay.as_secs(),
            "ACK TTL scan attempt ended unexpectedly; restarting with fresh source observations"
        );
        if stop.load(Ordering::Acquire) || wait(delay) {
            return;
        }
    }
}

fn scan_loop(config: &Config, selected: &DbPoolConfig, interval: Duration, stop: &AtomicBool) {
    let mut pool = None;
    let mut state = AckScanState::default();
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        let result = run_pass(config, selected, &mut pool, &mut state, stop, || {
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
        if pause(delay, stop) {
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
    let result = run_ack_ttl_slice(
        config,
        pool.as_ref()
            .expect("successful admission installs the pool"),
        state,
        || stop.load(Ordering::Acquire),
        has_time,
    );
    // This exact, internal refusal is emitted before the slice accesses its
    // pool. It says nothing about connection health. All source/query failures
    // carry a different contextual prefix and still retire the worker handle.
    let admission_deferred = result.as_ref().err().is_some_and(|error| {
        error.as_str() == "ACK scan deferred: recovery write admission is busy"
    });
    if result.is_err() && !admission_deferred {
        // The slice has already released its generation lease. A failed or
        // retired connection family must not be reused forever after recovery.
        // Preserve the bounded cursor: its next successful read either resumes
        // the same source or rebinds a changed generation before any escalation.
        *pool = None;
    }
    result
}

fn pause(duration: Duration, stop: &AtomicBool) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let nap = remaining.min(Duration::from_millis(100));
        std::thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
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
        conn.execute_raw(
            "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(1, 82, 'to')",
        )
        .unwrap();
        drop(conn);
        pool
    }

    #[test]
    fn first_escalation_seeds_generation_and_stamps_the_archive() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let (temp, config) = fixture();
            std::fs::create_dir_all(temp.path().join("mailbox")).unwrap();
            let control = seed(&config);
            let cx = Cx::for_testing();
            let selected = selected_pool(&config);
            let mut pool = None;
            let mut state = AckScanState::default();
            let stop = AtomicBool::new(false);
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
                (1, 1)
            );
            let conn = block_on(control.acquire(&cx)).into_result().unwrap();
            let generation = conn
                .query_sync(
                    "SELECT generation_id FROM db_identity WHERE singleton = 0",
                    &[],
                )
                .unwrap()[0]
                .get_named::<String>("generation_id")
                .unwrap();
            let id = conn
                .query_sync("SELECT id FROM file_reservations", &[])
                .unwrap()[0]
                .get_named::<i64>("id")
                .unwrap();
            let filename = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                Some(&generation),
                id,
            );
            let path = config
                .storage_root
                .join("projects/ack-worker/file_reservations")
                .join(filename);
            let artifact: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(artifact["db_generation"], generation);
            assert_eq!(artifact["id"], id);
        });
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
            assert!(
                error.contains("ACK scan database admission failed"),
                "{error}"
            );
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
            assert_eq!(
                rows[0].get_named::<String>("reason").unwrap(),
                "ack-overdue"
            );
            let id = rows[0].get_named::<i64>("id").unwrap();
            let generation = conn
                .query_sync(
                    "SELECT generation_id FROM db_identity WHERE singleton = 0",
                    &[],
                )
                .unwrap()[0]
                .get_named::<String>("generation_id")
                .unwrap();
            drop(conn);
            let filename = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
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

    fn seed_tail(pool: &DbPool, cx: &Cx) -> String {
        let conn = block_on(pool.acquire(cx)).into_result().unwrap();
        for id in [2_i64, 3] {
            conn.execute_sync(
                "INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) SELECT ?, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json FROM messages WHERE id = 1",
                &[id.into()],
            )
            .unwrap();
            conn.execute_sync(
                "INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(?, 82, 'to')",
                &[id.into()],
            )
            .unwrap();
        }
        conn.query_sync(
            "SELECT generation_id FROM db_identity WHERE singleton = 0",
            &[],
        )
        .unwrap()[0]
            .get_named::<String>("generation_id")
            .unwrap()
    }

    fn set_generation(pool: &DbPool, cx: &Cx, generation: &str) {
        let conn = block_on(pool.acquire(cx)).into_result().unwrap();
        assert_eq!(
            conn.execute_sync(
                "UPDATE db_identity SET generation_id = ? WHERE singleton = 0",
                &[mcp_agent_mail_db::sqlmodel_core::Value::Text(
                    generation.to_string()
                )],
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn failed_read_reopens_without_losing_the_tail_or_missing_a_new_ack() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let (temp, mut config) = fixture();
            std::fs::create_dir_all(temp.path().join("mailbox")).unwrap();
            config.ack_escalation_enabled = false;
            let control = seed(&config);
            let cx = Cx::for_testing();
            let generation = seed_tail(&control, &cx);
            let selected = selected_pool(&config);
            let mut pool = None;
            let mut state = AckScanState::default();
            let stop = AtomicBool::new(false);
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
                (3, 3)
            );
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || false).unwrap(),
                (3, 1)
            );
            let cursor = state.cursor.clone();
            let identity = state.identity.clone();
            let warned = state.warned.clone();
            let current = state.current.clone();
            assert!(cursor.is_some());
            // An actual source-validation error, not a mocked query outcome.
            set_generation(&control, &cx, "");
            let error = run_pass(&config, &selected, &mut pool, &mut state, &stop, || true)
                .expect_err("invalid generation must refuse the page");
            assert!(error.contains("failed to read overdue ACK page"), "{error}");
            assert!(
                pool.is_none(),
                "a failed source must not pin the worker's pool"
            );
            assert_eq!(state.cursor, cursor);
            assert_eq!(state.identity, identity);
            assert_eq!(state.warned, warned);
            assert_eq!(state.current, current);

            set_generation(&control, &cx, &generation);
            block_on(mcp_agent_mail_db::queries::acknowledge_message(
                &cx, &control, 82, 2,
            ))
            .into_result()
            .unwrap();
            // Reopening is not a fresh lap in an unchanged generation. Only
            // message 3 remains in the unconsumed window; message 2 is now ACKed.
            config.ack_escalation_enabled = true;
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
                (1, 1)
            );
            assert!(pool.is_some());
            assert!(state.cursor.is_none());
            assert_eq!(state.warned.len(), 2);
            let conn = block_on(control.acquire(&cx)).into_result().unwrap();
            let claims = conn
                .query_sync("SELECT agent_id, reason FROM file_reservations", &[])
                .unwrap();
            assert_eq!(claims.len(), 1);
            assert_eq!(claims[0].get_named::<i64>("agent_id").unwrap(), 82);
            assert_eq!(
                claims[0].get_named::<String>("reason").unwrap(),
                "ack-overdue"
            );
            let receipts = conn
                .query_sync(
                    "SELECT ack_ts FROM message_recipients WHERE message_id = 2",
                    &[],
                )
                .unwrap();
            assert!(
                receipts[0]
                    .get_named::<Option<i64>>("ack_ts")
                    .unwrap()
                    .is_some()
            );
        });
    }

    #[test]
    fn reopened_pool_restarts_a_changed_generation_before_publishing_a_claim() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let (temp, mut config) = fixture();
            std::fs::create_dir_all(temp.path().join("mailbox")).unwrap();
            config.ack_escalation_enabled = false;
            let control = seed(&config);
            let cx = Cx::for_testing();
            let original = seed_tail(&control, &cx);
            let selected = selected_pool(&config);
            let mut pool = None;
            let mut state = AckScanState::default();
            let stop = AtomicBool::new(false);
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || false).unwrap(),
                (3, 1)
            );
            set_generation(&control, &cx, "");
            assert!(run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).is_err());
            assert!(pool.is_none());
            // Exercise the persisted generation contract. This is not a
            // physical file-swap or cross-process recovery qualification.
            let replacement = format!("{original}ab");
            set_generation(&control, &cx, &replacement);
            config.ack_escalation_enabled = true;
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || false).unwrap(),
                (3, 1)
            );
            assert_eq!(
                state.identity.as_ref().unwrap().1.as_deref(),
                Some(replacement.as_str())
            );
            assert_eq!(state.current.len(), 1);
            assert!(state.current.iter().any(|key| key.message_id == 1));
            let conn = block_on(control.acquire(&cx)).into_result().unwrap();
            let claims = conn
                .query_sync("SELECT id FROM file_reservations", &[])
                .unwrap();
            assert_eq!(claims.len(), 1);
            let id = claims[0].get_named::<i64>("id").unwrap();
            drop(conn);
            let filename = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                Some(&replacement),
                id,
            );
            let artifact = config
                .storage_root
                .join("projects/ack-worker/file_reservations")
                .join(filename);
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
            assert_eq!(value["db_generation"], replacement);
            assert_eq!(value["id"], id);
            assert_eq!(value["agent"], "GreenStone");
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

    #[test]
    fn restart_backoff_is_bounded_and_resets_after_a_stable_attempt() {
        let mut previous = Duration::ZERO;
        for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
            previous = restart_delay(previous, Duration::ZERO);
            assert_eq!(previous, Duration::from_secs(seconds));
        }
        assert_eq!(
            restart_delay(Duration::MAX, Duration::ZERO),
            MAX_RESTART_DELAY
        );
        assert_eq!(
            restart_delay(
                MAX_RESTART_DELAY,
                MAX_RESTART_DELAY.saturating_sub(Duration::from_nanos(1))
            ),
            MAX_RESTART_DELAY
        );
        assert_eq!(
            restart_delay(MAX_RESTART_DELAY, MAX_RESTART_DELAY),
            MIN_RESTART_DELAY
        );
    }

    #[test]
    fn panics_and_unexpected_returns_restart_on_the_owned_thread() {
        let stop = AtomicBool::new(false);
        let calls = std::cell::Cell::new(0);
        let original = std::thread::current().id();
        let mut waits = Vec::new();
        supervise(
            &stop,
            || {
                assert_eq!(std::thread::current().id(), original);
                calls.set(calls.get() + 1);
                match calls.get() {
                    1 => panic!("injected ACK attempt panic"),
                    2 => {}
                    _ => stop.store(true, Ordering::Release),
                }
            },
            |delay| {
                waits.push(delay);
                // Assertions in an attempt are caught by the production
                // supervisor too. Fail outside it instead of looping forever
                // if an unexpected assertion prevents normal termination.
                assert!(waits.len() <= 2, "ACK attempt repeatedly failed");
                false
            },
            |_| Duration::ZERO,
        );
        assert_eq!(calls.get(), 3);
        assert_eq!(waits, [Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[test]
    fn failed_attempt_drops_resources_before_backoff_and_reentry() {
        struct Lease<'a>(&'a std::cell::Cell<usize>);
        impl Drop for Lease<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        let stop = AtomicBool::new(false);
        let calls = std::cell::Cell::new(0);
        let active = std::cell::Cell::new(0);
        supervise(
            &stop,
            || {
                assert_eq!(active.get(), 0);
                active.set(active.get() + 1);
                let _lease = Lease(&active);
                calls.set(calls.get() + 1);
                assert_ne!(calls.get(), 1, "unwind with attempt-local resources");
                stop.store(true, Ordering::Release);
            },
            |_| {
                assert_eq!(active.get(), 0);
                false
            },
            |_| Duration::ZERO,
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(active.get(), 0);
    }

    #[test]
    fn shutdown_before_start_or_during_unwind_never_restarts() {
        let stopped = AtomicBool::new(true);
        supervise(
            &stopped,
            || panic!("stopped worker entered an attempt"),
            |_| panic!("stopped worker entered backoff"),
            |_| panic!("stopped worker measured an attempt"),
        );
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        supervise(
            &stop,
            || {
                calls += 1;
                stop.store(true, Ordering::Release);
                panic!("shutdown races unwind");
            },
            |_| panic!("shutdown entered backoff"),
            |_| panic!("shutdown entered restart scheduling"),
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn interrupted_backoff_and_shutdown_after_wait_prevent_reentry() {
        for interrupted in [false, true] {
            let stop = AtomicBool::new(false);
            let mut calls = 0;
            supervise(
                &stop,
                || calls += 1,
                |_| {
                    if !interrupted {
                        stop.store(true, Ordering::Release);
                    }
                    interrupted
                },
                |_| Duration::ZERO,
            );
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn stable_running_attempt_resets_supervisor_delay() {
        let stop = AtomicBool::new(false);
        let calls = std::cell::Cell::new(0);
        let mut delays = Vec::new();
        supervise(
            &stop,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 4 {
                    stop.store(true, Ordering::Release);
                }
            },
            |delay| {
                delays.push(delay.as_secs());
                false
            },
            |_| {
                if calls.get() == 3 {
                    MAX_RESTART_DELAY
                } else {
                    Duration::ZERO
                }
            },
        );
        assert_eq!(delays, [1, 2, 1]);
    }

    #[test]
    fn unwinding_after_a_real_grant_resumes_without_replaying_it() {
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let (temp, config) = fixture();
            std::fs::create_dir_all(temp.path().join("mailbox")).unwrap();
            let control = seed(&config);
            let selected = selected_pool(&config);
            let cx = Cx::for_testing();
            let stop = AtomicBool::new(false);
            let attempts = std::cell::Cell::new(0);
            let original = std::cell::RefCell::new(None);
            let read_claims = || {
                let conn = block_on(control.acquire(&cx)).into_result().unwrap();
                conn.query_sync(
                    "SELECT id, agent_id, expires_ts FROM file_reservations ORDER BY id",
                    &[],
                )
                .unwrap()
                .iter()
                .map(|row| {
                    (
                        row.get_named::<i64>("id").unwrap(),
                        row.get_named::<i64>("agent_id").unwrap(),
                        row.get_named::<i64>("expires_ts").unwrap(),
                    )
                })
                .collect::<Vec<_>>()
            };
            supervise(
                &stop,
                || {
                    attempts.set(attempts.get() + 1);
                    let mut pool = None;
                    let mut state = AckScanState::default();
                    assert_eq!(
                        run_pass(&config, &selected, &mut pool, &mut state, &stop, || true)
                            .unwrap(),
                        (1, 1)
                    );
                    let claims = read_claims();
                    if attempts.get() == 1 {
                        assert_eq!(claims.len(), 1);
                        let conn = block_on(control.acquire(&cx)).into_result().unwrap();
                        let generation = conn
                            .query_sync(
                                "SELECT generation_id FROM db_identity WHERE singleton = 0",
                                &[],
                            )
                            .unwrap()[0]
                            .get_named::<String>("generation_id")
                            .unwrap();
                        drop(conn);
                        let filename = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
                            Some(&generation), claims[0].0,
                        );
                        let path = config
                            .storage_root
                            .join("projects/ack-worker/file_reservations")
                            .join(filename);
                        let bytes = std::fs::read(&path).unwrap();
                        *original.borrow_mut() = Some((claims[0], path, bytes));
                        panic!("injected failure after a committed ACK escalation");
                    }
                    assert_eq!(attempts.get(), 2);
                    assert_eq!(claims.len(), 2);
                    let retained = original.borrow();
                    let (before, path, bytes) = retained.as_ref().unwrap();
                    assert_eq!(claims[0], *before, "old grant was duplicated or renewed");
                    assert_eq!(claims[1].1, 81, "new delivery must receive its own grant");
                    assert_eq!(std::fs::read(path).unwrap().as_slice(), bytes.as_slice());
                    stop.store(true, Ordering::Release);
                },
                |delay| {
                    assert_eq!(delay, MIN_RESTART_DELAY);
                    assert_eq!(attempts.get(), 1);
                    block_on(mcp_agent_mail_db::queries::acknowledge_message(
                        &cx, &control, 82, 1,
                    ))
                    .into_result()
                    .unwrap();
                    let conn = block_on(control.acquire(&cx)).into_result().unwrap();
                    conn.execute_raw("INSERT INTO messages(id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) VALUES(2, 71, 82, 'after worker restart', 'New body', 'normal', 1, 2000000, '[]', '{}')").unwrap();
                    conn.execute_raw("INSERT INTO message_recipients(message_id, agent_id, kind) VALUES(2, 81, 'to')").unwrap();
                    false
                },
                |_| Duration::ZERO,
            );
            assert_eq!(attempts.get(), 2);
            let conn = block_on(control.acquire(&cx)).into_result().unwrap();
            let receipts = conn
                .query_sync(
                    "SELECT message_id, ack_ts FROM message_recipients ORDER BY message_id",
                    &[],
                )
                .unwrap();
            assert_eq!(receipts.len(), 2);
            assert!(
                receipts[0]
                    .get_named::<Option<i64>>("ack_ts")
                    .unwrap()
                    .is_some()
            );
            assert!(
                receipts[1]
                    .get_named::<Option<i64>>("ack_ts")
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn promotion_deferral_keeps_the_worker_pool_and_continuation() {
        const CHILD: &str = "AM_TEST_ACK_WORKER_POOL_ADMISSION_CHILD";
        let name = std::thread::current()
            .name()
            .expect("named test thread")
            .to_string();
        if std::env::var(CHILD).ok().as_deref() != Some(name.as_str()) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture", "--test-threads=1"])
                .env(CHILD, &name)
                .output()
                .expect("run isolated ACK worker admission test");
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .contains("ACK worker pool admission completed"),
                "isolated ACK worker admission failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
            let (temp, mut config) = fixture();
            std::fs::create_dir_all(temp.path().join("mailbox")).unwrap();
            config.ack_escalation_enabled = false;
            let control = seed(&config);
            let cx = Cx::for_testing();
            block_on(mcp_agent_mail_db::queries::db_generation_id(&cx, &control))
                .into_result()
                .unwrap();
            seed_tail(&control, &cx);
            let selected = selected_pool(&config);
            let stop = AtomicBool::new(false);
            let mut pool = None;
            let mut state = AckScanState::default();
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || false).unwrap(),
                (3, 1)
            );
            let engine = pool.as_ref().unwrap().engine_identity_key();
            let cursor = state.cursor.clone();
            let identity = state.identity.clone();
            let warned = state.warned.clone();
            let current = state.current.clone();
            assert!(cursor.is_some());
            let promotion =
                mcp_agent_mail_db::write_barrier::try_acquire_promotion_barrier_if_idle()
                    .expect("isolated process has no active writer");
            let result = run_pass(&config, &selected, &mut pool, &mut state, &stop, || {
                panic!("admission refusal must not process a row")
            });
            drop(promotion);
            let error = result.expect_err("promotion must refuse the scan");
            assert_eq!(error, "ACK scan deferred: recovery write admission is busy");
            assert!(pool.is_some(), "contention must not discard a usable pool");
            assert_eq!(pool.as_ref().unwrap().engine_identity_key(), engine);
            assert_eq!(state.cursor, cursor);
            assert_eq!(state.identity, identity);
            assert_eq!(state.warned, warned);
            assert_eq!(state.current, current);
            assert_eq!(
                next_ack_scan_delay(Duration::from_secs(60), true, true),
                Duration::from_secs(60)
            );
            assert_eq!(
                run_pass(&config, &selected, &mut pool, &mut state, &stop, || true).unwrap(),
                (2, 2)
            );
            assert_eq!(pool.as_ref().unwrap().engine_identity_key(), engine);
            assert!(state.cursor.is_none());
            assert_eq!(state.warned.len(), 3);
        });
        println!("ACK worker pool admission completed");
    }
}
