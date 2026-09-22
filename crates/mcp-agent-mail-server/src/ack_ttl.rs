//! Background worker for bounded ACK TTL scanning and escalation.
//!
//! Mirrors legacy Python `_worker_ack_ttl` in `http.py`:
//! - Scan unacknowledged `ack_required` messages
//! - Log warnings for overdue acks
//! - Optionally escalate via file reservations
//!
//! Each lap freezes its cutoff and message high-water mark. Fresh keyset pages
//! and consumed-prefix continuations prevent new mail or acknowledgments from
//! starving old deliveries. The worker yields its generation/write lease between
//! slices instead of retaining it while processing the whole backlog.
//!
//! Warning suppression is bounded and separate from escalation reconciliation:
//! active claims are checked again on later laps, including after an error.
//! Attempts are coalesced per recipient/month within each slice, and lookup
//! failures never authorize a broader inbox pattern or a different holder.
//! A proposal is revalidated in the grant transaction using its observed
//! generation and frozen cutoff. Only a confirmed new grant authorizes archive
//! publication; a stale delivery cannot suppress a pending sibling's proposal.

#![forbid(unsafe_code)]

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::ack_scan::{
    AckEscalationOutcome, AckEscalationRequest, AckScanCursor, MAX_ACK_SCAN_PAGE_SIZE,
    grant_ack_escalation, overdue_ack_page,
};
use mcp_agent_mail_db::{DbPool, DbPoolConfig, create_pool, micros_to_iso, now_micros, queries};
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const ACK_SCAN_SLICE_BUDGET: Duration = Duration::from_millis(25);
const ACK_SCAN_CONTINUATION_PAUSE: Duration = Duration::from_millis(250);
const MAX_ACK_WARNING_KEYS: usize = 4096;

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
            .stack_size(mcp_agent_mail_core::worker_stack_size())
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
    let interval = Duration::from_secs(config.ack_ttl_scan_interval_seconds.max(5));
    let startup_delay = interval.min(Duration::from_secs(8));

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

    if startup_delay > Duration::ZERO {
        info!(
            startup_delay_secs = startup_delay.as_secs(),
            "ACK TTL worker startup delay engaged"
        );
        if sleep_with_shutdown(startup_delay) {
            return;
        }
    }

    let mut state = AckScanState::default();
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("ACK TTL scan worker shutting down");
            return;
        }
        let started = Instant::now();
        let result = run_ack_ttl_slice(
            config,
            &pool,
            &mut state,
            || SHUTDOWN.load(Ordering::Acquire),
            || started.elapsed() < ACK_SCAN_SLICE_BUDGET,
        );
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
                warn!(error = %error, "ACK TTL scan slice failed; retaining continuation");
            }
        }
        // No database/write lease survives the slice call. A failed read must
        // not inherit the fast continuation cadence and create a retry storm.
        let delay = next_ack_scan_delay(interval, state.cursor.is_some(), failed);
        if sleep_with_shutdown(delay) {
            return;
        }
    }
}

fn sleep_with_shutdown(duration: Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let chunk = remaining.min(Duration::from_secs(1));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    false
}

/// Run one bounded page without wall-clock scheduling noise in legacy fixtures.
#[cfg(test)]
fn run_ack_ttl_cycle(config: &Config, pool: &DbPool) -> Result<(usize, usize), String> {
    run_ack_ttl_cycle_with_state(config, pool, &mut AckScanState::default())
}

#[cfg(test)]
fn run_ack_ttl_cycle_with_state(
    config: &Config,
    pool: &DbPool,
    state: &mut AckScanState,
) -> Result<(usize, usize), String> {
    run_ack_ttl_slice(config, pool, state, || false, || true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct OverdueAckKey {
    message_id: i64,
    agent_id: i64,
    created_ts: i64,
}

impl From<&queries::UnackedMessageRow> for OverdueAckKey {
    fn from(row: &queries::UnackedMessageRow) -> Self {
        Self {
            message_id: row.message_id,
            agent_id: row.agent_id,
            created_ts: row.created_ts,
        }
    }
}

/// Only cursor metadata and two bounded sets survive a slice. Warning history
/// is observational: it never suppresses an escalation attempt. Once the lap
/// completes, its observed keys replace the preceding lap's history. Large
/// backlogs can produce repeated warnings beyond the cache bound, not lost work.
#[derive(Default)]
struct AckScanState {
    cursor: Option<AckScanCursor>,
    identity: Option<(String, Option<String>)>,
    warned: HashSet<OverdueAckKey>,
    current: HashSet<OverdueAckKey>,
}

impl AckScanState {
    fn bind_scope(&mut self, scope: &str, generation: Option<&str>) {
        if self.identity.as_ref().is_some_and(|(old_scope, old_generation)| {
            old_scope == scope && old_generation.as_deref() == generation
        }) {
            return;
        }
        self.identity = Some((scope.to_string(), generation.map(str::to_string)));
        self.warned.clear();
        self.current.clear();
    }

    fn should_warn(&mut self, key: OverdueAckKey) -> bool {
        let already_seen = self.current.contains(&key);
        if self.current.len() < MAX_ACK_WARNING_KEYS {
            self.current.insert(key);
        }
        !already_seen && !self.warned.contains(&key)
    }

    fn finish_page(&mut self, cursor: Option<AckScanCursor>) {
        self.cursor = cursor;
        if self.cursor.is_none() {
            self.warned = std::mem::take(&mut self.current);
        }
    }
}

fn next_ack_scan_delay(interval: Duration, pending: bool, failed: bool) -> Duration {
    if pending && !failed {
        interval.min(ACK_SCAN_CONTINUATION_PAUSE)
    } else {
        interval
    }
}

/// One fresh observation and a consumed prefix. The time budget is cooperative:
/// it does not interrupt an individual query, escalation or archive operation.
/// Make one row of progress even if the query spends the processing budget, but
/// never override shutdown. Do not carry fetched payloads across recovery/yield.
fn run_ack_ttl_slice(
    config: &Config,
    pool: &DbPool,
    state: &mut AckScanState,
    mut stop: impl FnMut() -> bool,
    mut has_time: impl FnMut() -> bool,
) -> Result<(usize, usize), String> {
    if stop() {
        return Ok((0, 0));
    }
    // #219: keep generation exclusion through this page's escalations only.
    // Returning releases it BEFORE the worker pauses or reads its next page.
    let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
    if stop() {
        return Ok((0, 0));
    }
    let cx = block_on(async {
        Cx::current().expect("Runtime::block_on installs an ambient Cx for the polled future")
    });
    let now = now_micros();
    let ttl_us = i64::try_from(config.ack_ttl_seconds)
        .unwrap_or(1800)
        .saturating_mul(1_000_000);
    let page = match block_on(overdue_ack_page(
        &cx,
        pool,
        state.cursor.as_ref(),
        now.saturating_sub(ttl_us),
        MAX_ACK_SCAN_PAGE_SIZE,
    )) {
        Outcome::Ok(page) => page,
        other => return Err(format!("failed to read overdue ACK page: {other:?}")),
    };
    // Failed reads above preserve the continuation and warning history. A
    // successful changed-generation read has already restarted the DB cursor.
    state.bind_scope(page.database_scope(), page.generation_id());
    let scanned = page.rows.len();
    let mut consumed = 0;
    let mut attempted_escalations = HashSet::new();
    for row in &page.rows {
        if stop() || (consumed > 0 && !has_time()) {
            break;
        }
        if state.should_warn(OverdueAckKey::from(row)) {
            warn!(
                event = "ack_overdue",
                message_id = row.message_id,
                project_id = row.project_id,
                agent_id = row.agent_id,
                age_s = now.saturating_sub(row.created_ts) / 1_000_000,
                ttl_s = config.ack_ttl_seconds,
                "ACK overdue"
            );
        }
        // Staleness applies to one delivery, not its whole recipient/month.
        // Coalesce only a grant, existing coverage or a deferred error. Errors
        // remain bounded to one per group per slice and retry on a later lap.
        if config.ack_escalation_enabled {
            let key = (row.project_id, row.agent_id, inbox_month_path(row.created_ts));
            if !attempted_escalations.contains(&key) {
                let result = escalate_observed(
                    config, pool, &cx, row, page.generation_id(), page.overdue_before_ts(),
                );
                if !matches!(&result, Ok(false)) {
                    attempted_escalations.insert(key);
                }
                if let Err(error) = result {
                    warn!(
                        event = "ack_escalation_deferred",
                        message_id = row.message_id,
                        project_id = row.project_id,
                        agent_id = row.agent_id,
                        error = %error,
                        "ACK escalation failed; will retry on a later overdue scan"
                    );
                }
            }
        }
        consumed += 1;
    }
    let continuation = page
        .continuation_after(consumed)
        .map_err(|error| error.to_string())?;
    state.finish_page(continuation);
    // These counts describe this slice, not the size of the whole backlog.
    Ok((scanned, consumed))
}

fn inbox_month_path(created_ts: i64) -> String {
    // This is only a coalescing key. The grant API independently refuses an
    // unrepresentable timestamp instead of granting against an epoch fallback.
    let dt = chrono::DateTime::from_timestamp_micros(created_ts)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
    dt.format("%Y/%m").to_string()
}

/// Escalation paths must contain one literal agent component, never a glob
/// supplied by a failed lookup or malformed stored/configured identity.
fn validate_escalation_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.chars().any(|character| {
            character.is_control()
                || matches!(character, '/' | '\\' | '*' | '?' | '[' | ']' | '{' | '}')
        })
    {
        return Err("ACK escalation requires a literal, nonempty agent path component".to_string());
    }
    Ok(())
}

/// Execute with the original scan's generation and cutoff, never refreshed ones.
/// `Ok(false)` is a stale delivery: another message in its group must still be
/// considered. `Ok(true)` settles the group (grant, existing coverage or log mode).
fn escalate_observed(
    config: &Config,
    pool: &DbPool,
    cx: &Cx,
    row: &queries::UnackedMessageRow,
    generation_id: Option<&str>,
    overdue_before_ts: i64,
) -> Result<bool, String> {
    if !config.ack_escalation_mode.eq_ignore_ascii_case("file_reservation") {
        return Ok(true);
    }

    let project = match block_on(queries::get_project_by_id(cx, pool, row.project_id)) {
        Outcome::Ok(project) => project,
        other => return Err(format!("failed to fetch project: {other:?}")),
    };
    let recipient = match block_on(queries::get_agent_by_id(cx, pool, row.agent_id)) {
        Outcome::Ok(agent) => agent,
        other => return Err(format!("failed to resolve escalation recipient: {other:?}")),
    };
    if recipient.id != Some(row.agent_id) || recipient.project_id != row.project_id {
        return Err("ACK escalation recipient does not belong to the message project".to_string());
    }
    validate_escalation_agent_name(&recipient.name)?;
    let recipient_name = recipient.name;

    // Custom-holder setup remains preparatory. The eventual lease transaction
    // revalidates this exact holder, and never silently selects a replacement.
    let holder_name_cfg = &config.ack_escalation_claim_holder_name;
    let (holder_agent_id, holder_agent_name) = if holder_name_cfg.is_empty() {
        (row.agent_id, recipient_name.clone())
    } else {
        validate_escalation_agent_name(holder_name_cfg)?;
        let holder = match block_on(queries::insert_system_agent(
            cx,
            pool,
            row.project_id,
            holder_name_cfg,
            "ops",
            "system",
            "ops-escalation",
        )) {
            Outcome::Ok(agent) => agent,
            other => return Err(format!("failed to resolve custom escalation holder: {other:?}")),
        };
        if holder.project_id != row.project_id {
            return Err("ACK escalation holder belongs to another project".to_string());
        }
        validate_escalation_agent_name(&holder.name)?;
        let holder_id = holder
            .id
            .filter(|id| *id > 0)
            .ok_or_else(|| "ACK escalation holder has no valid database identity".to_string())?;
        (holder_id, holder.name)
    };

    let request = AckEscalationRequest {
        observed: row,
        overdue_before_ts,
        generation_id,
        project_slug: &project.slug,
        project_key: &project.human_key,
        recipient_name: &recipient_name,
        holder_id: holder_agent_id,
        holder_name: &holder_agent_name,
        ttl_seconds: i64::try_from(config.ack_escalation_claim_ttl_seconds).unwrap_or(3600),
        exclusive: config.ack_escalation_claim_exclusive,
    };
    let reservation = match block_on(grant_ack_escalation(cx, pool, &request)) {
        Outcome::Ok(AckEscalationOutcome::Granted(reservation)) => reservation,
        Outcome::Ok(AckEscalationOutcome::NoLongerOverdue) => return Ok(false),
        Outcome::Ok(AckEscalationOutcome::AlreadyCovered) => return Ok(true),
        other => return Err(format!("failed to admit escalation reservation: {other:?}")),
    };
    info!(
        event = "ack_escalation",
        message_id = row.message_id,
        project_id = row.project_id,
        holder_agent_id,
        pattern = %reservation.path_pattern,
        reservations_created = 1,
        "ACK escalation: committed file reservation"
    );

    let mut artifact = serde_json::json!({
        "id": reservation.id,
        "project": &project.human_key,
        "agent": &holder_agent_name,
        "path_pattern": &reservation.path_pattern,
        "exclusive": reservation.exclusive != 0,
        "reason": &reservation.reason,
        "created_ts": micros_to_iso(reservation.created_ts),
        "expires_ts": micros_to_iso(reservation.expires_ts),
    });
    if let Some(generation) = generation_id {
        artifact["db_generation"] = serde_json::json!(generation);
    }
    let op = mcp_agent_mail_storage::WriteOp::FileReservation {
        project_slug: project.slug.clone(),
        config: config.clone(),
        reservations: vec![artifact],
    };
    // Preserve direct grant publication (GH#178). A delayed queued active
    // grant could resurrect an artifact after a later directly written release.
    // The worker still holds its page's generation/write lease here. A failed
    // archive write is repaired by the existing reconcile-on-read path.
    match mcp_agent_mail_storage::write_op_sync_direct(&op) {
        mcp_agent_mail_storage::DirectArchiveWrite::Written
        | mcp_agent_mail_storage::DirectArchiveWrite::SkippedDiskCritical => {}
        mcp_agent_mail_storage::DirectArchiveWrite::Failed(error) => {
            warn!(
                error = %error,
                project = %project.slug,
                "ACK escalation archive write failed; leaving the artifact to reconcile-on-read"
            );
        }
    }
    Ok(true)
}

/// Existing direct-call fixtures create a fresh observation. Production always
/// passes the generation and cutoff of its already-fetched page.
#[cfg(test)]
fn escalate(
    config: &Config,
    pool: &DbPool,
    cx: &Cx,
    row: &queries::UnackedMessageRow,
    _now: i64,
) -> Result<(), String> {
    let page = match block_on(overdue_ack_page(cx, pool, None, i64::MAX, 1)) {
        Outcome::Ok(page) => page,
        other => return Err(format!("test observation: {other:?}")),
    };
    escalate_observed(config, pool, cx, row, page.generation_id(), page.overdue_before_ts())
        .map(|_| ())
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
        // Verify the path pattern matches legacy format for resolved agents.
        let name = "GreenCastle";
        let y = "2026";
        let m = "02";
        let pattern = format!("agents/{name}/inbox/{y}/{m}/*.md");
        assert_eq!(pattern, "agents/GreenCastle/inbox/2026/02/*.md");
        assert!(validate_escalation_agent_name(name).is_ok());
        assert!(validate_escalation_agent_name("*").is_err());
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

    /// `Config` for these tests: escalation writes reservation artifacts to
    /// the archive under `config.storage_root`, so it MUST point inside the
    /// test sandbox. `Config::from_env()` alone resolves to the operator's
    /// real mailbox archive and leaks junk projects into it (br-r6awv).
    fn test_config(tmp: &tempfile::TempDir) -> Config {
        let mut config = Config::from_env();
        config.storage_root = tmp.path().join("storage");
        config
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

    fn ensure_ephemeral_test_project(
        cx: &Cx,
        pool: &DbPool,
        human_key: &str,
    ) -> mcp_agent_mail_db::ProjectRow {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")],
            || match block_on(async { queries::ensure_project(cx, pool, human_key).await }) {
                Outcome::Ok(project) => project,
                other => panic!("ensure_project failed: {other:?}"),
            },
        )
    }

    fn seed_unacked_message() -> (tempfile::TempDir, DbPool, Cx, queries::UnackedMessageRow) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = make_test_pool(&tmp);
        let cx = Cx::for_testing();

        let project_root = tmp.path().join("project_root");
        std::fs::create_dir_all(&project_root).unwrap();
        let human_key = project_root.to_string_lossy().to_string();

        let project = ensure_ephemeral_test_project(&cx, &pool, &human_key);
        let project_id = project.id.expect("project id");

        let sender = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "RedFox", "test", "test", None, None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(sender, None) failed: {other:?}"),
        };
        let sender_id = sender.id.expect("sender id");

        let recipient = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "BlueBear", "test", "test", None, None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(recipient, None) failed: {other:?}"),
        };
        let recipient_id = recipient.id.expect("recipient id");

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build ack TTL fixture runtime");
        let _msg = match runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs ack TTL fixture context");
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

        let mut config = test_config(&_tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 1);
        assert_eq!(overdue, 1);
    }

    #[test]
    fn ack_ttl_cycle_respects_ttl_when_large() {
        let (_tmp, pool, _cx, _unacked) = seed_unacked_message();

        let mut config = test_config(&_tmp);
        config.ack_ttl_seconds = 10_000;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 0);
        assert_eq!(overdue, 0);
    }

    #[test]
    fn ack_ttl_stateful_cycle_escalates_newly_overdue_only_once() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = test_config(&_tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = true;
        config.ack_escalation_claim_holder_name.clear();

        let mut previously_overdue = AckScanState::default();

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

        let mut config = test_config(&_tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_exclusive = true;
        config.ack_escalation_claim_holder_name.clear();

        // First run with fresh in-memory state.
        let mut first_state = AckScanState::default();
        run_ack_ttl_cycle_with_state(&config, &pool, &mut first_state).expect("first cycle");

        // Simulate process restart: fresh state map, same overdue row still exists.
        let mut second_state = AckScanState::default();
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

        let mut config = test_config(&_tmp);
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

        let mut config = test_config(&_tmp);
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

        let mut config = test_config(&_tmp);
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

        let mut config = test_config(&_tmp);
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

        let mut config = test_config(&tmp);
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

        let mut config = test_config(&_tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 0);
        assert_eq!(overdue, 0);
    }

    #[test]
    fn escalation_with_custom_holder_uses_system_agent() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = test_config(&_tmp);
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

        let mut config = test_config(&_tmp);
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

        let project = ensure_ephemeral_test_project(&cx, &pool, &human_key);
        let project_id = project.id.expect("project id");

        let sender = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "RedFox", "test", "test", None, None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(sender, None) failed: {other:?}"),
        };
        let sender_id = sender.id.expect("sender id");

        let recip1 = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "BlueBear", "test", "test", None, None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(recip1, None) failed: {other:?}"),
        };
        let recip1_id = recip1.id.expect("recip1 id");

        let recip2 = match block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "GoldHawk", "test", "test", None, None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent(recip2, None) failed: {other:?}"),
        };
        let recip2_id = recip2.id.expect("recip2 id");

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build overdue message fixture runtime");

        // Create message 1 → recip1
        match runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs overdue message fixture context");
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
        match runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs overdue message fixture context");
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

        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0; // All messages immediately overdue
        config.ack_escalation_enabled = false;

        let (scanned, overdue) = run_ack_ttl_cycle(&config, &pool).expect("run cycle");
        assert_eq!(scanned, 2);
        assert_eq!(overdue, 2);
    }

    #[test]
    fn escalation_non_exclusive_reservation() {
        let (_tmp, pool, cx, unacked) = seed_unacked_message();

        let mut config = test_config(&_tmp);
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

        let mut config = test_config(&_tmp);
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

    #[test]
    fn escalation_scope_requires_literal_agent_components() {
        for name in ["BlueBear", "OpsEscalation", "legacy_agent-1"] {
            assert!(validate_escalation_agent_name(name).is_ok(), "{name}");
        }
        for name in [
            "",
            ".",
            "..",
            "*",
            "../BlueBear",
            "a/b",
            "a\\b",
            "a?",
            "a[b]",
            "a{b,c}",
            "a\n",
        ] {
            assert!(validate_escalation_agent_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn stateful_escalation_retries_after_a_failed_attempt() {
        let (tmp, pool, cx, unacked) = seed_unacked_message();
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_holder_name = "../invalid-holder".to_string();
        let mut state = AckScanState::default();

        for _ in 0..2 {
            assert_eq!(
                run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
                (1, 1)
            );
        }
        assert!(state.warned.contains(&OverdueAckKey::from(&unacked)));
        let before = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list claims before retry: {other:?}"),
        };
        assert!(
            before.is_empty(),
            "failed escalation must not change holders"
        );

        config.ack_escalation_claim_holder_name.clear();
        // Keep the same warning-dedupe state: the failed row was already seen.
        for _ in 0..2 {
            assert_eq!(
                run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
                (1, 1)
            );
        }
        let after = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list claims after retry: {other:?}"),
        };
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].agent_id, unacked.agent_id);
        assert!(after[0].path_pattern.starts_with("agents/BlueBear/inbox/"));
    }

    #[test]
    fn stateful_cycle_reacquires_a_released_claim_and_stops_after_ack() {
        let (tmp, pool, cx, unacked) = seed_unacked_message();
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_holder_name.clear();
        let mut state = AckScanState::default();
        run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap();

        match block_on(async {
            queries::release_reservations(
                &cx,
                &pool,
                unacked.project_id,
                unacked.agent_id,
                None,
                None,
            )
            .await
        }) {
            Outcome::Ok(rows) => assert_eq!(rows.len(), 1),
            other => panic!("release first claim: {other:?}"),
        }
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (1, 1)
        );
        let claims = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list renewed claims: {other:?}"),
        };
        assert_eq!(claims.len(), 2);
        assert_eq!(
            claims
                .iter()
                .filter(|claim| claim.released_ts.is_none())
                .count(),
            1
        );

        match block_on(async {
            queries::acknowledge_message(&cx, &pool, unacked.agent_id, unacked.message_id).await
        }) {
            Outcome::Ok(_) => {}
            other => panic!("acknowledge: {other:?}"),
        }
        match block_on(async {
            queries::release_reservations(
                &cx,
                &pool,
                unacked.project_id,
                unacked.agent_id,
                None,
                None,
            )
            .await
        }) {
            Outcome::Ok(rows) => assert_eq!(rows.len(), 1),
            other => panic!("release second claim: {other:?}"),
        }
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (0, 0)
        );
        assert!(state.warned.is_empty());
        assert!(state.current.is_empty());
        assert!(state.cursor.is_none());
        let active = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, true).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list post-ACK claims: {other:?}"),
        };
        assert!(
            active.is_empty(),
            "acknowledged messages must not reacquire claims"
        );
    }

    #[test]
    fn escalation_missing_recipient_never_reserves_all_inboxes() {
        let (tmp, pool, cx, mut unacked) = seed_unacked_message();
        let mut config = test_config(&tmp);
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_holder_name = "OpsEscalation".to_string();
        unacked.agent_id = i64::MAX;
        assert!(escalate(&config, &pool, &cx, &unacked, now_micros()).is_err());
        let claims = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list claims: {other:?}"),
        };
        assert!(claims.is_empty());
        assert!(!config.storage_root.exists());
    }

    #[test]
    fn escalation_rejects_a_recipient_from_another_project() {
        let (tmp, pool, cx, mut unacked) = seed_unacked_message();
        let other_root = tmp.path().join("other_project");
        std::fs::create_dir_all(&other_root).unwrap();
        let other = ensure_ephemeral_test_project(&cx, &pool, &other_root.to_string_lossy());
        unacked.project_id = other.id.unwrap();
        let mut config = test_config(&tmp);
        config.ack_escalation_mode = "file_reservation".to_string();
        config.ack_escalation_claim_holder_name = "OpsEscalation".to_string();
        assert!(escalate(&config, &pool, &cx, &unacked, now_micros()).is_err());
        let claims = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list cross-project claims: {other:?}"),
        };
        assert!(claims.is_empty());
        assert!(!config.storage_root.exists());
    }

    #[test]
    fn escalation_invalid_custom_holder_does_not_fall_back_to_recipient() {
        let (tmp, pool, cx, unacked) = seed_unacked_message();
        let mut config = test_config(&tmp);
        config.ack_escalation_mode = "file_reservation".to_string();
        for holder in ["*", "../outside", "Ops[AB]", "Ops/Other"] {
            config.ack_escalation_claim_holder_name = holder.to_string();
            assert!(escalate(&config, &pool, &cx, &unacked, now_micros()).is_err());
        }
        let claims = match block_on(async {
            queries::list_file_reservations(&cx, &pool, unacked.project_id, false).await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("list fallback claims: {other:?}"),
        };
        assert!(claims.is_empty());
        assert!(!config.storage_root.exists());
    }

    fn seed_more_overdue(
        cx: &Cx,
        pool: &DbPool,
        original: &queries::UnackedMessageRow,
        offsets: std::ops::RangeInclusive<i64>,
    ) {
        let conn = match block_on(pool.acquire(cx)) {
            Outcome::Ok(conn) => conn,
            other => panic!("acquire seed connection: {other:?}"),
        };
        conn.execute_sync("BEGIN IMMEDIATE", &[]).unwrap();
        for offset in offsets {
            let id = original.message_id.checked_add(offset).unwrap();
            conn.execute_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) SELECT ?, project_id, sender_id, 'bounded scan', 'Body', 'normal', 1, ?, '[]', '{}' FROM messages WHERE id = ?",
                &[id.into(), original.created_ts.into(), original.message_id.into()],
            )
            .unwrap();
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, 'to')",
                &[id.into(), original.agent_id.into()],
            )
            .unwrap();
        }
        conn.execute_sync("COMMIT", &[]).unwrap();
    }

    fn set_test_generation(cx: &Cx, pool: &DbPool, generation: &str) {
        let conn = match block_on(pool.acquire(cx)) {
            Outcome::Ok(conn) => conn,
            other => panic!("acquire generation fixture: {other:?}"),
        };
        assert_eq!(
            conn.execute_sync(
                "UPDATE db_identity SET generation_id = ? WHERE singleton = 0",
                &[mcp_agent_mail_db::sqlmodel_core::Value::Text(generation.to_string())],
            )
            .unwrap(),
            1
        );
    }

    fn seeded_generation(cx: &Cx, pool: &DbPool) -> String {
        match block_on(queries::db_generation_id(cx, pool)) {
            Outcome::Ok(Some(generation)) => generation,
            other => panic!("seed database generation: {other:?}"),
        }
    }

    #[test]
    fn warning_history_is_bounded_and_completed_laps_remove_stale_keys() {
        let mut state = AckScanState::default();
        state.bind_scope("db", Some("generation"));
        for index in 0..MAX_ACK_WARNING_KEYS * 3 {
            let key = OverdueAckKey {
                message_id: i64::try_from(index).unwrap() + 1,
                agent_id: 1,
                created_ts: 10,
            };
            assert!(state.should_warn(key));
        }
        assert_eq!(state.current.len(), MAX_ACK_WARNING_KEYS);
        state.finish_page(None);
        assert_eq!(state.warned.len(), MAX_ACK_WARNING_KEYS);
        assert!(state.current.is_empty());
        let retained = OverdueAckKey { message_id: 1, agent_id: 1, created_ts: 10 };
        assert!(!state.should_warn(retained));
        state.finish_page(None);
        assert_eq!(state.warned, HashSet::from([retained]));
        state.finish_page(None);
        assert!(state.warned.is_empty());
    }

    #[test]
    fn warning_history_never_crosses_mailbox_generation_or_reused_creation_time() {
        let key = OverdueAckKey { message_id: 1, agent_id: 1, created_ts: 10 };
        let mut state = AckScanState::default();
        for (scope, generation) in [("one", "a"), ("two", "a"), ("two", "b")] {
            state.bind_scope(scope, Some(generation));
            assert!(state.should_warn(key));
            assert!(!state.should_warn(key));
            state.finish_page(None);
            state.bind_scope(scope, Some(generation));
            assert!(!state.should_warn(key));
        }
        assert!(state.should_warn(OverdueAckKey { created_ts: 11, ..key }));
    }

    #[test]
    fn failed_pages_do_not_get_the_fast_continuation_cadence() {
        let interval = Duration::from_secs(60);
        assert_eq!(next_ack_scan_delay(interval, true, false), ACK_SCAN_CONTINUATION_PAUSE);
        for (pending, failed) in [(false, false), (false, true), (true, true)] {
            assert_eq!(next_ack_scan_delay(interval, pending, failed), interval);
        }
    }

    #[test]
    fn actual_worker_pages_and_partial_slices_visit_the_whole_backlog() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let additional = i64::try_from(MAX_ACK_SCAN_PAGE_SIZE * 2 + 2).unwrap();
        seed_more_overdue(&cx, &pool, &original, 1..=additional);
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;
        let mut state = AckScanState::default();
        assert_eq!(
            run_ack_ttl_slice(&config, &pool, &mut state, || false, || false).unwrap(),
            (MAX_ACK_SCAN_PAGE_SIZE, 1)
        );
        assert!(state.cursor.is_some());
        assert_eq!(state.current.len(), 1);
        let mut processed = 1;
        let mut slices = 1;
        while state.cursor.is_some() {
            let (scanned, consumed) =
                run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap();
            assert!(scanned <= MAX_ACK_SCAN_PAGE_SIZE);
            assert!(consumed > 0);
            processed += consumed;
            slices += 1;
            assert!(slices <= 4, "continuation must not restart from the first page");
        }
        assert_eq!(processed, usize::try_from(additional).unwrap() + 1);
        assert_eq!(state.warned.len(), processed);
        for offset in 0..=additional {
            assert!(state.warned.contains(&OverdueAckKey {
                message_id: original.message_id + offset,
                agent_id: original.agent_id,
                created_ts: original.created_ts,
            }));
        }
    }

    #[test]
    fn failed_observation_preserves_cursor_and_retries_the_unconsumed_tail() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let generation = seeded_generation(&cx, &pool);
        seed_more_overdue(&cx, &pool, &original, 1..=2);
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;
        let mut state = AckScanState::default();
        run_ack_ttl_slice(&config, &pool, &mut state, || false, || false).unwrap();
        let cursor = state.cursor.clone();
        let current = state.current.clone();
        set_test_generation(&cx, &pool, "");
        let error = run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap_err();
        assert!(error.contains("ACK_SCAN"), "{error}");
        assert_eq!(state.cursor, cursor);
        assert_eq!(state.current, current);
        set_test_generation(&cx, &pool, &generation);
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (2, 2)
        );
        assert!(state.cursor.is_none());
        assert_eq!(state.warned.len(), 3);
    }

    #[test]
    fn actual_generation_change_restarts_the_worker_lap() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let generation = seeded_generation(&cx, &pool);
        seed_more_overdue(&cx, &pool, &original, 1..=2);
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;
        let mut state = AckScanState::default();
        run_ack_ttl_slice(&config, &pool, &mut state, || false, || false).unwrap();
        set_test_generation(&cx, &pool, &format!("{generation}-replacement"));
        assert_eq!(
            run_ack_ttl_slice(&config, &pool, &mut state, || false, || false).unwrap(),
            (3, 1)
        );
        assert_eq!(state.current, HashSet::from([OverdueAckKey::from(&original)]));
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (2, 2)
        );
        assert!(state.cursor.is_none());
    }

    #[test]
    fn shutdown_between_rows_does_not_consume_a_fetched_tail() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        seed_more_overdue(&cx, &pool, &original, 1..=2);
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;
        let mut state = AckScanState::default();
        assert_eq!(
            run_ack_ttl_slice(&config, &pool, &mut state, || true, || true).unwrap(),
            (0, 0)
        );
        assert!(state.identity.is_none());
        let mut checks = 0;
        let result = run_ack_ttl_slice(
            &config,
            &pool,
            &mut state,
            || { checks += 1; checks >= 4 },
            || true,
        )
        .unwrap();
        assert_eq!(result, (3, 1));
        assert_eq!(checks, 4);
        assert_eq!(state.current.len(), 1);
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (2, 2)
        );
        assert_eq!(state.warned.len(), 3);
    }

    #[test]
    fn worker_observes_new_acks_without_chasing_new_mail_in_the_same_lap() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        seed_more_overdue(&cx, &pool, &original, 1..=2);
        let mut config = test_config(&tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = false;
        let mut state = AckScanState::default();
        run_ack_ttl_slice(&config, &pool, &mut state, || false, || false).unwrap();
        match block_on(queries::acknowledge_message(
            &cx, &pool, original.agent_id, original.message_id + 1,
        )) {
            Outcome::Ok(_) => {}
            other => panic!("ack between slices: {other:?}"),
        }
        seed_more_overdue(&cx, &pool, &original, 3..=3);
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (1, 1)
        );
        assert!(state.cursor.is_none());
        assert_eq!(state.warned.len(), 2);
        assert!(!state.warned.contains(&OverdueAckKey {
            message_id: original.message_id + 3,
            agent_id: original.agent_id,
            created_ts: original.created_ts,
        }));
        assert_eq!(
            run_ack_ttl_cycle_with_state(&config, &pool, &mut state).unwrap(),
            (3, 3)
        );
        assert_eq!(state.warned.len(), 3);
    }

    fn escalation_config(tmp: &tempfile::TempDir) -> Config {
        let mut config = test_config(tmp);
        config.ack_ttl_seconds = 0;
        config.ack_escalation_enabled = true;
        config.ack_escalation_mode = "file_reservation".into();
        config.ack_escalation_claim_holder_name.clear();
        config
    }

    fn claim_rows(cx: &Cx, pool: &DbPool, project_id: i64) -> Vec<mcp_agent_mail_db::FileReservationRow> {
        match block_on(queries::list_file_reservations(cx, pool, project_id, false)) {
            Outcome::Ok(rows) => rows,
            other => panic!("read observed grants: {other:?}"),
        }
    }

    #[test]
    fn acknowledgment_between_page_read_and_execution_creates_no_grant_or_artifact() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        seeded_generation(&cx, &pool);
        let config = escalation_config(&tmp);
        let mut state = AckScanState::default();
        let mut checks = 0;
        let result = run_ack_ttl_slice(
            &config,
            &pool,
            &mut state,
            || {
                checks += 1;
                // Entry and post-barrier checks precede paging. The third is
                // immediately before processing the already-observed first row.
                if checks == 3 {
                    match block_on(queries::acknowledge_message(
                        &cx, &pool, original.agent_id, original.message_id,
                    )) {
                        Outcome::Ok(_) => {}
                        other => panic!("ACK after page read: {other:?}"),
                    }
                }
                false
            },
            || true,
        ).unwrap();
        assert_eq!(checks, 3, "the interleaving must actually execute");
        assert_eq!(result, (1, 1));
        assert!(claim_rows(&cx, &pool, original.project_id).is_empty());
        assert!(!config.storage_root.exists());
        assert_eq!(run_ack_ttl_cycle(&config, &pool).unwrap(), (0, 0));
    }

    #[test]
    fn changed_generation_refuses_old_proposal_but_a_fresh_one_can_progress() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let old_generation = seeded_generation(&cx, &pool);
        let config = escalation_config(&tmp);
        let new_generation = format!("{old_generation}ab");
        set_test_generation(&cx, &pool, &new_generation);
        assert!(!escalate_observed(&config, &pool, &cx, &original, Some(&old_generation), original.created_ts).unwrap());
        assert!(claim_rows(&cx, &pool, original.project_id).is_empty());
        assert!(!config.storage_root.exists());
        // This is a new observation, not silently rebinding the old proposal.
        assert_eq!(run_ack_ttl_cycle(&config, &pool).unwrap(), (1, 1));
        assert_eq!(claim_rows(&cx, &pool, original.project_id).len(), 1);
    }

    #[test]
    fn committed_grant_artifact_is_stamped_and_duplicate_does_not_renew_it() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let generation = seeded_generation(&cx, &pool);
        let config = escalation_config(&tmp);
        assert_eq!(run_ack_ttl_cycle(&config, &pool).unwrap(), (1, 1));
        let before = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(before.len(), 1);
        let project = match block_on(queries::get_project_by_id(&cx, &pool, original.project_id)) {
            Outcome::Ok(project) => project,
            other => panic!("project for actual artifact: {other:?}"),
        };
        let filename = mcp_agent_mail_core::reservation_artifact::reservation_artifact_filename(
            Some(&generation), before[0].id.unwrap(),
        );
        let path = config.storage_root.join("projects").join(project.slug)
            .join("file_reservations").join(filename);
        let bytes = std::fs::read(&path).expect("confirmed grant must be published by the direct archive path");
        let artifact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(artifact["db_generation"], generation);
        assert_eq!(artifact["id"], before[0].id.unwrap());
        assert_eq!(artifact["path_pattern"], before[0].path_pattern);
        assert_eq!(run_ack_ttl_cycle(&config, &pool).unwrap(), (1, 1));
        let after = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, before[0].id);
        assert_eq!(after[0].expires_ts, before[0].expires_ts);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn acknowledgment_after_committed_grant_does_not_revoke_or_renew_the_lease() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let generation = seeded_generation(&cx, &pool);
        let config = escalation_config(&tmp);
        assert!(escalate_observed(&config, &pool, &cx, &original, Some(&generation), original.created_ts).unwrap());
        let before = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(before.len(), 1);
        match block_on(queries::acknowledge_message(
            &cx, &pool, original.agent_id, original.message_id,
        )) {
            Outcome::Ok(_) => {}
            other => panic!("ACK after committed grant: {other:?}"),
        }
        assert!(!escalate_observed(&config, &pool, &cx, &original, Some(&generation), original.created_ts).unwrap());
        let after = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, before[0].id);
        assert_eq!(after[0].expires_ts, before[0].expires_ts);
        assert_eq!(after[0].released_ts, before[0].released_ts);
    }

    #[test]
    fn acknowledged_page_head_does_not_suppress_a_pending_sibling() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        seed_more_overdue(&cx, &pool, &original, 1..=2);
        let config = escalation_config(&tmp);
        let mut state = AckScanState::default();
        let mut checks = 0;
        let result = run_ack_ttl_slice(
            &config, &pool, &mut state,
            || {
                checks += 1;
                if checks == 3 {
                    match block_on(queries::acknowledge_message(
                        &cx, &pool, original.agent_id, original.message_id,
                    )) {
                        Outcome::Ok(_) => {}
                        other => panic!("ACK after page read: {other:?}"),
                    }
                }
                false
            },
            || true,
        ).unwrap();
        assert_eq!(checks, 5);
        assert_eq!(result, (3, 3));
        let claims = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(claims.len(), 1, "a pending sibling must get a grant in the same slice");
        assert_eq!(claims[0].agent_id, original.agent_id);
        assert_eq!(claims[0].reason, "ack-overdue");
        assert!(state.cursor.is_none());
        assert_eq!(run_ack_ttl_cycle(&config, &pool).unwrap(), (2, 2));
        let repeated = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(repeated.len(), 1);
        assert_eq!(repeated[0].expires_ts, claims[0].expires_ts);
    }

    #[test]
    fn all_stale_siblings_finish_without_grants_or_artifacts() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        seed_more_overdue(&cx, &pool, &original, 1..=2);
        let config = escalation_config(&tmp);
        let mut state = AckScanState::default();
        let mut checks = 0;
        let result = run_ack_ttl_slice(
            &config, &pool, &mut state,
            || {
                checks += 1;
                if checks == 3 {
                    for offset in 0..=2 {
                        match block_on(queries::acknowledge_message(
                            &cx, &pool, original.agent_id, original.message_id + offset,
                        )) {
                            Outcome::Ok(_) => {}
                            other => panic!("ACK stale sibling: {other:?}"),
                        }
                    }
                }
                false
            },
            || true,
        ).unwrap();
        assert_eq!(checks, 5);
        assert_eq!(result, (3, 3));
        assert!(state.cursor.is_none());
        assert!(claim_rows(&cx, &pool, original.project_id).is_empty());
        assert!(!config.storage_root.exists());
    }

    #[test]
    fn worker_carries_original_cutoff_to_transactional_admission() {
        let (tmp, pool, cx, original) = seed_unacked_message();
        let generation = seeded_generation(&cx, &pool);
        let config = escalation_config(&tmp);
        assert!(!escalate_observed(
            &config, &pool, &cx, &original, Some(&generation), original.created_ts - 1,
        ).unwrap());
        assert!(claim_rows(&cx, &pool, original.project_id).is_empty());
        assert!(!config.storage_root.exists());
        assert!(escalate_observed(
            &config, &pool, &cx, &original, Some(&generation), original.created_ts,
        ).unwrap());
        let claims = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(claims.len(), 1);
        assert!(escalate_observed(
            &config, &pool, &cx, &original, Some(&generation), original.created_ts,
        ).unwrap(), "coverage, unlike staleness, settles the coalescing group");
        let covered = claim_rows(&cx, &pool, original.project_id);
        assert_eq!(covered.len(), 1);
        assert_eq!(covered[0].expires_ts, claims[0].expires_ts);
    }
}
