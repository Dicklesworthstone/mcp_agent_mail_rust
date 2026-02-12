//! Periodic DB poller that feeds [`TuiSharedState`] with fresh statistics.
//!
//! The poller runs on a dedicated background thread using sync `SQLite`
//! connections (not the async pool).  It wakes every `interval`, queries
//! aggregate counts + agent list, computes deltas against the previous
//! snapshot, and only pushes updates when something changed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::timestamps::now_micros;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{AgentSummary, ContactSummary, DbStatSnapshot, MailEvent, ProjectSummary};

/// Default polling interval (2 seconds).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Maximum agents to fetch per poll cycle.
const MAX_AGENTS: usize = 50;

/// Maximum projects to fetch per poll cycle.
const MAX_PROJECTS: usize = 100;

/// Maximum contact links to fetch per poll cycle.
const MAX_CONTACTS: usize = 200;

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

        while !self.stop.load(Ordering::Relaxed) {
            // Fetch fresh snapshot
            let snapshot = fetch_db_stats(&self.database_url);

            // Only push if data fields changed (ignore timestamp_micros)
            if snapshot_delta(&prev, &snapshot).any_changed() {
                self.state.update_db_stats(snapshot.clone());
                let _ = self
                    .state
                    .push_event(MailEvent::health_pulse(snapshot.clone()));
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
/// the snapshot.  On any error, returns a default (zeroed) snapshot.
fn fetch_db_stats(database_url: &str) -> DbStatSnapshot {
    let Some(conn) = open_sync_connection(database_url) else {
        return DbStatSnapshot::default();
    };

    let agents_list = fetch_agents_list(&conn);
    let projects_list = fetch_projects_list(&conn);
    let contacts_list = fetch_contacts_list(&conn);

    DbStatSnapshot {
        projects: count_query(&conn, "SELECT COUNT(*) AS c FROM projects"),
        agents: count_query(&conn, "SELECT COUNT(*) AS c FROM agents"),
        messages: count_query(&conn, "SELECT COUNT(*) AS c FROM messages"),
        file_reservations: count_query(
            &conn,
            "SELECT COUNT(*) AS c FROM file_reservations WHERE released_ts IS NULL",
        ),
        contact_links: count_query(&conn, "SELECT COUNT(*) AS c FROM agent_links"),
        ack_pending: count_query(
            &conn,
            "SELECT COUNT(*) AS c FROM message_recipients mr \
             JOIN messages m ON m.id = mr.message_id \
             WHERE m.ack_required = 1 AND mr.ack_ts IS NULL",
        ),
        agents_list,
        projects_list,
        contacts_list,
        timestamp_micros: now_micros(),
    }
}

/// Open a sync `SQLite` connection from a database URL.
fn open_sync_connection(database_url: &str) -> Option<DbConn> {
    let cfg = DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let path = cfg.sqlite_path().ok()?;
    DbConn::open_file(&path).ok()
}

/// Run a `SELECT COUNT(*) AS c FROM ...` query and return the count.
fn count_query(conn: &DbConn, sql: &str) -> u64 {
    conn.query_sync(sql, &[])
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<i64>("c").ok())
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0)
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
    conn.query_sync(
        &format!(
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
               WHERE released_ts IS NULL GROUP BY project_id) rc \
               ON rc.project_id = p.id \
             ORDER BY p.created_at DESC \
             LIMIT {MAX_PROJECTS}"
        ),
        &[],
    )
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
            timestamp_micros: 1,
        };
        let d = snapshot_delta(&a, &b);
        assert_eq!(d.changed_count(), 9);
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

    // ── fetch_db_stats with nonexistent DB ───────────────────────────

    #[test]
    fn fetch_stats_returns_default_on_bad_url() {
        let stats = fetch_db_stats("sqlite:///nonexistent_path_xyz.db");
        let expected = DbStatSnapshot {
            timestamp_micros: stats.timestamp_micros,
            ..DbStatSnapshot::default()
        };
        assert_eq!(stats, expected);
    }

    #[test]
    fn fetch_stats_returns_default_on_empty_url() {
        let stats = fetch_db_stats("");
        assert_eq!(stats, DbStatSnapshot::default());
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
}
