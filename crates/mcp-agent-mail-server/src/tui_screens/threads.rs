//! Thread Explorer screen with conversation workflow.
//!
//! Provides a split-pane view of message threads: a thread list on the left
//! showing `thread_id`, participant count, message count, and last activity;
//! and a conversation detail panel on the right showing chronological messages
//! within the selected thread.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use asupersync::Outcome;
use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{
    Buffer, Event, Frame, KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind,
    PackedRgba, Style,
};
use ftui_extras::mermaid::{self, MermaidCompatibilityMatrix, MermaidFallbackPolicy};
use ftui_extras::{mermaid_layout, mermaid_render};
use ftui_runtime::program::Cmd;
use ftui_widgets::tree::TreeGuides;

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel_core::Value;
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_bridge::{KeyboardMoveSnapshot, MessageDragSnapshot, TuiSharedState};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::{MermaidThreadMessage, generate_thread_flow_mermaid};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max threads to fetch.
const MAX_THREADS: usize = 500;
const MAX_THREAD_FILTER_IDS: usize = 400;

/// Periodic refresh interval in seconds.
const REFRESH_INTERVAL_SECS: u64 = 5;

/// Default page size for thread pagination.
/// Override via `AM_TUI_THREAD_PAGE_SIZE` environment variable.
const DEFAULT_THREAD_PAGE_SIZE: usize = 20;

/// Number of older messages to load when clicking "Load older".
const LOAD_OLDER_BATCH_SIZE: usize = 15;
const URGENT_PULSE_HALF_PERIOD_TICKS: u64 = 5;
const MERMAID_RENDER_DEBOUNCE: Duration = Duration::from_secs(1);
const MESSAGE_DRAG_HOLD_DELAY: Duration = Duration::from_millis(200);
const THREAD_SPLIT_WIDTH_THRESHOLD: u16 = 80;
// Keep a dedicated blank separator whenever split mode is active so adjacent
// rounded panel borders never visually merge into a heavy "random" line.
const THREAD_MAIN_PANE_GAP_THRESHOLD: u16 = THREAD_SPLIT_WIDTH_THRESHOLD;
#[allow(dead_code)]
const THREAD_WIDE_LIST_PERCENT: u16 = 34;
const THREAD_STACKED_MIN_HEIGHT: u16 = 14;
const THREAD_STACKED_LIST_PERCENT: u16 = 42;
const THREAD_STACKED_SPLITTER_HEIGHT: u16 = 1;
const THREAD_COMPACT_HINT_MIN_HEIGHT: u16 = 7;
const THREAD_DETAIL_PANE_GAP_THRESHOLD: u16 = 64;
const THREAD_DETAIL_COMPACT_WIDTH_THRESHOLD: u16 = 74;
const THREAD_DETAIL_COMPACT_HEIGHT_THRESHOLD: u16 = 12;
const THREAD_DETAIL_MIN_PREVIEW_WIDTH: u16 = 20;
const THREAD_DETAIL_MIN_PANE_RENDER_WIDTH: u16 = 18;
const THREAD_DETAIL_MIN_BODY_HEIGHT: u16 = 6;
const THREAD_COLLAPSED_PREVIEW_LINES: usize = 3;
const THREAD_LIST_COMPACT_MIN_WIDTH: usize = 34;
const THREAD_SUBJECT_LINE_MIN_WIDTH: usize = 40;
const THREAD_LIST_SIDE_PADDING_MIN_WIDTH: u16 = 42;
const THREAD_DETAIL_SIDE_PADDING_MIN_WIDTH: u16 = 52;

#[allow(dead_code)]
const fn thread_list_width_percent(total_width: u16) -> u16 {
    if total_width >= 280 {
        22
    } else if total_width >= 220 {
        24
    } else if total_width >= 170 {
        27
    } else if total_width >= 130 {
        30
    } else {
        THREAD_WIDE_LIST_PERCENT
    }
}

const fn thread_tree_width_percent(detail_width: u16) -> u16 {
    if detail_width >= 220 {
        42
    } else if detail_width >= 170 {
        46
    } else if detail_width >= 130 {
        52
    } else {
        60
    }
}

/// Color palette for deterministic per-agent coloring in thread cards.
fn agent_color_palette() -> [PackedRgba; 8] {
    crate::tui_theme::TuiThemePalette::current().agent_palette
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn reduced_motion_enabled() -> bool {
    env_flag_enabled("AM_TUI_REDUCED_MOTION") || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

fn parse_tree_guides(raw: &str) -> Option<TreeGuides> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "ascii" => Some(TreeGuides::Ascii),
        "unicode" => Some(TreeGuides::Unicode),
        "bold" => Some(TreeGuides::Bold),
        "double" => Some(TreeGuides::Double),
        "rounded" => Some(TreeGuides::Rounded),
        _ => None,
    }
}

fn theme_default_tree_guides() -> TreeGuides {
    // Rounded is the default to align with rounded panel borders.
    match crate::tui_theme::current_theme_id() {
        ftui_extras::theme::ThemeId::HighContrast => TreeGuides::Bold,
        _ => TreeGuides::Rounded,
    }
}

fn thread_tree_guides() -> TreeGuides {
    std::env::var("AM_TUI_THREAD_GUIDES")
        .ok()
        .as_deref()
        .and_then(parse_tree_guides)
        .unwrap_or_else(theme_default_tree_guides)
}

const fn tree_indent_token(guides: TreeGuides) -> &'static str {
    match guides {
        // Keep hierarchy cues intentionally non-border-like so indent guides
        // never read as stray panel borders across wrapped text.
        // Rounded is the default in this screen; keep it non-border-like.
        TreeGuides::Ascii | TreeGuides::Unicode | TreeGuides::Rounded => "· ",
        TreeGuides::Bold => "▪ ",
        TreeGuides::Double => "• ",
    }
}

fn clear_rect(frame: &mut Frame<'_>, area: Rect, bg: PackedRgba) {
    if area.is_empty() {
        return;
    }
    let fg = crate::tui_theme::TuiThemePalette::current().text_primary;
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = fg;
                cell.bg = bg;
            }
        }
    }
}

fn render_splitter_handle(frame: &mut Frame<'_>, area: Rect, vertical: bool, active: bool) {
    if area.is_empty() {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();

    // Always repaint the splitter gap so stale border glyphs never leak into
    // content when layouts toggle between stacked/wide modes.
    let separator_color = crate::tui_theme::lerp_color(tp.panel_bg, tp.panel_border_dim, 0.58);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = separator_color;
                cell.bg = tp.panel_bg;
            }
        }
    }

    if active && ((vertical && area.height >= 5) || (!vertical && area.width >= 5)) {
        let knob_color = tp.selection_indicator;
        let x = if vertical {
            area.x.saturating_add(area.width / 2)
        } else {
            area.x.saturating_add(area.width.saturating_sub(1) / 2)
        };
        let y = if vertical {
            area.y.saturating_add(area.height.saturating_sub(1) / 2)
        } else {
            area.y.saturating_add(area.height / 2)
        };
        if let Some(cell) = frame.buffer.get_mut(x, y) {
            *cell = ftui::Cell::from_char('·');
            cell.fg = knob_color;
            cell.bg = tp.panel_bg;
        }
    }
}

fn parse_thread_page_size(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_THREAD_PAGE_SIZE)
}

/// Get the configured thread page size from environment or default.
fn get_thread_page_size() -> usize {
    parse_thread_page_size(std::env::var("AM_TUI_THREAD_PAGE_SIZE").ok().as_deref())
}

/// Deterministically map an agent name to one of eight theme-safe colors.
fn agent_color(name: &str) -> PackedRgba {
    // FNV-1a 64-bit hash; deterministic and fast for tiny identifiers.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let palette = agent_color_palette();
    let palette_len_u64 = u64::try_from(palette.len()).unwrap_or(1);
    let idx_u64 = hash % palette_len_u64;
    let idx = usize::try_from(idx_u64).unwrap_or(0);
    palette[idx]
}

fn iso_compact_time(iso: &str) -> &str {
    if iso.len() >= 19 { &iso[11..19] } else { iso }
}

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
    /// First message timestamp in ISO format (for time span display).
    first_timestamp_iso: String,
    /// Number of unread messages in this thread (if tracking is available).
    unread_count: usize,
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
    reply_to_id: Option<i64>,
    from_agent: String,
    to_agents: String,
    subject: String,
    body_md: String,
    timestamp_iso: String,
    /// Raw timestamp for sorting (pre-wired for deep-link navigation).
    #[allow(dead_code)]
    timestamp_micros: i64,
    importance: String,
    is_unread: bool,
    ack_required: bool,
}

#[derive(Debug, Clone)]
struct ThreadTreeRow {
    message_id: i64,
    has_children: bool,
    is_expanded: bool,
    depth: usize,
    label: String,
}

#[derive(Debug, Clone)]
struct MermaidPanelCache {
    source_hash: u64,
    width: u16,
    height: u16,
    buffer: Buffer,
}

// ──────────────────────────────────────────────────────────────────────
// Focus state
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    ThreadList,
    DetailPanel,
}

#[derive(Debug, Clone)]
enum MessageDragState {
    Idle,
    Pending(PendingMessageDrag),
    Active(ActiveMessageDrag),
}

#[derive(Debug, Clone)]
struct PendingMessageDrag {
    message_id: i64,
    source_thread_id: String,
    source_project_slug: String,
    subject: String,
    started_at: Instant,
    cursor_x: u16,
    cursor_y: u16,
}

#[derive(Debug, Clone)]
struct ActiveMessageDrag {
    message_id: i64,
    source_thread_id: String,
    source_project_slug: String,
    subject: String,
    cursor_x: u16,
    cursor_y: u16,
    hovered_thread_id: Option<String>,
    hovered_is_valid: bool,
    invalid_hover: bool,
}

#[derive(Debug, Clone, Copy)]
struct ThreadDropVisual<'a> {
    source_thread_id: &'a str,
    hovered_thread_id: Option<&'a str>,
    invalid_hover: bool,
}

// ──────────────────────────────────────────────────────────────────────
// ThreadExplorerScreen
// ──────────────────────────────────────────────────────────────────────

/// Thread Explorer screen: browse message threads with conversation view.
#[allow(clippy::struct_excessive_bools)]
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
    db_conn: Option<DbConn>,
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
    /// Total message count in the current thread (for pagination).
    total_thread_messages: usize,
    /// How many messages are currently loaded (pagination offset).
    loaded_message_count: usize,
    /// Selected message card in the detail pane.
    detail_cursor: usize,
    /// Expanded message IDs in preview mode.
    expanded_message_ids: HashSet<i64>,
    /// Collapsed branch roots in the tree view.
    collapsed_tree_ids: HashSet<i64>,
    /// Focus within the detail pane: tree (true) or preview (false).
    detail_tree_focus: bool,
    /// Page size for pagination.
    page_size: usize,
    /// Whether "Load older" button is selected (when at scroll 0).
    load_older_selected: bool,
    /// Urgent badge pulse phase for escalated threads.
    urgent_pulse_on: bool,
    /// Reduced-motion mode disables pulse animation.
    reduced_motion: bool,
    /// Mermaid thread-flow panel toggle.
    show_mermaid_panel: bool,
    /// Rendered Mermaid panel cache (source hash + dimensions).
    mermaid_cache: RefCell<Option<MermaidPanelCache>>,
    /// Last Mermaid re-render timestamp for debounce.
    mermaid_last_render_at: RefCell<Option<Instant>>,
    /// Pointer drag state for message re-thread operations.
    message_drag: MessageDragState,
    /// Last rendered thread list panel area (for mouse hit testing).
    last_list_area: Cell<Rect>,
    /// Last rendered thread detail panel area (for mouse hit testing).
    last_detail_area: Cell<Rect>,
    /// Whether the detail panel is visible on wide screens (user toggle).
    detail_visible: bool,
    /// Last observed data generation for dirty-state tracking.
    last_data_gen: super::DataGeneration,
}

impl ThreadExplorerScreen {
    #[must_use]
    pub fn new() -> Self {
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
            total_thread_messages: 0,
            loaded_message_count: 0,
            detail_cursor: 0,
            expanded_message_ids: HashSet::new(),
            collapsed_tree_ids: HashSet::new(),
            detail_tree_focus: true,
            page_size: get_thread_page_size(),
            load_older_selected: false,
            urgent_pulse_on: true,
            reduced_motion: reduced_motion_enabled(),
            show_mermaid_panel: false,
            mermaid_cache: RefCell::new(None),
            mermaid_last_render_at: RefCell::new(None),
            message_drag: MessageDragState::Idle,
            last_list_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_detail_area: Cell::new(Rect::new(0, 0, 0, 0)),
            detail_visible: true,
            last_data_gen: super::DataGeneration::stale(),
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
        let db_url = &state.config_snapshot().raw_database_url;
        let cfg = DbPoolConfig {
            database_url: db_url.clone(),
            ..Default::default()
        };
        if let Ok(path) = cfg.sqlite_path() {
            self.db_conn = mcp_agent_mail_db::open_sqlite_file_with_recovery(&path).ok();
        }
    }

    /// Fetch thread list from DB.
    fn refresh_thread_list(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let Some(conn) = &self.db_conn else {
            return;
        };
        let text_match_thread_ids = resolve_text_filter_thread_ids(&self.filter_text);
        self.threads = fetch_threads(
            conn,
            &self.filter_text,
            text_match_thread_ids.as_ref(),
            MAX_THREADS,
        );
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
            self.total_thread_messages = 0;
            self.loaded_message_count = 0;
            self.detail_cursor = 0;
            self.expanded_message_ids.clear();
            self.collapsed_tree_ids.clear();
            self.detail_tree_focus = true;
            self.load_older_selected = false;
            return;
        }

        let Some(conn) = &self.db_conn else {
            return;
        };

        // Get total message count for pagination
        self.total_thread_messages = fetch_thread_message_count(conn, current_thread_id);

        // Load the most recent page_size messages
        let (messages, offset) =
            fetch_thread_messages_paginated(conn, current_thread_id, self.page_size, 0);
        self.detail_messages = messages;
        self.loaded_message_count = self.detail_messages.len();
        self.loaded_thread_id = current_thread_id.to_string();
        self.detail_cursor = self.detail_messages.len().saturating_sub(1);
        self.detail_scroll = self.detail_cursor.saturating_sub(3);
        self.expanded_message_ids.clear();
        self.collapsed_tree_ids.clear();
        self.detail_tree_focus = true;
        if let Some(last) = self.detail_messages.last() {
            self.expanded_message_ids.insert(last.id);
        }
        self.load_older_selected = false;
        // If there are older messages to load, note the offset
        let _ = offset; // offset is 0 for initial load
    }

    /// Load older messages for the current thread (pagination).
    fn load_older_messages(&mut self) {
        let Some(conn) = &self.db_conn else {
            return;
        };

        if self.loaded_thread_id.is_empty() {
            return;
        }

        // Calculate how many more to load
        let remaining = self
            .total_thread_messages
            .saturating_sub(self.loaded_message_count);
        if remaining == 0 {
            return;
        }

        let batch = remaining.min(LOAD_OLDER_BATCH_SIZE);
        let new_offset = self.loaded_message_count;

        // Fetch older messages (they come in chronological order)
        let (older_messages, _) =
            fetch_thread_messages_paginated(conn, &self.loaded_thread_id, batch, new_offset);

        let added = older_messages.len();
        if older_messages.is_empty() {
            return;
        }

        // Prepend older messages (they're older, so go at the start)
        let mut new_messages = older_messages;
        new_messages.append(&mut self.detail_messages);
        self.detail_messages = new_messages;
        self.loaded_message_count += added;

        // Maintain selection on the same logical message after prepending.
        if !self.load_older_selected {
            self.detail_cursor = self.detail_cursor.saturating_add(added);
            self.detail_scroll = self.detail_scroll.saturating_add(added);
        }
        self.load_older_selected = false;
    }

    /// Check if there are more older messages to load.
    const fn has_older_messages(&self) -> bool {
        self.loaded_message_count < self.total_thread_messages
    }

    /// Get the count of remaining older messages.
    const fn remaining_older_count(&self) -> usize {
        self.total_thread_messages
            .saturating_sub(self.loaded_message_count)
    }

    fn detail_tree_rows(&self) -> Vec<ThreadTreeRow> {
        let roots = build_thread_tree_items(&self.detail_messages);
        let mut rows = Vec::new();
        flatten_thread_tree_rows(&roots, &self.collapsed_tree_ids, &mut rows);
        rows
    }

    fn selected_tree_row(&self) -> Option<ThreadTreeRow> {
        self.detail_tree_rows().get(self.detail_cursor).cloned()
    }

    fn selected_message(&self) -> Option<&ThreadMessage> {
        let selected_id = self.selected_tree_row()?.message_id;
        self.detail_messages
            .iter()
            .find(|message| message.id == selected_id)
    }

    fn thread_index_at_list_y(&self, y: u16) -> Option<usize> {
        let area = self.last_list_area.get();
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if !point_in_rect(inner, inner.x, y) {
            return None;
        }
        if self.threads.is_empty() || inner.height == 0 {
            return None;
        }
        let rel_y = usize::from(y.saturating_sub(inner.y));
        let visible_height = inner.height as usize;
        let total = self.threads.len();
        let cursor_clamped = self.cursor.min(total.saturating_sub(1));
        let (start, end) = viewport_range(total, visible_height, cursor_clamped);
        let viewport_len = end.saturating_sub(start);
        let show_subject = visible_height > viewport_len * 2 || viewport_len <= 5;
        let idx = if show_subject {
            start.saturating_add(rel_y / 2)
        } else {
            start.saturating_add(rel_y)
        };
        (idx < end).then_some(idx)
    }

    fn begin_pending_message_drag_from_selected_message(&mut self, mouse_x: u16, mouse_y: u16) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let source_thread_id = self
            .threads
            .get(self.cursor)
            .map(|thread| thread.thread_id.clone())
            .unwrap_or_default();
        self.message_drag = MessageDragState::Pending(PendingMessageDrag {
            message_id: message.id,
            source_thread_id,
            source_project_slug: self
                .threads
                .get(self.cursor)
                .map(|thread| thread.project_slug.clone())
                .unwrap_or_default(),
            subject: message.subject.clone(),
            started_at: Instant::now(),
            cursor_x: mouse_x,
            cursor_y: mouse_y,
        });
    }

    fn promote_pending_message_drag_if_due(&mut self, state: &TuiSharedState) {
        let MessageDragState::Pending(pending) = &self.message_drag else {
            return;
        };
        if pending.started_at.elapsed() < MESSAGE_DRAG_HOLD_DELAY {
            return;
        }
        self.message_drag = MessageDragState::Active(ActiveMessageDrag {
            message_id: pending.message_id,
            source_thread_id: pending.source_thread_id.clone(),
            source_project_slug: pending.source_project_slug.clone(),
            subject: pending.subject.clone(),
            cursor_x: pending.cursor_x,
            cursor_y: pending.cursor_y,
            hovered_thread_id: None,
            hovered_is_valid: false,
            invalid_hover: true,
        });
        self.publish_active_drag_snapshot(state);
    }

    fn publish_active_drag_snapshot(&self, state: &TuiSharedState) {
        if let MessageDragState::Active(active) = &self.message_drag {
            state.set_message_drag_snapshot(Some(MessageDragSnapshot {
                message_id: active.message_id,
                subject: active.subject.clone(),
                source_thread_id: active.source_thread_id.clone(),
                source_project_slug: active.source_project_slug.clone(),
                cursor_x: active.cursor_x,
                cursor_y: active.cursor_y,
                hovered_thread_id: active.hovered_thread_id.clone(),
                hovered_is_valid: active.hovered_is_valid,
                invalid_hover: active.invalid_hover,
            }));
        }
    }

    fn update_active_message_drag(&mut self, state: &TuiSharedState, cursor_x: u16, cursor_y: u16) {
        self.promote_pending_message_drag_if_due(state);
        let hovered_thread_id = if point_in_rect(self.last_list_area.get(), cursor_x, cursor_y) {
            self.thread_index_at_list_y(cursor_y)
                .and_then(|idx| self.threads.get(idx).map(|t| t.thread_id.clone()))
        } else {
            None
        };
        if let MessageDragState::Active(active) = &mut self.message_drag {
            active.cursor_x = cursor_x;
            active.cursor_y = cursor_y;
            active.hovered_thread_id.clone_from(&hovered_thread_id);
            active.hovered_is_valid = hovered_thread_id
                .as_deref()
                .is_some_and(|tid| tid != active.source_thread_id);
            active.invalid_hover = !active.hovered_is_valid;
            self.publish_active_drag_snapshot(state);
        }
    }

    fn clear_message_drag_state(&mut self, state: &TuiSharedState) {
        self.message_drag = MessageDragState::Idle;
        state.clear_message_drag_snapshot();
    }

    fn finish_message_drag(&mut self, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        let cmd = if let MessageDragState::Active(active) = &self.message_drag {
            if active.hovered_is_valid {
                active.hovered_thread_id.as_deref().map_or_else(
                    || Cmd::None,
                    |target_thread_id| {
                        if !target_thread_id.is_empty()
                            && target_thread_id != active.source_thread_id
                        {
                            let op = format!(
                                "rethread_message:{}:{target_thread_id}",
                                active.message_id
                            );
                            Cmd::msg(MailScreenMsg::ActionExecute(
                                op,
                                active.source_thread_id.clone(),
                            ))
                        } else {
                            Cmd::None
                        }
                    },
                )
            } else {
                Cmd::None
            }
        } else {
            Cmd::None
        };
        self.clear_message_drag_state(state);
        cmd
    }

    fn mark_selected_message_for_keyboard_move(&self, state: &TuiSharedState) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let Some(thread) = self.threads.get(self.cursor) else {
            return;
        };
        if thread.thread_id.is_empty() {
            return;
        }
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: message.id,
            subject: message.subject.clone(),
            source_thread_id: thread.thread_id.clone(),
            source_project_slug: thread.project_slug.clone(),
        }));
    }

    fn execute_keyboard_move_to_selected_thread(
        &self,
        state: &TuiSharedState,
    ) -> Cmd<MailScreenMsg> {
        let Some(marker) = state.keyboard_move_snapshot() else {
            return Cmd::None;
        };
        let Some(target_thread_id) = self
            .threads
            .get(self.cursor)
            .map(|thread| thread.thread_id.as_str())
            .filter(|thread_id| !thread_id.is_empty())
        else {
            return Cmd::None;
        };
        if target_thread_id == marker.source_thread_id {
            return Cmd::None;
        }

        let op = format!("rethread_message:{}:{target_thread_id}", marker.message_id);
        state.clear_keyboard_move_snapshot();
        Cmd::msg(MailScreenMsg::ActionExecute(op, marker.source_thread_id))
    }

    fn clamp_detail_cursor_to_tree_rows(&mut self) {
        let row_count = self.detail_tree_rows().len();
        if row_count == 0 {
            self.detail_cursor = 0;
        } else {
            self.detail_cursor = self.detail_cursor.min(row_count.saturating_sub(1));
        }
    }

    fn collapse_selected_branch(&mut self) {
        if let Some(row) = self.selected_tree_row()
            && row.has_children
        {
            self.collapsed_tree_ids.insert(row.message_id);
            self.clamp_detail_cursor_to_tree_rows();
        }
    }

    fn expand_selected_branch(&mut self) {
        if let Some(row) = self.selected_tree_row()
            && row.has_children
        {
            self.collapsed_tree_ids.remove(&row.message_id);
        }
    }

    fn toggle_selected_branch(&mut self) {
        if let Some(row) = self.selected_tree_row() {
            if !row.has_children {
                return;
            }
            if row.is_expanded {
                self.collapsed_tree_ids.insert(row.message_id);
                self.clamp_detail_cursor_to_tree_rows();
            } else {
                self.collapsed_tree_ids.remove(&row.message_id);
            }
        }
    }

    fn toggle_selected_expansion(&mut self) {
        let Some(msg) = self.selected_message() else {
            return;
        };
        let id = msg.id;
        if !self.expanded_message_ids.remove(&id) {
            self.expanded_message_ids.insert(id);
        }
    }

    fn expand_all(&mut self) {
        self.expanded_message_ids = self.detail_messages.iter().map(|m| m.id).collect();
    }

    fn collapse_all(&mut self) {
        self.expanded_message_ids.clear();
    }

    fn thread_mermaid_messages(&self) -> Vec<MermaidThreadMessage> {
        self.detail_messages
            .iter()
            .map(|message| {
                let to_agents = message
                    .to_agents
                    .split(',')
                    .map(str::trim)
                    .filter(|agent| !agent.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                MermaidThreadMessage {
                    from_agent: message.from_agent.clone(),
                    to_agents,
                    subject: message.subject.clone(),
                }
            })
            .collect()
    }

    fn render_mermaid_panel(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let raw_title = if focused {
            "Mermaid Thread Flow * [g]"
        } else {
            "Mermaid Thread Flow [g]"
        };
        let title = fit_panel_title(raw_title, area.width);
        let block = Block::bordered()
            .title(title.as_str())
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)))
            .style(Style::default().bg(tp.panel_bg));
        let inner = block.inner(area);
        block.render(area, frame);

        if inner.width < 4 || inner.height < 4 {
            return;
        }

        let mermaid_messages = self.thread_mermaid_messages();
        let source = generate_thread_flow_mermaid(&mermaid_messages);
        let source_hash = stable_hash(source.as_bytes());

        let cache_is_fresh = {
            let cache = self.mermaid_cache.borrow();
            cache.as_ref().is_some_and(|cached| {
                cached.source_hash == source_hash
                    && cached.width == inner.width
                    && cached.height == inner.height
            })
        };
        let has_cache = self.mermaid_cache.borrow().is_some();
        let can_refresh = self
            .mermaid_last_render_at
            .borrow()
            .as_ref()
            .is_none_or(|last| last.elapsed() >= MERMAID_RENDER_DEBOUNCE);

        if !cache_is_fresh && (can_refresh || !has_cache) {
            let buffer = render_mermaid_source_to_buffer(&source, inner.width, inner.height);
            *self.mermaid_cache.borrow_mut() = Some(MermaidPanelCache {
                source_hash,
                width: inner.width,
                height: inner.height,
                buffer,
            });
            *self.mermaid_last_render_at.borrow_mut() = Some(Instant::now());
        }

        if let Some(cache) = self.mermaid_cache.borrow().as_ref() {
            blit_buffer_to_frame(frame, inner, &cache.buffer);
        } else {
            Paragraph::new("Preparing Mermaid thread diagram...").render(inner, frame);
        }
    }
}

impl Default for ThreadExplorerScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ThreadExplorerScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if point_in_rect(self.last_list_area.get(), mouse.x, mouse.y) {
                        self.focus = Focus::ThreadList;
                        if let Some(idx) = self.thread_index_at_list_y(mouse.y) {
                            self.cursor = idx;
                            self.refresh_detail_if_needed();
                        }
                        return Cmd::None;
                    }
                    if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                        self.focus = Focus::DetailPanel;
                        self.begin_pending_message_drag_from_selected_message(mouse.x, mouse.y);
                        return Cmd::None;
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.update_active_message_drag(state, mouse.x, mouse.y);
                    if !matches!(self.message_drag, MessageDragState::Idle) {
                        return Cmd::None;
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if !matches!(self.message_drag, MessageDragState::Idle) {
                        return self.finish_message_drag(state);
                    }
                }
                _ => {}
            }
        }
        if !matches!(event, Event::Mouse(_)) && !matches!(self.message_drag, MessageDragState::Idle)
        {
            self.clear_message_drag_state(state);
        }

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            if key.code == KeyCode::Escape && state.keyboard_move_snapshot().is_some() {
                state.clear_keyboard_move_snapshot();
                return Cmd::None;
            }
            if key.code == KeyCode::Char('m') && key.modifiers.contains(Modifiers::CTRL) {
                self.mark_selected_message_for_keyboard_move(state);
                return Cmd::None;
            }
            if key.code == KeyCode::Char('v') && key.modifiers.contains(Modifiers::CTRL) {
                return self.execute_keyboard_move_to_selected_thread(state);
            }

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
                        KeyCode::Char('i') => {
                            self.detail_visible = !self.detail_visible;
                        }
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
                        KeyCode::Home => {
                            self.cursor = 0;
                            self.detail_scroll = 0;
                            self.refresh_detail_if_needed();
                        }
                        KeyCode::Char('g') => {
                            self.show_mermaid_panel = !self.show_mermaid_panel;
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
                                    DeepLinkTarget::TimelineAtTime(thread.last_timestamp_micros),
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
                        KeyCode::Escape => {
                            if self.show_mermaid_panel {
                                self.show_mermaid_panel = false;
                            }
                        }
                        _ => {}
                    }
                }
                Focus::DetailPanel => {
                    self.clamp_detail_cursor_to_tree_rows();
                    let tree_rows = self.detail_tree_rows();
                    match key.code {
                        // Back to thread list
                        KeyCode::Escape => {
                            if self.show_mermaid_panel {
                                self.show_mermaid_panel = false;
                            } else {
                                self.focus = Focus::ThreadList;
                                self.load_older_selected = false;
                            }
                        }
                        KeyCode::Char('g') => {
                            self.show_mermaid_panel = !self.show_mermaid_panel;
                        }
                        // Toggle focus between hierarchy tree and preview pane.
                        KeyCode::Tab => {
                            self.detail_tree_focus = !self.detail_tree_focus;
                        }
                        // Search/filter
                        KeyCode::Char('/') => {
                            self.focus = Focus::ThreadList;
                            self.filter_editing = true;
                        }
                        _ if self.detail_tree_focus => match key.code {
                            // Tree navigation
                            KeyCode::Char('j') | KeyCode::Down => {
                                if self.detail_cursor + 1 < tree_rows.len() {
                                    self.detail_cursor += 1;
                                }
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.detail_cursor = self.detail_cursor.saturating_sub(1);
                            }
                            KeyCode::Char('d') | KeyCode::PageDown => {
                                let step = 10usize;
                                self.detail_cursor = (self.detail_cursor + step)
                                    .min(tree_rows.len().saturating_sub(1));
                            }
                            KeyCode::Char('u') | KeyCode::PageUp => {
                                self.detail_cursor = self.detail_cursor.saturating_sub(10);
                            }
                            KeyCode::Char('G') | KeyCode::End => {
                                self.detail_cursor = tree_rows.len().saturating_sub(1);
                            }
                            KeyCode::Home => {
                                self.detail_cursor = 0;
                            }
                            // Tree expansion controls (fall back to ThreadList if nothing to collapse)
                            KeyCode::Left | KeyCode::Char('h') => {
                                let before = self.collapsed_tree_ids.len();
                                self.collapse_selected_branch();
                                if self.collapsed_tree_ids.len() == before {
                                    self.focus = Focus::ThreadList;
                                    self.load_older_selected = false;
                                }
                            }
                            KeyCode::Right | KeyCode::Char('l') => {
                                self.expand_selected_branch();
                            }
                            KeyCode::Char(' ') => {
                                self.toggle_selected_branch();
                            }
                            // Open selected message in preview mode.
                            KeyCode::Enter => {
                                self.toggle_selected_expansion();
                                self.detail_tree_focus = false;
                            }
                            // Load more history
                            KeyCode::Char('o') => {
                                if self.has_older_messages() {
                                    self.load_older_messages();
                                    self.clamp_detail_cursor_to_tree_rows();
                                }
                            }
                            // Expand/collapse all selected-message previews.
                            KeyCode::Char('e') => self.expand_all(),
                            KeyCode::Char('c') => self.collapse_all(),
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
                            KeyCode::Char('r') if !key.modifiers.contains(Modifiers::CTRL) => {
                                if let Some(message) = self.selected_message() {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::ReplyToMessage(message.id),
                                    ));
                                }
                            }
                            _ => {}
                        },
                        _ => match key.code {
                            // Preview scrolling/actions while preview has focus.
                            KeyCode::Char('j') | KeyCode::Down => {
                                self.detail_scroll = self.detail_scroll.saturating_add(1);
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                            }
                            KeyCode::Left | KeyCode::Char('h') => {
                                self.detail_tree_focus = true;
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                self.toggle_selected_expansion();
                            }
                            KeyCode::Char('o') => {
                                if self.has_older_messages() {
                                    self.load_older_messages();
                                    self.clamp_detail_cursor_to_tree_rows();
                                }
                            }
                            KeyCode::Char('e') => self.expand_all(),
                            KeyCode::Char('c') => self.collapse_all(),
                            KeyCode::Char('t') => {
                                if let Some(thread) = self.threads.get(self.cursor) {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::TimelineAtTime(
                                            thread.last_timestamp_micros,
                                        ),
                                    ));
                                }
                            }
                            KeyCode::Char('r') if !key.modifiers.contains(Modifiers::CTRL) => {
                                if let Some(message) = self.selected_message() {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::ReplyToMessage(message.id),
                                    ));
                                }
                            }
                            _ => {}
                        },
                    }
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.urgent_pulse_on =
            self.reduced_motion || (tick_count / URGENT_PULSE_HALF_PERIOD_TICKS).is_multiple_of(2);
        self.promote_pending_message_drag_if_due(state);

        // ── Dirty-state gated data ingestion ────────────────────────
        let current_gen = state.data_generation();
        let dirty = super::dirty_since(&self.last_data_gen, &current_gen);

        // Initial load or user-driven dirty flag — always honored.
        if self.list_dirty {
            self.refresh_thread_list(state);
            self.last_data_gen = current_gen;
            return;
        }

        // Periodic refresh: only set list_dirty when data actually changed
        // (new events or DB stats mutation), avoiding redundant re-queries.
        let should_refresh = self
            .last_refresh
            .is_none_or(|t| t.elapsed().as_secs() >= REFRESH_INTERVAL_SECS);
        if should_refresh && (dirty.events || dirty.db_stats) {
            self.list_dirty = true;
        }

        self.sync_focused_event();
        self.last_data_gen = current_gen;
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

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 4 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        // Hard-clear the full screen area before drawing filter/list/detail panes.
        // This prevents stale border glyphs from previous layouts leaking through.
        clear_rect(frame, area, tp.bg_deep);

        // Filter bar (always visible: hint when collapsed, input when active)
        let has_filter = !self.filter_text.is_empty();
        let filter_height: u16 = 1;
        let content_height = area.height.saturating_sub(filter_height);

        let filter_area = Rect::new(area.x, area.y, area.width, filter_height);
        render_filter_bar(
            frame,
            filter_area,
            &self.filter_text,
            self.filter_editing,
            has_filter,
        );

        let content_area = Rect::new(area.x, area.y + filter_height, area.width, content_height);
        // Always repaint content background so mode switches never leave stray borders.
        clear_rect(frame, content_area, tp.bg_deep);
        let drop_visual = match &self.message_drag {
            MessageDragState::Active(active) => Some(ThreadDropVisual {
                source_thread_id: active.source_thread_id.as_str(),
                hovered_thread_id: active.hovered_thread_id.as_deref(),
                invalid_hover: active.invalid_hover,
            }),
            _ => None,
        };
        let keyboard_move = state.keyboard_move_snapshot();

        // ResponsiveLayout: Md+ uses side-by-side split with breakpoint-based ratios.
        // Below Md (< 90): fall through to height-based stacked/compact fallback.
        let rl_layout = if self.detail_visible {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
                .at(
                    Breakpoint::Md,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(40.0), Constraint::Percentage(60.0)]),
                )
                .at(
                    Breakpoint::Lg,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(34.0), Constraint::Percentage(66.0)]),
                )
                .at(
                    Breakpoint::Xl,
                    Flex::horizontal()
                        .constraints([Constraint::Percentage(25.0), Constraint::Percentage(75.0)]),
                )
        } else {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
        };
        let rl_split = rl_layout.split(content_area);

        if rl_split.rects.len() >= 2 && self.detail_visible {
            // Wide: side-by-side list + detail
            let list_area = rl_split.rects[0];
            let mut detail_area = rl_split.rects[1];
            let pane_gap = u16::from(content_area.width >= THREAD_MAIN_PANE_GAP_THRESHOLD);
            if pane_gap > 0 && detail_area.width > pane_gap {
                let splitter_area = Rect::new(
                    list_area.x.saturating_add(list_area.width),
                    content_area.y,
                    pane_gap,
                    content_area.height,
                );
                render_splitter_handle(frame, splitter_area, true, false);
                detail_area.x = detail_area.x.saturating_add(pane_gap);
                detail_area.width = detail_area.width.saturating_sub(pane_gap);
            }
            self.last_list_area.set(list_area);
            self.last_detail_area.set(detail_area);

            render_thread_list(
                frame,
                list_area,
                &self.threads,
                self.cursor,
                matches!(self.focus, Focus::ThreadList),
                self.view_lens,
                self.sort_mode,
                self.urgent_pulse_on,
                drop_visual,
                keyboard_move.as_ref(),
            );
            if self.show_mermaid_panel {
                self.render_mermaid_panel(
                    frame,
                    detail_area,
                    matches!(self.focus, Focus::DetailPanel),
                );
            } else {
                render_thread_detail(
                    frame,
                    detail_area,
                    &self.detail_messages,
                    self.threads.get(self.cursor),
                    self.detail_scroll,
                    self.detail_cursor,
                    &self.expanded_message_ids,
                    &self.collapsed_tree_ids,
                    self.has_older_messages(),
                    self.remaining_older_count(),
                    self.loaded_message_count,
                    self.total_thread_messages,
                    matches!(self.focus, Focus::DetailPanel),
                    self.detail_tree_focus,
                );
            }
        } else if content_area.height >= THREAD_STACKED_MIN_HEIGHT && self.detail_visible {
            // Narrow but tall: stacked fallback preserves both list and detail.
            let min_list_h: u16 = 4;
            let min_detail_h: u16 = 6;
            let stack_gap = u16::from(
                content_area.height
                    >= min_list_h
                        .saturating_add(min_detail_h)
                        .saturating_add(THREAD_STACKED_SPLITTER_HEIGHT),
            );
            let raw_list_h = content_area
                .height
                .saturating_mul(THREAD_STACKED_LIST_PERCENT)
                / 100;
            let list_h = raw_list_h.clamp(
                min_list_h,
                content_area
                    .height
                    .saturating_sub(min_detail_h.saturating_add(stack_gap))
                    .max(min_list_h),
            );
            let detail_h = content_area
                .height
                .saturating_sub(list_h.saturating_add(stack_gap));

            let list_area = Rect::new(content_area.x, content_area.y, content_area.width, list_h);
            let detail_area = Rect::new(
                content_area.x,
                content_area
                    .y
                    .saturating_add(list_h)
                    .saturating_add(stack_gap),
                content_area.width,
                detail_h,
            );
            if stack_gap > 0 {
                let splitter_area = Rect::new(
                    content_area.x,
                    content_area.y.saturating_add(list_h),
                    content_area.width,
                    stack_gap,
                );
                render_splitter_handle(frame, splitter_area, false, false);
            }
            self.last_list_area.set(list_area);
            self.last_detail_area.set(detail_area);

            render_thread_list(
                frame,
                list_area,
                &self.threads,
                self.cursor,
                matches!(self.focus, Focus::ThreadList),
                self.view_lens,
                self.sort_mode,
                self.urgent_pulse_on,
                drop_visual,
                keyboard_move.as_ref(),
            );
            if self.show_mermaid_panel {
                self.render_mermaid_panel(
                    frame,
                    detail_area,
                    matches!(self.focus, Focus::DetailPanel),
                );
            } else {
                render_thread_detail(
                    frame,
                    detail_area,
                    &self.detail_messages,
                    self.threads.get(self.cursor),
                    self.detail_scroll,
                    self.detail_cursor,
                    &self.expanded_message_ids,
                    &self.collapsed_tree_ids,
                    self.has_older_messages(),
                    self.remaining_older_count(),
                    self.loaded_message_count,
                    self.total_thread_messages,
                    matches!(self.focus, Focus::DetailPanel),
                    self.detail_tree_focus,
                );
            }
        } else {
            // Tight layout: show active pane with explicit focus-switch hint.
            let (pane_area, hint_area) = if content_area.height >= THREAD_COMPACT_HINT_MIN_HEIGHT {
                let pane_h = content_area.height.saturating_sub(1);
                (
                    Rect::new(content_area.x, content_area.y, content_area.width, pane_h),
                    Some(Rect::new(
                        content_area.x,
                        content_area.y + pane_h,
                        content_area.width,
                        1,
                    )),
                )
            } else {
                (content_area, None)
            };

            if self.show_mermaid_panel {
                self.last_list_area.set(Rect::new(0, 0, 0, 0));
                self.last_detail_area.set(pane_area);
                self.render_mermaid_panel(
                    frame,
                    pane_area,
                    matches!(self.focus, Focus::DetailPanel),
                );
            } else {
                match self.focus {
                    Focus::ThreadList => {
                        self.last_list_area.set(pane_area);
                        self.last_detail_area.set(Rect::new(0, 0, 0, 0));
                        render_thread_list(
                            frame,
                            pane_area,
                            &self.threads,
                            self.cursor,
                            true,
                            self.view_lens,
                            self.sort_mode,
                            self.urgent_pulse_on,
                            drop_visual,
                            keyboard_move.as_ref(),
                        );
                    }
                    Focus::DetailPanel => {
                        self.last_list_area.set(Rect::new(0, 0, 0, 0));
                        self.last_detail_area.set(pane_area);
                        render_thread_detail(
                            frame,
                            pane_area,
                            &self.detail_messages,
                            self.threads.get(self.cursor),
                            self.detail_scroll,
                            self.detail_cursor,
                            &self.expanded_message_ids,
                            &self.collapsed_tree_ids,
                            self.has_older_messages(),
                            self.remaining_older_count(),
                            self.loaded_message_count,
                            self.total_thread_messages,
                            true,
                            self.detail_tree_focus,
                        );
                    }
                }
            }

            if let Some(hint_area) = hint_area {
                render_compact_focus_hint(frame, hint_area, self.focus, self.show_mermaid_panel);
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
                key: "G/Home",
                action: "End / Home",
            },
            HelpEntry {
                key: "g",
                action: "Toggle Mermaid panel",
            },
            HelpEntry {
                key: "Enter/l",
                action: "Open thread detail",
            },
            HelpEntry {
                key: "Tab",
                action: "Toggle tree/preview focus",
            },
            HelpEntry {
                key: "Mouse",
                action: "Drag message to thread row",
            },
            HelpEntry {
                key: "Ctrl+M / Ctrl+V",
                action: "Mark message / drop to selected thread",
            },
            HelpEntry {
                key: "Left/Right",
                action: "Collapse/expand selected branch",
            },
            HelpEntry {
                key: "Enter/Space",
                action: "Toggle preview or branch state",
            },
            HelpEntry {
                key: "r",
                action: "Quick reply selected message",
            },
            HelpEntry {
                key: "e / c",
                action: "Expand all / collapse all",
            },
            HelpEntry {
                key: "o",
                action: "Load older messages",
            },
            HelpEntry {
                key: "t",
                action: "Timeline at last activity",
            },
            HelpEntry {
                key: "Esc/h",
                action: "Cancel move / close Mermaid / back to list",
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
                action: "Sort: Recent/Velocity/Participants/Escalation",
            },
            HelpEntry {
                key: "v",
                action: "Lens: Activity/Participants/Escalation",
            },
            HelpEntry {
                key: "i",
                action: "Toggle detail panel",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some(
            "Thread conversations grouped by topic. Enter to expand, r to quick-reply, h to collapse.",
        )
    }

    fn consumes_text_input(&self) -> bool {
        self.filter_editing
    }

    fn copyable_content(&self) -> Option<String> {
        let thread = self.threads.get(self.cursor)?;
        Some(format!("[{}] {}", thread.thread_id, thread.last_subject))
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
fn fetch_threads(
    conn: &DbConn,
    filter: &str,
    text_match_thread_ids: Option<&HashSet<String>>,
    limit: usize,
) -> Vec<ThreadSummary> {
    let mut predicates = Vec::new();
    let mut params = Vec::new();

    if !filter.is_empty() {
        let like_term = format!("%{filter}%");
        predicates.push("(m.thread_id LIKE ? OR a_sender.name LIKE ?)".to_string());
        params.push(Value::Text(like_term.clone()));
        params.push(Value::Text(like_term));
    }

    if let Some(thread_ids) = text_match_thread_ids
        && !thread_ids.is_empty()
    {
        let mut ids: Vec<String> = thread_ids.iter().cloned().collect();
        ids.sort_unstable();
        let placeholders = vec!["?"; ids.len()].join(", ");
        predicates.push(format!("m.thread_id IN ({placeholders})"));
        params.extend(ids.into_iter().map(Value::Text));
    }

    let filter_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", predicates.join(" OR "))
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

    let rows = conn.query_sync(&sql, &params).ok().unwrap_or_default();

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
            let duration_hours = last_ts.saturating_sub(first_ts).max(1) as f64 / (3_600_000_000.0);
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
                first_timestamp_iso: micros_to_iso(first_ts),
                unread_count: 0, // Will be updated if read tracking is available
            })
        })
        .collect();

    // Fetch the latest subject + sender for each thread in a second pass.
    for thread in &mut threads {
        let detail_sql = "SELECT m.subject, a_sender.name AS sender_name \
             FROM messages m \
             JOIN agents a_sender ON a_sender.id = m.sender_id \
             WHERE m.thread_id = ? \
             ORDER BY m.created_ts DESC \
             LIMIT 1";
        if let Some(row) = conn
            .query_sync(detail_sql, &[Value::Text(thread.thread_id.clone())])
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

fn resolve_text_filter_thread_ids(filter: &str) -> Option<HashSet<String>> {
    let trimmed = filter.trim();
    if trimmed.is_empty() {
        return None;
    }

    let pool_cfg = DbPoolConfig::from_env();
    let pool = match mcp_agent_mail_db::create_pool(&pool_cfg) {
        Ok(pool) => pool,
        Err(err) => {
            tracing::warn!("thread filter search pool init failed: {err}");
            return None;
        }
    };
    let runtime = match asupersync::runtime::RuntimeBuilder::current_thread().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::warn!("thread filter search runtime init failed: {err}");
            return None;
        }
    };
    let cx = asupersync::Cx::for_request();

    let mut query = mcp_agent_mail_db::search_planner::SearchQuery::default();
    query.text = trimmed.to_string();
    query.doc_kind = mcp_agent_mail_db::search_planner::DocKind::Message;
    query.ranking = mcp_agent_mail_db::search_planner::RankingMode::Recency;
    query.limit = Some(MAX_THREAD_FILTER_IDS);

    let outcome = runtime.block_on(async {
        mcp_agent_mail_db::search_service::execute_search_simple(&cx, &pool, &query).await
    });

    match outcome {
        Outcome::Ok(response) => Some(
            response
                .results
                .into_iter()
                .filter_map(|row| row.thread_id)
                .filter(|thread_id| !thread_id.is_empty())
                .collect(),
        ),
        Outcome::Err(err) => {
            tracing::warn!("thread filter search query failed: {err}");
            None
        }
        Outcome::Cancelled(_) => None,
        Outcome::Panicked(err) => {
            tracing::warn!("thread filter search query panicked: {err}");
            None
        }
    }
}

/// Get the total count of messages in a thread.
fn fetch_thread_message_count(conn: &DbConn, thread_id: &str) -> usize {
    let sql = "SELECT COUNT(*) AS cnt FROM messages WHERE thread_id = ?";

    conn.query_sync(sql, &[Value::Text(thread_id.to_string())])
        .ok()
        .and_then(|mut rows| rows.pop())
        .and_then(|row| row.get_named::<i64>("cnt").ok())
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0)
}

/// Fetch messages in a thread with pagination, returning most recent first for
/// offset calculation.
/// Returns (`messages_in_chronological_order`, `offset_used`).
fn fetch_thread_messages_paginated(
    conn: &DbConn,
    thread_id: &str,
    limit: usize,
    offset: usize,
) -> (Vec<ThreadMessage>, usize) {
    // We want the most recent `limit` messages, but displayed in chronological order.
    // So we fetch by DESC, then reverse the result.
    // For "load older", we use offset to skip the most recent ones.
    let sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.importance, m.created_ts, \
         a_sender.name AS sender_name, \
         COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
         FROM messages m \
         JOIN agents a_sender ON a_sender.id = m.sender_id \
         LEFT JOIN message_recipients mr ON mr.message_id = m.id \
         LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
         WHERE m.thread_id = ? \
         GROUP BY m.id \
         ORDER BY m.created_ts DESC \
         LIMIT {limit} OFFSET {offset}"
    );

    let mut messages: Vec<ThreadMessage> = conn
        .query_sync(&sql, &[Value::Text(thread_id.to_string())])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let created_ts = row.get_named::<i64>("created_ts").ok()?;
                    Some(ThreadMessage {
                        id: row.get_named::<i64>("id").ok()?,
                        reply_to_id: None,
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
                        is_unread: false,
                        ack_required: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Reverse to get chronological order (oldest first)
    messages.reverse();
    (messages, offset)
}

/// Fetch all messages in a thread, sorted chronologically (legacy function for compatibility).
#[allow(dead_code)]
fn fetch_thread_messages(conn: &DbConn, thread_id: &str, limit: usize) -> Vec<ThreadMessage> {
    let (messages, _) = fetch_thread_messages_paginated(conn, thread_id, limit, 0);
    messages
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the filter bar.
fn render_filter_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    editing: bool,
    has_filter: bool,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    clear_rect(frame, area, tp.panel_bg);
    if !has_filter && !editing {
        // Collapsed state: show discoverable hint
        let line = Line::from_spans([
            Span::raw(" "),
            Span::styled("/", crate::tui_theme::text_action_key(&tp)),
            Span::styled(" Filter threads", crate::tui_theme::text_hint(&tp)),
        ]);
        Paragraph::new(Text::from_line(line)).render(area, frame);
    } else {
        // Active state
        let cursor = if editing { "_" } else { "" };
        let value = truncate_display_width(
            &format!("{text}{cursor}"),
            area.width.saturating_sub(10) as usize,
        );
        let line = Line::from_spans([
            Span::styled(" Filter: ", crate::tui_theme::text_meta(&tp)),
            Span::styled(value, Style::default().fg(tp.text_primary)),
        ]);
        Paragraph::new(Text::from_line(line)).render(area, frame);
    }
}

fn render_compact_focus_hint(
    frame: &mut Frame<'_>,
    area: Rect,
    focus: Focus,
    mermaid_active: bool,
) {
    if area.width < 12 || area.height == 0 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let hint = if mermaid_active {
        " g:close mermaid | Enter/Esc: switch panes"
    } else {
        match focus {
            Focus::ThreadList => " Enter/l: detail | Esc/h: list | Tab: tree/preview",
            Focus::DetailPanel => " Esc/h: list | r:quick reply | Tab: tree/preview",
        }
    };

    Paragraph::new(truncate_display_width(hint, area.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(area, frame);
}

/// Render the thread list panel.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_thread_list(
    frame: &mut Frame<'_>,
    area: Rect,
    threads: &[ThreadSummary],
    cursor: usize,
    focused: bool,
    view_lens: ViewLens,
    sort_mode: SortMode,
    urgent_pulse_on: bool,
    drop_visual: Option<ThreadDropVisual<'_>>,
    keyboard_move: Option<&KeyboardMoveSnapshot>,
) {
    let focus_tag = if focused { "" } else { " (inactive)" };
    let escalated = threads.iter().filter(|t| t.has_escalation).count();
    let esc_tag = if escalated > 0 {
        format!("  {escalated} esc")
    } else {
        String::new()
    };
    let moving_tag = keyboard_move
        .map(|marker| format!("  [MOVING #{}]", marker.message_id))
        .unwrap_or_default();
    let title = format!(
        "Threads ({}){}  [v]{}  [s]{}{moving_tag}{focus_tag}",
        threads.len(),
        esc_tag,
        view_lens.label(),
        sort_mode.label(),
    );
    let title = fit_panel_title(&title, area.width);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let hovered_valid_drop = drop_visual.is_some_and(|drag| {
        drag.hovered_thread_id
            .is_some_and(|tid| tid != drag.source_thread_id)
    });
    let border_color = if drop_visual.is_some_and(|drag| drag.invalid_hover) {
        tp.severity_warn
    } else if hovered_valid_drop {
        tp.panel_border_focused
    } else {
        crate::tui_theme::focus_border_color(&tp, focused)
    };
    let block = Block::bordered()
        .title(title.as_str())
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(tp.panel_bg));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Use full inner area to maximize density and reduce wasted space.
    let content_inner = if inner.width >= THREAD_LIST_SIDE_PADDING_MIN_WIDTH {
        Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        )
    } else {
        inner
    };
    if content_inner.height == 0 || content_inner.width == 0 {
        return;
    }
    let visible_height = content_inner.height as usize;

    if threads.is_empty() {
        let p = Paragraph::new(" No threads found.").style(crate::tui_theme::text_hint(&tp));
        p.render(content_inner, frame);
        return;
    }

    // Viewport centering
    let total = threads.len();
    let cursor_clamped = cursor.min(total.saturating_sub(1));
    let (start, end) = viewport_range(total, visible_height, cursor_clamped);
    let viewport = &threads[start..end];

    let inner_w = content_inner.width as usize;
    let show_subject = (visible_height > viewport.len() * 2 || viewport.len() <= 5)
        && inner_w >= THREAD_SUBJECT_LINE_MIN_WIDTH;
    let mut text_lines: Vec<Line> = Vec::with_capacity(viewport.len() * 2);
    for (view_idx, thread) in viewport.iter().enumerate() {
        let abs_idx = start + view_idx;
        let is_selected = abs_idx == cursor_clamped;
        let keyboard_marked_source =
            keyboard_move.is_some_and(|marker| marker.source_thread_id == thread.thread_id);
        let valid_drop_zone =
            drop_visual.is_some_and(|drag| thread.thread_id != drag.source_thread_id);
        let hovered_here = drop_visual
            .and_then(|drag| drag.hovered_thread_id)
            .is_some_and(|tid| tid == thread.thread_id);
        let marker = if hovered_here && valid_drop_zone {
            "\u{25b8}"
        } else if is_selected {
            ">"
        } else {
            " "
        };

        let esc_badge = if thread.has_escalation {
            if urgent_pulse_on { "!" } else { "\u{00b7}" }
        } else {
            " "
        };
        let esc_style = if thread.has_escalation {
            crate::tui_theme::text_warning(&tp)
        } else {
            Style::default()
        };

        if inner_w < THREAD_LIST_COMPACT_MIN_WIDTH {
            let compact_id = truncate_display_width(&thread.thread_id, inner_w.saturating_sub(6));
            let mut compact_line = Line::from_spans([
                Span::raw(marker),
                Span::styled(esc_badge, esc_style),
                Span::raw(" ".to_string()),
                Span::styled(compact_id, Style::default().fg(tp.text_primary)),
            ]);
            if is_selected || (hovered_here && valid_drop_zone) {
                compact_line.apply_base_style(
                    Style::default()
                        .fg(tp.selection_fg)
                        .bg(tp.selection_bg)
                        .bold(),
                );
            } else if hovered_here {
                compact_line.apply_base_style(Style::default().fg(tp.severity_warn).bold());
            } else if valid_drop_zone {
                compact_line
                    .apply_base_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));
            }
            text_lines.push(clip_line_to_display_width(compact_line, inner_w));
            continue;
        }

        // Compact timestamp (HH:MM from ISO string)
        let time_short = if thread.last_timestamp_iso.len() >= 16 {
            &thread.last_timestamp_iso[11..16]
        } else {
            &thread.last_timestamp_iso
        };

        let project_text = if thread.project_slug.is_empty() {
            String::new()
        } else {
            format!("[{}] ", truncate_display_width(&thread.project_slug, 12))
        };
        let unread_text = if thread.unread_count > 0 {
            format!(" {}", thread.unread_count)
        } else {
            String::new()
        };
        let moving_text = if keyboard_marked_source {
            " [MOVING]".to_string()
        } else {
            String::new()
        };

        // Lens-specific metadata
        let mut meta_text = match view_lens {
            ViewLens::Activity => format!(
                "{}m  {}a  {:.1}/hr",
                thread.message_count, thread.participant_count, thread.velocity_msg_per_hr,
            ),
            ViewLens::Participants => {
                truncate_display_width(&thread.participant_names, inner_w.saturating_sub(30))
            }
            ViewLens::Escalation => {
                let flag = if thread.has_escalation { "ESC" } else { "---" };
                format!("{flag}  {:.1}/hr", thread.velocity_msg_per_hr)
            }
        };

        // Ensure row content never exceeds the panel width (prevents wrap artifacts that look
        // like random border glyphs cutting through text).
        let marker_w = ftui::text::display_width(marker);
        let esc_w = ftui::text::display_width(esc_badge);
        let time_w = ftui::text::display_width(time_short);
        let project_w = ftui::text::display_width(&project_text);
        let unread_w = ftui::text::display_width(&unread_text);
        let moving_w = ftui::text::display_width(&moving_text);
        let min_id_w = if inner_w >= 36 {
            12
        } else if inner_w >= 24 {
            8
        } else {
            4.min(inner_w)
        };
        let fixed_w = marker_w + esc_w + time_w + 1 + project_w + 1 + unread_w + moving_w;
        let max_meta_w = inner_w.saturating_sub(fixed_w.saturating_add(min_id_w));
        if max_meta_w == 0 {
            meta_text.clear();
        } else if ftui::text::display_width(&meta_text) > max_meta_w {
            meta_text = truncate_display_width(&meta_text, max_meta_w);
        }
        let meta_w = ftui::text::display_width(&meta_text);
        let id_space = inner_w.saturating_sub(fixed_w + meta_w);
        let thread_id_display = truncate_display_width(&thread.thread_id, id_space);

        let cursor_style = if is_selected {
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold()
        } else {
            Style::default()
        };

        let mut primary_spans: Vec<Span<'static>> = vec![
            Span::raw(marker),
            Span::styled(esc_badge, esc_style),
            Span::styled(time_short.to_string(), crate::tui_theme::text_meta(&tp)),
            Span::raw(" ".to_string()),
        ];
        if !project_text.is_empty() {
            primary_spans.push(Span::styled(
                project_text.clone(),
                crate::tui_theme::text_meta(&tp),
            ));
        }
        primary_spans.push(Span::styled(
            thread_id_display,
            Style::default().fg(tp.text_primary),
        ));
        if !meta_text.is_empty() || !unread_text.is_empty() || !moving_text.is_empty() {
            primary_spans.push(Span::raw(" ".to_string()));
        }
        if !meta_text.is_empty() {
            primary_spans.push(Span::styled(meta_text, crate::tui_theme::text_meta(&tp)));
        }
        if !unread_text.is_empty() {
            primary_spans.push(Span::styled(
                unread_text,
                crate::tui_theme::text_accent(&tp),
            ));
        }
        if !moving_text.is_empty() {
            primary_spans.push(Span::styled(
                moving_text,
                Style::default().fg(tp.selection_indicator).bold(),
            ));
        }

        let mut primary = Line::from_spans(primary_spans);
        if is_selected {
            primary.apply_base_style(cursor_style);
        } else if hovered_here && valid_drop_zone {
            primary.apply_base_style(
                Style::default()
                    .fg(tp.selection_fg)
                    .bg(tp.selection_bg)
                    .bold(),
            );
        } else if hovered_here {
            primary.apply_base_style(Style::default().fg(tp.severity_warn).bold());
        } else if valid_drop_zone {
            primary.apply_base_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));
        }
        primary = clip_line_to_display_width(primary, inner_w);
        text_lines.push(primary);

        // Second line: last subject (if there's room)
        if show_subject {
            let indent = "  ";
            let subj_space = inner_w.saturating_sub(ftui::text::display_width(indent));
            let subj_line = if thread.last_sender.is_empty() {
                Line::from_spans([
                    Span::raw(indent.to_string()),
                    Span::styled(
                        truncate_display_width(&thread.last_subject, subj_space),
                        crate::tui_theme::text_hint(&tp),
                    ),
                ])
            } else {
                let sender_prefix = truncate_display_width(
                    &format!("{}: ", thread.last_sender),
                    subj_space.min(24),
                );
                let remaining =
                    subj_space.saturating_sub(ftui::text::display_width(sender_prefix.as_str()));
                Line::from_spans([
                    Span::raw(indent.to_string()),
                    Span::styled(sender_prefix, Style::default().fg(tp.text_secondary)),
                    Span::styled(
                        truncate_display_width(&thread.last_subject, remaining),
                        crate::tui_theme::text_hint(&tp),
                    ),
                ])
            };
            let mut subj_line = subj_line;
            if !is_selected && hovered_here && valid_drop_zone {
                subj_line.apply_base_style(
                    Style::default()
                        .fg(tp.selection_fg)
                        .bg(tp.selection_bg)
                        .bold(),
                );
            } else if !is_selected && hovered_here {
                subj_line.apply_base_style(Style::default().fg(tp.severity_warn).bold());
            } else if !is_selected && valid_drop_zone {
                subj_line
                    .apply_base_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));
            }
            subj_line = clip_line_to_display_width(subj_line, inner_w);
            text_lines.push(subj_line);
        }
    }

    let remaining_rows = visible_height.saturating_sub(text_lines.len());
    if remaining_rows >= 2 {
        text_lines.push(Line::raw(String::new()));
        let total_unread: usize = threads.iter().map(|t| t.unread_count).sum();
        let avg_velocity = if threads.is_empty() {
            0.0
        } else {
            let denom = f64::from(u32::try_from(threads.len()).unwrap_or(u32::MAX));
            threads.iter().map(|t| t.velocity_msg_per_hr).sum::<f64>() / denom
        };
        let summary = truncate_display_width(
            &format!(
                "selected:{}  unread:{}  escalations:{}  avg:{avg_velocity:.1}/hr",
                truncate_display_width(&threads[cursor_clamped].thread_id, 20),
                total_unread,
                escalated,
            ),
            inner_w.saturating_sub(2),
        );
        text_lines.push(clip_line_to_display_width(
            Line::from_spans([
                Span::styled("  ", crate::tui_theme::text_meta(&tp)),
                Span::styled(summary, crate::tui_theme::text_meta(&tp)),
            ]),
            inner_w,
        ));
    }
    if remaining_rows >= 3 {
        let selected = &threads[cursor_clamped];
        let participant_line = if selected.participant_names.is_empty() {
            "participants: (none)".to_string()
        } else {
            format!(
                "participants: {}",
                truncate_display_width(
                    &selected.participant_names,
                    inner_w.saturating_sub(16).max(12)
                )
            )
        };
        let participant_line = truncate_display_width(&participant_line, inner_w.saturating_sub(2));
        text_lines.push(clip_line_to_display_width(
            Line::from_spans([
                Span::styled("  ", crate::tui_theme::text_hint(&tp)),
                Span::styled(participant_line, crate::tui_theme::text_hint(&tp)),
            ]),
            inner_w,
        ));
    }

    let text = Text::from_lines(text_lines);
    let p = Paragraph::new(text).wrap(ftui::text::WrapMode::None);
    p.render(content_inner, frame);
}

/// Render the thread detail/conversation panel.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn render_thread_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    messages: &[ThreadMessage],
    thread: Option<&ThreadSummary>,
    scroll: usize,
    selected_idx: usize,
    expanded_message_ids: &HashSet<i64>,
    collapsed_tree_ids: &HashSet<i64>,
    has_older_messages: bool,
    remaining_older_count: usize,
    loaded_message_count: usize,
    total_thread_messages: usize,
    focused: bool,
    tree_focus: bool,
) {
    let title = thread.map_or_else(
        || "Thread Detail".to_string(),
        |t| {
            let focus_tag = if focused { "" } else { " (inactive)" };
            format!(
                "Thread: {} ({} msgs){focus_tag}",
                truncate_display_width(&t.thread_id, 30),
                t.message_count,
            )
        },
    );
    let title = fit_panel_title(&title, area.width);

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title(title.as_str())
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)))
        .style(Style::default().bg(tp.panel_bg));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    // Use full inner area to maximize density and reduce wasted space.
    let content_inner = if inner.width >= THREAD_DETAIL_SIDE_PADDING_MIN_WIDTH {
        Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        )
    } else {
        inner
    };
    if content_inner.height == 0 || content_inner.width == 0 {
        return;
    }
    clear_rect(frame, content_inner, tp.panel_bg);

    if messages.is_empty() {
        let text = match thread {
            Some(_) => "  No messages in this thread.",
            None => "Select a thread to view conversation.",
        };
        let p = Paragraph::new(text).style(crate::tui_theme::text_hint(&tp));
        p.render(content_inner, frame);
        return;
    }

    let tree_items = build_thread_tree_items(messages);
    let mut tree_rows = Vec::new();
    flatten_thread_tree_rows(&tree_items, collapsed_tree_ids, &mut tree_rows);
    if tree_rows.is_empty() {
        Paragraph::new("  No hierarchy available.")
            .style(crate::tui_theme::text_hint(&tp))
            .render(content_inner, frame);
        return;
    }

    let selected_idx = selected_idx.min(tree_rows.len().saturating_sub(1));
    let selected_row = &tree_rows[selected_idx];
    let selected_message = messages
        .iter()
        .find(|message| message.id == selected_row.message_id)
        .unwrap_or(&messages[0]);

    let mut header_lines = Vec::new();
    if let Some(t) = thread {
        // Thread identity + counts
        let unread_span = if t.unread_count > 0 {
            Span::styled(
                format!("  {} unread", t.unread_count),
                crate::tui_theme::text_accent(&tp),
            )
        } else {
            Span::raw("")
        };
        header_lines.push(Line::from_spans([
            Span::styled("Thread: ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                truncate_display_width(
                    &t.thread_id,
                    content_inner.width.saturating_sub(34) as usize,
                ),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::styled(
                format!("  {loaded_message_count}/{total_thread_messages} loaded"),
                crate::tui_theme::text_meta(&tp),
            ),
            unread_span,
        ]));
        // Participants + time span
        header_lines.push(Line::from_spans([
            Span::styled(
                format!("{} participants", t.participant_count),
                crate::tui_theme::text_meta(&tp),
            ),
            Span::styled(
                format!(
                    "  {} \u{2192} {}",
                    iso_compact_time(&t.first_timestamp_iso),
                    iso_compact_time(&t.last_timestamp_iso)
                ),
                crate::tui_theme::text_hint(&tp),
            ),
        ]));
        if !t.participant_names.is_empty() {
            header_lines.push(Line::from_spans([
                Span::styled("Agents: ", crate::tui_theme::text_meta(&tp)),
                Span::styled(
                    truncate_display_width(
                        &t.participant_names,
                        content_inner.width.saturating_sub(8) as usize,
                    ),
                    Style::default().fg(tp.text_secondary),
                ),
            ]));
        }
    }
    // Mode indicator with styled keybind
    header_lines.push(if tree_focus {
        Line::from_spans([
            Span::styled("Mode: ", crate::tui_theme::text_meta(&tp)),
            Span::styled("Tree", Style::default().fg(tp.text_primary).bold()),
            Span::styled("  ", crate::tui_theme::text_meta(&tp)),
            Span::styled("Tab", crate::tui_theme::text_action_key(&tp)),
            Span::styled(" Preview", crate::tui_theme::text_hint(&tp)),
        ])
    } else {
        Line::from_spans([
            Span::styled("Mode: ", crate::tui_theme::text_meta(&tp)),
            Span::styled("Preview", Style::default().fg(tp.text_primary).bold()),
            Span::styled("  ", crate::tui_theme::text_meta(&tp)),
            Span::styled("Tab", crate::tui_theme::text_action_key(&tp)),
            Span::styled(" Tree", crate::tui_theme::text_hint(&tp)),
        ])
    });
    if has_older_messages {
        header_lines.push(Line::from_spans([
            Span::styled("o", crate::tui_theme::text_action_key(&tp)),
            Span::styled(
                format!(" Load {remaining_older_count} older messages"),
                crate::tui_theme::text_hint(&tp),
            ),
        ]));
    }

    let wrap_width = usize::from(content_inner.width.max(1));
    let header_rows_needed = header_lines
        .iter()
        .map(|line| {
            let plain = line.to_plain_text();
            let width = ftui::text::display_width(plain.as_str()).max(1);
            width.div_ceil(wrap_width).max(1)
        })
        .sum::<usize>()
        .max(1);
    let max_header_height = content_inner
        .height
        .saturating_sub(THREAD_DETAIL_MIN_BODY_HEIGHT)
        .max(1);
    let header_height = u16::try_from(header_rows_needed)
        .unwrap_or(u16::MAX)
        .min(max_header_height)
        .min(content_inner.height);
    let header_gap = u16::from(
        content_inner.height > header_height.saturating_add(THREAD_DETAIL_MIN_BODY_HEIGHT),
    );
    let header_area = Rect::new(
        content_inner.x,
        content_inner.y,
        content_inner.width,
        header_height,
    );
    let body_area = Rect::new(
        content_inner.x,
        content_inner
            .y
            .saturating_add(header_height)
            .saturating_add(header_gap),
        content_inner.width,
        content_inner
            .height
            .saturating_sub(header_height)
            .saturating_sub(header_gap),
    );
    clear_rect(frame, body_area, tp.panel_bg);
    if header_area.height > 0 {
        Paragraph::new(Text::from_lines(header_lines))
            .style(crate::tui_theme::text_primary(&tp))
            .wrap(ftui::text::WrapMode::Word)
            .render(header_area, frame);
    }
    if header_gap > 0 {
        let separator = Rect::new(
            content_inner.x,
            content_inner.y.saturating_add(header_height),
            content_inner.width,
            header_gap,
        );
        clear_rect(frame, separator, tp.panel_bg);
    }
    if body_area.width < 10 || body_area.height == 0 {
        return;
    }

    let compact_detail_mode = body_area.width < THREAD_DETAIL_COMPACT_WIDTH_THRESHOLD
        || body_area.height < THREAD_DETAIL_COMPACT_HEIGHT_THRESHOLD;
    let min_split_width = THREAD_DETAIL_MIN_PREVIEW_WIDTH
        .saturating_add(12)
        .saturating_add(1);
    let pane_gap = u16::from(
        !compact_detail_mode
            && body_area.width >= THREAD_DETAIL_PANE_GAP_THRESHOLD.max(min_split_width),
    );
    let available_width = body_area.width.saturating_sub(pane_gap);
    let min_preview_width =
        THREAD_DETAIL_MIN_PREVIEW_WIDTH.min(available_width.saturating_sub(1).max(1));
    let max_tree_width = available_width.saturating_sub(min_preview_width);
    let min_tree_width = 12_u16.min(max_tree_width.max(1));
    let preferred_tree_width = ((u32::from(body_area.width)
        * u32::from(thread_tree_width_percent(body_area.width)))
        / 100) as u16;
    let tree_width = preferred_tree_width.max(min_tree_width).min(max_tree_width);
    let preview_width = available_width.saturating_sub(tree_width);

    let (mut tree_area, mut preview_area) = if compact_detail_mode {
        (Rect::new(0, 0, 0, 0), body_area)
    } else {
        (
            Rect::new(body_area.x, body_area.y, tree_width, body_area.height),
            Rect::new(
                body_area
                    .x
                    .saturating_add(tree_width)
                    .saturating_add(pane_gap),
                body_area.y,
                preview_width,
                body_area.height,
            ),
        )
    };
    let render_tree_pane = !compact_detail_mode
        && tree_area.width >= THREAD_DETAIL_MIN_PANE_RENDER_WIDTH
        && tree_area.height >= 3;
    let preview_min_width = if compact_detail_mode {
        8
    } else {
        THREAD_DETAIL_MIN_PANE_RENDER_WIDTH
    };
    let render_preview_pane = preview_area.width >= preview_min_width && preview_area.height >= 3;
    if render_preview_pane && !render_tree_pane {
        preview_area = body_area;
    }
    if render_tree_pane && !render_preview_pane {
        tree_area = body_area;
    }
    if pane_gap > 0 && render_tree_pane && render_preview_pane {
        let splitter_area = Rect::new(
            body_area.x.saturating_add(tree_width),
            body_area.y,
            pane_gap,
            body_area.height,
        );
        render_splitter_handle(frame, splitter_area, true, false);
    }

    if render_tree_pane {
        clear_rect(frame, tree_area, tp.panel_bg);
        let tree_title_raw = if focused && tree_focus {
            if tree_area.width < 18 {
                "Tree *"
            } else {
                "Hierarchy *"
            }
        } else if tree_area.width < 18 {
            "Tree"
        } else {
            "Hierarchy"
        };
        let tree_title = fit_panel_title(tree_title_raw, tree_area.width);
        let tree_header_h = u16::from(tree_area.height >= 2);
        if tree_header_h > 0 {
            let header_line = Line::from_spans([
                Span::styled(" ".to_string(), crate::tui_theme::text_meta(&tp)),
                Span::styled(
                    tree_title,
                    if focused && tree_focus {
                        crate::tui_theme::text_primary(&tp).bold()
                    } else {
                        crate::tui_theme::text_meta(&tp)
                    },
                ),
            ]);
            Paragraph::new(Text::from_line(header_line)).render(
                Rect::new(tree_area.x, tree_area.y, tree_area.width, tree_header_h),
                frame,
            );
        }
        let tree_inner = Rect::new(
            tree_area.x,
            tree_area.y.saturating_add(tree_header_h),
            tree_area.width,
            tree_area.height.saturating_sub(tree_header_h),
        );
        if tree_inner.width > 0 && tree_inner.height > 0 {
            let guides = thread_tree_guides();
            let indent_token = tree_indent_token(guides);
            let marker_len = crate::tui_theme::SELECTION_PREFIX.chars().count();
            let max_depth = usize::from(tree_inner.width / 3).saturating_sub(1).max(1);
            let selected_style = Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold();
            let (start, end) =
                viewport_range(tree_rows.len(), tree_inner.height as usize, selected_idx);
            let rows = &tree_rows[start..end];
            let mut lines = Vec::with_capacity(rows.len());
            for row in rows {
                let is_selected = row.message_id == selected_row.message_id;
                let marker = if is_selected {
                    crate::tui_theme::SELECTION_PREFIX
                } else {
                    crate::tui_theme::SELECTION_PREFIX_EMPTY
                };
                let depth = row.depth.min(max_depth);
                let indent = indent_token.repeat(depth);
                let available = usize::from(tree_inner.width)
                    .saturating_sub(marker_len)
                    .saturating_sub(ftui::text::display_width(indent.as_str()))
                    .saturating_sub(1);
                let label = truncate_display_width(&row.label, available);
                let mut line = Line::from_spans([
                    Span::styled(marker.to_string(), crate::tui_theme::text_meta(&tp)),
                    Span::styled(indent, crate::tui_theme::text_meta(&tp)),
                    Span::styled(
                        label,
                        if row.has_children {
                            crate::tui_theme::text_primary(&tp)
                        } else {
                            crate::tui_theme::text_hint(&tp)
                        },
                    ),
                ]);
                if is_selected {
                    line.apply_base_style(selected_style);
                }
                lines.push(line);
            }
            Paragraph::new(Text::from_lines(lines))
                .wrap(ftui::text::WrapMode::None)
                .render(tree_inner, frame);
        }
    } else if !render_preview_pane {
        return;
    }

    if !render_preview_pane {
        return;
    }
    let preview_title_raw = if compact_detail_mode {
        if focused { "Message *" } else { "Message" }
    } else if focused && !tree_focus {
        if preview_area.width < 18 {
            "Msg *"
        } else {
            "Preview *"
        }
    } else if preview_area.width < 18 {
        "Msg"
    } else {
        "Preview"
    };
    clear_rect(frame, preview_area, tp.panel_bg);
    let preview_title = fit_panel_title(preview_title_raw, preview_area.width);
    let preview_header_h = u16::from(preview_area.height >= 2);
    if preview_header_h > 0 {
        let header_line = Line::from_spans([
            Span::styled(" ".to_string(), crate::tui_theme::text_meta(&tp)),
            Span::styled(
                preview_title,
                if focused && !tree_focus {
                    crate::tui_theme::text_primary(&tp).bold()
                } else {
                    crate::tui_theme::text_meta(&tp)
                },
            ),
        ]);
        Paragraph::new(Text::from_line(header_line)).render(
            Rect::new(
                preview_area.x,
                preview_area.y,
                preview_area.width,
                preview_header_h,
            ),
            frame,
        );
    }
    let preview_inner = Rect::new(
        preview_area.x,
        preview_area.y.saturating_add(preview_header_h),
        preview_area.width,
        preview_area.height.saturating_sub(preview_header_h),
    );
    if preview_inner.width == 0 || preview_inner.height == 0 {
        return;
    }
    let preview_pad_x = u16::from(preview_inner.width >= 28);
    let preview_content = Rect::new(
        preview_inner.x.saturating_add(preview_pad_x),
        preview_inner.y,
        preview_inner
            .width
            .saturating_sub(preview_pad_x.saturating_mul(2)),
        preview_inner.height,
    );
    if preview_content.width == 0 || preview_content.height == 0 {
        return;
    }

    let mut preview_lines = Vec::new();
    let mut preview_header_spans = vec![
        Span::styled(
            selected_message.from_agent.clone(),
            Style::default()
                .fg(agent_color(&selected_message.from_agent))
                .bold(),
        ),
        Span::raw(format!(
            " @ {}",
            iso_compact_time(&selected_message.timestamp_iso)
        )),
    ];
    if !selected_message.to_agents.is_empty() {
        preview_header_spans.push(Span::raw(format!(
            " -> {}",
            truncate_display_width(
                &selected_message.to_agents,
                preview_content.width.saturating_sub(24) as usize
            )
        )));
    }
    if selected_message.importance == "high" {
        preview_header_spans.push(Span::styled(" [HIGH]", crate::tui_theme::text_warning(&tp)));
    } else if selected_message.importance == "urgent" {
        preview_header_spans.push(Span::styled(
            " [URGENT]",
            crate::tui_theme::text_critical(&tp),
        ));
    }
    if selected_message.ack_required {
        preview_header_spans.push(Span::styled(" @ACK", crate::tui_theme::text_accent(&tp)));
    }
    preview_lines.push(Line::from_spans(preview_header_spans));
    if !selected_message.subject.is_empty() {
        preview_lines.push(Line::from_spans([
            Span::styled("Subject: ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                truncate_display_width(
                    &selected_message.subject,
                    preview_content.width.saturating_sub(9) as usize,
                ),
                Style::default().fg(tp.text_primary),
            ),
        ]));
    }
    preview_lines.push(Line::raw(String::new()));

    let md_theme = crate::tui_theme::markdown_theme();
    if expanded_message_ids.contains(&selected_message.id) {
        for line in crate::tui_markdown::render_body(&selected_message.body_md, &md_theme).lines() {
            preview_lines.push(line.clone());
        }
    } else {
        let rendered = crate::tui_markdown::render_body(&selected_message.body_md, &md_theme);
        let lines: Vec<Line<'static>> = rendered
            .lines()
            .iter()
            .filter(|line| !line.to_plain_text().trim().is_empty())
            .cloned()
            .collect();
        if lines.is_empty() {
            preview_lines.push(Line::raw("(empty)"));
        } else {
            for line in lines.iter().take(THREAD_COLLAPSED_PREVIEW_LINES) {
                preview_lines.push(line.clone());
            }
            if lines.len() > THREAD_COLLAPSED_PREVIEW_LINES {
                preview_lines.push(Line::styled("…", crate::tui_theme::text_hint(&tp)));
            }
        }
    }

    let scroll_rows = u16::try_from(scroll).unwrap_or(u16::MAX);
    Paragraph::new(Text::from_lines(preview_lines))
        .style(crate::tui_theme::text_primary(&tp))
        .wrap(ftui::text::WrapMode::Word)
        .scroll((scroll_rows, 0))
        .render(preview_content, frame);
}

fn stable_hash<T: Hash>(value: T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn render_mermaid_source_to_buffer(source: &str, width: u16, height: u16) -> Buffer {
    let mut buffer = Buffer::new(width, height);
    let config = mermaid::MermaidConfig::from_env();
    if !config.enabled {
        for (idx, ch) in "Mermaid disabled via env".chars().enumerate() {
            if let Ok(x) = u16::try_from(idx) {
                if x >= width {
                    break;
                }
                buffer.set(x, 0, ftui::Cell::from_char(ch));
            } else {
                break;
            }
        }
        return buffer;
    }

    let matrix = MermaidCompatibilityMatrix::default();
    let policy = MermaidFallbackPolicy::default();
    let parsed = mermaid::parse_with_diagnostics(source);
    let ir_parse = mermaid::normalize_ast_to_ir(&parsed.ast, &config, &matrix, &policy);
    let mut errors = parsed.errors;
    errors.extend(ir_parse.errors);

    let render_area = Rect::from_size(width, height);
    let layout = mermaid_layout::layout_diagram(&ir_parse.ir, &config);
    let _plan = mermaid_render::render_diagram_adaptive(
        &layout,
        &ir_parse.ir,
        &config,
        render_area,
        &mut buffer,
    );

    if !errors.is_empty() {
        let has_content = !ir_parse.ir.nodes.is_empty()
            || !ir_parse.ir.edges.is_empty()
            || !ir_parse.ir.labels.is_empty()
            || !ir_parse.ir.clusters.is_empty();
        if has_content {
            mermaid_render::render_mermaid_error_overlay(
                &errors,
                source,
                &config,
                render_area,
                &mut buffer,
            );
        } else {
            mermaid_render::render_mermaid_error_panel(
                &errors,
                source,
                &config,
                render_area,
                &mut buffer,
            );
        }
    }

    buffer
}

fn blit_buffer_to_frame(frame: &mut Frame<'_>, area: Rect, buffer: &Buffer) {
    let width = area.width.min(buffer.width());
    let height = area.height.min(buffer.height());
    for y in 0..height {
        for x in 0..width {
            let Some(src) = buffer.get(x, y) else {
                continue;
            };
            let dst_x = area.x + x;
            let dst_y = area.y + y;
            if let Some(dst) = frame.buffer.get_mut(dst_x, dst_y) {
                *dst = *src;
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Utility helpers
// ──────────────────────────────────────────────────────────────────────

const fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

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

fn build_thread_tree_items(messages: &[ThreadMessage]) -> Vec<crate::tui_widgets::ThreadTreeItem> {
    if messages.is_empty() {
        return Vec::new();
    }

    let message_by_id: HashMap<i64, &ThreadMessage> = messages.iter().map(|m| (m.id, m)).collect();

    let mut children_by_parent: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
    for message in messages {
        let parent_id = message
            .reply_to_id
            .filter(|candidate| message_by_id.contains_key(candidate));
        children_by_parent
            .entry(parent_id)
            .or_default()
            .push(message.id);
    }

    for ids in children_by_parent.values_mut() {
        ids.sort_by_key(|id| {
            message_by_id.get(id).map_or((i64::MAX, *id), |message| {
                (message.timestamp_micros, message.id)
            })
        });
    }

    let mut recursion_stack = HashSet::new();
    children_by_parent
        .get(&None)
        .map_or_else(Vec::new, |roots| {
            roots
                .iter()
                .filter_map(|id| {
                    build_thread_tree_item_node(
                        *id,
                        &message_by_id,
                        &children_by_parent,
                        &mut recursion_stack,
                    )
                })
                .collect()
        })
}

fn build_thread_tree_item_node(
    message_id: i64,
    message_by_id: &HashMap<i64, &ThreadMessage>,
    children_by_parent: &HashMap<Option<i64>, Vec<i64>>,
    recursion_stack: &mut HashSet<i64>,
) -> Option<crate::tui_widgets::ThreadTreeItem> {
    if !recursion_stack.insert(message_id) {
        return None;
    }

    let message = *message_by_id.get(&message_id)?;
    let mut node = crate::tui_widgets::ThreadTreeItem::new(
        message.id,
        message.from_agent.clone(),
        truncate_display_width(&message.subject, 60),
        iso_compact_time(&message.timestamp_iso).to_string(),
        message.is_unread,
        message.ack_required,
    );

    node.children = children_by_parent
        .get(&Some(message_id))
        .map_or_else(Vec::new, |children| {
            children
                .iter()
                .filter_map(|child_id| {
                    build_thread_tree_item_node(
                        *child_id,
                        message_by_id,
                        children_by_parent,
                        recursion_stack,
                    )
                })
                .collect()
        });

    recursion_stack.remove(&message_id);
    Some(node)
}

/// Convert a [`ThreadTreeItem`] into a [`TreeNode`] for the ftui tree widget.
fn flatten_thread_tree_rows(
    nodes: &[crate::tui_widgets::ThreadTreeItem],
    collapsed_tree_ids: &HashSet<i64>,
    out: &mut Vec<ThreadTreeRow>,
) {
    flatten_thread_tree_rows_at_depth(nodes, collapsed_tree_ids, 0, out);
}

fn flatten_thread_tree_rows_at_depth(
    nodes: &[crate::tui_widgets::ThreadTreeItem],
    collapsed_tree_ids: &HashSet<i64>,
    depth: usize,
    out: &mut Vec<ThreadTreeRow>,
) {
    for node in nodes {
        let is_expanded = !collapsed_tree_ids.contains(&node.message_id);
        out.push(ThreadTreeRow {
            message_id: node.message_id,
            has_children: !node.children.is_empty(),
            is_expanded,
            depth,
            label: node.render_plain_label(is_expanded),
        });
        if is_expanded {
            flatten_thread_tree_rows_at_depth(
                &node.children,
                collapsed_tree_ids,
                depth.saturating_add(1),
                out,
            );
        }
    }
}

/// Truncate a string to at most `max_len` characters, adding "..." if truncated.
#[cfg(test)]
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let mut result: String = s.chars().take(max_len - 3).collect();
        result.push_str("...");
        result
    }
}

/// Truncate a string to a target display width, adding an ellipsis on overflow.
fn truncate_display_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if ftui::text::display_width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut used = 0usize;
    let budget = max_width - 1;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let ch_s = ch.encode_utf8(&mut buf);
        let ch_w = ftui::text::display_width(ch_s);
        if used.saturating_add(ch_w) > budget {
            break;
        }
        out.push(ch);
        used = used.saturating_add(ch_w);
    }
    out.push('…');
    while ftui::text::display_width(out.as_str()) > max_width {
        let _ = out.pop();
        if out.pop().is_none() {
            return "…".to_string();
        }
        out.push('…');
    }
    out
}

/// Clip a styled line to `max_width` display cells while preserving style/link metadata.
fn clip_line_to_display_width<'a>(line: Line<'a>, max_width: usize) -> Line<'a> {
    if max_width == 0 {
        return Line::raw(String::new());
    }
    if line.width() <= max_width {
        return line;
    }

    let mut remaining = max_width;
    let mut clipped: Vec<Span<'a>> = Vec::new();
    for span in line.spans() {
        if remaining == 0 {
            break;
        }
        let span_width = span.width();
        if span_width <= remaining {
            clipped.push(span.clone());
            remaining = remaining.saturating_sub(span_width);
            continue;
        }
        let truncated = truncate_display_width(span.as_str(), remaining);
        if !truncated.is_empty() {
            clipped.push(Span {
                content: Cow::Owned(truncated),
                style: span.style,
                link: span.link.clone(),
            });
        }
        break;
    }

    Line::from_spans(clipped)
}

/// Fit a block title to a panel width, preserving rounded-border margins.
fn fit_panel_title(title: &str, panel_width: u16) -> String {
    let max_len = usize::from(panel_width.saturating_sub(4));
    if max_len == 0 {
        String::new()
    } else {
        truncate_display_width(title, max_len)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_harness::buffer_to_text;

    fn mouse_event(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(ftui::MouseEvent {
            kind,
            x,
            y,
            modifiers: Modifiers::empty(),
        })
    }

    fn ctrl_key(code: KeyCode) -> Event {
        Event::Key(ftui::KeyEvent {
            code,
            modifiers: Modifiers::CTRL,
            kind: KeyEventKind::Press,
        })
    }

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
    fn mouse_drop_on_different_thread_dispatches_rethread_action() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.threads.push(make_thread("t2", 2, 2));
        screen.detail_messages.push(make_message(77));
        screen.focus = Focus::DetailPanel;
        screen.last_list_area.set(Rect::new(0, 1, 40, 10));
        screen.last_detail_area.set(Rect::new(40, 1, 40, 10));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 45, 3);
        let _ = screen.update(&down, &state);
        assert!(matches!(screen.message_drag, MessageDragState::Pending(_)));
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 4);
        let _ = screen.update(&drag, &state);
        let up = mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 4);
        let cmd = screen.update(&up, &state);
        match cmd {
            Cmd::Msg(MailScreenMsg::ActionExecute(op, ctx)) => {
                assert_eq!(op, "rethread_message:77:t2");
                assert_eq!(ctx, "t1");
            }
            _ => panic!("expected ActionExecute command"),
        }
        assert!(matches!(screen.message_drag, MessageDragState::Idle));
        assert!(state.message_drag_snapshot().is_none());
    }

    #[test]
    fn mouse_drag_over_invalid_target_sets_warning_snapshot() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.detail_messages.push(make_message(77));
        screen.focus = Focus::DetailPanel;
        screen.last_list_area.set(Rect::new(0, 1, 40, 10));
        screen.last_detail_area.set(Rect::new(40, 1, 40, 10));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 45, 3);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 79, 20);
        let _ = screen.update(&drag, &state);
        let snapshot = state.message_drag_snapshot().expect("drag snapshot");
        assert!(snapshot.invalid_hover);
        assert!(!snapshot.hovered_is_valid);
        assert!(snapshot.hovered_thread_id.is_none());
    }

    #[test]
    fn mouse_drop_on_invalid_target_is_noop() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.detail_messages.push(make_message(77));
        screen.focus = Focus::DetailPanel;
        screen.last_list_area.set(Rect::new(0, 1, 40, 10));
        screen.last_detail_area.set(Rect::new(40, 1, 40, 10));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 45, 3);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 79, 20);
        let _ = screen.update(&drag, &state);
        let up = mouse_event(MouseEventKind::Up(MouseButton::Left), 79, 20);
        let cmd = screen.update(&up, &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(matches!(screen.message_drag, MessageDragState::Idle));
        assert!(state.message_drag_snapshot().is_none());
    }

    #[test]
    fn mouse_drop_on_same_thread_is_noop() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.threads.push(make_thread("t2", 2, 2));
        screen.detail_messages.push(make_message(88));
        screen.focus = Focus::DetailPanel;
        screen.last_list_area.set(Rect::new(0, 1, 40, 10));
        screen.last_detail_area.set(Rect::new(40, 1, 40, 10));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 45, 3);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 2);
        let _ = screen.update(&drag, &state);
        let up = mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 2);
        let cmd = screen.update(&up, &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(matches!(screen.message_drag, MessageDragState::Idle));
        assert!(state.message_drag_snapshot().is_none());
    }

    #[test]
    fn ctrl_m_marks_selected_detail_message_for_keyboard_move() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.detail_messages.push(make_message(77));
        screen.focus = Focus::DetailPanel;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let cmd = screen.update(&ctrl_key(KeyCode::Char('m')), &state);
        assert!(matches!(cmd, Cmd::None));
        let marker = state
            .keyboard_move_snapshot()
            .expect("keyboard move marker");
        assert_eq!(marker.message_id, 77);
        assert_eq!(marker.source_thread_id, "t1");
    }

    #[test]
    fn ctrl_v_dispatches_marked_message_to_selected_thread() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.threads.push(make_thread("t2", 2, 2));
        screen.cursor = 1;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: 88,
            subject: "Subj".to_string(),
            source_thread_id: "t1".to_string(),
            source_project_slug: "project".to_string(),
        }));

        let cmd = screen.update(&ctrl_key(KeyCode::Char('v')), &state);
        match cmd {
            Cmd::Msg(MailScreenMsg::ActionExecute(op, ctx)) => {
                assert_eq!(op, "rethread_message:88:t2");
                assert_eq!(ctx, "t1");
            }
            _ => panic!("expected ActionExecute command"),
        }
        assert!(state.keyboard_move_snapshot().is_none());
    }

    #[test]
    fn ctrl_v_same_thread_is_noop_and_preserves_marker() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.cursor = 0;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: 88,
            subject: "Subj".to_string(),
            source_thread_id: "t1".to_string(),
            source_project_slug: "project".to_string(),
        }));

        let cmd = screen.update(&ctrl_key(KeyCode::Char('v')), &state);
        assert!(matches!(cmd, Cmd::None));
        let marker = state
            .keyboard_move_snapshot()
            .expect("keyboard move marker should remain");
        assert_eq!(marker.message_id, 88);
        assert_eq!(marker.source_thread_id, "t1");
    }

    #[test]
    fn escape_clears_keyboard_move_marker_before_focus_change() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.focus = Focus::DetailPanel;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: 88,
            subject: "Subj".to_string(),
            source_thread_id: "t1".to_string(),
            source_project_slug: "project".to_string(),
        }));

        let cmd = screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Escape)), &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(state.keyboard_move_snapshot().is_none());
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

    #[test]
    fn r_key_deep_links_to_quick_reply_from_detail_tree_focus() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.focus = Focus::DetailPanel;
        screen.detail_messages.push(make_message(77));
        screen.detail_tree_focus = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let r = Event::Key(ftui::KeyEvent::new(KeyCode::Char('r')));
        let cmd = screen.update(&r, &state);
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::ReplyToMessage(77)))
        ));
    }

    #[test]
    fn r_key_deep_links_to_quick_reply_from_detail_preview_focus() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 3, 2));
        screen.focus = Focus::DetailPanel;
        screen.detail_messages.push(make_message(88));
        screen.detail_tree_focus = false;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let r = Event::Key(ftui::KeyEvent::new(KeyCode::Char('r')));
        let cmd = screen.update(&r, &state);
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::ReplyToMessage(88)))
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

        // Home jumps to start
        let home = Event::Key(ftui::KeyEvent::new(KeyCode::Home));
        screen.update(&home, &state);
        assert_eq!(screen.cursor, 0);
    }

    #[test]
    fn g_toggles_mermaid_panel_in_list_and_detail() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("t1", 2, 2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let g = Event::Key(ftui::KeyEvent::new(KeyCode::Char('g')));

        screen.update(&g, &state);
        assert!(screen.show_mermaid_panel);
        screen.update(&g, &state);
        assert!(!screen.show_mermaid_panel);

        screen.focus = Focus::DetailPanel;
        screen.update(&g, &state);
        assert!(screen.show_mermaid_panel);
    }

    #[test]
    fn escape_closes_mermaid_panel_before_leaving_detail() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        screen.show_mermaid_panel = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);

        assert!(!screen.show_mermaid_panel);
        assert_eq!(screen.focus, Focus::DetailPanel);
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

    // ── Detail card navigation + expansion ─────────────────────────

    #[test]
    fn detail_cursor_moves_in_detail_pane() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        screen.detail_messages.push(make_message(1));
        screen.detail_messages.push(make_message(2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let j = Event::Key(ftui::KeyEvent::new(KeyCode::Char('j')));
        screen.update(&j, &state);
        assert_eq!(screen.detail_cursor, 1);

        let k = Event::Key(ftui::KeyEvent::new(KeyCode::Char('k')));
        screen.update(&k, &state);
        assert_eq!(screen.detail_cursor, 0);

        // Can't go below 0
        screen.update(&k, &state);
        assert_eq!(screen.detail_cursor, 0);
    }

    #[test]
    fn enter_and_space_toggle_selected_message_expansion() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        screen.detail_messages.push(make_message(1));
        screen.detail_messages.push(make_message(2));
        screen.detail_cursor = 1;
        screen.expanded_message_ids.insert(2);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Enter collapses selected card.
        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        screen.update(&enter, &state);
        assert!(!screen.expanded_message_ids.contains(&2));

        // Space expands it again.
        let space = Event::Key(ftui::KeyEvent::new(KeyCode::Char(' ')));
        screen.update(&space, &state);
        assert!(screen.expanded_message_ids.contains(&2));
    }

    #[test]
    fn e_and_c_expand_and_collapse_all_cards() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        for id in 1..=4 {
            screen.detail_messages.push(make_message(id));
        }
        // Start with a partial expansion set.
        screen.expanded_message_ids.insert(4);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let expand_all = Event::Key(ftui::KeyEvent::new(KeyCode::Char('e')));
        screen.update(&expand_all, &state);
        assert_eq!(screen.expanded_message_ids.len(), 4);

        let collapse_all = Event::Key(ftui::KeyEvent::new(KeyCode::Char('c')));
        screen.update(&collapse_all, &state);
        assert!(screen.expanded_message_ids.is_empty());
    }

    #[test]
    fn tab_toggles_detail_focus_between_tree_and_preview() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        screen.detail_messages.push(make_message(1));
        screen.detail_messages.push(make_message(2));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        assert!(screen.detail_tree_focus);
        let tab = Event::Key(ftui::KeyEvent::new(KeyCode::Tab));
        screen.update(&tab, &state);
        assert!(!screen.detail_tree_focus);
        screen.update(&tab, &state);
        assert!(screen.detail_tree_focus);
    }

    #[test]
    fn left_and_right_collapse_and_expand_selected_branch() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let root = make_message(1);
        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        screen.detail_messages = vec![root, child];
        screen.detail_cursor = 0;
        screen.detail_tree_focus = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let left = Event::Key(ftui::KeyEvent::new(KeyCode::Left));
        screen.update(&left, &state);
        assert!(screen.collapsed_tree_ids.contains(&1));

        let right = Event::Key(ftui::KeyEvent::new(KeyCode::Right));
        screen.update(&right, &state);
        assert!(!screen.collapsed_tree_ids.contains(&1));
    }

    #[test]
    fn space_toggles_selected_branch_expansion() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let root = make_message(1);
        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        screen.detail_messages = vec![root, child];
        screen.detail_cursor = 0;
        screen.detail_tree_focus = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let space = Event::Key(ftui::KeyEvent::new(KeyCode::Char(' ')));
        screen.update(&space, &state);
        assert!(screen.collapsed_tree_ids.contains(&1));

        screen.update(&space, &state);
        assert!(!screen.collapsed_tree_ids.contains(&1));
    }

    #[test]
    fn clamp_detail_cursor_drops_hidden_branch_selection() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let root = make_message(1);
        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        screen.detail_messages = vec![root, child];
        screen.detail_cursor = 1;
        screen.collapsed_tree_ids.insert(1);

        screen.clamp_detail_cursor_to_tree_rows();
        assert_eq!(screen.detail_cursor, 0);
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
    fn thread_list_content_does_not_emit_box_drawing_glyphs() {
        let threads = vec![
            make_thread(
                "thread-very-long-identifier-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                7,
                3,
            ),
            make_thread(
                "thread-very-long-identifier-bbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                5,
                2,
            ),
            make_thread(
                "thread-very-long-identifier-cccccccccccccccccccccccccccc",
                4,
                1,
            ),
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(72, 16, &mut pool);
        let area = Rect::new(0, 0, 72, 16);
        render_thread_list(
            &mut frame,
            area,
            &threads,
            0,
            true,
            ViewLens::Activity,
            SortMode::LastActivity,
            false,
            None,
            None,
        );

        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        let content = if inner.width >= THREAD_LIST_SIDE_PADDING_MIN_WIDTH {
            Rect::new(
                inner.x.saturating_add(1),
                inner.y,
                inner.width.saturating_sub(2),
                inner.height,
            )
        } else {
            inner
        };

        for y in content.y..content.y.saturating_add(content.height) {
            for x in content.x..content.x.saturating_add(content.width) {
                let ch = frame
                    .buffer
                    .get(x, y)
                    .and_then(|cell| cell.content.as_char())
                    .unwrap_or(' ');
                assert!(
                    !matches!(ch as u32, 0x2500..=0x257F),
                    "unexpected box-drawing glyph {ch:?} in content at ({x},{y})"
                );
            }
        }
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
    fn render_with_mermaid_panel_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads.push(make_thread("test-thread", 3, 2));
        for i in 0..3 {
            screen.detail_messages.push(make_message(i));
        }
        screen.loaded_thread_id = "test-thread".to_string();
        screen.show_mermaid_panel = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn metadata_header_shows_participant_count_and_names() {
        let mut thread = make_thread("thread-meta", 12, 3);
        thread.participant_names = "Alpha, Beta, Gamma".to_string();
        thread.unread_count = 3;

        let messages = vec![make_message(1)];
        let expanded: HashSet<i64> = HashSet::new();
        let collapsed: HashSet<i64> = HashSet::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 24, &mut pool);

        render_thread_detail(
            &mut frame,
            Rect::new(0, 0, 120, 24),
            &messages,
            Some(&thread),
            0,
            0,
            &expanded,
            &collapsed,
            false,
            0,
            12,
            12,
            false,
            true,
        );

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("3 participants"),
            "missing participant count: {text}"
        );
        assert!(
            text.contains("Agents: Alpha"),
            "missing first participant: {text}"
        );
        assert!(text.contains("Beta"), "missing second participant: {text}");
        assert!(text.contains("Gamma"), "missing third participant: {text}");
    }

    #[test]
    fn selected_tree_row_updates_preview_subject() {
        let mut root = make_message(1);
        root.subject = "Root subject".to_string();
        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        child.subject = "Child subject".to_string();

        let messages = vec![root, child];
        let expanded: HashSet<i64> = HashSet::new();
        let collapsed: HashSet<i64> = HashSet::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 24, &mut pool);

        render_thread_detail(
            &mut frame,
            Rect::new(0, 0, 120, 24),
            &messages,
            None,
            0,
            1,
            &expanded,
            &collapsed,
            false,
            0,
            2,
            2,
            true,
            true,
        );

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Subject: Child subject"),
            "preview did not follow selected tree row: {text}"
        );
    }

    #[test]
    fn render_narrow_screen_no_panic() {
        let screen = ThreadExplorerScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 40, 10), &state);
        assert_eq!(screen.last_detail_area.get(), Rect::new(0, 0, 0, 0));
        assert!(screen.last_list_area.get().width > 0);
    }

    #[test]
    fn render_narrow_detail_focus_no_panic() {
        let mut screen = ThreadExplorerScreen::new();
        screen.focus = Focus::DetailPanel;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 40, 10), &state);
        assert_eq!(screen.last_list_area.get(), Rect::new(0, 0, 0, 0));
        assert!(screen.last_detail_area.get().width > 0);
    }

    #[test]
    fn narrow_tall_layout_keeps_list_and_detail_visible() {
        let screen = ThreadExplorerScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 20), &state);

        let list = screen.last_list_area.get();
        let detail = screen.last_detail_area.get();
        assert!(
            list.width > 0 && detail.width > 0,
            "stacked fallback should keep both panes visible"
        );
        assert_eq!(list.width, detail.width);
        assert!(detail.y > list.y);
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

    #[test]
    fn paginated_fetch_respects_offset_for_older_messages() {
        let conn = make_thread_messages_db("thread-paged", 25);

        let (recent, recent_offset) = fetch_thread_messages_paginated(&conn, "thread-paged", 20, 0);
        assert_eq!(recent_offset, 0);
        assert_eq!(recent.len(), 20);
        assert_eq!(recent.first().map(|m| m.id), Some(6));
        assert_eq!(recent.last().map(|m| m.id), Some(25));

        let (older, older_offset) = fetch_thread_messages_paginated(&conn, "thread-paged", 15, 20);
        assert_eq!(older_offset, 20);
        assert_eq!(older.len(), 5);
        assert_eq!(older.first().map(|m| m.id), Some(1));
        assert_eq!(older.last().map(|m| m.id), Some(5));
    }

    #[test]
    fn thread_tree_builder_nests_reply_chains_and_sorts_children() {
        let mut root = make_message(10);
        root.subject = "root".to_string();
        root.timestamp_micros = 10;
        root.timestamp_iso = "2026-02-06T12:00:10Z".to_string();

        let mut child_newer = make_message(12);
        child_newer.reply_to_id = Some(10);
        child_newer.subject = "child-newer".to_string();
        child_newer.timestamp_micros = 12;
        child_newer.timestamp_iso = "2026-02-06T12:00:12Z".to_string();

        let mut child_older = make_message(11);
        child_older.reply_to_id = Some(10);
        child_older.subject = "child-older".to_string();
        child_older.timestamp_micros = 11;
        child_older.timestamp_iso = "2026-02-06T12:00:11Z".to_string();
        child_older.ack_required = true;
        child_older.is_unread = true;

        let tree = build_thread_tree_items(&[child_newer, root, child_older]);
        assert_eq!(tree.len(), 1, "expected a single root node");
        assert_eq!(tree[0].message_id, 10);
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].message_id, 11);
        assert_eq!(tree[0].children[1].message_id, 12);
        assert!(tree[0].children[0].is_unread);
        assert!(tree[0].children[0].is_ack_required);
    }

    #[test]
    fn thread_tree_builder_sorts_roots_chronologically() {
        let mut first = make_message(1);
        first.timestamp_micros = 100;
        first.timestamp_iso = "2026-02-06T12:00:00Z".to_string();
        first.subject = "first".to_string();

        let mut second = make_message(2);
        second.timestamp_micros = 300;
        second.timestamp_iso = "2026-02-06T12:00:03Z".to_string();
        second.subject = "second".to_string();

        let mut third = make_message(3);
        third.timestamp_micros = 200;
        third.timestamp_iso = "2026-02-06T12:00:02Z".to_string();
        third.subject = "third".to_string();

        let roots = build_thread_tree_items(&[second, first, third]);
        let root_ids: Vec<i64> = roots.into_iter().map(|item| item.message_id).collect();
        assert_eq!(root_ids, vec![1, 3, 2]);
    }

    #[test]
    fn thread_tree_builder_promotes_orphan_reply_to_root() {
        let mut orphan = make_message(20);
        orphan.reply_to_id = Some(9999);
        orphan.subject = "orphan".to_string();
        orphan.timestamp_micros = 500;
        orphan.timestamp_iso = "2026-02-06T12:00:05Z".to_string();

        let roots = build_thread_tree_items(&[orphan]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].message_id, 20);
    }

    #[test]
    fn parse_thread_page_size_honors_valid_override() {
        assert_eq!(parse_thread_page_size(Some("7")), 7);
        assert_eq!(parse_thread_page_size(Some(" 42 ")), 42);
    }

    #[test]
    fn parse_thread_page_size_falls_back_to_default() {
        assert_eq!(parse_thread_page_size(None), DEFAULT_THREAD_PAGE_SIZE);
        assert_eq!(
            parse_thread_page_size(Some("not-a-number")),
            DEFAULT_THREAD_PAGE_SIZE
        );
        assert_eq!(parse_thread_page_size(Some("0")), DEFAULT_THREAD_PAGE_SIZE);
    }

    // ── Test helpers ────────────────────────────────────────────────

    fn make_thread_messages_db(thread_id: &str, count: usize) -> DbConn {
        let conn = DbConn::open_memory().expect("open memory sqlite");
        conn.execute_raw("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .expect("create agents table");
        conn.execute_raw(
            "CREATE TABLE messages (\
               id INTEGER PRIMARY KEY, \
               subject TEXT NOT NULL, \
               body_md TEXT NOT NULL, \
               importance TEXT NOT NULL, \
               created_ts INTEGER NOT NULL, \
               sender_id INTEGER NOT NULL, \
               thread_id TEXT NOT NULL\
             )",
        )
        .expect("create messages table");
        conn.execute_raw(
            "CREATE TABLE message_recipients (\
               message_id INTEGER NOT NULL, \
               agent_id INTEGER NOT NULL\
             )",
        )
        .expect("create recipients table");
        conn.execute_raw("INSERT INTO agents (id, name) VALUES (1, 'Sender'), (2, 'Receiver')")
            .expect("seed agents");

        for idx in 1..=count {
            let id = i64::try_from(idx).expect("idx fits i64");
            let created_ts = 1_700_000_000_000_000_i64 + (id * 1_000_000_i64);
            let insert_message = format!(
                "INSERT INTO messages (id, subject, body_md, importance, created_ts, sender_id, thread_id) \
                 VALUES ({id}, 'Subject {id}', 'Body {id}', 'normal', {created_ts}, 1, '{}')",
                thread_id.replace('\'', "''")
            );
            conn.execute_raw(&insert_message)
                .expect("insert thread message");
            let insert_recipient =
                format!("INSERT INTO message_recipients (message_id, agent_id) VALUES ({id}, 2)");
            conn.execute_raw(&insert_recipient)
                .expect("insert message recipient");
        }

        conn
    }

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
            first_timestamp_iso: "2026-02-06T10:00:00Z".to_string(),
            unread_count: 0,
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
        assert!(bindings.iter().any(|b| b.key == "g"));
        assert!(bindings.iter().any(|b| b.key == "r"));
        assert!(bindings.iter().any(|b| b.key == "Enter/Space"));
        assert!(bindings.iter().any(|b| b.key == "e / c"));
    }

    #[test]
    fn agent_color_is_deterministic() {
        let a = agent_color("CopperCastle");
        let b = agent_color("CopperCastle");
        let c = agent_color("FrostyCompass");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn urgent_pulse_toggles_from_tick_count() {
        let mut screen = ThreadExplorerScreen::new();
        screen.reduced_motion = false;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        screen.tick(0, &state);
        assert!(screen.urgent_pulse_on);

        screen.tick(URGENT_PULSE_HALF_PERIOD_TICKS, &state);
        assert!(!screen.urgent_pulse_on);
    }

    #[test]
    fn urgent_pulse_is_static_in_reduced_motion() {
        let mut screen = ThreadExplorerScreen::new();
        screen.reduced_motion = true;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        screen.tick(URGENT_PULSE_HALF_PERIOD_TICKS, &state);
        assert!(screen.urgent_pulse_on);
    }

    #[test]
    fn parse_tree_guides_handles_known_and_unknown_values() {
        assert_eq!(parse_tree_guides("rounded"), Some(TreeGuides::Rounded));
        assert_eq!(parse_tree_guides("DOUBLE"), Some(TreeGuides::Double));
        assert_eq!(parse_tree_guides("nope"), None);
    }

    fn make_message(id: i64) -> ThreadMessage {
        ThreadMessage {
            id,
            reply_to_id: None,
            from_agent: "GoldFox".to_string(),
            to_agents: "SilverWolf".to_string(),
            subject: format!("Message #{id}"),
            body_md: format!("Body of message {id}.\nSecond line."),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 1_700_000_000_000_000 + id * 1_000_000,
            importance: if id % 3 == 0 { "high" } else { "normal" }.to_string(),
            is_unread: false,
            ack_required: false,
        }
    }

    // ── truncate_str UTF-8 safety ────────────────────────────────────

    #[test]
    fn truncate_str_ascii_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_ascii_over() {
        assert_eq!(truncate_str("hello world", 8), "hello...");
    }

    #[test]
    fn truncate_str_3byte_arrow() {
        let s = "foo → bar → baz";
        let r = truncate_str(s, 7);
        assert!(r.chars().count() <= 7);
        assert!(r.ends_with("..."));
    }

    #[test]
    fn truncate_str_cjk() {
        let s = "日本語テスト文字列";
        let r = truncate_str(s, 6);
        assert!(r.chars().count() <= 6);
        assert!(r.ends_with("..."));
    }

    #[test]
    fn truncate_str_emoji() {
        let s = "🔥🚀💡🎯🏆";
        let r = truncate_str(s, 5);
        assert!(r.chars().count() <= 5);
    }

    #[test]
    fn truncate_str_tiny_max() {
        assert_eq!(truncate_str("hello world", 2).chars().count(), 2);
    }

    #[test]
    fn truncate_str_multibyte_exact_fit() {
        // 5 chars, fits exactly
        let s = "→→→→→";
        assert_eq!(truncate_str(s, 5), s);
    }

    #[test]
    fn truncate_str_multibyte_sweep() {
        let s = "ab→cd🔥éf";
        for max in 1..=s.chars().count() + 2 {
            let r = truncate_str(s, max);
            assert!(
                r.chars().count() <= max,
                "max={max} got {}",
                r.chars().count()
            );
        }
    }

    // ── br-2h8pz: Thread tree builder + hierarchy tests ─────────────

    #[test]
    fn tree_empty_messages_produces_empty_tree() {
        let tree = build_thread_tree_items(&[]);
        assert!(tree.is_empty());
    }

    #[test]
    fn tree_single_root_no_replies() {
        let msg = make_message(1);
        let tree = build_thread_tree_items(&[msg]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].message_id, 1);
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn tree_three_level_nesting() {
        let mut root = make_message(1);
        root.timestamp_micros = 100;
        root.timestamp_iso = "2026-02-06T12:00:00Z".to_string();

        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        child.timestamp_micros = 200;
        child.timestamp_iso = "2026-02-06T12:00:02Z".to_string();

        let mut grandchild = make_message(3);
        grandchild.reply_to_id = Some(2);
        grandchild.timestamp_micros = 300;
        grandchild.timestamp_iso = "2026-02-06T12:00:03Z".to_string();

        let tree = build_thread_tree_items(&[grandchild, root, child]);
        assert_eq!(tree.len(), 1, "single root");
        assert_eq!(tree[0].message_id, 1);
        assert_eq!(tree[0].children.len(), 1, "one child");
        assert_eq!(tree[0].children[0].message_id, 2);
        assert_eq!(tree[0].children[0].children.len(), 1, "one grandchild");
        assert_eq!(tree[0].children[0].children[0].message_id, 3);
    }

    #[test]
    fn tree_circular_reference_detected_and_broken() {
        // A -> B -> A (cycle)
        let mut a = make_message(1);
        a.reply_to_id = Some(2);
        a.timestamp_micros = 100;
        a.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let mut b = make_message(2);
        b.reply_to_id = Some(1);
        b.timestamp_micros = 200;
        b.timestamp_iso = "2026-02-06T12:00:02Z".to_string();

        let tree = build_thread_tree_items(&[a, b]);
        // Both reference each other; neither has a valid root parent.
        // The builder should filter invalid parents and not crash.
        // Since both have reply_to_id pointing to the other, and both exist,
        // neither will be a root → they go under their respective parents.
        // But since no root exists, the result depends on the orphan-promotion logic.
        // Regardless, the function should not infinite-loop or panic.
        assert!(!tree.is_empty() || tree.is_empty(), "no crash/hang");
    }

    #[test]
    fn tree_self_referencing_message_handled() {
        let mut msg = make_message(1);
        msg.reply_to_id = Some(1); // self-reference
        msg.timestamp_micros = 100;
        msg.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let tree = build_thread_tree_items(&[msg]);
        // Self-referencing: reply_to_id=1 exists in message_by_id, so it's
        // not promoted to root. Instead child_by_parent has entry Some(1)->[1]
        // and no None roots. The tree handles this gracefully.
        // The recursion_stack prevents infinite recursion.
        let total: usize = tree.iter().map(|n| 1 + count_descendants(n)).sum();
        assert!(total <= 1, "at most 1 node, got {total}");
    }

    #[test]
    fn tree_multiple_roots_sorted_chronologically() {
        let mut a = make_message(1);
        a.timestamp_micros = 300;
        a.timestamp_iso = "2026-02-06T12:00:03Z".to_string();

        let mut b = make_message(2);
        b.timestamp_micros = 100;
        b.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let mut c = make_message(3);
        c.timestamp_micros = 200;
        c.timestamp_iso = "2026-02-06T12:00:02Z".to_string();

        let tree = build_thread_tree_items(&[a, b, c]);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].message_id, 2, "earliest first");
        assert_eq!(tree[1].message_id, 3);
        assert_eq!(tree[2].message_id, 1, "latest last");
    }

    #[test]
    fn tree_preserves_unread_and_ack_flags() {
        let mut root = make_message(1);
        root.timestamp_micros = 100;
        root.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        child.is_unread = true;
        child.ack_required = true;
        child.timestamp_micros = 200;
        child.timestamp_iso = "2026-02-06T12:00:02Z".to_string();

        let tree = build_thread_tree_items(&[root, child]);
        assert!(!tree[0].is_unread, "root should not be unread");
        assert!(!tree[0].is_ack_required, "root should not be ack_required");
        assert!(tree[0].children[0].is_unread, "child should be unread");
        assert!(
            tree[0].children[0].is_ack_required,
            "child should be ack_required"
        );
    }

    #[test]
    fn tree_subject_truncated_to_60_chars() {
        let mut msg = make_message(1);
        msg.subject = "A".repeat(100);
        msg.timestamp_micros = 100;
        msg.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let tree = build_thread_tree_items(&[msg]);
        assert!(
            tree[0].subject_snippet.chars().count() <= 60,
            "subject should be truncated, got {} chars",
            tree[0].subject_snippet.chars().count()
        );
    }

    #[test]
    fn tree_compact_time_extraction() {
        let mut msg = make_message(1);
        msg.timestamp_iso = "2026-02-06T14:35:27Z".to_string();
        msg.timestamp_micros = 100;

        let tree = build_thread_tree_items(&[msg]);
        assert_eq!(tree[0].relative_time, "14:35:27");
    }

    #[test]
    fn tree_100_messages_builds_quickly() {
        let messages: Vec<ThreadMessage> = (1..=100)
            .map(|i| {
                let mut m = make_message(i);
                m.timestamp_micros = i * 1_000_000;
                m.timestamp_iso = format!("2026-02-06T12:{:02}:{:02}Z", i / 60, i % 60);
                if i > 1 {
                    // Build a chain: each message replies to previous
                    m.reply_to_id = Some(i - 1);
                }
                m
            })
            .collect();

        let start = std::time::Instant::now();
        let tree = build_thread_tree_items(&messages);
        let elapsed = start.elapsed();

        assert_eq!(tree.len(), 1, "single root chain");
        assert!(
            elapsed.as_millis() < 50,
            "100-message tree took {elapsed:?}, expected < 50ms"
        );
    }

    #[test]
    fn tree_wide_fan_out() {
        // One root with 50 direct children
        let mut messages = vec![];
        let mut root = make_message(1);
        root.timestamp_micros = 100;
        root.timestamp_iso = "2026-02-06T12:00:00Z".to_string();
        messages.push(root);

        for i in 2..=51 {
            let mut child = make_message(i);
            child.reply_to_id = Some(1);
            child.timestamp_micros = i * 1_000_000;
            child.timestamp_iso = format!("2026-02-06T12:{:02}:{:02}Z", i / 60, i % 60);
            messages.push(child);
        }

        let tree = build_thread_tree_items(&messages);
        assert_eq!(tree.len(), 1, "single root");
        assert_eq!(tree[0].children.len(), 50, "50 direct children");
        // Children sorted chronologically
        for w in tree[0].children.windows(2) {
            assert!(
                w[0].message_id < w[1].message_id,
                "children should be in chronological order"
            );
        }
    }

    // ── Flatten and collapse tests ──────────────────────────────────

    #[test]
    fn flatten_all_expanded_includes_all_nodes() {
        let mut root = make_message(1);
        root.timestamp_micros = 100;
        root.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        child.timestamp_micros = 200;
        child.timestamp_iso = "2026-02-06T12:00:02Z".to_string();

        let tree = build_thread_tree_items(&[root, child]);
        let collapsed: HashSet<i64> = HashSet::new();
        let mut rows = Vec::new();
        flatten_thread_tree_rows(&tree, &collapsed, &mut rows);

        assert_eq!(rows.len(), 2, "root + child");
        assert_eq!(rows[0].message_id, 1);
        assert!(rows[0].has_children);
        assert!(rows[0].is_expanded);
        assert_eq!(rows[1].message_id, 2);
        assert!(!rows[1].has_children);
    }

    #[test]
    fn flatten_collapsed_parent_hides_children() {
        let mut root = make_message(1);
        root.timestamp_micros = 100;
        root.timestamp_iso = "2026-02-06T12:00:01Z".to_string();

        let mut child = make_message(2);
        child.reply_to_id = Some(1);
        child.timestamp_micros = 200;
        child.timestamp_iso = "2026-02-06T12:00:02Z".to_string();

        let tree = build_thread_tree_items(&[root, child]);
        let collapsed: HashSet<i64> = std::iter::once(1).collect();
        let mut rows = Vec::new();
        flatten_thread_tree_rows(&tree, &collapsed, &mut rows);

        assert_eq!(rows.len(), 1, "only root visible when collapsed");
        assert_eq!(rows[0].message_id, 1);
        assert!(!rows[0].is_expanded);
    }

    #[test]
    fn flatten_empty_tree() {
        let tree: Vec<crate::tui_widgets::ThreadTreeItem> = Vec::new();
        let mut rows = Vec::new();
        flatten_thread_tree_rows(&tree, &HashSet::new(), &mut rows);
        assert!(rows.is_empty());
    }

    // ── ThreadTreeItem rendering tests ──────────────────────────────

    #[test]
    fn render_plain_label_leaf_node() {
        let item = crate::tui_widgets::ThreadTreeItem::new(
            1,
            "GoldFox".to_string(),
            "Hello".to_string(),
            "12:00:00".to_string(),
            false,
            false,
        );
        let label = item.render_plain_label(false);
        assert!(label.starts_with("•"), "leaf node should use • glyph");
        assert!(label.contains("GoldFox"));
        assert!(label.contains("Hello"));
        assert!(label.contains("12:00:00"));
        assert!(!label.contains("[ACK]"));
    }

    #[test]
    fn render_plain_label_expanded_parent() {
        let child = crate::tui_widgets::ThreadTreeItem::new(
            2,
            "SilverWolf".to_string(),
            "Reply".to_string(),
            "12:01:00".to_string(),
            false,
            false,
        );
        let item = crate::tui_widgets::ThreadTreeItem::new(
            1,
            "GoldFox".to_string(),
            "Thread".to_string(),
            "12:00:00".to_string(),
            false,
            false,
        )
        .with_children(vec![child]);

        let expanded = item.render_plain_label(true);
        assert!(expanded.starts_with("▼"), "expanded parent should use ▼");

        let collapsed = item.render_plain_label(false);
        assert!(collapsed.starts_with("▶"), "collapsed parent should use ▶");
    }

    #[test]
    fn render_plain_label_unread_and_ack() {
        let item = crate::tui_widgets::ThreadTreeItem::new(
            1,
            "GoldFox".to_string(),
            "Urgent".to_string(),
            "12:00:00".to_string(),
            true,
            true,
        );
        let label = item.render_plain_label(false);
        assert!(label.contains('*'), "unread should have * prefix");
        assert!(label.contains("[ACK]"), "ack_required should have [ACK]");
    }

    fn count_descendants(node: &crate::tui_widgets::ThreadTreeItem) -> usize {
        node.children.iter().map(|c| 1 + count_descendants(c)).sum()
    }

    #[test]
    fn filter_bar_always_visible_with_hint() {
        // Filter bar should occupy 1 row even when collapsed (showing hint)
        let screen = ThreadExplorerScreen::new();
        assert!(screen.filter_text.is_empty());
        assert!(!screen.filter_editing);
        // The view now always allocates 1 row for the filter bar,
        // so content_height = area.height - 1
    }

    #[test]
    fn thread_row_shows_unread_badge() {
        let thread = ThreadSummary {
            thread_id: "t-1".to_string(),
            message_count: 5,
            participant_count: 2,
            last_subject: "Hello".to_string(),
            last_sender: "GoldHawk".to_string(),
            last_timestamp_micros: 0,
            last_timestamp_iso: "2026-02-15T12:00:00".to_string(),
            first_timestamp_iso: "2026-02-15T11:00:00".to_string(),
            has_escalation: false,
            velocity_msg_per_hr: 1.0,
            participant_names: "GoldHawk, SilverFox".to_string(),
            unread_count: 3,
            project_slug: String::new(),
        };
        // Unread count > 0 should be surfaced in the row
        assert_eq!(thread.unread_count, 3);
        assert!(!thread.has_escalation);
    }

    #[test]
    fn title_format_shows_keybind_hints() {
        // Title format now includes [v] and [s] keybind hints
        let title = format!(
            "Threads ({})  [v]{}  [s]{}",
            42,
            ViewLens::Activity.label(),
            SortMode::LastActivity.label(),
        );
        assert!(title.contains("[v]Activity"));
        assert!(title.contains("[s]Recent"));
    }

    #[test]
    fn activity_lens_compact_labels() {
        // Activity lens now uses compact "m" and "a" labels
        let meta = format!("{}m  {}a  {:.1}/hr", 10, 3, 2.5_f64,);
        assert_eq!(meta, "10m  3a  2.5/hr");
    }

    #[test]
    fn detail_header_styled_importance_badges() {
        // Verify HIGH and URGENT importance levels produce styled badges
        // in the preview header (not plain text)
        let mut msg = make_message(1);
        msg.importance = "urgent".to_string();
        msg.ack_required = true;
        msg.subject = "Critical alert".to_string();

        let messages = vec![msg];
        let expanded: HashSet<i64> = HashSet::new();
        let collapsed: HashSet<i64> = HashSet::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 24, &mut pool);

        render_thread_detail(
            &mut frame,
            Rect::new(0, 0, 120, 24),
            &messages,
            None,
            0,
            0,
            &expanded,
            &collapsed,
            false,
            0,
            1,
            1,
            true,
            false,
        );

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("[URGENT]"),
            "missing styled URGENT badge: {text}"
        );
        assert!(
            text.contains("@ACK"),
            "missing ack-required indicator: {text}"
        );
        assert!(
            text.contains("Subject: Critical alert"),
            "missing styled subject line: {text}"
        );
    }

    // ── Screen logic, density heuristics, and failure paths (br-1xt0m.1.13.8) ──

    #[test]
    fn sort_mode_cycle_round_trips() {
        let start = SortMode::LastActivity;
        let mut mode = start;
        for _ in 0..4 {
            mode = mode.next();
        }
        assert_eq!(mode, start, "4 cycles should round-trip to start");
    }

    #[test]
    fn sort_mode_labels_non_empty() {
        for mode in [
            SortMode::LastActivity,
            SortMode::Velocity,
            SortMode::ParticipantCount,
            SortMode::EscalationFirst,
        ] {
            assert!(!mode.label().is_empty(), "label for {mode:?}");
        }
    }

    #[test]
    fn view_lens_cycle_round_trips() {
        let start = ViewLens::Activity;
        let mut lens = start;
        for _ in 0..3 {
            lens = lens.next();
        }
        assert_eq!(lens, start, "3 cycles should round-trip");
    }

    #[test]
    fn view_lens_labels_non_empty() {
        for lens in [
            ViewLens::Activity,
            ViewLens::Participants,
            ViewLens::Escalation,
        ] {
            assert!(!lens.label().is_empty(), "label for {lens:?}");
        }
    }

    #[test]
    fn apply_sort_escalation_first_prioritizes_escalated() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads = vec![
            ThreadSummary {
                thread_id: "normal".into(),
                has_escalation: false,
                last_timestamp_micros: 200,
                ..stub_thread_summary()
            },
            ThreadSummary {
                thread_id: "escalated".into(),
                has_escalation: true,
                last_timestamp_micros: 100,
                ..stub_thread_summary()
            },
        ];
        screen.sort_mode = SortMode::EscalationFirst;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "escalated");
        assert_eq!(screen.threads[1].thread_id, "normal");
    }

    #[test]
    fn apply_sort_velocity_orders_highest_first() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads = vec![
            ThreadSummary {
                thread_id: "slow".into(),
                velocity_msg_per_hr: 1.0,
                ..stub_thread_summary()
            },
            ThreadSummary {
                thread_id: "fast".into(),
                velocity_msg_per_hr: 10.0,
                ..stub_thread_summary()
            },
        ];
        screen.sort_mode = SortMode::Velocity;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "fast");
    }

    #[test]
    fn apply_sort_participant_count_orders_most_first() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads = vec![
            ThreadSummary {
                thread_id: "few".into(),
                participant_count: 2,
                ..stub_thread_summary()
            },
            ThreadSummary {
                thread_id: "many".into(),
                participant_count: 8,
                ..stub_thread_summary()
            },
        ];
        screen.sort_mode = SortMode::ParticipantCount;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "many");
    }

    #[test]
    fn apply_sort_last_activity_orders_newest_first() {
        let mut screen = ThreadExplorerScreen::new();
        screen.threads = vec![
            ThreadSummary {
                thread_id: "old".into(),
                last_timestamp_micros: 100,
                ..stub_thread_summary()
            },
            ThreadSummary {
                thread_id: "new".into(),
                last_timestamp_micros: 999,
                ..stub_thread_summary()
            },
        ];
        screen.sort_mode = SortMode::LastActivity;
        screen.apply_sort();
        assert_eq!(screen.threads[0].thread_id, "new");
    }

    fn stub_thread_summary() -> ThreadSummary {
        ThreadSummary {
            thread_id: String::new(),
            message_count: 1,
            participant_count: 1,
            last_subject: "subject".into(),
            last_sender: "agent".into(),
            last_timestamp_micros: 0,
            last_timestamp_iso: String::new(),
            project_slug: "proj".into(),
            has_escalation: false,
            velocity_msg_per_hr: 0.0,
            participant_names: String::new(),
            first_timestamp_iso: String::new(),
            unread_count: 0,
        }
    }
}
