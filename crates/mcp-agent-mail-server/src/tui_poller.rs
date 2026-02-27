//! Periodic DB poller that feeds [`TuiSharedState`] with fresh statistics.
//!
//! The poller runs on a dedicated background thread using sync `SQLite`
//! connections (not the async pool).  It wakes every `interval`, queries
//! aggregate counts + agent list, computes deltas against the previous
//! snapshot, refreshes shared stats every cycle, and emits health pulses
//! on data changes plus periodic heartbeat intervals.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::queries::ACTIVE_RESERVATION_PREDICATE;
use mcp_agent_mail_db::sqlmodel_core::{Error as SqlError, Row, Value};
use mcp_agent_mail_db::timestamps::now_micros;
use mcp_agent_mail_db::{
    ensure_sqlite_file_healthy, ensure_sqlite_file_healthy_with_archive,
    is_sqlite_recovery_error_message, open_sqlite_file_with_recovery,
};

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, MailEvent, ProjectSummary, ReservationSnapshot,
};

/// Default polling interval (2 seconds).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Prevent accidental zero/near-zero env values from creating a busy-loop.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum agents to fetch per poll cycle.
const MAX_AGENTS: usize = 50;

/// Maximum projects to fetch per poll cycle.
const MAX_PROJECTS: usize = 100;

/// Maximum contact links to fetch per poll cycle.
const MAX_CONTACTS: usize = 200;

/// Maximum reservation rows to fetch per poll cycle.
const MAX_RESERVATIONS: usize = 1000;
/// Maximum silent interval before a heartbeat `HealthPulse` is emitted.
const HEALTH_PULSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Minimum interval between poller-triggered sqlite recovery attempts per path.
const POLLER_RECOVERY_MIN_INTERVAL: Duration = Duration::from_secs(15);
/// Re-evaluate legacy reservation scan mode periodically (per DB path).
#[allow(clippy::duration_suboptimal_units)]
const RESERVATION_SCAN_MODE_CACHE_TTL: Duration = Duration::from_secs(300);
static POLLER_RECOVERY_GATES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
static RESERVATION_SCAN_MODE_CACHE: OnceLock<Mutex<HashMap<String, ReservationScanCacheEntry>>> =
    OnceLock::new();

/// Batched aggregate counters used to populate [`DbStatSnapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DbSnapshotCounts {
    projects: u64,
    agents: u64,
    messages: u64,
    file_reservations: u64,
    contact_links: u64,
    ack_pending: u64,
}

#[derive(Debug, Default)]
struct ReservationSnapshotBundle {
    active_count: u64,
    active_counts_by_project: HashMap<i64, u64>,
    snapshots: Vec<ReservationSnapshot>,
}

#[derive(Debug, Clone)]
struct SnapshotHeapEntry {
    sort_key: (i64, i64),
    snapshot: ReservationSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationScanMode {
    /// Legacy mode: decode/filter all rows in Rust to preserve TEXT timestamp
    /// compatibility from very old schemas.
    FullLegacy,
    /// Fast path: rely on SQL predicates for active reservations.
    ActiveFast,
}

#[derive(Debug, Clone, Copy)]
struct ReservationScanCacheEntry {
    mode: ReservationScanMode,
    checked_at: Instant,
}

impl PartialEq for SnapshotHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.sort_key == other.sort_key
    }
}

impl Eq for SnapshotHeapEntry {}

impl PartialOrd for SnapshotHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for SnapshotHeapEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.sort_key.cmp(&other.sort_key)
    }
}

/// Groups DB queries used by the TUI poller so related reads can be fetched
/// with fewer round-trips.
struct DbStatQueryBatcher<'a> {
    conn: &'a DbConn,
    sqlite_path: Option<&'a str>,
}

impl<'a> DbStatQueryBatcher<'a> {
    #[allow(dead_code)]
    const fn new(conn: &'a DbConn) -> Self {
        Self {
            conn,
            sqlite_path: None,
        }
    }

    const fn new_with_path(conn: &'a DbConn, sqlite_path: &'a str) -> Self {
        Self {
            conn,
            sqlite_path: Some(sqlite_path),
        }
    }

    fn handle_query_error(&self, error: &SqlError) {
        let message = error.to_string();
        if !is_sqlite_recovery_error_message(&message) {
            return;
        }
        if let Some(path) = self.sqlite_path {
            maybe_attempt_sqlite_recovery(path, &message);
        }
    }

    fn fetch_snapshot(&self) -> DbStatSnapshot {
        let now = now_micros();
        let reservation_bundle =
            fetch_reservation_snapshot_bundle(self.conn, now, self.sqlite_path);
        let counts = self.fetch_counts_with_reservation_count(reservation_bundle.active_count);
        DbStatSnapshot {
            projects: counts.projects,
            agents: counts.agents,
            messages: counts.messages,
            file_reservations: counts.file_reservations,
            contact_links: counts.contact_links,
            ack_pending: counts.ack_pending,
            agents_list: fetch_agents_list(self.conn),
            projects_list: fetch_projects_list_with_reservation_counts(
                self.conn,
                Some(&reservation_bundle.active_counts_by_project),
            ),
            contacts_list: fetch_contacts_list(self.conn),
            reservation_snapshots: reservation_bundle.snapshots,
            timestamp_micros: now,
        }
    }

    #[cfg(test)]
    fn fetch_counts(&self) -> DbSnapshotCounts {
        let now = now_micros();
        let reservation_count = self.count_active_reservations(now);
        self.fetch_counts_with_reservation_count(reservation_count)
    }

    fn fetch_counts_with_reservation_count(&self, reservation_count: u64) -> DbSnapshotCounts {
        let reservation_count_sql = "SELECT \
             (SELECT COUNT(*) FROM projects) AS projects_count, \
             (SELECT COUNT(*) FROM agents) AS agents_count, \
             (SELECT COUNT(*) FROM messages) AS messages_count, \
             (SELECT COUNT(*) FROM agent_links) AS contacts_count, \
             (SELECT COUNT(*) FROM message_recipients \
               WHERE ack_ts IS NULL \
                 AND message_id IN (SELECT id FROM messages WHERE ack_required = 1)) AS ack_pending_count";
        let batched_rows = match self.conn.query_sync(reservation_count_sql, &[]) {
            Ok(rows) => Some(rows),
            Err(err) => {
                self.handle_query_error(&err);
                None
            }
        };

        let batched = batched_rows
            .and_then(|rows| rows.into_iter().next())
            .map(|row| {
                let read_count = |key: &str| {
                    row.get_named::<i64>(key)
                        .ok()
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(0)
                };
                DbSnapshotCounts {
                    projects: read_count("projects_count"),
                    agents: read_count("agents_count"),
                    messages: read_count("messages_count"),
                    file_reservations: reservation_count,
                    contact_links: read_count("contacts_count"),
                    ack_pending: read_count("ack_pending_count"),
                }
            });

        if let Some(counts) = batched {
            return counts;
        }

        self.fetch_counts_fallback_with_reservation_count(reservation_count)
    }

    fn fetch_counts_fallback_with_reservation_count(
        &self,
        reservation_count: u64,
    ) -> DbSnapshotCounts {
        DbSnapshotCounts {
            projects: self
                .run_count_query("SELECT COUNT(*) AS c FROM projects", &[])
                .unwrap_or(0),
            agents: self
                .run_count_query("SELECT COUNT(*) AS c FROM agents", &[])
                .unwrap_or(0),
            messages: self
                .run_count_query("SELECT COUNT(*) AS c FROM messages", &[])
                .unwrap_or(0),
            file_reservations: reservation_count,
            contact_links: self
                .run_count_query("SELECT COUNT(*) AS c FROM agent_links", &[])
                .unwrap_or(0),
            ack_pending: self
                .run_count_query(
                    "SELECT COUNT(*) AS c FROM message_recipients \
                     WHERE ack_ts IS NULL \
                       AND message_id IN (SELECT id FROM messages WHERE ack_required = 1)",
                    &[],
                )
                .unwrap_or(0),
        }
    }

    fn run_count_query(&self, sql: &str, params: &[Value]) -> Option<u64> {
        match self.conn.query_sync(sql, params) {
            Ok(rows) => rows
                .into_iter()
                .next()
                .and_then(|row| row.get_named::<i64>("c").ok())
                .and_then(|v| u64::try_from(v).ok()),
            Err(err) => {
                self.handle_query_error(&err);
                None
            }
        }
    }

    #[cfg(test)]
    fn count_active_reservations(&self, now: i64) -> u64 {
        // Keep count semantics in lock-step with `is_active_reservation_row`.
        // Legacy databases may store active sentinels in `released_ts` as text
        // (`"0"`, `"0.0"`, `"null"`, etc.), which SQL-only `IS NULL` checks miss.
        // The Rust row scanner is authoritative and already used for snapshots.
        self.count_active_reservations_fallback_scan(now)
    }

    #[cfg(test)]
    fn count_active_reservations_fallback_scan(&self, now: i64) -> u64 {
        let rows = match self.conn.query_sync(
            "SELECT expires_ts AS raw_expires_ts, released_ts AS raw_released_ts FROM file_reservations",
            &[],
        ) {
            Ok(rows) => rows,
            Err(err) => {
                self.handle_query_error(&err);
                return 0;
            }
        };
        #[cfg(test)]
        if let Some(first) = rows.first() {
            debug_row_shape("count_active_reservations_fallback_scan", first);
        }
        u64::try_from(
            rows.into_iter()
                .filter(|row| {
                    is_active_reservation_row(row, now, "raw_expires_ts", "raw_released_ts")
                })
                .count(),
        )
        .unwrap_or(u64::MAX)
    }
}

// ──────────────────────────────────────────────────────────────────────
// DbPoller
// ──────────────────────────────────────────────────────────────────────

/// Periodically queries the `SQLite` database and pushes [`DbStatSnapshot`]
/// into [`TuiSharedState`].  Emits `MailEvent::HealthPulse` on each
/// change so the event stream stays up to date.
pub struct DbPoller {
    state: Arc<TuiSharedState>,
    database_url: String,
    interval: Duration,
    stop: Arc<AtomicBool>,
}

/// Handle returned by [`DbPoller::start`].
pub struct DbPollerHandle {
    join: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl DbPoller {
    /// Create a new poller.  Call [`Self::start`] to spawn the background
    /// thread.
    #[must_use]
    pub fn new(state: Arc<TuiSharedState>, database_url: String) -> Self {
        Self {
            state,
            database_url,
            interval: poll_interval_from_env(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Override the polling interval (for tests).
    #[must_use]
    pub const fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Spawn the background polling thread.
    #[must_use]
    pub fn start(self) -> DbPollerHandle {
        let stop = Arc::clone(&self.stop);
        let join = thread::Builder::new()
            .name("tui-db-poller".into())
            .spawn(move || {
                self.run();
            })
            .expect("spawn tui-db-poller thread");
        DbPollerHandle {
            join: Some(join),
            stop,
        }
    }

    /// Main polling loop.
    fn run(self) {
        let mut prev = DbStatSnapshot::default();
        let now = Instant::now();
        let mut last_health_emit = now
            .checked_sub(HEALTH_PULSE_HEARTBEAT_INTERVAL)
            .unwrap_or(now);
        let mut panic_recovery_active = false;

        while !self.stop.load(Ordering::Relaxed) {
            // Fetch fresh snapshot
            let snapshot = if let Ok(snapshot) =
                catch_optional_panic(|| fetch_db_stats(&self.database_url))
            {
                if panic_recovery_active {
                    tracing::info!(
                        "tui-db-poller recovered after a panic; resuming normal polling"
                    );
                    panic_recovery_active = false;
                }
                snapshot
            } else {
                if !panic_recovery_active {
                    tracing::warn!(
                        "tui-db-poller recovered from a panic while polling DB; keeping UI alive"
                    );
                    panic_recovery_active = true;
                }
                None
            };
            if let Some(snapshot) = snapshot {
                let changed = snapshot_delta(&prev, &snapshot).any_changed();
                // Always refresh shared DB stats so timestamp/list snapshots
                // stay current even when aggregate counters are steady.
                self.state.update_db_stats(snapshot.clone());
                if changed || last_health_emit.elapsed() >= HEALTH_PULSE_HEARTBEAT_INTERVAL {
                    let _ = self
                        .state
                        .push_event(MailEvent::health_pulse(snapshot.clone()));
                    last_health_emit = Instant::now();
                }
                prev = snapshot;
            }

            // Sleep in small increments so we notice shutdown quickly
            let mut remaining = self.interval;
            let tick = Duration::from_millis(100);
            while remaining > Duration::ZERO && !self.stop.load(Ordering::Relaxed) {
                let sleep = remaining.min(tick);
                thread::sleep(sleep);
                remaining = remaining.saturating_sub(sleep);
            }
        }
    }
}

impl DbPollerHandle {
    /// Signal the poller to stop and wait for the thread to exit.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// Signal stop without waiting.
    pub fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Wait for the thread to exit (call after `signal_stop`).
    pub fn join(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for DbPollerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ──────────────────────────────────────────────────────────────────────
// DB query helpers
// ──────────────────────────────────────────────────────────────────────

/// Run a closure that returns `Option<T>`, converting unwind panics into `Err`.
///
/// The TUI poller uses this to keep the UI responsive when underlying storage
/// layers panic unexpectedly (for example, during transient driver failures).
fn catch_optional_panic<T, F>(fetcher: F) -> std::thread::Result<Option<T>>
where
    F: FnOnce() -> Option<T> + std::panic::UnwindSafe,
{
    std::panic::catch_unwind(fetcher)
}

fn maybe_attempt_sqlite_recovery(sqlite_path: &str, reason: &str) {
    if sqlite_path == ":memory:" {
        return;
    }

    let gates = POLLER_RECOVERY_GATES.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    {
        let mut guard = match gates.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(last_attempt) = guard.get(sqlite_path)
            && now.duration_since(*last_attempt) < POLLER_RECOVERY_MIN_INTERVAL
        {
            return;
        }
        guard.insert(sqlite_path.to_string(), now);
    }

    let sqlite_path_buf = Path::new(sqlite_path).to_path_buf();
    let storage_root = mcp_agent_mail_core::config::env_value("STORAGE_ROOT");
    #[allow(clippy::option_if_let_else)]
    let recovery_result = if let Some(root) = storage_root {
        let root_path = Path::new(&root);
        if root_path.is_dir() {
            ensure_sqlite_file_healthy_with_archive(&sqlite_path_buf, root_path)
        } else {
            ensure_sqlite_file_healthy(&sqlite_path_buf)
        }
    } else {
        ensure_sqlite_file_healthy(&sqlite_path_buf)
    };

    match recovery_result {
        Ok(()) => tracing::warn!(
            path = %sqlite_path,
            reason = %reason,
            "tui poller auto-recovered sqlite file after recoverable query error"
        ),
        Err(err) => tracing::warn!(
            path = %sqlite_path,
            reason = %reason,
            error = %err,
            "tui poller attempted sqlite recovery but it failed"
        ),
    }
}

/// Fetch a complete [`DbStatSnapshot`] from the database.
///
/// Opens a fresh sync connection, runs aggregate queries, and returns
/// the snapshot.  On any error, returns `None` so callers can keep the
/// previous snapshot instead of clearing existing data.
fn fetch_db_stats(database_url: &str) -> Option<DbStatSnapshot> {
    let (conn, sqlite_path) = open_sync_connection_with_path(database_url)?;
    Some(DbStatQueryBatcher::new_with_path(&conn, &sqlite_path).fetch_snapshot())
}

/// Open a sync `SQLite` connection from a database URL (public for compose dispatch).
#[must_use]
pub fn open_sync_connection_pub(database_url: &str) -> Option<DbConn> {
    open_sync_connection(database_url)
}

/// Open a sync `SQLite` connection from a database URL.
fn open_sync_connection(database_url: &str) -> Option<DbConn> {
    open_sync_connection_with_path(database_url).map(|(conn, _)| conn)
}

fn open_sync_connection_with_path(database_url: &str) -> Option<(DbConn, String)> {
    // `:memory:` URLs would create a brand-new private DB per poll cycle,
    // which diverges from the server pool and yields misleading empty
    // snapshots. Skip polling in that mode instead of reporting false zeros.
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return None;
    }
    let cfg = DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let mut path = cfg.sqlite_path().ok()?;
    let parsed = Path::new(&path);
    if !parsed.is_absolute() && !path.starts_with("./") && !path.starts_with("../") {
        let absolute_candidate = Path::new("/").join(parsed);
        if !parsed.exists() && absolute_candidate.exists() {
            tracing::warn!(
                relative_path = %parsed.display(),
                absolute_candidate = %absolute_candidate.display(),
                "detected malformed sqlite URL path; using absolute fallback"
            );
            path = absolute_candidate.to_string_lossy().into_owned();
        }
    }
    match open_sqlite_file_with_recovery(&path) {
        Ok(conn) => Some((conn, path)),
        Err(err) => {
            let err_msg = err.to_string();
            if is_sqlite_recovery_error_message(&err_msg) {
                maybe_attempt_sqlite_recovery(&path, &err_msg);
                open_sqlite_file_with_recovery(&path)
                    .ok()
                    .map(|conn| (conn, path))
            } else {
                None
            }
        }
    }
}

/// Fetch the agent list ordered by most recently active.
fn fetch_agents_list(conn: &DbConn) -> Vec<AgentSummary> {
    conn.query_sync(
        &format!(
            "SELECT name, program, last_active_ts FROM agents \
             ORDER BY last_active_ts DESC, id DESC LIMIT {MAX_AGENTS}"
        ),
        &[],
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                Some(AgentSummary {
                    name: row.get_named::<String>("name").ok()?,
                    program: row.get_named::<String>("program").ok()?,
                    last_active_ts: row.get_named::<i64>("last_active_ts").ok()?,
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Fetch the project list with per-project agent/message/reservation counts.
#[cfg(test)]
fn fetch_projects_list(conn: &DbConn) -> Vec<ProjectSummary> {
    fetch_projects_list_with_reservation_counts(conn, None)
}

fn fetch_projects_list_with_reservation_counts(
    conn: &DbConn,
    reservation_counts_override: Option<&HashMap<i64, u64>>,
) -> Vec<ProjectSummary> {
    let sql = format!(
        "WITH recent_projects AS ( \
           SELECT id, slug, human_key, created_at \
           FROM projects \
           ORDER BY created_at DESC, id DESC \
           LIMIT {MAX_PROJECTS} \
         ), \
         agent_counts AS ( \
           SELECT project_id, COUNT(*) AS cnt \
           FROM agents \
           WHERE project_id IN (SELECT id FROM recent_projects) \
           GROUP BY project_id \
         ), \
         message_counts AS ( \
           SELECT project_id, COUNT(*) AS cnt \
           FROM messages \
           WHERE project_id IN (SELECT id FROM recent_projects) \
           GROUP BY project_id \
         ) \
         SELECT p.id, p.slug, p.human_key, p.created_at, \
                COALESCE(ac.cnt, 0) AS agent_count, \
                COALESCE(mc.cnt, 0) AS message_count \
         FROM recent_projects p \
         LEFT JOIN agent_counts ac ON ac.project_id = p.id \
         LEFT JOIN message_counts mc ON mc.project_id = p.id \
         ORDER BY p.created_at DESC, p.id DESC"
    );
    let fallback_reservation_counts = reservation_counts_override
        .is_none()
        .then(|| fetch_active_reservation_counts_by_project(conn, now_micros()));
    let reservation_counts = reservation_counts_override.unwrap_or_else(|| {
        fallback_reservation_counts
            .as_ref()
            .unwrap_or_else(|| unreachable!("fallback reservation counts should exist"))
    });
    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let project_id = row.get_named::<i64>("id").ok()?;
                    Some(ProjectSummary {
                        id: project_id,
                        slug: row.get_named::<String>("slug").ok()?,
                        human_key: row.get_named::<String>("human_key").ok()?,
                        agent_count: row
                            .get_named::<i64>("agent_count")
                            .ok()
                            .and_then(|v| u64::try_from(v).ok())
                            .unwrap_or(0),
                        message_count: row
                            .get_named::<i64>("message_count")
                            .ok()
                            .and_then(|v| u64::try_from(v).ok())
                            .unwrap_or(0),
                        reservation_count: reservation_counts
                            .get(&project_id)
                            .copied()
                            .unwrap_or(0),
                        created_at: row.get_named::<i64>("created_at").ok().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn fetch_active_reservation_counts_by_project(conn: &DbConn, now: i64) -> HashMap<i64, u64> {
    let Ok(rows) = conn.query_sync(
        "SELECT project_id, expires_ts AS raw_expires_ts, released_ts AS raw_released_ts FROM file_reservations",
        &[],
    ) else {
        return HashMap::new();
    };
    #[cfg(test)]
    if let Some(first) = rows.first() {
        debug_row_shape("fetch_active_reservation_counts_by_project", first);
    }
    let mut counts = HashMap::new();
    for row in rows {
        if !is_active_reservation_row(&row, now, "raw_expires_ts", "raw_released_ts") {
            continue;
        }
        let Some(project_id) = parse_raw_i64(&row, "project_id") else {
            continue;
        };
        *counts.entry(project_id).or_insert(0) += 1;
    }
    counts
}

/// Fetch the contact links list with agent names resolved.
fn fetch_contacts_list(conn: &DbConn) -> Vec<ContactSummary> {
    conn.query_sync(
        &format!(
            "SELECT \
             a1.name AS from_agent, a2.name AS to_agent, \
             p1.slug AS from_project, p2.slug AS to_project, \
             al.status, al.reason, al.updated_ts, al.expires_ts \
             FROM agent_links al \
             JOIN agents a1 ON a1.id = al.a_agent_id \
             JOIN agents a2 ON a2.id = al.b_agent_id \
             JOIN projects p1 ON p1.id = al.a_project_id \
             JOIN projects p2 ON p2.id = al.b_project_id \
             ORDER BY al.updated_ts DESC, al.id DESC \
             LIMIT {MAX_CONTACTS}"
        ),
        &[],
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                Some(ContactSummary {
                    from_agent: row.get_named::<String>("from_agent").ok()?,
                    to_agent: row.get_named::<String>("to_agent").ok()?,
                    from_project_slug: row.get_named::<String>("from_project").ok()?,
                    to_project_slug: row.get_named::<String>("to_project").ok()?,
                    status: row.get_named::<String>("status").ok()?,
                    reason: row.get_named::<String>("reason").ok().unwrap_or_default(),
                    updated_ts: row.get_named::<i64>("updated_ts").ok().unwrap_or(0),
                    expires_ts: row.get_named::<i64>("expires_ts").ok(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Parse a raw timestamp value (integer or text) into microseconds.
///
/// Handles:
/// - Integer/real → returned as-is (assumed microseconds)
/// - Text containing only digits → parsed as integer microseconds
/// - Text in `YYYY-MM-DD HH:MM:SS.ffffff` format → parsed via chrono-free manual conversion
/// - Anything else → 0
fn parse_raw_ts(row: &Row, col: &str) -> i64 {
    match row.get_by_name(col) {
        Some(Value::Timestamp(v) | Value::TimestampTz(v) | Value::Time(v) | Value::BigInt(v)) => *v,
        Some(Value::Date(v) | Value::Int(v)) => i64::from(*v),
        Some(Value::SmallInt(v)) => i64::from(*v),
        Some(Value::TinyInt(v)) => i64::from(*v),
        Some(Value::Bool(v)) => i64::from(*v),
        Some(Value::Double(v)) => parse_float_ts(*v),
        Some(Value::Float(v)) => parse_float_ts(f64::from(*v)),
        Some(Value::Decimal(s) | Value::Text(s)) => parse_text_timestamp(s),
        _ => 0,
    }
}

fn parse_raw_i64(row: &Row, col: &str) -> Option<i64> {
    match row.get_by_name(col) {
        Some(Value::Timestamp(v) | Value::TimestampTz(v) | Value::Time(v) | Value::BigInt(v)) => {
            Some(*v)
        }
        Some(Value::Date(v) | Value::Int(v)) => Some(i64::from(*v)),
        Some(Value::SmallInt(v)) => Some(i64::from(*v)),
        Some(Value::TinyInt(v)) => Some(i64::from(*v)),
        Some(Value::Bool(v)) => Some(i64::from(*v)),
        Some(Value::Double(v)) => Some(parse_float_ts(*v)),
        Some(Value::Float(v)) => Some(parse_float_ts(f64::from(*v))),
        Some(Value::Decimal(s) | Value::Text(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Convert a floating timestamp into microseconds with saturation.
#[allow(clippy::cast_possible_truncation)]
fn parse_float_ts(value: f64) -> i64 {
    const I64_MAX_F64: f64 = 9_223_372_036_854_775_807.0;
    const I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;

    if !value.is_finite() {
        return 0;
    }
    let truncated = value.trunc();
    if truncated >= I64_MAX_F64 {
        i64::MAX
    } else if truncated <= I64_MIN_F64 {
        i64::MIN
    } else {
        truncated as i64
    }
}

/// Convert a text timestamp to microseconds.
///
/// Recognises pure-numeric strings (microsecond integers stored as text) and
/// `YYYY-MM-DD HH:MM:SS[.ffffff]` datetime strings.
fn parse_text_timestamp(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    // Pure numeric string → microseconds
    if let Ok(v) = s.parse::<i64>() {
        return v;
    }
    // Decimal numeric text is also treated as microseconds.
    if let Ok(v) = s.parse::<f64>() {
        return parse_float_ts(v);
    }
    // Try YYYY-MM-DD HH:MM:SS[.ffffff] format
    // Split on space → date part + time part
    let Some((date_part, time_part)) = s.split_once(' ') else {
        return 0;
    };
    let date_parts: Vec<&str> = date_part.split('-').collect();
    if date_parts.len() != 3 {
        return 0;
    }
    let year: i64 = date_parts[0].parse().unwrap_or(0);
    let month: i64 = date_parts[1].parse().unwrap_or(0);
    let day: i64 = date_parts[2].parse().unwrap_or(0);
    if year == 0 || month == 0 || day == 0 {
        return 0;
    }
    // Parse time part: HH:MM:SS[.ffffff]
    let (time_hms, frac_str) = match time_part.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (time_part, ""),
    };
    let time_parts: Vec<&str> = time_hms.split(':').collect();
    if time_parts.len() != 3 {
        return 0;
    }
    let hour: i64 = time_parts[0].parse().unwrap_or(0);
    let min: i64 = time_parts[1].parse().unwrap_or(0);
    let sec: i64 = time_parts[2].parse().unwrap_or(0);
    // Fractional seconds → microseconds (pad/truncate to 6 digits)
    let frac_micros: i64 = if frac_str.is_empty() {
        0
    } else {
        let safe_frac: String = frac_str.chars().take(6).collect();
        let padded = format!("{safe_frac:0<6}");
        padded.parse().unwrap_or(0)
    };
    // Convert to unix timestamp using a simplified calculation
    // (good enough for display timestamps, not a precise calendar)
    let epoch_days = days_from_civil(year, month, day);
    let unix_seconds = epoch_days * 86400 + hour * 3600 + min * 60 + sec;
    unix_seconds * 1_000_000 + frac_micros
}

fn is_active_reservation_row(row: &Row, now: i64, expires_col: &str, released_col: &str) -> bool {
    parse_raw_ts(row, expires_col) > now && released_ts_is_active(row.get_by_name(released_col))
}

fn released_ts_is_active(raw: Option<&Value>) -> bool {
    match raw {
        None | Some(Value::Null) => true,
        Some(Value::Timestamp(v) | Value::TimestampTz(v) | Value::Time(v) | Value::BigInt(v)) => {
            *v <= 0
        }
        Some(Value::Date(v) | Value::Int(v)) => *v <= 0,
        Some(Value::SmallInt(v)) => *v <= 0,
        Some(Value::TinyInt(v)) => *v <= 0,
        Some(Value::Bool(v)) => !*v,
        Some(Value::Double(v)) => *v <= 0.0,
        Some(Value::Float(v)) => *v <= 0.0,
        Some(Value::Decimal(s) | Value::Text(s)) => released_text_is_active(s),
        _ => false,
    }
}

fn released_text_is_active(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "0" | "null" | "none") {
        return true;
    }
    trimmed.parse::<f64>().is_ok_and(|number| number <= 0.0)
}

#[cfg(test)]
fn debug_row_shape(context: &str, row: &Row) {
    if std::env::var("AM_DEBUG_TUI_POLLER").ok().as_deref() != Some("1") {
        return;
    }
    let columns: Vec<String> = row.column_names().map(ToString::to_string).collect();
    let values: Vec<Value> = row.values().cloned().collect();
    eprintln!("{context}: columns={columns:?} values={values:?}");
}

/// Days from civil date (year, month 1-12, day 1-31) to Unix epoch.
/// Adapted from Howard Hinnant's `days_from_civil` algorithm.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const RESERVATION_LEGACY_SCAN_SQL: &str = "SELECT \
   fr.id, \
   fr.project_id AS raw_project_id, \
   COALESCE(p.slug, '[unknown-project]') AS project_slug, \
   COALESCE(a.name, '[unknown-agent]') AS agent_name, \
   fr.path_pattern, \
   fr.\"exclusive\", \
   fr.created_ts AS raw_created_ts, \
   fr.expires_ts AS raw_expires_ts, \
   fr.released_ts AS raw_released_ts \
 FROM file_reservations fr \
 LEFT JOIN projects p ON p.id = fr.project_id \
 LEFT JOIN agents a ON a.id = fr.agent_id \
 ORDER BY fr.id ASC";

static RESERVATION_ACTIVE_FAST_SQL: OnceLock<String> = OnceLock::new();

fn reservation_active_fast_sql() -> &'static str {
    RESERVATION_ACTIVE_FAST_SQL
        .get_or_init(|| {
            format!(
                "SELECT \
                   fr.id, \
                   fr.project_id AS raw_project_id, \
                   COALESCE(p.slug, '[unknown-project]') AS project_slug, \
                   COALESCE(a.name, '[unknown-agent]') AS agent_name, \
                   fr.path_pattern, \
                   fr.\"exclusive\", \
                   fr.created_ts AS raw_created_ts, \
                   fr.expires_ts AS raw_expires_ts, \
                   fr.released_ts AS raw_released_ts \
                 FROM file_reservations fr \
                 LEFT JOIN projects p ON p.id = fr.project_id \
                 LEFT JOIN agents a ON a.id = fr.agent_id \
                 WHERE ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts > ? \
                 ORDER BY fr.id ASC"
            )
        })
        .as_str()
}

fn reservation_scan_mode(conn: &DbConn, sqlite_path: Option<&str>) -> ReservationScanMode {
    let now = Instant::now();
    if let Some(path) = sqlite_path {
        let cache = RESERVATION_SCAN_MODE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        {
            let guard = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = guard.get(path)
                && now.duration_since(entry.checked_at) < RESERVATION_SCAN_MODE_CACHE_TTL
            {
                return entry.mode;
            }
        }
        let mode = detect_reservation_scan_mode(conn);
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                path.to_string(),
                ReservationScanCacheEntry {
                    mode,
                    checked_at: now,
                },
            );
        return mode;
    }

    detect_reservation_scan_mode(conn)
}

fn detect_reservation_scan_mode(conn: &DbConn) -> ReservationScanMode {
    // Conservative policy: if detection is uncertain, keep legacy full-scan
    // semantics so we never drop active reservations from the UI.
    let Some(declared_text) = file_reservations_expires_declared_text(conn) else {
        return ReservationScanMode::FullLegacy;
    };
    if declared_text {
        return ReservationScanMode::FullLegacy;
    }

    let Some(has_text_values) = file_reservations_contains_text_expires_values(conn) else {
        return ReservationScanMode::FullLegacy;
    };
    if has_text_values {
        ReservationScanMode::FullLegacy
    } else {
        ReservationScanMode::ActiveFast
    }
}

fn file_reservations_expires_declared_text(conn: &DbConn) -> Option<bool> {
    let rows = conn
        .query_sync("PRAGMA table_info(file_reservations)", &[])
        .ok()?;
    for row in rows {
        let Ok(name) = row.get_named::<String>("name") else {
            continue;
        };
        if name != "expires_ts" {
            continue;
        }
        let declared = row.get_named::<String>("type").ok().unwrap_or_default();
        let upper = declared.to_ascii_uppercase();
        return Some(upper.contains("TEXT") || upper.contains("CHAR") || upper.contains("CLOB"));
    }
    None
}

fn file_reservations_contains_text_expires_values(conn: &DbConn) -> Option<bool> {
    conn.query_sync(
        "SELECT 1 AS has_text \
         FROM file_reservations \
         WHERE typeof(expires_ts) = 'text' \
         LIMIT 1",
        &[],
    )
    .ok()
    .map(|rows| !rows.is_empty())
}

#[allow(clippy::too_many_lines)]
fn fetch_reservation_snapshot_bundle(
    conn: &DbConn,
    now: i64,
    sqlite_path: Option<&str>,
) -> ReservationSnapshotBundle {
    let scan_mode = reservation_scan_mode(conn, sqlite_path);
    let rows = match scan_mode {
        ReservationScanMode::ActiveFast => {
            conn.query_sync(reservation_active_fast_sql(), &[Value::BigInt(now)])
        }
        ReservationScanMode::FullLegacy => conn.query_sync(RESERVATION_LEGACY_SCAN_SQL, &[]),
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(
                mode = ?scan_mode,
                error = ?err,
                "tui_poller.fetch_reservation_snapshots query failed"
            );
            return ReservationSnapshotBundle::default();
        }
    };
    #[cfg(test)]
    if let Some(first) = rows.first() {
        debug_row_shape("fetch_reservation_snapshots", first);
    }

    let mut active_count = 0_u64;
    let mut active_counts_by_project: HashMap<i64, u64> = HashMap::new();
    let mut snapshots = BinaryHeap::new();

    for row in rows {
        if !is_active_reservation_row(&row, now, "raw_expires_ts", "raw_released_ts") {
            continue;
        }

        active_count = active_count.saturating_add(1);
        if let Some(project_id) = parse_raw_i64(&row, "raw_project_id") {
            let count = active_counts_by_project.entry(project_id).or_insert(0_u64);
            *count = (*count).saturating_add(1_u64);
        }

        if MAX_RESERVATIONS == 0 {
            continue;
        }

        let Some(id) = parse_raw_i64(&row, "id") else {
            continue;
        };
        let Some(path_pattern) = row.get_named::<String>("path_pattern").ok() else {
            continue;
        };
        let snapshot = ReservationSnapshot {
            id,
            project_slug: row
                .get_named::<String>("project_slug")
                .ok()
                .unwrap_or_else(|| "[unknown-project]".to_string()),
            agent_name: row
                .get_named::<String>("agent_name")
                .ok()
                .unwrap_or_else(|| "[unknown-agent]".to_string()),
            path_pattern,
            exclusive: row
                .get_named::<i64>("exclusive")
                .ok()
                .is_none_or(|value| value != 0),
            granted_ts: parse_raw_ts(&row, "raw_created_ts"),
            expires_ts: parse_raw_ts(&row, "raw_expires_ts"),
            released_ts: None,
        };
        let entry = SnapshotHeapEntry {
            sort_key: (snapshot.expires_ts, snapshot.id),
            snapshot,
        };
        if snapshots.len() < MAX_RESERVATIONS {
            snapshots.push(entry);
            continue;
        }
        if snapshots
            .peek()
            .is_some_and(|worst| entry.sort_key < worst.sort_key)
        {
            let _ = snapshots.pop();
            snapshots.push(entry);
        }
    }

    let mut snapshots: Vec<_> = snapshots.into_iter().map(|entry| entry.snapshot).collect();
    snapshots.sort_by_key(|snapshot| (snapshot.expires_ts, snapshot.id));
    ReservationSnapshotBundle {
        active_count,
        active_counts_by_project,
        snapshots,
    }
}

/// Fetch active file reservations with project and agent names.
///
/// This is reused by the reservations screen as a direct fallback when the
/// background poller snapshot is unavailable or stale.
#[allow(clippy::too_many_lines)]
pub(crate) fn fetch_reservation_snapshots(conn: &DbConn) -> Vec<ReservationSnapshot> {
    fetch_reservation_snapshot_bundle(conn, now_micros(), None).snapshots
}

/// Read `CONSOLE_POLL_INTERVAL_MS` from environment, default 2000ms.
/// Values below [`MIN_POLL_INTERVAL`] are clamped to avoid tight spin loops.
fn poll_interval_from_env() -> Duration {
    std::env::var("CONSOLE_POLL_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_POLL_INTERVAL, |ms| {
            Duration::from_millis(ms).max(MIN_POLL_INTERVAL)
        })
}

// ──────────────────────────────────────────────────────────────────────
// Delta detection helpers (public for testing)
// ──────────────────────────────────────────────────────────────────────

/// Compute which fields changed between two snapshots.
#[must_use]
pub fn snapshot_delta(prev: &DbStatSnapshot, curr: &DbStatSnapshot) -> SnapshotDelta {
    SnapshotDelta {
        projects_changed: prev.projects != curr.projects,
        agents_changed: prev.agents != curr.agents,
        messages_changed: prev.messages != curr.messages,
        reservations_changed: prev.file_reservations != curr.file_reservations,
        contacts_changed: prev.contact_links != curr.contact_links,
        ack_changed: prev.ack_pending != curr.ack_pending,
        agents_list_changed: prev.agents_list != curr.agents_list,
        projects_list_changed: prev.projects_list != curr.projects_list,
        contacts_list_changed: prev.contacts_list != curr.contacts_list,
        reservation_snapshots_changed: prev.reservation_snapshots != curr.reservation_snapshots,
    }
}

/// Which fields changed between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SnapshotDelta {
    pub projects_changed: bool,
    pub agents_changed: bool,
    pub messages_changed: bool,
    pub reservations_changed: bool,
    pub contacts_changed: bool,
    pub ack_changed: bool,
    pub agents_list_changed: bool,
    pub projects_list_changed: bool,
    pub contacts_list_changed: bool,
    pub reservation_snapshots_changed: bool,
}

impl SnapshotDelta {
    /// Whether any field changed.
    #[must_use]
    pub const fn any_changed(&self) -> bool {
        self.projects_changed
            || self.agents_changed
            || self.messages_changed
            || self.reservations_changed
            || self.contacts_changed
            || self.ack_changed
            || self.agents_list_changed
            || self.projects_list_changed
            || self.contacts_list_changed
            || self.reservation_snapshots_changed
    }

    /// Count of changed fields.
    #[must_use]
    pub fn changed_count(&self) -> usize {
        [
            self.projects_changed,
            self.agents_changed,
            self.messages_changed,
            self.reservations_changed,
            self.contacts_changed,
            self.ack_changed,
            self.agents_list_changed,
            self.projects_list_changed,
            self.contacts_list_changed,
            self.reservation_snapshots_changed,
        ]
        .iter()
        .filter(|&&b| b)
        .count()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::Config;

    const FAR_FUTURE_MICROS: i64 = 4_102_444_800_000_000; // 2100-01-01T00:00:00Z

    // ── Delta detection ──────────────────────────────────────────────

    #[test]
    fn delta_detects_no_change() {
        let a = DbStatSnapshot::default();
        let b = DbStatSnapshot::default();
        let d = snapshot_delta(&a, &b);
        assert!(!d.any_changed());
        assert_eq!(d.changed_count(), 0);
    }

    #[test]
    fn delta_detects_single_field_change() {
        let a = DbStatSnapshot::default();
        let mut b = a.clone();
        b.messages = 42;
        let d = snapshot_delta(&a, &b);
        assert!(d.any_changed());
        assert!(d.messages_changed);
        assert!(!d.projects_changed);
        assert_eq!(d.changed_count(), 1);
    }

    #[test]
    fn delta_detects_multiple_changes() {
        let a = DbStatSnapshot {
            projects: 1,
            agents: 2,
            messages: 10,
            file_reservations: 3,
            contact_links: 1,
            ack_pending: 0,
            agents_list: vec![],
            timestamp_micros: 100,
            ..Default::default()
        };
        let b = DbStatSnapshot {
            projects: 2,
            agents: 2,
            messages: 15,
            file_reservations: 3,
            contact_links: 1,
            ack_pending: 1,
            agents_list: vec![],
            timestamp_micros: 200,
            ..Default::default()
        };
        let d = snapshot_delta(&a, &b);
        assert!(d.projects_changed);
        assert!(d.messages_changed);
        assert!(d.ack_changed);
        assert!(!d.agents_changed);
        assert!(!d.reservations_changed);
        assert!(!d.reservation_snapshots_changed);
        assert_eq!(d.changed_count(), 3);
    }

    #[test]
    fn delta_detects_agents_list_change() {
        let a = DbStatSnapshot {
            agents_list: vec![AgentSummary {
                name: "GoldFox".into(),
                program: "claude-code".into(),
                last_active_ts: 100,
            }],
            ..Default::default()
        };
        let mut b = a.clone();
        b.agents_list[0].last_active_ts = 200;
        let d = snapshot_delta(&a, &b);
        assert!(d.agents_list_changed);
        assert_eq!(d.changed_count(), 1);
    }

    #[test]
    fn delta_detects_reservation_snapshot_change_without_count_change() {
        let a = DbStatSnapshot {
            file_reservations: 1,
            reservation_snapshots: vec![ReservationSnapshot {
                id: 1,
                project_slug: "proj".into(),
                agent_name: "BlueLake".into(),
                path_pattern: "src/**".into(),
                exclusive: true,
                granted_ts: 10,
                expires_ts: 20,
                released_ts: None,
            }],
            ..Default::default()
        };
        let b = DbStatSnapshot {
            file_reservations: 1,
            reservation_snapshots: vec![ReservationSnapshot {
                id: 1,
                project_slug: "proj".into(),
                agent_name: "BlueLake".into(),
                path_pattern: "tests/**".into(),
                exclusive: true,
                granted_ts: 10,
                expires_ts: 20,
                released_ts: None,
            }],
            ..Default::default()
        };

        let d = snapshot_delta(&a, &b);
        assert!(!d.reservations_changed);
        assert!(d.reservation_snapshots_changed);
        assert_eq!(d.changed_count(), 1);
    }

    #[test]
    fn delta_detects_all_fields_changed() {
        let a = DbStatSnapshot::default();
        let b = DbStatSnapshot {
            projects: 1,
            agents: 1,
            messages: 1,
            file_reservations: 1,
            contact_links: 1,
            ack_pending: 1,
            agents_list: vec![AgentSummary {
                name: "X".into(),
                program: "Y".into(),
                last_active_ts: 1,
            }],
            projects_list: vec![ProjectSummary {
                id: 1,
                slug: "p".into(),
                ..Default::default()
            }],
            contacts_list: vec![ContactSummary {
                from_agent: "A".into(),
                to_agent: "B".into(),
                ..Default::default()
            }],
            reservation_snapshots: vec![ReservationSnapshot {
                id: 1,
                project_slug: "p".into(),
                agent_name: "A".into(),
                path_pattern: "*.rs".into(),
                exclusive: true,
                granted_ts: 1,
                expires_ts: 999,
                released_ts: None,
            }],
            timestamp_micros: 1,
        };
        let d = snapshot_delta(&a, &b);
        assert_eq!(d.changed_count(), 10);
    }

    // ── Poll interval ────────────────────────────────────────────────

    #[test]
    fn default_poll_interval() {
        // Without env var set, should use default
        let interval = DEFAULT_POLL_INTERVAL;
        assert_eq!(interval.as_millis(), 2000);
    }

    // ── DbPoller construction ────────────────────────────────────────

    #[test]
    fn poller_construction_and_interval_override() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller = DbPoller::new(Arc::clone(&state), "sqlite:///test.db".into())
            .with_interval(Duration::from_millis(500));
        assert_eq!(poller.interval, Duration::from_millis(500));
        assert!(!poller.stop.load(Ordering::Relaxed));
    }

    // ── Handle stop semantics ────────────────────────────────────────

    #[test]
    fn handle_stop_is_idempotent() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller = DbPoller::new(Arc::clone(&state), "sqlite:///nonexistent.db".into())
            .with_interval(Duration::from_millis(50));
        let mut handle = poller.start();

        // Stop twice should be fine
        handle.stop();
        handle.stop();
    }

    #[test]
    fn handle_signal_and_join() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller = DbPoller::new(Arc::clone(&state), "sqlite:///nonexistent.db".into())
            .with_interval(Duration::from_millis(50));
        let mut handle = poller.start();

        handle.signal_stop();
        handle.join();
    }

    // ── Integration: poller pushes stats ─────────────────────────────

    #[test]
    fn poller_pushes_snapshot_on_change() {
        // Create a temp DB with the expected tables
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_poller.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        // Create tables
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER)",
            &[],
        )
        .expect("create file_reservations");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS agent_links (id INTEGER PRIMARY KEY)",
            &[],
        )
        .expect("create agent_links");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, ack_ts INTEGER)",
            &[],
        )
        .expect("create message_recipients");

        // Insert some data
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('proj1', 'hk1', 100)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (name, program, last_active_ts) VALUES ('GoldFox', 'claude-code', 200)",
            &[],
        )
        .expect("insert agent");
        conn.execute_sync("INSERT INTO messages (id) VALUES (1)", &[])
            .expect("insert message");
        drop(conn);

        // Start poller
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(50));
        let mut handle = poller.start();

        // Wait for at least one poll cycle
        thread::sleep(Duration::from_millis(200));

        // Check that stats were pushed
        let snapshot = state.db_stats_snapshot().expect("should have stats");
        assert_eq!(snapshot.projects, 1);
        assert_eq!(snapshot.agents, 1);
        assert_eq!(snapshot.messages, 1);
        assert_eq!(snapshot.agents_list.len(), 1);
        assert_eq!(snapshot.agents_list[0].name, "GoldFox");

        // Check a HealthPulse event was emitted
        let events = state.recent_events(10);
        assert!(
            events
                .iter()
                .any(|e| e.kind() == crate::tui_events::MailEventKind::HealthPulse),
            "expected a HealthPulse event"
        );

        handle.stop();
    }

    #[test]
    fn poller_skips_update_when_no_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_no_change.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        // Create minimal tables (empty DB)
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_sync("CREATE TABLE projects (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync("CREATE TABLE messages (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync("CREATE TABLE agent_links (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, ack_ts INTEGER)",
            &[],
        )
        .expect("create");
        drop(conn);

        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 100);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(50));
        let mut handle = poller.start();

        // Wait for multiple poll cycles
        thread::sleep(Duration::from_millis(300));

        // Should only have emitted ONE HealthPulse (the initial change from default -> zeroed+timestamp)
        let events = state.recent_events(100);
        let pulse_count = events
            .iter()
            .filter(|e| e.kind() == crate::tui_events::MailEventKind::HealthPulse)
            .count();

        // At most 1-2 (initial change detection), not one per cycle
        assert!(
            pulse_count <= 2,
            "expected at most 2 health pulses for unchanged DB, got {pulse_count}"
        );

        handle.stop();
    }

    #[test]
    fn poller_refreshes_snapshot_timestamp_without_data_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_snapshot_refresh.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_sync("CREATE TABLE projects (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync("CREATE TABLE messages (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync("CREATE TABLE agent_links (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, ack_ts INTEGER)",
            &[],
        )
        .expect("create");
        drop(conn);

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(50));
        let mut handle = poller.start();

        thread::sleep(Duration::from_millis(120));
        let first = state.db_stats_snapshot().expect("first snapshot");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut second = first.clone();
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
            second = state.db_stats_snapshot().expect("second snapshot");
            if second.timestamp_micros > first.timestamp_micros {
                break;
            }
        }

        assert!(
            second.timestamp_micros > first.timestamp_micros,
            "expected timestamp_micros to advance even with unchanged counts"
        );

        handle.stop();
    }

    #[test]
    fn batcher_fetch_counts_aggregates_metrics_in_single_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_batch_counts.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, ack_required INTEGER)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, released_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_agent_id INTEGER, b_agent_id INTEGER, a_project_id INTEGER, b_project_id INTEGER, status TEXT, reason TEXT, updated_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create links");
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, ack_ts INTEGER)",
            &[],
        )
        .expect("create recipients");

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES
             (1, 'proj-a', 'hk-a', 100), (2, 'proj-b', 'hk-b', 200)",
            &[],
        )
        .expect("insert projects");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, last_active_ts) VALUES
             (1, 1, 'BlueLake', 'codex', 100), (2, 1, 'RedFox', 'claude', 101), (3, 2, 'GoldPeak', 'codex', 102)",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, ack_required) VALUES
             (10, 1, 1, 1), (11, 1, 2, 0)",
            &[],
        )
        .expect("insert messages");
        conn.execute_sync(
            "INSERT INTO file_reservations (id, project_id, released_ts, expires_ts) VALUES
             (20, 1, NULL, 4102444800000000), (21, 1, 12345, 4102444800000000)",
            &[],
        )
        .expect("insert reservations");
        conn.execute_sync(
            "INSERT INTO agent_links (id, a_agent_id, b_agent_id, a_project_id, b_project_id, status, reason, updated_ts, expires_ts) VALUES
             (30, 1, 2, 1, 1, 'accepted', '', 0, NULL),
             (31, 2, 3, 1, 2, 'accepted', '', 0, NULL)",
            &[],
        )
        .expect("insert links");
        conn.execute_sync(
            "INSERT INTO message_recipients (id, message_id, ack_ts) VALUES
             (40, 10, NULL), (41, 10, 99999), (42, 11, NULL)",
            &[],
        )
        .expect("insert recipients");

        let counts = DbStatQueryBatcher::new(&conn).fetch_counts();
        assert_eq!(
            counts,
            DbSnapshotCounts {
                projects: 2,
                agents: 3,
                messages: 2,
                file_reservations: 1,
                contact_links: 2,
                ack_pending: 1,
            }
        );
    }

    // ── fetch_db_stats with nonexistent DB ───────────────────────────

    #[test]
    fn fetch_stats_returns_none_on_bad_url() {
        // Use 4 slashes for absolute path; /dev/null is a file so subdir creation fails.
        assert!(fetch_db_stats("sqlite:////dev/null/impossible.db").is_none());
    }

    #[test]
    fn fetch_stats_returns_none_on_empty_url() {
        assert!(fetch_db_stats("").is_none());
    }

    // ── open_sync_connection ─────────────────────────────────────────

    #[test]
    fn open_sync_connection_returns_none_on_bad_path() {
        // Use 4 slashes for absolute path; /dev/null is a file so subdir creation fails.
        assert!(open_sync_connection("sqlite:////dev/null/impossible.db").is_none());
    }

    #[test]
    fn open_sync_connection_succeeds_with_valid_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let url = format!("sqlite:///{}", db_path.display());
        assert!(open_sync_connection(&url).is_some());
    }

    #[test]
    fn open_sync_connection_returns_none_for_memory_url() {
        assert!(open_sync_connection("sqlite:///:memory:").is_none());
        assert!(open_sync_connection("sqlite:///:memory:?cache=shared").is_none());
    }

    #[test]
    fn catch_optional_panic_returns_value_when_no_panic() {
        let result = catch_optional_panic(|| Some(7_u64));
        assert_eq!(result.expect("no panic expected"), Some(7));
    }

    #[test]
    fn catch_optional_panic_converts_panic_to_error() {
        let result = catch_optional_panic::<u64, _>(|| panic!("boom"));
        assert!(result.is_err(), "panic should be captured");
    }

    #[test]
    fn reservation_snapshots_keep_rows_when_agent_or_project_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_orphans.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts INTEGER
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES
                (1, 111, 222, 'src/**', 1, 1000000, 4102444800000000, NULL)",
            &[],
        )
        .expect("insert orphan reservation");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project_slug, "[unknown-project]");
        assert_eq!(rows[0].agent_name, "[unknown-agent]");
        assert_eq!(rows[0].path_pattern, "src/**");
    }

    #[test]
    fn reservation_snapshots_accept_legacy_text_timestamps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_legacy_timestamps.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts TEXT,
                expires_ts TEXT,
                released_ts TEXT
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'proj')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (2, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES
                (1, 1, 2, 'src/**', 1, '2099-12-31 10:00:00.123456', '2099-12-31 11:00:00.123456', NULL),
                (2, 1, 2, 'tests/**', 0, '2099-12-31 10:10:00.000000', '2099-12-31 11:10:00.000000', ''),
                (3, 1, 2, 'docs/**', 0, '2099-12-31 10:20:00.000000', '2099-12-31 11:20:00.000000', '2099-12-31 10:30:00.000000')",
            &[],
        )
        .expect("insert reservations");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path_pattern, "src/**");
        assert_eq!(rows[1].path_pattern, "tests/**");
        assert!(rows[0].granted_ts > 0);
        assert!(rows[0].expires_ts > rows[0].granted_ts);
        assert!(rows.iter().all(|row| row.released_ts.is_none()));
    }

    #[test]
    fn reservation_snapshots_keep_invalid_text_timestamp_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_invalid_timestamps.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts TEXT,
                expires_ts TEXT,
                released_ts TEXT
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'proj')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES (1, 1, 1, 'broken/**', 1, 'not-a-date', '4102444800000000', NULL)",
            &[],
        )
        .expect("insert reservation");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path_pattern, "broken/**");
        assert_eq!(rows[0].granted_ts, 0);
        assert_eq!(rows[0].expires_ts, FAR_FUTURE_MICROS);
    }

    #[test]
    fn reservation_snapshots_treat_zero_released_ts_as_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_zero_released.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts INTEGER
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'proj')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES
                (1, 1, 1, 'src/**', 1, 1000, 4102444800000000, 0),
                (2, 1, 1, 'tests/**', 1, 1000, 4102444800000000, NULL),
                (3, 1, 1, 'docs/**', 1, 1000, 4102444800000000, 123456)",
            &[],
        )
        .expect("insert reservations");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.path_pattern == "src/**"));
        assert!(rows.iter().any(|row| row.path_pattern == "tests/**"));
    }

    #[test]
    fn reservation_snapshots_accept_numeric_text_micros() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_numeric_text.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts TEXT,
                expires_ts TEXT,
                released_ts TEXT
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'proj')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES
                (1, 1, 1, 'src/**', 1, '1771210958613964', '4102444800000000', '0'),
                (2, 1, 1, 'docs/**', 1, '1771210958613999', '4102444800000000', '1771211000000000')",
            &[],
        )
        .expect("insert reservations");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path_pattern, "src/**");
        assert_eq!(rows[0].granted_ts, 1_771_210_958_613_964);
        assert_eq!(rows[0].expires_ts, FAR_FUTURE_MICROS);
    }

    #[test]
    fn reservation_snapshots_treat_numeric_text_zero_variants_as_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_numeric_zero_variants.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts TEXT
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'proj')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES
                (1, 1, 1, 'src/**', 1, 1000, 4102444800000000, '0.0'),
                (2, 1, 1, 'tests/**', 0, 1000, 4102444800000000, '-1'),
                (3, 1, 1, 'docs/**', 1, 1000, 4102444800000000, '1771211000000000')",
            &[],
        )
        .expect("insert reservations");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.path_pattern == "src/**"));
        assert!(rows.iter().any(|row| row.path_pattern == "tests/**"));
    }

    #[test]
    fn fetch_counts_treats_legacy_active_released_ts_sentinels_as_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_counts_legacy_released_ts.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync("CREATE TABLE projects (id INTEGER PRIMARY KEY)", &[])
            .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync("CREATE TABLE messages (id INTEGER PRIMARY KEY)", &[])
            .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE message_recipients (message_id INTEGER, ack_ts INTEGER)",
            &[],
        )
        .expect("create recipients");
        conn.execute_sync("CREATE TABLE agent_links (id INTEGER PRIMARY KEY)", &[])
            .expect("create links");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                exclusive INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts TEXT
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts, released_ts)
             VALUES
                (1, 1, 1, 'src/**', 1, 1000, 4102444800000000, NULL),
                (2, 1, 1, 'tests/**', 1, 1000, 4102444800000000, '0'),
                (3, 1, 1, 'docs/**', 1, 1000, 4102444800000000, 'null'),
                (4, 1, 1, 'tmp/**', 1, 1000, 4102444800000000, '0.0'),
                (5, 1, 1, 'build/**', 1, 1000, 4102444800000000, '1771211000000000')",
            &[],
        )
        .expect("insert reservations");

        let counts = DbStatQueryBatcher::new(&conn).fetch_counts();
        assert_eq!(counts.file_reservations, 4);
    }

    // ── Additional coverage tests ────────────────────────────────────

    #[test]
    fn db_snapshot_counts_default() {
        let counts = DbSnapshotCounts::default();
        assert_eq!(counts.projects, 0);
        assert_eq!(counts.agents, 0);
        assert_eq!(counts.messages, 0);
        assert_eq!(counts.file_reservations, 0);
        assert_eq!(counts.contact_links, 0);
        assert_eq!(counts.ack_pending, 0);
    }

    #[test]
    fn snapshot_delta_identical_nondefault_no_change() {
        let snap = DbStatSnapshot {
            projects: 5,
            agents: 3,
            messages: 100,
            file_reservations: 10,
            contact_links: 2,
            ack_pending: 1,
            agents_list: vec![AgentSummary {
                name: "GoldFox".into(),
                program: "claude-code".into(),
                last_active_ts: 1000,
            }],
            ..Default::default()
        };
        let d = snapshot_delta(&snap, &snap);
        assert!(!d.any_changed());
        assert_eq!(d.changed_count(), 0);
    }

    #[test]
    fn snapshot_delta_projects_list_change() {
        let a = DbStatSnapshot::default();
        let b = DbStatSnapshot {
            projects_list: vec![ProjectSummary {
                id: 1,
                slug: "test".into(),
                human_key: "hk".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let d = snapshot_delta(&a, &b);
        assert!(d.projects_list_changed);
        assert!(!d.projects_changed); // count didn't change
        assert_eq!(d.changed_count(), 1);
    }

    #[test]
    fn snapshot_delta_contacts_list_change() {
        let a = DbStatSnapshot::default();
        let b = DbStatSnapshot {
            contacts_list: vec![ContactSummary {
                from_agent: "A".into(),
                to_agent: "B".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let d = snapshot_delta(&a, &b);
        assert!(d.contacts_list_changed);
        assert_eq!(d.changed_count(), 1);
    }

    #[test]
    fn snapshot_delta_ack_only() {
        let a = DbStatSnapshot {
            ack_pending: 0,
            ..Default::default()
        };
        let b = DbStatSnapshot {
            ack_pending: 5,
            ..Default::default()
        };
        let d = snapshot_delta(&a, &b);
        assert!(d.ack_changed);
        assert!(!d.messages_changed);
        assert_eq!(d.changed_count(), 1);
    }

    #[test]
    fn active_reservation_predicate_is_nonempty() {
        assert!(!ACTIVE_RESERVATION_PREDICATE.is_empty());
        assert!(ACTIVE_RESERVATION_PREDICATE.contains("released_ts IS NULL"));
    }

    #[test]
    fn max_constants_are_positive() {
        const {
            assert!(MAX_AGENTS > 0);
            assert!(MAX_PROJECTS > 0);
            assert!(MAX_CONTACTS > 0);
            assert!(MAX_RESERVATIONS > 0);
        }
    }

    #[test]
    fn batcher_fetch_counts_fallback_on_empty_tables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_fallback_counts.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync("CREATE TABLE projects (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync("CREATE TABLE messages (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync("CREATE TABLE agent_links (id INTEGER PRIMARY KEY)", &[])
            .expect("create");
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY, message_id INTEGER, ack_ts INTEGER)",
            &[],
        )
        .expect("create");

        let counts = DbStatQueryBatcher::new(&conn).fetch_counts();
        assert_eq!(counts, DbSnapshotCounts::default());
    }

    #[test]
    fn fetch_agents_list_returns_empty_for_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_agents_no_table.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        // No tables created
        let agents = fetch_agents_list(&conn);
        assert!(agents.is_empty());
    }

    #[test]
    fn fetch_projects_list_returns_empty_for_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_no_table.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        let projects = fetch_projects_list(&conn);
        assert!(projects.is_empty());
    }

    #[test]
    fn fetch_contacts_list_returns_empty_for_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_contacts_no_table.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        let contacts = fetch_contacts_list(&conn);
        assert!(contacts.is_empty());
    }

    #[test]
    fn fetch_reservation_snapshots_returns_empty_for_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservations_no_table.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        let reservations = fetch_reservation_snapshots(&conn);
        assert!(reservations.is_empty());
    }

    #[test]
    fn fetch_agents_list_ordered_by_last_active_desc() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_agents_order.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync(
            "INSERT INTO agents (name, program, last_active_ts) VALUES
             ('OldAgent', 'codex', 100),
             ('NewAgent', 'claude', 300),
             ('MidAgent', 'gemini', 200)",
            &[],
        )
        .expect("insert");

        let agents = fetch_agents_list(&conn);
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].name, "NewAgent");
        assert_eq!(agents[1].name, "MidAgent");
        assert_eq!(agents[2].name, "OldAgent");
    }

    #[test]
    fn fetch_agents_list_uses_id_tiebreak_for_stable_ordering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_agents_order_tie.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync(
            "INSERT INTO agents (id, name, program, last_active_ts) VALUES
             (41, 'Alpha', 'codex', 500),
             (42, 'Beta', 'claude', 500)",
            &[],
        )
        .expect("insert");

        let agents = fetch_agents_list(&conn);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "Beta");
        assert_eq!(agents[1].name, "Alpha");
    }

    #[test]
    fn fetch_projects_list_includes_aggregate_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_aggregates.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, released_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create reservations");

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj', 'hk', 100)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, last_active_ts) VALUES (1, 'A', 'x', 0), (1, 'B', 'y', 0)",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO messages (project_id) VALUES (1), (1), (1)",
            &[],
        )
        .expect("insert messages");
        conn.execute_sync(
            "INSERT INTO file_reservations (project_id, released_ts, expires_ts) VALUES (1, NULL, 4102444800000000)",
            &[],
        )
        .expect("insert reservation");

        let projects = fetch_projects_list(&conn);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].slug, "proj");
        assert_eq!(projects[0].agent_count, 2);
        assert_eq!(projects[0].message_count, 3);
        assert_eq!(projects[0].reservation_count, 1);
    }

    #[test]
    fn fetch_projects_list_uses_id_tiebreak_for_stable_ordering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_order_tie.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, released_ts INTEGER, expires_ts INTEGER)",
            &[],
        )
        .expect("create reservations");

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES
             (11, 'alpha', '/p/a', 1000),
             (12, 'beta', '/p/b', 1000)",
            &[],
        )
        .expect("insert projects");

        let projects = fetch_projects_list(&conn);
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].slug, "beta");
        assert_eq!(projects[1].slug, "alpha");
    }

    #[test]
    fn health_pulse_heartbeat_interval_is_reasonable() {
        assert!(HEALTH_PULSE_HEARTBEAT_INTERVAL.as_secs() >= 5);
        assert!(HEALTH_PULSE_HEARTBEAT_INTERVAL.as_secs() <= 60);
    }
}
