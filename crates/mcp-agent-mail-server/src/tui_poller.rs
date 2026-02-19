//! Periodic DB poller that feeds [`TuiSharedState`] with fresh statistics.
//!
//! The poller runs on a dedicated background thread using sync `SQLite`
//! connections (not the async pool).  It wakes every `interval`, queries
//! aggregate counts + agent list, computes deltas against the previous
//! snapshot, refreshes shared stats every cycle, and emits health pulses
//! on data changes plus periodic heartbeat intervals.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::timestamps::now_micros;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, MailEvent, ProjectSummary, ReservationSnapshot,
};

/// Default polling interval (2 seconds).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
/// `SQLite` predicate for active reservations across legacy sentinel values.
const ACTIVE_RESERVATION_PREDICATE: &str = "released_ts IS NULL \
    OR (typeof(released_ts) IN ('integer', 'real') AND released_ts <= 0) \
    OR (typeof(released_ts) = 'text' AND lower(trim(released_ts)) IN ('', '0', 'null', 'none')) \
    OR (typeof(released_ts) = 'text' \
      AND length(trim(released_ts)) > 0 \
      AND trim(released_ts) GLOB '*[0-9]*' \
      AND trim(released_ts) NOT GLOB '*[^0-9.+-]*' \
      AND CAST(trim(released_ts) AS REAL) <= 0)";

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

/// Groups DB queries used by the TUI poller so related reads can be fetched
/// with fewer round-trips.
struct DbStatQueryBatcher<'a> {
    conn: &'a DbConn,
}

impl<'a> DbStatQueryBatcher<'a> {
    const fn new(conn: &'a DbConn) -> Self {
        Self { conn }
    }

    fn fetch_snapshot(&self) -> DbStatSnapshot {
        let counts = self.fetch_counts();
        DbStatSnapshot {
            projects: counts.projects,
            agents: counts.agents,
            messages: counts.messages,
            file_reservations: counts.file_reservations,
            contact_links: counts.contact_links,
            ack_pending: counts.ack_pending,
            agents_list: fetch_agents_list(self.conn),
            projects_list: fetch_projects_list(self.conn),
            contacts_list: fetch_contacts_list(self.conn),
            reservation_snapshots: fetch_reservation_snapshots(self.conn),
            timestamp_micros: now_micros(),
        }
    }

    fn fetch_counts(&self) -> DbSnapshotCounts {
        let reservation_count_sql = format!(
            "SELECT \
             (SELECT COUNT(*) FROM projects) AS projects_count, \
             (SELECT COUNT(*) FROM agents) AS agents_count, \
             (SELECT COUNT(*) FROM messages) AS messages_count, \
             (SELECT COUNT(*) FROM file_reservations WHERE ({ACTIVE_RESERVATION_PREDICATE})) \
               AS reservations_count, \
             (SELECT COUNT(*) FROM agent_links) AS contacts_count, \
             (SELECT COUNT(*) FROM message_recipients mr \
                JOIN messages m ON m.id = mr.message_id \
               WHERE m.ack_required = 1 AND mr.ack_ts IS NULL) AS ack_pending_count"
        );
        let batched = self
            .conn
            .query_sync(&reservation_count_sql, &[])
            .ok()
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
                    file_reservations: read_count("reservations_count"),
                    contact_links: read_count("contacts_count"),
                    ack_pending: read_count("ack_pending_count"),
                }
            });

        if let Some(counts) = batched {
            return counts;
        }

        self.fetch_counts_fallback()
    }

    fn fetch_counts_fallback(&self) -> DbSnapshotCounts {
        DbSnapshotCounts {
            projects: self
                .run_count_query("SELECT COUNT(*) AS c FROM projects")
                .unwrap_or(0),
            agents: self
                .run_count_query("SELECT COUNT(*) AS c FROM agents")
                .unwrap_or(0),
            messages: self
                .run_count_query("SELECT COUNT(*) AS c FROM messages")
                .unwrap_or(0),
            file_reservations: self.run_count_query(&format!(
                "SELECT COUNT(*) AS c FROM file_reservations WHERE ({ACTIVE_RESERVATION_PREDICATE})"
            ))
            .unwrap_or(0),
            contact_links: self
                .run_count_query("SELECT COUNT(*) AS c FROM agent_links")
                .unwrap_or(0),
            ack_pending: self
                .run_count_query(
                    "SELECT COUNT(*) AS c FROM message_recipients mr \
                     JOIN messages m ON m.id = mr.message_id \
                     WHERE m.ack_required = 1 AND mr.ack_ts IS NULL",
                )
                .unwrap_or(0),
        }
    }

    fn run_count_query(&self, sql: &str) -> Option<u64> {
        self.conn
            .query_sync(sql, &[])
            .ok()?
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .and_then(|v| u64::try_from(v).ok())
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
            .spawn(move || self.run())
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

        while !self.stop.load(Ordering::Relaxed) {
            // Fetch fresh snapshot
            if let Some(snapshot) = fetch_db_stats(&self.database_url) {
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

/// Fetch a complete [`DbStatSnapshot`] from the database.
///
/// Opens a fresh sync connection, runs aggregate queries, and returns
/// the snapshot.  On any error, returns `None` so callers can keep the
/// previous snapshot instead of clearing existing data.
fn fetch_db_stats(database_url: &str) -> Option<DbStatSnapshot> {
    let conn = open_sync_connection(database_url)?;
    Some(DbStatQueryBatcher::new(&conn).fetch_snapshot())
}

/// Open a sync `SQLite` connection from a database URL (public for compose dispatch).
#[must_use]
pub fn open_sync_connection_pub(database_url: &str) -> Option<DbConn> {
    open_sync_connection(database_url)
}

/// Open a sync `SQLite` connection from a database URL.
fn open_sync_connection(database_url: &str) -> Option<DbConn> {
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
    let path = cfg.sqlite_path().ok()?;
    DbConn::open_file(&path).ok()
}

/// Fetch the agent list ordered by most recently active.
fn fetch_agents_list(conn: &DbConn) -> Vec<AgentSummary> {
    conn.query_sync(
        &format!(
            "SELECT name, program, last_active_ts FROM agents \
             ORDER BY last_active_ts DESC LIMIT {MAX_AGENTS}"
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
fn fetch_projects_list(conn: &DbConn) -> Vec<ProjectSummary> {
    let sql = format!(
        "SELECT p.id, p.slug, p.human_key, p.created_at, \
         COALESCE(ac.cnt, 0) AS agent_count, \
         COALESCE(mc.cnt, 0) AS message_count, \
         COALESCE(rc.cnt, 0) AS reservation_count \
         FROM projects p \
         LEFT JOIN (SELECT project_id, COUNT(*) AS cnt FROM agents GROUP BY project_id) ac \
           ON ac.project_id = p.id \
         LEFT JOIN (SELECT project_id, COUNT(*) AS cnt FROM messages GROUP BY project_id) mc \
           ON mc.project_id = p.id \
         LEFT JOIN (SELECT project_id, COUNT(*) AS cnt FROM file_reservations \
           WHERE ({ACTIVE_RESERVATION_PREDICATE}) GROUP BY project_id) rc \
           ON rc.project_id = p.id \
         ORDER BY p.created_at DESC \
         LIMIT {MAX_PROJECTS}"
    );
    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    Some(ProjectSummary {
                        id: row.get_named::<i64>("id").ok()?,
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
                        reservation_count: row
                            .get_named::<i64>("reservation_count")
                            .ok()
                            .and_then(|v| u64::try_from(v).ok())
                            .unwrap_or(0),
                        created_at: row.get_named::<i64>("created_at").ok().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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
             ORDER BY al.updated_ts DESC \
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

/// Fetch active file reservations with project and agent names.
///
/// This is reused by the reservations screen as a direct fallback when the
/// background poller snapshot is unavailable or stale.
#[allow(clippy::too_many_lines)]
pub(crate) fn fetch_reservation_snapshots(conn: &DbConn) -> Vec<ReservationSnapshot> {
    let sql = format!(
        "SELECT \
           fr.id, \
           COALESCE(p.slug, '[unknown-project]') AS project_slug, \
           COALESCE(a.name, '[unknown-agent]') AS agent_name, \
           fr.path_pattern, \
           fr.exclusive, \
           CASE \
             WHEN fr.created_ts IS NULL THEN 0 \
             WHEN typeof(fr.created_ts) IN ('integer', 'real') THEN CAST(fr.created_ts AS INTEGER) \
             WHEN typeof(fr.created_ts) = 'text' \
               AND length(trim(fr.created_ts)) > 0 \
               AND trim(fr.created_ts) NOT GLOB '*[^0-9]*' THEN CAST(trim(fr.created_ts) AS INTEGER) \
             WHEN typeof(fr.created_ts) = 'text' THEN COALESCE( \
               CAST(strftime('%s', fr.created_ts) AS INTEGER) * 1000000 + \
                 CASE \
                   WHEN instr(fr.created_ts, '.') > 0 THEN CAST(substr(fr.created_ts || '000000', instr(fr.created_ts, '.') + 1, 6) AS INTEGER) \
                   ELSE 0 \
                 END, \
               0 \
             ) \
             ELSE 0 \
           END AS created_ts_micros, \
           CASE \
             WHEN fr.expires_ts IS NULL THEN 0 \
             WHEN typeof(fr.expires_ts) IN ('integer', 'real') THEN CAST(fr.expires_ts AS INTEGER) \
             WHEN typeof(fr.expires_ts) = 'text' \
               AND length(trim(fr.expires_ts)) > 0 \
               AND trim(fr.expires_ts) NOT GLOB '*[^0-9]*' THEN CAST(trim(fr.expires_ts) AS INTEGER) \
             WHEN typeof(fr.expires_ts) = 'text' THEN COALESCE( \
               CAST(strftime('%s', fr.expires_ts) AS INTEGER) * 1000000 + \
                 CASE \
                   WHEN instr(fr.expires_ts, '.') > 0 THEN CAST(substr(fr.expires_ts || '000000', instr(fr.expires_ts, '.') + 1, 6) AS INTEGER) \
                   ELSE 0 \
                 END, \
               0 \
             ) \
             ELSE 0 \
           END AS expires_ts_micros \
         FROM file_reservations fr \
         LEFT JOIN projects p ON p.id = fr.project_id \
         LEFT JOIN agents a ON a.id = fr.agent_id \
         WHERE ({ACTIVE_RESERVATION_PREDICATE}) \
         ORDER BY expires_ts_micros ASC \
         LIMIT {MAX_RESERVATIONS}"
    );

    let rows = match conn.query_sync(&sql, &[]) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(
                error = ?err,
                "tui_poller.fetch_reservation_snapshots query failed"
            );
            return Vec::new();
        }
    };

    rows.into_iter()
        .filter_map(|row| {
            Some(ReservationSnapshot {
                id: row.get_named::<i64>("id").ok()?,
                project_slug: row
                    .get_named::<String>("project_slug")
                    .ok()
                    .unwrap_or_else(|| "[unknown-project]".to_string()),
                agent_name: row
                    .get_named::<String>("agent_name")
                    .ok()
                    .unwrap_or_else(|| "[unknown-agent]".to_string()),
                path_pattern: row.get_named::<String>("path_pattern").ok()?,
                exclusive: row
                    .get_named::<i64>("exclusive")
                    .ok()
                    .is_none_or(|value| value != 0),
                granted_ts: row.get_named::<i64>("created_ts_micros").ok().unwrap_or(0),
                expires_ts: row.get_named::<i64>("expires_ts_micros").ok().unwrap_or(0),
                released_ts: None,
            })
        })
        .collect()
}

/// Read `CONSOLE_POLL_INTERVAL_MS` from environment, default 2000ms.
fn poll_interval_from_env() -> Duration {
    std::env::var("CONSOLE_POLL_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_POLL_INTERVAL, Duration::from_millis)
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
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER)",
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
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER)",
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
        thread::sleep(Duration::from_millis(120));
        let second = state.db_stats_snapshot().expect("second snapshot");

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
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, released_ts INTEGER)",
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
            "INSERT INTO file_reservations (id, project_id, released_ts) VALUES
             (20, 1, NULL), (21, 1, 12345)",
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
        assert!(fetch_db_stats("sqlite:///no/such/dir/nonexistent.db").is_none());
    }

    #[test]
    fn fetch_stats_returns_none_on_empty_url() {
        assert!(fetch_db_stats("").is_none());
    }

    // ── open_sync_connection ─────────────────────────────────────────

    #[test]
    fn open_sync_connection_returns_none_on_bad_path() {
        assert!(open_sync_connection("sqlite:///no/such/dir/db.sqlite3").is_none());
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
                (1, 111, 222, 'src/**', 1, 1000000, 2000000, NULL)",
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
                (1, 1, 2, 'src/**', 1, '2026-02-10 10:00:00.123456', '2026-02-10 11:00:00.123456', NULL),
                (2, 1, 2, 'tests/**', 0, '2026-02-10 10:10:00.000000', '2026-02-10 11:10:00.000000', ''),
                (3, 1, 2, 'docs/**', 0, '2026-02-10 10:20:00.000000', '2026-02-10 11:20:00.000000', '2026-02-10 10:30:00.000000')",
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
             VALUES (1, 1, 1, 'broken/**', 1, 'not-a-date', 'still-not-a-date', NULL)",
            &[],
        )
        .expect("insert reservation");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path_pattern, "broken/**");
        assert_eq!(rows[0].granted_ts, 0);
        assert_eq!(rows[0].expires_ts, 0);
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
                (1, 1, 1, 'src/**', 1, 1000, 2000, 0),
                (2, 1, 1, 'tests/**', 1, 1000, 2000, NULL),
                (3, 1, 1, 'docs/**', 1, 1000, 2000, 123456)",
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
                (1, 1, 1, 'src/**', 1, '1771210958613964', '1771218158613964', '0'),
                (2, 1, 1, 'docs/**', 1, '1771210958613999', '1771218158613999', '1771211000000000')",
            &[],
        )
        .expect("insert reservations");

        let rows = fetch_reservation_snapshots(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path_pattern, "src/**");
        assert_eq!(rows[0].granted_ts, 1_771_210_958_613_964);
        assert_eq!(rows[0].expires_ts, 1_771_218_158_613_964);
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
                (1, 1, 1, 'src/**', 1, 1000, 2000, '0.0'),
                (2, 1, 1, 'tests/**', 0, 1000, 2000, '-1'),
                (3, 1, 1, 'docs/**', 1, 1000, 2000, '1771211000000000')",
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
                (1, 1, 1, 'src/**', 1, 1000, 2000, NULL),
                (2, 1, 1, 'tests/**', 1, 1000, 2000, '0'),
                (3, 1, 1, 'docs/**', 1, 1000, 2000, 'null'),
                (4, 1, 1, 'tmp/**', 1, 1000, 2000, '0.0'),
                (5, 1, 1, 'build/**', 1, 1000, 2000, '1771211000000000')",
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
        assert!(MAX_AGENTS > 0);
        assert!(MAX_PROJECTS > 0);
        assert!(MAX_CONTACTS > 0);
        assert!(MAX_RESERVATIONS > 0);
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
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, released_ts INTEGER)",
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
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, released_ts INTEGER)",
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
            "INSERT INTO file_reservations (project_id, released_ts) VALUES (1, NULL)",
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
    fn health_pulse_heartbeat_interval_is_reasonable() {
        assert!(HEALTH_PULSE_HEARTBEAT_INTERVAL.as_secs() >= 5);
        assert!(HEALTH_PULSE_HEARTBEAT_INTERVAL.as_secs() <= 60);
    }
}
