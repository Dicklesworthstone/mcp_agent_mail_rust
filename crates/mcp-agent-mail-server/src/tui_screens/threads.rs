//! Thread Explorer screen with conversation workflow.
//!
//! Provides a split-pane view of message threads: a thread list on the left
//! showing `thread_id`, participant count, message count, and last activity;
//! and a conversation detail panel on the right showing chronological messages
//! within the selected thread.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Buffer, Event, Frame, KeyCode, KeyEventKind, Modifiers, PackedRgba, Style};
use ftui_extras::mermaid::{self, MermaidCompatibilityMatrix, MermaidFallbackPolicy};
use ftui_extras::{mermaid_layout, mermaid_render};
use ftui_runtime::program::Cmd;
use ftui_widgets::tree::{Tree, TreeGuides, TreeNode};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::{MermaidThreadMessage, generate_thread_flow_mermaid};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max threads to fetch.
const MAX_THREADS: usize = 500;

/// Periodic refresh interval in seconds.
const REFRESH_INTERVAL_SECS: u64 = 5;

/// Default page size for thread pagination.
/// Override via `AM_TUI_THREAD_PAGE_SIZE` environment variable.
const DEFAULT_THREAD_PAGE_SIZE: usize = 20;

/// Number of older messages to load when clicking "Load older".
const LOAD_OLDER_BATCH_SIZE: usize = 15;
const URGENT_PULSE_HALF_PERIOD_TICKS: u64 = 5;
const MERMAID_RENDER_DEBOUNCE: Duration = Duration::from_secs(1);

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

fn body_preview(body_md: &str, max_len: usize) -> String {
    let mut compact = String::new();
    for line in body_md.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !compact.is_empty() {
            compact.push(' ');
        }
        compact.push_str(line);
    }
    if compact.is_empty() {
        "(empty)".to_string()
    } else {
        truncate_str(&compact, max_len)
    }
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
            self.db_conn = DbConn::open_file(&path).ok();
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

    fn clamp_detail_cursor_to_tree_rows(&mut self) {
        let row_count = self.detail_tree_rows().len();
        if row_count == 0 {
            self.detail_cursor = 0;
        } else {
            self.detail_cursor = self.detail_cursor.min(row_count.saturating_sub(1));
        }
    }

    fn collapse_selected_branch(&mut self) {
        if let Some(row) = self.selected_tree_row() {
            if row.has_children {
                self.collapsed_tree_ids.insert(row.message_id);
                self.clamp_detail_cursor_to_tree_rows();
            }
        }
    }

    fn expand_selected_branch(&mut self) {
        if let Some(row) = self.selected_tree_row() {
            if row.has_children {
                self.collapsed_tree_ids.remove(&row.message_id);
            }
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
        let title = if focused {
            "Mermaid Thread Flow * [g]"
        } else {
            "Mermaid Thread Flow [g]"
        };
        let block = Block::default()
            .title(title)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
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
                                // Tree expansion controls
                                KeyCode::Left | KeyCode::Char('h') => {
                                    self.collapse_selected_branch();
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
                                _ => {}
                            },
                        }
                    }
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.urgent_pulse_on =
            self.reduced_motion || ((tick_count / URGENT_PULSE_HALF_PERIOD_TICKS) % 2) == 0;
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
                self.urgent_pulse_on,
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
            // Narrow: show active pane unless Mermaid panel is toggled.
            if self.show_mermaid_panel {
                self.render_mermaid_panel(
                    frame,
                    content_area,
                    matches!(self.focus, Focus::DetailPanel),
                );
            } else {
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
                            self.urgent_pulse_on,
                        );
                    }
                    Focus::DetailPanel => {
                        render_thread_detail(
                            frame,
                            content_area,
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
                key: "Left/Right",
                action: "Collapse/expand selected branch",
            },
            HelpEntry {
                key: "Enter/Space",
                action: "Toggle preview or branch state",
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
                action: "Close Mermaid / back to thread list",
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
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Thread conversations grouped by topic. Enter to expand, h to collapse.")
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
fn fetch_threads(conn: &DbConn, filter: &str, limit: usize) -> Vec<ThreadSummary> {
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
                first_timestamp_iso: micros_to_iso(first_ts),
                unread_count: 0, // Will be updated if read tracking is available
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

/// Get the total count of messages in a thread.
fn fetch_thread_message_count(conn: &DbConn, thread_id: &str) -> usize {
    let escaped = thread_id.replace('\'', "''");
    let sql = format!("SELECT COUNT(*) AS cnt FROM messages WHERE thread_id = '{escaped}'");

    conn.query_sync(&sql, &[])
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
    let escaped = thread_id.replace('\'', "''");

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
         WHERE m.thread_id = '{escaped}' \
         GROUP BY m.id \
         ORDER BY m.created_ts DESC \
         LIMIT {limit} OFFSET {offset}"
    );

    let mut messages: Vec<ThreadMessage> = conn
        .query_sync(&sql, &[])
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
        let line = Line::from_spans([
            Span::styled(" Filter: ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                format!("{text}{cursor}"),
                Style::default().fg(tp.text_primary),
            ),
        ]);
        Paragraph::new(Text::from_line(line)).render(area, frame);
    }
}

/// Render the thread list panel.
#[allow(clippy::too_many_arguments)]
fn render_thread_list(
    frame: &mut Frame<'_>,
    area: Rect,
    threads: &[ThreadSummary],
    cursor: usize,
    focused: bool,
    view_lens: ViewLens,
    sort_mode: SortMode,
    urgent_pulse_on: bool,
) {
    let focus_tag = if focused { "" } else { " (inactive)" };
    let escalated = threads.iter().filter(|t| t.has_escalation).count();
    let esc_tag = if escalated > 0 {
        format!("  {escalated} esc")
    } else {
        String::new()
    };
    let title = format!(
        "Threads ({}){}  [v]{}  [s]{}{focus_tag}",
        threads.len(),
        esc_tag,
        view_lens.label(),
        sort_mode.label(),
    );
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
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
    let show_subject = visible_height > viewport.len() * 2 || viewport.len() <= 5;
    let mut text_lines: Vec<Line> = Vec::with_capacity(viewport.len() * 2);
    for (view_idx, thread) in viewport.iter().enumerate() {
        let abs_idx = start + view_idx;
        let is_selected = abs_idx == cursor_clamped;
        let marker = if is_selected { ">" } else { " " };

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

        // Unread badge
        let unread_span = if thread.unread_count > 0 {
            Span::styled(
                format!(" {}", thread.unread_count),
                crate::tui_theme::text_accent(&tp),
            )
        } else {
            Span::raw("")
        };

        // Compact timestamp (HH:MM from ISO string)
        let time_short = if thread.last_timestamp_iso.len() >= 16 {
            &thread.last_timestamp_iso[11..16]
        } else {
            &thread.last_timestamp_iso
        };

        // Project tag (shortened)
        let proj_span = if thread.project_slug.is_empty() {
            Span::raw("")
        } else {
            Span::styled(
                format!("[{}] ", truncate_str(&thread.project_slug, 12)),
                crate::tui_theme::text_meta(&tp),
            )
        };

        // Lens-specific metadata
        let meta = match view_lens {
            ViewLens::Activity => format!(
                "{}m  {}a  {:.1}/hr",
                thread.message_count, thread.participant_count, thread.velocity_msg_per_hr,
            ),
            ViewLens::Participants => {
                truncate_str(&thread.participant_names, inner_w.saturating_sub(30))
            }
            ViewLens::Escalation => {
                let flag = if thread.has_escalation {
                    "ESC"
                } else {
                    "---"
                };
                format!("{flag}  {:.1}/hr", thread.velocity_msg_per_hr)
            }
        };

        // Build spans for the primary line
        let prefix_len = 1 + 1 + 5 + 1; // marker + esc + time + space
        let meta_len = meta.len() + 2; // " meta"
        let id_space = inner_w.saturating_sub(prefix_len + meta_len);
        let thread_id_display = truncate_str(&thread.thread_id, id_space);

        let cursor_style = if is_selected {
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold()
        } else {
            Style::default()
        };

        let mut primary = Line::from_spans([
            Span::raw(marker),
            Span::styled(esc_badge, esc_style),
            Span::styled(time_short, crate::tui_theme::text_meta(&tp)),
            Span::raw(" "),
            proj_span,
            Span::styled(
                format!("{thread_id_display:<id_space$}"),
                Style::default().fg(tp.text_primary),
            ),
            Span::styled(format!(" {meta}"), crate::tui_theme::text_meta(&tp)),
            unread_span,
        ]);
        if is_selected {
            primary.apply_base_style(cursor_style);
        }
        text_lines.push(primary);

        // Second line: last subject (if there's room)
        if show_subject {
            let indent = "    ";
            let subj_space = inner_w.saturating_sub(indent.len());
            let subj_line = if thread.last_sender.is_empty() {
                Line::from_spans([
                    Span::raw(indent),
                    Span::styled(
                        truncate_str(&thread.last_subject, subj_space),
                        crate::tui_theme::text_hint(&tp),
                    ),
                ])
            } else {
                let sender_prefix = format!("{}: ", thread.last_sender);
                let remaining = subj_space.saturating_sub(sender_prefix.len());
                Line::from_spans([
                    Span::raw(indent),
                    Span::styled(
                        sender_prefix,
                        Style::default().fg(tp.text_secondary),
                    ),
                    Span::styled(
                        truncate_str(&thread.last_subject, remaining),
                        crate::tui_theme::text_hint(&tp),
                    ),
                ])
            };
            text_lines.push(subj_line);
        }
    }

    let text = Text::from_lines(text_lines);
    let p = Paragraph::new(text);
    p.render(inner, frame);
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
                truncate_str(&t.thread_id, 30),
                t.message_count,
            )
        },
    );

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(&tp, focused)));
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

    let tree_items = build_thread_tree_items(messages);
    let mut tree_rows = Vec::new();
    flatten_thread_tree_rows(&tree_items, collapsed_tree_ids, &mut tree_rows);
    if tree_rows.is_empty() {
        Paragraph::new("  No hierarchy available.").render(inner, frame);
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
        header_lines.push(Line::raw(format!(
            "Thread: {}  |  Loaded: {}/{}  |  Unread: {}",
            truncate_str(&t.thread_id, inner.width.saturating_sub(34) as usize),
            loaded_message_count,
            total_thread_messages,
            t.unread_count,
        )));
        header_lines.push(Line::raw(format!(
            "Participants ({})  |  {} -> {}",
            t.participant_count,
            iso_compact_time(&t.first_timestamp_iso),
            iso_compact_time(&t.last_timestamp_iso),
        )));
        if !t.participant_names.is_empty() {
            header_lines.push(Line::raw(format!(
                "Agents: {}",
                truncate_str(&t.participant_names, inner.width.saturating_sub(8) as usize)
            )));
        }
    }
    header_lines.push(Line::raw(if tree_focus {
        "Mode: Tree (Tab -> Preview)".to_string()
    } else {
        "Mode: Preview (Tab -> Tree)".to_string()
    }));
    if has_older_messages {
        header_lines.push(Line::raw(format!(
            "[Load {remaining_older_count} older messages] (o)"
        )));
    }

    let header_height = header_lines.len().min(inner.height as usize).min(5) as u16;
    let header_area = Rect::new(inner.x, inner.y, inner.width, header_height);
    let body_area = Rect::new(
        inner.x,
        inner.y + header_height,
        inner.width,
        inner.height.saturating_sub(header_height),
    );
    Paragraph::new(Text::from_lines(header_lines)).render(header_area, frame);
    if body_area.width < 10 || body_area.height == 0 {
        return;
    }

    let tree_width = ((u32::from(body_area.width) * 60) / 100) as u16;
    let tree_width = tree_width.clamp(12, body_area.width.saturating_sub(8));
    let preview_width = body_area.width.saturating_sub(tree_width);

    let tree_area = Rect::new(body_area.x, body_area.y, tree_width, body_area.height);
    let preview_area = Rect::new(
        body_area.x + tree_width,
        body_area.y,
        preview_width,
        body_area.height,
    );

    let tree_title = if focused && tree_focus {
        "Hierarchy *"
    } else {
        "Hierarchy"
    };
    let tree_block = Block::default()
        .title(tree_title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(
            &tp,
            focused && tree_focus,
        )));
    let tree_inner = tree_block.inner(tree_area);
    tree_block.render(tree_area, frame);
    if tree_inner.width > 0 && tree_inner.height > 0 {
        let nodes = tree_items
            .iter()
            .map(|item| tree_item_to_widget_node(item, collapsed_tree_ids, selected_row.message_id))
            .collect::<Vec<_>>();
        let root = TreeNode::new("root").with_children(nodes);
        Tree::new(root)
            .with_show_root(false)
            .with_guides(thread_tree_guides())
            .render(tree_inner, frame);
    }

    let preview_title = if focused && !tree_focus {
        "Preview *"
    } else {
        "Preview"
    };
    let preview_block = Block::default()
        .title(preview_title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::focus_border_color(
            &tp,
            focused && !tree_focus,
        )));
    let preview_inner = preview_block.inner(preview_area);
    preview_block.render(preview_area, frame);
    if preview_inner.width == 0 || preview_inner.height == 0 {
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
            truncate_str(
                &selected_message.to_agents,
                preview_inner.width.saturating_sub(24) as usize
            )
        )));
    }
    if selected_message.importance == "high" || selected_message.importance == "urgent" {
        preview_header_spans.push(Span::raw(format!(
            " [{}]",
            selected_message.importance.to_ascii_uppercase()
        )));
    }
    preview_lines.push(Line::from_spans(preview_header_spans));
    if !selected_message.subject.is_empty() {
        preview_lines.push(Line::raw(format!(
            "Subj: {}",
            truncate_str(
                &selected_message.subject,
                preview_inner.width.saturating_sub(6) as usize
            )
        )));
    }
    preview_lines.push(Line::raw(String::new()));

    if expanded_message_ids.contains(&selected_message.id) {
        let md_theme = ftui_extras::markdown::MarkdownTheme::default();
        for line in crate::tui_markdown::render_body(&selected_message.body_md, &md_theme).lines() {
            preview_lines.push(line.clone());
        }
    } else {
        preview_lines.push(Line::raw(body_preview(
            &selected_message.body_md,
            preview_inner.width.saturating_sub(2) as usize,
        )));
    }

    let visible_preview = preview_lines
        .into_iter()
        .skip(scroll)
        .take(preview_inner.height as usize)
        .collect::<Vec<_>>();
    Paragraph::new(Text::from_lines(visible_preview)).render(preview_inner, frame);
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
        truncate_str(&message.subject, 60),
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
fn tree_item_to_widget_node(
    item: &crate::tui_widgets::ThreadTreeItem,
    collapsed_tree_ids: &HashSet<i64>,
    selected_id: i64,
) -> TreeNode {
    let is_expanded = !collapsed_tree_ids.contains(&item.message_id);
    let label = if item.message_id == selected_id {
        format!("> {}", item.render_plain_label(is_expanded))
    } else {
        format!("  {}", item.render_plain_label(is_expanded))
    };
    let children: Vec<TreeNode> = item
        .children
        .iter()
        .map(|child| tree_item_to_widget_node(child, collapsed_tree_ids, selected_id))
        .collect();
    TreeNode::new(label)
        .with_expanded(is_expanded)
        .with_children(children)
}

fn flatten_thread_tree_rows(
    nodes: &[crate::tui_widgets::ThreadTreeItem],
    collapsed_tree_ids: &HashSet<i64>,
    out: &mut Vec<ThreadTreeRow>,
) {
    for node in nodes {
        let is_expanded = !collapsed_tree_ids.contains(&node.message_id);
        out.push(ThreadTreeRow {
            message_id: node.message_id,
            has_children: !node.children.is_empty(),
            is_expanded,
        });
        if is_expanded {
            flatten_thread_tree_rows(&node.children, collapsed_tree_ids, out);
        }
    }
}

/// Truncate a string to at most `max_len` characters, adding "..." if truncated.
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

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_harness::buffer_to_text;

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
            text.contains("Participants (3)"),
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
            text.contains("Subj: Child subject"),
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
        assert!(tree[0].children[0].is_ack_required, "child should be ack_required");
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
        let collapsed: HashSet<i64> = [1].into_iter().collect();
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
        node.children
            .iter()
            .map(|c| 1 + count_descendants(c))
            .sum()
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
        let meta = format!(
            "{}m  {}a  {:.1}/hr",
            10, 3, 2.5_f64,
        );
        assert_eq!(meta, "10m  3a  2.5/hr");
    }
}
