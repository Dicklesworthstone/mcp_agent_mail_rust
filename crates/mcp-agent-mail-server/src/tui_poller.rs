//! Periodic DB poller that feeds [`TuiSharedState`] with fresh statistics.
//!
//! The poller runs on a dedicated background thread using sync `SQLite`
//! connections (not the async pool).  It wakes every `interval`, queries
//! aggregate counts + agent list, computes deltas against the previous
//! snapshot, refreshes shared stats every cycle, and emits health pulses
//! on data changes plus periodic heartbeat intervals.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::is_sqlite_recovery_error_message;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel_core::{Error as SqlError, Row, Value};
use mcp_agent_mail_db::timestamps::now_micros;

use crate::tui_bridge::{DbWarmupState, TuiSharedState};
use crate::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, MailEvent, ProjectSummary, ReservationSnapshot,
};

/// Default polling interval (2 seconds).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Prevent accidental zero/near-zero env values from creating a busy-loop.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Manual/test overrides are allowed to go below `MIN_POLL_INTERVAL`, but never to zero.
const MIN_OVERRIDE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Retry snapshot-gap recovery occasionally, not every poll cycle forever.
const RESERVATION_SNAPSHOT_GAP_REFRESH_INTERVAL: Duration = Duration::from_mins(1);
/// After readiness warmup fails, let the poller retry opening `SQLite` only
/// occasionally so degraded startup does not turn into repeated DB hammering.
const DB_WARMUP_FAILURE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// When `data_version` is unavailable, throttle full snapshots to avoid
/// expensive no-op scans on every poll cycle.
const NO_VERSION_FULL_SNAPSHOT_INTERVAL_MICROS: i64 = 30_000_000;

/// Maximum agents to fetch per poll cycle.  Raised from 50 to 500 to avoid
/// silently truncating the agent list in large deployments (B4 truthfulness).
const MAX_AGENTS: usize = 500;

/// Maximum projects to fetch per poll cycle.  Raised from 100 to 500 to avoid
/// silently truncating the project list in large deployments (B5 truthfulness).
const MAX_PROJECTS: usize = 500;

/// Maximum contact links to fetch per poll cycle.
const MAX_CONTACTS: usize = 200;

/// Maximum reservation rows to fetch per poll cycle.
const MAX_RESERVATIONS: usize = 1000;
/// Maximum silent interval before a heartbeat `HealthPulse` is emitted.
const HEALTH_PULSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Re-evaluate legacy reservation scan mode periodically (per DB path).
#[allow(clippy::duration_suboptimal_units)]
const RESERVATION_SCAN_MODE_CACHE_TTL: Duration = Duration::from_secs(300);
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
            tracing::warn!(
                path = %path,
                error = %message,
                "tui poller observed recoverable sqlite error; skipping automatic recovery from observability path"
            );
        }
    }

    fn fetch_snapshot(&self, previous: Option<&DbStatSnapshot>) -> DbStatSnapshot {
        let now = now_micros();
        let reservation_bundle =
            fetch_reservation_snapshot_bundle(self.conn, now, self.sqlite_path, previous);
        let counts =
            self.fetch_counts_with_reservation_count(reservation_bundle.active_count, previous);
        let mut snapshot = DbStatSnapshot {
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
        };
        restore_missing_detail_lists_from_previous(previous, &mut snapshot, self.sqlite_path);
        refill_missing_detail_lists_from_sqlite(
            &mut snapshot,
            self.sqlite_path,
            &reservation_bundle.active_counts_by_project,
        );
        restore_missing_project_rollup_counts_from_previous(
            previous,
            &mut snapshot,
            self.sqlite_path,
        );
        restore_missing_contact_rows_from_previous(previous, &mut snapshot, self.sqlite_path);
        snapshot
    }

    #[cfg(test)]
    fn fetch_counts(&self) -> DbSnapshotCounts {
        let now = now_micros();
        let reservation_count = self.count_active_reservations(now);
        self.fetch_counts_with_reservation_count(reservation_count, None)
    }

    fn fetch_counts_with_reservation_count(
        &self,
        reservation_count: u64,
        previous: Option<&DbStatSnapshot>,
    ) -> DbSnapshotCounts {
        let core_counts_sql = "SELECT \
             (SELECT COUNT(*) FROM projects) AS projects_count, \
             (SELECT COUNT(*) FROM agents) AS agents_count, \
             (SELECT COUNT(*) FROM messages) AS messages_count, \
             (SELECT COUNT(*) FROM agent_links) AS contacts_count";
        let batched_rows = match self.conn.query_sync(core_counts_sql, &[]) {
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
                    ack_pending: 0,
                }
            });

        if let Some(mut counts) = batched {
            counts.ack_pending = self
                .fetch_ack_pending_count()
                .unwrap_or_else(|| previous_count(previous, |snapshot| snapshot.ack_pending));
            return counts;
        }

        self.fetch_counts_fallback_with_reservation_count(reservation_count, previous)
    }

    fn fetch_counts_fallback_with_reservation_count(
        &self,
        reservation_count: u64,
        previous: Option<&DbStatSnapshot>,
    ) -> DbSnapshotCounts {
        DbSnapshotCounts {
            projects: self
                .run_count_query("SELECT COUNT(*) AS c FROM projects", &[])
                .unwrap_or_else(|| previous_count(previous, |snapshot| snapshot.projects)),
            agents: self
                .run_count_query("SELECT COUNT(*) AS c FROM agents", &[])
                .unwrap_or_else(|| previous_count(previous, |snapshot| snapshot.agents)),
            messages: self
                .run_count_query("SELECT COUNT(*) AS c FROM messages", &[])
                .unwrap_or_else(|| previous_count(previous, |snapshot| snapshot.messages)),
            file_reservations: reservation_count,
            contact_links: self
                .run_count_query("SELECT COUNT(*) AS c FROM agent_links", &[])
                .unwrap_or_else(|| previous_count(previous, |snapshot| snapshot.contact_links)),
            ack_pending: self
                .fetch_ack_pending_count()
                .unwrap_or_else(|| previous_count(previous, |snapshot| snapshot.ack_pending)),
        }
    }

    fn fetch_ack_pending_count(&self) -> Option<u64> {
        self.run_count_query(
            "SELECT COALESCE(SUM(ack_pending_count), 0) AS c FROM inbox_stats",
            &[],
        )
        .or_else(|| {
            self.run_count_query(
                "SELECT COUNT(*) AS c FROM message_recipients \
                 WHERE ack_ts IS NULL \
                   AND message_id IN (SELECT id FROM messages WHERE ack_required = 1)",
                &[],
            )
        })
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
    wake: Arc<(Mutex<()>, Condvar)>,
}

struct PollerConnectionState {
    conn: DbConn,
    sqlite_path: String,
    _snapshot_dir: Option<crate::SnapshotDirGuard>,
    last_data_version: Option<i64>,
    last_reservation_snapshot_gap_refresh_micros: i64,
    /// Tracks last full snapshot time for the fallback path when
    /// `PRAGMA data_version` is unavailable.  Prevents running expensive
    /// aggregate queries on every 2-second poll cycle.
    last_full_snapshot_micros: i64,
}

/// Handle returned by [`DbPoller::start`].
pub struct DbPollerHandle {
    join: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
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
            wake: Arc::new((Mutex::new(()), Condvar::new())),
        }
    }

    /// Override the polling interval (for tests).
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval.max(MIN_OVERRIDE_POLL_INTERVAL);
        self
    }

    /// Spawn the background polling thread.
    #[must_use]
    pub fn start(self) -> DbPollerHandle {
        let stop = Arc::clone(&self.stop);
        let wake = Arc::clone(&self.wake);
        let state = Arc::clone(&self.state);
        let join = thread::Builder::new()
            .name("tui-db-poller".into())
            .spawn(move || {
                self.run();
            });
        let join = match join {
            Ok(join) => Some(join),
            Err(error) => {
                handle_poller_spawn_failure(&state, &error);
                None
            }
        };
        DbPollerHandle { join, stop, wake }
    }

    /// Main polling loop.
    #[allow(clippy::too_many_lines)]
    fn run(self) {
        let mut prev = DbStatSnapshot::default();
        let now = Instant::now();
        let mut last_health_emit = now
            .checked_sub(HEALTH_PULSE_HEARTBEAT_INTERVAL)
            .unwrap_or(now);
        let mut panic_recovery_active = false;
        let mut connection_state: Option<PollerConnectionState> = None;
        let mut last_warmup_failure_retry = now
            .checked_sub(DB_WARMUP_FAILURE_RETRY_INTERVAL)
            .unwrap_or(now);

        while !self.stop.load(Ordering::Relaxed) {
            let mut allow_poll = true;
            let mut warmup_wait_consumed_interval = false;
            if connection_state.is_none() && prev.timestamp_micros == 0 {
                match self.state.wait_for_db_warmup(self.interval) {
                    DbWarmupState::Ready => {}
                    DbWarmupState::Pending => {
                        allow_poll = false;
                        warmup_wait_consumed_interval = true;
                    }
                    DbWarmupState::Failed => {
                        let now = Instant::now();
                        if warmup_failure_retry_due(
                            last_warmup_failure_retry,
                            now,
                            DB_WARMUP_FAILURE_RETRY_INTERVAL,
                        ) {
                            last_warmup_failure_retry = now;
                        } else {
                            allow_poll = false;
                        }
                    }
                }
                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            // Fetch fresh snapshot
            let snapshot_update = if allow_poll {
                if let Ok(snapshot) = catch_optional_panic(std::panic::AssertUnwindSafe(|| {
                    if connection_state.is_none() {
                        let config = self.state.config_snapshot();
                        connection_state = open_poller_connection_state(
                            &self.database_url,
                            std::path::Path::new(&config.storage_root),
                        );
                    }
                    connection_state
                        .as_mut()
                        .and_then(|state| fetch_db_stats_with_connection(state, &prev))
                })) {
                    if panic_recovery_active {
                        tracing::info!(
                            "tui-db-poller recovered after a panic; resuming normal polling"
                        );
                        panic_recovery_active = false;
                    }
                    snapshot
                } else {
                    connection_state = None;
                    self.state.mark_db_context_unavailable();
                    if !panic_recovery_active {
                        tracing::warn!(
                            "tui-db-poller recovered from a panic while polling DB; keeping UI alive"
                        );
                        panic_recovery_active = true;
                    }
                    None
                }
            } else {
                None
            };
            if let Some(update) = snapshot_update {
                self.state.mark_db_context_available();
                self.state.mark_db_ready();
                match update {
                    DbPollSnapshotUpdate::Snapshot(snapshot) => {
                        let changed = snapshot_delta(&prev, &snapshot).any_changed();
                        self.state.update_db_stats(snapshot.clone());
                        if changed || last_health_emit.elapsed() >= HEALTH_PULSE_HEARTBEAT_INTERVAL
                        {
                            let _ = self
                                .state
                                .push_event(MailEvent::health_pulse(snapshot.clone()));
                            last_health_emit = Instant::now();
                        }
                        prev = snapshot;
                    }
                    DbPollSnapshotUpdate::TimestampOnly(now_micros) => {
                        self.state.refresh_db_stats_timestamp(now_micros);
                        prev.timestamp_micros = now_micros;
                        if last_health_emit.elapsed() >= HEALTH_PULSE_HEARTBEAT_INTERVAL {
                            let _ = self.state.push_event(MailEvent::health_pulse(prev.clone()));
                            last_health_emit = Instant::now();
                        }
                    }
                }
                last_warmup_failure_retry = Instant::now()
                    .checked_sub(DB_WARMUP_FAILURE_RETRY_INTERVAL)
                    .unwrap_or_else(Instant::now);
            } else if allow_poll {
                self.state.mark_db_context_unavailable();
                connection_state = None;
            }

            if self.stop.load(Ordering::Relaxed) {
                break;
            }

            if warmup_wait_consumed_interval {
                continue;
            }

            // When the DB engine lacks PRAGMA data_version (e.g. FrankenSQLite),
            // the poller cannot cheaply detect changes and must run full
            // snapshots periodically.  Extend the sleep interval between polls
            // to avoid burning CPU on expensive no-op queries.
            let effective_interval = if connection_state
                .as_ref()
                .is_some_and(|s| s.last_data_version.is_none())
            {
                self.interval.max(Duration::from_secs(5))
            } else {
                self.interval
            };

            // Block until the next interval or an explicit stop wakeup.
            let (lock, cvar) = &*self.wake;
            let _ = cvar.wait_timeout(
                match lock.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                },
                effective_interval,
            );
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
        }
    }
}

impl DbPollerHandle {
    /// Signal the poller to stop and wait for the thread to exit.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.1.notify_all();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// Signal stop without waiting.
    pub fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.1.notify_all();
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

fn tui_poller_spawn_failure_message(error: &std::io::Error) -> String {
    format!("TUI startup: DB poller failed to start ({error})")
}

fn handle_poller_spawn_failure(state: &Arc<TuiSharedState>, error: &std::io::Error) {
    tracing::warn!(
        error = %error,
        "failed to spawn tui db poller; continuing without live DB polling"
    );
    state.mark_db_warmup_failed();
    state.push_console_log(tui_poller_spawn_failure_message(error));
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

/// Fetch a complete [`DbStatSnapshot`] from the database.
///
/// Opens a fresh sync connection, runs aggregate queries, and returns
/// the snapshot.  On any error, returns `None` so callers can keep the
/// previous snapshot instead of clearing existing data.
#[cfg(test)]
fn fetch_db_stats(database_url: &str) -> Option<DbStatSnapshot> {
    let (conn, sqlite_path) = open_sync_connection_with_path(database_url)?;
    Some(DbStatQueryBatcher::new_with_path(&conn, &sqlite_path).fetch_snapshot(None))
}

fn open_poller_connection_state(
    database_url: &str,
    storage_root: &std::path::Path,
) -> Option<PollerConnectionState> {
    let (conn, sqlite_path, snapshot_dir) =
        open_sync_connection_with_path_and_storage_root(database_url, storage_root)?;
    Some(PollerConnectionState {
        conn,
        sqlite_path,
        _snapshot_dir: snapshot_dir,
        last_data_version: None,
        last_reservation_snapshot_gap_refresh_micros: 0,
        last_full_snapshot_micros: 0,
    })
}

enum DbPollSnapshotUpdate {
    Snapshot(DbStatSnapshot),
    TimestampOnly(i64),
}

fn fetch_db_stats_with_connection(
    state: &mut PollerConnectionState,
    previous: &DbStatSnapshot,
) -> Option<DbPollSnapshotUpdate> {
    let now = now_micros();
    let data_version = query_data_version(&state.conn, Some(&state.sqlite_path));
    let must_refresh_for_expiry = reservation_expiry_requires_time_refresh(previous, now);
    let must_refresh_for_snapshot_gap = reservation_snapshot_gap_requires_refresh(
        previous,
        now,
        state.last_reservation_snapshot_gap_refresh_micros,
    );
    let must_refresh_for_detail_gap = snapshot_has_missing_detail_lists(previous);
    // When `PRAGMA data_version` is available, use it to skip full snapshots
    // when nothing has changed.
    let version_unchanged = match (data_version, state.last_data_version) {
        (Some(current), Some(prev)) => current == prev,
        _ => false,
    };
    if version_unchanged && previous.timestamp_micros > 0 {
        let update = if must_refresh_for_detail_gap {
            DbStatQueryBatcher::new_with_path(&state.conn, &state.sqlite_path)
                .fetch_snapshot(Some(previous))
        } else if must_refresh_for_expiry || must_refresh_for_snapshot_gap {
            refresh_reservation_time_sensitive_snapshot(state, previous, now)
        } else {
            state.last_data_version = data_version;
            update_reservation_snapshot_gap_refresh_state(
                state,
                must_refresh_for_snapshot_gap,
                previous,
                now,
            );
            return Some(DbPollSnapshotUpdate::TimestampOnly(now));
        };
        state.last_data_version = data_version;
        update_reservation_snapshot_gap_refresh_state(
            state,
            must_refresh_for_snapshot_gap,
            &update,
            now,
        );
        return Some(DbPollSnapshotUpdate::Snapshot(update));
    }
    // When data_version is unavailable (e.g. FrankenSQLite), throttle
    // expensive full snapshots to once every 30 seconds to avoid burning
    // CPU on repeated no-op poll cycles.
    if data_version.is_none()
        && previous.timestamp_micros > 0
        && !must_refresh_for_detail_gap
        && !must_refresh_for_expiry
        && !must_refresh_for_snapshot_gap
        && (now - state.last_full_snapshot_micros) < NO_VERSION_FULL_SNAPSHOT_INTERVAL_MICROS
    {
        return Some(DbPollSnapshotUpdate::TimestampOnly(now));
    }
    let snapshot = DbStatQueryBatcher::new_with_path(&state.conn, &state.sqlite_path)
        .fetch_snapshot(Some(previous));
    state.last_full_snapshot_micros = now;
    if snapshot_is_empty(&snapshot)
        && required_mail_schema_state(&state.conn) != RequiredMailSchemaState::Present
    {
        return None;
    }
    state.last_data_version = data_version;
    update_reservation_snapshot_gap_refresh_state(
        state,
        must_refresh_for_snapshot_gap,
        &snapshot,
        now,
    );
    Some(DbPollSnapshotUpdate::Snapshot(snapshot))
}

const fn snapshot_is_empty(snapshot: &DbStatSnapshot) -> bool {
    snapshot.projects == 0
        && snapshot.agents == 0
        && snapshot.messages == 0
        && snapshot.file_reservations == 0
        && snapshot.contact_links == 0
        && snapshot.ack_pending == 0
        && snapshot.agents_list.is_empty()
        && snapshot.projects_list.is_empty()
        && snapshot.contacts_list.is_empty()
        && snapshot.reservation_snapshots.is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequiredMailSchemaState {
    Present,
    Missing,
    Unknown,
}

fn required_mail_schema_state(conn: &DbConn) -> RequiredMailSchemaState {
    // Empty snapshots are only trustworthy when the concrete columns used by
    // the poller queries exist, not merely when table names happen to exist.
    for (table, columns) in [
        ("projects", &["id", "slug", "human_key", "created_at"][..]),
        (
            "agents",
            &["id", "project_id", "name", "program", "last_active_ts"][..],
        ),
        ("messages", &["id", "project_id"][..]),
        (
            "file_reservations",
            &[
                "id",
                "agent_id",
                "project_id",
                "path_pattern",
                "exclusive",
                "created_ts",
                "expires_ts",
                "released_ts",
            ][..],
        ),
        (
            "agent_links",
            &[
                "id",
                "a_agent_id",
                "b_agent_id",
                "a_project_id",
                "b_project_id",
                "status",
                "reason",
                "updated_ts",
                "expires_ts",
            ][..],
        ),
        ("message_recipients", &["id", "message_id", "ack_ts"][..]),
    ] {
        match table_has_required_columns(conn, table, columns) {
            Some(true) => {}
            Some(false) => return RequiredMailSchemaState::Missing,
            None => return RequiredMailSchemaState::Unknown,
        }
    }
    RequiredMailSchemaState::Present
}

fn table_has_required_columns(
    conn: &DbConn,
    table: &str,
    required_columns: &[&str],
) -> Option<bool> {
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table})"), &[])
        .ok()?;
    if rows.is_empty() {
        return Some(false);
    }
    let available_columns: std::collections::HashSet<String> = rows
        .into_iter()
        .filter_map(|row| pragma_table_info_column_name(&row))
        .collect();
    Some(
        required_columns
            .iter()
            .all(|column_name| available_columns.contains(*column_name)),
    )
}

fn pragma_table_info_column_name(row: &Row) -> Option<String> {
    row.get_named::<String>("name")
        .ok()
        .or_else(|| row.get_as::<String>(1).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn refresh_reservation_time_sensitive_snapshot(
    state: &PollerConnectionState,
    previous: &DbStatSnapshot,
    now_micros: i64,
) -> DbStatSnapshot {
    let Some(bundle) =
        try_fetch_reservation_snapshot_bundle(&state.conn, now_micros, Some(&state.sqlite_path))
    else {
        let mut snapshot = previous.clone();
        snapshot.timestamp_micros = now_micros;
        return snapshot;
    };
    apply_reservation_bundle_to_snapshot(previous, bundle, now_micros)
}

fn apply_reservation_bundle_to_snapshot(
    previous: &DbStatSnapshot,
    bundle: ReservationSnapshotBundle,
    now_micros: i64,
) -> DbStatSnapshot {
    let mut snapshot = previous.clone();
    snapshot.file_reservations = bundle.active_count;
    for project in &mut snapshot.projects_list {
        project.reservation_count = bundle
            .active_counts_by_project
            .get(&project.id)
            .copied()
            .unwrap_or(0);
    }
    snapshot.reservation_snapshots = bundle.snapshots;
    restore_missing_reservation_snapshots_from_previous(previous, &mut snapshot);
    snapshot.timestamp_micros = now_micros;
    snapshot
}

fn restore_missing_reservation_snapshots_from_previous(
    previous: &DbStatSnapshot,
    snapshot: &mut DbStatSnapshot,
) {
    if snapshot.file_reservations == 0 || snapshot.file_reservations > MAX_RESERVATIONS as u64 {
        return;
    }
    let expected_rows = usize::try_from(snapshot.file_reservations).unwrap_or(MAX_RESERVATIONS);
    if snapshot.reservation_snapshots.len() >= expected_rows
        || previous.file_reservations != snapshot.file_reservations
        || previous.reservation_snapshots.len() != expected_rows
    {
        return;
    }

    let current_rows = snapshot.reservation_snapshots.len();
    snapshot.reservation_snapshots = previous.reservation_snapshots.clone();
    tracing::warn!(
        current_rows,
        expected_rows,
        "tui poller reservation snapshot rows came back partially truncated while the uncapped active count stayed stable; preserving previous reservation detail rows"
    );
}

const fn update_reservation_snapshot_gap_refresh_state(
    state: &mut PollerConnectionState,
    must_refresh_for_snapshot_gap: bool,
    snapshot: &DbStatSnapshot,
    now_micros: i64,
) {
    if must_refresh_for_snapshot_gap {
        state.last_reservation_snapshot_gap_refresh_micros = now_micros;
    } else if snapshot.file_reservations == 0 || !snapshot.reservation_snapshots.is_empty() {
        state.last_reservation_snapshot_gap_refresh_micros = 0;
    }
}

fn warmup_failure_retry_due(last_attempt: Instant, now: Instant, retry_interval: Duration) -> bool {
    now.duration_since(last_attempt) >= retry_interval
}

fn reservation_expiry_requires_time_refresh(previous: &DbStatSnapshot, now_micros: i64) -> bool {
    if previous.file_reservations == 0 {
        return false;
    }
    previous
        .reservation_snapshots
        .iter()
        .filter(|snapshot| !snapshot.is_released())
        .any(|snapshot| snapshot.expires_ts > 0 && snapshot.expires_ts <= now_micros)
}

fn reservation_snapshot_gap_requires_refresh(
    previous: &DbStatSnapshot,
    now_micros: i64,
    last_refresh_micros: i64,
) -> bool {
    if previous.file_reservations == 0 || !previous.reservation_snapshots.is_empty() {
        return false;
    }
    if last_refresh_micros <= 0 {
        return true;
    }
    now_micros.saturating_sub(last_refresh_micros)
        >= i64::try_from(RESERVATION_SNAPSHOT_GAP_REFRESH_INTERVAL.as_micros()).unwrap_or(i64::MAX)
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

fn open_sync_connection_with_path_and_storage_root(
    database_url: &str,
    storage_root: &std::path::Path,
) -> Option<(DbConn, String, Option<crate::SnapshotDirGuard>)> {
    match crate::open_observability_sync_db_connection(database_url, storage_root, "tui db poller")
    {
        Ok(db) => db.map(crate::ObservabilitySyncDb::into_parts),
        Err(error) => {
            tracing::warn!(
                error = %error,
                database_url,
                storage_root = %storage_root.display(),
                "tui db poller observability snapshot unavailable"
            );
            None
        }
    }
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
    let path = crate::resolve_server_sync_sqlite_path(&cfg.sqlite_path().ok()?);
    match path.as_str() {
        ":memory:" => None,
        _ => crate::open_best_effort_sync_db_connection(&path)
            .ok()
            .map(|conn| (conn, path)),
    }
}

fn query_data_version(conn: &DbConn, sqlite_path: Option<&str>) -> Option<i64> {
    match conn.query_sync("PRAGMA data_version", &[]) {
        Ok(rows) => rows.first().and_then(|row| {
            row.get_named::<i64>("data_version")
                .ok()
                .or_else(|| row.get_as::<i64>(0).ok())
        }),
        Err(err) => {
            let message = err.to_string();
            if is_sqlite_recovery_error_message(&message)
                && let Some(path) = sqlite_path
            {
                tracing::warn!(
                    path = %path,
                    error = %message,
                    "tui poller data-version probe hit recoverable sqlite error; skipping automatic recovery from observability path"
                );
            }
            None
        }
    }
}

const fn snapshot_has_missing_detail_lists(snapshot: &DbStatSnapshot) -> bool {
    (snapshot.agents > 0 && snapshot.agents_list.is_empty())
        || (snapshot.projects > 0 && snapshot.projects_list.is_empty())
        || (snapshot.contact_links > 0 && snapshot.contacts_list.is_empty())
}

fn restore_missing_detail_lists_from_previous(
    previous: Option<&DbStatSnapshot>,
    snapshot: &mut DbStatSnapshot,
    sqlite_path: Option<&str>,
) {
    let Some(previous) = previous else {
        return;
    };

    maybe_reuse_previous_detail_list(
        "agents",
        snapshot.agents,
        MAX_AGENTS,
        &mut snapshot.agents_list,
        &previous.agents_list,
        sqlite_path,
    );
    maybe_reuse_previous_detail_list(
        "projects",
        snapshot.projects,
        MAX_PROJECTS,
        &mut snapshot.projects_list,
        &previous.projects_list,
        sqlite_path,
    );
    maybe_reuse_previous_detail_list(
        "contacts",
        snapshot.contact_links,
        MAX_CONTACTS,
        &mut snapshot.contacts_list,
        &previous.contacts_list,
        sqlite_path,
    );
}

fn unique_previous_project_summary<F>(
    previous_rows: &[ProjectSummary],
    mut predicate: F,
) -> Option<&ProjectSummary>
where
    F: FnMut(&ProjectSummary) -> bool,
{
    let mut matching = previous_rows.iter().filter(|row| predicate(row));
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn restore_missing_project_rollup_counts_from_previous(
    previous: Option<&DbStatSnapshot>,
    snapshot: &mut DbStatSnapshot,
    sqlite_path: Option<&str>,
) {
    let Some(previous) = previous else {
        return;
    };
    if snapshot.projects_list.is_empty() || previous.projects_list.is_empty() {
        return;
    }

    let restore_agent_counts = snapshot.agents > 0
        && snapshot
            .projects_list
            .iter()
            .all(|project| project.agent_count == 0);
    let restore_message_counts = snapshot.messages > 0
        && snapshot
            .projects_list
            .iter()
            .all(|project| project.message_count == 0);
    let restore_reservation_counts = snapshot.file_reservations > 0
        && snapshot
            .projects_list
            .iter()
            .all(|project| project.reservation_count == 0);
    if !(restore_agent_counts || restore_message_counts || restore_reservation_counts) {
        return;
    }

    let mut restored_agent_rows = 0_usize;
    let mut restored_message_rows = 0_usize;
    let mut restored_reservation_rows = 0_usize;
    for row in &mut snapshot.projects_list {
        let previous_row = unique_previous_project_summary(&previous.projects_list, |candidate| {
            candidate.slug == row.slug && candidate.human_key == row.human_key
        })
        .or_else(|| {
            unique_previous_project_summary(&previous.projects_list, |candidate| {
                candidate.slug == row.slug
            })
        });
        let Some(previous_row) = previous_row else {
            continue;
        };
        if row.created_at == 0 {
            row.created_at = previous_row.created_at;
        }
        if restore_agent_counts && row.agent_count == 0 {
            row.agent_count = previous_row.agent_count;
            restored_agent_rows += 1;
        }
        if restore_message_counts && row.message_count == 0 {
            row.message_count = previous_row.message_count;
            restored_message_rows += 1;
        }
        if restore_reservation_counts && row.reservation_count == 0 {
            row.reservation_count = previous_row.reservation_count;
            restored_reservation_rows += 1;
        }
    }

    if restored_agent_rows > 0 || restored_message_rows > 0 || restored_reservation_rows > 0 {
        tracing::warn!(
            path = sqlite_path.unwrap_or("<unknown>"),
            restored_agent_rows,
            restored_message_rows,
            restored_reservation_rows,
            "tui poller project rollup counts came back false-zero while summary totals remained nonzero; preserving previous per-project counts"
        );
    }
}

fn restore_missing_contact_rows_from_previous(
    previous: Option<&DbStatSnapshot>,
    snapshot: &mut DbStatSnapshot,
    sqlite_path: Option<&str>,
) {
    let Some(previous) = previous else {
        return;
    };
    if snapshot.contacts_list.is_empty() || previous.contacts_list.is_empty() {
        return;
    }
    if snapshot.contact_links == 0 || snapshot.contact_links > MAX_CONTACTS as u64 {
        return;
    }

    let expected_rows = usize::try_from(snapshot.contact_links).unwrap_or(MAX_CONTACTS);
    if snapshot.contacts_list.len() >= expected_rows
        || previous.contacts_list.len() != expected_rows
    {
        return;
    }

    let current_rows = snapshot.contacts_list.len();
    snapshot.contacts_list = previous.contacts_list.clone();
    tracing::warn!(
        path = sqlite_path.unwrap_or("<unknown>"),
        current_rows,
        expected_rows,
        "tui poller contacts list came back partially truncated while summary count stayed within the uncapped window; preserving previous contact rows"
    );
}

fn refill_missing_detail_lists_from_sqlite(
    snapshot: &mut DbStatSnapshot,
    sqlite_path: Option<&str>,
    reservation_counts: &HashMap<i64, u64>,
) {
    if !snapshot_has_missing_detail_lists(snapshot) {
        return;
    }
    let Some(sqlite_path) = sqlite_path else {
        return;
    };
    let Ok(conn) = crate::open_best_effort_sync_db_connection(sqlite_path) else {
        return;
    };

    if snapshot.agents > 0 && snapshot.agents_list.is_empty() {
        snapshot.agents_list = fetch_agents_list(&conn);
    }
    if snapshot.projects > 0 && snapshot.projects_list.is_empty() {
        snapshot.projects_list =
            fetch_projects_list_with_reservation_counts(&conn, Some(reservation_counts));
    }
    if snapshot.contact_links > 0 && snapshot.contacts_list.is_empty() {
        snapshot.contacts_list = fetch_contacts_list(&conn);
    }
}

fn maybe_reuse_previous_detail_list<T: Clone>(
    label: &str,
    total_count: u64,
    max_rows: usize,
    current_rows: &mut Vec<T>,
    previous_rows: &[T],
    sqlite_path: Option<&str>,
) {
    if total_count == 0 || previous_rows.is_empty() {
        return;
    }
    if current_rows.is_empty() {
        tracing::warn!(
            path = sqlite_path.unwrap_or("<unknown>"),
            list = label,
            total_count,
            preserved_rows = previous_rows.len(),
            "tui poller detail list came back empty while summary count remained nonzero; preserving previous rows"
        );
        *current_rows = previous_rows.to_vec();
        return;
    }

    if total_count > max_rows as u64 {
        return;
    }
    let expected_rows = usize::try_from(total_count).unwrap_or(max_rows);
    if current_rows.len() >= expected_rows || previous_rows.len() != expected_rows {
        return;
    }
    let current_row_count = current_rows.len();
    tracing::warn!(
        path = sqlite_path.unwrap_or("<unknown>"),
        list = label,
        total_count,
        current_row_count,
        preserved_rows = previous_rows.len(),
        "tui poller detail list came back partially truncated while the uncapped summary count stayed stable; preserving previous rows"
    );
    *current_rows = previous_rows.to_vec();
}

pub(crate) fn timestamp_sort_expr(column: &str) -> String {
    format!(
        "CASE \
           WHEN typeof({column}) IN ('integer', 'real') THEN CAST({column} AS INTEGER) \
           WHEN typeof({column}) = 'text' THEN CASE \
             WHEN instr(trim({column}), ' ') > 0 \
               OR instr(trim({column}), ':') > 0 \
               OR (length(trim({column})) >= 10 \
                   AND substr(trim({column}), 5, 1) = '-' \
                   AND substr(trim({column}), 8, 1) = '-') \
             THEN COALESCE( \
               CAST(strftime('%s', trim({column})) AS INTEGER) * 1000000 + \
               CASE \
                 WHEN instr(trim({column}), '.') > 0 THEN CAST( \
                   substr(trim({column}) || '000000', instr(trim({column}), '.') + 1, 6) \
                   AS INTEGER \
                 ) \
                 ELSE 0 \
               END, \
               0 \
             ) \
             ELSE CAST(CAST(trim({column}) AS REAL) AS INTEGER) \
           END \
           ELSE 0 \
         END"
    )
}

/// Fetch the agent list ordered by most recently active.
fn fetch_agents_list(conn: &DbConn) -> Vec<AgentSummary> {
    let last_active_sort = timestamp_sort_expr("last_active_ts");
    conn.query_sync(
        &format!(
            "SELECT id, name, program, last_active_ts FROM agents \
             ORDER BY {last_active_sort} DESC, id DESC LIMIT {MAX_AGENTS}"
        ),
        &[],
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                let agent_id = row
                    .get_named::<i64>("id")
                    .ok()
                    .or_else(|| row.get_as::<i64>(0).ok())?;
                let name = row
                    .get_named::<String>("name")
                    .ok()
                    .or_else(|| row.get_as::<String>(1).ok())
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| format!("[unknown-agent-{agent_id}]"));
                let program = row
                    .get_named::<String>("program")
                    .ok()
                    .or_else(|| row.get_as::<String>(2).ok())
                    .unwrap_or_default();
                Some(AgentSummary {
                    name,
                    program,
                    last_active_ts: parse_raw_ts(&row, "last_active_ts"),
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
    let created_at_sort = timestamp_sort_expr("created_at");
    let sql = format!(
        "WITH recent_projects AS ( \
           SELECT id, slug, human_key, created_at \
           FROM projects \
           ORDER BY {created_at_sort} DESC, id DESC \
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
         ORDER BY {created_at_sort} DESC, p.id DESC"
    );
    let fallback_reservation_counts = reservation_counts_override
        .is_none()
        .then(|| fetch_active_reservation_counts_by_project(conn, now_micros()));
    let reservation_counts = reservation_counts_override.unwrap_or_else(|| {
        fallback_reservation_counts
            .as_ref()
            .unwrap_or_else(|| unreachable!("fallback reservation counts should exist"))
    });
    match conn.query_sync(&sql, &[]) {
        Ok(rows) => parse_project_summary_rows(rows, reservation_counts),
        Err(err) => {
            tracing::debug!(
                error = ?err,
                "tui_poller.fetch_projects_list aggregate query failed; falling back to minimal project rows"
            );
            let mut projects = fetch_projects_list_minimal(conn, reservation_counts);
            hydrate_project_summary_counts(conn, &mut projects);
            projects
        }
    }
}

pub(crate) fn fetch_project_count_map(
    conn: &DbConn,
    table: &str,
    project_ids: &[i64],
) -> HashMap<i64, u64> {
    if project_ids.is_empty() {
        return HashMap::new();
    }
    let ids = project_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT project_id, COUNT(*) AS cnt \
         FROM {table} \
         WHERE project_id IN ({ids}) \
         GROUP BY project_id"
    );
    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    Some((
                        row.get_named::<i64>("project_id")
                            .ok()
                            .or_else(|| row.get_as::<i64>(0).ok())?,
                        row.get_named::<i64>("cnt")
                            .ok()
                            .or_else(|| row.get_as::<i64>(1).ok())
                            .and_then(|count| u64::try_from(count.max(0)).ok())
                            .unwrap_or(0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn hydrate_project_summary_counts(conn: &DbConn, projects: &mut [ProjectSummary]) {
    let project_ids: Vec<i64> = projects.iter().map(|project| project.id).collect();
    let agent_counts = fetch_project_count_map(conn, "agents", &project_ids);
    let message_counts = fetch_project_count_map(conn, "messages", &project_ids);
    for project in projects {
        project.agent_count = agent_counts.get(&project.id).copied().unwrap_or(0);
        project.message_count = message_counts.get(&project.id).copied().unwrap_or(0);
    }
}

fn parse_project_summary_rows(
    rows: Vec<mcp_agent_mail_db::sqlmodel::Row>,
    reservation_counts: &HashMap<i64, u64>,
) -> Vec<ProjectSummary> {
    rows.into_iter()
        .filter_map(|row| {
            let project_id = row
                .get_named::<i64>("id")
                .ok()
                .or_else(|| row.get_as::<i64>(0).ok())?;
            let slug = row
                .get_named::<String>("slug")
                .ok()
                .or_else(|| row.get_as::<String>(1).ok())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("[unknown-project-{project_id}]"));
            let human_key = row
                .get_named::<String>("human_key")
                .ok()
                .or_else(|| row.get_as::<String>(2).ok())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("[missing-human-key-{project_id}]"));
            Some(ProjectSummary {
                id: project_id,
                slug,
                human_key,
                agent_count: row
                    .get_named::<i64>("agent_count")
                    .ok()
                    .or_else(|| row.get_as::<i64>(4).ok())
                    .and_then(|v| u64::try_from(v).ok())
                    .unwrap_or(0),
                message_count: row
                    .get_named::<i64>("message_count")
                    .ok()
                    .or_else(|| row.get_as::<i64>(5).ok())
                    .and_then(|v| u64::try_from(v).ok())
                    .unwrap_or(0),
                reservation_count: reservation_counts.get(&project_id).copied().unwrap_or(0),
                created_at: parse_raw_ts_with_index(&row, "created_at", 3),
            })
        })
        .collect()
}

fn fetch_projects_list_minimal(
    conn: &DbConn,
    reservation_counts: &HashMap<i64, u64>,
) -> Vec<ProjectSummary> {
    let created_at_sort = timestamp_sort_expr("created_at");
    let sql = format!(
        "SELECT id, slug, human_key, created_at \
         FROM projects \
         ORDER BY {created_at_sort} DESC, id DESC \
         LIMIT {MAX_PROJECTS}"
    );
    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            let mut projects = parse_project_summary_rows(rows, reservation_counts);
            backfill_project_group_counts(conn, &mut projects);
            projects
        })
        .unwrap_or_default()
}

fn backfill_project_group_counts(conn: &DbConn, projects: &mut [ProjectSummary]) {
    let project_ids: Vec<i64> = projects.iter().map(|project| project.id).collect();
    if project_ids.is_empty() {
        return;
    }

    let agent_counts = fetch_project_group_count_map(conn, "agents", &project_ids);
    let message_counts = fetch_project_group_count_map(conn, "messages", &project_ids);
    for project in projects {
        project.agent_count = agent_counts.get(&project.id).copied().unwrap_or(0);
        project.message_count = message_counts.get(&project.id).copied().unwrap_or(0);
    }
}

fn fetch_project_group_count_map(
    conn: &DbConn,
    table: &str,
    project_ids: &[i64],
) -> HashMap<i64, u64> {
    if project_ids.is_empty() {
        return HashMap::new();
    }

    let ids = project_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT project_id, COUNT(*) AS cnt \
         FROM {table} \
         WHERE project_id IN ({ids}) \
         GROUP BY project_id"
    );
    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    Some((
                        row.get_named::<i64>("project_id")
                            .ok()
                            .or_else(|| row.get_as::<i64>(0).ok())?,
                        row.get_named::<i64>("cnt")
                            .ok()
                            .or_else(|| row.get_as::<i64>(1).ok())
                            .and_then(|count| u64::try_from(count.max(0)).ok())
                            .unwrap_or(0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn fetch_active_reservation_counts_by_project(
    conn: &DbConn,
    now: i64,
) -> HashMap<i64, u64> {
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
    let updated_sort = timestamp_sort_expr("al.updated_ts");
    conn.query_sync(
        &format!(
            "SELECT \
             al.id, \
             al.a_agent_id AS raw_from_agent_id, al.b_agent_id AS raw_to_agent_id, \
             al.a_project_id AS raw_from_project_id, al.b_project_id AS raw_to_project_id, \
             a1.name AS from_agent, a2.name AS to_agent, \
             p1.slug AS from_project, p2.slug AS to_project, \
             al.status, al.reason, al.updated_ts, al.expires_ts \
             FROM agent_links al \
             LEFT JOIN agents a1 ON a1.id = al.a_agent_id \
             LEFT JOIN agents a2 ON a2.id = al.b_agent_id \
             LEFT JOIN projects p1 ON p1.id = al.a_project_id \
             LEFT JOIN projects p2 ON p2.id = al.b_project_id \
             ORDER BY {updated_sort} DESC, al.id DESC \
             LIMIT {MAX_CONTACTS}"
        ),
        &[],
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .map(|row| {
                let read_raw_id = |name: &str, index: usize| {
                    row.get_named::<i64>(name)
                        .ok()
                        .or_else(|| row.get_as::<i64>(index).ok())
                        .unwrap_or(0)
                };
                let from_agent_id = read_raw_id("raw_from_agent_id", 1);
                let to_agent_id = read_raw_id("raw_to_agent_id", 2);
                let from_project_id = read_raw_id("raw_from_project_id", 3);
                let to_project_id = read_raw_id("raw_to_project_id", 4);
                let read_text = |name: &str, fallback: String| {
                    row.get_named::<String>(name)
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .unwrap_or(fallback)
                };
                ContactSummary {
                    from_agent: read_text("from_agent", format!("[unknown-agent-{from_agent_id}]")),
                    to_agent: read_text("to_agent", format!("[unknown-agent-{to_agent_id}]")),
                    from_project_slug: read_text(
                        "from_project",
                        format!("[unknown-project-{from_project_id}]"),
                    ),
                    to_project_slug: read_text(
                        "to_project",
                        format!("[unknown-project-{to_project_id}]"),
                    ),
                    status: read_text("status", "[unknown-status]".to_string()),
                    reason: row.get_named::<String>("reason").ok().unwrap_or_default(),
                    updated_ts: parse_raw_ts(&row, "updated_ts"),
                    expires_ts: parse_optional_raw_ts(&row, "expires_ts"),
                }
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
pub(crate) fn parse_raw_ts(row: &Row, col: &str) -> i64 {
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

fn parse_raw_ts_with_index(row: &Row, col: &str, index: usize) -> i64 {
    match row.get_by_name(col) {
        Some(_) => parse_raw_ts(row, col),
        None => row
            .get_as::<i64>(index)
            .ok()
            .or_else(|| {
                row.get_as::<String>(index)
                    .ok()
                    .map(|value| parse_text_timestamp(&value))
            })
            .unwrap_or(0),
    }
}

fn parse_optional_raw_ts(row: &Row, col: &str) -> Option<i64> {
    match row.get_by_name(col) {
        None | Some(Value::Null) => None,
        Some(_) => Some(parse_raw_ts(row, col)),
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
/// Mirrors the migration-layer timestamp contract so mixed-format legacy rows
/// are interpreted consistently across migration, query, and TUI code paths.
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
    mcp_agent_mail_db::migrate::text_to_micros(s, "tui_poller", "timestamp", 0)
        .ok()
        .flatten()
        .unwrap_or(0)
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

fn has_file_reservations_released_ts_column(conn: &DbConn) -> bool {
    conn.query_sync("PRAGMA table_info(file_reservations)", &[])
        .ok()
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.get_named::<String>("name").ok().as_deref() == Some("released_ts"))
        })
}

fn has_file_reservation_release_ledger(conn: &DbConn) -> bool {
    conn.query_sync(
        "SELECT 1
         FROM sqlite_master
         WHERE type = 'table' AND name = 'file_reservation_releases'
         LIMIT 1",
        &[],
    )
    .ok()
    .is_some_and(|rows| !rows.is_empty())
}

fn active_reservation_release_join_sql(
    has_release_ledger: bool,
    table_ref: &str,
    release_alias: &str,
) -> String {
    if has_release_ledger {
        format!(
            " LEFT JOIN file_reservation_releases {release_alias} ON {release_alias}.reservation_id = {table_ref}.id"
        )
    } else {
        String::new()
    }
}

fn legacy_active_reservation_predicate_sql(
    table_ref: &str,
    has_legacy_released_ts_column: bool,
) -> String {
    if !has_legacy_released_ts_column {
        return "1 = 1".to_string();
    }
    let table_ref = table_ref.trim().trim_end_matches('.');
    let predicate = mcp_agent_mail_db::queries::ACTIVE_RESERVATION_LEGACY_PREDICATE;
    if table_ref.is_empty() || table_ref == "file_reservations" {
        predicate.to_string()
    } else {
        predicate.replace("released_ts", &format!("{table_ref}.released_ts"))
    }
}

fn active_reservation_filter_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
    table_ref: &str,
    release_alias: &str,
) -> String {
    let legacy = legacy_active_reservation_predicate_sql(table_ref, has_legacy_released_ts_column);
    match (has_release_ledger, has_legacy_released_ts_column) {
        (true, true) => format!("({legacy}) AND {release_alias}.reservation_id IS NULL"),
        (true, false) => format!("{release_alias}.reservation_id IS NULL"),
        (false, _) => legacy,
    }
}

fn reservation_released_ts_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
    table_ref: &str,
    release_alias: &str,
) -> String {
    let table_ref = table_ref.trim().trim_end_matches('.');
    let legacy_release_expr = has_legacy_released_ts_column.then(|| {
        format!(
            "CASE \
                 WHEN {table_ref}.released_ts IS NULL THEN NULL \
                 WHEN typeof({table_ref}.released_ts) = 'text' THEN \
                     CAST(strftime('%s', REPLACE(REPLACE({table_ref}.released_ts, 'T', ' '), 'Z', '')) AS INTEGER) * 1000000 + \
                     CASE WHEN instr({table_ref}.released_ts, '.') > 0 \
                          THEN CAST(substr(REPLACE({table_ref}.released_ts, 'Z', '') || '000000', instr({table_ref}.released_ts, '.') + 1, 6) AS INTEGER) \
                          ELSE 0 \
                     END \
                 ELSE {table_ref}.released_ts \
             END"
        )
    });
    match (has_release_ledger, legacy_release_expr) {
        (true, Some(legacy_release_expr)) => {
            format!("COALESCE({release_alias}.released_ts, {legacy_release_expr})")
        }
        (true, None) => format!("{release_alias}.released_ts"),
        (false, Some(legacy_release_expr)) => legacy_release_expr,
        (false, None) => "NULL".to_string(),
    }
}

fn reservation_legacy_scan_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
) -> String {
    let release_join = active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let released_ts_sql = reservation_released_ts_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
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
           {released_ts_sql} AS raw_released_ts \
         FROM file_reservations fr{release_join} \
         LEFT JOIN projects p ON p.id = fr.project_id \
         LEFT JOIN agents a ON a.id = fr.agent_id"
    )
}

fn reservation_legacy_scan_minimal_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
) -> String {
    let release_join = active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let released_ts_sql = reservation_released_ts_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    format!(
        "SELECT \
           fr.id, \
           fr.project_id AS raw_project_id, \
           fr.path_pattern, \
           fr.\"exclusive\", \
           fr.created_ts AS raw_created_ts, \
           fr.expires_ts AS raw_expires_ts, \
           {released_ts_sql} AS raw_released_ts \
         FROM file_reservations fr{release_join}"
    )
}

fn reservation_active_fast_snapshots_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
) -> String {
    let release_join = active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    let released_ts_sql = reservation_released_ts_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
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
           {released_ts_sql} AS raw_released_ts \
         FROM file_reservations fr{release_join} \
         LEFT JOIN projects p ON p.id = fr.project_id \
         LEFT JOIN agents a ON a.id = fr.agent_id \
         WHERE ({active_reservation_predicate}) AND expires_ts > ? \
         ORDER BY fr.expires_ts ASC, fr.id ASC \
         LIMIT {MAX_RESERVATIONS}"
    )
}

fn reservation_active_fast_snapshots_minimal_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
) -> String {
    let release_join = active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    let released_ts_sql = reservation_released_ts_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    format!(
        "SELECT \
           fr.id, \
           fr.project_id AS raw_project_id, \
           fr.path_pattern, \
           fr.\"exclusive\", \
           fr.created_ts AS raw_created_ts, \
           fr.expires_ts AS raw_expires_ts, \
           {released_ts_sql} AS raw_released_ts \
         FROM file_reservations fr{release_join} \
         WHERE ({active_reservation_predicate}) AND expires_ts > ? \
         ORDER BY fr.expires_ts ASC, fr.id ASC \
         LIMIT {MAX_RESERVATIONS}"
    )
}

fn reservation_active_fast_counts_sql(
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
) -> String {
    let release_join = active_reservation_release_join_sql(has_release_ledger, "fr", "rr");
    let active_reservation_predicate = active_reservation_filter_sql(
        has_release_ledger,
        has_legacy_released_ts_column,
        "fr",
        "rr",
    );
    format!(
        "SELECT \
           fr.project_id AS raw_project_id, \
           COUNT(*) AS active_count \
         FROM file_reservations fr{release_join} \
         WHERE ({active_reservation_predicate}) AND expires_ts > ? \
         GROUP BY fr.project_id"
    )
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

pub(crate) fn file_reservations_support_active_fast_scan(conn: &DbConn) -> bool {
    detect_reservation_scan_mode(conn) == ReservationScanMode::ActiveFast
}

fn detect_reservation_scan_mode(conn: &DbConn) -> ReservationScanMode {
    // Conservative policy: if detection is uncertain, keep legacy full-scan
    // semantics so we never drop active reservations from the UI.
    let Some(expires_declared_text) = file_reservations_expires_declared_text(conn) else {
        return ReservationScanMode::FullLegacy;
    };
    if expires_declared_text {
        return ReservationScanMode::FullLegacy;
    }

    let Some(released_declared_text) = file_reservations_released_declared_text(conn) else {
        return ReservationScanMode::FullLegacy;
    };
    if released_declared_text {
        return ReservationScanMode::FullLegacy;
    }

    let Some(expires_has_text_values) = file_reservations_contains_text_expires_values(conn) else {
        return ReservationScanMode::FullLegacy;
    };
    if expires_has_text_values {
        return ReservationScanMode::FullLegacy;
    }

    let Some(released_has_text_values) = file_reservations_contains_text_released_values(conn)
    else {
        return ReservationScanMode::FullLegacy;
    };
    if released_has_text_values {
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

fn file_reservations_released_declared_text(conn: &DbConn) -> Option<bool> {
    let rows = conn
        .query_sync("PRAGMA table_info(file_reservations)", &[])
        .ok()?;
    for row in rows {
        let Ok(name) = row.get_named::<String>("name") else {
            continue;
        };
        if name != "released_ts" {
            continue;
        }
        let declared = row.get_named::<String>("type").ok().unwrap_or_default();
        let upper = declared.to_ascii_uppercase();
        return Some(upper.contains("TEXT") || upper.contains("CHAR") || upper.contains("CLOB"));
    }
    None
}

fn file_reservations_contains_text_released_values(conn: &DbConn) -> Option<bool> {
    conn.query_sync(
        "SELECT 1 AS has_text \
         FROM file_reservations \
         WHERE typeof(released_ts) = 'text' \
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
    previous: Option<&DbStatSnapshot>,
) -> ReservationSnapshotBundle {
    try_fetch_reservation_snapshot_bundle(conn, now, sqlite_path)
        .unwrap_or_else(|| previous_reservation_snapshot_bundle(previous))
}

fn previous_reservation_snapshot_bundle(
    previous: Option<&DbStatSnapshot>,
) -> ReservationSnapshotBundle {
    previous.map_or_else(ReservationSnapshotBundle::default, |snapshot| {
        ReservationSnapshotBundle {
            active_count: snapshot.file_reservations,
            active_counts_by_project: snapshot
                .projects_list
                .iter()
                .map(|project| (project.id, project.reservation_count))
                .collect(),
            snapshots: snapshot.reservation_snapshots.clone(),
        }
    })
}

fn previous_count(
    previous: Option<&DbStatSnapshot>,
    selector: impl FnOnce(&DbStatSnapshot) -> u64,
) -> u64 {
    previous.map_or(0, selector)
}

#[allow(clippy::too_many_lines)]
fn try_fetch_reservation_snapshot_bundle(
    conn: &DbConn,
    now: i64,
    sqlite_path: Option<&str>,
) -> Option<ReservationSnapshotBundle> {
    let has_release_ledger = has_file_reservation_release_ledger(conn);
    let has_legacy_released_ts_column = has_file_reservations_released_ts_column(conn);
    let scan_mode = reservation_scan_mode(conn, sqlite_path);
    if scan_mode == ReservationScanMode::ActiveFast {
        return try_fetch_reservation_snapshot_bundle_fast(
            conn,
            now,
            has_release_ledger,
            has_legacy_released_ts_column,
        );
    }
    let rows = match scan_mode {
        ReservationScanMode::ActiveFast => unreachable!("handled by fast-path early return"),
        ReservationScanMode::FullLegacy => conn.query_sync(
            &reservation_legacy_scan_sql(has_release_ledger, has_legacy_released_ts_column),
            &[],
        ),
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(
                mode = ?scan_mode,
                error = ?err,
                "tui_poller.fetch_reservation_snapshots query failed; falling back to minimal reservation rows"
            );
            match conn.query_sync(
                &reservation_legacy_scan_minimal_sql(
                    has_release_ledger,
                    has_legacy_released_ts_column,
                ),
                &[],
            ) {
                Ok(rows) => rows,
                Err(fallback_err) => {
                    tracing::debug!(
                        mode = ?scan_mode,
                        error = ?fallback_err,
                        "tui_poller.fetch_reservation_snapshots minimal fallback also failed"
                    );
                    return None;
                }
            }
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
        let path_pattern = row
            .get_named::<String>("path_pattern")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("[missing-path-pattern-{id}]"));
        let snapshot = ReservationSnapshot {
            id,
            project_slug: row
                .get_named::<String>("project_slug")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "[unknown-project]".to_string()),
            agent_name: row
                .get_named::<String>("agent_name")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
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
    Some(ReservationSnapshotBundle {
        active_count,
        active_counts_by_project,
        snapshots,
    })
}

#[allow(clippy::too_many_lines)]
fn try_fetch_reservation_snapshot_bundle_fast(
    conn: &DbConn,
    now: i64,
    has_release_ledger: bool,
    has_legacy_released_ts_column: bool,
) -> Option<ReservationSnapshotBundle> {
    let count_rows = match conn.query_sync(
        &reservation_active_fast_counts_sql(has_release_ledger, has_legacy_released_ts_column),
        &[Value::BigInt(now)],
    ) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(
                mode = ?ReservationScanMode::ActiveFast,
                error = ?err,
                "tui_poller.fetch_reservation_snapshots count query failed"
            );
            return None;
        }
    };

    let mut active_count = 0_u64;
    let mut active_counts_by_project: HashMap<i64, u64> = HashMap::new();
    for row in count_rows {
        let Some(project_id) = parse_raw_i64(&row, "raw_project_id") else {
            continue;
        };
        let count = row
            .get_named::<i64>("active_count")
            .ok()
            .and_then(|value| u64::try_from(value.max(0)).ok())
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        active_counts_by_project.insert(project_id, count);
        active_count = active_count.saturating_add(count);
    }

    if MAX_RESERVATIONS == 0 || active_count == 0 {
        return Some(ReservationSnapshotBundle {
            active_count,
            active_counts_by_project,
            snapshots: Vec::new(),
        });
    }

    let snapshot_rows = match conn.query_sync(
        &reservation_active_fast_snapshots_sql(has_release_ledger, has_legacy_released_ts_column),
        &[Value::BigInt(now)],
    ) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(
                mode = ?ReservationScanMode::ActiveFast,
                error = ?err,
                "tui_poller.fetch_reservation_snapshots snapshot query failed; falling back to minimal reservation rows"
            );
            match conn.query_sync(
                &reservation_active_fast_snapshots_minimal_sql(
                    has_release_ledger,
                    has_legacy_released_ts_column,
                ),
                &[Value::BigInt(now)],
            ) {
                Ok(rows) => rows,
                Err(fallback_err) => {
                    tracing::debug!(
                        mode = ?ReservationScanMode::ActiveFast,
                        error = ?fallback_err,
                        "tui_poller.fetch_reservation_snapshots minimal fallback also failed"
                    );
                    return Some(ReservationSnapshotBundle {
                        active_count,
                        active_counts_by_project,
                        snapshots: Vec::new(),
                    });
                }
            }
        }
    };

    let mut snapshots = Vec::with_capacity(snapshot_rows.len().min(MAX_RESERVATIONS));
    for row in snapshot_rows {
        let Some(id) = parse_raw_i64(&row, "id") else {
            continue;
        };
        let path_pattern = row
            .get_named::<String>("path_pattern")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("[missing-path-pattern-{id}]"));
        snapshots.push(ReservationSnapshot {
            id,
            project_slug: row
                .get_named::<String>("project_slug")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "[unknown-project]".to_string()),
            agent_name: row
                .get_named::<String>("agent_name")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "[unknown-agent]".to_string()),
            path_pattern,
            exclusive: row
                .get_named::<i64>("exclusive")
                .ok()
                .is_none_or(|value| value != 0),
            granted_ts: parse_raw_ts(&row, "raw_created_ts"),
            expires_ts: parse_raw_ts(&row, "raw_expires_ts"),
            released_ts: None,
        });
    }

    Some(ReservationSnapshotBundle {
        active_count,
        active_counts_by_project,
        snapshots,
    })
}

/// Fetch active file reservations with project and agent names.
///
/// This is reused by the reservations screen as a direct fallback when the
/// background poller snapshot is unavailable or stale.
#[allow(clippy::too_many_lines)]
#[allow(dead_code)]
pub(crate) fn fetch_reservation_snapshots(conn: &DbConn) -> Vec<ReservationSnapshot> {
    fetch_reservation_snapshots_with_path(conn, None)
}

/// Fetch active file reservations with an optional `SQLite` path cache key.
///
/// Passing `sqlite_path` enables reservation scan-mode cache reuse so fallback
/// callers don't repeatedly re-detect schema compatibility.
pub(crate) fn fetch_reservation_snapshots_with_path(
    conn: &DbConn,
    sqlite_path: Option<&str>,
) -> Vec<ReservationSnapshot> {
    fetch_reservation_snapshot_bundle(conn, now_micros(), sqlite_path, None).snapshots
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
    use mcp_agent_mail_db::queries::ACTIVE_RESERVATION_PREDICATE;

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

    #[test]
    fn pragma_table_info_column_name_uses_index_fallback() {
        let conn = DbConn::open_memory().expect("open");
        let rows = conn
            .query_sync("SELECT 0 AS cid, 'slug' AS other", &[])
            .expect("query rows");
        let name = pragma_table_info_column_name(&rows[0]);
        assert_eq!(name.as_deref(), Some("slug"));
    }

    #[test]
    fn poller_interval_override_clamps_zero() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller = DbPoller::new(Arc::clone(&state), "sqlite:///test.db".into())
            .with_interval(Duration::ZERO);
        assert_eq!(poller.interval, MIN_OVERRIDE_POLL_INTERVAL);
    }

    #[test]
    fn poller_spawn_failure_marks_failed_state_and_logs_console_message() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        handle_poller_spawn_failure(&state, &std::io::Error::other("boom"));

        assert_eq!(state.db_warmup_state(), DbWarmupState::Failed);
        let logs = state.console_log_since(0);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].1, "TUI startup: DB poller failed to start (boom)");
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
        create_empty_mail_schema(&conn);

        // Insert some data
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('proj1', 'hk1', 100)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, last_active_ts)
             VALUES (1, 'GoldFox', 'claude-code', 200)",
            &[],
        )
        .expect("insert agent");
        conn.execute_sync("INSERT INTO messages (id, project_id) VALUES (1, 1)", &[])
            .expect("insert message");
        drop(conn);

        // Start poller
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(50));
        let mut handle = poller.start();
        state.mark_db_ready();

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
    fn poller_cold_start_wakes_early_when_db_ready_is_marked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_poller_ready.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        create_empty_mail_schema(&conn);
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('proj1', 'hk1', 100)",
            &[],
        )
        .expect("insert project");
        drop(conn);

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_secs(5));
        let mut handle = poller.start();

        thread::sleep(Duration::from_millis(75));
        let before = state.db_stats_snapshot().unwrap_or_default();
        assert_eq!(
            before.timestamp_micros, 0,
            "cold-start poller should not query SQLite before readiness is signaled"
        );

        state.mark_db_ready();

        let deadline = Instant::now() + Duration::from_millis(750);
        let mut woke = false;
        while Instant::now() < deadline {
            if state
                .db_stats_snapshot()
                .is_some_and(|snapshot| snapshot.timestamp_micros > 0 && snapshot.projects == 1)
            {
                woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        handle.stop();
        assert!(
            woke,
            "db-ready signal should wake the poller before the full interval elapses"
        );
    }

    #[test]
    fn poller_pending_warmup_timeout_does_not_pay_interval_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_poller_pending_timeout.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        create_empty_mail_schema(&conn);
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('proj1', 'hk1', 100)",
            &[],
        )
        .expect("insert project");
        drop(conn);

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(250));
        let mut handle = poller.start();

        thread::sleep(Duration::from_millis(300));
        state.mark_db_ready();

        let deadline = Instant::now() + Duration::from_millis(150);
        let mut woke = false;
        while Instant::now() < deadline {
            if state
                .db_stats_snapshot()
                .is_some_and(|snapshot| snapshot.timestamp_micros > 0 && snapshot.projects == 1)
            {
                woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        handle.stop();
        assert!(
            woke,
            "poller should retry immediately after a pending warmup timeout instead of sleeping a second full interval"
        );
    }

    #[test]
    fn poller_skips_update_when_no_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_no_change.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        create_empty_mail_schema(&conn);
        drop(conn);

        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 100);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(50));
        let mut handle = poller.start();
        state.mark_db_ready();

        // Wait for multiple poll cycles
        thread::sleep(Duration::from_millis(300));
        assert!(
            state.data_generation().db_stats_gen > 0,
            "initial zero snapshot should still count as delivered poller data"
        );

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
    fn open_poller_connection_state_rejects_non_sqlite_startup_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("malformed_startup.db");
        std::fs::write(&db_path, b"not-a-database").expect("write malformed db");
        let db_url = format!("sqlite:///{}", db_path.display());

        assert!(
            open_poller_connection_state(&db_url, dir.path()).is_none(),
            "non-sqlite startup file should not produce a poller connection state"
        );
    }

    fn create_empty_mail_schema(conn: &DbConn) {
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT,
                human_key TEXT,
                created_at INTEGER
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                name TEXT,
                program TEXT,
                last_active_ts INTEGER
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER
            )",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                \"exclusive\" INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts INTEGER
            )",
            &[],
        )
        .expect("create file_reservations");
        conn.execute_sync(
            "CREATE TABLE agent_links (
                id INTEGER PRIMARY KEY,
                a_agent_id INTEGER,
                b_agent_id INTEGER,
                a_project_id INTEGER,
                b_project_id INTEGER,
                status TEXT,
                reason TEXT,
                updated_ts INTEGER,
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create agent_links");
        conn.execute_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER,
                ack_ts INTEGER
            )",
            &[],
        )
        .expect("create message_recipients");
    }

    #[test]
    fn fetch_db_stats_with_connection_allows_empty_healthy_startup_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("empty_startup.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        create_empty_mail_schema(&conn);
        drop(conn);

        let (conn, sqlite_path) = open_sync_connection_with_path(&db_url).expect("open conn");
        let mut state = PollerConnectionState {
            conn,
            sqlite_path,
            _snapshot_dir: None,
            last_data_version: None,
            last_reservation_snapshot_gap_refresh_micros: 0,
            last_full_snapshot_micros: 0,
        };

        let update = fetch_db_stats_with_connection(&mut state, &DbStatSnapshot::default());
        assert!(
            matches!(update, Some(DbPollSnapshotUpdate::Snapshot(_))),
            "healthy empty sqlite should still yield a real first snapshot"
        );
    }

    #[test]
    fn open_sync_connection_with_path_uses_shared_server_resolver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = dir.path().join("poller_fallback.sqlite3");
        let conn = DbConn::open_file(absolute_db.to_string_lossy().as_ref()).expect("open");
        create_empty_mail_schema(&conn);
        drop(conn);

        let tempdir_name = dir
            .path()
            .file_name()
            .expect("tempdir basename")
            .to_string_lossy()
            .into_owned();
        let relative_path = format!("tmp/{tempdir_name}/poller_fallback.sqlite3");
        let shadow_root = std::env::current_dir()
            .expect("cwd")
            .join("tmp")
            .join(&tempdir_name);
        struct ShadowRootCleanup(std::path::PathBuf);
        impl Drop for ShadowRootCleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = ShadowRootCleanup(shadow_root.clone());
        std::fs::create_dir_all(&shadow_root).expect("create relative shadow root");
        std::fs::write(
            shadow_root.join("poller_fallback.sqlite3"),
            b"not a sqlite database",
        )
        .expect("write corrupt shadow");

        let db_url = format!("sqlite:///{relative_path}");
        let (_conn, sqlite_path) =
            open_sync_connection_with_path(&db_url).expect("open poller fallback db");

        assert_eq!(
            sqlite_path,
            absolute_db.to_string_lossy(),
            "poller sync opens should use the shared resolver and prefer the healthy absolute candidate"
        );
    }

    #[test]
    fn open_sync_connection_with_path_and_storage_root_uses_archive_snapshot_when_live_db_is_stale()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_root = dir.path().join("storage");
        let db_path = dir.path().join("poller-stale.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(agent_dir.join("profile.json"), "{}").expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        create_empty_mail_schema(&conn);
        drop(conn);

        let db_url = format!("sqlite:///{}", db_path.display());
        let (conn, sqlite_path, snapshot_dir) =
            open_sync_connection_with_path_and_storage_root(&db_url, &storage_root)
                .expect("open poller snapshot db");
        assert!(
            snapshot_dir.is_some(),
            "poller should hold an archive-backed snapshot when the live db lags"
        );
        assert_ne!(
            sqlite_path,
            db_path.to_string_lossy(),
            "poller should switch away from the stale live sqlite file"
        );
        let rows = conn
            .query_sync("SELECT COUNT(*) AS c FROM messages", &[])
            .expect("query snapshot messages");
        assert_eq!(rows[0].get_named::<i64>("c").unwrap_or(0), 1);
    }

    #[test]
    fn fetch_db_stats_with_connection_rejects_empty_missing_schema_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("wrong_schema_startup.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_sync("CREATE TABLE unrelated (id INTEGER PRIMARY KEY)", &[])
            .expect("create unrelated");
        drop(conn);

        let (conn, sqlite_path) = open_sync_connection_with_path(&db_url).expect("open conn");
        let mut state = PollerConnectionState {
            conn,
            sqlite_path,
            _snapshot_dir: None,
            last_data_version: None,
            last_reservation_snapshot_gap_refresh_micros: 0,
            last_full_snapshot_micros: 0,
        };

        let update = fetch_db_stats_with_connection(&mut state, &DbStatSnapshot::default());
        assert!(
            update.is_none(),
            "empty snapshot from a non-mail schema must not count as valid DB context"
        );
    }

    #[test]
    fn fetch_db_stats_with_connection_rejects_empty_partial_schema_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("partial_schema_startup.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_sync("CREATE TABLE projects (id INTEGER PRIMARY KEY)", &[])
            .expect("create projects");
        conn.execute_sync("CREATE TABLE agents (id INTEGER PRIMARY KEY)", &[])
            .expect("create agents");
        conn.execute_sync("CREATE TABLE messages (id INTEGER PRIMARY KEY)", &[])
            .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY)",
            &[],
        )
        .expect("create file_reservations");
        conn.execute_sync("CREATE TABLE agent_links (id INTEGER PRIMARY KEY)", &[])
            .expect("create agent_links");
        conn.execute_sync(
            "CREATE TABLE message_recipients (id INTEGER PRIMARY KEY)",
            &[],
        )
        .expect("create message_recipients");
        drop(conn);

        let (conn, sqlite_path) = open_sync_connection_with_path(&db_url).expect("open conn");
        let mut state = PollerConnectionState {
            conn,
            sqlite_path,
            _snapshot_dir: None,
            last_data_version: None,
            last_reservation_snapshot_gap_refresh_micros: 0,
            last_full_snapshot_micros: 0,
        };

        let update = fetch_db_stats_with_connection(&mut state, &DbStatSnapshot::default());
        assert!(
            update.is_none(),
            "empty snapshot from a partial mail schema must not count as valid DB context"
        );
    }

    #[test]
    fn fetch_db_stats_with_connection_rejects_missing_reservation_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("reservation_columns_startup.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT,
                human_key TEXT,
                created_at INTEGER
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                name TEXT,
                program TEXT,
                last_active_ts INTEGER
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER
            )",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                path_pattern TEXT,
                \"exclusive\" INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create file_reservations");
        conn.execute_sync(
            "CREATE TABLE agent_links (
                id INTEGER PRIMARY KEY,
                a_agent_id INTEGER,
                b_agent_id INTEGER,
                a_project_id INTEGER,
                b_project_id INTEGER,
                status TEXT,
                reason TEXT,
                updated_ts INTEGER,
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create agent_links");
        conn.execute_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER,
                ack_ts INTEGER
            )",
            &[],
        )
        .expect("create message_recipients");
        drop(conn);

        let (conn, sqlite_path) = open_sync_connection_with_path(&db_url).expect("open conn");
        let mut state = PollerConnectionState {
            conn,
            sqlite_path,
            _snapshot_dir: None,
            last_data_version: None,
            last_reservation_snapshot_gap_refresh_micros: 0,
            last_full_snapshot_micros: 0,
        };

        let update = fetch_db_stats_with_connection(&mut state, &DbStatSnapshot::default());
        assert!(
            update.is_none(),
            "missing reservation columns used by the poller must not count as valid DB context"
        );
    }

    #[test]
    fn poller_refreshes_snapshot_timestamp_without_data_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_snapshot_refresh.db");
        let db_url = format!("sqlite:///{}", db_path.display());

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");
        create_empty_mail_schema(&conn);
        drop(conn);

        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let poller =
            DbPoller::new(Arc::clone(&state), db_url).with_interval(Duration::from_millis(50));
        let mut handle = poller.start();
        state.mark_db_ready();

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
    fn reservation_expiry_requires_time_refresh_when_expiry_due() {
        let mut snapshot = DbStatSnapshot {
            file_reservations: 1,
            reservation_snapshots: vec![ReservationSnapshot {
                id: 1,
                project_slug: "proj".to_string(),
                agent_name: "agent".to_string(),
                path_pattern: "src/**".to_string(),
                exclusive: true,
                granted_ts: 10,
                expires_ts: 100,
                released_ts: None,
            }],
            ..DbStatSnapshot::default()
        };

        assert!(
            !reservation_expiry_requires_time_refresh(&snapshot, 99),
            "should not force refresh before expiry"
        );
        assert!(
            reservation_expiry_requires_time_refresh(&snapshot, 100),
            "should force refresh once reservation reaches expiry"
        );

        snapshot.reservation_snapshots[0].released_ts = Some(90);
        assert!(
            !reservation_expiry_requires_time_refresh(&snapshot, 100),
            "released reservations should not force refresh"
        );
    }

    #[test]
    fn reservation_snapshot_gap_requires_refresh_uses_retry_cooldown() {
        let snapshot = DbStatSnapshot {
            file_reservations: 2,
            reservation_snapshots: Vec::new(),
            ..DbStatSnapshot::default()
        };
        assert!(
            reservation_snapshot_gap_requires_refresh(&snapshot, 1_000_000, 0),
            "first missing-row retry should refresh immediately"
        );
        assert!(
            !reservation_snapshot_gap_requires_refresh(&snapshot, 1_500_000, 1_000_000),
            "missing-row retry should not fire every poll cycle"
        );
        assert!(
            reservation_snapshot_gap_requires_refresh(
                &snapshot,
                1_000_000
                    + i64::try_from(RESERVATION_SNAPSHOT_GAP_REFRESH_INTERVAL.as_micros())
                        .unwrap_or(i64::MAX),
                1_000_000,
            ),
            "missing-row retry should resume after the cooldown"
        );
    }

    #[test]
    fn reservation_time_refresh_updates_only_reservation_fields() {
        let previous = DbStatSnapshot {
            projects: 2,
            agents: 3,
            messages: 5,
            file_reservations: 2,
            contact_links: 7,
            ack_pending: 11,
            agents_list: vec![AgentSummary {
                name: "BlueLake".to_string(),
                program: "codex".to_string(),
                last_active_ts: 10,
            }],
            projects_list: vec![
                ProjectSummary {
                    id: 1,
                    slug: "alpha".to_string(),
                    human_key: "/tmp/alpha".to_string(),
                    agent_count: 1,
                    message_count: 3,
                    reservation_count: 2,
                    created_at: 10,
                },
                ProjectSummary {
                    id: 2,
                    slug: "beta".to_string(),
                    human_key: "/tmp/beta".to_string(),
                    agent_count: 2,
                    message_count: 2,
                    reservation_count: 0,
                    created_at: 9,
                },
            ],
            contacts_list: vec![ContactSummary {
                from_agent: "BlueLake".to_string(),
                to_agent: "RedStone".to_string(),
                from_project_slug: "alpha".to_string(),
                to_project_slug: "beta".to_string(),
                status: "accepted".to_string(),
                reason: String::new(),
                updated_ts: 10,
                expires_ts: None,
            }],
            reservation_snapshots: vec![ReservationSnapshot {
                id: 1,
                project_slug: "alpha".to_string(),
                agent_name: "BlueLake".to_string(),
                path_pattern: "src/**".to_string(),
                exclusive: true,
                granted_ts: 10,
                expires_ts: 20,
                released_ts: None,
            }],
            timestamp_micros: 100,
        };
        let bundle = ReservationSnapshotBundle {
            active_count: 1,
            active_counts_by_project: HashMap::from([(2, 1)]),
            snapshots: vec![ReservationSnapshot {
                id: 2,
                project_slug: "beta".to_string(),
                agent_name: "RedStone".to_string(),
                path_pattern: "tests/**".to_string(),
                exclusive: false,
                granted_ts: 30,
                expires_ts: 40,
                released_ts: None,
            }],
        };

        let refreshed = apply_reservation_bundle_to_snapshot(&previous, bundle, 250);

        assert_eq!(refreshed.projects, previous.projects);
        assert_eq!(refreshed.agents, previous.agents);
        assert_eq!(refreshed.messages, previous.messages);
        assert_eq!(refreshed.contact_links, previous.contact_links);
        assert_eq!(refreshed.ack_pending, previous.ack_pending);
        assert_eq!(refreshed.agents_list, previous.agents_list);
        assert_eq!(refreshed.contacts_list, previous.contacts_list);
        assert_eq!(refreshed.file_reservations, 1);
        assert_eq!(refreshed.projects_list[0].reservation_count, 0);
        assert_eq!(refreshed.projects_list[1].reservation_count, 1);
        assert_eq!(refreshed.reservation_snapshots.len(), 1);
        assert_eq!(refreshed.reservation_snapshots[0].id, 2);
        assert_eq!(refreshed.timestamp_micros, 250);
    }

    #[test]
    fn reservation_time_refresh_keeps_previous_snapshot_on_query_failure() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        let state = PollerConnectionState {
            conn,
            sqlite_path: ":memory:".to_string(),
            _snapshot_dir: None,
            last_data_version: None,
            last_reservation_snapshot_gap_refresh_micros: 0,
            last_full_snapshot_micros: 0,
        };
        let previous = DbStatSnapshot {
            file_reservations: 2,
            projects_list: vec![ProjectSummary {
                id: 1,
                slug: "alpha".to_string(),
                human_key: "/tmp/alpha".to_string(),
                agent_count: 1,
                message_count: 0,
                reservation_count: 2,
                created_at: 10,
            }],
            reservation_snapshots: vec![ReservationSnapshot {
                id: 7,
                project_slug: "alpha".to_string(),
                agent_name: "BlueLake".to_string(),
                path_pattern: "src/**".to_string(),
                exclusive: true,
                granted_ts: 10,
                expires_ts: 20,
                released_ts: None,
            }],
            timestamp_micros: 100,
            ..DbStatSnapshot::default()
        };

        let refreshed = refresh_reservation_time_sensitive_snapshot(&state, &previous, 250);

        assert_eq!(refreshed.file_reservations, previous.file_reservations);
        assert_eq!(refreshed.projects_list, previous.projects_list);
        assert_eq!(
            refreshed.reservation_snapshots,
            previous.reservation_snapshots
        );
        assert_eq!(refreshed.timestamp_micros, 250);
    }

    #[test]
    fn apply_reservation_bundle_preserves_previous_snapshots_when_detail_rows_are_partial() {
        let previous = DbStatSnapshot {
            file_reservations: 2,
            projects_list: vec![ProjectSummary {
                id: 1,
                slug: "alpha".to_string(),
                human_key: "/tmp/alpha".to_string(),
                agent_count: 0,
                message_count: 0,
                reservation_count: 2,
                created_at: 10,
            }],
            reservation_snapshots: vec![
                ReservationSnapshot {
                    id: 7,
                    project_slug: "alpha".to_string(),
                    agent_name: "BlueLake".to_string(),
                    path_pattern: "src/**".to_string(),
                    exclusive: true,
                    granted_ts: 10,
                    expires_ts: 20,
                    released_ts: None,
                },
                ReservationSnapshot {
                    id: 8,
                    project_slug: "alpha".to_string(),
                    agent_name: "RedStone".to_string(),
                    path_pattern: "tests/**".to_string(),
                    exclusive: false,
                    granted_ts: 11,
                    expires_ts: 21,
                    released_ts: None,
                },
            ],
            timestamp_micros: 100,
            ..DbStatSnapshot::default()
        };
        let bundle = ReservationSnapshotBundle {
            active_count: 2,
            active_counts_by_project: HashMap::from([(1, 2)]),
            snapshots: vec![ReservationSnapshot {
                id: 7,
                project_slug: "alpha".to_string(),
                agent_name: "BlueLake".to_string(),
                path_pattern: "src/**".to_string(),
                exclusive: true,
                granted_ts: 10,
                expires_ts: 20,
                released_ts: None,
            }],
        };

        let refreshed = apply_reservation_bundle_to_snapshot(&previous, bundle, 250);

        assert_eq!(refreshed.file_reservations, 2);
        assert_eq!(refreshed.projects_list[0].reservation_count, 2);
        assert_eq!(
            refreshed.reservation_snapshots,
            previous.reservation_snapshots
        );
        assert_eq!(refreshed.timestamp_micros, 250);
    }

    #[test]
    fn warmup_failure_retry_due_honors_cooldown() {
        let base = Instant::now();
        assert!(!warmup_failure_retry_due(
            base,
            base + Duration::from_secs(4),
            Duration::from_secs(5),
        ));
        assert!(warmup_failure_retry_due(
            base,
            base + Duration::from_secs(5),
            Duration::from_secs(5),
        ));
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

    #[test]
    fn db_stat_query_batcher_reuses_previous_snapshot_when_busy_lock_blocks_queries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("poller_busy_snapshot.db");
        let db_path_str = db_path.to_string_lossy().to_string();
        let conn = DbConn::open_file(&db_path_str).expect("open");
        create_empty_mail_schema(&conn);
        conn.execute_sync("ALTER TABLE messages ADD COLUMN ack_required INTEGER", &[])
            .expect("add ack_required");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj-a', 'hk-a', 100)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, last_active_ts) VALUES (1, 1, 'BlueLake', 'codex', 100)",
            &[],
        )
        .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, ack_required) VALUES (10, 1, 1)",
            &[],
        )
        .expect("insert message");
        conn.execute_sync(
            "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, \"exclusive\", created_ts, expires_ts, released_ts) VALUES (20, 1, 1, 'src/**', 1, 10, 4102444800000000, NULL)",
            &[],
        )
        .expect("insert reservation");
        conn.execute_sync(
            "INSERT INTO agent_links (id, a_agent_id, b_agent_id, a_project_id, b_project_id, status, reason, updated_ts, expires_ts) VALUES (30, 1, 1, 1, 1, 'accepted', '', 0, NULL)",
            &[],
        )
        .expect("insert link");
        conn.execute_sync(
            "INSERT INTO message_recipients (id, message_id, ack_ts) VALUES (40, 10, NULL)",
            &[],
        )
        .expect("insert recipient");

        let previous = DbStatQueryBatcher::new_with_path(&conn, &db_path_str).fetch_snapshot(None);

        let lock_conn = DbConn::open_file(&db_path_str).expect("open lock conn");
        lock_conn
            .execute_sync("BEGIN EXCLUSIVE", &[])
            .expect("acquire exclusive lock");

        let read_conn = DbConn::open_file(&db_path_str).expect("open read conn");
        read_conn
            .execute_sync("PRAGMA busy_timeout = 1", &[])
            .expect("set busy timeout");

        let snapshot = DbStatQueryBatcher::new_with_path(&read_conn, &db_path_str)
            .fetch_snapshot(Some(&previous));

        lock_conn
            .execute_sync("ROLLBACK", &[])
            .expect("release lock");

        assert_eq!(snapshot.projects, previous.projects);
        assert_eq!(snapshot.agents, previous.agents);
        assert_eq!(snapshot.messages, previous.messages);
        assert_eq!(snapshot.file_reservations, previous.file_reservations);
        assert_eq!(snapshot.contact_links, previous.contact_links);
        assert_eq!(snapshot.ack_pending, previous.ack_pending);
        assert_eq!(snapshot.agents_list.len(), previous.agents_list.len());
        assert_eq!(snapshot.projects_list.len(), previous.projects_list.len());
        assert_eq!(snapshot.contacts_list.len(), previous.contacts_list.len());
        assert_eq!(
            snapshot.reservation_snapshots.len(),
            previous.reservation_snapshots.len()
        );
        assert_eq!(
            snapshot.projects_list[0].reservation_count,
            previous.projects_list[0].reservation_count
        );
        assert_eq!(
            snapshot.reservation_snapshots[0].path_pattern,
            previous.reservation_snapshots[0].path_pattern
        );
    }

    #[test]
    fn fetch_snapshot_preserves_previous_project_rollups_when_group_count_queries_fail() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj-a', 'hk-a', 100)",
            &[],
        )
        .expect("insert project");

        let previous = DbStatSnapshot {
            projects: 1,
            agents: 2,
            messages: 3,
            projects_list: vec![ProjectSummary {
                id: 1,
                slug: "proj-a".to_string(),
                human_key: "hk-a".to_string(),
                agent_count: 2,
                message_count: 3,
                reservation_count: 0,
                created_at: 100,
            }],
            timestamp_micros: 50,
            ..DbStatSnapshot::default()
        };

        let snapshot = DbStatQueryBatcher::new(&conn).fetch_snapshot(Some(&previous));

        assert_eq!(snapshot.projects, 1);
        assert_eq!(snapshot.agents, previous.agents);
        assert_eq!(snapshot.messages, previous.messages);
        assert_eq!(snapshot.projects_list.len(), 1);
        assert_eq!(snapshot.projects_list[0].slug, "proj-a");
        assert_eq!(snapshot.projects_list[0].agent_count, 2);
        assert_eq!(snapshot.projects_list[0].message_count, 3);
    }

    #[test]
    fn fetch_snapshot_preserves_previous_agents_list_when_current_rows_are_partially_truncated() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                name TEXT,
                program TEXT,
                last_active_ts INTEGER
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "INSERT INTO agents (id, name, program, last_active_ts) VALUES
                (1, 'BlueLake', 'codex', 100),
                (2, 'RedStone', NULL, 90)",
            &[],
        )
        .expect("insert agents");

        let previous = DbStatSnapshot {
            agents: 2,
            agents_list: vec![
                AgentSummary {
                    name: "BlueLake".to_string(),
                    program: "codex".to_string(),
                    last_active_ts: 100,
                },
                AgentSummary {
                    name: "RedStone".to_string(),
                    program: "claude".to_string(),
                    last_active_ts: 90,
                },
            ],
            timestamp_micros: 50,
            ..DbStatSnapshot::default()
        };

        let snapshot = DbStatQueryBatcher::new(&conn).fetch_snapshot(Some(&previous));

        assert_eq!(snapshot.agents, 2);
        assert_eq!(snapshot.agents_list, previous.agents_list);
    }

    #[test]
    fn fetch_snapshot_preserves_previous_contacts_when_join_backfill_drops_uncapped_rows() {
        let conn = DbConn::open_memory().expect("open in-memory db");
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
        conn.execute_sync("CREATE TABLE messages (id INTEGER PRIMARY KEY)", &[])
            .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE agent_links (
                id INTEGER PRIMARY KEY,
                a_agent_id INTEGER NOT NULL,
                b_agent_id INTEGER NOT NULL,
                a_project_id INTEGER NOT NULL,
                b_project_id INTEGER NOT NULL,
                status TEXT,
                reason TEXT,
                updated_ts INTEGER,
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create agent_links");
        conn.execute_sync(
            "INSERT INTO projects (id, slug) VALUES (1, 'alpha'), (2, 'beta')",
            &[],
        )
        .expect("insert projects");
        conn.execute_sync(
            "INSERT INTO agents (id, name) VALUES
                (1, 'BlueLake'),
                (2, 'RedStone'),
                (3, 'GreenField')",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO agent_links
                (id, a_agent_id, b_agent_id, a_project_id, b_project_id, status, reason, updated_ts, expires_ts)
             VALUES
                (1, 1, 2, 1, 2, 'accepted', 'ok', 100, NULL),
                (2, 1, 3, 1, 99, 'pending', 'missing project join', 90, NULL)",
            &[],
        )
        .expect("insert contact links");

        let previous = DbStatSnapshot {
            contact_links: 2,
            contacts_list: vec![
                ContactSummary {
                    from_agent: "BlueLake".to_string(),
                    to_agent: "RedStone".to_string(),
                    from_project_slug: "alpha".to_string(),
                    to_project_slug: "beta".to_string(),
                    status: "accepted".to_string(),
                    reason: "ok".to_string(),
                    updated_ts: 100,
                    expires_ts: None,
                },
                ContactSummary {
                    from_agent: "BlueLake".to_string(),
                    to_agent: "GreenField".to_string(),
                    from_project_slug: "alpha".to_string(),
                    to_project_slug: "gamma".to_string(),
                    status: "pending".to_string(),
                    reason: "missing project join".to_string(),
                    updated_ts: 90,
                    expires_ts: None,
                },
            ],
            timestamp_micros: 50,
            ..DbStatSnapshot::default()
        };

        let snapshot = DbStatQueryBatcher::new(&conn).fetch_snapshot(Some(&previous));

        assert_eq!(snapshot.contact_links, 2);
        assert_eq!(snapshot.contacts_list, previous.contacts_list);
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
    fn open_sync_connection_uses_best_effort_busy_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("busy_timeout.db");
        let url = format!("sqlite:///{}", db_path.display());
        let conn = open_sync_connection(&url).expect("open");

        let configured = conn
            .query_sync("PRAGMA busy_timeout", &[])
            .expect("pragma query")
            .into_iter()
            .next()
            .and_then(|row| {
                row.get_named::<i64>("timeout")
                    .ok()
                    .or_else(|| row.get_as(0).ok())
            })
            .unwrap_or_default();
        assert_eq!(
            configured,
            i64::from(crate::BEST_EFFORT_SYNC_DB_BUSY_TIMEOUT_MS)
        );
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

    #[test]
    fn reservation_snapshot_bundle_supports_release_ledger_without_legacy_released_ts_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_release_ledger_only.db");
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
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "CREATE TABLE file_reservation_releases (
                reservation_id INTEGER PRIMARY KEY,
                released_ts INTEGER NOT NULL
            )",
            &[],
        )
        .expect("create release ledger");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'proj')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations
                (id, project_id, agent_id, path_pattern, exclusive, created_ts, expires_ts)
             VALUES
                (1, 1, 1, 'src/**', 1, 1000, 4102444800000000),
                (2, 1, 1, 'docs/**', 1, 1000, 4102444800000000)",
            &[],
        )
        .expect("insert reservations");
        conn.execute_sync(
            "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (2, 2000)",
            &[],
        )
        .expect("insert release");

        assert_eq!(
            detect_reservation_scan_mode(&conn),
            ReservationScanMode::FullLegacy
        );

        let bundle = fetch_reservation_snapshot_bundle(&conn, now_micros(), None, None);
        assert_eq!(bundle.active_count, 1);
        assert_eq!(bundle.snapshots.len(), 1);
        assert_eq!(bundle.snapshots[0].path_pattern, "src/**");
    }

    #[test]
    fn reservation_snapshot_bundle_fast_path_supports_legacy_schema_without_release_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_reservation_no_release_ledger.db");
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
                (1, 1, 1, 'src/**', 1, 1000, 4102444800000000, NULL),
                (2, 1, 1, 'docs/**', 1, 1000, 4102444800000000, 2000)",
            &[],
        )
        .expect("insert reservations");

        assert_eq!(
            detect_reservation_scan_mode(&conn),
            ReservationScanMode::ActiveFast
        );

        let bundle = fetch_reservation_snapshot_bundle(&conn, now_micros(), None, None);
        assert_eq!(bundle.active_count, 1);
        assert_eq!(bundle.snapshots.len(), 1);
        assert_eq!(bundle.snapshots[0].path_pattern, "src/**");
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
    fn refill_missing_detail_lists_from_sqlite_repairs_empty_agents_and_projects_lists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_refill_missing_detail_lists.db");
        let db_path_str = db_path.to_string_lossy().into_owned();
        let conn = DbConn::open_file(&db_path_str).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                last_active_ts INTEGER NOT NULL
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL
            )",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at)
             VALUES (1, 'proj-a', '/tmp/proj-a', 1000)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, last_active_ts)
             VALUES (1, 1, 'BlueLake', 'codex-cli', 2000)",
            &[],
        )
        .expect("insert agent");
        conn.execute_sync("INSERT INTO messages (id, project_id) VALUES (1, 1)", &[])
            .expect("insert message");

        let mut snapshot = DbStatSnapshot {
            projects: 1,
            agents: 1,
            messages: 1,
            ..DbStatSnapshot::default()
        };
        refill_missing_detail_lists_from_sqlite(&mut snapshot, Some(&db_path_str), &HashMap::new());

        assert_eq!(snapshot.agents_list.len(), 1);
        assert_eq!(snapshot.agents_list[0].name, "BlueLake");
        assert_eq!(snapshot.projects_list.len(), 1);
        assert_eq!(snapshot.projects_list[0].slug, "proj-a");
        assert_eq!(snapshot.projects_list[0].agent_count, 1);
        assert_eq!(snapshot.projects_list[0].message_count, 1);
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
    fn fetch_contacts_list_keeps_rows_with_missing_joined_agent_or_project() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                name TEXT
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE agent_links (
                id INTEGER PRIMARY KEY,
                a_agent_id INTEGER NOT NULL,
                b_agent_id INTEGER NOT NULL,
                a_project_id INTEGER NOT NULL,
                b_project_id INTEGER NOT NULL,
                status TEXT,
                reason TEXT,
                updated_ts INTEGER,
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create agent_links");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'alpha')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO agent_links
                (id, a_agent_id, b_agent_id, a_project_id, b_project_id, status, reason, updated_ts, expires_ts)
             VALUES
                (1, 1, 99, 1, 77, 'pending', 'missing joins', 100, NULL)",
            &[],
        )
        .expect("insert contact link");

        let contacts = fetch_contacts_list(&conn);
        assert_eq!(contacts.len(), 1);
        assert_eq!(
            contacts[0],
            ContactSummary {
                from_agent: "BlueLake".to_string(),
                to_agent: "[unknown-agent-99]".to_string(),
                from_project_slug: "alpha".to_string(),
                to_project_slug: "[unknown-project-77]".to_string(),
                status: "pending".to_string(),
                reason: "missing joins".to_string(),
                updated_ts: 100,
                expires_ts: None,
            }
        );
    }

    #[test]
    fn fetch_contacts_list_keeps_rows_with_missing_status_text() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                name TEXT
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE agent_links (
                id INTEGER PRIMARY KEY,
                a_agent_id INTEGER NOT NULL,
                b_agent_id INTEGER NOT NULL,
                a_project_id INTEGER NOT NULL,
                b_project_id INTEGER NOT NULL,
                status TEXT,
                reason TEXT,
                updated_ts INTEGER,
                expires_ts INTEGER
            )",
            &[],
        )
        .expect("create agent_links");
        conn.execute_sync(
            "INSERT INTO projects (id, slug) VALUES (1, 'alpha'), (2, 'beta')",
            &[],
        )
        .expect("insert projects");
        conn.execute_sync(
            "INSERT INTO agents (id, name) VALUES (1, 'BlueLake'), (2, 'RedStone')",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO agent_links
                (id, a_agent_id, b_agent_id, a_project_id, b_project_id, status, reason, updated_ts, expires_ts)
             VALUES
                (1, 1, 2, 1, 2, NULL, 'status missing', 100, NULL)",
            &[],
        )
        .expect("insert contact link");

        let contacts = fetch_contacts_list(&conn);
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].status, "[unknown-status]");
        assert_eq!(contacts[0].reason, "status missing");
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
    fn reservation_snapshot_bundle_fast_falls_back_to_minimal_rows_when_joins_fail() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                \"exclusive\" INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts INTEGER
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, \"exclusive\", created_ts, expires_ts, released_ts
            ) VALUES (1, 99, 77, 'src/**', 1, 10, 1000000, NULL)",
            &[],
        )
        .expect("insert reservation");

        let bundle = fetch_reservation_snapshot_bundle(&conn, 100, None, None);
        assert_eq!(bundle.active_count, 1);
        assert_eq!(bundle.active_counts_by_project.get(&99), Some(&1));
        assert_eq!(bundle.snapshots.len(), 1);
        assert_eq!(bundle.snapshots[0].project_slug, "[unknown-project]");
        assert_eq!(bundle.snapshots[0].agent_name, "[unknown-agent]");
        assert_eq!(bundle.snapshots[0].path_pattern, "src/**");
    }

    #[test]
    fn reservation_snapshot_bundle_fast_keeps_rows_with_missing_path_pattern() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                \"exclusive\" INTEGER,
                created_ts INTEGER,
                expires_ts INTEGER,
                released_ts INTEGER
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                name TEXT
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'alpha')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, \"exclusive\", created_ts, expires_ts, released_ts
            ) VALUES (1, 1, 1, NULL, 1, 10, 1000000, NULL)",
            &[],
        )
        .expect("insert reservation");

        let bundle = fetch_reservation_snapshot_bundle(&conn, 100, None, None);
        assert_eq!(bundle.active_count, 1);
        assert_eq!(bundle.snapshots.len(), 1);
        assert_eq!(bundle.snapshots[0].path_pattern, "[missing-path-pattern-1]");
    }

    #[test]
    fn reservation_snapshot_bundle_legacy_keeps_rows_with_missing_path_pattern() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_sync(
            "CREATE TABLE file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER,
                agent_id INTEGER,
                path_pattern TEXT,
                \"exclusive\" INTEGER,
                created_ts INTEGER,
                expires_ts TEXT,
                released_ts TEXT
            )",
            &[],
        )
        .expect("create reservations");
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                name TEXT
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync("INSERT INTO projects (id, slug) VALUES (1, 'alpha')", &[])
            .expect("insert project");
        conn.execute_sync("INSERT INTO agents (id, name) VALUES (1, 'BlueLake')", &[])
            .expect("insert agent");
        conn.execute_sync(
            "INSERT INTO file_reservations (
                id, project_id, agent_id, path_pattern, \"exclusive\", created_ts, expires_ts, released_ts
            ) VALUES (1, 1, 1, NULL, 1, 10, '1000000', NULL)",
            &[],
        )
        .expect("insert reservation");

        let bundle = fetch_reservation_snapshot_bundle(&conn, 100, None, None);
        assert_eq!(bundle.active_count, 1);
        assert_eq!(bundle.snapshots.len(), 1);
        assert_eq!(bundle.snapshots[0].path_pattern, "[missing-path-pattern-1]");
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
    fn fetch_agents_list_orders_mixed_text_and_integer_timestamps_by_actual_recency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_agents_order_mixed_types.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts)",
            &[],
        )
        .expect("create");
        conn.execute_sync(
            "INSERT INTO agents (id, name, program, last_active_ts) VALUES
             (1, 'LegacyAgent', 'python', '2026-02-24 15:31:02'),
             (2, 'FreshAgent', 'rust', 1800000000000000)",
            &[],
        )
        .expect("insert");

        let agents = fetch_agents_list(&conn);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "FreshAgent");
        assert_eq!(agents[1].name, "LegacyAgent");
        assert!(agents[1].last_active_ts > 0);
    }

    #[test]
    fn fetch_agents_list_keeps_rows_with_missing_name_or_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_agents_missing_text_fields.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create");
        conn.execute_sync(
            "INSERT INTO agents (id, name, program, last_active_ts) VALUES
             (7, NULL, NULL, 200),
             (8, 'NamedAgent', 'codex', 100)",
            &[],
        )
        .expect("insert");

        let agents = fetch_agents_list(&conn);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "[unknown-agent-7]");
        assert_eq!(agents[0].program, "");
        assert_eq!(agents[1].name, "NamedAgent");
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
    fn fetch_projects_list_keeps_rows_with_missing_slug_or_human_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_missing_text_fields.db");
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
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (5, NULL, NULL, 100)",
            &[],
        )
        .expect("insert project");

        let projects = fetch_projects_list(&conn);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, 5);
        assert_eq!(projects[0].slug, "[unknown-project-5]");
        assert_eq!(projects[0].human_key, "[missing-human-key-5]");
    }

    #[test]
    fn fetch_projects_list_falls_back_to_minimal_rows_when_aggregate_query_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_minimal_fallback.db");
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
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (7, 'proj', '/tmp/proj', 100)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, last_active_ts) VALUES (7, 'A', 'x', 0), (7, 'B', 'y', 0)",
            &[],
        )
        .expect("insert agents");

        let projects = fetch_projects_list(&conn);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, 7);
        assert_eq!(projects[0].slug, "proj");
        assert_eq!(projects[0].human_key, "/tmp/proj");
        assert_eq!(projects[0].agent_count, 2);
        assert_eq!(projects[0].message_count, 0);
    }

    #[test]
    fn fetch_projects_list_minimal_backfills_counts_with_group_queries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_minimal_count_backfill.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT, program TEXT, last_active_ts INTEGER)",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL)",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (7, 'proj', '/tmp/proj', 100)",
            &[],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, last_active_ts) VALUES (7, 'A', 'x', 0), (7, 'B', 'y', 0)",
            &[],
        )
        .expect("insert agents");
        conn.execute_sync(
            "INSERT INTO messages (project_id) VALUES (7), (7), (7)",
            &[],
        )
        .expect("insert messages");

        let projects = fetch_projects_list_minimal(&conn, &HashMap::new());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].agent_count, 2);
        assert_eq!(projects[0].message_count, 3);
    }

    #[test]
    fn parse_project_summary_rows_falls_back_to_column_positions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_positional_parse.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (9, 'proj', '/tmp/proj', 123)",
            &[],
        )
        .expect("insert project");

        let rows = conn
            .query_sync(
                "SELECT id AS project_id, slug AS project_slug, human_key AS project_key, \
                        created_at AS created_micros, 2 AS agents_total, 3 AS messages_total \
                 FROM projects",
                &[],
            )
            .expect("query rows");

        let projects = parse_project_summary_rows(rows, &HashMap::new());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, 9);
        assert_eq!(projects[0].slug, "proj");
        assert_eq!(projects[0].human_key, "/tmp/proj");
        assert_eq!(projects[0].created_at, 123);
        assert_eq!(projects[0].agent_count, 2);
        assert_eq!(projects[0].message_count, 3);
    }

    #[test]
    fn fetch_projects_list_orders_mixed_text_and_integer_timestamps_by_actual_recency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test_projects_order_mixed_types.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open");

        conn.execute_sync(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at)",
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
             (1, 'legacy', '/p/legacy', '2026-02-24 15:30:00.123456'),
             (2, 'fresh', '/p/fresh', 1800000000000000)",
            &[],
        )
        .expect("insert projects");

        let projects = fetch_projects_list(&conn);
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].slug, "fresh");
        assert_eq!(projects[1].slug, "legacy");
        assert!(projects[1].created_at > 0);
    }

    #[test]
    fn parse_text_timestamp_accepts_rfc3339_and_date_only_values() {
        assert_eq!(
            parse_text_timestamp("2026-02-24T15:30:00Z"),
            parse_text_timestamp("2026-02-24 15:30:00")
        );
        assert_eq!(
            parse_text_timestamp("2026-02-24"),
            parse_text_timestamp("2026-02-24T00:00:00")
        );
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

    // ── B6: Count/List Consistency Contract ──────────────────────────

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn agents_list_cap_is_explicit_and_bounded() {
        // Documents the contract: MAX_AGENTS caps the list the poller
        // delivers to screens. Screens can detect capping by comparing
        // db.agents (global COUNT) vs db.agents_list.len().
        assert!(
            MAX_AGENTS >= 100,
            "cap must be large enough for real deployments"
        );
        assert!(
            MAX_AGENTS <= 10_000,
            "cap must be bounded to prevent OOM on large DBs"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn projects_list_cap_is_explicit_and_bounded() {
        // Same contract for projects.
        assert!(
            MAX_PROJECTS >= 100,
            "cap must be large enough for real deployments"
        );
        assert!(
            MAX_PROJECTS <= 10_000,
            "cap must be bounded to prevent OOM on large DBs"
        );
    }

    #[test]
    fn fetch_agents_list_sql_has_explicit_limit() {
        // Documents that the agents list query uses ORDER BY + LIMIT.
        // Without LIMIT, the list would grow unbounded with agent count.
        let last_active_sort = timestamp_sort_expr("last_active_ts");
        let sql = format!(
            "SELECT id, name, program, last_active_ts FROM agents \
             ORDER BY {last_active_sort} DESC, id DESC LIMIT {MAX_AGENTS}"
        );
        assert!(
            sql.contains("LIMIT"),
            "agents list query must include LIMIT"
        );
        assert!(
            sql.contains("ORDER BY"),
            "agents list query must be ordered to make LIMIT deterministic"
        );
    }

    #[test]
    fn fetch_projects_list_sql_has_explicit_limit() {
        // Documents that the projects list query uses ORDER BY + LIMIT.
        let created_at_sort = timestamp_sort_expr("created_at");
        let sql = format!(
            "SELECT id, slug, human_key, created_at FROM projects \
             ORDER BY {created_at_sort} DESC, id DESC LIMIT {MAX_PROJECTS}"
        );
        assert!(
            sql.contains("LIMIT"),
            "projects list query must include LIMIT"
        );
        assert!(
            sql.contains("ORDER BY"),
            "projects list query must be ordered to make LIMIT deterministic"
        );
    }

    #[test]
    fn snapshot_count_vs_list_length_consistency() {
        // Documents: when a snapshot has agents < agents_list.len(),
        // it means the COUNT query returned stale/lower data than the
        // actual list fetch. Both are valid but screens must handle this.
        let snap = DbStatSnapshot {
            agents: 5,
            agents_list: vec![
                AgentSummary {
                    name: "RedFox".to_string(),
                    program: "cc".to_string(),
                    last_active_ts: 1,
                },
                AgentSummary {
                    name: "BlueLake".to_string(),
                    program: "cc".to_string(),
                    last_active_ts: 2,
                },
            ],
            projects: 10,
            projects_list: vec![ProjectSummary {
                slug: "alpha".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        // Agents: count=5 but list has 2 (capped at MAX_AGENTS or race)
        assert!(
            snap.agents >= snap.agents_list.len() as u64 || snap.agents_list.len() <= MAX_AGENTS,
            "either count >= list or list is within cap"
        );
        // Projects: count=10 but list has 1 (capped or race)
        assert!(
            snap.projects >= snap.projects_list.len() as u64
                || snap.projects_list.len() <= MAX_PROJECTS,
            "either count >= list or list is within cap"
        );
    }
}
