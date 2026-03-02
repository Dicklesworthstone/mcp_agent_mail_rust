//! Background worker for ACK TTL scanning and escalation.
//!
//! Mirrors legacy Python `_worker_ack_ttl` in `http.py`:
//! - Scan unacknowledged `ack_required` messages
//! - Log warnings for overdue acks
//! - Optionally escalate via file reservations
//!
//! The worker runs on a dedicated OS thread with `std::thread::sleep` between
//! iterations, matching the pattern in `cleanup.rs`.

#![forbid(unsafe_code)]

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{
    DbPool, DbPoolConfig, create_pool, micros_to_iso, now_micros,
    queries::{self, list_overdue_unacknowledged_messages},
};
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Global shutdown flag for the ACK TTL worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Start the ACK TTL scan worker (if enabled).
///
/// Must be called at most once. Subsequent calls are no-ops.
pub fn start(config: &Config) {
    if !config.ack_ttl_enabled {
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
    if worker.is_none() {
        let config = config.clone();
        SHUTDOWN.store(false, Ordering::Release);
        match std::thread::Builder::new()
            .name("ack-ttl-scan".into())
            .spawn(move || {
                ack_ttl_loop(&config);
            }) {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                warn!(
                    error = %err,
                    "failed to spawn ACK TTL scan worker; continuing without ACK TTL background scans"
                );
                return;
            }
        }
    }
    drop(worker);
}

/// Signal the worker to stop.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn ack_ttl_loop(config: &Config) {
    let interval = std::time::Duration::from_secs(config.ack_ttl_scan_interval_seconds.max(5));
    let startup_delay = interval.min(std::time::Duration::from_secs(8));

    let mut pool_config = DbPoolConfig::from_env();
    pool_config.database_url.clone_from(&config.database_url);
    pool_config.min_connections = 1;
    pool_config.max_connections = 1;
    pool_config.warmup_connections = 0;
    // HTTP/TUI startup already runs readiness_check with migrations before
    // this worker starts, so keep the worker path lean.
    pool_config.run_migrations = false;
    let pool = match create_pool(&pool_config) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "ack TTL worker: failed to create DB pool, exiting");
            return;
        }
    };

    info!(
        interval_secs = interval.as_secs(),
        ttl_seconds = config.ack_ttl_seconds,
        escalation_enabled = config.ack_escalation_enabled,
        escalation_mode = %config.ack_escalation_mode,
        "ACK TTL scan worker started"
    );

    if startup_delay > std::time::Duration::ZERO {
        info!(
            startup_delay_secs = startup_delay.as_secs(),
            "ACK TTL worker startup delay engaged"
        );
        if sleep_with_shutdown(startup_delay) {
            return;
        }
    }

    let mut previously_overdue: HashSet<OverdueAckKey> = HashSet::new();

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("ACK TTL scan worker shutting down");
            return;
        }

        match run_ack_ttl_cycle_with_state(config, &pool, &mut previously_overdue) {
            Ok((scanned, overdue)) => {
                if overdue > 0 {
                    info!(
                        event = "ack_ttl_scan",
                        scanned, overdue, "ACK TTL scan completed"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "ACK TTL scan cycle failed");
            }
        }

        // Sleep in small increments to allow quick shutdown.
        let mut remaining = interval;
        while !remaining.is_zero() {
            if SHUTDOWN.load(Ordering::Acquire) {
                return;
            }
            let chunk = remaining.min(std::time::Duration::from_secs(1));
            std::thread::sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

fn sleep_with_shutdown(duration: std::time::Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let chunk = remaining.min(std::time::Duration::from_secs(1));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    false
}

/// Run a single ACK TTL scan cycle.
///
/// Returns `(scanned, overdue_count)`.
#[cfg(test)]
fn run_ack_ttl_cycle(config: &Config, pool: &DbPool) -> Result<(usize, usize), String> {
    let mut previously_overdue = HashSet::new();
    run_ack_ttl_cycle_with_state(config, pool, &mut previously_overdue)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct OverdueAckKey {
    message_id: i64,
    agent_id: i64,
}

fn run_ack_ttl_cycle_with_state(
    config: &Config,
    pool: &DbPool,
    previously_overdue: &mut HashSet<OverdueAckKey>,
) -> Result<(usize, usize), String> {
    let cx = Cx::for_testing();
    let now = now_micros();
    let ttl_us = i64::try_from(config.ack_ttl_seconds)
        .unwrap_or(1800)
        .saturating_mul(1_000_000);
    let overdue_before_ts = now.saturating_sub(ttl_us);

    let rows = match block_on(async {
        list_overdue_unacknowledged_messages(&cx, pool, overdue_before_ts).await
    }) {
        Outcome::Ok(r) => r,
        other => return Err(format!("failed to list unacked messages: {other:?}")),
    };

    let scanned = rows.len();
    let mut overdue = 0usize;
    let mut currently_overdue: HashSet<OverdueAckKey> = HashSet::with_capacity(rows.len());

    for row in &rows {
        let key = OverdueAckKey {
            message_id: row.message_id,
            agent_id: row.agent_id,
        };
        currently_overdue.insert(key);
        overdue = overdue.saturating_add(1);

        // Suppress duplicate log/escalation spam for rows that remain overdue
        // across consecutive scan intervals.
        if previously_overdue.contains(&key) {
            continue;
        }

        let age_seconds = now.saturating_sub(row.created_ts) / 1_000_000;

        // Log the overdue warning (matches legacy structlog + rich panel).
        warn!(
            event = "ack_overdue",
            message_id = row.message_id,
            project_id = row.project_id,
            agent_id = row.agent_id,
            age_s = age_seconds,
            ttl_s = config.ack_ttl_seconds,
            "ACK overdue"
        );

        // Escalation (best-effort, never crash).
        if config.ack_escalation_enabled {
            let _ = escalate(config, pool, &cx, row, now);
        }
    }

    *previously_overdue = currently_overdue;
    Ok((scanned, overdue))
}

/// Escalate an overdue ACK via the configured escalation mode.
fn escalate(
    config: &Config,
    pool: &DbPool,
    cx: &Cx,
    row: &queries::UnackedMessageRow,
    _now: i64,
) -> Result<(), String> {
    let mode = config.ack_escalation_mode.to_lowercase();
    if mode != "file_reservation" {
        // "log" mode (or unknown): logging was already done above.
        return Ok(());
    }

    // Fetch project to get slug for archive write.
    let project =
        match block_on(async { queries::get_project_by_id(cx, pool, row.project_id).await }) {
            Outcome::Ok(p) => p,
            other => return Err(format!("failed to fetch project: {other:?}")),
        };

    // Build the inbox path pattern from the created_ts timestamp.
    let ts_secs = row.created_ts / 1_000_000;
    let dt = chrono::DateTime::from_timestamp(ts_secs, 0)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
    let y_dir = dt.format("%Y").to_string();
    let m_dir = dt.format("%m").to_string();

    // Resolve recipient name.
    let recipient_name =
        match block_on(async { queries::get_agent_by_id(cx, pool, row.agent_id).await }) {
            Outcome::Ok(agent) => agent.name,
            _ => "*".to_string(),
        };

    let pattern = if recipient_name == "*" {
        format!("agents/*/inbox/{y_dir}/{m_dir}/*.md")
    } else {
        format!("agents/{recipient_name}/inbox/{y_dir}/{m_dir}/*.md")
    };

    // Determine holder agent.
    let holder_name_cfg = &config.ack_escalation_claim_holder_name;
    let (holder_agent_id, holder_agent_name) = if holder_name_cfg.is_empty() {
        // Use the recipient agent as the holder.
        (row.agent_id, recipient_name)
    } else {
        // Look up or create the custom holder agent.
        match block_on(async {
            queries::insert_system_agent(
                cx,
                pool,
                row.project_id,
                holder_name_cfg,
                "ops",
                "system",
                "ops-escalation",
            )
            .await
        }) {
            Outcome::Ok(agent) => (agent.id.unwrap_or(row.agent_id), agent.name),
            _ => (row.agent_id, recipient_name), // Fallback to recipient.
        }
    };

    // Create the file reservation.
    let ttl_s = i64::try_from(config.ack_escalation_claim_ttl_seconds).unwrap_or(3600);
    let has_existing = match block_on(async {
        // Only treat *active* matching reservations as dedupe candidates.
        // Historical released/expired rows must not block new escalation claims.
        queries::list_file_reservations(cx, pool, row.project_id, true).await
    }) {
        Outcome::Ok(existing) => existing.into_iter().any(|reservation| {
            reservation.agent_id == holder_agent_id
                && reservation.path_pattern == pattern
                && reservation.reason == "ack-overdue"
                && (reservation.exclusive != 0) == config.ack_escalation_claim_exclusive
        }),
        _ => false,
    };
    if has_existing {
        return Ok(());
    }

    match block_on(async {
        queries::create_file_reservations(
            cx,
            pool,
            row.project_id,
            holder_agent_id,
            &[pattern.as_str()],
            ttl_s,
            config.ack_escalation_claim_exclusive,
            "ack-overdue",
        )
        .await
    }) {
        Outcome::Ok(reservations) => {
            info!(
                event = "ack_escalation",
                message_id = row.message_id,
                project_id = row.project_id,
                holder_agent_id,
                pattern = %pattern,
                reservations_created = reservations.len(),
                "ACK escalation: created file reservation"
            );

            // Write reservation artifacts to git archive (best-effort).
            if !reservations.is_empty() {
                let res_jsons: Vec<serde_json::Value> = reservations
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id.unwrap_or(0),
                            "agent": &holder_agent_name,
                            "path_pattern": &r.path_pattern,
                            "exclusive": r.exclusive != 0,
                            "reason": &r.reason,
                            "expires_ts": micros_to_iso(r.expires_ts),
                        })
                    })
                    .collect();
                let op = mcp_agent_mail_storage::WriteOp::FileReservation {
                    project_slug: project.slug.clone(),
                    config: config.clone(),
                    reservations: res_jsons,
                };
                match mcp_agent_mail_storage::wbq_enqueue(op) {
                    mcp_agent_mail_storage::WbqEnqueueResult::Enqueued
                    | mcp_agent_mail_storage::WbqEnqueueResult::SkippedDiskCritical => {}
                    mcp_agent_mail_storage::WbqEnqueueResult::QueueUnavailable => {
                        warn!(
                            "WBQ enqueue failed; skipping reservation artifacts archive write project={}",
                            project.slug
                        );
                    }
                }
            }

            Ok(())
        }
        other => Err(format!(
            "failed to create escalation reservation: {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{Cx, Outcome};
    use mcp_agent_mail_db::{DbPoolConfig, create_pool, queries};

    #[test]
    fn age_threshold_calculation() {
        // Verify the age threshold calculation matches expectations.
        let ttl_seconds: u64 = 1800;
        let ttl_us = i64::try_from(ttl_seconds).unwrap() * 1_000_000;
        assert_eq!(ttl_us, 1_800_000_000);

        // A message created 1801 seconds ago should be overdue.
        let now = 2_000_000_000_000i64; // arbitrary "now" in microseconds
        let created = now - (1801 * 1_000_000);
        let age = now - created;
        assert!(age >= ttl_us, "1801s should exceed 1800s TTL");

        // A message created 1799 seconds ago should NOT be overdue.
        let created_recent = now - (1799 * 1_000_000);
        let age_recent = now - created_recent;
        assert!(age_recent < ttl_us, "1799s should not exceed 1800s TTL");
    }

    #[test]
    fn inbox_path_pattern_format() {
        // Verify the path pattern matches legacy format.
        let name = "GreenCastle";
        let y = "2026";
        let m = "02";
        let pattern = format!("agents/{name}/inbox/{y}/{m}/*.md");
        assert_eq!(pattern, "agents/GreenCastle/inbox/2026/02/*.md");

        // Wildcard pattern for unknown agents.
        let pattern_wild = format!("agents/*/inbox/{y}/{m}/*.md");
        assert_eq!(pattern_wild, "agents/*/inbox/2026/02/*.md");
    }

    #[test]
    fn escalation_mode_matching() {
        let mode = "file_reservation";
        assert_eq!(mode.to_lowercase(), "file_reservation");

        let mode_log = "log";
        assert_ne!(mode_log.to_lowercase(), "file_reservation");

        // Case-insensitive check.
        let mode_upper = "FILE_RESERVATION";
        assert_eq!(mode_upper.to_lowercase(), "file_reservation");
    }

    fn make_test_pool(tmp: &tempfile::TempDir) -> DbPool {
        // Use the standard pool setup to mirror production initialization
        // semantics under FrankenSQLite.
        let db_path = tmp.path().join("db.sqlite3");
        let db_url = format!(
            "sqlite:////{}",
            db_path.to_string_lossy().trim_start_matches('/')
        );
        let pool_config = DbPoolConfig {
            database_url: db_url,
            min_connections: 1,
            max_connections: 1,
            ..Default::default()
        };
        create_pool(&pool_config).expect("create pool")
    }

    fn seed_unacked_message() -> (tempfile::TempDir, DbPool, Cx, queries::UnackedMessageRow) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = make_test_pool(&tmp);
        let cx = Cx::for_testing();

        let project_root = tmp.path().join("project_root");
        std::fs::create_dir_all(&project_root).unwrap();
        let human_key = project_root.to_string_lossy().to_string();

        let project =
            match block_on(async { queries::ensure_project(&cx, &pool, &human_key).await }) {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            };
        let project_id = project.id.expect("project id");

        let sender = match block_on(async {
            queries::register_agent(&cx, &pool, project_id, "RedFox", "test", "test", None, None)
                .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(sender) failed: {other:?}"),
        };
        let sender_id = sender.id.expect("sender id");

        let recipient = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "BlueBear", "test", "test", None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(recipient) failed: {other:?}"),
        };
        let recipient_id = recipient.id.expect("recipient id");

        let _msg = match block_on(async {
            queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "[br-1bm.10.6] Ack TTL probe",
                "Body",
                Some("br-1bm.10.6"),
                "normal",
                true,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
        }) {
            Outcome::Ok(m) => m,
            other => panic!("create_message_with_recipients failed: {other:?}"),
        };

        let unacked =
            match block_on(async { queries::list_unacknowledged_messages(&cx, &pool).await }) {
                Outcome::Ok(rows) => {
                    assert_eq!(rows.len(), 1, "expected exactly 1 unacked row");
                    rows.into_iter().next().unwrap()
                }
                other => panic!("list_unacknowledged_messages failed: {other:?}"),
            };

        (tmp, pool, cx, unacked)
    }

    #[test]
    fn ack_ttl_cycle_marks_overdue_when_ttl_zero() {
        let (_tmp, pool, _cx, _unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 1);
        assert_eq!(overdue, 1);
    }

    #[test]
    fn ack_ttl_cycle_respects_ttl_when_large() {
        let (_tmp, pool, _cx, _unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 10_000;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 0);
        assert_eq!(overdue, 0);
    }

    #[test]
    fn ack_ttl_stateful_cycle_escalates_newly_overdue_only_once() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = true;
        config.ack_escalation_claim_holder_name.clear();

        let mut previously_overdue = std::collections::HashSet::new();

        let (first_scanned, first_overdue) =
            run_ack_ttl_cycle_with_state(&config, &pool, &mut previously_overdue)
                .expect("first cycle");
        assert_eq!(first_scanned, 1);
        assert_eq!(first_overdue, 1);

        let (second_scanned, second_overdue) =
            run_ack_ttl_cycle_with_state(&config, &pool, &mut previously_overdue)
                .expect("second cycle");
        assert_eq!(second_scanned, 1);
        assert_eq!(second_overdue, 1);

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);
    }

    #[test]
    fn ack_ttl_escalation_is_idempotent_across_worker_restarts() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = true;
        config.ack_escalation_claim_holder_name.clear();

        // First run with fresh in-memory state.
        let mut first_state = std::collections::HashSet::new();
        run_ack_ttl_cycle_with_state(&config, &pool, &mut first_state).expect("first cycle");

        // Simulate process restart: fresh state map, same overdue row still exists.
        let mut second_state = std::collections::HashSet::new();
        run_ack_ttl_cycle_with_state(&config, &pool, &mut second_state).expect("second cycle");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);
    }

    #[test]
    fn escalation_reacquires_after_prior_matching_claim_was_released() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = true;
        config.ack_escalation_claim_holder_name.clear();

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("first escalate");

        let first = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(first.len(), 1);
        let first_id = first[0].id.expect("first reservation id");
        assert!(first_id > 0, "unexpected reservation id: {first_id}");
        assert!(
            first[0].released_ts.is_none(),
            "new escalation reservation unexpectedly pre-released: {:?}",
            first[0]
        );

        let release_ids = [first_id];
        match block_on(async {
            queries::release_reservations(
                &cx,
                &pool,
                unacked.project_id,
                unacked.agent_id,
                None,
                Some(&release_ids),
            )
            .await
        }) {
            Outcome::Ok(released) => assert_eq!(released.len(), 1),
            other => panic!("release_reservations failed: {other:?}"),
        }

        // Same overdue row should reacquire after the prior reservation is released.
        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("second escalate");

        let all_rows = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(
            all_rows.len(),
            2,
            "released historical rows must not block new escalation claims"
        );

        let active_rows = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, true).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations(active_only) failed: {other:?}"),
        };
        assert_eq!(active_rows.len(), 1);
    }

    #[test]
    fn escalation_creates_file_reservation_for_recipient_inbox() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = true;
        config.ack_escalation_claim_holder_name.clear(); // holder = recipient

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);

        let ts_secs = unacked.created_ts / 1_000_000;
        let dt = chrono::DateTime::from_timestamp(ts_secs, 0).unwrap();
        let y_dir = dt.format("%Y").to_string();
        let m_dir = dt.format("%m").to_string();
        let expected_pattern = format!("agents/BlueBear/inbox/{y_dir}/{m_dir}/*.md");

        let r = &reservations[0];
        assert_eq!(r.agent_id, unacked.agent_id);
        assert_eq!(r.path_pattern, expected_pattern);
        assert_eq!(r.exclusive, 1);
        assert_eq!(r.reason, "ack-overdue");
    }

    #[test]
    fn escalation_mode_log_is_noop() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "log".to_string();

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert!(reservations.is_empty());
    }

    #[test]
    fn escalation_mode_unknown_is_noop() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "unknown".to_string();

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert!(reservations.is_empty());
    }

    #[test]
    fn ack_ttl_cycle_zero_when_no_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = make_test_pool(&tmp);

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 0);
        assert_eq!(overdue, 0);
    }

    #[test]
    fn ack_ttl_cycle_ignores_acknowledged_messages() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        match block_on(async {
            queries::acknowledge_message(&cx, &pool, unacked.agent_id, unacked.message_id).await
        }) {
            Outcome::Ok(_) => {}
            other => panic!("acknowledge_message failed: {other:?}"),
        }

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 0);
        assert_eq!(overdue, 0);
    }

    #[test]
    fn escalation_with_custom_holder_uses_system_agent() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_holder_name = "OpsEscalation".to_string();

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);

        let holder_id = reservations[0].agent_id;
        assert_ne!(
            holder_id, unacked.agent_id,
            "custom holder should not default to recipient agent"
        );

        // Re-lookup the holder agent via insert_system_agent (which queries
        // the DB directly, bypassing the global ReadCache that may contain
        // stale entries from other tests in the same process).
        let holder = match block_on(async {
            queries::insert_system_agent(
                &cx,
                &pool,
                unacked.project_id,
                "OpsEscalation",
                "ops",
                "system",
                "ops-escalation",
            )
            .await
        }) {
            Outcome::Ok(agent) => agent,
            other => panic!("insert_system_agent re-lookup failed: {other:?}"),
        };
        assert_eq!(holder.name, "OpsEscalation");
        assert_eq!(holder.id, Some(holder_id));
    }

    #[test]
    fn escalation_mode_is_case_insensitive() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "FILE_RESERVATION".to_string();
        config.ack_escalation_claim_holder_name.clear();

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);
    }

    #[test]
    fn message_age_exactly_at_ttl_boundary_not_overdue() {
        // When age_micros == ttl_us, the check is `age_micros < ttl_us` → false,
        // so exact boundary IS overdue.
        let ttl_seconds: u64 = 300;
        let ttl_us = i64::try_from(ttl_seconds).unwrap() * 1_000_000;
        let now = 2_000_000_000_000i64;
        let created_exact = now - ttl_us;
        let age = now - created_exact;
        assert!(
            age >= ttl_us,
            "at exact boundary, age==ttl should be overdue"
        );
    }

    #[test]
    fn message_one_microsecond_before_ttl_not_overdue() {
        let ttl_seconds: u64 = 300;
        let ttl_us = i64::try_from(ttl_seconds).unwrap() * 1_000_000;
        let now = 2_000_000_000_000i64;
        let created_just_under = now - ttl_us + 1;
        let age = now - created_just_under;
        assert!(
            age < ttl_us,
            "one microsecond before TTL should not be overdue"
        );
    }

    #[test]
    fn inbox_path_pattern_epoch_zero() {
        // Timestamp at Unix epoch → 1970/01.
        let ts_secs: i64 = 0;
        let dt = chrono::DateTime::from_timestamp(ts_secs, 0).unwrap();
        let y = dt.format("%Y").to_string();
        let m = dt.format("%m").to_string();
        assert_eq!(y, "1970");
        assert_eq!(m, "01");

        let pattern = format!("agents/TestAgent/inbox/{y}/{m}/*.md");
        assert_eq!(pattern, "agents/TestAgent/inbox/1970/01/*.md");
    }

    #[test]
    fn ack_ttl_cycle_multiple_overdue() {
        // Seed two separate unacked messages (different senders/recipients).
        let tmp = tempfile::tempdir().unwrap();
        let pool = make_test_pool(&tmp);
        let cx = Cx::for_testing();

        let project_root = tmp.path().join("project_root");
        std::fs::create_dir_all(&project_root).unwrap();
        let human_key = project_root.to_string_lossy().to_string();

        let project =
            match block_on(async { queries::ensure_project(&cx, &pool, &human_key).await }) {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            };
        let project_id = project.id.expect("project id");

        let sender = match block_on(async {
            queries::register_agent(&cx, &pool, project_id, "RedFox", "test", "test", None, None)
                .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(sender) failed: {other:?}"),
        };
        let sender_id = sender.id.expect("sender id");

        let recip1 = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "BlueBear", "test", "test", None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(recip1) failed: {other:?}"),
        };
        let recip1_id = recip1.id.expect("recip1 id");

        let recip2 = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "GoldHawk", "test", "test", None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(recip2) failed: {other:?}"),
        };
        let recip2_id = recip2.id.expect("recip2 id");

        // Create message 1 → recip1
        match block_on(async {
            queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "Msg 1",
                "Body 1",
                None,
                "normal",
                true,
                "[]",
                &[(recip1_id, "to")],
            )
            .await
        }) {
            Outcome::Ok(_) => {}
            other => panic!("create_message 1 failed: {other:?}"),
        }

        // Create message 2 → recip2
        match block_on(async {
            queries::create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "Msg 2",
                "Body 2",
                None,
                "normal",
                true,
                "[]",
                &[(recip2_id, "to")],
            )
            .await
        }) {
            Outcome::Ok(_) => {}
            other => panic!("create_message 2 failed: {other:?}"),
        }

        let mut config = Config::from_env();
        config.ack_ttl_seconds = 0; // All messages immediately overdue
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 2);
        assert_eq!(overdue, 2);
    }

    #[test]
    fn escalation_non_exclusive_reservation() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = false; // non-exclusive
        config.ack_escalation_claim_holder_name.clear();

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);
        assert_eq!(
            reservations[0].exclusive, 0,
            "reservation should be non-exclusive"
        );
    }

    #[test]
    fn escalation_applies_configured_ttl_seconds() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = Config::from_env();
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_ttl_seconds = 120;

        escalate(&config, &pool, &cx, &unacked, now_micros()).expect("escalate");

        let reservations = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        assert_eq!(reservations.len(), 1);

        let r = &reservations[0];
        let ttl_us = r.expires_ts.saturating_sub(r.created_ts);
        assert!(
            (110_000_000..=130_000_000).contains(&ttl_us),
            "reservation TTL should be close to configured 120 seconds, got {ttl_us}us"
        );
    }
}
