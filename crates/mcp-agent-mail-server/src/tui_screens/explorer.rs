//! Unified inbox/outbox explorer screen.
//!
//! Provides a cross-project mailbox browser with direction-aware filtering,
//! multiple sort modes, grouping, and ack-status filters.  Reuses type
//! definitions from [`mcp_agent_mail_db::mail_explorer`] for consistency
//! with the MCP tool surface.

use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, Style};
use ftui_runtime::program::Cmd;
use ftui_widgets::StatefulWidget;
use ftui_widgets::input::TextInput;
use ftui_widgets::virtualized::{RenderItem, VirtualizedList, VirtualizedListState};
use std::cell::RefCell;

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::mail_explorer::{AckFilter, Direction, ExplorerStats, GroupMode, SortMode};
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel::{Row, Value};
use mcp_agent_mail_db::timestamps::{micros_to_iso, now_micros};

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

const MAX_ENTRIES: usize = 200;
const DEBOUNCE_TICKS: u8 = 1;

/// Default SLA threshold for overdue acks: 30 minutes in microseconds.
const ACK_SLA_THRESHOLD_MICROS: i64 = 30 * 60 * 1_000_000;

// ──────────────────────────────────────────────────────────────────────
// Focus and filter rail
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    SearchBar,
    FilterRail,
    ResultList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterSlot {
    Direction,
    Sort,
    Group,
    Ack,
}

impl FilterSlot {
    const fn next(self) -> Self {
        match self {
            Self::Direction => Self::Sort,
            Self::Sort => Self::Group,
            Self::Group => Self::Ack,
            Self::Ack => Self::Direction,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Direction => Self::Ack,
            Self::Sort => Self::Direction,
            Self::Group => Self::Sort,
            Self::Ack => Self::Group,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cycling helpers for mail_explorer types
// ──────────────────────────────────────────────────────────────────────

const fn next_direction(d: Direction) -> Direction {
    match d {
        Direction::All => Direction::Inbound,
        Direction::Inbound => Direction::Outbound,
        Direction::Outbound => Direction::All,
    }
}

const fn next_sort(s: SortMode) -> SortMode {
    match s {
        SortMode::DateDesc => SortMode::DateAsc,
        SortMode::DateAsc => SortMode::ImportanceDesc,
        SortMode::ImportanceDesc => SortMode::AgentAlpha,
        SortMode::AgentAlpha => SortMode::DateDesc,
    }
}

const fn next_group(g: GroupMode) -> GroupMode {
    match g {
        GroupMode::None => GroupMode::Project,
        GroupMode::Project => GroupMode::Thread,
        GroupMode::Thread => GroupMode::Agent,
        GroupMode::Agent => GroupMode::None,
    }
}

const fn next_ack(a: AckFilter) -> AckFilter {
    match a {
        AckFilter::All => AckFilter::PendingAck,
        AckFilter::PendingAck => AckFilter::Acknowledged,
        AckFilter::Acknowledged => AckFilter::Unread,
        AckFilter::Unread => AckFilter::All,
    }
}

const fn direction_label(d: Direction) -> &'static str {
    match d {
        Direction::All => "All",
        Direction::Inbound => "Inbox",
        Direction::Outbound => "Outbox",
    }
}

const fn sort_label(s: SortMode) -> &'static str {
    match s {
        SortMode::DateDesc => "Newest",
        SortMode::DateAsc => "Oldest",
        SortMode::ImportanceDesc => "Priority",
        SortMode::AgentAlpha => "Agent A-Z",
    }
}

const fn group_label(g: GroupMode) -> &'static str {
    match g {
        GroupMode::None => "Flat",
        GroupMode::Project => "Project",
        GroupMode::Thread => "Thread",
        GroupMode::Agent => "Agent",
    }
}

const fn ack_label(a: AckFilter) -> &'static str {
    match a {
        AckFilter::All => "All",
        AckFilter::PendingAck => "Pending",
        AckFilter::Acknowledged => "Ack'd",
        AckFilter::Unread => "Unread",
    }
}

// ──────────────────────────────────────────────────────────────────────
// Display entry (lightweight for TUI)
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DisplayEntry {
    message_id: i64,
    project_slug: String,
    sender_name: String,
    to_agents: String,
    subject: String,
    body_preview: String,
    thread_id: Option<String>,
    importance: String,
    ack_required: bool,
    created_ts: i64,
    direction: Direction,
    read_ts: Option<i64>,
    ack_ts: Option<i64>,
}

/// Wrapper for `VirtualizedList` rendering of explorer results.
#[derive(Debug, Clone)]
struct ExplorerDisplayRow {
    entry: DisplayEntry,
}

impl RenderItem for ExplorerDisplayRow {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool) {
        use ftui::text::{Line, Span};

        if area.height == 0 || area.width < 10 {
            return;
        }

        let w = area.width as usize;

        // Marker for selected row
        let marker = if selected {
            crate::tui_theme::SELECTION_PREFIX
        } else {
            crate::tui_theme::SELECTION_PREFIX_EMPTY
        };
        let tp = crate::tui_theme::TuiThemePalette::current();
        let cursor_style = Style::default()
            .fg(tp.selection_fg)
            .bg(tp.selection_bg)
            .bold();

        // Direction badge
        let dir_badge = match self.entry.direction {
            Direction::Inbound => "\u{2190}",  // ←
            Direction::Outbound => "\u{2192}", // →
            Direction::All => " ",
        };

        // Importance badge
        let imp_badge = match self.entry.importance.as_str() {
            "urgent" => "!!",
            "high" => "!",
            _ => " ",
        };

        // Ack badge
        let ack_badge = if self.entry.ack_required {
            if self.entry.ack_ts.is_some() {
                "\u{2713}" // ✓
            } else {
                "?"
            }
        } else {
            " "
        };

        // Time column
        let time = {
            let iso = micros_to_iso(self.entry.created_ts);
            if iso.len() >= 19 {
                iso[11..19].to_string()
            } else {
                iso
            }
        };

        // Build prefix
        let prefix = format!(
            "{marker}{dir_badge}{imp_badge:>2}{ack_badge} #{:<5} {time:>8} ",
            self.entry.message_id
        );
        let prefix_len = prefix.chars().count();

        // Title with remaining space
        let title_space = w.saturating_sub(prefix_len);
        let title = truncate_str(&self.entry.subject, title_space);

        // Build line
        let mut line = Line::from_spans([Span::raw(prefix), Span::raw(title)]);
        if selected {
            line.apply_base_style(cursor_style);
        }

        ftui::widgets::paragraph::Paragraph::new(ftui::text::Text::from_line(line))
            .render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pressure board data
// ──────────────────────────────────────────────────────────────────────

/// An overdue ack-required message that has exceeded the SLA threshold.
#[derive(Debug, Clone)]
struct AckPressureCard {
    agent_name: String,
    project_slug: String,
    count: usize,
    oldest_ts: i64,
    age_minutes: i64,
}

/// Unread message concentration for a specific agent/project.
#[derive(Debug, Clone)]
struct UnreadPressureCard {
    agent_name: String,
    project_slug: String,
    unread_count: usize,
    total_inbound: usize,
}

/// A file reservation that is near expiry or represents a hotspot.
#[derive(Debug, Clone)]
struct ReservationPressureCard {
    agent_name: String,
    project_slug: String,
    path_pattern: String,
    ttl_remaining_minutes: i64,
    exclusive: bool,
}

/// Aggregate pressure board state.
#[derive(Debug, Clone, Default)]
struct PressureBoard {
    overdue_acks: Vec<AckPressureCard>,
    unread_hotspots: Vec<UnreadPressureCard>,
    reservation_pressure: Vec<ReservationPressureCard>,
    computed_at: i64,
}

impl PressureBoard {
    fn total_cards(&self) -> usize {
        self.overdue_acks.len() + self.unread_hotspots.len() + self.reservation_pressure.len()
    }

    fn is_empty(&self) -> bool {
        self.total_cards() == 0
    }
}

// ──────────────────────────────────────────────────────────────────────
// MailExplorerScreen
// ──────────────────────────────────────────────────────────────────────

/// Unified inbox/outbox explorer with direction, sort, group, and ack filters.
#[allow(clippy::struct_excessive_bools)]
pub struct MailExplorerScreen {
    // Filter state
    agent_filter: String,
    direction: Direction,
    sort_mode: SortMode,
    group_mode: GroupMode,
    ack_filter: AckFilter,

    // Search
    search_input: TextInput,

    // Results
    entries: Vec<DisplayEntry>,
    cursor: usize,
    detail_scroll: usize,

    // Stats
    stats: ExplorerStats,

    // Focus
    focus: Focus,
    active_filter: FilterSlot,

    // DB/search state
    db_conn: Option<DbConn>,
    db_conn_attempted: bool,
    last_error: Option<String>,
    debounce_remaining: u8,
    search_dirty: bool,

    // Pressure board
    pressure_mode: bool,
    pressure_board: PressureBoard,
    pressure_cursor: usize,
    pressure_dirty: bool,

    /// Synthetic event for the focused message (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,

    /// `VirtualizedList` state for O(1) scrolling.
    list_state: RefCell<VirtualizedListState>,
}

impl MailExplorerScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent_filter: String::new(),
            direction: Direction::All,
            sort_mode: SortMode::DateDesc,
            group_mode: GroupMode::None,
            ack_filter: AckFilter::All,
            search_input: TextInput::new()
                .with_placeholder("Filter messages... (/ to focus)")
                .with_focused(false),
            entries: Vec::new(),
            cursor: 0,
            detail_scroll: 0,
            stats: ExplorerStats::default(),
            focus: Focus::ResultList,
            active_filter: FilterSlot::Direction,
            db_conn: None,
            db_conn_attempted: false,
            last_error: None,
            debounce_remaining: 0,
            search_dirty: true,

            pressure_mode: false,
            pressure_board: PressureBoard::default(),
            pressure_cursor: 0,
            pressure_dirty: true,

            focused_synthetic: None,
            list_state: RefCell::new(VirtualizedListState::default()),
        }
    }

    /// Sync the `VirtualizedListState` selection with the current cursor.
    fn sync_list_state(&self) {
        let mut state = self.list_state.borrow_mut();
        state.select(Some(self.cursor));
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected message.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self.entries.get(self.cursor).map(|e| {
            crate::tui_events::MailEvent::message_sent(
                e.message_id,
                &e.sender_name,
                e.to_agents.split(", ").map(String::from).collect(),
                &e.subject,
                e.thread_id.as_deref().unwrap_or(""),
                &e.project_slug,
            )
        });
    }

    fn ensure_db_conn(&mut self, state: &TuiSharedState) {
        if self.db_conn.is_some() || self.db_conn_attempted {
            return;
        }
        self.db_conn_attempted = true;
        let db_url = &state.config_snapshot().raw_database_url;
        let cfg = DbPoolConfig {
            database_url: db_url.clone(),
            ..Default::default()
        };
        if let Ok(path) = cfg.sqlite_path() {
            self.db_conn = DbConn::open_file(&path).ok();
        }
    }

    fn execute_query(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let Some(conn) = self.db_conn.take() else {
            return;
        };

        let text_filter = self.search_input.value().trim().to_string();

        // Build and execute inbound + outbound queries
        let mut all_entries = Vec::new();

        if self.direction != Direction::Outbound {
            match self.fetch_inbound(&conn, &text_filter) {
                Ok(entries) => all_entries.extend(entries),
                Err(e) => {
                    self.last_error = Some(e);
                    self.db_conn = Some(conn);
                    self.search_dirty = false;
                    return;
                }
            }
        }

        if self.direction != Direction::Inbound {
            match self.fetch_outbound(&conn, &text_filter) {
                Ok(entries) => all_entries.extend(entries),
                Err(e) => {
                    self.last_error = Some(e);
                    self.db_conn = Some(conn);
                    self.search_dirty = false;
                    return;
                }
            }
        }

        // Compute stats
        self.stats = compute_stats(&all_entries);

        // Sort
        sort_entries(&mut all_entries, self.sort_mode);

        // Truncate
        all_entries.truncate(MAX_ENTRIES);
        self.entries = all_entries;

        // Clamp cursor
        if self.entries.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.entries.len() - 1);
        }
        self.detail_scroll = 0;
        self.last_error = None;
        self.search_dirty = false;
        self.db_conn = Some(conn);
    }

    fn refresh_pressure_board(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let Some(conn) = self.db_conn.take() else {
            return;
        };

        let now = now_micros();

        let overdue_acks = Self::query_overdue_acks(&conn, now);
        let unread_hotspots = Self::query_unread_hotspots(&conn);
        let reservation_pressure = Self::query_reservation_pressure(&conn, now);

        self.pressure_board = PressureBoard {
            overdue_acks: overdue_acks.unwrap_or_default(),
            unread_hotspots: unread_hotspots.unwrap_or_default(),
            reservation_pressure: reservation_pressure.unwrap_or_default(),
            computed_at: now,
        };

        let total_cards = self.pressure_board.total_cards();
        if total_cards == 0 {
            self.pressure_cursor = 0;
        } else {
            self.pressure_cursor = self.pressure_cursor.min(total_cards - 1);
        }
        self.pressure_dirty = false;
        self.db_conn = Some(conn);
    }

    fn query_overdue_acks(conn: &DbConn, now: i64) -> Result<Vec<AckPressureCard>, String> {
        let threshold = now - ACK_SLA_THRESHOLD_MICROS;

        let sql = "SELECT a.name AS agent_name, p.slug AS project_slug, \
                   COUNT(*) AS cnt, MIN(m.created_ts) AS oldest_ts \
                   FROM message_recipients r \
                   JOIN messages m ON m.id = r.message_id \
                   JOIN agents a ON a.id = r.agent_id \
                   JOIN projects p ON p.id = m.project_id \
                   WHERE m.ack_required = 1 AND r.ack_ts IS NULL \
                   AND m.created_ts < ?1 \
                   GROUP BY a.name, p.slug \
                   ORDER BY oldest_ts ASC \
                   LIMIT 50";

        let params = vec![Value::BigInt(threshold)];
        conn.query_sync(sql, &params)
            .map_err(|e| format!("Overdue acks query: {e}"))
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|row| {
                        let oldest_ts: i64 = row.get_named("oldest_ts").ok()?;
                        let age_micros = now.saturating_sub(oldest_ts);
                        Some(AckPressureCard {
                            agent_name: row.get_named("agent_name").unwrap_or_default(),
                            project_slug: row.get_named("project_slug").unwrap_or_default(),
                            count: row
                                .get_named::<i64>("cnt")
                                .ok()
                                .and_then(|v| usize::try_from(v).ok())
                                .unwrap_or(0),
                            oldest_ts,
                            age_minutes: age_micros / 60_000_000,
                        })
                    })
                    .collect()
            })
    }

    fn query_unread_hotspots(conn: &DbConn) -> Result<Vec<UnreadPressureCard>, String> {
        let sql = "SELECT a.name AS agent_name, p.slug AS project_slug, \
                   COUNT(CASE WHEN r.read_ts IS NULL THEN 1 END) AS unread_count, \
                   COUNT(*) AS total_inbound \
                   FROM message_recipients r \
                   JOIN messages m ON m.id = r.message_id \
                   JOIN agents a ON a.id = r.agent_id \
                   JOIN projects p ON p.id = m.project_id \
                   GROUP BY a.name, p.slug \
                   HAVING unread_count > 0 \
                   ORDER BY unread_count DESC \
                   LIMIT 50";

        conn.query_sync(sql, &[])
            .map_err(|e| format!("Unread hotspots query: {e}"))
            .map(|rows| {
                rows.into_iter()
                    .map(|row| UnreadPressureCard {
                        agent_name: row.get_named("agent_name").unwrap_or_default(),
                        project_slug: row.get_named("project_slug").unwrap_or_default(),
                        unread_count: row
                            .get_named::<i64>("unread_count")
                            .ok()
                            .and_then(|v| usize::try_from(v).ok())
                            .unwrap_or(0),
                        total_inbound: row
                            .get_named::<i64>("total_inbound")
                            .ok()
                            .and_then(|v| usize::try_from(v).ok())
                            .unwrap_or(0),
                    })
                    .collect()
            })
    }

    fn query_reservation_pressure(
        conn: &DbConn,
        now: i64,
    ) -> Result<Vec<ReservationPressureCard>, String> {
        let sql = "SELECT fr.path_pattern, fr.exclusive, fr.expires_ts, \
                   a.name AS agent_name, p.slug AS project_slug \
                   FROM file_reservations fr \
                   JOIN agents a ON a.id = fr.agent_id \
                   JOIN projects p ON p.id = fr.project_id \
                   WHERE fr.released_ts IS NULL AND fr.expires_ts > ?1 \
                   ORDER BY fr.expires_ts ASC \
                   LIMIT 50";

        let params = vec![Value::BigInt(now)];
        conn.query_sync(sql, &params)
            .map_err(|e| format!("Reservation pressure query: {e}"))
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|row| {
                        let expires_ts: i64 = row.get_named("expires_ts").ok()?;
                        let remaining_micros = expires_ts.saturating_sub(now);
                        Some(ReservationPressureCard {
                            agent_name: row.get_named("agent_name").unwrap_or_default(),
                            project_slug: row.get_named("project_slug").unwrap_or_default(),
                            path_pattern: row.get_named("path_pattern").unwrap_or_default(),
                            ttl_remaining_minutes: remaining_micros / 60_000_000,
                            exclusive: row
                                .get_named::<i64>("exclusive")
                                .ok()
                                .is_some_and(|v| v != 0),
                        })
                    })
                    .collect()
            })
    }

    fn fetch_inbound(&self, conn: &DbConn, text_filter: &str) -> Result<Vec<DisplayEntry>, String> {
        let mut conditions = Vec::new();
        let mut params: Vec<Value> = Vec::new();

        // Agent filter (filter by recipient name)
        if !self.agent_filter.is_empty() {
            conditions.push("a_recip.name = ?".to_string());
            params.push(Value::Text(self.agent_filter.clone()));
        }

        // Text filter
        if !text_filter.is_empty() {
            let escaped = text_filter.replace('%', "\\%").replace('_', "\\_");
            let like = format!("%{escaped}%");
            let idx = params.len() + 1;
            conditions.push(format!(
                "(m.subject LIKE ?{idx} ESCAPE '\\' OR m.body_md LIKE ?{idx} ESCAPE '\\')"
            ));
            params.push(Value::Text(like));
        }

        // Ack filter
        match self.ack_filter {
            AckFilter::PendingAck => {
                conditions.push("m.ack_required = 1".to_string());
                conditions.push("r.ack_ts IS NULL".to_string());
            }
            AckFilter::Acknowledged => {
                conditions.push("m.ack_required = 1".to_string());
                conditions.push("r.ack_ts IS NOT NULL".to_string());
            }
            AckFilter::Unread => {
                conditions.push("r.read_ts IS NULL".to_string());
            }
            AckFilter::All => {}
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" AND {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT m.id, m.subject, m.body_md, m.importance, m.ack_required, m.created_ts, \
             m.thread_id, s.name AS sender_name, p.slug AS project_slug, \
             r.read_ts, r.ack_ts, \
             COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
             FROM message_recipients r \
             JOIN messages m ON m.id = r.message_id \
             JOIN agents s ON s.id = m.sender_id \
             JOIN projects p ON p.id = m.project_id \
             LEFT JOIN message_recipients mr2 ON mr2.message_id = m.id \
             LEFT JOIN agents a_recip ON a_recip.id = mr2.agent_id \
             WHERE 1=1{where_clause} \
             GROUP BY m.id \
             ORDER BY m.created_ts DESC \
             LIMIT {MAX_ENTRIES}"
        );

        conn.query_sync(&sql, &params)
            .map_err(|e| format!("Inbound query: {e}"))
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|row| map_entry(&row, Direction::Inbound))
                    .collect()
            })
    }

    fn fetch_outbound(
        &self,
        conn: &DbConn,
        text_filter: &str,
    ) -> Result<Vec<DisplayEntry>, String> {
        let mut conditions = Vec::new();
        let mut params: Vec<Value> = Vec::new();

        // Agent filter (filter by sender name)
        if !self.agent_filter.is_empty() {
            conditions.push("s.name = ?".to_string());
            params.push(Value::Text(self.agent_filter.clone()));
        }

        // Text filter
        if !text_filter.is_empty() {
            let escaped = text_filter.replace('%', "\\%").replace('_', "\\_");
            let like = format!("%{escaped}%");
            let idx = params.len() + 1;
            conditions.push(format!(
                "(m.subject LIKE ?{idx} ESCAPE '\\' OR m.body_md LIKE ?{idx} ESCAPE '\\')"
            ));
            params.push(Value::Text(like));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" AND {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT m.id, m.subject, m.body_md, m.importance, m.ack_required, m.created_ts, \
             m.thread_id, s.name AS sender_name, p.slug AS project_slug, \
             COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
             FROM messages m \
             JOIN agents s ON s.id = m.sender_id \
             JOIN projects p ON p.id = m.project_id \
             LEFT JOIN message_recipients mr ON mr.message_id = m.id \
             LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
             WHERE 1=1{where_clause} \
             GROUP BY m.id \
             ORDER BY m.created_ts DESC \
             LIMIT {MAX_ENTRIES}"
        );

        conn.query_sync(&sql, &params)
            .map_err(|e| format!("Outbound query: {e}"))
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|row| map_entry(&row, Direction::Outbound))
                    .collect()
            })
    }

    const fn toggle_active_filter(&mut self) {
        match self.active_filter {
            FilterSlot::Direction => self.direction = next_direction(self.direction),
            FilterSlot::Sort => self.sort_mode = next_sort(self.sort_mode),
            FilterSlot::Group => self.group_mode = next_group(self.group_mode),
            FilterSlot::Ack => self.ack_filter = next_ack(self.ack_filter),
        }
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    fn reset_filters(&mut self) {
        self.direction = Direction::All;
        self.sort_mode = SortMode::DateDesc;
        self.group_mode = GroupMode::None;
        self.ack_filter = AckFilter::All;
        self.agent_filter.clear();
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }
}

impl Default for MailExplorerScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for MailExplorerScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        // Mouse: scroll wheel moves result list cursor (parity with j/k)
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                ftui::MouseEventKind::ScrollDown => {
                    if self.focus == Focus::ResultList && !self.entries.is_empty() {
                        self.cursor = (self.cursor + 1).min(self.entries.len() - 1);
                        self.detail_scroll = 0;
                    }
                }
                ftui::MouseEventKind::ScrollUp => {
                    if self.focus == Focus::ResultList {
                        self.cursor = self.cursor.saturating_sub(1);
                        self.detail_scroll = 0;
                    }
                }
                _ => {}
            }
            return Cmd::None;
        }
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match self.focus {
                    Focus::SearchBar => match key.code {
                        KeyCode::Enter => {
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                            self.focus = Focus::ResultList;
                            self.search_input.set_focused(false);
                        }
                        KeyCode::Escape => {
                            self.focus = Focus::ResultList;
                            self.search_input.set_focused(false);
                        }
                        KeyCode::Tab => {
                            self.focus = Focus::FilterRail;
                            self.search_input.set_focused(false);
                        }
                        _ => {
                            let before = self.search_input.value().to_string();
                            self.search_input.handle_event(event);
                            if self.search_input.value() != before {
                                self.search_dirty = true;
                                self.debounce_remaining = DEBOUNCE_TICKS;
                            }
                        }
                    },

                    Focus::FilterRail => match key.code {
                        KeyCode::Escape | KeyCode::Char('q') | KeyCode::Tab => {
                            self.focus = Focus::ResultList;
                        }
                        KeyCode::Char('/') => {
                            self.focus = Focus::SearchBar;
                            self.search_input.set_focused(true);
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            self.active_filter = self.active_filter.next();
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            self.active_filter = self.active_filter.prev();
                        }
                        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
                            self.toggle_active_filter();
                        }
                        KeyCode::Left => {
                            // Same as toggle for simplicity
                            self.toggle_active_filter();
                        }
                        KeyCode::Char('r') => {
                            self.reset_filters();
                        }
                        _ => {}
                    },

                    Focus::ResultList => match key.code {
                        KeyCode::Char('/') => {
                            self.focus = Focus::SearchBar;
                            self.search_input.set_focused(true);
                        }
                        KeyCode::Tab | KeyCode::Char('f') => {
                            self.focus = Focus::FilterRail;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !self.entries.is_empty() {
                                self.cursor = (self.cursor + 1).min(self.entries.len() - 1);
                                self.detail_scroll = 0;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            self.cursor = self.cursor.saturating_sub(1);
                            self.detail_scroll = 0;
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            if !self.entries.is_empty() {
                                self.cursor = self.entries.len() - 1;
                                self.detail_scroll = 0;
                            }
                        }
                        KeyCode::Char('g') | KeyCode::Home => {
                            self.cursor = 0;
                            self.detail_scroll = 0;
                        }
                        KeyCode::Char('d') | KeyCode::PageDown => {
                            if !self.entries.is_empty() {
                                self.cursor = (self.cursor + 20).min(self.entries.len() - 1);
                                self.detail_scroll = 0;
                            }
                        }
                        KeyCode::Char('u') | KeyCode::PageUp => {
                            self.cursor = self.cursor.saturating_sub(20);
                            self.detail_scroll = 0;
                        }
                        KeyCode::Char('J') => {
                            self.detail_scroll += 1;
                        }
                        KeyCode::Char('K') => {
                            self.detail_scroll = self.detail_scroll.saturating_sub(1);
                        }
                        // Deep-link: Enter on result
                        KeyCode::Enter => {
                            if let Some(entry) = self.entries.get(self.cursor) {
                                return Cmd::msg(MailScreenMsg::DeepLink(
                                    DeepLinkTarget::MessageById(entry.message_id),
                                ));
                            }
                        }
                        // Quick filter toggles
                        KeyCode::Char('D') => {
                            self.direction = next_direction(self.direction);
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                        }
                        KeyCode::Char('s') => {
                            self.sort_mode = next_sort(self.sort_mode);
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                        }
                        KeyCode::Char('a') => {
                            self.ack_filter = next_ack(self.ack_filter);
                            self.search_dirty = true;
                            self.debounce_remaining = 0;
                        }
                        // Toggle pressure board
                        KeyCode::Char('P') => {
                            self.pressure_mode = !self.pressure_mode;
                            if self.pressure_mode {
                                self.pressure_dirty = true;
                            }
                        }
                        // Clear all
                        KeyCode::Char('c') if key.modifiers.contains(Modifiers::CTRL) => {
                            self.search_input.clear();
                            self.reset_filters();
                        }
                        _ => {}
                    },
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, _tick_count: u64, state: &TuiSharedState) {
        if self.search_dirty {
            if self.debounce_remaining > 0 {
                self.debounce_remaining -= 1;
            } else {
                self.execute_query(state);
            }
        }

        if self.pressure_mode && self.pressure_dirty {
            self.refresh_pressure_board(state);
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 4 || area.width < 30 {
            return;
        }

        // Sync list state with cursor before rendering
        self.sync_list_state();

        // Layout: header (3-4h) + body
        let header_h: u16 = if area.height >= 6 { 4 } else { 3 };
        let body_h = area.height.saturating_sub(header_h);

        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let body_area = Rect::new(area.x, area.y + header_h, area.width, body_h);

        render_header(
            frame,
            header_area,
            &self.search_input,
            self,
            matches!(self.focus, Focus::SearchBar),
        );

        if self.pressure_mode {
            // Pressure board takes the full body area
            render_pressure_board(
                frame,
                body_area,
                &self.pressure_board,
                self.pressure_cursor,
                matches!(self.focus, Focus::ResultList),
            );
        } else {
            // Normal mode: filter rail (left) + results + detail (right)
            let filter_w: u16 = if area.width >= 100 { 18 } else { 14 };
            let remaining_w = body_area.width.saturating_sub(filter_w);

            let filter_area = Rect::new(body_area.x, body_area.y, filter_w, body_area.height);

            if remaining_w >= 60 {
                let results_w = remaining_w * 45 / 100;
                let detail_w = remaining_w - results_w;
                let results_area = Rect::new(
                    body_area.x + filter_w,
                    body_area.y,
                    results_w,
                    body_area.height,
                );
                let detail_area = Rect::new(
                    body_area.x + filter_w + results_w,
                    body_area.y,
                    detail_w,
                    body_area.height,
                );

                let filter_focused = matches!(self.focus, Focus::FilterRail);
                let results_focused = matches!(self.focus, Focus::ResultList);
                render_filter_rail(frame, filter_area, self, filter_focused);
                render_results(
                    frame,
                    results_area,
                    &self.entries,
                    &mut self.list_state.borrow_mut(),
                    results_focused,
                );
                render_detail(
                    frame,
                    detail_area,
                    self.entries.get(self.cursor),
                    self.detail_scroll,
                    results_focused,
                );
            } else {
                let results_area = Rect::new(
                    body_area.x + filter_w,
                    body_area.y,
                    remaining_w,
                    body_area.height,
                );
                let filter_focused = matches!(self.focus, Focus::FilterRail);
                let results_focused = matches!(self.focus, Focus::ResultList);
                render_filter_rail(frame, filter_area, self, filter_focused);
                render_results(
                    frame,
                    results_area,
                    &self.entries,
                    &mut self.list_state.borrow_mut(),
                    results_focused,
                );
            }
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "/",
                action: "Focus search",
            },
            HelpEntry {
                key: "f",
                action: "Focus filter rail",
            },
            HelpEntry {
                key: "Tab",
                action: "Cycle focus",
            },
            HelpEntry {
                key: "j/k",
                action: "Navigate",
            },
            HelpEntry {
                key: "Enter",
                action: "Toggle filter / Deep-link",
            },
            HelpEntry {
                key: "D",
                action: "Cycle direction",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort",
            },
            HelpEntry {
                key: "a",
                action: "Cycle ack filter",
            },
            HelpEntry {
                key: "d/u",
                action: "Page down/up",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
            },
            HelpEntry {
                key: "Ctrl+C",
                action: "Clear all",
            },
            HelpEntry {
                key: "P",
                action: "Pressure board",
            },
            HelpEntry {
                key: "r",
                action: "Reset filters",
            },
            HelpEntry {
                key: "Mouse",
                action: "Wheel scroll results",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Explore mailbox structure. Filter by agent, project, or date range.")
    }

    fn consumes_text_input(&self) -> bool {
        matches!(self.focus, Focus::SearchBar)
    }

    fn copyable_content(&self) -> Option<String> {
        let entry = self.entries.get(self.cursor)?;
        if entry.body_preview.is_empty() {
            Some(entry.subject.clone())
        } else {
            Some(format!("{}\n\n{}", entry.subject, entry.body_preview))
        }
    }

    fn title(&self) -> &'static str {
        "Explorer"
    }

    fn tab_label(&self) -> &'static str {
        "Explore"
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        match target {
            DeepLinkTarget::ExplorerForAgent(name) => {
                self.agent_filter.clone_from(name);
                self.search_dirty = true;
                self.debounce_remaining = 0;
                true
            }
            _ => false,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Row mapping
// ──────────────────────────────────────────────────────────────────────

fn map_entry(row: &Row, direction: Direction) -> Option<DisplayEntry> {
    let message_id: i64 = row.get_named("id").ok()?;
    let body: String = row.get_named("body_md").unwrap_or_default();
    let preview = truncate_str(&body, 120);

    Some(DisplayEntry {
        message_id,
        project_slug: row.get_named("project_slug").unwrap_or_default(),
        sender_name: row.get_named("sender_name").unwrap_or_default(),
        to_agents: row.get_named("to_agents").unwrap_or_default(),
        subject: row.get_named("subject").unwrap_or_default(),
        body_preview: preview,
        thread_id: row.get_named("thread_id").ok(),
        importance: row
            .get_named("importance")
            .unwrap_or_else(|_| "normal".to_string()),
        ack_required: row.get_named::<i64>("ack_required").is_ok_and(|v| v != 0),
        created_ts: row.get_named("created_ts").ok()?,
        direction,
        read_ts: row.get_named("read_ts").ok(),
        ack_ts: row.get_named("ack_ts").ok(),
    })
}

// ──────────────────────────────────────────────────────────────────────
// Sorting
// ──────────────────────────────────────────────────────────────────────

fn importance_rank(imp: &str) -> u8 {
    match imp {
        "urgent" => 4,
        "high" => 3,
        "normal" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn sort_entries(entries: &mut [DisplayEntry], mode: SortMode) {
    match mode {
        SortMode::DateDesc => entries.sort_by_key(|e| std::cmp::Reverse(e.created_ts)),
        SortMode::DateAsc => entries.sort_by_key(|e| e.created_ts),
        SortMode::ImportanceDesc => {
            entries.sort_by(|a, b| {
                importance_rank(&b.importance)
                    .cmp(&importance_rank(&a.importance))
                    .then_with(|| b.created_ts.cmp(&a.created_ts))
            });
        }
        SortMode::AgentAlpha => {
            entries.sort_by(|a, b| {
                let agent_a = if a.direction == Direction::Inbound {
                    &a.sender_name
                } else {
                    &a.to_agents
                };
                let agent_b = if b.direction == Direction::Inbound {
                    &b.sender_name
                } else {
                    &b.to_agents
                };
                agent_a
                    .to_lowercase()
                    .cmp(&agent_b.to_lowercase())
                    .then_with(|| b.created_ts.cmp(&a.created_ts))
            });
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Stats
// ──────────────────────────────────────────────────────────────────────

fn compute_stats(entries: &[DisplayEntry]) -> ExplorerStats {
    use std::collections::HashSet;

    let mut projects = HashSet::new();
    let mut threads = HashSet::new();
    let mut agents = HashSet::new();
    let mut inbound = 0usize;
    let mut outbound = 0usize;
    let mut unread = 0usize;
    let mut pending_ack = 0usize;

    for e in entries {
        projects.insert(&e.project_slug);
        if let Some(ref tid) = e.thread_id {
            threads.insert(tid.as_str());
        }
        agents.insert(e.sender_name.as_str());

        if e.direction == Direction::Inbound {
            inbound += 1;
            if e.read_ts.is_none() {
                unread += 1;
            }
            if e.ack_required && e.ack_ts.is_none() {
                pending_ack += 1;
            }
        } else {
            outbound += 1;
        }
    }

    ExplorerStats {
        inbound_count: inbound,
        outbound_count: outbound,
        unread_count: unread,
        pending_ack_count: pending_ack,
        unique_threads: threads.len(),
        unique_projects: projects.len(),
        unique_agents: agents.len(),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &TextInput,
    screen: &MailExplorerScreen,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let dir = direction_label(screen.direction);
    let count = screen.entries.len();
    let focus_label = if screen.focus == Focus::SearchBar {
        " [EDITING]"
    } else {
        ""
    };
    let agent_label = if screen.agent_filter.is_empty() {
        String::new()
    } else {
        format!(" @{}", screen.agent_filter)
    };

    let stat_line = if screen.pressure_mode {
        let pb = &screen.pressure_board;
        let age = if pb.computed_at > 0 {
            let iso = micros_to_iso(pb.computed_at);
            if iso.len() >= 19 {
                iso[11..19].to_string()
            } else {
                iso
            }
        } else {
            "---".to_string()
        };
        format!(
            "PRESSURE: ack:{} unread:{} reserv:{} @{age} | P:toggle",
            pb.overdue_acks.len(),
            pb.unread_hotspots.len(),
            pb.reservation_pressure.len(),
        )
    } else {
        let stats = &screen.stats;
        format!(
            "in:{} out:{} unread:{} ack:{} thr:{} proj:{}",
            stats.inbound_count,
            stats.outbound_count,
            stats.unread_count,
            stats.pending_ack_count,
            stats.unique_threads,
            stats.unique_projects,
        )
    };

    let mode_label = if screen.pressure_mode {
        " [PRESSURE]"
    } else {
        ""
    };
    let title = format!("Explorer {dir}{agent_label} ({count}){focus_label}{mode_label}");
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    input.render(input_area, frame);

    if inner.height >= 2 {
        let w = inner.width as usize;
        let (hint, style) = screen.last_error.as_ref().map_or_else(
            || {
                (
                    truncate_str(&stat_line, w),
                    crate::tui_theme::text_meta(&tp),
                )
            },
            |err| {
                (
                    truncate_str(&format!("ERR: {err}"), w),
                    crate::tui_theme::text_error(&tp),
                )
            },
        );

        let hint_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        Paragraph::new(hint).style(style).render(hint_area, frame);
    }
}

fn render_filter_rail(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &MailExplorerScreen,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Filters")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let in_rail = screen.focus == Focus::FilterRail;
    let w = inner.width as usize;

    let filters: &[(FilterSlot, &str, &str)] = &[
        (
            FilterSlot::Direction,
            "Dir",
            direction_label(screen.direction),
        ),
        (FilterSlot::Sort, "Sort", sort_label(screen.sort_mode)),
        (FilterSlot::Group, "Group", group_label(screen.group_mode)),
        (FilterSlot::Ack, "Ack", ack_label(screen.ack_filter)),
    ];

    for (i, &(slot, label, value)) in filters.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let y = inner.y + (i as u16) * 2;
        if y >= inner.y + inner.height {
            break;
        }

        let is_active = in_rail && screen.active_filter == slot;
        let marker = if is_active { '>' } else { ' ' };

        let label_style = if is_active {
            crate::tui_theme::text_facet_active(&tp)
        } else {
            crate::tui_theme::text_meta(&tp)
        };

        let label_text = format!("{marker} {label}");
        let label_line = truncate_str(&label_text, w);
        let label_area = Rect::new(inner.x, y, inner.width, 1);
        Paragraph::new(label_line)
            .style(label_style)
            .render(label_area, frame);

        let value_y = y + 1;
        if value_y < inner.y + inner.height {
            let val_text = format!("  [{value}]");
            let val_line = truncate_str(&val_text, w);
            let val_area = Rect::new(inner.x, value_y, inner.width, 1);
            let val_style = if is_active {
                Style::default().fg(tp.selection_indicator)
            } else {
                Style::default()
            };
            Paragraph::new(val_line)
                .style(val_style)
                .render(val_area, frame);
        }
    }

    // Help hint
    let help_y = inner.y + inner.height - 1;
    if help_y > inner.y + 9 {
        let hint = if in_rail {
            "Enter:toggle r:reset"
        } else {
            "f:filters"
        };
        let hint_area = Rect::new(inner.x, help_y, inner.width, 1);
        Paragraph::new(truncate_str(hint, w))
            .style(crate::tui_theme::text_hint(&tp))
            .render(hint_area, frame);
    }
}

fn render_results(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[DisplayEntry],
    list_state: &mut VirtualizedListState,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Messages")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if entries.is_empty() {
        Paragraph::new("  No messages found.").render(inner, frame);
        return;
    }

    // Build rows for VirtualizedList
    let rows: Vec<ExplorerDisplayRow> = entries
        .iter()
        .map(|entry| ExplorerDisplayRow {
            entry: entry.clone(),
        })
        .collect();

    VirtualizedList::new(&rows)
        .show_scrollbar(true)
        .render(inner, frame, list_state);
}

#[allow(clippy::cast_possible_truncation)]
fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    entry: Option<&DisplayEntry>,
    scroll: usize,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Detail")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let Some(entry) = entry else {
        Paragraph::new("  Select a message to view details.").render(inner, frame);
        return;
    };

    let mut lines = Vec::new();
    let dir = match entry.direction {
        Direction::Inbound => "Inbound",
        Direction::Outbound => "Outbound",
        Direction::All => "Unknown",
    };
    lines.push(format!("Dir:     {dir}"));
    lines.push(format!("Subject: {}", entry.subject));
    lines.push(format!("From:    {}", entry.sender_name));
    lines.push(format!("To:      {}", entry.to_agents));
    lines.push(format!("Project: {}", entry.project_slug));
    if let Some(ref tid) = entry.thread_id {
        lines.push(format!("Thread:  {tid}"));
    }
    lines.push(format!("Import.: {}", entry.importance));
    if entry.ack_required {
        let ack_status = if entry.ack_ts.is_some() {
            "acknowledged"
        } else {
            "pending"
        };
        lines.push(format!("Ack:     {ack_status}"));
    }
    lines.push(format!("Time:    {}", micros_to_iso(entry.created_ts)));

    lines.push(String::new());
    lines.push("--- Preview ---".to_string());
    lines.push(entry.body_preview.clone());

    let skip = scroll.min(lines.len().saturating_sub(1));
    let visible = &lines[skip..];
    let text = visible.join("\n");
    Paragraph::new(text).render(inner, frame);
}

// ──────────────────────────────────────────────────────────────────────
// Pressure board rendering
// ──────────────────────────────────────────────────────────────────────

#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn render_pressure_board(
    frame: &mut Frame<'_>,
    area: Rect,
    board: &PressureBoard,
    cursor: usize,
    focused: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Pressure Board")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if board.is_empty() {
        Paragraph::new("  No pressure signals. All clear.")
            .style(Style::default().fg(tp.severity_ok))
            .render(inner, frame);
        return;
    }

    let w = inner.width as usize;

    // Build all lines with their styles
    let mut lines: Vec<(String, Style)> = Vec::new();
    let mut card_line_indices: Vec<usize> = Vec::new();
    let mut card_index: usize = 0;

    // Section: Overdue Acks
    if !board.overdue_acks.is_empty() {
        lines.push((
            format!(
                "\u{26A0} Overdue Acks ({} agent/project groups)",
                board.overdue_acks.len()
            ),
            Style::default().fg(tp.metric_messages),
        ));

        for card in &board.overdue_acks {
            let marker = if card_index == cursor { '>' } else { ' ' };
            let oldest_time = {
                let iso = micros_to_iso(card.oldest_ts);
                if iso.len() >= 19 {
                    iso[11..19].to_string()
                } else {
                    iso
                }
            };
            let line = format!(
                "{marker} {:<16} {:<20} {:>3} overdue  {}m ago (since {oldest_time})",
                truncate_str(&card.agent_name, 16),
                truncate_str(&card.project_slug, 20),
                card.count,
                card.age_minutes,
            );
            card_line_indices.push(lines.len());
            lines.push((
                truncate_str(&line, w),
                if card_index == cursor {
                    Style::default().fg(tp.selection_indicator)
                } else {
                    Style::default().fg(tp.severity_error)
                },
            ));
            card_index += 1;
        }
        lines.push((String::new(), Style::default()));
    }

    // Section: Unread Hotspots
    if !board.unread_hotspots.is_empty() {
        lines.push((
            format!(
                "\u{1F4EC} Unread Concentrations ({} agent/project groups)",
                board.unread_hotspots.len()
            ),
            Style::default().fg(tp.metric_messages),
        ));

        for card in &board.unread_hotspots {
            let marker = if card_index == cursor { '>' } else { ' ' };
            let pct = (card.unread_count * 100)
                .checked_div(card.total_inbound)
                .unwrap_or(0);
            let line = format!(
                "{marker} {:<16} {:<20} {:>3} unread / {} total ({}%)",
                truncate_str(&card.agent_name, 16),
                truncate_str(&card.project_slug, 20),
                card.unread_count,
                card.total_inbound,
                pct,
            );
            card_line_indices.push(lines.len());
            lines.push((
                truncate_str(&line, w),
                if card_index == cursor {
                    Style::default().fg(tp.selection_indicator)
                } else {
                    Style::default().fg(tp.severity_warn)
                },
            ));
            card_index += 1;
        }
        lines.push((String::new(), Style::default()));
    }

    // Section: Reservation Pressure
    if !board.reservation_pressure.is_empty() {
        lines.push((
            format!(
                "\u{1F512} Reservation Pressure ({} active)",
                board.reservation_pressure.len()
            ),
            Style::default().fg(tp.metric_messages),
        ));

        for card in &board.reservation_pressure {
            let marker = if card_index == cursor { '>' } else { ' ' };
            let excl = if card.exclusive { "excl" } else { "share" };
            let urgency = if card.ttl_remaining_minutes <= 10 {
                "!!"
            } else if card.ttl_remaining_minutes <= 30 {
                "! "
            } else {
                "  "
            };
            let line = format!(
                "{marker}{urgency}{:<16} {:<20} {excl} {:>4}m  {}",
                truncate_str(&card.agent_name, 16),
                truncate_str(&card.project_slug, 20),
                card.ttl_remaining_minutes,
                truncate_str(&card.path_pattern, 30),
            );
            card_line_indices.push(lines.len());
            let style = if card_index == cursor {
                Style::default().fg(tp.selection_indicator)
            } else if card.ttl_remaining_minutes <= 10 {
                Style::default().fg(tp.severity_error)
            } else {
                Style::default().fg(tp.metric_requests)
            };
            lines.push((truncate_str(&line, w), style));
            card_index += 1;
        }
    }

    // Render visible lines
    let visible_h = inner.height as usize;

    // Find the cursor card's line index for scrolling
    let cursor_line = card_line_indices.get(cursor).copied().unwrap_or(0);
    let (start, end) = viewport_range(lines.len(), visible_h, cursor_line);
    let viewport = &lines[start..end];

    for (i, (text, style)) in viewport.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let line_area = Rect::new(inner.x, y, inner.width, 1);
        Paragraph::new(text.as_str())
            .style(*style)
            .render(line_area, frame);
    }
}

fn viewport_range(total: usize, visible: usize, cursor: usize) -> (usize, usize) {
    if total <= visible {
        return (0, total);
    }
    let half = visible / 2;
    let start = if cursor <= half {
        0
    } else if cursor + half >= total {
        total.saturating_sub(visible)
    } else {
        cursor - half
    };
    let end = (start + visible).min(total);
    (start, end)
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        t.push('\u{2026}');
        t
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_harness::buffer_to_text;

    #[test]
    fn screen_defaults() {
        let screen = MailExplorerScreen::new();
        assert_eq!(screen.focus, Focus::ResultList);
        assert_eq!(screen.direction, Direction::All);
        assert_eq!(screen.sort_mode, SortMode::DateDesc);
        assert_eq!(screen.group_mode, GroupMode::None);
        assert_eq!(screen.ack_filter, AckFilter::All);
        assert!(screen.entries.is_empty());
        assert!(screen.search_dirty);
        assert!(screen.agent_filter.is_empty());
    }

    #[test]
    fn direction_cycles() {
        let mut d = Direction::All;
        d = next_direction(d);
        assert_eq!(d, Direction::Inbound);
        d = next_direction(d);
        assert_eq!(d, Direction::Outbound);
        d = next_direction(d);
        assert_eq!(d, Direction::All);
    }

    #[test]
    fn sort_mode_cycles() {
        let mut s = SortMode::DateDesc;
        s = next_sort(s);
        assert_eq!(s, SortMode::DateAsc);
        s = next_sort(s);
        assert_eq!(s, SortMode::ImportanceDesc);
        s = next_sort(s);
        assert_eq!(s, SortMode::AgentAlpha);
        s = next_sort(s);
        assert_eq!(s, SortMode::DateDesc);
    }

    #[test]
    fn group_mode_cycles() {
        let mut g = GroupMode::None;
        g = next_group(g);
        assert_eq!(g, GroupMode::Project);
        g = next_group(g);
        assert_eq!(g, GroupMode::Thread);
        g = next_group(g);
        assert_eq!(g, GroupMode::Agent);
        g = next_group(g);
        assert_eq!(g, GroupMode::None);
    }

    #[test]
    fn ack_filter_cycles() {
        let mut a = AckFilter::All;
        a = next_ack(a);
        assert_eq!(a, AckFilter::PendingAck);
        a = next_ack(a);
        assert_eq!(a, AckFilter::Acknowledged);
        a = next_ack(a);
        assert_eq!(a, AckFilter::Unread);
        a = next_ack(a);
        assert_eq!(a, AckFilter::All);
    }

    #[test]
    fn filter_slot_cycles() {
        let mut s = FilterSlot::Direction;
        s = s.next();
        assert_eq!(s, FilterSlot::Sort);
        s = s.next();
        assert_eq!(s, FilterSlot::Group);
        s = s.next();
        assert_eq!(s, FilterSlot::Ack);
        s = s.next();
        assert_eq!(s, FilterSlot::Direction);
    }

    #[test]
    fn filter_slot_prev_cycles() {
        let mut s = FilterSlot::Direction;
        s = s.prev();
        assert_eq!(s, FilterSlot::Ack);
        s = s.prev();
        assert_eq!(s, FilterSlot::Group);
    }

    #[test]
    fn toggle_active_filter_direction() {
        let mut screen = MailExplorerScreen::new();
        screen.active_filter = FilterSlot::Direction;
        screen.search_dirty = false;
        screen.toggle_active_filter();
        assert_eq!(screen.direction, Direction::Inbound);
        assert!(screen.search_dirty);
    }

    #[test]
    fn reset_filters_clears_all() {
        let mut screen = MailExplorerScreen::new();
        screen.direction = Direction::Inbound;
        screen.sort_mode = SortMode::ImportanceDesc;
        screen.group_mode = GroupMode::Thread;
        screen.ack_filter = AckFilter::PendingAck;
        screen.agent_filter = "TestAgent".to_string();
        screen.search_dirty = false;

        screen.reset_filters();

        assert_eq!(screen.direction, Direction::All);
        assert_eq!(screen.sort_mode, SortMode::DateDesc);
        assert_eq!(screen.group_mode, GroupMode::None);
        assert_eq!(screen.ack_filter, AckFilter::All);
        assert!(screen.agent_filter.is_empty());
        assert!(screen.search_dirty);
    }

    #[test]
    fn deep_link_explorer_for_agent() {
        let mut screen = MailExplorerScreen::new();
        let handled =
            screen.receive_deep_link(&DeepLinkTarget::ExplorerForAgent("Fox".to_string()));
        assert!(handled);
        assert_eq!(screen.agent_filter, "Fox");
        assert!(screen.search_dirty);
    }

    #[test]
    fn deep_link_other_ignored() {
        let mut screen = MailExplorerScreen::new();
        assert!(!screen.receive_deep_link(&DeepLinkTarget::MessageById(1)));
    }

    #[test]
    fn sort_entries_date_desc() {
        let mut entries = vec![
            test_entry(1, 100, Direction::Inbound),
            test_entry(2, 300, Direction::Outbound),
            test_entry(3, 200, Direction::Inbound),
        ];
        sort_entries(&mut entries, SortMode::DateDesc);
        assert_eq!(entries[0].message_id, 2);
        assert_eq!(entries[1].message_id, 3);
        assert_eq!(entries[2].message_id, 1);
    }

    #[test]
    fn sort_entries_importance() {
        let mut entries = vec![
            test_entry_with_importance(1, 100, "normal"),
            test_entry_with_importance(2, 200, "urgent"),
            test_entry_with_importance(3, 300, "high"),
        ];
        sort_entries(&mut entries, SortMode::ImportanceDesc);
        assert_eq!(entries[0].message_id, 2); // urgent
        assert_eq!(entries[1].message_id, 3); // high
        assert_eq!(entries[2].message_id, 1); // normal
    }

    #[test]
    fn compute_stats_basic() {
        let entries = vec![
            test_entry(1, 100, Direction::Inbound),
            test_entry(2, 200, Direction::Outbound),
            test_entry(3, 300, Direction::Inbound),
        ];
        let stats = compute_stats(&entries);
        assert_eq!(stats.inbound_count, 2);
        assert_eq!(stats.outbound_count, 1);
    }

    #[test]
    fn compute_stats_unread_and_ack() {
        let mut e1 = test_entry(1, 100, Direction::Inbound);
        e1.read_ts = None;
        e1.ack_required = true;
        e1.ack_ts = None;

        let mut e2 = test_entry(2, 200, Direction::Inbound);
        e2.read_ts = Some(150);
        e2.ack_required = true;
        e2.ack_ts = Some(160);

        let stats = compute_stats(&[e1, e2]);
        assert_eq!(stats.unread_count, 1);
        assert_eq!(stats.pending_ack_count, 1);
    }

    #[test]
    fn viewport_range_small() {
        assert_eq!(viewport_range(5, 10, 0), (0, 5));
        assert_eq!(viewport_range(5, 10, 4), (0, 5));
    }

    #[test]
    fn viewport_range_centered() {
        let (start, end) = viewport_range(100, 20, 50);
        assert!(start <= 50);
        assert!(end > 50);
        assert_eq!(end - start, 20);
    }

    #[test]
    fn screen_renders_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = MailExplorerScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 40), &state);
    }

    #[test]
    fn screen_renders_narrow_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = MailExplorerScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(50, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 50, 20), &state);
    }

    #[test]
    fn screen_renders_tiny_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = MailExplorerScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(10, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 3), &state);
    }

    #[test]
    fn keybindings_nonempty() {
        let screen = MailExplorerScreen::new();
        assert!(!screen.keybindings().is_empty());
    }

    #[test]
    fn consumes_text_when_search_focused() {
        let mut screen = MailExplorerScreen::new();
        assert!(!screen.consumes_text_input());
        screen.focus = Focus::SearchBar;
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn screen_title_and_label() {
        let screen = MailExplorerScreen::new();
        assert_eq!(screen.title(), "Explorer");
        assert_eq!(screen.tab_label(), "Explore");
    }

    #[test]
    fn labels_are_nonempty() {
        assert!(!direction_label(Direction::All).is_empty());
        assert!(!sort_label(SortMode::DateDesc).is_empty());
        assert!(!group_label(GroupMode::None).is_empty());
        assert!(!ack_label(AckFilter::All).is_empty());
    }

    #[test]
    fn importance_rank_ordering() {
        assert!(importance_rank("urgent") > importance_rank("high"));
        assert!(importance_rank("high") > importance_rank("normal"));
        assert!(importance_rank("normal") > importance_rank("low"));
    }

    #[test]
    fn header_renders_with_error() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = MailExplorerScreen::new();
        screen.last_error = Some("test error".to_string());

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 80, 10), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(text.contains("ERR:"), "expected ERR line, got:\n{text}");
    }

    // ── Pressure board tests ─────────────────────────────────

    #[test]
    fn pressure_board_default_is_empty() {
        let pb = PressureBoard::default();
        assert!(pb.is_empty());
        assert_eq!(pb.total_cards(), 0);
    }

    #[test]
    fn pressure_board_total_cards() {
        let pb = PressureBoard {
            overdue_acks: vec![test_ack_card("AgentA", 3, 45)],
            unread_hotspots: vec![
                test_unread_card("AgentB", 5, 10),
                test_unread_card("AgentC", 2, 8),
            ],
            reservation_pressure: vec![test_reservation_card("AgentD", "src/**", 15)],
            computed_at: 0,
        };
        assert_eq!(pb.total_cards(), 4);
        assert!(!pb.is_empty());
    }

    #[test]
    fn pressure_toggle_sets_dirty() {
        let mut screen = MailExplorerScreen::new();
        assert!(!screen.pressure_mode);
        screen.pressure_dirty = false;

        // Simulate pressing 'P'
        let ev = ftui::Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('P'),
            modifiers: Modifiers::SHIFT,
            kind: KeyEventKind::Press,
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&ev, &state);

        assert!(screen.pressure_mode);
        assert!(screen.pressure_dirty);
    }

    #[test]
    fn pressure_toggle_off() {
        let mut screen = MailExplorerScreen::new();
        screen.pressure_mode = true;

        let ev = ftui::Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('P'),
            modifiers: Modifiers::SHIFT,
            kind: KeyEventKind::Press,
        });
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.update(&ev, &state);

        assert!(!screen.pressure_mode);
    }

    #[test]
    fn pressure_board_renders_empty() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = MailExplorerScreen::new();
        screen.pressure_mode = true;
        screen.pressure_dirty = false;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 40), &state);
        let text = buffer_to_text(&frame.buffer);
        // In pressure mode, the PRESSURE label appears in the header and
        // the body shows the Pressure Board panel.
        assert!(
            text.contains("PRESSURE") || text.contains("Pressure"),
            "expected PRESSURE or Pressure in output, got:\n{text}"
        );
    }

    #[test]
    fn pressure_board_renders_with_data() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = MailExplorerScreen::new();
        screen.pressure_mode = true;
        screen.pressure_dirty = false;
        screen.pressure_board = PressureBoard {
            overdue_acks: vec![test_ack_card("RedFox", 5, 90)],
            unread_hotspots: vec![test_unread_card("BlueLake", 12, 50)],
            reservation_pressure: vec![test_reservation_card("GoldHawk", "src/**", 8)],
            computed_at: 0,
        };

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 40), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Overdue Acks"),
            "expected Overdue Acks section, got:\n{text}"
        );
        assert!(
            text.contains("Unread"),
            "expected Unread section, got:\n{text}"
        );
        assert!(
            text.contains("Reservation"),
            "expected Reservation section, got:\n{text}"
        );
    }

    #[test]
    fn pressure_board_renders_narrow() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = MailExplorerScreen::new();
        screen.pressure_mode = true;
        screen.pressure_dirty = false;
        screen.pressure_board = PressureBoard {
            overdue_acks: vec![test_ack_card("TestAgent", 2, 60)],
            unread_hotspots: Vec::new(),
            reservation_pressure: Vec::new(),
            computed_at: 0,
        };

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 20), &state);
        // Should not panic
    }

    #[test]
    fn pressure_header_shows_mode() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = MailExplorerScreen::new();
        screen.pressure_mode = true;
        screen.pressure_dirty = false;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 10), &state);
        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("PRESSURE"),
            "expected PRESSURE label, got:\n{text}"
        );
    }

    #[test]
    fn pressure_keybinding_listed() {
        let screen = MailExplorerScreen::new();
        let bindings = screen.keybindings();
        assert!(
            bindings.iter().any(|h| h.key == "P"),
            "P keybinding should be listed"
        );
    }

    // ── Test helpers ──────────────────────────────────────────

    fn test_ack_card(agent: &str, count: usize, age_minutes: i64) -> AckPressureCard {
        AckPressureCard {
            agent_name: agent.to_string(),
            project_slug: "test-project".to_string(),
            count,
            oldest_ts: 0,
            age_minutes,
        }
    }

    fn test_unread_card(
        agent: &str,
        unread_count: usize,
        total_inbound: usize,
    ) -> UnreadPressureCard {
        UnreadPressureCard {
            agent_name: agent.to_string(),
            project_slug: "test-project".to_string(),
            unread_count,
            total_inbound,
        }
    }

    fn test_reservation_card(agent: &str, path: &str, ttl_minutes: i64) -> ReservationPressureCard {
        ReservationPressureCard {
            agent_name: agent.to_string(),
            project_slug: "test-project".to_string(),
            path_pattern: path.to_string(),
            ttl_remaining_minutes: ttl_minutes,
            exclusive: true,
        }
    }

    fn test_entry(id: i64, ts: i64, direction: Direction) -> DisplayEntry {
        DisplayEntry {
            message_id: id,
            project_slug: "test-project".to_string(),
            sender_name: "TestAgent".to_string(),
            to_agents: "OtherAgent".to_string(),
            subject: format!("Subject {id}"),
            body_preview: String::new(),
            thread_id: None,
            importance: "normal".to_string(),
            ack_required: false,
            created_ts: ts,
            direction,
            read_ts: None,
            ack_ts: None,
        }
    }

    fn test_entry_with_importance(id: i64, ts: i64, importance: &str) -> DisplayEntry {
        DisplayEntry {
            importance: importance.to_string(),
            ..test_entry(id, ts, Direction::Inbound)
        }
    }

    // ── truncate_str UTF-8 safety ────────────────────────────────────

    #[test]
    fn truncate_str_ascii_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_ascii_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_ascii_over() {
        let r = truncate_str("hello world", 6);
        assert_eq!(r.chars().count(), 6); // 5 + ellipsis
        assert!(r.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_str_2byte_chars() {
        // é is 2 bytes in UTF-8
        let s = "héllo wörld";
        let r = truncate_str(s, 6);
        assert_eq!(r.chars().count(), 6);
        assert!(r.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_str_3byte_arrow() {
        // → is 3 bytes in UTF-8 — this was the original crash
        let s = "foo → bar → baz";
        let r = truncate_str(s, 7);
        assert_eq!(r.chars().count(), 7);
        assert!(r.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_str_cjk() {
        // CJK chars are 3 bytes each
        let s = "日本語テスト文字列";
        let r = truncate_str(s, 5);
        assert_eq!(r.chars().count(), 5);
        assert!(r.ends_with('\u{2026}'));
        assert!(r.starts_with("日本語テ"));
    }

    #[test]
    fn truncate_str_emoji_4byte() {
        // Emoji are 4 bytes each
        let s = "🔥🚀💡🎯🏆";
        let r = truncate_str(s, 3);
        assert_eq!(r.chars().count(), 3);
        assert!(r.ends_with('\u{2026}'));
        assert!(r.starts_with("🔥🚀"));
    }

    #[test]
    fn truncate_str_mixed_multibyte() {
        let s = "abc→def🔥ghi";
        let r = truncate_str(s, 6);
        assert_eq!(r.chars().count(), 6);
        assert!(r.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_str_max_one() {
        let r = truncate_str("hello", 1);
        assert_eq!(r, "\u{2026}");
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 5), "");
    }

    // ── Mouse parity tests (br-1xt0m.1.12.4) ──────────────────

    fn stub_entry(id: i64) -> DisplayEntry {
        DisplayEntry {
            message_id: id,
            project_slug: String::new(),
            sender_name: String::new(),
            to_agents: String::new(),
            subject: String::new(),
            body_preview: String::new(),
            thread_id: None,
            importance: String::new(),
            ack_required: false,
            created_ts: 0,
            direction: Direction::Inbound,
            read_ts: None,
            ack_ts: None,
        }
    }

    #[test]
    fn mouse_scroll_down_moves_cursor_forward() {
        let mut screen = MailExplorerScreen::new();
        screen.entries = vec![stub_entry(1), stub_entry(2), stub_entry(3)];
        screen.cursor = 0;
        screen.focus = Focus::ResultList;

        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);

        let scroll_down = ftui::Event::Mouse(ftui::MouseEvent::new(
            ftui::MouseEventKind::ScrollDown,
            10,
            10,
        ));
        screen.update(&scroll_down, &state);
        assert_eq!(screen.cursor, 1, "scroll down should advance cursor");
    }

    #[test]
    fn mouse_scroll_up_moves_cursor_back() {
        let mut screen = MailExplorerScreen::new();
        screen.entries = vec![stub_entry(1), stub_entry(2)];
        screen.cursor = 1;
        screen.focus = Focus::ResultList;

        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);

        let scroll_up = ftui::Event::Mouse(ftui::MouseEvent::new(
            ftui::MouseEventKind::ScrollUp,
            10,
            10,
        ));
        screen.update(&scroll_up, &state);
        assert_eq!(screen.cursor, 0, "scroll up should move cursor back");
    }
}
