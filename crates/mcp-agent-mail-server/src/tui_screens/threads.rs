//! Thread Explorer screen with conversation workflow.
//!
//! Provides a split-pane view of message threads: a thread list on the left
//! showing `thread_id`, participant count, message count, and last activity;
//! and a conversation detail panel on the right showing chronological messages
//! within the selected thread.

use std::time::Instant;

use ftui::layout::Rect;
use ftui::text::{Line, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers};
use ftui_runtime::program::Cmd;

use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max threads to fetch.
const MAX_THREADS: usize = 500;

/// Max messages to show within a single thread detail.
const MAX_THREAD_MESSAGES: usize = 200;

/// Periodic refresh interval in seconds.
const REFRESH_INTERVAL_SECS: u64 = 5;

// ──────────────────────────────────────────────────────────────────────
// ThreadSummary — a row in the thread list
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ThreadSummary {
    thread_id: String,
    message_count: usize,
    participant_count: usize,
    last_subject: String,
    last_sender: String,
    last_timestamp_micros: i64,
    last_timestamp_iso: String,
    /// Project slug for cross-project display.
    project_slug: String,
    /// Whether any message in the thread has high/urgent importance.
    has_escalation: bool,
    /// Message velocity: messages per hour over the thread's lifetime.
    velocity_msg_per_hr: f64,
    /// Participant names (comma-separated).
    participant_names: String,
    /// First message timestamp (for velocity calculation).
    #[allow(dead_code)]
    first_timestamp_micros: i64,
}

// ──────────────────────────────────────────────────────────────────────
// View lens and sort mode
// ──────────────────────────────────────────────────────────────────────

/// Determines what secondary info is shown per thread row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewLens {
    /// Default: message count + participant count.
    Activity,
    /// Show participant names.
    Participants,
    /// Show escalation markers and velocity.
    Escalation,
}

impl ViewLens {
    const fn next(self) -> Self {
        match self {
            Self::Activity => Self::Participants,
            Self::Participants => Self::Escalation,
            Self::Escalation => Self::Activity,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Activity => "Activity",
            Self::Participants => "Participants",
            Self::Escalation => "Escalation",
        }
    }
}

/// Sort criteria for the thread list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    /// Most recently active first.
    LastActivity,
    /// Highest message velocity first.
    Velocity,
    /// Most participants first.
    ParticipantCount,
    /// Escalated threads first, then by activity.
    EscalationFirst,
}

impl SortMode {
    const fn next(self) -> Self {
        match self {
            Self::LastActivity => Self::Velocity,
            Self::Velocity => Self::ParticipantCount,
            Self::ParticipantCount => Self::EscalationFirst,
            Self::EscalationFirst => Self::LastActivity,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::LastActivity => "Recent",
            Self::Velocity => "Velocity",
            Self::ParticipantCount => "Participants",
            Self::EscalationFirst => "Escalation",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// ThreadMessage — a message within a thread detail
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ThreadMessage {
    id: i64,
    from_agent: String,
    to_agents: String,
    subject: String,
    body_md: String,
    timestamp_iso: String,
    /// Raw timestamp for sorting (pre-wired for deep-link navigation).
    #[allow(dead_code)]
    timestamp_micros: i64,
    importance: String,
}

// ──────────────────────────────────────────────────────────────────────
// Focus state
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ThreadList,
    DetailPanel,
}

// ──────────────────────────────────────────────────────────────────────
// ThreadExplorerScreen
// ──────────────────────────────────────────────────────────────────────

/// Thread Explorer screen: browse message threads with conversation view.
pub struct ThreadExplorerScreen {
    /// All threads sorted by last activity.
    threads: Vec<ThreadSummary>,
    /// Cursor position in the thread list.
    cursor: usize,
    /// Messages in the currently selected thread.
    detail_messages: Vec<ThreadMessage>,
    /// Scroll offset in the detail panel.
    detail_scroll: usize,
    /// Current focus pane.
    focus: Focus,
    /// Lazy-opened DB connection.
    db_conn: Option<SqliteConnection>,
    /// Whether we attempted to open the DB connection.
    db_conn_attempted: bool,
    /// Last refresh time for periodic re-query.
    last_refresh: Option<Instant>,
    /// Thread ID of the currently loaded detail (avoids redundant queries).
    loaded_thread_id: String,
    /// Whether we need to re-fetch the thread list.
    list_dirty: bool,
    /// Search/filter text (empty = show all).
    filter_text: String,
    /// Whether we're in filter input mode.
    filter_editing: bool,
    /// Active view lens (cycles with Tab).
    view_lens: ViewLens,
    /// Active sort mode (cycles with 's').
    sort_mode: SortMode,
    /// Synthetic event for the focused thread (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
}

impl ThreadExplorerScreen {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            threads: Vec::new(),
            cursor: 0,
            detail_messages: Vec::new(),
            detail_scroll: 0,
            focus: Focus::ThreadList,
            db_conn: None,
            db_conn_attempted: false,
            last_refresh: None,
            loaded_thread_id: String::new(),
            list_dirty: true,
            filter_text: String::new(),
            filter_editing: false,
            view_lens: ViewLens::Activity,
            sort_mode: SortMode::LastActivity,
            focused_synthetic: None,
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected thread.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self.threads.get(self.cursor).map(|t| {
            crate::tui_events::MailEvent::message_sent(
                0, // no single message id
                &t.last_sender,
                t.participant_names.split(", ").map(String::from).collect(),
                &t.last_subject,
                &t.thread_id,
                &t.project_slug,
            )
        });
    }

    /// Re-sort the thread list according to the active sort mode.
    fn apply_sort(&mut self) {
        match self.sort_mode {
            SortMode::LastActivity => {
                self.threads
                    .sort_by_key(|t| std::cmp::Reverse(t.last_timestamp_micros));
            }
            SortMode::Velocity => {
                self.threads.sort_by(|a, b| {
                    b.velocity_msg_per_hr
                        .partial_cmp(&a.velocity_msg_per_hr)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortMode::ParticipantCount => {
                self.threads
                    .sort_by_key(|t| std::cmp::Reverse(t.participant_count));
            }
            SortMode::EscalationFirst => {
                self.threads.sort_by(|a, b| {
                    b.has_escalation
                        .cmp(&a.has_escalation)
                        .then(b.last_timestamp_micros.cmp(&a.last_timestamp_micros))
                });
            }
        }
    }

    /// Ensure we have a DB connection, opening one if needed.
    fn ensure_db_conn(&mut self, state: &TuiSharedState) {
        if self.db_conn.is_some() || self.db_conn_attempted {
            return;
        }
        self.db_conn_attempted = true;
        let db_url = &state.config_snapshot().database_url;
        let cfg = DbPoolConfig {
            database_url: db_url.clone(),
            ..Default::default()
        };
        if let Ok(path) = cfg.sqlite_path() {
            self.db_conn = SqliteConnection::open_file(&path).ok();
        }
    }

    /// Fetch thread list from DB.
    fn refresh_thread_list(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let Some(conn) = &self.db_conn else {
            return;
        };

        self.threads = fetch_threads(conn, &self.filter_text, MAX_THREADS);
        self.apply_sort();
        self.last_refresh = Some(Instant::now());
        self.list_dirty = false;

        // Clamp cursor
        if self.threads.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.threads.len() - 1);
        }

        // Refresh detail if thread changed
        self.refresh_detail_if_needed();
    }

    /// Refresh the detail panel if the selected thread changed.
    fn refresh_detail_if_needed(&mut self) {
        let current_thread_id = self
            .threads
            .get(self.cursor)
            .map_or("", |t| t.thread_id.as_str());

        if current_thread_id == self.loaded_thread_id && !self.loaded_thread_id.is_empty() {
            return;
        }

        if current_thread_id.is_empty() {
            self.detail_messages.clear();
            self.loaded_thread_id.clear();
            self.detail_scroll = 0;
            return;
        }

        let Some(conn) = &self.db_conn else {
            return;
        };

        self.detail_messages = fetch_thread_messages(conn, current_thread_id, MAX_THREAD_MESSAGES);
        self.loaded_thread_id = current_thread_id.to_string();
        self.detail_scroll = 0;
    }
}

impl Default for ThreadExplorerScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ThreadExplorerScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                // Filter editing mode
                if self.filter_editing {
                    match key.code {
                        KeyCode::Enter | KeyCode::Escape => {
                            self.filter_editing = false;
                            if key.code == KeyCode::Enter {
                                self.list_dirty = true;
                            }
                            return Cmd::None;
                        }
                        KeyCode::Backspace => {
                            self.filter_text.pop();
                            self.list_dirty = true;
                            return Cmd::None;
                        }
                        KeyCode::Char(c) => {
                            self.filter_text.push(c);
                            self.list_dirty = true;
                            return Cmd::None;
                        }
                        _ => return Cmd::None,
                    }
                }

                match self.focus {
                    Focus::ThreadList => {
                        match key.code {
                            // Cursor navigation
                            KeyCode::Char('j') | KeyCode::Down => {
                                if !self.threads.is_empty() {
                                    self.cursor = (self.cursor + 1).min(self.threads.len() - 1);
                                    self.detail_scroll = 0;
                                    self.refresh_detail_if_needed();
                                }
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.cursor = self.cursor.saturating_sub(1);
                                self.detail_scroll = 0;
                                self.refresh_detail_if_needed();
                            }
                            KeyCode::Char('G') | KeyCode::End => {
                                if !self.threads.is_empty() {
                                    self.cursor = self.threads.len() - 1;
                                    self.detail_scroll = 0;
                                    self.refresh_detail_if_needed();
                                }
                            }
                            KeyCode::Char('g') | KeyCode::Home => {
                                self.cursor = 0;
                                self.detail_scroll = 0;
                                self.refresh_detail_if_needed();
                            }
                            // Page navigation
                            KeyCode::Char('d') | KeyCode::PageDown => {
                                if !self.threads.is_empty() {
                                    self.cursor = (self.cursor + 20).min(self.threads.len() - 1);
                                    self.detail_scroll = 0;
                                    self.refresh_detail_if_needed();
                                }
                            }
                            KeyCode::Char('u') | KeyCode::PageUp => {
                                self.cursor = self.cursor.saturating_sub(20);
                                self.detail_scroll = 0;
                                self.refresh_detail_if_needed();
                            }
                            // Enter detail pane (or deep-link to messages)
                            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                                self.focus = Focus::DetailPanel;
                            }
                            // Deep-link: jump to timeline at thread last activity.
                            KeyCode::Char('t') => {
                                if let Some(thread) = self.threads.get(self.cursor) {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::TimelineAtTime(
                                            thread.last_timestamp_micros,
                                        ),
                                    ));
                                }
                            }
                            // Search/filter
                            KeyCode::Char('/') => {
                                self.filter_editing = true;
                            }
                            // Cycle sort mode
                            KeyCode::Char('s') => {
                                self.sort_mode = self.sort_mode.next();
                                self.apply_sort();
                            }
                            // Cycle view lens
                            KeyCode::Char('v') => {
                                self.view_lens = self.view_lens.next();
                            }
                            // Clear filter
                            KeyCode::Char('c') if key.modifiers.contains(Modifiers::CTRL) => {
                                self.filter_text.clear();
                                self.list_dirty = true;
                            }
                            _ => {}
                        }
                    }
                    Focus::DetailPanel => {
                        match key.code {
                            // Back to thread list
                            KeyCode::Escape | KeyCode::Char('h') | KeyCode::Left => {
                                self.focus = Focus::ThreadList;
                            }
                            // Scroll detail
                            KeyCode::Char('j') | KeyCode::Down => {
                                self.detail_scroll += 1;
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                            }
                            KeyCode::Char('d') | KeyCode::PageDown => {
                                self.detail_scroll += 20;
                            }
                            KeyCode::Char('u') | KeyCode::PageUp => {
                                self.detail_scroll = self.detail_scroll.saturating_sub(20);
                            }
                            KeyCode::Char('G') | KeyCode::End => {
                                // Jump to bottom of detail
                                self.detail_scroll = self.detail_messages.len().saturating_mul(10); // generous upper bound
                            }
                            KeyCode::Char('g') | KeyCode::Home => {
                                self.detail_scroll = 0;
                            }
                            // Deep-link to message by pressing Enter on a visible message
                            KeyCode::Enter => {
                                if let Some(msg) = self.detail_messages.first() {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::MessageById(msg.id),
                                    ));
                                }
                            }
                            // Deep-link: jump to timeline at thread last activity.
                            KeyCode::Char('t') => {
                                if let Some(thread) = self.threads.get(self.cursor) {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::TimelineAtTime(
                                            thread.last_timestamp_micros,
                                        ),
                                    ));
                                }
                            }
                            // Search/filter
                            KeyCode::Char('/') => {
                                self.focus = Focus::ThreadList;
                                self.filter_editing = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, _tick_count: u64, state: &TuiSharedState) {
        // Initial load or dirty flag
        if self.list_dirty {
            self.refresh_thread_list(state);
            return;
        }

        // Periodic refresh
        let should_refresh = self
            .last_refresh
            .is_none_or(|t| t.elapsed().as_secs() >= REFRESH_INTERVAL_SECS);
        if should_refresh {
            self.list_dirty = true;
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        match target {
            DeepLinkTarget::ThreadById(thread_id) => {
                // Find thread by ID and move cursor to it
                if let Some(pos) = self.threads.iter().position(|t| t.thread_id == *thread_id) {
                    self.cursor = pos;
                    self.detail_scroll = 0;
                    self.focus = Focus::ThreadList;
                    self.refresh_detail_if_needed();
                } else {
                    // Thread not yet loaded; force a refresh then try again
                    self.filter_text.clear();
                    self.list_dirty = true;
                    // Store the target for post-refresh resolution
                    self.loaded_thread_id.clear();
                }
                true
            }
            _ => false,
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 4 || area.width < 20 {
            return;
        }

        // Filter bar (1 row if editing or has filter text, 0 otherwise)
        let has_filter = self.filter_editing || !self.filter_text.is_empty();
        let filter_height: u16 = u16::from(has_filter);
        let content_height = area.height.saturating_sub(filter_height);

        // Render filter bar
        if has_filter {
            let filter_area = Rect::new(area.x, area.y, area.width, filter_height);
            render_filter_bar(frame, filter_area, &self.filter_text, self.filter_editing);
        }

        let content_area = Rect::new(area.x, area.y + filter_height, area.width, content_height);

        // Split content: thread list (left) + detail (right) if wide enough
        if content_area.width >= 80 {
            let list_width = content_area.width * 40 / 100;
            let detail_width = content_area.width - list_width;
            let list_area = Rect::new(
                content_area.x,
                content_area.y,
                list_width,
                content_area.height,
            );
            let detail_area = Rect::new(
                content_area.x + list_width,
                content_area.y,
                detail_width,
                content_area.height,
            );

            render_thread_list(
                frame,
                list_area,
                &self.threads,
                self.cursor,
                matches!(self.focus, Focus::ThreadList),
                self.view_lens,
                self.sort_mode,
            );
            render_thread_detail(
                frame,
                detail_area,
                &self.detail_messages,
                self.threads.get(self.cursor),
                self.detail_scroll,
                matches!(self.focus, Focus::DetailPanel),
            );
        } else {
            // Narrow: show only the active pane
            match self.focus {
                Focus::ThreadList => {
                    render_thread_list(
                        frame,
                        content_area,
                        &self.threads,
                        self.cursor,
                        true,
                        self.view_lens,
                        self.sort_mode,
                    );
                }
                Focus::DetailPanel => {
                    render_thread_detail(
                        frame,
                        content_area,
                        &self.detail_messages,
                        self.threads.get(self.cursor),
                        self.detail_scroll,
                        true,
                    );
                }
            }
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate threads / scroll",
            },
            HelpEntry {
                key: "d/u",
                action: "Page down/up",
            },
            HelpEntry {
                key: "G/g",
                action: "End / Home",
            },
            HelpEntry {
                key: "Enter/l",
                action: "Open thread detail",
            },
            HelpEntry {
                key: "t",
                action: "Timeline at last activity",
            },
            HelpEntry {
                key: "Esc/h",
                action: "Back to thread list",
            },
            HelpEntry {
                key: "/",
                action: "Filter threads",
            },
            HelpEntry {
                key: "Ctrl+C",
                action: "Clear filter",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort mode",
            },
            HelpEntry {
                key: "v",
                action: "Cycle view lens",
            },
        ]
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_editing
    }

    fn title(&self) -> &'static str {
        "Threads"
    }

    fn tab_label(&self) -> &'static str {
        "Threads"
    }
}

// ──────────────────────────────────────────────────────────────────────
// DB query helpers
// ──────────────────────────────────────────────────────────────────────

/// Fetch thread summaries grouped by `thread_id`, sorted by last activity.
fn fetch_threads(conn: &SqliteConnection, filter: &str, limit: usize) -> Vec<ThreadSummary> {
    let filter_clause = if filter.is_empty() {
        String::new()
    } else {
        let escaped = filter.replace('\'', "''");
        format!(
            "WHERE m.thread_id LIKE '%{escaped}%' \
             OR m.subject LIKE '%{escaped}%' \
             OR a_sender.name LIKE '%{escaped}%'"
        )
    };

    let sql = format!(
        "SELECT \
           m.thread_id, \
           COUNT(DISTINCT m.id) AS msg_count, \
           COUNT(DISTINCT a_sender.name) AS participant_count, \
           GROUP_CONCAT(DISTINCT a_sender.name) AS participant_names, \
           MAX(m.created_ts) AS last_ts, \
           MIN(m.created_ts) AS first_ts, \
           p.slug AS project_slug, \
           MAX(CASE WHEN m.importance IN ('high','urgent') THEN 1 ELSE 0 END) AS has_escalation \
         FROM messages m \
         JOIN agents a_sender ON a_sender.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
         {filter_clause} \
         GROUP BY m.thread_id \
         HAVING m.thread_id != '' AND m.thread_id IS NOT NULL \
         ORDER BY last_ts DESC \
         LIMIT {limit}"
    );

    let rows = conn.query_sync(&sql, &[]).ok().unwrap_or_default();

    let mut threads: Vec<ThreadSummary> = rows
        .into_iter()
        .filter_map(|row| {
            let thread_id = row.get_named::<String>("thread_id").ok()?;
            let last_ts = row.get_named::<i64>("last_ts").ok().unwrap_or(0);
            let first_ts = row.get_named::<i64>("first_ts").ok().unwrap_or(last_ts);
            let msg_count = row
                .get_named::<i64>("msg_count")
                .ok()
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(0);

            // Compute velocity: msgs/hour over thread lifetime.
            // Precision loss acceptable: microsecond timestamps and message counts
            // don't need f64's full mantissa for display purposes.
            #[allow(clippy::cast_precision_loss)]
            let duration_hours = (last_ts - first_ts).max(1) as f64 / (3_600_000_000.0);
            #[allow(clippy::cast_precision_loss)]
            let velocity = if duration_hours > 0.001 {
                msg_count as f64 / duration_hours
            } else {
                msg_count as f64 // single-burst thread
            };

            Some(ThreadSummary {
                thread_id,
                message_count: msg_count,
                participant_count: row
                    .get_named::<i64>("participant_count")
                    .ok()
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(0),
                last_subject: String::new(),
                last_sender: String::new(),
                last_timestamp_micros: last_ts,
                last_timestamp_iso: micros_to_iso(last_ts),
                project_slug: row
                    .get_named::<String>("project_slug")
                    .ok()
                    .unwrap_or_default(),
                has_escalation: row.get_named::<i64>("has_escalation").ok().unwrap_or(0) != 0,
                velocity_msg_per_hr: velocity,
                participant_names: row
                    .get_named::<String>("participant_names")
                    .ok()
                    .unwrap_or_default(),
                first_timestamp_micros: first_ts,
            })
        })
        .collect();

    // Fetch the latest subject + sender for each thread in a second pass.
    for thread in &mut threads {
        let detail_sql = format!(
            "SELECT m.subject, a_sender.name AS sender_name \
             FROM messages m \
             JOIN agents a_sender ON a_sender.id = m.sender_id \
             WHERE m.thread_id = '{}' \
             ORDER BY m.created_ts DESC \
             LIMIT 1",
            thread.thread_id.replace('\'', "''")
        );
        if let Some(row) = conn
            .query_sync(&detail_sql, &[])
            .ok()
            .and_then(|mut rows| rows.pop())
        {
            thread.last_subject = row.get_named::<String>("subject").ok().unwrap_or_default();
            thread.last_sender = row
                .get_named::<String>("sender_name")
                .ok()
                .unwrap_or_default();
        }
    }

    threads
}

/// Fetch all messages in a thread, sorted chronologically.
fn fetch_thread_messages(
    conn: &SqliteConnection,
    thread_id: &str,
    limit: usize,
) -> Vec<ThreadMessage> {
    let escaped = thread_id.replace('\'', "''");
    let sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.importance, m.created_ts, \
         a_sender.name AS sender_name, \
         COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
         FROM messages m \
         JOIN agents a_sender ON a_sender.id = m.sender_id \
         LEFT JOIN message_recipients mr ON mr.message_id = m.id \
         LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
         WHERE m.thread_id = '{escaped}' \
         GROUP BY m.id \
         ORDER BY m.created_ts ASC \
         LIMIT {limit}"
    );

    conn.query_sync(&sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let created_ts = row.get_named::<i64>("created_ts").ok()?;
                    Some(ThreadMessage {
                        id: row.get_named::<i64>("id").ok()?,
                        from_agent: row
                            .get_named::<String>("sender_name")
                            .ok()
                            .unwrap_or_default(),
                        to_agents: row
                            .get_named::<String>("to_agents")
                            .ok()
                            .unwrap_or_default(),
                        subject: row.get_named::<String>("subject").ok().unwrap_or_default(),
                        body_md: row.get_named::<String>("body_md").ok().unwrap_or_default(),
                        timestamp_iso: micros_to_iso(created_ts),
                        timestamp_micros: created_ts,
                        importance: row
                            .get_named::<String>("importance")
                            .ok()
                            .unwrap_or_else(|| "normal".to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the filter bar.
fn render_filter_bar(frame: &mut Frame<'_>, area: Rect, text: &str, editing: bool) {
    let cursor = if editing { "_" } else { "" };
    let line = format!(" Filter: {text}{cursor}");
    let p = Paragraph::new(line);
    p.render(area, frame);
}

/// Render the thread list panel.
fn render_thread_list(
    frame: &mut Frame<'_>,
    area: Rect,
    threads: &[ThreadSummary],
    cursor: usize,
    focused: bool,
    view_lens: ViewLens,
    sort_mode: SortMode,
) {
    let focus_tag = if focused { "" } else { " (inactive)" };
    let escalated = threads.iter().filter(|t| t.has_escalation).count();
    let esc_tag = if escalated > 0 {
        format!(" | {escalated} escalated")
    } else {
        String::new()
    };
    let title = format!(
        "Threads ({} total){esc_tag} [Lens:{} Sort:{}]{focus_tag}",
        threads.len(),
        view_lens.label(),
        sort_mode.label(),
    );
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let visible_height = inner.height as usize;

    if threads.is_empty() {
        let p = Paragraph::new("  No threads found.");
        p.render(inner, frame);
        return;
    }

    // Viewport centering
    let total = threads.len();
    let cursor_clamped = cursor.min(total.saturating_sub(1));
    let (start, end) = viewport_range(total, visible_height, cursor_clamped);
    let viewport = &threads[start..end];

    let inner_w = inner.width as usize;
    let mut lines = Vec::with_capacity(viewport.len());
    for (view_idx, thread) in viewport.iter().enumerate() {
        let abs_idx = start + view_idx;
        let marker = if abs_idx == cursor_clamped { '>' } else { ' ' };
        let esc_badge = if thread.has_escalation { "!" } else { " " };

        // Compact timestamp (HH:MM from ISO string)
        let time_short = if thread.last_timestamp_iso.len() >= 16 {
            &thread.last_timestamp_iso[11..16]
        } else {
            &thread.last_timestamp_iso
        };

        // Project tag (shortened)
        let proj_tag = if thread.project_slug.is_empty() {
            String::new()
        } else {
            format!("[{}] ", truncate_str(&thread.project_slug, 12))
        };

        // Lens-specific metadata
        let meta = match view_lens {
            ViewLens::Activity => format!(
                "{} msgs, {} agents, {:.1}/hr",
                thread.message_count, thread.participant_count, thread.velocity_msg_per_hr,
            ),
            ViewLens::Participants => {
                truncate_str(&thread.participant_names, inner_w.saturating_sub(30))
            }
            ViewLens::Escalation => {
                let flag = if thread.has_escalation {
                    "ESCALATED"
                } else {
                    "normal"
                };
                format!("{flag} | {:.1} msg/hr", thread.velocity_msg_per_hr)
            }
        };

        let prefix = format!("{marker}{esc_badge}{time_short} {proj_tag}");
        let meta_len = meta.len() + 2; // " [...]"
        let id_space = inner_w.saturating_sub(prefix.len() + meta_len);
        let thread_id_display = truncate_str(&thread.thread_id, id_space);

        let line = if inner_w > prefix.len() + id_space + meta_len {
            format!("{prefix}{thread_id_display:<id_space$} [{meta}]")
        } else {
            format!("{prefix}{thread_id_display}")
        };
        lines.push(line);

        // Second line: last subject (if there's room)
        if visible_height > viewport.len() * 2 || viewport.len() <= 5 {
            let indent = "    ";
            let subj_space = inner_w.saturating_sub(indent.len());
            let subj_line = if thread.last_sender.is_empty() {
                format!("{indent}{}", truncate_str(&thread.last_subject, subj_space))
            } else {
                let sender_prefix = format!("{}: ", thread.last_sender);
                let remaining = subj_space.saturating_sub(sender_prefix.len());
                format!(
                    "{indent}{sender_prefix}{}",
                    truncate_str(&thread.last_subject, remaining)
                )
            };
            lines.push(subj_line);
        }
    }

    let text = lines.join("\n");
    let p = Paragraph::new(text);
    p.render(inner, frame);
}

/// Render the thread detail/conversation panel.
fn render_thread_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    messages: &[ThreadMessage],
    thread: Option<&ThreadSummary>,
    scroll: usize,
    focused: bool,
) {
    let title = thread.map_or_else(
        || "Thread Detail".to_string(),
        |t| {
            let focus_tag = if focused { "" } else { " (inactive)" };
            format!(
                "Thread: {} ({} msgs){focus_tag}",
                truncate_str(&t.thread_id, 30),
                t.message_count,
            )
        },
    );

    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if messages.is_empty() {
        let text = match thread {
            Some(_) => "  No messages in this thread.",
            None => "  Select a thread to view conversation.",
        };
        let p = Paragraph::new(text);
        p.render(inner, frame);
        return;
    }

    // Build conversation view: each message as a "chat bubble"
    let body_width = inner.width as usize;
    let md_theme = ftui_extras::markdown::MarkdownTheme::default();
    let mut styled_lines: Vec<Line> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        if i > 0 {
            styled_lines.push(Line::raw("")); // Separator between messages
        }

        // Header line: sender → recipients, timestamp
        let importance_badge = match msg.importance.as_str() {
            "high" => " [!]",
            "urgent" => " [!!]",
            _ => "",
        };

        // Compact timestamp
        let time_short = if msg.timestamp_iso.len() >= 19 {
            &msg.timestamp_iso[11..19]
        } else {
            &msg.timestamp_iso
        };

        let to_display = if msg.to_agents.is_empty() {
            String::new()
        } else {
            format!(" -> {}", msg.to_agents)
        };

        let header = format!(
            "[#{id}] {sender}{to}{badge} ({time})",
            id = msg.id,
            sender = msg.from_agent,
            to = to_display,
            badge = importance_badge,
            time = time_short,
        );
        styled_lines.push(Line::raw(truncate_str(&header, body_width)));

        // Subject (if different from thread_id)
        if !msg.subject.is_empty() {
            styled_lines.push(Line::raw(format!(
                "  Subj: {}",
                truncate_str(&msg.subject, body_width.saturating_sub(8))
            )));
        }

        // Body (rendered with GFM markdown support)
        let body_text = crate::tui_markdown::render_body(&msg.body_md, &md_theme);
        for line in body_text.lines() {
            styled_lines.push(line.clone());
        }
    }

    // Apply scroll offset
    let visible_height = inner.height as usize;
    let visible: Vec<Line> = styled_lines
        .into_iter()
        .skip(scroll)
        .take(visible_height)
        .collect();
    let text = Text::from_lines(visible);
    let p = Paragraph::new(text);
    p.render(inner, frame);
}

// ──────────────────────────────────────────────────────────────────────
// Utility helpers
// ──────────────────────────────────────────────────────────────────────

/// Compute the viewport [start, end) to keep cursor visible.
fn viewport_range(total: usize, height: usize, cursor: usize) -> (usize, usize) {
    if total <= height {
        return (0, total);
    }
    let half = height / 2;
    let ideal_start = cursor.saturating_sub(half);
    let start = ideal_start.min(total - height);
    let end = (start + height).min(total);
    (start, end)
}

/// Truncate a string to at most `max_len` characters, adding "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let mut result: String = s.chars().take(max_len - 3).collect();
        result.push_str("...");
        result
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ────────────────────────────────────────────────

    #[test]
    fn new_screen_defaults() {
        let screen = ThreadExplorerScreen::new();
        assert_eq!(screen.cursor, 0);
        assert_eq!(screen.detail_scroll, 0);
        assert!(matches!(screen.focus, Focus::ThreadList));
        assert!(screen.threads.is_empty());
        assert!(screen.detail_messages.is_empty());
        assert!(screen.list_dirty);
        assert!(screen.filter_text.is_empty());
        assert!(!screen.filter_editing);
    }

    #[test]
    fn default_impl_works() {
        let screen = ThreadExplorerScreen::default();
        assert!(screen.threads.is_empty());
    }

    // ── Focus switching ─────────────────────────────────────────────

    #[test]
    fn enter_switches_to_detail() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        screen.update(&enter, &state);
        assert!(matches!(screen.focus, Focus::DetailPanel));
    }

    #[test]
    fn escape_returns_to_thread_list() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);
        assert!(matches!(screen.focus, Focus::ThreadList));
    }

    #[test]
    fn h_key_returns_to_thread_list() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let h = Event::Key(ftui::KeyEvent::new(KeyCode::Char('h')));
        screen.update(&h, &state);
        assert!(matches!(screen.focus, Focus::ThreadList));
    }

    #[test]
    fn l_key_enters_detail() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let l = Event::Key(ftui::KeyEvent::new(KeyCode::Char('l')));
        screen.update(&l, &state);
        assert!(matches!(screen.focus, Focus::DetailPanel));
    }

    #[test]
    fn t_key_deep_links_to_timeline_at_last_activity() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let t = Event::Key(ftui::KeyEvent::new(KeyCode::Char('t')));

        let cmd = screen.update(&t, &state);
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::TimelineAtTime(
                1_700_000_000_000_000
            )))
        ));

        // Same behavior from the detail panel.
        screen.focus = Focus::DetailPanel;
        let cmd2 = screen.update(&t, &state);
        assert!(matches!(
            cmd2,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::TimelineAtTime(
                1_700_000_000_000_000
            )))
        ));
    }

    // ── Cursor navigation ───────────────────────────────────────────

    #[test]
    fn cursor_navigation_with_threads() {
        let mut screen = ThreadExplorerScreen::new();
        for i in 0..10 {
            screen.threads.push(make_thread(&format!("t{i}"), 3, 2));
        }
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // j moves down
        let j = Event::Key(ftui::KeyEvent::new(KeyCode::Char('j')));
        screen.update(&j, &state);
        assert_eq!(screen.cursor, 1);

        // k moves up
        let k = Event::Key(ftui::KeyEvent::new(KeyCode::Char('k')));
        screen.update(&k, &state);
        assert_eq!(screen.cursor, 0);

        // G jumps to end
        let g_upper = Event::Key(ftui::KeyEvent::new(KeyCode::Char('G')));
        screen.update(&g_upper, &state);
        assert_eq!(screen.cursor, 9);

        // g jumps to start
        let g_lower = Event::Key(ftui::KeyEvent::new(KeyCode::Char('g')));
        screen.update(&g_lower, &state);
        assert_eq!(screen.cursor, 0);
    }

    #[test]
    fn cursor_clamps_at_bounds() {
        let mut screen = ThreadExplorerScreen::new();
        for i in 0..3 {
            screen.threads.push(make_thread(&format!("t{i}"), 1, 1));
        }
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Try to go past end
        for _ in 0..10 {
            let j = Event::Key(ftui::KeyEvent::new(KeyCode::Char('j')));
            screen.update(&j, &state);
        }
        assert_eq!(screen.cursor, 2);

        // Try to go before start
        for _ in 0..10 {
            let k = Event::Key(ftui::KeyEvent::new(KeyCode::Char('k')));
            screen.update(&k, &state);
        }
        assert_eq!(screen.cursor, 0);
    }

    // ── Detail scroll ───────────────────────────────────────────────

    #[test]
    fn detail_scroll_in_detail_pane() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        screen.detail_messages.push(make_message(1));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let j = Event::Key(ftui::KeyEvent::new(KeyCode::Char('j')));
        screen.update(&j, &state);
        assert_eq!(screen.detail_scroll, 1);

        let k = Event::Key(ftui::KeyEvent::new(KeyCode::Char('k')));
        screen.update(&k, &state);
        assert_eq!(screen.detail_scroll, 0);

        // Can't go below 0
        screen.update(&k, &state);
        assert_eq!(screen.detail_scroll, 0);
    }

    // ── Filter editing ──────────────────────────────────────────────

    #[test]
    fn slash_enters_filter_mode() {
        let mut screen = ThreadExplorerScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.filter_editing);
    }

    #[test]
    fn filter_typing_appends_chars() {
        let mut screen = ThreadExplorerScreen::new();
        screen.filter_editing = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        for ch in "abc".chars() {
            let ev = Event::Key(ftui::KeyEvent::new(KeyCode::Char(ch)));
            screen.update(&ev, &state);
        }
        assert_eq!(screen.filter_text, "abc");
    }

    #[test]
    fn filter_backspace_removes_char() {
        let mut screen = ThreadExplorerScreen::new();
        screen.filter_editing = true;
        screen.filter_text = "abc".to_string();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let bs = Event::Key(ftui::KeyEvent::new(KeyCode::Backspace));
        screen.update(&bs, &state);
        assert_eq!(screen.filter_text, "ab");
    }

    #[test]
    fn filter_enter_exits_editing() {
        let mut screen = ThreadExplorerScreen::new();
        screen.filter_editing = true;
        screen.filter_text = "test".to_string();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        screen.update(&enter, &state);
        assert!(!screen.filter_editing);
        assert!(screen.list_dirty);
    }

    #[test]
    fn filter_escape_exits_editing() {
        let mut screen = ThreadExplorerScreen::new();
        screen.filter_editing = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);
        assert!(!screen.filter_editing);
    }

    // ── consumes_text_input ─────────────────────────────────────────

    #[test]
    fn consumes_text_input_when_filtering() {
        let mut screen = ThreadExplorerScreen::new();
        assert!(!screen.consumes_text_input());
        screen.filter_editing = true;
        assert!(screen.consumes_text_input());
    }

    // ── Deep-link ───────────────────────────────────────────────────

    #[test]
    fn receive_deep_link_thread_by_id() {
        let mut screen = ThreadExplorerScreen::new();
        for i in 0..5 {
            screen
                .threads
                .push(make_thread(&format!("thread-{i}"), 2, 1));
        }

        let handled = screen.receive_deep_link(&DeepLinkTarget::ThreadById("thread-3".to_string()));
        assert!(handled);
        assert_eq!(screen.cursor, 3);
        assert!(matches!(screen.focus, Focus::ThreadList));
    }

    #[test]
    fn receive_deep_link_unknown_thread_triggers_refresh() {
        let mut screen = ThreadExplorerScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::ThreadById("unknown".to_string()));
        assert!(handled);
        assert!(screen.list_dirty);
    }

    #[test]
    fn receive_deep_link_unrelated_returns_false() {
        let mut screen = ThreadExplorerScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::MessageById(42));
        assert!(!handled);
    }

    // ── Titles ──────────────────────────────────────────────────────

    #[test]
    fn title_and_label() {
        let screen = ThreadExplorerScreen::new();
        assert_eq!(screen.title(), "Threads");
        assert_eq!(screen.tab_label(), "Threads");
    }

    // ── Keybindings ─────────────────────────────────────────────────

    #[test]
    fn keybindings_not_empty() {
        let screen = ThreadExplorerScreen::new();
        assert!(!screen.keybindings().is_empty());
    }

    // ── Rendering (no-panic) ────────────────────────────────────────

    #[test]
    fn render_full_screen_empty_no_panic() {
        let screen = ThreadExplorerScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_with_threads_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        for i in 0..5 {
            screen
                .threads
                .push(make_thread(&format!("thread-{i}"), i + 1, i + 1));
        }
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_with_detail_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("test-thread", 3, 2));
        for i in 0..3 {
            screen.detail_messages.push(make_message(i));
        }
        screen.loaded_thread_id = "test-thread".to_string();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_narrow_screen_no_panic() {
        let screen = ThreadExplorerScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 40, 10), &state);
    }

    #[test]
    fn render_narrow_detail_focus_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 40, 10), &state);
    }

    #[test]
    fn render_minimum_size_no_panic() {
        let screen = ThreadExplorerScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(20, 4, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 20, 4), &state);
    }

    #[test]
    fn render_with_filter_bar_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.filter_text = "test".to_string();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_with_scroll_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        screen.threads.push(make_thread("t1", 10, 3));
        for i in 0..10 {
            screen.detail_messages.push(make_message(i));
        }
        screen.detail_scroll = 5;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    // ── Viewport ────────────────────────────────────────────────────

    #[test]
    fn viewport_small_list() {
        let (start, end) = viewport_range(5, 20, 3);
        assert_eq!(start, 0);
        assert_eq!(end, 5);
    }

    #[test]
    fn viewport_keeps_cursor_visible() {
        let (start, end) = viewport_range(100, 20, 80);
        assert!(start <= 80);
        assert!(end > 80);
        assert_eq!(end - start, 20);
    }

    // ── Truncation ──────────────────────────────────────────────────

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate_str("hello world", 8), "hello...");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    // ── Page navigation ─────────────────────────────────────────────

    #[test]
    fn page_down_up_in_thread_list() {
        let mut screen = ThreadExplorerScreen::new();
        for i in 0..50 {
            screen.threads.push(make_thread(&format!("t{i}"), 1, 1));
        }
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let d = Event::Key(ftui::KeyEvent::new(KeyCode::Char('d')));
        screen.update(&d, &state);
        assert_eq!(screen.cursor, 20);

        let u = Event::Key(ftui::KeyEvent::new(KeyCode::Char('u')));
        screen.update(&u, &state);
        assert_eq!(screen.cursor, 0);
    }

    // ── Test helpers ────────────────────────────────────────────────

    fn make_thread(id: &str, msg_count: usize, participant_count: usize) -> ThreadSummary {
        ThreadSummary {
            thread_id: id.to_string(),
            message_count: msg_count,
            participant_count,
            last_subject: format!("Re: Discussion in {id}"),
            last_sender: "GoldFox".to_string(),
            last_timestamp_micros: 1_700_000_000_000_000,
            last_timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            project_slug: "test-proj".to_string(),
            has_escalation: false,
            #[allow(clippy::cast_precision_loss)]
            velocity_msg_per_hr: msg_count as f64 / 2.0,
            participant_names: "GoldFox,SilverWolf".to_string(),
            first_timestamp_micros: 1_699_993_600_000_000,
        }
    }

    fn make_escalated_thread(id: &str, msg_count: usize) -> ThreadSummary {
        let mut t = make_thread(id, msg_count, 3);
        t.has_escalation = true;
        t.velocity_msg_per_hr = 10.0;
        t
    }

    // ── View lens ───────────────────────────────────────────────────

    #[test]
    fn view_lens_cycles() {
        assert_eq!(ViewLens::Activity.next(), ViewLens::Participants);
        assert_eq!(ViewLens::Participants.next(), ViewLens::Escalation);
        assert_eq!(ViewLens::Escalation.next(), ViewLens::Activity);
    }

    #[test]
    fn view_lens_labels() {
        assert_eq!(ViewLens::Activity.label(), "Activity");
        assert_eq!(ViewLens::Participants.label(), "Participants");
        assert_eq!(ViewLens::Escalation.label(), "Escalation");
    }

    #[test]
    fn v_key_cycles_view_lens() {
        let mut screen = ThreadExplorerScreen::new();
        assert_eq!(screen.view_lens, ViewLens::Activity);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let v = Event::Key(ftui::KeyEvent::new(KeyCode::Char('v')));
        screen.update(&v, &state);
        assert_eq!(screen.view_lens, ViewLens::Participants);

        screen.update(&v, &state);
        assert_eq!(screen.view_lens, ViewLens::Escalation);

        screen.update(&v, &state);
        assert_eq!(screen.view_lens, ViewLens::Activity);
    }

    // ── Sort mode ──────────────────────────────────────────────────

    #[test]
    fn sort_mode_cycles() {
        assert_eq!(SortMode::LastActivity.next(), SortMode::Velocity);
        assert_eq!(SortMode::Velocity.next(), SortMode::ParticipantCount);
        assert_eq!(SortMode::ParticipantCount.next(), SortMode::EscalationFirst);
        assert_eq!(SortMode::EscalationFirst.next(), SortMode::LastActivity);
    }

    #[test]
    fn sort_mode_labels() {
        assert_eq!(SortMode::LastActivity.label(), "Recent");
        assert_eq!(SortMode::Velocity.label(), "Velocity");
        assert_eq!(SortMode::ParticipantCount.label(), "Participants");
        assert_eq!(SortMode::EscalationFirst.label(), "Escalation");
    }

    #[test]
    fn s_key_cycles_sort_mode() {
        let mut screen = ThreadExplorerScreen::new();
        assert_eq!(screen.sort_mode, SortMode::LastActivity);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_eq!(screen.sort_mode, SortMode::Velocity);
    }

    // ── Sorting correctness ────────────────────────────────────────

    #[test]
    fn sort_by_velocity() {
        let mut screen = ThreadExplorerScreen::new();
        let mut t1 = make_thread("slow", 2, 1);
        t1.velocity_msg_per_hr = 1.0;
        let mut t2 = make_thread("fast", 10, 2);
        t2.velocity_msg_per_hr = 50.0;
        screen.threads = vec![t1, t2];

        screen.sort_mode = SortMode::Velocity;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "fast");
        assert_eq!(screen.threads[1].thread_id, "slow");
    }

    #[test]
    fn sort_by_participant_count() {
        let mut screen = ThreadExplorerScreen::new();
        let t1 = make_thread("few", 3, 1);
        let t2 = make_thread("many", 3, 10);
        screen.threads = vec![t1, t2];

        screen.sort_mode = SortMode::ParticipantCount;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "many");
    }

    #[test]
    fn sort_escalation_first() {
        let mut screen = ThreadExplorerScreen::new();
        let t1 = make_thread("normal", 5, 2);
        let t2 = make_escalated_thread("urgent", 5);
        screen.threads = vec![t1, t2];

        screen.sort_mode = SortMode::EscalationFirst;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "urgent");
        assert!(screen.threads[0].has_escalation);
    }

    // ── Cross-project + escalation rendering ───────────────────────

    #[test]
    fn render_with_escalation_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen
            .threads
            .push(make_escalated_thread("alert-thread", 8));
        screen.threads.push(make_thread("normal-thread", 3, 2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_participants_lens_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.view_lens = ViewLens::Participants;
        screen.threads.push(make_thread("t1", 3, 2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_escalation_lens_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.view_lens = ViewLens::Escalation;
        screen.threads.push(make_escalated_thread("hot-thread", 10));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    // ── New keybindings ────────────────────────────────────────────

    #[test]
    fn keybindings_include_sort_and_lens() {
        let screen = ThreadExplorerScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.iter().any(|b| b.key == "s"));
        assert!(bindings.iter().any(|b| b.key == "v"));
        assert!(bindings.iter().any(|b| b.key == "t"));
    }

    fn make_message(id: i64) -> ThreadMessage {
        ThreadMessage {
            id,
            from_agent: "GoldFox".to_string(),
            to_agents: "SilverWolf".to_string(),
            subject: format!("Message #{id}"),
            body_md: format!("Body of message {id}.\nSecond line."),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 1_700_000_000_000_000 + id * 1_000_000,
            importance: if id % 3 == 0 { "high" } else { "normal" }.to_string(),
        }
    }
}
