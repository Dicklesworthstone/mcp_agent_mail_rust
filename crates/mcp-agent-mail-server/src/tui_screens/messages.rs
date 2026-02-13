//! Message Browser screen with search bar, results list, and detail panel.
//!
//! Provides full-text search across all messages via FTS5 and live event
//! stream search.  Results are displayed in a split-pane layout with
//! keyboard-first navigation.

use std::cell::RefCell;
use std::time::Instant;

use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, Style};
use ftui_runtime::program::Cmd;
use ftui_widgets::StatefulWidget;
use ftui_widgets::input::TextInput;
use ftui_widgets::virtualized::{RenderItem, VirtualizedList, VirtualizedListState};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_action_menu::{ActionEntry, messages_actions};
use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEventKind;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Number of results per page.
const PAGE_SIZE: usize = 50;

/// Debounce delay in ticks (each tick ~100ms, so 2 ticks = ~200ms).
const DEBOUNCE_TICKS: u8 = 2;

/// Max results to cache.
const MAX_RESULTS: usize = 1000;

/// Max body preview length in the results list (used for future
/// inline preview in narrow mode).
#[allow(dead_code)]
const BODY_PREVIEW_LEN: usize = 80;

// ──────────────────────────────────────────────────────────────────────
// Query presets — reusable filter shortcuts
// ──────────────────────────────────────────────────────────────────────

/// A named query preset for quick search access.
#[derive(Debug, Clone)]
struct QueryPreset {
    /// Display label (shown in status bar).
    label: &'static str,
    /// The query string to inject into the search bar.
    query: &'static str,
    /// Short description for help overlay (shown in preset picker).
    #[allow(dead_code)]
    description: &'static str,
}

/// Built-in presets cycled with `p` key.
const QUERY_PRESETS: &[QueryPreset] = &[
    QueryPreset {
        label: "All",
        query: "",
        description: "Show all recent messages",
    },
    QueryPreset {
        label: "Urgent",
        query: "urgent",
        description: "Urgent importance messages",
    },
    QueryPreset {
        label: "High",
        query: "high",
        description: "High importance messages",
    },
    QueryPreset {
        label: "Ack",
        query: "ack",
        description: "Messages requiring acknowledgement",
    },
    QueryPreset {
        label: "Error",
        query: "error",
        description: "Messages containing error",
    },
    QueryPreset {
        label: "Plan",
        query: "plan",
        description: "Planning and coordination messages",
    },
];

/// Describes how the last search was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMethod {
    /// No search executed yet.
    None,
    /// Showing recent messages (empty query).
    Recent,
    /// FTS5 full-text match.
    Fts,
    /// LIKE fallback (FTS returned no results).
    LikeFallback,
}

// ──────────────────────────────────────────────────────────────────────
// MessageEntry — a single search result
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct MessageEntry {
    id: i64,
    subject: String,
    from_agent: String,
    to_agents: String,
    project_slug: String,
    thread_id: String,
    timestamp_iso: String,
    /// Raw timestamp for sorting/comparison (pre-wired for br-10wc.6.3).
    #[allow(dead_code)]
    timestamp_micros: i64,
    body_md: String,
    importance: String,
    ack_required: bool,
    /// Whether to display the project column (true in Global mode).
    show_project: bool,
}

impl RenderItem for MessageEntry {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool) {
        use ftui::widgets::Widget;
        if area.height == 0 || area.width < 10 {
            return;
        }
        let inner_w = area.width as usize;

        // Marker for selected row
        let marker = if selected { '>' } else { ' ' };
        let cursor_style = Style::default().bold().reverse();

        // Importance badge
        let badge = match self.importance.as_str() {
            "high" => "!",
            "urgent" => "!!",
            _ => " ",
        };

        // ID or "LIVE" marker
        let id_str = if self.id >= 0 {
            format!("#{}", self.id)
        } else {
            "LIVE".to_string()
        };

        // Compact timestamp (HH:MM:SS from ISO string)
        let time_short = if self.timestamp_iso.len() >= 19 {
            &self.timestamp_iso[11..19]
        } else {
            &self.timestamp_iso
        };

        // Project badge (only in Global mode)
        let project_badge = if self.show_project && !self.project_slug.is_empty() {
            // Show first 8 chars of project slug
            let slug = if self.project_slug.len() > 8 {
                &self.project_slug[..8]
            } else {
                &self.project_slug
            };
            format!("[{slug:>8}] ")
        } else {
            String::new()
        };

        let prefix = format!("{marker} {badge:>2} {id_str:>6} {time_short} {project_badge}");
        let remaining = inner_w.saturating_sub(prefix.len());
        let subj = truncate_str(&self.subject, remaining);

        let mut line = Line::from_spans([Span::raw(format!("{prefix}{subj}"))]);
        if selected {
            line.apply_base_style(cursor_style);
        }
        let paragraph = Paragraph::new(Text::from_line(line));
        paragraph.render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

// ──────────────────────────────────────────────────────────────────────
// Inbox mode: Local vs Global
// ──────────────────────────────────────────────────────────────────────

/// Viewing mode for the Messages screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InboxMode {
    /// Show messages from a single project.
    Local(String),
    /// Show messages from ALL projects.
    #[default]
    Global,
}

impl InboxMode {
    /// Display label for the mode indicator.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Local(slug) => format!("Local: {slug}"),
            Self::Global => "Global: all projects".to_string(),
        }
    }

    /// True if in Global mode.
    #[must_use]
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Focus state
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    SearchBar,
    ResultList,
}

// ──────────────────────────────────────────────────────────────────────
// MessageBrowserScreen
// ──────────────────────────────────────────────────────────────────────

/// Full-text search and browsing across all messages.
pub struct MessageBrowserScreen {
    search_input: TextInput,
    results: Vec<MessageEntry>,
    cursor: usize,
    detail_scroll: usize,
    focus: Focus,
    /// VirtualizedList state for efficient rendering.
    list_state: RefCell<VirtualizedListState>,
    /// Last search term that was actually executed.
    last_search: String,
    /// Ticks remaining before executing a search after input changes.
    debounce_remaining: u8,
    /// Whether we need to re-query.
    search_dirty: bool,
    /// Lazy-opened DB connection for message queries.
    db_conn: Option<DbConn>,
    /// Whether we attempted to open the DB connection.
    db_conn_attempted: bool,
    /// Total result count (may be more than `results.len()`).
    total_results: usize,
    /// Last tick we refreshed (for periodic refresh of empty-query mode).
    last_refresh: Option<Instant>,
    /// Current preset index (0 = "All" / no preset).
    preset_index: usize,
    /// How the last search was resolved (for explainability).
    search_method: SearchMethod,
    /// Synthetic event for the focused message (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
    /// Inbox mode: Local (single project) or Global (all projects).
    inbox_mode: InboxMode,
    /// Last active project slug when switching from Local to Global
    /// (used to restore when switching back).
    last_local_project: Option<String>,
}

impl MessageBrowserScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            search_input: TextInput::new()
                .with_placeholder("Search messages... (/ to focus)")
                .with_focused(false),
            results: Vec::new(),
            cursor: 0,
            detail_scroll: 0,
            focus: Focus::ResultList,
            list_state: RefCell::new(VirtualizedListState::default()),
            last_search: String::new(),
            debounce_remaining: 0,
            search_dirty: true, // Initial load
            db_conn: None,
            db_conn_attempted: false,
            total_results: 0,
            last_refresh: None,
            preset_index: 0,
            search_method: SearchMethod::None,
            focused_synthetic: None,
            inbox_mode: InboxMode::Global,
            last_local_project: None,
        }
    }

    /// Toggle between Local and Global inbox modes.
    ///
    /// When switching Global -> Local, uses the last known project or the
    /// currently focused message's project. When switching Local -> Global,
    /// remembers the current project for later restoration.
    fn toggle_inbox_mode(&mut self) {
        match &self.inbox_mode {
            InboxMode::Global => {
                // Switch to Local mode
                // Use the last local project, or infer from the focused message
                let project_slug = self
                    .last_local_project
                    .clone()
                    .or_else(|| {
                        self.results
                            .get(self.cursor)
                            .map(|m| m.project_slug.clone())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| "default".to_string());
                self.inbox_mode = InboxMode::Local(project_slug);
            }
            InboxMode::Local(slug) => {
                // Remember current project before switching to Global
                self.last_local_project = Some(slug.clone());
                self.inbox_mode = InboxMode::Global;
            }
        }
        // Trigger a re-query with the new mode
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    /// Sync the VirtualizedListState with our cursor position.
    fn sync_list_state(&self) {
        let mut state = self.list_state.borrow_mut();
        state.select(Some(self.cursor));
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected message.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self.results.get(self.cursor).map(|entry| {
            crate::tui_events::MailEvent::message_sent(
                entry.id,
                &entry.from_agent,
                entry.to_agents.split(", ").map(String::from).collect(),
                &entry.subject,
                &entry.thread_id,
                &entry.project_slug,
            )
        });
    }

    /// Apply a query preset by index, injecting its query into the search bar.
    fn apply_preset(&mut self, index: usize) {
        let idx = index % QUERY_PRESETS.len();
        self.preset_index = idx;
        let preset = &QUERY_PRESETS[idx];
        self.search_input.set_value(preset.query);
        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    /// Return the current active preset, if any.
    fn active_preset(&self) -> &QueryPreset {
        &QUERY_PRESETS[self.preset_index]
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

    /// Execute a search query against the database.
    fn execute_search(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let Some(conn) = &self.db_conn else {
            return;
        };

        let query = self.search_input.value().trim().to_string();
        self.last_refresh = Some(Instant::now());

        // Determine if we should show project column (Global mode)
        let show_project = self.inbox_mode.is_global();

        // Get optional project filter for Local mode
        let project_filter = match &self.inbox_mode {
            InboxMode::Local(slug) => Some(slug.as_str()),
            InboxMode::Global => None,
        };

        let (results, total, method) = if query.is_empty() {
            self.last_search.clear();
            let (r, t) = fetch_recent_messages(conn, PAGE_SIZE, project_filter, show_project);
            (r, t, SearchMethod::Recent)
        } else {
            self.last_search.clone_from(&query);
            let (r, t, m) =
                search_messages_fts(conn, &query, MAX_RESULTS, project_filter, show_project);
            (r, t, m)
        };
        self.search_method = method;

        self.results = results;
        self.total_results = total;

        // Clamp cursor
        if self.results.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.results.len() - 1);
        }
        self.detail_scroll = 0;
        self.search_dirty = false;
    }

    /// Also search the live event ring buffer for `MessageSent`/`MessageReceived` events.
    fn search_live_events(state: &TuiSharedState, query: &str) -> Vec<MessageEntry> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();
        let events = state.recent_events(500);
        events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind(),
                    MailEventKind::MessageSent | MailEventKind::MessageReceived
                )
            })
            .filter_map(|e| {
                let summary = format!("{e:?}");
                if summary.to_lowercase().contains(&query_lower) {
                    // Extract what we can from the MailEvent
                    Some(MessageEntry {
                        id: -1, // Live events don't have DB IDs
                        subject: format!("[LIVE] {:?}", e.kind()),
                        from_agent: String::new(),
                        to_agents: String::new(),
                        project_slug: String::new(),
                        thread_id: String::new(),
                        timestamp_iso: micros_to_iso(e.timestamp_micros()),
                        timestamp_micros: e.timestamp_micros(),
                        body_md: summary,
                        importance: "normal".to_string(),
                        ack_required: false,
                        show_project: false,
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

impl Default for MessageBrowserScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for MessageBrowserScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match self.focus {
                    Focus::SearchBar => {
                        match key.code {
                            KeyCode::Enter => {
                                // Execute search immediately and switch to results
                                self.search_dirty = true;
                                self.debounce_remaining = 0;
                                self.focus = Focus::ResultList;
                                self.search_input.set_focused(false);
                                return Cmd::None;
                            }
                            KeyCode::Escape | KeyCode::Tab => {
                                self.focus = Focus::ResultList;
                                self.search_input.set_focused(false);
                                return Cmd::None;
                            }
                            _ => {
                                let before = self.search_input.value().to_string();
                                self.search_input.handle_event(event);
                                if self.search_input.value() != before {
                                    self.search_dirty = true;
                                    self.debounce_remaining = DEBOUNCE_TICKS;
                                }
                                return Cmd::None;
                            }
                        }
                    }
                    Focus::ResultList => {
                        match key.code {
                            // Enter search mode
                            KeyCode::Char('/') | KeyCode::Tab => {
                                self.focus = Focus::SearchBar;
                                self.search_input.set_focused(true);
                                return Cmd::None;
                            }
                            // Cursor navigation
                            KeyCode::Char('j') | KeyCode::Down => {
                                if !self.results.is_empty() {
                                    self.cursor = (self.cursor + 1).min(self.results.len() - 1);
                                    self.detail_scroll = 0;
                                }
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.cursor = self.cursor.saturating_sub(1);
                                self.detail_scroll = 0;
                            }
                            KeyCode::Char('G') | KeyCode::End => {
                                if !self.results.is_empty() {
                                    self.cursor = self.results.len() - 1;
                                    self.detail_scroll = 0;
                                }
                            }
                            KeyCode::Home => {
                                self.cursor = 0;
                                self.detail_scroll = 0;
                            }
                            // Toggle inbox mode (Local/Global)
                            KeyCode::Char('g') => {
                                self.toggle_inbox_mode();
                                return Cmd::None;
                            }
                            // Page navigation
                            KeyCode::Char('d') | KeyCode::PageDown => {
                                if !self.results.is_empty() {
                                    self.cursor = (self.cursor + 20).min(self.results.len() - 1);
                                    self.detail_scroll = 0;
                                }
                            }
                            KeyCode::Char('u') | KeyCode::PageUp => {
                                self.cursor = self.cursor.saturating_sub(20);
                                self.detail_scroll = 0;
                            }
                            // Detail scroll
                            KeyCode::Char('J') => {
                                self.detail_scroll += 1;
                            }
                            KeyCode::Char('K') => {
                                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                            }
                            // Deep-link: jump to timeline at message timestamp
                            KeyCode::Enter => {
                                if let Some(entry) = self.results.get(self.cursor) {
                                    return Cmd::msg(MailScreenMsg::DeepLink(
                                        DeepLinkTarget::TimelineAtTime(entry.timestamp_micros),
                                    ));
                                }
                            }
                            // Cycle query presets
                            KeyCode::Char('p') => {
                                self.apply_preset(self.preset_index + 1);
                            }
                            KeyCode::Char('P') => {
                                let idx = if self.preset_index == 0 {
                                    QUERY_PRESETS.len() - 1
                                } else {
                                    self.preset_index - 1
                                };
                                self.apply_preset(idx);
                            }
                            // Clear search
                            KeyCode::Char('c') if key.modifiers.contains(Modifiers::CTRL) => {
                                self.search_input.clear();
                                self.search_dirty = true;
                                self.debounce_remaining = 0;
                                self.preset_index = 0;
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
        // Debounce search execution
        if self.search_dirty {
            if self.debounce_remaining > 0 {
                self.debounce_remaining -= 1;
            } else {
                self.execute_search(state);
            }
        }

        // Periodic refresh for empty-query mode (every 5 seconds)
        if self.search_input.value().is_empty() {
            let should_refresh = self.last_refresh.is_none_or(|t| t.elapsed().as_secs() >= 5);
            if should_refresh {
                self.search_dirty = true;
                self.debounce_remaining = 0;
            }
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        match target {
            DeepLinkTarget::MessageById(id) => {
                // Find message by ID and move cursor to it
                if let Some(pos) = self.results.iter().position(|m| m.id == *id) {
                    self.cursor = pos;
                    self.detail_scroll = 0;
                    self.focus = Focus::ResultList;
                    self.search_input.set_focused(false);
                }
                true
            }
            _ => false,
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 4 || area.width < 20 {
            return;
        }

        // Layout: 1 row search bar, remaining split into results + detail
        let search_height: u16 = 3; // border + input + border
        let content_height = area.height.saturating_sub(search_height);

        let search_area = Rect::new(area.x, area.y, area.width, search_height);
        let content_area = Rect::new(area.x, area.y + search_height, area.width, content_height);

        // Render search bar with explainability and mode indicator
        let method_label = match self.search_method {
            SearchMethod::None => "",
            SearchMethod::Recent => "recent",
            SearchMethod::Fts => "FTS",
            SearchMethod::LikeFallback => "LIKE",
        };
        let preset_label = if self.preset_index > 0 {
            self.active_preset().label
        } else {
            ""
        };
        let mode_label = self.inbox_mode.label();
        render_search_bar(
            frame,
            search_area,
            &self.search_input,
            self.total_results,
            matches!(self.focus, Focus::SearchBar),
            method_label,
            preset_label,
            &mode_label,
        );

        // Split content: 45% results, 55% detail (if wide enough)
        if content_area.width >= 80 {
            let results_width = content_area.width * 45 / 100;
            let detail_width = content_area.width - results_width;
            let results_area = Rect::new(
                content_area.x,
                content_area.y,
                results_width,
                content_area.height,
            );
            let detail_area = Rect::new(
                content_area.x + results_width,
                content_area.y,
                detail_width,
                content_area.height,
            );

            // Sync and borrow list state for rendering
            self.sync_list_state();
            let mut list_state = self.list_state.borrow_mut();
            render_results_list(frame, results_area, &self.results, &mut list_state);
            drop(list_state);
            render_detail_panel(
                frame,
                detail_area,
                self.results.get(self.cursor),
                self.detail_scroll,
            );
        } else {
            // Narrow: results only
            self.sync_list_state();
            let mut list_state = self.list_state.borrow_mut();
            render_results_list(frame, content_area, &self.results, &mut list_state);
        }

        // Also merge live events into display if searching
        let _live_results = Self::search_live_events(state, self.search_input.value());
        // Live results displayed as annotations in the results list
        // (full integration deferred to br-10wc.6.3)
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "/",
                action: "Search",
            },
            HelpEntry {
                key: "j/k",
                action: "Navigate results",
            },
            HelpEntry {
                key: "d/u",
                action: "Page down/up",
            },
            HelpEntry {
                key: "G/Home",
                action: "End / Start",
            },
            HelpEntry {
                key: "g",
                action: "Toggle Local/Global",
            },
            HelpEntry {
                key: "Enter",
                action: "Jump to timeline",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
            },
            HelpEntry {
                key: "Tab",
                action: "Toggle focus",
            },
            HelpEntry {
                key: "Esc",
                action: "Exit search",
            },
            HelpEntry {
                key: "Ctrl+C",
                action: "Clear search",
            },
            HelpEntry {
                key: "p/P",
                action: "Next/prev preset",
            },
        ]
    }

    fn consumes_text_input(&self) -> bool {
        matches!(self.focus, Focus::SearchBar)
    }

    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        let message = self.results.get(self.cursor)?;

        let thread_id = if message.thread_id.is_empty() {
            None
        } else {
            Some(message.thread_id.as_str())
        };

        let actions = messages_actions(message.id, thread_id, &message.from_agent);

        // Anchor row is cursor position + header offset
        let anchor_row = (self.cursor as u16).saturating_add(3);
        let context_id = message.id.to_string();

        Some((actions, anchor_row, context_id))
    }

    fn title(&self) -> &'static str {
        "Messages"
    }

    fn tab_label(&self) -> &'static str {
        "Msg"
    }
}

// ──────────────────────────────────────────────────────────────────────
// DB query helpers
// ──────────────────────────────────────────────────────────────────────

/// Fetch recent messages (empty query mode).
///
/// If `project_filter` is Some, only fetch messages from that project (Local mode).
/// Otherwise, fetch from all projects (Global mode).
fn fetch_recent_messages(
    conn: &DbConn,
    limit: usize,
    project_filter: Option<&str>,
    show_project: bool,
) -> (Vec<MessageEntry>, usize) {
    let where_clause = if let Some(slug) = project_filter {
        let escaped_slug = slug.replace('\'', "''");
        format!("WHERE p.slug = '{escaped_slug}'")
    } else {
        String::new()
    };

    let sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.thread_id, m.importance, m.ack_required, \
         m.created_ts, \
         a_sender.name AS sender_name, \
         p.slug AS project_slug, \
         COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
         FROM messages m \
         JOIN agents a_sender ON a_sender.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
         LEFT JOIN message_recipients mr ON mr.message_id = m.id \
         LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
         {where_clause} \
         GROUP BY m.id \
         ORDER BY m.created_ts DESC \
         LIMIT {limit}"
    );

    let total = count_messages(conn, project_filter);
    let results = query_messages(conn, &sql, show_project);
    (results, total)
}

/// Full-text search using FTS5, returning results and the search method used.
///
/// If `project_filter` is Some, only search within that project (Local mode).
/// Otherwise, search across all projects (Global mode).
fn search_messages_fts(
    conn: &DbConn,
    query: &str,
    limit: usize,
    project_filter: Option<&str>,
    show_project: bool,
) -> (Vec<MessageEntry>, usize, SearchMethod) {
    // Sanitize the FTS query
    let sanitized = sanitize_fts_query(query);
    if sanitized.is_empty() {
        return (Vec::new(), 0, SearchMethod::LikeFallback);
    }

    // Build project filter for SQL
    let project_condition = if let Some(slug) = project_filter {
        let escaped_slug = slug.replace('\'', "''");
        format!("AND p.slug = '{escaped_slug}'")
    } else {
        String::new()
    };

    let sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.thread_id, m.importance, m.ack_required, \
         m.created_ts, \
         a_sender.name AS sender_name, \
         p.slug AS project_slug, \
         COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
         FROM fts_messages fts \
         JOIN messages m ON m.id = fts.message_id \
         JOIN agents a_sender ON a_sender.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
         LEFT JOIN message_recipients mr ON mr.message_id = m.id \
         LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
         WHERE fts_messages MATCH '{sanitized}' {project_condition} \
         GROUP BY m.id \
         ORDER BY rank \
         LIMIT {limit}"
    );

    // Try FTS first, fall back to LIKE
    let results = query_messages(conn, &sql, show_project);
    if !results.is_empty() {
        let total = results.len();
        return (results, total, SearchMethod::Fts);
    }

    // LIKE fallback
    let escaped = query.replace('\'', "''");
    let like_where = if let Some(slug) = project_filter {
        let escaped_slug = slug.replace('\'', "''");
        format!(
            "WHERE (m.subject LIKE '%{escaped}%' OR m.body_md LIKE '%{escaped}%') \
             AND p.slug = '{escaped_slug}'"
        )
    } else {
        format!("WHERE m.subject LIKE '%{escaped}%' OR m.body_md LIKE '%{escaped}%'")
    };

    let like_sql = format!(
        "SELECT m.id, m.subject, m.body_md, m.thread_id, m.importance, m.ack_required, \
         m.created_ts, \
         a_sender.name AS sender_name, \
         p.slug AS project_slug, \
         COALESCE(GROUP_CONCAT(DISTINCT a_recip.name), '') AS to_agents \
         FROM messages m \
         JOIN agents a_sender ON a_sender.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
         LEFT JOIN message_recipients mr ON mr.message_id = m.id \
         LEFT JOIN agents a_recip ON a_recip.id = mr.agent_id \
         {like_where} \
         GROUP BY m.id \
         ORDER BY m.created_ts DESC \
         LIMIT {limit}"
    );

    let results = query_messages(conn, &like_sql, show_project);
    let total = results.len();
    (results, total, SearchMethod::LikeFallback)
}

/// Execute a message query and extract rows into `MessageEntry` structs.
fn query_messages(conn: &DbConn, sql: &str, show_project: bool) -> Vec<MessageEntry> {
    conn.query_sync(sql, &[])
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let created_ts = row.get_named::<i64>("created_ts").ok()?;
                    Some(MessageEntry {
                        id: row.get_named::<i64>("id").ok()?,
                        subject: row.get_named::<String>("subject").ok().unwrap_or_default(),
                        from_agent: row
                            .get_named::<String>("sender_name")
                            .ok()
                            .unwrap_or_default(),
                        to_agents: row
                            .get_named::<String>("to_agents")
                            .ok()
                            .unwrap_or_default(),
                        project_slug: row
                            .get_named::<String>("project_slug")
                            .ok()
                            .unwrap_or_default(),
                        thread_id: row
                            .get_named::<String>("thread_id")
                            .ok()
                            .unwrap_or_default(),
                        timestamp_iso: micros_to_iso(created_ts),
                        timestamp_micros: created_ts,
                        body_md: row.get_named::<String>("body_md").ok().unwrap_or_default(),
                        importance: row
                            .get_named::<String>("importance")
                            .ok()
                            .unwrap_or_else(|| "normal".to_string()),
                        ack_required: row.get_named::<i64>("ack_required").ok().unwrap_or(0) != 0,
                        show_project,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Count total messages, optionally filtered by project.
fn count_messages(conn: &DbConn, project_filter: Option<&str>) -> usize {
    let sql = if let Some(slug) = project_filter {
        let escaped_slug = slug.replace('\'', "''");
        format!(
            "SELECT COUNT(*) AS c FROM messages m \
             JOIN projects p ON p.id = m.project_id \
             WHERE p.slug = '{escaped_slug}'"
        )
    } else {
        "SELECT COUNT(*) AS c FROM messages".to_string()
    };

    conn.query_sync(&sql, &[])
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<i64>("c").ok())
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0)
}

/// Sanitize an FTS5 query to prevent syntax errors.
///
/// Removes FTS5 operators and wraps tokens in double quotes.
fn sanitize_fts_query(query: &str) -> String {
    let mut tokens = Vec::new();
    for word in query.split_whitespace() {
        // Skip FTS5 operators
        let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
        if w.is_empty()
            || w.eq_ignore_ascii_case("AND")
            || w.eq_ignore_ascii_case("OR")
            || w.eq_ignore_ascii_case("NOT")
            || w.eq_ignore_ascii_case("NEAR")
        {
            continue;
        }
        // Quote the token
        let escaped = w.replace('"', "");
        tokens.push(format!("\"{escaped}\""));
    }
    tokens.join(" ")
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the search bar with explainability metadata and mode indicator.
fn render_search_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &TextInput,
    total_results: usize,
    focused: bool,
    method_label: &str,
    preset_label: &str,
    mode_label: &str,
) {
    let mut title = if focused {
        format!("Search ({total_results} results) [EDITING]")
    } else {
        format!("Search ({total_results} results)")
    };
    // Append search method for explainability
    if !method_label.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut title, format_args!(" via {method_label}"));
    }
    // Show active preset name
    if !preset_label.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut title, format_args!(" | Preset: {preset_label}"));
    }
    // Show inbox mode indicator
    if !mode_label.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut title, format_args!(" | [{mode_label}]"));
    }
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    // Render the TextInput inside the block
    if inner.height > 0 && inner.width > 0 {
        input.render(inner, frame);
    }
}

/// Render the results list using VirtualizedList.
fn render_results_list(
    frame: &mut Frame<'_>,
    area: Rect,
    results: &[MessageEntry],
    list_state: &mut VirtualizedListState,
) {
    let block = Block::default()
        .title("Results")
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if results.is_empty() {
        let p = Paragraph::new("  No messages found.");
        p.render(inner, frame);
        return;
    }

    let list = VirtualizedList::new(results)
        .style(Style::default())
        .highlight_style(Style::default().bold().reverse())
        .show_scrollbar(true);

    StatefulWidget::render(&list, inner, frame, list_state);
}

/// Render the detail panel for the selected message.
#[allow(clippy::cast_possible_truncation)]
fn render_detail_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    entry: Option<&MessageEntry>,
    scroll: usize,
) {
    let block = Block::default()
        .title("Detail")
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let Some(msg) = entry else {
        let p = Paragraph::new("  Select a message to view details.");
        p.render(inner, frame);
        return;
    };

    // Build detail text
    let mut lines = Vec::new();
    lines.push(format!("From:    {}", msg.from_agent));
    lines.push(format!("To:      {}", msg.to_agents));
    lines.push(format!("Subject: {}", msg.subject));
    if !msg.thread_id.is_empty() {
        lines.push(format!("Thread:  {}", msg.thread_id));
    }
    lines.push(format!("Project: {}", msg.project_slug));
    lines.push(format!("Time:    {}", msg.timestamp_iso));
    lines.push(format!("Import.: {}", msg.importance));
    if msg.ack_required {
        lines.push("Ack:     required".to_string());
    }
    if msg.id >= 0 {
        lines.push(format!("ID:      #{}", msg.id));
    }
    lines.push(String::new()); // Blank separator
    lines.push("--- Body ---".to_string());

    // Render message body with GFM markdown support using current TUI theme
    let md_theme = crate::tui_theme::markdown_theme();
    let body_text = crate::tui_markdown::render_body(&msg.body_md, &md_theme);
    let body_height = body_text.height();

    // Build header as plain text lines
    let header_height = lines.len();

    // Apply scroll offset across combined header + body
    let visible_height = inner.height as usize;
    if scroll < header_height {
        // Some header lines visible
        let header_visible: Vec<&str> = lines
            .iter()
            .skip(scroll)
            .take(visible_height)
            .map(String::as_str)
            .collect();
        let header_text = header_visible.join("\n");

        let header_rows = header_visible.len().min(visible_height);
        let header_area = Rect::new(inner.x, inner.y, inner.width, header_rows as u16);
        let p = Paragraph::new(header_text);
        p.render(header_area, frame);

        // Render body in remaining space
        let body_rows = visible_height.saturating_sub(header_rows);
        if body_rows > 0 {
            let body_area = Rect::new(
                inner.x,
                inner.y + header_rows as u16,
                inner.width,
                body_rows as u16,
            );
            let p = Paragraph::new(body_text);
            p.render(body_area, frame);
        }
    } else {
        // Scrolled past header — only body visible
        let body_scroll = scroll - header_height;
        // Extract visible portion of body text by skipping lines
        let all_lines = body_text.lines();
        let visible_body: Vec<_> = all_lines
            .iter()
            .skip(body_scroll)
            .take(visible_height)
            .cloned()
            .collect();
        let _ = body_height; // suppress unused warning
        let text = ftui::text::Text::from_lines(visible_body);
        let p = Paragraph::new(text);
        p.render(inner, frame);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Utility helpers
// ──────────────────────────────────────────────────────────────────────

/// Compute the viewport [start, end) to keep cursor visible.
/// (Retained for test coverage; VirtualizedList handles this internally.)
#[allow(dead_code)]
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

    // ── Construction ────────────────────────────────────────────────

    #[test]
    fn new_screen_defaults() {
        let screen = MessageBrowserScreen::new();
        assert_eq!(screen.cursor, 0);
        assert_eq!(screen.detail_scroll, 0);
        assert!(matches!(screen.focus, Focus::ResultList));
        assert!(screen.results.is_empty());
        assert!(screen.search_dirty);
    }

    #[test]
    fn default_impl_works() {
        let screen = MessageBrowserScreen::default();
        assert!(screen.results.is_empty());
    }

    // ── Focus switching ─────────────────────────────────────────────

    #[test]
    fn slash_enters_search_mode() {
        let mut screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let event = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&event, &state);
        assert!(matches!(screen.focus, Focus::SearchBar));
    }

    #[test]
    fn escape_exits_search_mode() {
        let mut screen = MessageBrowserScreen::new();
        screen.focus = Focus::SearchBar;
        screen.search_input.set_focused(true);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let event = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&event, &state);
        assert!(matches!(screen.focus, Focus::ResultList));
    }

    #[test]
    fn tab_toggles_focus() {
        let mut screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // ResultList -> SearchBar
        let tab = Event::Key(ftui::KeyEvent::new(KeyCode::Tab));
        screen.update(&tab, &state);
        assert!(matches!(screen.focus, Focus::SearchBar));

        // SearchBar -> ResultList
        screen.update(&tab, &state);
        assert!(matches!(screen.focus, Focus::ResultList));
    }

    // ── Cursor navigation ───────────────────────────────────────────

    #[test]
    fn cursor_navigation_with_results() {
        let mut screen = MessageBrowserScreen::new();
        // Seed some results
        for i in 0..10 {
            screen.results.push(MessageEntry {
                id: i,
                subject: format!("Message {i}"),
                from_agent: "GoldFox".to_string(),
                to_agents: "SilverWolf".to_string(),
                project_slug: "proj1".to_string(),
                thread_id: String::new(),
                timestamp_iso: "2026-02-06T12:00:00".to_string(),
                timestamp_micros: 0,
                body_md: "Body text".to_string(),
                importance: "normal".to_string(),
                ack_required: false,
                show_project: false,
            });
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
    fn cursor_clamps_at_bounds() {
        let mut screen = MessageBrowserScreen::new();
        for i in 0..3 {
            screen.results.push(MessageEntry {
                id: i,
                subject: format!("Msg {i}"),
                from_agent: String::new(),
                to_agents: String::new(),
                project_slug: String::new(),
                thread_id: String::new(),
                timestamp_iso: String::new(),
                timestamp_micros: 0,
                body_md: String::new(),
                importance: "normal".to_string(),
                ack_required: false,
                show_project: false,
            });
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

    #[test]
    fn detail_scroll() {
        let mut screen = MessageBrowserScreen::new();
        screen.results.push(MessageEntry {
            id: 1,
            subject: "Test".to_string(),
            from_agent: String::new(),
            to_agents: String::new(),
            project_slug: String::new(),
            thread_id: String::new(),
            timestamp_iso: String::new(),
            timestamp_micros: 0,
            body_md: "Long body\nwith\nmany\nlines".to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let j_upper = Event::Key(ftui::KeyEvent::new(KeyCode::Char('J')));
        screen.update(&j_upper, &state);
        assert_eq!(screen.detail_scroll, 1);

        let k_upper = Event::Key(ftui::KeyEvent::new(KeyCode::Char('K')));
        screen.update(&k_upper, &state);
        assert_eq!(screen.detail_scroll, 0);

        // Can't go below 0
        screen.update(&k_upper, &state);
        assert_eq!(screen.detail_scroll, 0);
    }

    // ── consumes_text_input ─────────────────────────────────────────

    #[test]
    fn consumes_text_input_when_searching() {
        let mut screen = MessageBrowserScreen::new();
        assert!(!screen.consumes_text_input());
        screen.focus = Focus::SearchBar;
        assert!(screen.consumes_text_input());
    }

    // ── FTS sanitization ────────────────────────────────────────────

    #[test]
    fn sanitize_fts_empty() {
        assert!(sanitize_fts_query("").is_empty());
    }

    #[test]
    fn sanitize_fts_simple_terms() {
        let result = sanitize_fts_query("hello world");
        assert_eq!(result, "\"hello\" \"world\"");
    }

    #[test]
    fn sanitize_fts_strips_operators() {
        let result = sanitize_fts_query("foo AND bar OR NOT baz");
        // AND, OR, NOT are stripped
        assert_eq!(result, "\"foo\" \"bar\" \"baz\"");
    }

    #[test]
    fn sanitize_fts_handles_special_chars() {
        let result = sanitize_fts_query("test-case with_underscore");
        assert_eq!(result, "\"test-case\" \"with_underscore\"");
    }

    #[test]
    fn sanitize_fts_strips_quotes() {
        let result = sanitize_fts_query(r#""quoted" term"#);
        assert_eq!(result, "\"quoted\" \"term\"");
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

    #[test]
    fn truncate_very_short_max() {
        assert_eq!(truncate_str("hello", 2), "he");
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

    // ── Rendering (no-panic) ────────────────────────────────────────

    #[test]
    fn render_search_bar_no_panic() {
        let input = TextInput::new().with_placeholder("Search...");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_search_bar(
            &mut frame,
            Rect::new(0, 0, 80, 3),
            &input,
            42,
            false,
            "FTS",
            "",
            "", // mode_label
        );
    }

    #[test]
    fn render_results_empty_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let mut list_state = VirtualizedListState::default();
        render_results_list(&mut frame, Rect::new(0, 0, 40, 20), &[], &mut list_state);
    }

    #[test]
    fn render_results_with_entries_no_panic() {
        let entries = vec![
            MessageEntry {
                id: 1,
                subject: "Test message".to_string(),
                from_agent: "GoldFox".to_string(),
                to_agents: "SilverWolf".to_string(),
                project_slug: "proj1".to_string(),
                thread_id: "thread-1".to_string(),
                timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
                timestamp_micros: 0,
                body_md: "Hello world".to_string(),
                importance: "high".to_string(),
                ack_required: true,
                show_project: false,
            },
            MessageEntry {
                id: 2,
                subject: "Another message".to_string(),
                from_agent: "BluePeak".to_string(),
                to_agents: "RedLake".to_string(),
                project_slug: "proj2".to_string(),
                thread_id: String::new(),
                timestamp_iso: "2026-02-06T13:00:00Z".to_string(),
                timestamp_micros: 0,
                body_md: "Body content".to_string(),
                importance: "normal".to_string(),
                ack_required: false,
                show_project: false,
            },
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let mut list_state = VirtualizedListState::default();
        list_state.select(Some(0));
        render_results_list(
            &mut frame,
            Rect::new(0, 0, 40, 20),
            &entries,
            &mut list_state,
        );
    }

    #[test]
    fn render_detail_no_message_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_detail_panel(&mut frame, Rect::new(40, 0, 40, 20), None, 0);
    }

    #[test]
    fn render_detail_with_message_no_panic() {
        let msg = MessageEntry {
            id: 1,
            subject: "Test subject with a somewhat long title".to_string(),
            from_agent: "GoldFox".to_string(),
            to_agents: "SilverWolf, BluePeak".to_string(),
            project_slug: "my-project".to_string(),
            thread_id: "thread-123".to_string(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: "This is the body of the message.\nIt has multiple lines.\nAnd some content."
                .to_string(),
            importance: "urgent".to_string(),
            ack_required: true,
            show_project: false,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_detail_panel(&mut frame, Rect::new(40, 0, 40, 20), Some(&msg), 0);
    }

    #[test]
    fn render_detail_with_scroll_no_panic() {
        let msg = MessageEntry {
            id: 1,
            subject: "Scrolled".to_string(),
            from_agent: "Agent".to_string(),
            to_agents: String::new(),
            project_slug: String::new(),
            thread_id: String::new(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: (0..50)
                .map(|i| format!("Line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_detail_panel(&mut frame, Rect::new(40, 0, 40, 20), Some(&msg), 10);
    }

    #[test]
    fn render_full_screen_no_panic() {
        let screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn render_narrow_screen_no_panic() {
        let screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(40, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 40, 10), &state);
    }

    #[test]
    fn render_minimum_size_no_panic() {
        let screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(20, 4, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 20, 4), &state);
    }

    // ── Titles ──────────────────────────────────────────────────────

    #[test]
    fn title_and_label() {
        let screen = MessageBrowserScreen::new();
        assert_eq!(screen.title(), "Messages");
        assert_eq!(screen.tab_label(), "Msg");
    }

    // ── Keybindings ─────────────────────────────────────────────────

    #[test]
    fn keybindings_not_empty() {
        let screen = MessageBrowserScreen::new();
        assert!(!screen.keybindings().is_empty());
    }

    // ── Enter in search mode triggers immediate search ──────────────

    #[test]
    fn enter_in_search_triggers_search() {
        let mut screen = MessageBrowserScreen::new();
        screen.focus = Focus::SearchBar;
        screen.search_input.set_focused(true);
        screen.debounce_remaining = 5;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        screen.update(&enter, &state);

        assert!(matches!(screen.focus, Focus::ResultList));
        assert!(screen.search_dirty);
        assert_eq!(screen.debounce_remaining, 0);
    }

    // ── Deep-link routing ───────────────────────────────────────────

    #[test]
    fn enter_in_result_list_emits_deep_link() {
        let mut screen = MessageBrowserScreen::new();
        screen.results.push(MessageEntry {
            id: 42,
            subject: "Test".to_string(),
            from_agent: String::new(),
            to_agents: String::new(),
            project_slug: String::new(),
            thread_id: String::new(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 1_000_000,
            body_md: String::new(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let cmd = screen.update(&enter, &state);

        // Should emit a Msg with DeepLink
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::TimelineAtTime(
                1_000_000
            )))
        ));
    }

    #[test]
    fn enter_on_empty_results_is_noop() {
        let mut screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let cmd = screen.update(&enter, &state);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn receive_deep_link_message_by_id() {
        let mut screen = MessageBrowserScreen::new();
        for i in 0..5 {
            screen.results.push(MessageEntry {
                id: i * 10,
                subject: format!("Msg {i}"),
                from_agent: String::new(),
                to_agents: String::new(),
                project_slug: String::new(),
                thread_id: String::new(),
                timestamp_iso: String::new(),
                timestamp_micros: 0,
                body_md: String::new(),
                importance: "normal".to_string(),
                ack_required: false,
                show_project: false,
            });
        }

        // Deep-link to message ID 30 (index 3)
        let handled = screen.receive_deep_link(&DeepLinkTarget::MessageById(30));
        assert!(handled);
        assert_eq!(screen.cursor, 3);
        assert!(matches!(screen.focus, Focus::ResultList));
    }

    #[test]
    fn receive_deep_link_unknown_is_ignored() {
        let mut screen = MessageBrowserScreen::new();
        let handled = screen.receive_deep_link(&DeepLinkTarget::ThreadById("x".to_string()));
        assert!(!handled);
    }

    // ── Query presets ──────────────────────────────────────────────

    #[test]
    fn presets_have_valid_structure() {
        assert!(QUERY_PRESETS.len() >= 4);
        for preset in QUERY_PRESETS {
            assert!(!preset.label.is_empty());
            assert!(!preset.description.is_empty());
        }
        // First preset should be "All" (empty query)
        assert_eq!(QUERY_PRESETS[0].label, "All");
        assert!(QUERY_PRESETS[0].query.is_empty());
    }

    #[test]
    fn apply_preset_sets_query() {
        let mut screen = MessageBrowserScreen::new();
        screen.apply_preset(1); // "Urgent"
        assert_eq!(screen.preset_index, 1);
        assert_eq!(screen.search_input.value(), "urgent");
        assert!(screen.search_dirty);
        assert_eq!(screen.debounce_remaining, 0);
    }

    #[test]
    fn apply_preset_wraps_around() {
        let mut screen = MessageBrowserScreen::new();
        screen.apply_preset(QUERY_PRESETS.len()); // Should wrap to 0
        assert_eq!(screen.preset_index, 0);
        assert!(screen.search_input.value().is_empty());
    }

    #[test]
    fn p_key_cycles_presets_forward() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        assert_eq!(screen.preset_index, 0);

        let p = Event::Key(ftui::KeyEvent::new(KeyCode::Char('p')));
        screen.update(&p, &state);
        assert_eq!(screen.preset_index, 1);
        assert_eq!(screen.search_input.value(), "urgent");
    }

    #[test]
    fn big_p_key_cycles_presets_backward() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        assert_eq!(screen.preset_index, 0);

        let p = Event::Key(ftui::KeyEvent::new(KeyCode::Char('P')));
        screen.update(&p, &state);
        assert_eq!(screen.preset_index, QUERY_PRESETS.len() - 1);
    }

    #[test]
    fn ctrl_c_resets_preset() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.apply_preset(2);
        assert_eq!(screen.preset_index, 2);

        let ctrl_c = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: Modifiers::CTRL,
            kind: KeyEventKind::Press,
        });
        screen.update(&ctrl_c, &state);
        assert_eq!(screen.preset_index, 0);
        assert!(screen.search_input.value().is_empty());
    }

    #[test]
    fn active_preset_returns_current() {
        let mut screen = MessageBrowserScreen::new();
        assert_eq!(screen.active_preset().label, "All");
        screen.apply_preset(3);
        assert_eq!(screen.active_preset().label, "Ack");
    }

    // ── Search method explainability ───────────────────────────────

    #[test]
    fn new_screen_has_no_search_method() {
        let screen = MessageBrowserScreen::new();
        assert_eq!(screen.search_method, SearchMethod::None);
    }

    #[test]
    fn search_method_variants_exist() {
        // Ensure all variants compile
        let _ = SearchMethod::None;
        let _ = SearchMethod::Recent;
        let _ = SearchMethod::Fts;
        let _ = SearchMethod::LikeFallback;
    }

    #[test]
    fn render_search_bar_with_metadata_no_panic() {
        let input = TextInput::new().with_placeholder("Search...");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_search_bar(
            &mut frame,
            Rect::new(0, 0, 80, 3),
            &input,
            42,
            false,
            "FTS",
            "Urgent",
            "", // mode_label
        );
    }

    #[test]
    fn render_search_bar_empty_metadata_no_panic() {
        let input = TextInput::new().with_placeholder("Search...");
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        render_search_bar(
            &mut frame,
            Rect::new(0, 0, 80, 3),
            &input,
            0,
            true,
            "",
            "",
            "",
        );
    }

    #[test]
    fn keybindings_include_preset() {
        let screen = MessageBrowserScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.iter().any(|b| b.key == "p/P"));
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

    // ── InboxMode tests ────────────────────────────────────────────────

    #[test]
    fn inbox_mode_default_is_global() {
        let screen = MessageBrowserScreen::new();
        assert!(matches!(screen.inbox_mode, InboxMode::Global));
    }

    #[test]
    fn inbox_mode_label_global() {
        let mode = InboxMode::Global;
        assert_eq!(mode.label(), "Global: all projects");
        assert!(mode.is_global());
    }

    #[test]
    fn inbox_mode_label_local() {
        let mode = InboxMode::Local("my-project".to_string());
        assert_eq!(mode.label(), "Local: my-project");
        assert!(!mode.is_global());
    }

    #[test]
    fn g_key_toggles_inbox_mode() {
        let mut screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Start in Global mode
        assert!(matches!(screen.inbox_mode, InboxMode::Global));

        // Press 'g' to toggle to Local mode
        let g = Event::Key(ftui::KeyEvent::new(KeyCode::Char('g')));
        screen.update(&g, &state);
        assert!(matches!(screen.inbox_mode, InboxMode::Local(_)));
        assert!(screen.search_dirty);

        // Press 'g' again to toggle back to Global mode
        screen.search_dirty = false;
        screen.update(&g, &state);
        assert!(matches!(screen.inbox_mode, InboxMode::Global));
        assert!(screen.search_dirty);
    }

    #[test]
    fn toggle_inbox_mode_remembers_last_project() {
        let mut screen = MessageBrowserScreen::new();

        // Start in Local mode with a project
        screen.inbox_mode = InboxMode::Local("my-project".to_string());

        // Toggle to Global (should remember "my-project")
        screen.toggle_inbox_mode();
        assert!(matches!(screen.inbox_mode, InboxMode::Global));
        assert_eq!(screen.last_local_project, Some("my-project".to_string()));

        // Toggle back to Local (should restore "my-project")
        screen.toggle_inbox_mode();
        assert!(matches!(screen.inbox_mode, InboxMode::Local(ref s) if s == "my-project"));
    }

    #[test]
    fn toggle_inbox_mode_infers_project_from_cursor() {
        let mut screen = MessageBrowserScreen::new();
        screen.results.push(MessageEntry {
            id: 1,
            subject: "Test".to_string(),
            from_agent: String::new(),
            to_agents: String::new(),
            project_slug: "inferred-project".to_string(),
            thread_id: String::new(),
            timestamp_iso: String::new(),
            timestamp_micros: 0,
            body_md: String::new(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        screen.cursor = 0;

        // Start in Global mode, no last_local_project set
        assert!(screen.last_local_project.is_none());

        // Toggle to Local should infer from current message
        screen.toggle_inbox_mode();
        assert!(matches!(screen.inbox_mode, InboxMode::Local(ref s) if s == "inferred-project"));
    }

    #[test]
    fn keybindings_include_inbox_mode() {
        let screen = MessageBrowserScreen::new();
        let bindings = screen.keybindings();
        assert!(
            bindings
                .iter()
                .any(|b| b.key == "g" && b.action.contains("Local/Global"))
        );
    }
}
