//! Message Browser screen with search bar, results list, and detail panel.
//!
//! Provides unified Search V3 lookups across all messages plus live event
//! stream merging. Results are displayed in a split-pane layout with
//! keyboard-first navigation.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asupersync::Cx;
use fastmcp::prelude::McpContext;
use fastmcp_core::block_on;
use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind, Style};
use ftui_extras::image::{
    DetectionHints, Image, ImageFit, ImageProtocol, Iterm2Dimension, Iterm2Options, detect_protocol,
};
use ftui_extras::syntax::{JsonTokenizer, LineState, TokenKind, Tokenizer};
use ftui_runtime::program::Cmd;
use ftui_widgets::StatefulWidget;
use ftui_widgets::input::TextInput;
use ftui_widgets::textarea::TextArea;
use ftui_widgets::virtualized::{RenderItem, VirtualizedList, VirtualizedListState};

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel_core::Value;
use mcp_agent_mail_db::timestamps::micros_to_iso;
use serde::Deserialize;

use crate::tui_action_menu::{ActionEntry, ActionKind, messages_actions, messages_batch_actions};
use crate::tui_bridge::{KeyboardMoveSnapshot, MessageDragSnapshot, TuiSharedState};
use crate::tui_events::MailEvent;
use crate::tui_layout::{DockLayout, DockPosition};
use crate::tui_persist::{
    ScreenFilterPresetStore, console_persist_path_from_env_or_default,
    load_screen_filter_presets_or_default, save_screen_filter_presets, screen_filter_presets_path,
};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg, SelectionState};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Number of results per page.
const PAGE_SIZE: usize = 50;
const MESSAGES_PRESET_SCREEN_ID: &str = "messages";

/// Debounce delay in ticks. Zero means immediate search-as-you-type.
const DEBOUNCE_TICKS: u8 = 0;

/// Max results to cache.
const MAX_RESULTS: usize = 1000;
const URGENT_PULSE_HALF_PERIOD_TICKS: u64 = 5;
const SHIMMER_WINDOW_MICROS: i64 = 500_000;
const SHIMMER_MAX_ROWS: usize = 5;
const SHIMMER_HIGHLIGHT_WIDTH: usize = 5;
const COMPOSE_BODY_MIN_ROWS: u16 = 10;
const QUICK_REPLY_BODY_MIN_ROWS: u16 = 8;
const COMPOSE_SENDER_NAME: &str = "HumanOverseer";
const COMPOSE_IMPORTANCE_LEVELS: [&str; 3] = ["normal", "high", "urgent"];
const MESSAGE_DRAG_HOLD_DELAY: Duration = Duration::from_millis(200);
const MESSAGE_DOCK_HIDE_HEIGHT_THRESHOLD: u16 = 8;
const MESSAGE_STACKED_WIDTH_THRESHOLD: u16 = 68;
const MESSAGE_STACKED_MIN_HEIGHT: u16 = 12;
const MESSAGE_STACKED_DOCK_RATIO: f32 = 0.38;
const MESSAGE_DEFAULT_DOCK_RATIO: f32 = 0.40;
const MESSAGE_SPLIT_GAP_THRESHOLD: u16 = 60;
const MESSAGE_WIDE_DOCK_MIN_WIDTH: u16 = 150;
const MESSAGE_ULTRAWIDE_DOCK_MIN_WIDTH: u16 = 220;

/// Max body preview length in the results list (used for future
/// inline preview in narrow mode).
#[allow(dead_code)]
const BODY_PREVIEW_LEN: usize = 80;

static MESSAGE_URGENT_PULSE_ON: AtomicBool = AtomicBool::new(true);

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn reduced_motion_enabled() -> bool {
    env_flag_enabled("AM_TUI_REDUCED_MOTION") || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

const fn responsive_message_dock_ratio(width: u16) -> f32 {
    if width >= MESSAGE_ULTRAWIDE_DOCK_MIN_WIDTH {
        0.58
    } else if width >= 180 {
        0.52
    } else if width >= MESSAGE_WIDE_DOCK_MIN_WIDTH {
        0.47
    } else {
        MESSAGE_DEFAULT_DOCK_RATIO
    }
}

fn render_splitter_handle(frame: &mut Frame<'_>, area: Rect, vertical: bool, active: bool) {
    if area.is_empty() {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();

    // Repaint the whole splitter gap first so prior layout artifacts never
    // remain visible as stray borders across list/detail content.
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
            cell.fg = tp.selection_indicator;
            cell.bg = tp.panel_bg;
        }
    }
}

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
    /// Showing live event-ring results while the DB is unavailable.
    Live,
    /// Showing recent messages (empty query).
    Recent,
    /// Unified search service (lexical/semantic/hybrid routing).
    Unified,
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
    /// Raw timestamp for sorting/merging with live events.
    timestamp_micros: i64,
    body_md: String,
    importance: String,
    ack_required: bool,
    /// Whether to display the project column (true in Global mode).
    show_project: bool,
}

#[derive(Debug)]
struct RawMessageRow {
    id: i64,
    subject: String,
    body_md: String,
    thread_id: String,
    importance: String,
    ack_required: bool,
    created_ts: i64,
    sender_id: i64,
    project_id: i64,
    recipients_json: String,
}

#[derive(Debug, Default, Deserialize)]
struct StoredRecipients {
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
}

impl RenderItem for MessageEntry {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        self.render_row(
            area,
            frame,
            selected,
            None,
            MessageDropZoneState::None,
            false,
            false,
        );
    }

    fn height(&self) -> u16 {
        1
    }
}

impl MessageEntry {
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn render_row(
        &self,
        area: Rect,
        frame: &mut Frame,
        selected: bool,
        shimmer_progress: Option<f64>,
        drop_zone: MessageDropZoneState,
        keyboard_marked: bool,
        batch_selected: bool,
    ) {
        use ftui::widgets::Widget;
        if area.height == 0 || area.width < 10 {
            return;
        }
        let inner_w = area.width as usize;
        let tp = crate::tui_theme::TuiThemePalette::current();
        let row_bg = if selected {
            tp.selection_bg
        } else if self.id.rem_euclid(2) == 0 {
            crate::tui_theme::lerp_color(tp.panel_bg, tp.bg_surface, 0.20)
        } else {
            crate::tui_theme::lerp_color(tp.panel_bg, tp.bg_surface, 0.10)
        };

        // Marker for selected row
        let marker = if selected {
            crate::tui_theme::SELECTION_PREFIX
        } else {
            crate::tui_theme::SELECTION_PREFIX_EMPTY
        };
        let batch_marker = if batch_selected { "[x]" } else { "[ ]" };
        let batch_marker_style = if batch_selected {
            Style::default()
                .fg(tp.selection_indicator)
                .bg(row_bg)
                .bold()
        } else {
            crate::tui_theme::text_meta(&tp).bg(row_bg)
        };
        let cursor_style = Style::default()
            .fg(tp.selection_fg)
            .bg(tp.selection_bg)
            .bold();
        let row_default_style = Style::default().fg(tp.text_primary).bg(row_bg);

        // Importance badge
        let pulse_on = MESSAGE_URGENT_PULSE_ON.load(Ordering::Relaxed);
        let (badge, badge_style) = match self.importance.as_str() {
            "high" => (
                "!\u{2219}",
                Style::default().fg(tp.help_bg).bg(tp.severity_warn).bold(),
            ),
            "urgent" => {
                let fg = if pulse_on {
                    tp.badge_urgent_bg
                } else {
                    crate::tui_theme::lerp_color(tp.badge_urgent_bg, tp.text_disabled, 0.5)
                };
                ("!!", Style::default().fg(tp.badge_urgent_fg).bg(fg).bold())
            }
            _ => (
                "\u{00b7}\u{00b7}",
                Style::default()
                    .fg(tp.text_disabled)
                    .bg(crate::tui_theme::lerp_color(row_bg, tp.bg_overlay, 0.22)),
            ),
        };

        // Ack-required indicator
        let ack_badge = if self.ack_required { "@" } else { "\u{00b7}" };
        let ack_style = if self.ack_required {
            Style::default()
                .fg(tp.badge_info_fg)
                .bg(tp.badge_info_bg)
                .bold()
        } else {
            Style::default()
                .fg(tp.text_disabled)
                .bg(crate::tui_theme::lerp_color(row_bg, tp.bg_overlay, 0.18))
        };

        // ID or "LIVE" marker with distinct styling
        let (id_str, id_style) = if self.id >= 0 {
            (
                format!("#{}", self.id),
                crate::tui_theme::text_meta(&tp).bg(row_bg),
            )
        } else {
            (
                "LIVE".to_string(),
                crate::tui_theme::text_accent(&tp).bg(row_bg).bold(),
            )
        };

        // Compact timestamp (HH:MM:SS from ISO string)
        let time_short: String = if self.timestamp_iso.len() >= 19 {
            self.timestamp_iso[11..19].to_string()
        } else {
            self.timestamp_iso.clone()
        };
        let time_style = crate::tui_theme::text_meta(&tp).bg(row_bg);

        // Sender (truncated to 12 chars, Unicode-safe).
        let sender_end = char_index_to_byte_offset(&self.from_agent, 12);
        let sender = self.from_agent[..sender_end].to_string();
        let sender_style = Style::default().fg(tp.text_secondary).bg(row_bg);

        // Project badge (only in Global mode)
        let project_badge = if self.show_project && !self.project_slug.is_empty() {
            let slug_end = char_index_to_byte_offset(&self.project_slug, 8);
            let slug = &self.project_slug[..slug_end];
            format!("[{slug:>8}] ")
        } else {
            String::new()
        };
        let moving_badge = if keyboard_marked { " [MOVING]" } else { "" };

        // Calculate how much space remains for subject
        // Format: marker + badge(2) + ack(1) + space + id(6) + space + time(8) + space + sender(<=12) + space + project + subject
        let fixed_len = ftui::text::display_width(marker)
            + batch_marker.chars().count()
            + 1 // spacer
            + 2  // badge
            + 1  // ack
            + 1  // space
            + id_str.len().max(6)
            + 1  // space
            + 8  // time
            + 1  // space
            + sender.chars().count()
            + 1  // space
            + project_badge.chars().count()
            + moving_badge.chars().count();
        let remaining = inner_w.saturating_sub(fixed_len);
        let subj = truncate_str(&self.subject, remaining);

        let project_style = Style::default()
            .fg(tp.status_accent)
            .bg(crate::tui_theme::lerp_color(row_bg, tp.status_accent, 0.18))
            .bold();
        let mut spans = vec![
            Span::styled(batch_marker, batch_marker_style),
            Span::styled(" ", Style::default().fg(tp.text_primary).bg(row_bg)),
            Span::styled(marker, Style::default().fg(tp.text_primary).bg(row_bg)),
            Span::styled(format!("{badge:>2}"), badge_style),
            Span::styled(ack_badge, ack_style),
            Span::styled(" ", Style::default().fg(tp.text_primary).bg(row_bg)),
            Span::styled(format!("{id_str:>6}"), id_style),
            Span::styled(" ", Style::default().fg(tp.text_primary).bg(row_bg)),
            Span::styled(time_short, time_style),
            Span::styled(" ", Style::default().fg(tp.text_primary).bg(row_bg)),
            Span::styled(format!("{sender:<12}"), sender_style),
            Span::styled(" ", Style::default().fg(tp.text_primary).bg(row_bg)),
            Span::styled(project_badge, project_style),
            Span::styled(
                moving_badge,
                Style::default()
                    .fg(tp.selection_indicator)
                    .bg(row_bg)
                    .bold(),
            ),
        ];
        let base_subject_style = if matches!(self.importance.as_str(), "high" | "urgent") {
            Style::default().fg(tp.text_primary).bg(row_bg).bold()
        } else {
            Style::default().fg(tp.text_primary).bg(row_bg)
        };
        if let Some(progress) = shimmer_progress.filter(|_| !selected) {
            if let Some((start_char, end_char)) =
                subject_shimmer_window(&subj, progress, SHIMMER_HIGHLIGHT_WIDTH)
            {
                let start_byte = char_index_to_byte_offset(&subj, start_char);
                let end_byte = char_index_to_byte_offset(&subj, end_char);
                let prefix = &subj[..start_byte];
                let highlight = &subj[start_byte..end_byte];
                let suffix = &subj[end_byte..];
                if !prefix.is_empty() {
                    spans.push(Span::styled(prefix.to_string(), base_subject_style));
                }
                if !highlight.is_empty() {
                    spans.push(Span::styled(
                        highlight.to_string(),
                        Style::default().fg(tp.selection_indicator).bold(),
                    ));
                }
                if !suffix.is_empty() {
                    spans.push(Span::styled(suffix.to_string(), base_subject_style));
                }
            } else {
                spans.push(Span::styled(subj, base_subject_style));
            }
        } else {
            spans.push(Span::styled(subj, base_subject_style));
        }
        let mut line = Line::from_spans(spans);
        if selected {
            line.apply_base_style(cursor_style);
        } else {
            line.apply_base_style(row_default_style);
            let drop_style = match drop_zone {
                MessageDropZoneState::None => None,
                MessageDropZoneState::Valid => {
                    Some(Style::default().fg(tp.selection_fg).bg(tp.selection_bg))
                }
                MessageDropZoneState::HoveredValid => Some(
                    Style::default()
                        .fg(tp.selection_fg)
                        .bg(tp.selection_bg)
                        .bold(),
                ),
                MessageDropZoneState::HoveredInvalid => {
                    Some(Style::default().fg(tp.severity_warn).bold())
                }
            };
            if let Some(style) = drop_style {
                line.apply_base_style(style);
            }
        }
        let paragraph = Paragraph::new(Text::from_line(line));
        paragraph.render(area, frame);
    }
}

#[derive(Clone, Copy)]
struct MessageRenderRow<'a> {
    entry: &'a MessageEntry,
    shimmer_progress: Option<f64>,
    drop_zone: MessageDropZoneState,
    keyboard_marked: bool,
    batch_selected: bool,
}

#[derive(Clone, Copy)]
struct MessageDropVisual<'a> {
    source_thread_id: &'a str,
    hovered_thread_id: Option<&'a str>,
    invalid_hover: bool,
}

impl RenderItem for MessageRenderRow<'_> {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        self.entry.render_row(
            area,
            frame,
            selected,
            self.shimmer_progress,
            self.drop_zone,
            self.keyboard_marked,
            self.batch_selected,
        );
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
    pub const fn is_global(&self) -> bool {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailViewMode {
    Markdown,
    JsonTree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockDragState {
    Idle,
    Dragging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresetDialogMode {
    None,
    Save,
    Load,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavePresetField {
    Name,
    Description,
}

impl SavePresetField {
    const fn next(self) -> Self {
        match self {
            Self::Name => Self::Description,
            Self::Description => Self::Name,
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageDropZoneState {
    None,
    Valid,
    HoveredValid,
    HoveredInvalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeField {
    To,
    Cc,
    Subject,
    ThreadId,
    Importance,
    AckRequired,
    Body,
}

impl ComposeField {
    const fn next(self) -> Self {
        match self {
            Self::To => Self::Cc,
            Self::Cc => Self::Subject,
            Self::Subject => Self::ThreadId,
            Self::ThreadId => Self::Importance,
            Self::Importance => Self::AckRequired,
            Self::AckRequired => Self::Body,
            Self::Body => Self::To,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::To => Self::Body,
            Self::Cc => Self::To,
            Self::Subject => Self::Cc,
            Self::ThreadId => Self::Subject,
            Self::Importance => Self::ThreadId,
            Self::AckRequired => Self::Importance,
            Self::Body => Self::AckRequired,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuickReplyField {
    Body,
    AckRequired,
}

impl QuickReplyField {
    const fn next(self) -> Self {
        match self {
            Self::Body => Self::AckRequired,
            Self::AckRequired => Self::Body,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Body => Self::AckRequired,
            Self::AckRequired => Self::Body,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ComposeValidationErrors {
    to: Option<String>,
    cc: Option<String>,
    subject: Option<String>,
    thread_id: Option<String>,
    body: Option<String>,
    general: Option<String>,
}

impl ComposeValidationErrors {
    const fn has_any(&self) -> bool {
        self.to.is_some()
            || self.cc.is_some()
            || self.subject.is_some()
            || self.thread_id.is_some()
            || self.body.is_some()
            || self.general.is_some()
    }
}

#[derive(Debug, Default, Clone)]
struct QuickReplyValidationErrors {
    body: Option<String>,
    general: Option<String>,
}

impl QuickReplyValidationErrors {
    const fn has_any(&self) -> bool {
        self.body.is_some() || self.general.is_some()
    }
}

#[derive(Debug, Clone)]
struct QuickReplyContext {
    message_id: i64,
    project_slug: String,
    to_agent: String,
    thread_id: Option<String>,
    subject: String,
    original_from_agent: String,
    original_timestamp_iso: String,
    original_body_md: String,
}

impl QuickReplyContext {
    fn from_entry(entry: &MessageEntry) -> Option<Self> {
        let project_slug = entry.project_slug.trim().to_string();
        let to_agent = entry.from_agent.trim().to_string();
        if project_slug.is_empty() || to_agent.is_empty() {
            return None;
        }
        let thread_id = if entry.thread_id.trim().is_empty() {
            None
        } else {
            Some(entry.thread_id.trim().to_string())
        };
        Some(Self {
            message_id: entry.id,
            project_slug,
            to_agent,
            thread_id,
            subject: prefixed_reply_subject(&entry.subject),
            original_from_agent: entry.from_agent.clone(),
            original_timestamp_iso: entry.timestamp_iso.clone(),
            original_body_md: entry.body_md.clone(),
        })
    }
}

#[derive(Debug, Clone)]
struct ComposePayload {
    project_slug: String,
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    thread_id: Option<String>,
    body_md: String,
    importance: String,
    ack_required: bool,
}

#[derive(Debug, Clone)]
struct ComposeFormState {
    project_slug: String,
    to_input: TextInput,
    cc_input: TextInput,
    subject_input: TextInput,
    thread_id_input: TextInput,
    body_input: TextArea,
    importance_idx: usize,
    ack_required: bool,
    focus: ComposeField,
    available_agents: Vec<String>,
    suggestions: Vec<String>,
    suggestion_cursor: usize,
    errors: ComposeValidationErrors,
}

impl ComposeFormState {
    fn new(
        project_slug: String,
        prefill_to: Option<String>,
        available_agents: Vec<String>,
    ) -> Self {
        let mut form = Self {
            project_slug,
            to_input: TextInput::new()
                .with_placeholder("Recipient agent (comma-separated)")
                .with_focused(true),
            cc_input: TextInput::new().with_placeholder("CC recipients (optional)"),
            subject_input: TextInput::new().with_placeholder("Subject (required, max 200 chars)"),
            thread_id_input: TextInput::new()
                .with_placeholder("Thread ID (optional, auto-generated if blank)"),
            body_input: TextArea::new()
                .with_placeholder("Markdown body...")
                .with_soft_wrap(true)
                .with_focus(false),
            importance_idx: 0,
            ack_required: false,
            focus: ComposeField::To,
            available_agents,
            suggestions: Vec::new(),
            suggestion_cursor: 0,
            errors: ComposeValidationErrors::default(),
        };
        if let Some(to) = prefill_to {
            form.to_input.set_value(&to);
        }
        form.update_focus();
        form.refresh_suggestions();
        form
    }

    const fn importance(&self) -> &'static str {
        COMPOSE_IMPORTANCE_LEVELS[self.importance_idx]
    }

    fn update_focus(&mut self) {
        self.to_input
            .set_focused(matches!(self.focus, ComposeField::To));
        self.cc_input
            .set_focused(matches!(self.focus, ComposeField::Cc));
        self.subject_input
            .set_focused(matches!(self.focus, ComposeField::Subject));
        self.thread_id_input
            .set_focused(matches!(self.focus, ComposeField::ThreadId));
        self.body_input
            .set_focused(matches!(self.focus, ComposeField::Body));
    }

    fn set_focus(&mut self, focus: ComposeField) {
        self.focus = focus;
        self.update_focus();
        self.refresh_suggestions();
    }

    fn cycle_focus_next(&mut self) {
        self.set_focus(self.focus.next());
    }

    fn cycle_focus_prev(&mut self) {
        self.set_focus(self.focus.prev());
    }

    const fn recipient_input(&self) -> Option<&TextInput> {
        match self.focus {
            ComposeField::To => Some(&self.to_input),
            ComposeField::Cc => Some(&self.cc_input),
            _ => None,
        }
    }

    const fn recipient_input_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            ComposeField::To => Some(&mut self.to_input),
            ComposeField::Cc => Some(&mut self.cc_input),
            _ => None,
        }
    }

    fn refresh_suggestions(&mut self) {
        let Some(input) = self.recipient_input() else {
            self.suggestions.clear();
            self.suggestion_cursor = 0;
            return;
        };

        let raw = input.value();
        let (_, prefix) = split_recipient_prefix(raw);
        let prefix_lower = prefix.to_ascii_lowercase();
        let already = parse_recipient_list(raw);
        self.suggestions = self
            .available_agents
            .iter()
            .filter(|name| {
                let starts_with_ci = prefix_lower.is_empty() || {
                    name.len() >= prefix_lower.len()
                        && name.is_char_boundary(prefix_lower.len())
                        && name[..prefix_lower.len()].eq_ignore_ascii_case(&prefix_lower)
                };
                starts_with_ci && !already.iter().any(|existing| existing == *name)
            })
            .take(6)
            .cloned()
            .collect();

        if self.suggestion_cursor >= self.suggestions.len() {
            self.suggestion_cursor = 0;
        }
    }

    const fn move_suggestion(&mut self, delta: isize) {
        if self.suggestions.is_empty() {
            return;
        }
        let len = self.suggestions.len();
        if delta.is_negative() {
            self.suggestion_cursor = if self.suggestion_cursor == 0 {
                len - 1
            } else {
                self.suggestion_cursor - 1
            };
        } else {
            self.suggestion_cursor = (self.suggestion_cursor + 1) % len;
        }
    }

    fn apply_suggestion(&mut self) -> bool {
        if self.suggestions.is_empty() {
            return false;
        }
        let selected = self.suggestions[self.suggestion_cursor].clone();
        let Some(input) = self.recipient_input_mut() else {
            return false;
        };
        let current = input.value().to_string();
        let (prefix_start, _) = split_recipient_prefix(&current);
        let base = current[..prefix_start].trim_end();
        let next = if base.is_empty() {
            selected
        } else if base.ends_with(',') {
            format!("{base} {selected}")
        } else {
            format!("{base}, {selected}")
        };
        input.set_value(&next);
        self.refresh_suggestions();
        true
    }
}

#[derive(Debug, Clone)]
struct QuickReplyFormState {
    context: QuickReplyContext,
    body_input: TextArea,
    ack_required: bool,
    focus: QuickReplyField,
    errors: QuickReplyValidationErrors,
}

impl QuickReplyFormState {
    fn from_entry(entry: &MessageEntry) -> Option<Self> {
        let context = QuickReplyContext::from_entry(entry)?;
        let mut form = Self {
            context,
            body_input: TextArea::new()
                .with_placeholder("Reply body (Markdown)...")
                .with_soft_wrap(true)
                .with_focus(true),
            ack_required: entry.ack_required,
            focus: QuickReplyField::Body,
            errors: QuickReplyValidationErrors::default(),
        };
        form.update_focus();
        Some(form)
    }

    fn update_focus(&mut self) {
        self.body_input
            .set_focused(matches!(self.focus, QuickReplyField::Body));
    }

    fn set_focus(&mut self, focus: QuickReplyField) {
        self.focus = focus;
        self.update_focus();
    }

    fn cycle_focus_next(&mut self) {
        self.set_focus(self.focus.next());
    }

    fn cycle_focus_prev(&mut self) {
        self.set_focus(self.focus.prev());
    }
}

#[derive(Debug, Clone)]
struct MessageDetailRenderCache {
    message_id: i64,
    width: u16,
    body_hash: u64,
    theme_key: &'static str,
    rendered: Text<'static>,
    estimated_lines: usize,
}

#[derive(Debug, Clone)]
struct QuickReplyPreviewCache {
    body_hash: u64,
    width: u16,
    theme_key: &'static str,
    rendered: Text<'static>,
}

thread_local! {
    static QUICK_REPLY_PREVIEW_CACHE: RefCell<Option<QuickReplyPreviewCache>> = const { RefCell::new(None) };
}

// ──────────────────────────────────────────────────────────────────────
// MessageBrowserScreen
// ──────────────────────────────────────────────────────────────────────

/// Full-text search and browsing across all messages.
pub struct MessageBrowserScreen {
    search_input: TextInput,
    results: Vec<MessageEntry>,
    cursor: usize,
    detail_scroll: Cell<usize>,
    detail_view_mode: Cell<DetailViewMode>,
    focus: Focus,
    /// `VirtualizedList` state for efficient rendering.
    list_state: RefCell<VirtualizedListState>,
    /// Multi-selection state for batch actions.
    selected_message_ids: SelectionState<i64>,
    /// Last search term that was actually executed.
    last_search: String,
    /// Ticks remaining before executing a search after input changes.
    debounce_remaining: u8,
    /// Whether we need to re-query.
    search_dirty: bool,
    /// Lazy-opened DB connection for message queries.
    db_conn: Option<DbConn>,
    /// Holds the temp snapshot directory alive when this screen binds to an archive snapshot.
    _db_snapshot_dir: Option<crate::SnapshotDirGuard>,
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
    /// Reduced-motion mode forces static urgency badges.
    reduced_motion: bool,
    /// Small animation phase for header/status flourish.
    ui_phase: u8,
    /// Resizable results/detail layout.
    dock: DockLayout,
    /// Current drag state while resizing dock split.
    dock_drag: DockDragState,
    /// Pointer drag state for message re-thread operations.
    message_drag: MessageDragState,
    /// Last rendered content area for hit testing.
    last_content_area: Cell<Rect>,
    /// Last rendered search bar area.
    last_search_area: Cell<Rect>,
    /// Last rendered results area.
    last_results_area: Cell<Rect>,
    /// Last rendered detail area.
    last_detail_area: Cell<Rect>,
    /// Last observed data generation for dirty-state tracking.
    last_data_gen: super::DataGeneration,
    /// Quick reply modal state (when active).
    quick_reply_form: Option<QuickReplyFormState>,
    /// Message compose modal state (when active).
    compose_form: Option<ComposeFormState>,
    /// Cache for rendered message body with inline images:
    detail_cache: RefCell<Option<MessageDetailRenderCache>>,
    /// Per-selection collapsible JSON tree interaction state.
    json_tree_state: RefCell<crate::tui_markdown::JsonTreeViewState>,
    /// On-disk path for persisted screen filter presets.
    filter_presets_path: PathBuf,
    /// Preset store loaded from `filter_presets_path`.
    filter_presets: ScreenFilterPresetStore,
    /// Cursor position when `focused_synthetic` was last built.
    /// Avoids rebuilding the synthetic event every tick when cursor hasn't moved.
    focused_sync_cursor: usize,
    /// Results generation counter incremented whenever `results` changes.
    results_generation: usize,
    /// Results generation when `focused_synthetic` was last built.
    focused_sync_results_gen: usize,
    /// Active preset dialog mode (save/load/none).
    preset_dialog_mode: PresetDialogMode,
    /// Save dialog field focus.
    save_preset_field: SavePresetField,
    /// Save dialog: preset name input buffer.
    save_preset_name: String,
    /// Save dialog: optional description input buffer.
    save_preset_description: String,
    /// Load dialog selected preset row.
    load_preset_cursor: usize,
}

impl MessageBrowserScreen {
    fn build(filter_presets_path_override: Option<PathBuf>) -> Self {
        let filter_presets_path = filter_presets_path_override.unwrap_or_else(|| {
            let console_path = console_persist_path_from_env_or_default();
            screen_filter_presets_path(&console_path)
        });
        let filter_presets = load_screen_filter_presets_or_default(&filter_presets_path);
        Self {
            search_input: TextInput::new()
                .with_placeholder("Search messages... (/ to focus)")
                .with_focused(false),
            results: Vec::new(),
            cursor: 0,
            detail_scroll: Cell::new(0),
            detail_view_mode: Cell::new(DetailViewMode::Markdown),
            focus: Focus::ResultList,
            list_state: RefCell::new(VirtualizedListState::default()),
            selected_message_ids: SelectionState::new(),
            last_search: String::new(),
            debounce_remaining: 0,
            search_dirty: true, // Initial load
            db_conn: None,
            _db_snapshot_dir: None,
            db_conn_attempted: false,
            total_results: 0,
            last_refresh: None,
            preset_index: 0,
            search_method: SearchMethod::None,
            focused_synthetic: None,
            inbox_mode: InboxMode::Global,
            last_local_project: None,
            reduced_motion: reduced_motion_enabled(),
            ui_phase: 0,
            dock: DockLayout::right_40(),
            dock_drag: DockDragState::Idle,
            message_drag: MessageDragState::Idle,
            last_content_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_search_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_results_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_detail_area: Cell::new(Rect::new(0, 0, 0, 0)),
            last_data_gen: super::DataGeneration::stale(),
            quick_reply_form: None,
            compose_form: None,
            detail_cache: RefCell::new(None),
            json_tree_state: RefCell::new(crate::tui_markdown::JsonTreeViewState::default()),
            focused_sync_cursor: usize::MAX,
            results_generation: 0,
            focused_sync_results_gen: usize::MAX,
            filter_presets_path,
            filter_presets,
            preset_dialog_mode: PresetDialogMode::None,
            save_preset_field: SavePresetField::Name,
            save_preset_name: String::new(),
            save_preset_description: String::new(),
            load_preset_cursor: 0,
        }
    }

    #[cfg(test)]
    fn with_filter_presets_path_for_test(path: &std::path::Path) -> Self {
        Self::build(Some(path.to_path_buf()))
    }

    #[must_use]
    pub fn new() -> Self {
        Self::build(None)
    }

    fn bind_observability_db(
        &mut self,
        database_url: &str,
        storage_root: &std::path::Path,
        context: &str,
    ) {
        match crate::open_observability_sync_db_connection(database_url, storage_root, context) {
            Ok(Some(db)) => {
                let (conn, _, snapshot_dir) = db.into_parts();
                self.db_conn = Some(conn);
                self._db_snapshot_dir = snapshot_dir;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, context, "messages screen db bind failed");
            }
        }
    }

    fn update_urgent_pulse(&self, tick_count: u64) {
        if self.reduced_motion {
            MESSAGE_URGENT_PULSE_ON.store(true, Ordering::Relaxed);
            return;
        }
        let pulse_on = (tick_count / URGENT_PULSE_HALF_PERIOD_TICKS).is_multiple_of(2);
        MESSAGE_URGENT_PULSE_ON.store(pulse_on, Ordering::Relaxed);
    }

    fn compose_project_slug_from_context(&self) -> Option<String> {
        match &self.inbox_mode {
            InboxMode::Local(slug) if !slug.is_empty() && slug != "default" => {
                return Some(slug.clone());
            }
            _ => {}
        }

        if let Some(entry) = self.results.get(self.cursor)
            && !entry.project_slug.is_empty()
        {
            return Some(entry.project_slug.clone());
        }

        None
    }

    fn latest_project_slug(&self) -> Option<String> {
        let conn = self.db_conn.as_ref()?;
        let sql = "SELECT slug FROM projects ORDER BY created_at DESC LIMIT 1";
        conn.query_sync(sql, &[])
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .and_then(|row| row.get_named::<String>("slug").ok())
    }

    fn load_agent_names_for_project(&self, project_slug: &str) -> Result<Vec<String>, String> {
        let Some(conn) = &self.db_conn else {
            return Err("Database connection unavailable".to_string());
        };
        let (sql, params) = synthetic_project_id(project_slug).map_or_else(
            || {
                (
                    "SELECT a.name AS name \
                 FROM agents a \
                 WHERE a.project_id = (SELECT id FROM projects WHERE slug = ? LIMIT 1) \
                 ORDER BY a.name",
                    vec![Value::Text(project_slug.to_string())],
                )
            },
            |project_id| {
                (
                    "SELECT name FROM agents WHERE project_id = ? ORDER BY name",
                    vec![Value::BigInt(project_id)],
                )
            },
        );
        conn.query_sync(sql, &params)
            .map_err(|err| format!("Failed to load agents for compose: {err}"))
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|row| row.get_named::<String>("name").ok())
                    .collect()
            })
    }

    fn open_compose_error_form(
        &mut self,
        project_slug: String,
        prefill_to: Option<String>,
        error: String,
    ) {
        let mut form = ComposeFormState::new(project_slug, prefill_to, Vec::new());
        form.errors.general = Some(error);
        self.quick_reply_form = None;
        self.compose_form = Some(form);
    }

    fn open_compose_modal(&mut self, state: Option<&TuiSharedState>, prefill_to: Option<String>) {
        if self.db_conn.is_none() {
            if let Some(state) = state {
                self.ensure_db_conn(state);
            } else {
                let config = mcp_agent_mail_core::Config::from_env();
                self.bind_observability_db(
                    &config.database_url,
                    &config.storage_root,
                    "messages compose modal",
                );
                self.db_conn_attempted = true;
            }
        }
        let project_slug = if let Some(project_slug) = self.compose_project_slug_from_context() {
            project_slug
        } else if let Some(project_slug) = self.latest_project_slug() {
            project_slug
        } else {
            if self.db_conn.is_none() {
                self.db_conn_attempted = false;
                self.open_compose_error_form(
                    String::new(),
                    prefill_to,
                    "Database connection unavailable".to_string(),
                );
                return;
            }
            self.open_compose_error_form(
                String::new(),
                prefill_to,
                "Unable to determine project for compose. Select a message first or switch to Local mode."
                    .to_string(),
            );
            return;
        };

        let agents = match self.load_agent_names_for_project(&project_slug) {
            Ok(agents) => agents,
            Err(error) => {
                self.db_conn_attempted = false;
                self.open_compose_error_form(project_slug, prefill_to, error);
                return;
            }
        };
        let mut form = ComposeFormState::new(project_slug, prefill_to, agents);
        if form.available_agents.is_empty() {
            form.errors.general = Some(
                "No registered agents found in this project. Register agents before composing."
                    .to_string(),
            );
        }
        self.quick_reply_form = None;
        self.compose_form = Some(form);
    }

    fn ensure_modal_db_conn(&mut self, state: Option<&TuiSharedState>) {
        if self.db_conn.is_some() {
            return;
        }
        if let Some(shared) = state {
            self.ensure_db_conn(shared);
            if self.db_conn.is_some() {
                return;
            }
        }
        let config = mcp_agent_mail_core::Config::from_env();
        self.bind_observability_db(
            &config.database_url,
            &config.storage_root,
            "messages modal context",
        );
        self.db_conn_attempted = true;
    }

    fn runtime_config_from_state(state: Option<&TuiSharedState>) -> mcp_agent_mail_core::Config {
        state.map_or_else(mcp_agent_mail_core::Config::from_env, |shared| {
            let snapshot = shared.config_snapshot();
            let mut config = mcp_agent_mail_core::Config::from_env();
            config.database_url = snapshot.raw_database_url;
            config.storage_root = PathBuf::from(snapshot.storage_root);
            config
        })
    }

    fn open_live_mutation_db_connection(state: Option<&TuiSharedState>) -> Result<DbConn, String> {
        let config = Self::runtime_config_from_state(state);
        let sqlite_path = crate::resolve_server_database_url_sqlite_path(&config.database_url)
            .ok_or_else(|| "Database connection unavailable".to_string())?;
        if !sqlite_path.exists() {
            return Err("Database connection unavailable".to_string());
        }
        crate::open_interactive_sync_db_connection(sqlite_path.to_string_lossy().as_ref())
            .map_err(|e| format!("Database connection unavailable: {e}"))
    }

    fn fetch_message_entry_by_id(
        &mut self,
        message_id: i64,
        state: Option<&TuiSharedState>,
    ) -> Result<MessageEntry, String> {
        self.ensure_modal_db_conn(state);
        let Some(conn) = &self.db_conn else {
            return Err("Database connection unavailable".to_string());
        };
        let show_project = self.inbox_mode.is_global();
        let params = [Value::BigInt(message_id)];
        let rows = query_raw_message_rows(
            conn,
            "SELECT id, subject, body_md, thread_id, importance, ack_required, \
             created_ts, sender_id, project_id, recipients_json \
             FROM messages \
             WHERE id = ? \
             LIMIT 1",
            &params,
        );
        message_entries_from_rows(conn, rows, show_project)
            .into_iter()
            .next()
            .ok_or_else(|| format!("Message #{message_id} not found"))
    }

    fn open_quick_reply_modal_for_entry(&mut self, entry: &MessageEntry) -> Result<(), String> {
        let Some(form) = QuickReplyFormState::from_entry(entry) else {
            return Err("Selected message is missing required reply context".to_string());
        };
        self.quick_reply_form = Some(form);
        self.compose_form = None;
        Ok(())
    }

    fn open_quick_reply_modal_by_message_id(
        &mut self,
        message_id: i64,
        state: Option<&TuiSharedState>,
    ) -> Result<(), String> {
        if let Some(entry) = self
            .results
            .iter()
            .find(|entry| entry.id == message_id)
            .cloned()
        {
            return self.open_quick_reply_modal_for_entry(&entry);
        }
        let entry = self.fetch_message_entry_by_id(message_id, state)?;
        self.open_quick_reply_modal_for_entry(&entry)
    }

    fn ensure_compose_sender(
        &self,
        project_slug: &str,
        state: Option<&TuiSharedState>,
    ) -> Result<(), String> {
        let conn = Self::open_live_mutation_db_connection(state)?;
        let project_id = if let Some(project_id) = synthetic_project_id(project_slug) {
            project_id
        } else {
            let project_sql = "SELECT id FROM projects WHERE slug = ? LIMIT 1";
            conn.query_sync(project_sql, &[Value::Text(project_slug.to_string())])
                .map_err(|e| format!("Failed to resolve project: {e}"))?
                .into_iter()
                .next()
                .and_then(|row| row.get_named::<i64>("id").ok())
                .ok_or_else(|| format!("Project not found: {project_slug}"))?
        };

        let existing_sender = conn
            .query_sync(
                "SELECT id, inception_ts FROM agents WHERE project_id = ? AND name = ? LIMIT 1",
                &[
                    Value::BigInt(project_id),
                    Value::Text(COMPOSE_SENDER_NAME.to_string()),
                ],
            )
            .map_err(|e| format!("Failed to check compose sender: {e}"))?
            .into_iter()
            .next();
        let now = unix_epoch_micros_now().unwrap_or(0);
        let inception_ts = if let Some(row) = existing_sender {
            let existing_inception = row.get_named::<i64>("inception_ts").unwrap_or(now);
            conn.execute_sync(
                "UPDATE agents SET last_active_ts = ? WHERE project_id = ? AND name = ?",
                &[
                    Value::BigInt(now),
                    Value::BigInt(project_id),
                    Value::Text(COMPOSE_SENDER_NAME.to_string()),
                ],
            )
            .map_err(|e| format!("Failed to update compose sender activity: {e}"))?;
            existing_inception
        } else {
            conn.execute_sync(
                "INSERT INTO agents \
                 (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    Value::BigInt(project_id),
                    Value::Text(COMPOSE_SENDER_NAME.to_string()),
                    Value::Text("am-tui".to_string()),
                    Value::Text("human".to_string()),
                    Value::Text("Human operator composing messages in TUI".to_string()),
                    Value::BigInt(now),
                    Value::BigInt(now),
                    Value::Text("auto".to_string()),
                    Value::Text("auto".to_string()),
                ],
            )
            .map_err(|e| format!("Failed to create compose sender: {e}"))?;
            now
        };

        let config = Self::runtime_config_from_state(state);
        if let Ok(Some(archive)) = mcp_agent_mail_storage::open_archive(&config, project_slug) {
            let agent_json = serde_json::json!({
                "name": COMPOSE_SENDER_NAME,
                "program": "am-tui",
                "model": "human",
                "task_description": "Human operator composing messages in TUI",
                "inception_ts": micros_to_iso(inception_ts),
                "last_active_ts": micros_to_iso(now),
                "attachments_policy": "auto",
            });
            let _ = mcp_agent_mail_storage::write_agent_profile_with_config(
                &archive,
                &config,
                &agent_json,
            );
        }
        Ok(())
    }

    fn submit_compose_form(&mut self, state: Option<&TuiSharedState>) -> Cmd<MailScreenMsg> {
        let (payload, errors) = match self.compose_form.as_ref() {
            Some(form) => match validate_compose_form(form) {
                Ok(payload) => (Some(payload), ComposeValidationErrors::default()),
                Err(errors) => (None, *errors),
            },
            None => return Cmd::None,
        };
        if errors.has_any() {
            if let Some(form) = self.compose_form.as_mut() {
                form.errors = errors;
            }
            return Cmd::None;
        }
        let Some(mut payload) = payload else {
            return Cmd::None;
        };
        if payload.thread_id.is_none() {
            let micros = unix_epoch_micros_now().unwrap_or(0);
            payload.thread_id = Some(format!("tui-{micros}"));
        }

        if let Err(err) = self.ensure_compose_sender(&payload.project_slug, state) {
            if let Some(form) = self.compose_form.as_mut() {
                form.errors.general = Some(err.clone());
            }
            return Cmd::msg(MailScreenMsg::ActionExecute(
                "compose_result:error".to_string(),
                err,
            ));
        }

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = block_on(mcp_agent_mail_tools::messaging::send_message(
            &ctx,
            payload.project_slug.clone(),
            COMPOSE_SENDER_NAME.to_string(),
            payload.to.clone(),
            payload.subject.clone(),
            payload.body_md.clone(),
            if payload.cc.is_empty() {
                None
            } else {
                Some(payload.cc.clone())
            },
            None,
            None,
            None,
            Some(payload.importance.clone()),
            Some(payload.ack_required),
            payload.thread_id.clone(),
            None,
            None,
            None,
            None,
        ));

        match result {
            Ok(_) => {
                let to_summary = if payload.to.is_empty() {
                    if let Some(first) = payload.cc.first() {
                        if payload.cc.len() == 1 {
                            format!("cc: {first}")
                        } else {
                            format!("cc: {} (+{})", first, payload.cc.len() - 1)
                        }
                    } else {
                        "undisclosed".to_string()
                    }
                } else if payload.to.len() == 1 {
                    payload.to[0].clone()
                } else {
                    format!("{} (+{})", payload.to[0], payload.to.len() - 1)
                };
                let thread = payload.thread_id.unwrap_or_else(|| "n/a".to_string());
                self.compose_form = None;
                self.search_dirty = true;
                self.debounce_remaining = 0;
                Cmd::msg(MailScreenMsg::ActionExecute(
                    "compose_result:ok".to_string(),
                    format!("to {to_summary} · thread {thread}"),
                ))
            }
            Err(err) => {
                let message = err.to_string();
                if let Some(form) = self.compose_form.as_mut() {
                    form.errors.general = Some(message.clone());
                }
                Cmd::msg(MailScreenMsg::ActionExecute(
                    "compose_result:error".to_string(),
                    message,
                ))
            }
        }
    }

    fn submit_quick_reply_form(&mut self, state: Option<&TuiSharedState>) -> Cmd<MailScreenMsg> {
        let (body_md, errors) = match self.quick_reply_form.as_ref() {
            Some(form) => match validate_quick_reply_form(form) {
                Ok(body) => (Some(body), QuickReplyValidationErrors::default()),
                Err(errors) => (None, *errors),
            },
            None => return Cmd::None,
        };
        if errors.has_any() {
            if let Some(form) = self.quick_reply_form.as_mut() {
                form.errors = errors;
            }
            return Cmd::None;
        }
        let Some(body_md) = body_md else {
            return Cmd::None;
        };
        let Some(context) = self
            .quick_reply_form
            .as_ref()
            .map(|form| form.context.clone())
        else {
            return Cmd::None;
        };

        self.ensure_modal_db_conn(state);
        if let Err(err) = self.ensure_compose_sender(&context.project_slug, state) {
            if let Some(form) = self.quick_reply_form.as_mut() {
                form.errors.general = Some(err.clone());
            }
            return Cmd::msg(MailScreenMsg::ActionExecute(
                "quick_reply_result:error".to_string(),
                err,
            ));
        }

        let ack_required = self.quick_reply_form.as_ref().map(|form| form.ack_required);

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = block_on(mcp_agent_mail_tools::messaging::reply_message(
            &ctx,
            context.project_slug.clone(),
            context.message_id,
            COMPOSE_SENDER_NAME.to_string(),
            body_md,
            None,
            None,
            None,
            None,
            None,
            ack_required,
            None,
            None,
            None, // sender_token
        ));

        match result {
            Ok(_) => {
                let thread = context.thread_id.as_deref().unwrap_or("derived");
                let ack = if self
                    .quick_reply_form
                    .as_ref()
                    .is_some_and(|form| form.ack_required)
                {
                    "required"
                } else {
                    "optional"
                };
                self.quick_reply_form = None;
                self.search_dirty = true;
                self.debounce_remaining = 0;
                Cmd::msg(MailScreenMsg::ActionExecute(
                    "quick_reply_result:ok".to_string(),
                    format!("to {} · thread {thread} · ack {ack}", context.to_agent),
                ))
            }
            Err(err) => {
                let message = err.to_string();
                if let Some(form) = self.quick_reply_form.as_mut() {
                    form.errors.general = Some(message.clone());
                }
                Cmd::msg(MailScreenMsg::ActionExecute(
                    "quick_reply_result:error".to_string(),
                    message,
                ))
            }
        }
    }

    fn handle_quick_reply_event(
        &mut self,
        event: &Event,
        state: &TuiSharedState,
    ) -> Cmd<MailScreenMsg> {
        if self.quick_reply_form.is_none() {
            return Cmd::None;
        }
        if let Event::Mouse(mouse) = event {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let modal_area = quick_reply_modal_rect(self.screen_area());
                if !point_in_rect(modal_area, mouse.x, mouse.y) {
                    self.quick_reply_form = None;
                }
            }
            // Quick reply modal traps pointer input.
            return Cmd::None;
        }
        let Event::Key(key) = event else {
            return Cmd::None;
        };
        if key.kind != KeyEventKind::Press {
            return Cmd::None;
        }

        let ctrl_enter =
            key.modifiers.contains(Modifiers::CTRL) && matches!(key.code, KeyCode::Enter);
        if matches!(key.code, KeyCode::Escape) {
            self.quick_reply_form = None;
            return Cmd::None;
        }
        if ctrl_enter || matches!(key.code, KeyCode::F(5)) {
            return self.submit_quick_reply_form(Some(state));
        }

        let Some(form) = self.quick_reply_form.as_mut() else {
            return Cmd::None;
        };
        match key.code {
            KeyCode::Tab => {
                form.cycle_focus_next();
                return Cmd::None;
            }
            KeyCode::BackTab => {
                form.cycle_focus_prev();
                return Cmd::None;
            }
            _ => {}
        }
        match form.focus {
            QuickReplyField::Body => {
                let _ = form.body_input.handle_event(event);
                form.errors.body = None;
                form.errors.general = None;
            }
            QuickReplyField::AckRequired => match key.code {
                KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Left | KeyCode::Right => {
                    form.ack_required = !form.ack_required;
                }
                _ => {}
            },
        }
        Cmd::None
    }

    fn handle_compose_event(
        &mut self,
        event: &Event,
        state: &TuiSharedState,
    ) -> Cmd<MailScreenMsg> {
        if self.compose_form.is_none() {
            return Cmd::None;
        }
        if let Event::Mouse(mouse) = event {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let modal_area = compose_modal_rect(self.screen_area());
                if !point_in_rect(modal_area, mouse.x, mouse.y) {
                    self.compose_form = None;
                }
            }
            // Compose modal traps pointer input.
            return Cmd::None;
        }
        let Event::Key(key) = event else {
            return Cmd::None;
        };
        if key.kind != KeyEventKind::Press {
            return Cmd::None;
        }

        let ctrl_enter =
            key.modifiers.contains(Modifiers::CTRL) && matches!(key.code, KeyCode::Enter);
        if matches!(key.code, KeyCode::Escape) {
            self.compose_form = None;
            return Cmd::None;
        }
        if ctrl_enter || matches!(key.code, KeyCode::F(5)) {
            return self.submit_compose_form(Some(state));
        }

        let Some(form) = self.compose_form.as_mut() else {
            return Cmd::None;
        };
        match key.code {
            KeyCode::Tab => {
                form.cycle_focus_next();
                return Cmd::None;
            }
            KeyCode::BackTab => {
                form.cycle_focus_prev();
                return Cmd::None;
            }
            _ => {}
        }

        apply_compose_field_key(form, event, key.code);
        Cmd::None
    }

    /// Toggle between Local and Global inbox modes.
    ///
    /// When switching Global -> Local, uses the last known project or the
    /// currently focused message's project. When switching Local -> Global,
    /// remembers the current project for later restoration.
    fn toggle_inbox_mode(&mut self) {
        let mut changed = false;
        match &self.inbox_mode {
            InboxMode::Global => {
                // Switch to Local mode
                // Use the last local project, or infer from the focused message
                let project_slug = self
                    .last_local_project
                    .clone()
                    .filter(|s| !s.is_empty() && s != "default")
                    .or_else(|| {
                        self.results
                            .get(self.cursor)
                            .map(|m| m.project_slug.clone())
                            .filter(|s| !s.is_empty())
                    })
                    .or_else(|| self.latest_project_slug());
                if let Some(project_slug) = project_slug {
                    self.inbox_mode = InboxMode::Local(project_slug);
                    changed = true;
                }
            }
            InboxMode::Local(slug) => {
                // Remember current project before switching to Global
                self.last_local_project = Some(slug.clone());
                self.inbox_mode = InboxMode::Global;
                changed = true;
            }
        }
        if changed {
            // Trigger a re-query with the new mode.
            self.search_dirty = true;
            self.debounce_remaining = 0;
        }
    }

    /// Sync the `VirtualizedListState` with our cursor position.
    fn sync_list_state(&self) {
        let mut state = self.list_state.borrow_mut();
        if self.results.is_empty() {
            state.select(None);
        } else {
            state.select(Some(self.cursor));
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected message.
    fn sync_focused_event(&mut self) {
        if self.focused_sync_cursor == self.cursor
            && self.focused_sync_results_gen == self.results_generation
        {
            return;
        }
        self.focused_sync_cursor = self.cursor;
        self.focused_sync_results_gen = self.results_generation;
        self.focused_synthetic = self.results.get(self.cursor).map(|entry| {
            crate::tui_events::MailEvent::message_sent(
                entry.id,
                &entry.from_agent,
                entry.to_agents.split(", ").map(String::from).collect(),
                &entry.subject,
                &entry.thread_id,
                &entry.project_slug,
                &entry.body_md,
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

    fn preset_names(&self) -> Vec<String> {
        self.filter_presets.list_names(MESSAGES_PRESET_SCREEN_ID)
    }

    fn persist_filter_presets(&self) {
        if let Err(err) =
            save_screen_filter_presets(&self.filter_presets_path, &self.filter_presets)
        {
            eprintln!(
                "messages: failed to save presets to {}: {err}",
                self.filter_presets_path.display()
            );
        }
    }

    fn snapshot_filter_values(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        values.insert(
            "query".to_string(),
            self.search_input.value().trim().to_string(),
        );
        values.insert("preset_index".to_string(), self.preset_index.to_string());
        match &self.inbox_mode {
            InboxMode::Local(project_slug) => {
                values.insert("inbox_mode".to_string(), "local".to_string());
                values.insert("local_project".to_string(), project_slug.clone());
            }
            InboxMode::Global => {
                values.insert("inbox_mode".to_string(), "global".to_string());
                if let Some(slug) = &self.last_local_project {
                    values.insert("last_local_project".to_string(), slug.clone());
                }
            }
        }
        values
    }

    fn save_named_preset(&mut self, name: &str, description: Option<String>) -> bool {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return false;
        }
        let trimmed_description = description.and_then(|text| {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        self.filter_presets.upsert(
            MESSAGES_PRESET_SCREEN_ID,
            trimmed_name.to_string(),
            trimmed_description,
            self.snapshot_filter_values(),
        );
        self.persist_filter_presets();
        true
    }

    fn apply_preset_values(&mut self, values: &BTreeMap<String, String>) {
        let query = values.get("query").cloned().unwrap_or_default();
        self.search_input.set_value(&query);
        if let Some(raw_index) = values
            .get("preset_index")
            .and_then(|raw| raw.parse::<usize>().ok())
        {
            self.preset_index = raw_index.min(QUERY_PRESETS.len().saturating_sub(1));
        } else if let Some((idx, _)) = QUERY_PRESETS
            .iter()
            .enumerate()
            .find(|(_, preset)| preset.query == query)
        {
            self.preset_index = idx;
        } else {
            self.preset_index = 0;
        }

        match values
            .get("inbox_mode")
            .map(|raw| raw.trim().to_ascii_lowercase())
        {
            Some(mode) if mode == "local" => {
                let local = values
                    .get("local_project")
                    .cloned()
                    .or_else(|| self.last_local_project.clone())
                    .filter(|slug| !slug.is_empty() && slug != "default")
                    .or_else(|| self.latest_project_slug());
                if let Some(local) = local {
                    self.last_local_project = Some(local.clone());
                    self.inbox_mode = InboxMode::Local(local);
                } else {
                    self.inbox_mode = InboxMode::Global;
                }
            }
            _ => {
                self.inbox_mode = InboxMode::Global;
                if let Some(last_local) = values
                    .get("last_local_project")
                    .cloned()
                    .filter(|slug| !slug.is_empty())
                {
                    self.last_local_project = Some(last_local);
                }
            }
        }

        self.search_dirty = true;
        self.debounce_remaining = 0;
    }

    fn apply_named_preset(&mut self, name: &str) -> bool {
        let Some(preset) = self
            .filter_presets
            .get(MESSAGES_PRESET_SCREEN_ID, name)
            .cloned()
        else {
            return false;
        };
        self.apply_preset_values(&preset.values);
        true
    }

    fn remove_named_preset(&mut self, name: &str) -> bool {
        let removed = self.filter_presets.remove(MESSAGES_PRESET_SCREEN_ID, name);
        if removed {
            self.persist_filter_presets();
        }
        removed
    }

    fn open_save_preset_dialog(&mut self) {
        self.preset_dialog_mode = PresetDialogMode::Save;
        self.save_preset_field = SavePresetField::Name;
        self.save_preset_description.clear();
        if self.save_preset_name.is_empty() {
            self.save_preset_name = "messages-preset".to_string();
        }
    }

    fn open_load_preset_dialog(&mut self) {
        self.preset_dialog_mode = PresetDialogMode::Load;
        let names = self.preset_names();
        if names.is_empty() {
            self.load_preset_cursor = 0;
        } else {
            self.load_preset_cursor = self.load_preset_cursor.min(names.len().saturating_sub(1));
        }
    }

    fn handle_save_dialog_key(&mut self, key: &ftui::KeyEvent) {
        match key.code {
            KeyCode::Escape => {
                self.preset_dialog_mode = PresetDialogMode::None;
            }
            KeyCode::Tab => {
                self.save_preset_field = self.save_preset_field.next();
            }
            KeyCode::Backspace => match self.save_preset_field {
                SavePresetField::Name => {
                    self.save_preset_name.pop();
                }
                SavePresetField::Description => {
                    self.save_preset_description.pop();
                }
            },
            KeyCode::Enter => {
                let preset_name = self.save_preset_name.clone();
                if self.save_named_preset(&preset_name, Some(self.save_preset_description.clone()))
                {
                    self.preset_dialog_mode = PresetDialogMode::None;
                }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(Modifiers::CTRL) => {
                match self.save_preset_field {
                    SavePresetField::Name => self.save_preset_name.push(ch),
                    SavePresetField::Description => self.save_preset_description.push(ch),
                }
            }
            _ => {}
        }
    }

    fn handle_load_dialog_key(&mut self, key: &ftui::KeyEvent) {
        let names = self.preset_names();
        match key.code {
            KeyCode::Escape => {
                self.preset_dialog_mode = PresetDialogMode::None;
            }
            KeyCode::Char('j') | KeyCode::Down if !names.is_empty() => {
                self.load_preset_cursor = (self.load_preset_cursor + 1).min(names.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.load_preset_cursor = self.load_preset_cursor.saturating_sub(1);
            }
            KeyCode::Delete => {
                if let Some(name) = names.get(self.load_preset_cursor) {
                    let _ = self.remove_named_preset(name);
                }
                let refreshed = self.preset_names();
                if refreshed.is_empty() {
                    self.load_preset_cursor = 0;
                } else {
                    self.load_preset_cursor = self
                        .load_preset_cursor
                        .min(refreshed.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if let Some(name) = names.get(self.load_preset_cursor) {
                    let _ = self.apply_named_preset(name);
                    self.preset_dialog_mode = PresetDialogMode::None;
                }
            }
            _ => {}
        }
    }

    fn set_cursor_from_results_click(&mut self, y: u16) {
        if self.results.is_empty() {
            return;
        }
        let area = self.last_results_area.get();
        let list_height = area.height.saturating_sub(2) as usize;
        if list_height == 0 {
            return;
        }
        let inner_top = area.y.saturating_add(1);
        if y < inner_top {
            return;
        }
        let row = usize::from(y.saturating_sub(inner_top));
        if row >= list_height {
            return;
        }
        let start = self.list_state.borrow().scroll_offset();
        let idx = start.saturating_add(row);
        if idx < self.results.len() {
            self.cursor = idx;
            self.detail_scroll.set(0);
        }
    }

    fn result_index_at_y(&self, y: u16) -> Option<usize> {
        if self.results.is_empty() {
            return None;
        }
        let area = self.last_results_area.get();
        let list_height = area.height.saturating_sub(2) as usize;
        if list_height == 0 {
            return None;
        }
        let inner_top = area.y.saturating_add(1);
        if y < inner_top {
            return None;
        }
        let row = usize::from(y.saturating_sub(inner_top));
        if row >= list_height {
            return None;
        }
        let start = self.list_state.borrow().scroll_offset();
        let idx = start.saturating_add(row);
        (idx < self.results.len()).then_some(idx)
    }

    fn thread_id_for_result_index(&self, idx: usize) -> Option<String> {
        self.results.get(idx).and_then(|entry| {
            if entry.thread_id.is_empty() {
                None
            } else {
                Some(entry.thread_id.clone())
            }
        })
    }

    fn begin_pending_message_drag_for_result(&mut self, idx: usize, mouse_x: u16, mouse_y: u16) {
        let Some(entry) = self.results.get(idx) else {
            return;
        };
        self.message_drag = MessageDragState::Pending(PendingMessageDrag {
            message_id: entry.id,
            source_thread_id: entry.thread_id.clone(),
            source_project_slug: entry.project_slug.clone(),
            subject: entry.subject.clone(),
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
        let hovered_thread_id = if point_in_rect(self.last_results_area.get(), cursor_x, cursor_y) {
            self.result_index_at_y(cursor_y)
                .and_then(|idx| self.thread_id_for_result_index(idx))
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

    fn selected_result_entry(&self) -> Option<&MessageEntry> {
        self.results.get(self.cursor)
    }

    fn selected_message_ids_sorted(&self) -> Vec<i64> {
        let mut ids = self.selected_message_ids.selected_items();
        ids.sort_unstable();
        ids
    }

    fn prune_selection_to_visible(&mut self) {
        let visible_ids: std::collections::HashSet<i64> =
            self.results.iter().map(|e| e.id).collect();
        self.selected_message_ids
            .retain(|message_id| visible_ids.contains(message_id));
    }

    fn clear_message_selection(&mut self) {
        self.selected_message_ids.clear();
    }

    fn toggle_selection_for_cursor(&mut self) {
        if let Some(entry) = self.selected_result_entry() {
            self.selected_message_ids.toggle(entry.id);
        }
    }

    fn select_all_visible_messages(&mut self) {
        self.selected_message_ids
            .select_all(self.results.iter().map(|entry| entry.id));
    }

    fn extend_visual_selection_to_cursor(&mut self) {
        if !self.selected_message_ids.visual_mode() {
            return;
        }
        if let Some(entry) = self.selected_result_entry() {
            self.selected_message_ids.select(entry.id);
        }
    }

    fn mark_selected_result_for_keyboard_move(&self, state: &TuiSharedState) {
        let Some(entry) = self.selected_result_entry() else {
            return;
        };
        if entry.thread_id.is_empty() {
            return;
        }
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: entry.id,
            subject: entry.subject.clone(),
            source_thread_id: entry.thread_id.clone(),
            source_project_slug: entry.project_slug.clone(),
        }));
    }

    fn execute_keyboard_move_to_selected_context(
        &self,
        state: &TuiSharedState,
    ) -> Cmd<MailScreenMsg> {
        let Some(marker) = state.keyboard_move_snapshot() else {
            return Cmd::None;
        };
        let Some(target_thread_id) = self
            .selected_result_entry()
            .map(|entry| entry.thread_id.as_str())
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

    const fn detail_visible(&self) -> bool {
        let area = self.last_detail_area.get();
        area.width > 0 && area.height > 0
    }

    fn screen_area(&self) -> Rect {
        let search = self.last_search_area.get();
        let content = self.last_content_area.get();
        let bottom = content.y.saturating_add(content.height);
        let height = bottom.saturating_sub(search.y);
        Rect::new(search.x, search.y, search.width.max(content.width), height)
    }

    fn detail_content_wrap_width(area: Rect) -> usize {
        if area.width == 0 {
            return 80;
        }
        let inner_width = area.width.saturating_sub(2);
        let width_with_scrollbar = if inner_width > 6 {
            inner_width.saturating_sub(1)
        } else {
            inner_width
        };
        let content_width = if width_with_scrollbar > 2 {
            width_with_scrollbar.saturating_sub(2)
        } else {
            width_with_scrollbar
        };
        usize::from(content_width.max(1))
    }

    /// Rough estimate of lines in the detail panel for a message entry.
    fn detail_max_scroll(&self) -> usize {
        let Some(entry) = self.results.get(self.cursor) else {
            return 0;
        };
        let area = self.last_detail_area.get();
        // Fallback viewport for pre-render calls (unit tests or early key events).
        let visible_height = if area.height <= 2 {
            8
        } else {
            usize::from(area.height.saturating_sub(2))
        };
        let content_width = Self::detail_content_wrap_width(area);
        let total_lines = if self.detail_view_mode.get() == DetailViewMode::JsonTree {
            let tree_snapshot = {
                let mut tree = self.json_tree_state.borrow_mut();
                if tree.sync_body(&entry.body_md) {
                    tree.clamp_cursor();
                    Some((tree.rows(), tree.cursor()))
                } else {
                    None
                }
            };

            if let Some((rows, cursor)) = tree_snapshot {
                let mut combined_lines: Vec<Line<'static>> = vec![
                    Line::raw(format!("From:    {}", entry.from_agent)),
                    Line::raw(format!("To:      {}", entry.to_agents)),
                    Line::raw(format!("Subject: {}", entry.subject)),
                ];
                if !entry.thread_id.is_empty() {
                    combined_lines.push(Line::raw(format!("Thread:  {}", entry.thread_id)));
                }
                combined_lines.push(Line::raw(format!("Project: {}", entry.project_slug)));
                combined_lines.push(Line::raw(format!("Time:    {}", entry.timestamp_iso)));
                combined_lines.push(Line::raw(format!("Import.: {}", entry.importance)));
                if entry.ack_required {
                    combined_lines.push(Line::raw("Ack:     required".to_string()));
                }
                if entry.id >= 0 {
                    combined_lines.push(Line::raw(format!("ID:      #{}", entry.id)));
                }
                combined_lines.push(Line::raw(String::new()));
                combined_lines.push(Line::raw("--- JSON Tree ---".to_string()));

                let tp = crate::tui_theme::TuiThemePalette::current();
                let selected_style = Style::default().fg(tp.selection_indicator).bold();
                let marker_style = Style::default().fg(tp.text_disabled);
                for (idx, row) in rows.iter().enumerate() {
                    let selected = idx == cursor;
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    spans.push(Span::styled(
                        if selected {
                            "▸ ".to_string()
                        } else {
                            "  ".to_string()
                        },
                        if selected {
                            selected_style
                        } else {
                            marker_style
                        },
                    ));
                    spans.extend(row.line.spans().iter().cloned());
                    combined_lines.push(Line::from_spans(spans));
                }

                estimate_wrapped_line_count(&combined_lines, content_width)
            } else {
                let width_for_estimate =
                    u16::try_from(content_width.saturating_add(2)).unwrap_or(u16::MAX);
                estimate_message_detail_lines(entry, width_for_estimate)
            }
        } else {
            let width_for_estimate =
                u16::try_from(content_width.saturating_add(2)).unwrap_or(u16::MAX);
            estimate_message_detail_lines(entry, width_for_estimate)
        };
        total_lines.saturating_sub(visible_height)
    }

    fn scroll_detail_by(&self, delta: isize) {
        let max = self.detail_max_scroll();
        if delta.is_negative() {
            self.detail_scroll.set(
                self.detail_scroll
                    .get()
                    .saturating_sub(delta.unsigned_abs())
                    .min(max),
            );
        } else {
            #[allow(clippy::cast_sign_loss)]
            let add = delta as usize;
            self.detail_scroll
                .set(self.detail_scroll.get().saturating_add(add).min(max));
        }
    }

    fn toggle_detail_view_mode(&self) {
        match self.detail_view_mode.get() {
            DetailViewMode::Markdown => {
                let Some(entry) = self.results.get(self.cursor) else {
                    return;
                };
                if self.json_tree_state.borrow_mut().sync_body(&entry.body_md) {
                    self.detail_view_mode.set(DetailViewMode::JsonTree);
                    self.detail_scroll.set(0);
                }
            }
            DetailViewMode::JsonTree => {
                self.detail_view_mode.set(DetailViewMode::Markdown);
                self.detail_scroll.set(0);
            }
        }
    }

    fn sync_json_tree_scroll(&self, row_count: usize) {
        let area = self.last_detail_area.get();
        let visible_rows = usize::from(area.height.saturating_sub(4)).max(1);
        if row_count <= visible_rows {
            self.detail_scroll.set(0);
            return;
        }
        let cursor = self.json_tree_state.borrow().cursor();
        if cursor < self.detail_scroll.get() {
            self.detail_scroll.set(cursor);
        } else if cursor >= self.detail_scroll.get().saturating_add(visible_rows) {
            self.detail_scroll.set(
                cursor
                    .saturating_add(1)
                    .saturating_sub(visible_rows)
                    .min(row_count.saturating_sub(visible_rows)),
            );
        } else {
            self.detail_scroll.set(
                self.detail_scroll
                    .get()
                    .min(row_count.saturating_sub(visible_rows)),
            );
        }
    }

    fn handle_json_tree_navigation(&self, key: &ftui::KeyEvent) -> bool {
        if self.detail_view_mode.get() != DetailViewMode::JsonTree {
            return false;
        }

        let Some(entry) = self.results.get(self.cursor) else {
            self.detail_view_mode.set(DetailViewMode::Markdown);
            self.detail_scroll.set(0);
            return false;
        };
        if !self.json_tree_state.borrow_mut().sync_body(&entry.body_md) {
            self.detail_view_mode.set(DetailViewMode::Markdown);
            self.detail_scroll.set(0);
            return false;
        }

        let mut handled = true;
        match key.code {
            KeyCode::Char('J') => {
                self.toggle_detail_view_mode();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.json_tree_state.borrow_mut().move_cursor_by(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.json_tree_state.borrow_mut().move_cursor_by(-1);
            }
            KeyCode::Char('d') | KeyCode::PageDown => {
                self.json_tree_state.borrow_mut().move_cursor_by(8);
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                self.json_tree_state.borrow_mut().move_cursor_by(-8);
            }
            KeyCode::Home => {
                self.json_tree_state.borrow_mut().move_cursor_by(isize::MIN);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.json_tree_state.borrow_mut().move_cursor_by(isize::MAX);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let _ = self.json_tree_state.borrow_mut().toggle_selected();
            }
            KeyCode::Left => {
                let rows = self.json_tree_state.borrow().rows();
                if let Some(row) = rows.get(self.json_tree_state.borrow().cursor())
                    && row.expandable
                    && row.expanded
                {
                    let _ = self.json_tree_state.borrow_mut().toggle_selected();
                }
            }
            KeyCode::Right => {
                let rows = self.json_tree_state.borrow().rows();
                if let Some(row) = rows.get(self.json_tree_state.borrow().cursor())
                    && row.expandable
                    && !row.expanded
                {
                    let _ = self.json_tree_state.borrow_mut().toggle_selected();
                }
            }
            _ => handled = false,
        }

        if handled && self.detail_view_mode.get() == DetailViewMode::JsonTree {
            self.json_tree_state.borrow_mut().clamp_cursor();
            self.sync_json_tree_scroll(self.json_tree_state.borrow().rows().len());
        }
        handled
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
        let cfg = state.config_snapshot();
        self.bind_observability_db(
            &cfg.raw_database_url,
            std::path::Path::new(&cfg.storage_root),
            "messages screen",
        );
    }

    fn can_preserve_previous_results_for_live_fallback(
        &self,
        query: &str,
        show_project: bool,
        project_filter: Option<&str>,
    ) -> bool {
        if self.results.is_empty() || self.search_method == SearchMethod::Live {
            return false;
        }
        let same_query =
            (query.is_empty() && self.last_search.is_empty()) || self.last_search == query;
        same_query
            && self
                .results
                .iter()
                .all(|entry| entry.show_project == show_project)
            && project_filter
                .is_none_or(|slug| self.results.iter().all(|entry| entry.project_slug == slug))
    }

    fn merge_live_results_into_previous_results(
        previous_results: &[MessageEntry],
        live_results: Vec<MessageEntry>,
    ) -> Vec<MessageEntry> {
        let mut merged = previous_results.to_vec();
        let mut known_ids: std::collections::HashSet<i64> = previous_results
            .iter()
            .filter_map(|entry| (entry.id > 0).then_some(entry.id))
            .collect();
        for entry in live_results {
            if entry.id > 0 && !known_ids.insert(entry.id) {
                continue;
            }
            merged.push(entry);
        }
        merged.sort_by_key(|entry| std::cmp::Reverse(entry.timestamp_micros));
        merged
    }

    /// Execute a search query against the database, merging live event results.
    #[allow(clippy::too_many_lines)]
    fn execute_search(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let query = self.search_input.value().trim().to_string();
        self.last_refresh = Some(Instant::now());

        // Determine if we should show project column (Global mode)
        let show_project = self.inbox_mode.is_global();

        // Get optional project filter for Local mode
        let project_filter = match &self.inbox_mode {
            InboxMode::Local(slug) => Some(slug.as_str()),
            InboxMode::Global => None,
        };

        let Some(conn) = &self.db_conn else {
            let previous_results = self.results.clone();
            let previous_total = self.total_results;
            let preserve_previous = self.can_preserve_previous_results_for_live_fallback(
                &query,
                show_project,
                project_filter,
            );
            let mut results = Self::search_live_events(state, &query, show_project);
            if let Some(slug) = project_filter {
                results.retain(|entry| entry.project_slug == slug);
            }
            if preserve_previous {
                results =
                    Self::merge_live_results_into_previous_results(&previous_results, results);
            } else {
                results.sort_by_key(|entry| std::cmp::Reverse(entry.timestamp_micros));
            }

            if query.is_empty() {
                self.last_search.clear();
            } else {
                self.last_search.clone_from(&query);
            }
            self.search_method = SearchMethod::Live;

            self.results = results;
            self.results_generation = self.results_generation.wrapping_add(1);
            self.prune_selection_to_visible();
            self.total_results = if preserve_previous {
                previous_total.max(self.results.len())
            } else {
                self.results.len()
            };

            if self.results.is_empty() {
                self.cursor = 0;
            } else {
                self.cursor = self.cursor.min(self.results.len() - 1);
            }
            self.detail_scroll.set(0);
            self.search_dirty = false;
            // Avoid hot-looping failed open attempts on every tick.
            self.db_conn_attempted = false;
            return;
        };

        let (mut results, total, method) = if query.is_empty() {
            self.last_search.clear();
            let (r, t) = fetch_recent_messages(conn, PAGE_SIZE, project_filter, show_project);
            (r, t, SearchMethod::Recent)
        } else {
            self.last_search.clone_from(&query);
            let (r, t, m) =
                search_messages_unified(conn, &query, MAX_RESULTS, project_filter, show_project);
            (r, t, m)
        };
        self.search_method = method;

        // Merge live events from the event ring buffer.
        // Live events may contain messages not yet committed to the DB.
        let live = Self::search_live_events(state, &query, show_project);
        let mut live_added = 0usize;
        if !live.is_empty() {
            // Collect DB result IDs for dedup (live events with a positive ID
            // that already appears in DB results are skipped).
            let db_ids: std::collections::HashSet<i64> = results.iter().map(|r| r.id).collect();
            for entry in live {
                if entry.id > 0 && db_ids.contains(&entry.id) {
                    continue;
                }
                // Apply project filter for Local mode
                if let Some(slug) = project_filter
                    && entry.project_slug != slug
                {
                    continue;
                }
                results.push(entry);
                live_added = live_added.saturating_add(1);
            }
            // Re-sort by timestamp descending (newest first)
            results.sort_by_key(|r| std::cmp::Reverse(r.timestamp_micros));
        }

        self.results = results;
        self.results_generation = self.results_generation.wrapping_add(1);
        self.prune_selection_to_visible();
        self.total_results = total.saturating_add(live_added);

        // Clamp cursor
        if self.results.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.results.len() - 1);
        }
        self.detail_scroll.set(0);
        self.search_dirty = false;

        // Emit truthfulness diagnostic (br-2k3qx.2.2 / A2)
        let rendered_count = u64::try_from(self.results.len()).unwrap_or(u64::MAX);
        let raw_count_u64 = u64::try_from(self.total_results).unwrap_or(u64::MAX);
        let dropped_count = raw_count_u64.saturating_sub(rendered_count);
        let cfg = state.config_snapshot();
        let transport_mode = cfg.transport_mode().to_string();
        let scope = format!(
            "message_search.results;method={:?};live_added={live_added}",
            self.search_method
        );
        state.push_screen_diagnostic(crate::tui_bridge::ScreenDiagnosticSnapshot {
            screen: "messages".to_string(),
            scope,
            query_params: format!(
                "query={};inbox_mode={:?};total={};rendered={rendered_count};live_added={live_added}",
                if query.is_empty() { "(recent)" } else { &query },
                self.inbox_mode,
                self.total_results,
            ),
            raw_count: raw_count_u64,
            rendered_count,
            dropped_count,
            timestamp_micros: chrono::Utc::now().timestamp_micros(),
            db_url: cfg.database_url,
            storage_root: cfg.storage_root,
            transport_mode,
            auth_enabled: cfg.auth_enabled,
        });
    }

    /// Search the live event ring buffer for `MessageSent`/`MessageReceived` events.
    ///
    /// When `query` is empty, returns all recent message events (for merging
    /// with the "recent messages" default view).  When non-empty, filters by
    /// substring match against subject, sender, and recipients.
    fn search_live_events(
        state: &TuiSharedState,
        query: &str,
        show_project: bool,
    ) -> Vec<MessageEntry> {
        let query_lower = query.to_lowercase();
        let events = state.recent_events(500);
        events
            .iter()
            .filter_map(|e| {
                let (id, from, to, subject, thread_id, project, body_md) = match e {
                    MailEvent::MessageSent {
                        id,
                        from,
                        to,
                        subject,
                        thread_id,
                        project,
                        body_md,
                        ..
                    }
                    | MailEvent::MessageReceived {
                        id,
                        from,
                        to,
                        subject,
                        thread_id,
                        project,
                        body_md,
                        ..
                    } => (
                        *id,
                        from.as_str(),
                        to,
                        subject.as_str(),
                        thread_id.as_str(),
                        project.as_str(),
                        body_md.as_str(),
                    ),
                    _ => return None,
                };

                if !live_message_event_matches_query(
                    from,
                    to,
                    subject,
                    thread_id,
                    project,
                    body_md,
                    &query_lower,
                ) {
                    return None;
                }

                Some(MessageEntry {
                    id: if id > 0 { id } else { -1 },
                    subject: subject.to_string(),
                    from_agent: from.to_string(),
                    to_agents: to.join(", "),
                    project_slug: project.to_string(),
                    thread_id: thread_id.to_string(),
                    timestamp_iso: micros_to_iso(e.timestamp_micros()),
                    timestamp_micros: e.timestamp_micros(),
                    body_md: body_md.to_string(),
                    importance: "normal".to_string(),
                    ack_required: false,
                    show_project,
                })
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
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if self.quick_reply_form.is_some() {
            return self.handle_quick_reply_event(event, state);
        }
        if self.compose_form.is_some() {
            return self.handle_compose_event(event, state);
        }
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            if self.preset_dialog_mode != PresetDialogMode::None {
                match self.preset_dialog_mode {
                    PresetDialogMode::Save => self.handle_save_dialog_key(key),
                    PresetDialogMode::Load => self.handle_load_dialog_key(key),
                    PresetDialogMode::None => {}
                }
                return Cmd::None;
            }
            if key.modifiers.contains(Modifiers::CTRL) {
                match key.code {
                    KeyCode::Char('s') => {
                        self.open_save_preset_dialog();
                        return Cmd::None;
                    }
                    KeyCode::Char('l') => {
                        self.open_load_preset_dialog();
                        return Cmd::None;
                    }
                    _ => {}
                }
            }
        }
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Escape
            && state.keyboard_move_snapshot().is_some()
        {
            state.clear_keyboard_move_snapshot();
            return Cmd::None;
        }
        if !matches!(event, Event::Mouse(_)) && !matches!(self.message_drag, MessageDragState::Idle)
        {
            self.clear_message_drag_state(state);
        }
        match event {
            Event::Mouse(mouse) => {
                let content_area = self.last_content_area.get();
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.detail_visible()
                            && self.dock.hit_test_border(content_area, mouse.x, mouse.y)
                        {
                            self.dock_drag = DockDragState::Dragging;
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_search_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::SearchBar;
                            self.search_input.set_focused(true);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_results_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::ResultList;
                            self.search_input.set_focused(false);
                            self.set_cursor_from_results_click(mouse.y);
                            self.extend_visual_selection_to_cursor();
                            if let Some(idx) = self.result_index_at_y(mouse.y) {
                                self.begin_pending_message_drag_for_result(idx, mouse.x, mouse.y);
                            }
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                            self.focus = Focus::ResultList;
                            self.search_input.set_focused(false);
                            return Cmd::None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if self.dock_drag == DockDragState::Dragging {
                            self.dock.drag_to(content_area, mouse.x, mouse.y);
                            return Cmd::None;
                        }
                        self.update_active_message_drag(state, mouse.x, mouse.y);
                        if !matches!(self.message_drag, MessageDragState::Idle) {
                            return Cmd::None;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        self.dock_drag = DockDragState::Idle;
                        if !matches!(self.message_drag, MessageDragState::Idle) {
                            return self.finish_message_drag(state);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if point_in_rect(self.last_search_area.get(), mouse.x, mouse.y) {
                            self.apply_preset(self.preset_index + 1);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                            self.scroll_detail_by(1);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_results_area.get(), mouse.x, mouse.y)
                            && !self.results.is_empty()
                        {
                            self.cursor = (self.cursor + 1).min(self.results.len() - 1);
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                            return Cmd::None;
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if point_in_rect(self.last_search_area.get(), mouse.x, mouse.y) {
                            let idx = if self.preset_index == 0 {
                                QUERY_PRESETS.len() - 1
                            } else {
                                self.preset_index - 1
                            };
                            self.apply_preset(idx);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_detail_area.get(), mouse.x, mouse.y) {
                            self.scroll_detail_by(-1);
                            return Cmd::None;
                        }
                        if point_in_rect(self.last_results_area.get(), mouse.x, mouse.y) {
                            self.cursor = self.cursor.saturating_sub(1);
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                            return Cmd::None;
                        }
                    }
                    _ => {}
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match self.focus {
                Focus::SearchBar => match key.code {
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
                },
                Focus::ResultList => {
                    if self.handle_json_tree_navigation(key) {
                        return Cmd::None;
                    }
                    match key.code {
                        KeyCode::Escape if state.keyboard_move_snapshot().is_some() => {
                            state.clear_keyboard_move_snapshot();
                            return Cmd::None;
                        }
                        // Enter search mode
                        KeyCode::Char('/') | KeyCode::Tab => {
                            self.focus = Focus::SearchBar;
                            self.search_input.set_focused(true);
                            return Cmd::None;
                        }
                        // Cursor navigation
                        KeyCode::Char('j') | KeyCode::Down if !self.results.is_empty() => {
                            self.cursor = (self.cursor + 1).min(self.results.len() - 1);
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            self.cursor = self.cursor.saturating_sub(1);
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                        }
                        KeyCode::Char('G') | KeyCode::End if !self.results.is_empty() => {
                            self.cursor = self.results.len() - 1;
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                        }
                        KeyCode::Home => {
                            self.cursor = 0;
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                        }
                        // Toggle inbox mode (Local/Global)
                        KeyCode::Char('g') => {
                            self.toggle_inbox_mode();
                            return Cmd::None;
                        }
                        // Page navigation
                        KeyCode::Char('d') | KeyCode::PageDown if !self.results.is_empty() => {
                            self.cursor = (self.cursor + 20).min(self.results.len() - 1);
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                        }
                        KeyCode::Char('u') | KeyCode::PageUp => {
                            self.cursor = self.cursor.saturating_sub(20);
                            self.detail_scroll.set(0);
                            self.extend_visual_selection_to_cursor();
                        }
                        // Multi-select controls
                        KeyCode::Char(' ') => {
                            self.toggle_selection_for_cursor();
                            return Cmd::None;
                        }
                        KeyCode::Char('v') if !key.modifiers.contains(Modifiers::CTRL) => {
                            let enabled = self.selected_message_ids.toggle_visual_mode();
                            if enabled {
                                self.extend_visual_selection_to_cursor();
                            }
                            return Cmd::None;
                        }
                        KeyCode::Char('A') => {
                            self.select_all_visible_messages();
                            return Cmd::None;
                        }
                        KeyCode::Char('C') if !key.modifiers.contains(Modifiers::CTRL) => {
                            self.clear_message_selection();
                            return Cmd::None;
                        }
                        // Detail scroll
                        KeyCode::Char('J') => {
                            let previous_mode = self.detail_view_mode.get();
                            self.toggle_detail_view_mode();
                            if previous_mode == DetailViewMode::Markdown
                                && self.detail_view_mode.get() == DetailViewMode::Markdown
                            {
                                self.scroll_detail_by(1);
                            }
                        }
                        KeyCode::Char('K') => self.scroll_detail_by(-1),
                        // Split layout controls
                        KeyCode::Char('i') => self.dock.toggle_visible(),
                        KeyCode::Char(']') => self.dock.grow_dock(),
                        KeyCode::Char('[') => self.dock.shrink_dock(),
                        KeyCode::Char('}') => self.dock.cycle_position(),
                        KeyCode::Char('{') => self.dock.cycle_position_prev(),
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
                        // Compose message modal
                        KeyCode::Char('c') if !key.modifiers.contains(Modifiers::CTRL) => {
                            self.open_compose_modal(Some(state), None);
                            return Cmd::None;
                        }
                        // Quick reply modal
                        KeyCode::Char('r') if !key.modifiers.contains(Modifiers::CTRL) => {
                            if let Some(entry) = self.results.get(self.cursor).cloned()
                                && let Err(err) = self.open_quick_reply_modal_for_entry(&entry)
                            {
                                return Cmd::msg(MailScreenMsg::ActionExecute(
                                    "quick_reply_result:error".to_string(),
                                    err,
                                ));
                            }
                            return Cmd::None;
                        }
                        // Mark selected message for keyboard move.
                        KeyCode::Char('m') if key.modifiers.contains(Modifiers::CTRL) => {
                            self.mark_selected_result_for_keyboard_move(state);
                            return Cmd::None;
                        }
                        // Drop marked message onto current selected thread context.
                        KeyCode::Char('v') if key.modifiers.contains(Modifiers::CTRL) => {
                            return self.execute_keyboard_move_to_selected_context(state);
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
            },
            _ => {}
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.update_urgent_pulse(tick_count);
        self.ui_phase = (tick_count % 16) as u8;
        self.promote_pending_message_drag_if_due(state);
        // Debounce search execution
        if self.search_dirty {
            if self.debounce_remaining > 0 {
                self.debounce_remaining -= 1;
            } else {
                self.execute_search(state);
            }
        }

        if self.search_method == SearchMethod::Live {
            let should_retry_db = self.last_refresh.is_none_or(|t| t.elapsed().as_secs() >= 5);
            if should_retry_db {
                self.db_conn_attempted = false;
                self.search_dirty = true;
                self.debounce_remaining = 0;
            }
        }

        // Periodic refresh for empty-query mode (every 5 seconds),
        // but only when underlying data has actually changed.
        if self.search_input.value().is_empty() {
            let should_refresh = self.last_refresh.is_none_or(|t| t.elapsed().as_secs() >= 5);
            if should_refresh {
                let current_gen = state.data_generation();
                let dirty = super::dirty_since(&self.last_data_gen, &current_gen);
                if dirty.any() {
                    self.search_dirty = true;
                    self.debounce_remaining = 0;
                }
                self.last_data_gen = current_gen;
            }
        }
        self.sync_focused_event();
    }

    fn handle_action(&mut self, operation: &str, _context: &str) -> Cmd<MailScreenMsg> {
        match operation {
            "batch_acknowledge" => {
                let ids = self.selected_message_ids_sorted();
                self.clear_message_selection();
                let mut batch = Vec::new();
                for id in ids {
                    batch.push(Cmd::msg(MailScreenMsg::ActionExecute(
                        "acknowledge".to_string(),
                        format!("msg:{id}"),
                    )));
                }
                Cmd::batch(batch)
            }
            "batch_mark_read" => {
                let ids = self.selected_message_ids_sorted();
                self.clear_message_selection();
                let mut batch = Vec::new();
                for id in ids {
                    batch.push(Cmd::msg(MailScreenMsg::ActionExecute(
                        "mark_read".to_string(),
                        format!("msg:{id}"),
                    )));
                }
                Cmd::batch(batch)
            }
            "batch_mark_unread" => {
                let ids = self.selected_message_ids_sorted();
                self.clear_message_selection();
                let mut batch = Vec::new();
                for id in ids {
                    batch.push(Cmd::msg(MailScreenMsg::ActionExecute(
                        "mark_unread".to_string(),
                        format!("msg:{id}"),
                    )));
                }
                Cmd::batch(batch)
            }
            _ => Cmd::None,
        }
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
                    self.detail_scroll.set(0);
                    self.focus = Focus::ResultList;
                    self.search_input.set_focused(false);
                }
                true
            }
            DeepLinkTarget::ComposeToAgent(agent_name) => {
                let prefill = if agent_name.is_empty() {
                    None
                } else {
                    Some(agent_name.clone())
                };
                self.open_compose_modal(None, prefill);
                true
            }
            DeepLinkTarget::ReplyToMessage(message_id) => {
                let _ = self.open_quick_reply_modal_by_message_id(*message_id, None);
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 12 {
            let tp = crate::tui_theme::TuiThemePalette::current();
            Block::default()
                .title("Messages")
                .border_type(BorderType::Rounded)
                .border_style(crate::tui_theme::text_meta(&tp))
                .render(area, frame);
            return;
        }

        // Always paint the full content area so no cells remain stale between resizes.
        let tp = crate::tui_theme::TuiThemePalette::current();
        Paragraph::new("")
            .style(Style::default().fg(tp.text_primary).bg(tp.bg_deep))
            .render(area, frame);

        // Layout: search bar + dock-split content area.
        // Give the header an extra row on larger terminals for richer status text.
        let search_height: u16 = if area.height >= 18 {
            5
        } else if area.height >= 12 {
            4
        } else {
            3
        };
        let content_height = area.height.saturating_sub(search_height);

        let search_area = Rect::new(area.x, area.y, area.width, search_height);
        let content_area = Rect::new(area.x, area.y + search_height, area.width, content_height);
        self.last_search_area.set(search_area);
        self.last_content_area.set(content_area);

        // Render search bar with explainability and mode indicator
        let method_label = match self.search_method {
            SearchMethod::None => "",
            SearchMethod::Live => "live-events",
            SearchMethod::Recent => "recent",
            SearchMethod::Unified => "search-v3",
        };
        let preset_label = if self.preset_index > 0 {
            self.active_preset().label
        } else {
            ""
        };
        let mode_label = self.inbox_mode.label();
        let mut dock = self.dock;
        let mut stacked_fallback = false;
        if dock.visible
            && matches!(dock.position, DockPosition::Left | DockPosition::Right)
            && (dock.ratio - MESSAGE_DEFAULT_DOCK_RATIO).abs() <= 0.01
        {
            dock.set_ratio(responsive_message_dock_ratio(content_area.width));
        }
        if content_area.height < MESSAGE_DOCK_HIDE_HEIGHT_THRESHOLD {
            dock.visible = false;
        } else if content_area.width < MESSAGE_STACKED_WIDTH_THRESHOLD {
            if content_area.height >= MESSAGE_STACKED_MIN_HEIGHT {
                stacked_fallback = true;
                dock.visible = true;
                dock.position = DockPosition::Bottom;
                dock.set_ratio(MESSAGE_STACKED_DOCK_RATIO);
            } else {
                dock.visible = false;
            }
        }
        let layout_label = if dock.visible {
            if stacked_fallback {
                format!("Stacked {}", dock.state_label())
            } else {
                dock.state_label()
            }
        } else {
            "List only".to_string()
        };
        let telemetry = runtime_telemetry_line(state, self.ui_phase);
        render_search_bar(
            frame,
            search_area,
            &self.search_input,
            self.total_results,
            matches!(self.focus, Focus::SearchBar),
            method_label,
            preset_label,
            &mode_label,
            &layout_label,
            self.ui_phase,
            MESSAGE_URGENT_PULSE_ON.load(Ordering::Relaxed),
            &telemetry,
        );

        let split = dock.split(content_area);
        let mut results_area = split.primary;
        let mut detail_area = split.dock;
        if let Some(mut dock_area) = detail_area {
            let split_extent = if dock.position.is_horizontal() {
                content_area.height
            } else {
                content_area.width
            };
            let split_gap = u16::from(split_extent >= MESSAGE_SPLIT_GAP_THRESHOLD);
            if split_gap > 0 {
                let splitter_area = match dock.position {
                    DockPosition::Right => {
                        dock_area.x = dock_area.x.saturating_add(split_gap);
                        dock_area.width = dock_area.width.saturating_sub(split_gap);
                        Rect::new(
                            split.primary.x.saturating_add(split.primary.width),
                            content_area.y,
                            split_gap,
                            content_area.height,
                        )
                    }
                    DockPosition::Left => {
                        results_area.x = results_area.x.saturating_add(split_gap);
                        results_area.width = results_area.width.saturating_sub(split_gap);
                        Rect::new(
                            dock_area.x.saturating_add(dock_area.width),
                            content_area.y,
                            split_gap,
                            content_area.height,
                        )
                    }
                    DockPosition::Bottom => {
                        dock_area.y = dock_area.y.saturating_add(split_gap);
                        dock_area.height = dock_area.height.saturating_sub(split_gap);
                        Rect::new(
                            content_area.x,
                            split.primary.y.saturating_add(split.primary.height),
                            content_area.width,
                            split_gap,
                        )
                    }
                    DockPosition::Top => {
                        results_area.y = results_area.y.saturating_add(split_gap);
                        results_area.height = results_area.height.saturating_sub(split_gap);
                        Rect::new(
                            content_area.x,
                            dock_area.y.saturating_add(dock_area.height),
                            content_area.width,
                            split_gap,
                        )
                    }
                };
                render_splitter_handle(
                    frame,
                    splitter_area,
                    !dock.position.is_horizontal(),
                    self.dock_drag == DockDragState::Dragging,
                );
            }
            detail_area = if dock_area.width > 0 && dock_area.height > 0 {
                Some(dock_area)
            } else {
                None
            };
        }

        self.last_results_area.set(results_area);
        self.last_detail_area
            .set(detail_area.unwrap_or(Rect::new(0, 0, 0, 0)));

        // Sync and borrow list state for rendering
        self.sync_list_state();
        let mut list_state = self.list_state.borrow_mut();
        let results_focused = matches!(self.focus, Focus::ResultList);
        let drop_visual = match &self.message_drag {
            MessageDragState::Active(active) => Some(MessageDropVisual {
                source_thread_id: active.source_thread_id.as_str(),
                hovered_thread_id: active.hovered_thread_id.as_deref(),
                invalid_hover: active.invalid_hover,
            }),
            _ => None,
        };
        let keyboard_marked_message_id = state
            .keyboard_move_snapshot()
            .map(|marker| marker.message_id);
        render_results_list(
            frame,
            results_area,
            &self.results,
            &mut list_state,
            results_focused,
            state.config_snapshot().tui_effects,
            self.reduced_motion,
            drop_visual,
            keyboard_marked_message_id,
            &self.selected_message_ids,
        );
        drop(list_state);

        if let Some(detail_area) = detail_area {
            let mut json_rows: Option<Vec<crate::tui_markdown::JsonTreeRow>> = None;
            if self.detail_view_mode.get() == DetailViewMode::JsonTree {
                if let Some(entry) = self.results.get(self.cursor) {
                    if self.json_tree_state.borrow_mut().sync_body(&entry.body_md) {
                        self.json_tree_state.borrow_mut().clamp_cursor();
                        let rows = self.json_tree_state.borrow().rows();
                        self.sync_json_tree_scroll(rows.len());
                        json_rows = Some(rows);
                    } else {
                        self.detail_view_mode.set(DetailViewMode::Markdown);
                        self.detail_scroll.set(0);
                    }
                } else {
                    self.detail_view_mode.set(DetailViewMode::Markdown);
                    self.detail_scroll.set(0);
                }
            }
            render_detail_panel(
                frame,
                detail_area,
                self.results.get(self.cursor),
                self.detail_scroll.get(),
                !matches!(self.focus, Focus::SearchBar),
                &self.detail_cache,
                self.detail_view_mode.get(),
                json_rows.as_deref(),
                self.json_tree_state.borrow().cursor(),
            );
        }

        if let Some(form) = &self.quick_reply_form {
            render_quick_reply_modal(frame, area, form);
        } else if let Some(form) = &self.compose_form {
            render_compose_modal(frame, area, form);
        }

        match self.preset_dialog_mode {
            PresetDialogMode::Save => render_save_preset_dialog(
                frame,
                area,
                &self.save_preset_name,
                &self.save_preset_description,
                self.save_preset_field,
            ),
            PresetDialogMode::Load => {
                let names = self.preset_names();
                render_load_preset_dialog(frame, area, &names, self.load_preset_cursor);
            }
            PresetDialogMode::None => {}
        }
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
                key: "Space / v",
                action: "Toggle selection / visual select mode",
            },
            HelpEntry {
                key: "A / C",
                action: "Select all visible / clear selection",
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
                key: "J",
                action: "Toggle Markdown/JSON tree detail",
            },
            HelpEntry {
                key: "j/k + Enter (JSON)",
                action: "Move tree cursor / expand-collapse node",
            },
            HelpEntry {
                key: "K",
                action: "Scroll detail up (markdown)",
            },
            HelpEntry {
                key: "i [ ] { }",
                action: "Toggle/resize/reposition split",
            },
            HelpEntry {
                key: "Mouse",
                action: "Click/select, wheel preset/scroll, drag split",
            },
            HelpEntry {
                key: "Esc",
                action: "Exit search / cancel move",
            },
            HelpEntry {
                key: "Ctrl+C",
                action: "Clear search",
            },
            HelpEntry {
                key: "Ctrl+M / Ctrl+V",
                action: "Mark message / drop to current thread",
            },
            HelpEntry {
                key: "p/P",
                action: "Next/prev preset",
            },
            HelpEntry {
                key: "Ctrl+S",
                action: "Save current preset",
            },
            HelpEntry {
                key: "Ctrl+L",
                action: "Load saved preset",
            },
            HelpEntry {
                key: "Delete",
                action: "Delete selected preset (load dialog)",
            },
            HelpEntry {
                key: "c",
                action: "Compose message",
            },
            HelpEntry {
                key: "r",
                action: "Quick reply",
            },
            HelpEntry {
                key: "F5/Ctrl+Enter",
                action: "Submit compose/reply form",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some(
            "Browse and triage messages. Space/v/A/C manage multi-select; Ctrl+S/Ctrl+L manage saved presets; Enter jumps timeline.",
        )
    }

    fn consumes_text_input(&self) -> bool {
        self.compose_form.is_some()
            || self.quick_reply_form.is_some()
            || matches!(self.focus, Focus::SearchBar)
    }

    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        let message = self.results.get(self.cursor)?;
        let selected_ids = self.selected_message_ids_sorted();

        let mut actions = if selected_ids.len() > 1 {
            messages_batch_actions(selected_ids.len())
        } else {
            let thread_id = if message.thread_id.is_empty() {
                None
            } else {
                Some(message.thread_id.as_str())
            };

            messages_actions(message.id, thread_id, &message.from_agent)
        };

        if selected_ids.len() <= 1 && self.detail_view_mode.get() == DetailViewMode::JsonTree {
            let mut tree = self.json_tree_state.borrow_mut();
            if tree.sync_body(&message.body_md) {
                tree.clamp_cursor();
                if let Some(path) = tree.selected_path() {
                    actions.push(
                        ActionEntry::new("Copy JSON path", ActionKind::CopyToClipboard(path))
                            .with_description("Copy selected JSON node path")
                            .with_keybinding("Y"),
                    );
                }
                if let Some(value) = tree.selected_value_text() {
                    actions.push(
                        ActionEntry::new("Copy JSON value", ActionKind::CopyToClipboard(value))
                            .with_description("Copy selected JSON node value")
                            .with_keybinding("y"),
                    );
                }
            }
        }

        // Anchor row is cursor position + header offset
        #[allow(clippy::cast_possible_truncation)]
        let anchor_row = (self.cursor as u16).saturating_add(3);
        let context_id = if selected_ids.len() > 1 {
            format!(
                "batch:{}",
                selected_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            message.id.to_string()
        };

        Some((actions, anchor_row, context_id))
    }

    fn copyable_content(&self) -> Option<String> {
        let msg = self.results.get(self.cursor)?;
        if self.detail_view_mode.get() == DetailViewMode::JsonTree {
            let mut tree = self.json_tree_state.borrow_mut();
            if tree.sync_body(&msg.body_md) {
                tree.clamp_cursor();
                if let Some(payload) = tree.selected_copy_payload() {
                    return Some(payload);
                }
            }
        }
        if msg.body_md.is_empty() {
            Some(msg.subject.clone())
        } else {
            Some(format!("{}\n\n{}", msg.subject, msg.body_md))
        }
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
    let (sql, params, total) = match project_filter {
        Some(slug) => {
            let Some(project_id) = project_id_for_slug(conn, slug) else {
                return (Vec::new(), 0);
            };
            (
                format!(
                    "SELECT id, subject, body_md, thread_id, importance, ack_required, \
                     created_ts, sender_id, project_id, recipients_json \
                     FROM messages \
                     WHERE project_id = ? \
                     ORDER BY created_ts DESC \
                     LIMIT {limit}"
                ),
                vec![Value::BigInt(project_id)],
                count_messages(conn, project_filter),
            )
        }
        None => (
            format!(
                "SELECT id, subject, body_md, thread_id, importance, ack_required, \
                 created_ts, sender_id, project_id, recipients_json \
                 FROM messages \
                 ORDER BY created_ts DESC \
                 LIMIT {limit}"
            ),
            Vec::new(),
            count_messages(conn, None),
        ),
    };

    let rows = query_raw_message_rows(conn, &sql, &params);
    let results = message_entries_from_rows(conn, rows, show_project);
    (results, total)
}

/// Unified message search, returning results and the search method used.
///
/// If `project_filter` is Some, only search within that project (Local mode).
/// Otherwise, search across all projects (Global mode).
fn search_messages_unified(
    conn: &DbConn,
    query: &str,
    limit: usize,
    project_filter: Option<&str>,
    show_project: bool,
) -> (Vec<MessageEntry>, usize, SearchMethod) {
    let project_id = project_filter.and_then(|slug| project_id_for_slug(conn, slug));
    let mut search_query = mcp_agent_mail_db::search_planner::SearchQuery {
        text: query.trim().to_string(),
        doc_kind: mcp_agent_mail_db::search_planner::DocKind::Message,
        project_id,
        limit: Some(limit),
        ..Default::default()
    };
    // Messages screen expects newest-first ordering when scanning inbox content.
    search_query.ranking = mcp_agent_mail_db::search_planner::RankingMode::Recency;

    let response = match run_message_search_via_service(&search_query) {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!("messages screen unified search failed: {err}");
            return (Vec::new(), 0, SearchMethod::Unified);
        }
    };

    let message_ids: Vec<i64> = response.results.iter().map(|r| r.id).collect();
    let project_ids: Vec<i64> = response
        .results
        .iter()
        .filter_map(|r| r.project_id)
        .collect();
    let recipient_map = recipient_names_by_message(conn, &message_ids);
    let project_slug_map = project_slugs_by_id(conn, &project_ids);

    let mut out = Vec::with_capacity(response.results.len());
    for row in response.results {
        let created_ts = row.created_ts.unwrap_or(0);
        let project_slug = row
            .project_id
            .and_then(|pid| project_slug_map.get(&pid).cloned())
            .unwrap_or_default();
        out.push(MessageEntry {
            id: row.id,
            subject: row.title,
            from_agent: row.from_agent.unwrap_or_default(),
            to_agents: recipient_map.get(&row.id).cloned().unwrap_or_default(),
            project_slug,
            thread_id: row.thread_id.unwrap_or_default(),
            timestamp_iso: micros_to_iso(created_ts),
            timestamp_micros: created_ts,
            body_md: row.body,
            importance: row.importance.unwrap_or_else(|| "normal".to_string()),
            ack_required: row.ack_required.unwrap_or(false),
            show_project,
        });
    }

    let total = out.len();
    (out, total, SearchMethod::Unified)
}

fn run_message_search_via_service(
    query: &mcp_agent_mail_db::search_planner::SearchQuery,
) -> Result<mcp_agent_mail_db::search_planner::SearchResponse, String> {
    let pool_cfg = DbPoolConfig::from_env();
    let pool = mcp_agent_mail_db::create_pool(&pool_cfg)
        .map_err(|e| format!("failed to initialize DB pool: {e}"))?;
    let cx = Cx::for_request();
    match block_on(mcp_agent_mail_db::search_service::execute_search_simple(
        &cx, &pool, query,
    )) {
        asupersync::Outcome::Ok(resp) => Ok(resp),
        asupersync::Outcome::Err(e) => Err(e.to_string()),
        asupersync::Outcome::Cancelled(_) => Err("request cancelled".to_string()),
        asupersync::Outcome::Panicked(p) => Err(format!("request panicked: {p}")),
    }
}

fn project_id_for_slug(conn: &DbConn, slug: &str) -> Option<i64> {
    if let Some(project_id) = synthetic_project_id(slug) {
        return Some(project_id);
    }
    conn.query_sync(
        "SELECT id FROM projects WHERE slug = ? LIMIT 1",
        &[Value::Text(slug.to_string())],
    )
    .ok()
    .and_then(|rows| rows.into_iter().next())
    .and_then(|row| row.get_named::<i64>("id").ok())
}

fn project_slugs_by_id(
    conn: &DbConn,
    project_ids: &[i64],
) -> std::collections::HashMap<i64, String> {
    if project_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let mut dedup = std::collections::BTreeSet::new();
    dedup.extend(project_ids.iter().copied());
    if dedup.is_empty() {
        return std::collections::HashMap::new();
    }

    let placeholders = vec!["?"; dedup.len()].join(", ");
    let sql = format!("SELECT id, slug FROM projects WHERE id IN ({placeholders})");
    let params: Vec<Value> = dedup.iter().copied().map(Value::BigInt).collect();
    let mut slugs = std::collections::HashMap::new();
    if let Ok(rows) = conn.query_sync(&sql, &params) {
        for row in rows {
            let Ok(id) = row.get_named::<i64>("id") else {
                continue;
            };
            let slug = row
                .get_named::<String>("slug")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| unknown_project_label(id));
            slugs.insert(id, slug);
        }
    }
    for id in dedup {
        slugs.entry(id).or_insert_with(|| unknown_project_label(id));
    }
    slugs
}

fn recipient_names_by_message(
    conn: &DbConn,
    message_ids: &[i64],
) -> std::collections::HashMap<i64, String> {
    if message_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let mut dedup = std::collections::BTreeSet::new();
    dedup.extend(message_ids.iter().copied());
    if dedup.is_empty() {
        return std::collections::HashMap::new();
    }

    let placeholders = vec!["?"; dedup.len()].join(", ");
    let sql = format!(
        "SELECT mr.message_id, mr.agent_id AS raw_agent_id, a.name AS agent_name \
         FROM message_recipients mr \
         LEFT JOIN agents a ON a.id = mr.agent_id \
         WHERE mr.message_id IN ({placeholders}) \
         ORDER BY mr.message_id ASC, mr.agent_id ASC"
    );
    let params: Vec<Value> = dedup.into_iter().map(Value::BigInt).collect();
    conn.query_sync(&sql, &params)
        .ok()
        .map(|rows| {
            let mut grouped: std::collections::HashMap<i64, Vec<String>> =
                std::collections::HashMap::new();
            for row in rows {
                let Ok(message_id) = row.get_named::<i64>("message_id") else {
                    continue;
                };
                let agent_id = row.get_named::<i64>("raw_agent_id").unwrap_or(0);
                let agent_name = row
                    .get_named::<String>("agent_name")
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| format!("[unknown-agent-{agent_id}]"));
                let names = grouped.entry(message_id).or_default();
                if !names.iter().any(|existing| existing == &agent_name) {
                    names.push(agent_name);
                }
            }
            grouped
                .into_iter()
                .map(|(message_id, names)| (message_id, names.join(", ")))
                .collect()
        })
        .unwrap_or_default()
}

fn query_raw_message_rows(conn: &DbConn, sql: &str, params: &[Value]) -> Vec<RawMessageRow> {
    conn.query_sync(sql, params)
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    let created_ts = row.get_named::<i64>("created_ts").ok()?;
                    Some(RawMessageRow {
                        id: row.get_named::<i64>("id").ok()?,
                        subject: row.get_named::<String>("subject").ok().unwrap_or_default(),
                        sender_id: row.get_named::<i64>("sender_id").ok()?,
                        project_id: row.get_named::<i64>("project_id").ok()?,
                        thread_id: row
                            .get_named::<String>("thread_id")
                            .ok()
                            .unwrap_or_default(),
                        body_md: row.get_named::<String>("body_md").ok().unwrap_or_default(),
                        importance: row
                            .get_named::<String>("importance")
                            .ok()
                            .unwrap_or_else(|| "normal".to_string()),
                        ack_required: row.get_named::<i64>("ack_required").ok().unwrap_or(0) != 0,
                        created_ts,
                        recipients_json: row
                            .get_named::<String>("recipients_json")
                            .ok()
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn message_entries_from_rows(
    conn: &DbConn,
    rows: Vec<RawMessageRow>,
    show_project: bool,
) -> Vec<MessageEntry> {
    if rows.is_empty() {
        return Vec::new();
    }

    let sender_ids: Vec<i64> = rows.iter().map(|row| row.sender_id).collect();
    let project_ids: Vec<i64> = rows.iter().map(|row| row.project_id).collect();
    let sender_name_map = agent_names_by_id(conn, &sender_ids);
    let project_slug_map = project_slugs_by_id(conn, &project_ids);

    rows.into_iter()
        .map(|row| MessageEntry {
            id: row.id,
            subject: row.subject,
            from_agent: sender_name_map
                .get(&row.sender_id)
                .cloned()
                .unwrap_or_else(|| unknown_agent_label(row.sender_id)),
            to_agents: recipient_names_from_json(&row.recipients_json),
            project_slug: project_slug_map
                .get(&row.project_id)
                .cloned()
                .unwrap_or_else(|| unknown_project_label(row.project_id)),
            thread_id: row.thread_id,
            timestamp_iso: micros_to_iso(row.created_ts),
            timestamp_micros: row.created_ts,
            body_md: row.body_md,
            importance: row.importance,
            ack_required: row.ack_required,
            show_project,
        })
        .collect()
}

fn agent_names_by_id(conn: &DbConn, agent_ids: &[i64]) -> std::collections::HashMap<i64, String> {
    if agent_ids.is_empty() {
        return std::collections::HashMap::new();
    }

    let mut dedup = std::collections::BTreeSet::new();
    dedup.extend(agent_ids.iter().copied());
    if dedup.is_empty() {
        return std::collections::HashMap::new();
    }

    let placeholders = vec!["?"; dedup.len()].join(", ");
    let sql = format!("SELECT id, name FROM agents WHERE id IN ({placeholders})");
    let params: Vec<Value> = dedup.iter().copied().map(Value::BigInt).collect();
    let mut names = std::collections::HashMap::new();
    if let Ok(rows) = conn.query_sync(&sql, &params) {
        for row in rows {
            let Ok(id) = row.get_named::<i64>("id") else {
                continue;
            };
            let name = row
                .get_named::<String>("name")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| unknown_agent_label(id));
            names.insert(id, name);
        }
    }
    for id in dedup {
        names.entry(id).or_insert_with(|| unknown_agent_label(id));
    }
    names
}

fn unknown_agent_label(agent_id: i64) -> String {
    format!("[unknown-agent-{agent_id}]")
}

fn unknown_project_label(project_id: i64) -> String {
    format!("[unknown-project-{project_id}]")
}

fn synthetic_project_id(project_slug: &str) -> Option<i64> {
    let rest = project_slug.strip_prefix("[unknown-project-")?;
    let project_id = rest.strip_suffix(']')?;
    project_id.parse::<i64>().ok().filter(|id| *id > 0)
}

fn recipient_names_from_json(raw: &str) -> String {
    let parsed = serde_json::from_str::<StoredRecipients>(raw).unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    let mut recipients = Vec::new();
    for group in [parsed.to, parsed.cc, parsed.bcc] {
        for name in group {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            let folded = trimmed.to_ascii_lowercase();
            if seen.insert(folded) {
                recipients.push(trimmed.to_string());
            }
        }
    }
    recipients.join(", ")
}

/// Count total messages, optionally filtered by project.
fn count_messages(conn: &DbConn, project_filter: Option<&str>) -> usize {
    let (sql, params) = match project_filter {
        Some(slug) => {
            let Some(project_id) = project_id_for_slug(conn, slug) else {
                return 0;
            };
            (
                "SELECT COUNT(*) AS c FROM messages WHERE project_id = ?",
                vec![Value::BigInt(project_id)],
            )
        }
        None => ("SELECT COUNT(*) AS c FROM messages", Vec::new()),
    };

    conn.query_sync(sql, &params)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<i64>("c").ok())
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0)
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the search bar with explainability metadata and mode indicator.
#[allow(clippy::too_many_arguments)]
fn render_search_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &TextInput,
    total_results: usize,
    focused: bool,
    method_label: &str,
    preset_label: &str,
    mode_label: &str,
    layout_label: &str,
    ui_phase: u8,
    pulse_on: bool,
    telemetry: &str,
) {
    let spinner = spinner_glyph(ui_phase);
    let mut title = if focused {
        format!("{spinner} Search ({total_results} results) [EDITING]")
    } else {
        format!("{spinner} Search ({total_results} results)")
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
    if !layout_label.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut title, format_args!(" | {layout_label}"));
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::tui_theme::lerp_color(
            crate::tui_theme::focus_border_color(&tp, focused),
            tp.status_accent,
            0.45,
        )))
        .style(
            Style::default()
                .fg(tp.text_primary)
                .bg(crate::tui_theme::lerp_color(
                    tp.panel_bg,
                    tp.status_accent,
                    0.08,
                )),
        );
    let inner = block.inner(area);
    block.render(area, frame);

    // Render the TextInput inside the block
    if inner.height > 0 && inner.width > 0 {
        let content_inner = if inner.width > 2 {
            Rect::new(
                inner.x.saturating_add(1),
                inner.y,
                inner.width.saturating_sub(2),
                inner.height,
            )
        } else {
            inner
        };
        let input_area = Rect::new(content_inner.x, content_inner.y, content_inner.width, 1);
        input.render(input_area, frame);
        if content_inner.height > 1 {
            let pulse = if pulse_on { "\u{25cf}" } else { "\u{25cb}" };
            let meter = pulse_meter(ui_phase, 10);
            let hint = format!(
                "{pulse} {meter}  Mouse: click/select, wheel preset/scroll, drag split border   Ops: / j k J(toggle tree) K"
            );
            let hint_area = Rect::new(content_inner.x, content_inner.y + 1, content_inner.width, 1);
            Paragraph::new(truncate_str(&hint, content_inner.width as usize))
                .style(crate::tui_theme::text_hint(&tp))
                .render(hint_area, frame);
        }
        if content_inner.height > 2 {
            let telemetry_area =
                Rect::new(content_inner.x, content_inner.y + 2, content_inner.width, 1);
            Paragraph::new(truncate_str(telemetry, content_inner.width as usize))
                .style(Style::default().fg(tp.selection_indicator))
                .render(telemetry_area, frame);
        }
    }
}

/// Render the results list using `VirtualizedList`.
#[allow(clippy::too_many_arguments)]
fn render_results_list(
    frame: &mut Frame<'_>,
    area: Rect,
    results: &[MessageEntry],
    list_state: &mut VirtualizedListState,
    focused: bool,
    effects_enabled: bool,
    reduced_motion: bool,
    drop_visual: Option<MessageDropVisual<'_>>,
    keyboard_marked_message_id: Option<i64>,
    selected_message_ids: &SelectionState<i64>,
) {
    let selected_count = selected_message_ids.len();
    let title = if results.is_empty() {
        "Results".to_string()
    } else if selected_count > 0 {
        format!("Results ({}) · selected {selected_count}", results.len())
    } else {
        format!("Results ({})", results.len())
    };
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
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(
            Style::default()
                .fg(tp.text_primary)
                .bg(crate::tui_theme::lerp_color(
                    tp.panel_bg,
                    tp.selection_indicator,
                    0.07,
                )),
        );
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

    let shimmer_progresses =
        compute_shimmer_progresses(results, effects_enabled && !reduced_motion);
    let rows: Vec<MessageRenderRow<'_>> = results
        .iter()
        .enumerate()
        .map(|(idx, entry)| MessageRenderRow {
            entry,
            shimmer_progress: shimmer_progresses[idx],
            drop_zone: drop_visual.map_or(MessageDropZoneState::None, |drag| {
                let thread_id = (!entry.thread_id.is_empty()).then_some(entry.thread_id.as_str());
                let hovered_here = drag.hovered_thread_id == thread_id;
                let valid = thread_id.is_some_and(|tid| tid != drag.source_thread_id);
                if hovered_here {
                    if valid {
                        MessageDropZoneState::HoveredValid
                    } else {
                        MessageDropZoneState::HoveredInvalid
                    }
                } else if valid {
                    MessageDropZoneState::Valid
                } else {
                    MessageDropZoneState::None
                }
            }),
            keyboard_marked: keyboard_marked_message_id == Some(entry.id),
            batch_selected: selected_message_ids.contains(&entry.id),
        })
        .collect();

    let list = VirtualizedList::new(rows.as_slice())
        .style(crate::tui_theme::text_primary(&tp))
        .highlight_style(
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold(),
        )
        .show_scrollbar(rows.len() > usize::from(inner.height));

    StatefulWidget::render(&list, inner, frame, list_state);
}

/// Returns `true` if `body` (after trimming whitespace) starts with `{` or `[`,
/// indicating it is likely a JSON payload.
#[cfg(test)]
fn looks_like_json(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

/// Colorize a JSON body into styled `Line`s using the current TUI theme palette.
///
/// Uses the `JsonTokenizer` from ftui-extras for lexing, then post-processes
/// tokens to distinguish object keys from string values: a `String` token
/// immediately followed (ignoring whitespace) by a `Punctuation` token whose
/// text is `:` is classified as a key.
#[allow(dead_code)] // kept for tests and fallback experiments; main path uses markdown rendering.
fn colorize_json_body(body: &str, tp: &crate::tui_theme::TuiThemePalette) -> Text<'static> {
    let tokenizer = JsonTokenizer;
    let key_style = crate::tui_theme::style_json_key(tp);
    let string_style = crate::tui_theme::style_json_string(tp);
    let number_style = crate::tui_theme::style_json_number(tp);
    let literal_style = crate::tui_theme::style_json_literal(tp);
    let punct_style = crate::tui_theme::style_json_punctuation(tp);

    let mut lines: Vec<Line> = Vec::new();
    let mut state = LineState::Normal;

    for raw_line in body.split('\n') {
        // Strip trailing \r for CRLF sources
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let (tokens, new_state) = tokenizer.tokenize_line(line, state);
        state = new_state;

        if tokens.is_empty() {
            lines.push(Line::raw(line.to_string()));
            continue;
        }

        let mut spans: Vec<Span<'static>> = Vec::with_capacity(tokens.len());

        for (i, tok) in tokens.iter().enumerate() {
            let text = &line[tok.range.clone()];
            let style = match tok.kind {
                TokenKind::String => {
                    // Determine if this string is an object key by looking ahead
                    // past any whitespace tokens for a ':' punctuation token.
                    let mut is_key = false;
                    for following in &tokens[i + 1..] {
                        if following.kind == TokenKind::Whitespace {
                            continue;
                        }
                        if following.kind == TokenKind::Punctuation {
                            let ft = &line[following.range.clone()];
                            if ft == ":" {
                                is_key = true;
                            }
                        }
                        break;
                    }
                    if is_key { key_style } else { string_style }
                }
                TokenKind::Number => number_style,
                TokenKind::Boolean | TokenKind::Constant => literal_style,
                TokenKind::Delimiter | TokenKind::Punctuation => punct_style,
                _ => Style::default(),
            };
            // Allocate an owned String to decouple from the line borrow
            spans.push(Span::styled(text.to_string(), style));
        }

        lines.push(Line::from_spans(spans));
    }

    Text::from_lines(lines)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkdownImageRef {
    alt: String,
    source: String,
}

/// Extract markdown image references in the form `![alt](source)`.
fn collect_markdown_image_refs(markdown: &str) -> Vec<MarkdownImageRef> {
    let mut out = Vec::new();
    let mut cursor = 0usize;

    while cursor < markdown.len() {
        let Some(rel_start) = markdown[cursor..].find("![") else {
            break;
        };
        let alt_start = cursor + rel_start + 2;
        let Some(rel_alt_end) = markdown[alt_start..].find(']') else {
            break;
        };
        let alt_end = alt_start + rel_alt_end;
        let after_alt = alt_end.saturating_add(1);
        if after_alt >= markdown.len() || !markdown[after_alt..].starts_with('(') {
            cursor = after_alt;
            continue;
        }
        let src_start = after_alt + 1;
        let Some(rel_src_end) = markdown[src_start..].find(')') else {
            break;
        };
        let src_end = src_start + rel_src_end;

        let alt = markdown[alt_start..alt_end].trim();
        let source = markdown[src_start..src_end].trim();
        if !source.is_empty() {
            out.push(MarkdownImageRef {
                alt: alt.to_string(),
                source: source.to_string(),
            });
        }
        cursor = src_end.saturating_add(1);
    }

    out
}

const fn image_protocol_name(protocol: ImageProtocol) -> &'static str {
    match protocol {
        ImageProtocol::Kitty => "kitty",
        ImageProtocol::Iterm2 => "iterm2",
        ImageProtocol::Sixel => "sixel",
        ImageProtocol::Ascii => "ascii",
    }
}

/// Build a textual/ASCII image preview block appended below markdown body.
///
/// This favors robust degraded behavior: no panics on missing/invalid bytes.
fn build_inline_image_block(markdown: &str, width: u16) -> String {
    build_inline_image_block_with_hints(markdown, width, &DetectionHints::from_env())
}

fn build_inline_image_block_with_hints(
    markdown: &str,
    width: u16,
    hints: &DetectionHints,
) -> String {
    let refs = collect_markdown_image_refs(markdown);
    if refs.is_empty() {
        return String::new();
    }

    let caps = ftui::TerminalCapabilities::detect();
    let protocol = detect_protocol(caps, hints);
    let mut lines = Vec::new();
    let preview_width = u32::from(width.clamp(16, 120));

    for image_ref in refs {
        let label = if image_ref.alt.is_empty() {
            image_ref.source.as_str()
        } else {
            image_ref.alt.as_str()
        };
        lines.push(format!(
            "[Image: {label} | protocol={}]",
            image_protocol_name(protocol)
        ));

        let max_bytes = 5 * 1024 * 1024; // 5 MB limit
        let read_result = std::fs::File::open(&image_ref.source).and_then(|mut f| {
            use std::io::Read;
            let mut buf = Vec::new();
            f.by_ref().take(max_bytes + 1).read_to_end(&mut buf)?;
            if buf.len() > usize::try_from(max_bytes).unwrap_or(usize::MAX) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "image too large",
                ))
            } else {
                Ok(buf)
            }
        });

        match read_result {
            Ok(bytes) => match Image::from_bytes(&bytes) {
                Ok(image) => {
                    match protocol {
                        ImageProtocol::Kitty => match image.encode_kitty(
                            Some(preview_width),
                            Some(8),
                            ImageFit::Contain,
                        ) {
                            Ok(chunks) => lines.push(format!(
                                "[kitty inline payload prepared: {} chunk(s)]",
                                chunks.len()
                            )),
                            Err(_) => lines
                                .push(format!("[Image kitty encode failed: {}]", image_ref.source)),
                        },
                        ImageProtocol::Iterm2 => {
                            let options = Iterm2Options {
                                width: Some(Iterm2Dimension::Cells(preview_width)),
                                height: Some(Iterm2Dimension::Cells(8)),
                                ..Iterm2Options::default()
                            };
                            match image.encode_iterm2(
                                Some(preview_width),
                                Some(8),
                                ImageFit::Contain,
                                &options,
                            ) {
                                Ok(sequence) => lines.push(format!(
                                    "[iterm2 inline payload prepared: {} byte(s)]",
                                    sequence.len()
                                )),
                                Err(_) => lines.push(format!(
                                    "[Image iTerm2 encode failed: {}]",
                                    image_ref.source
                                )),
                            }
                        }
                        ImageProtocol::Sixel => lines.push(
                            "[sixel detected; using ASCII fallback preview (encoder unavailable)]"
                                .to_string(),
                        ),
                        ImageProtocol::Ascii => {}
                    }

                    for line in image.render_ascii(preview_width, 8, ImageFit::Contain) {
                        lines.push(truncate_str(&line, width as usize));
                    }
                }
                Err(_) => lines.push(format!("[Image decode failed: {}]", image_ref.source)),
            },
            Err(_) => lines.push(format!("[Image unavailable: {}]", image_ref.source)),
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

/// Render the detail panel for the selected message.
#[allow(clippy::cast_possible_truncation)]
/// Estimate how many lines the detail panel needs for a message entry.
fn estimate_message_detail_lines(entry: &MessageEntry, width: u16) -> usize {
    // Header lines: From, To, Subject, Project, Time, Importance = 6
    let mut count: usize = 6;
    if !entry.thread_id.is_empty() {
        count += 1; // Thread line
    }
    if entry.ack_required {
        count += 1; // Ack line
    }
    if entry.id >= 0 {
        count += 1; // ID line
    }
    count += 2; // Blank separator + "--- Body ---"

    // Body lines: approximate wrapping
    let avail_width = usize::from(width.saturating_sub(2)).max(1); // -2 for borders/padding
    let body_lines = entry
        .body_md
        .lines()
        .map(|line| {
            let len = ftui::text::display_width(line);
            if len == 0 {
                1
            } else {
                len.div_ceil(avail_width)
            }
        })
        .sum::<usize>()
        .max(1);

    count += body_lines;

    // Markdown image references may expand into additional inline preview rows.
    // Over-estimate to avoid clamping scroll too early.
    count += collect_markdown_image_refs(&entry.body_md)
        .len()
        .saturating_mul(12);
    count
}

fn estimate_wrapped_line_count(lines: &[Line<'_>], width: usize) -> usize {
    let wrap_width = width.max(1);
    lines
        .iter()
        .map(|line| {
            let len = line.width();
            if len == 0 {
                1
            } else {
                len.div_ceil(wrap_width)
            }
        })
        .sum::<usize>()
        .max(1)
}

fn wrap_markdown_for_message_panel<'a>(text: &Text<'a>, width: u16) -> Text<'a> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for line in text.lines() {
        let plain = line.to_plain_text();
        if is_rendered_markdown_table_line(&plain) {
            // Preserve table geometry; wrapped tables become visually broken.
            out.push(line.clone());
            continue;
        }
        if line.width() <= width {
            out.push(line.clone());
            continue;
        }
        out.extend(line.wrap(width, ftui::text::WrapMode::Word));
    }
    Text::from_lines(out)
}

fn is_rendered_markdown_table_line(plain: &str) -> bool {
    let trimmed = plain.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed
        .chars()
        .any(|c| matches!(c, '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' | '─'))
    {
        return true;
    }
    trimmed.starts_with('│')
        && trimmed.ends_with('│')
        && trimmed.chars().filter(|&c| c == '│').count() >= 2
}

fn stable_hash<T: Hash>(value: T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn contains_case_insensitive(text: &str, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    if query_lower.is_ascii() {
        let q_bytes = query_lower.as_bytes();
        text.as_bytes()
            .windows(q_bytes.len())
            .any(|w| w.eq_ignore_ascii_case(q_bytes))
    } else {
        text.to_lowercase().contains(query_lower)
    }
}

fn live_message_event_matches_query(
    from: &str,
    to: &[String],
    subject: &str,
    thread_id: &str,
    project: &str,
    body_md: &str,
    query_lower: &str,
) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    contains_case_insensitive(from, query_lower)
        || contains_case_insensitive(subject, query_lower)
        || contains_case_insensitive(thread_id, query_lower)
        || contains_case_insensitive(project, query_lower)
        || to
            .iter()
            .any(|recipient| contains_case_insensitive(recipient, query_lower))
        || contains_case_insensitive(body_md, query_lower)
}

#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
#[allow(clippy::too_many_arguments)]
fn render_detail_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    entry: Option<&MessageEntry>,
    scroll: usize,
    focused: bool,
    cache: &RefCell<Option<MessageDetailRenderCache>>,
    detail_view_mode: DetailViewMode,
    json_rows: Option<&[crate::tui_markdown::JsonTreeRow]>,
    json_cursor: usize,
) {
    let detail_title = entry.map_or_else(
        || "Detail".to_string(),
        |msg| {
            let importance = match msg.importance.as_str() {
                "urgent" => "!!",
                "high" => "!",
                _ => "\u{00b7}",
            };
            if detail_view_mode == DetailViewMode::JsonTree {
                format!("Detail {importance} | JSON Tree")
            } else {
                let viewport = usize::from(area.height.saturating_sub(2)).max(1);
                let width = if area.width == 0 { 80 } else { area.width };
                let total = cache
                    .borrow()
                    .as_ref()
                    .filter(|c| c.message_id == msg.id && c.width == width)
                    .map_or_else(
                        || estimate_message_detail_lines(msg, width),
                        |c| c.estimated_lines,
                    );
                let max_scroll = total.saturating_sub(viewport);
                let clamped = scroll.min(max_scroll);
                format!("Detail {importance} [{clamped}/{max_scroll}]")
            }
        },
    );
    let tp = crate::tui_theme::TuiThemePalette::current();
    let accent = entry.map_or(tp.panel_border_focused, |msg| {
        match msg.importance.as_str() {
            "urgent" => tp.severity_error,
            "high" => tp.severity_warn,
            _ => tp.status_accent,
        }
    });
    let border_color = if focused {
        crate::tui_theme::lerp_color(
            crate::tui_theme::focus_border_color(&tp, true),
            accent,
            0.55,
        )
    } else {
        crate::tui_theme::lerp_color(tp.panel_border_dim, accent, 0.30)
    };
    let block = Block::default()
        .title(&detail_title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(
            Style::default()
                .fg(tp.text_primary)
                .bg(crate::tui_theme::lerp_color(tp.panel_bg, accent, 0.08)),
        );
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (content_inner, scrollbar_area) = if inner.width > 6 {
        (
            Rect::new(
                inner.x,
                inner.y,
                inner.width.saturating_sub(1),
                inner.height,
            ),
            Some(Rect::new(
                inner.x + inner.width.saturating_sub(1),
                inner.y,
                1,
                inner.height,
            )),
        )
    } else {
        (inner, None)
    };
    let content_inner = if content_inner.width > 2 {
        Rect::new(
            content_inner.x.saturating_add(1),
            content_inner.y,
            content_inner.width.saturating_sub(2),
            content_inner.height,
        )
    } else {
        content_inner
    };

    let Some(msg) = entry else {
        let p = Paragraph::new("Select a message to view details.");
        p.render(content_inner, frame);
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
    let show_json_tree = detail_view_mode == DetailViewMode::JsonTree && json_rows.is_some();
    lines.push(if show_json_tree {
        "--- JSON Tree ---".to_string()
    } else {
        "--- Body ---".to_string()
    });

    // Combine header and body into one Text for unified scrolling
    let mut combined_lines: Vec<Line<'static>> = Vec::new();
    for line in lines {
        combined_lines.push(Line::raw(line));
    }

    let total_estimated = if show_json_tree {
        let tp = crate::tui_theme::TuiThemePalette::current();
        if let Some(rows) = json_rows {
            let selected_style = Style::default().fg(tp.selection_indicator).bold();
            let marker_style = Style::default().fg(tp.text_disabled);
            for (idx, row) in rows.iter().enumerate() {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let selected = idx == json_cursor;
                spans.push(Span::styled(
                    if selected {
                        "▸ ".to_string()
                    } else {
                        "  ".to_string()
                    },
                    if selected {
                        selected_style
                    } else {
                        marker_style
                    },
                ));
                spans.extend(row.line.spans().iter().cloned());
                combined_lines.push(Line::from_spans(spans));
            }
        }
        estimate_wrapped_line_count(&combined_lines, usize::from(content_inner.width))
    } else {
        let (body_text, est_lines) = {
            let width = content_inner.width;
            let body_hash = stable_hash(msg.body_md.as_bytes());
            let theme_key = crate::tui_theme::current_theme_env_value();
            let mut cached = cache.borrow_mut();

            #[allow(clippy::option_if_let_else)]
            if let Some(hit) = cached.as_ref().filter(|c| {
                c.message_id == msg.id
                    && c.width == width
                    && c.body_hash == body_hash
                    && c.theme_key == theme_key
            }) {
                (hit.rendered.clone(), hit.estimated_lines)
            } else {
                let mut body_md = msg.body_md.clone();
                let image_block = build_inline_image_block(&msg.body_md, width);
                if !image_block.is_empty() {
                    body_md.push_str("\n\n");
                    body_md.push_str(&image_block);
                }
                let md_theme = crate::tui_theme::markdown_theme();
                let rendered = crate::tui_markdown::render_message_body(&body_md, &md_theme)
                    .map_or_else(Text::default, |text| {
                        wrap_markdown_for_message_panel(&text, width)
                    });
                let estimated_lines = estimate_message_detail_lines(msg, width);
                *cached = Some(MessageDetailRenderCache {
                    message_id: msg.id,
                    width,
                    body_hash,
                    theme_key,
                    rendered: rendered.clone(),
                    estimated_lines,
                });
                (rendered, estimated_lines)
            }
        };

        for line in body_text.lines() {
            combined_lines.push(line.clone());
        }

        est_lines
    };

    let combined_text = Text::from_lines(combined_lines);

    // Apply scroll using Paragraph's internal wrapping support
    let visible_height = usize::from(content_inner.height);
    let max_scroll = total_estimated.saturating_sub(visible_height);
    let clamped_scroll = scroll.min(max_scroll);

    Paragraph::new(combined_text)
        .wrap(ftui::text::WrapMode::None)
        .scroll((clamped_scroll as u16, 0))
        .render(content_inner, frame);

    if let Some(bar_area) = scrollbar_area {
        render_vertical_scrollbar(
            frame,
            bar_area,
            clamped_scroll,
            visible_height,
            total_estimated,
            focused,
        );
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    scroll: usize,
    visible: usize,
    total: usize,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let track_style = crate::tui_theme::text_disabled(&tp);
    let thumb_style = Style::default()
        .fg(if focused {
            tp.selection_indicator
        } else {
            tp.status_accent
        })
        .bold();
    let rows = area.height as usize;
    let mut lines = Vec::with_capacity(rows);
    if total <= visible || rows == 0 {
        lines.extend((0..rows).map(|_| Line::styled("\u{2502}", track_style)));
    } else {
        let thumb_len = ((visible as f32 / total as f32) * rows as f32)
            .ceil()
            .max(1.0) as usize;
        let max_start = rows.saturating_sub(thumb_len);
        let denom = total.saturating_sub(visible).max(1) as f32;
        let thumb_start = ((scroll as f32 / denom) * max_start as f32).round() as usize;
        for row in 0..rows {
            if row >= thumb_start && row < thumb_start + thumb_len {
                lines.push(Line::styled("\u{2588}", thumb_style));
            } else {
                lines.push(Line::styled("\u{2502}", track_style));
            }
        }
    }
    Paragraph::new(Text::from_lines(lines)).render(area, frame);
}

fn render_compose_label(
    frame: &mut Frame<'_>,
    inner: Rect,
    cursor_y: &mut u16,
    bottom: u16,
    label: &str,
    focused: bool,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    if *cursor_y >= bottom {
        return;
    }
    let style = if focused {
        Style::default().fg(tp.selection_indicator).bold()
    } else {
        crate::tui_theme::text_meta(tp)
    };
    Paragraph::new(label.to_string())
        .style(style)
        .render(Rect::new(inner.x, *cursor_y, inner.width, 1), frame);
    *cursor_y = (*cursor_y).saturating_add(1);
}

fn render_compose_error_line(
    frame: &mut Frame<'_>,
    inner: Rect,
    cursor_y: &mut u16,
    bottom: u16,
    error: Option<&str>,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    if let Some(err) = error
        && *cursor_y < bottom
    {
        Paragraph::new(truncate_str(err, inner.width as usize))
            .style(crate::tui_theme::text_warning(tp))
            .render(Rect::new(inner.x, *cursor_y, inner.width, 1), frame);
        *cursor_y = (*cursor_y).saturating_add(1);
    }
}

#[must_use]
fn preset_modal_rect(area: Rect, width: u16, height: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let width = width.clamp(24, area.width.saturating_sub(2));
    let height = height.clamp(6, area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn render_save_preset_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    name: &str,
    description: &str,
    active_field: SavePresetField,
) {
    if area.width < 36 || area.height < 8 {
        return;
    }
    let overlay = preset_modal_rect(area, 64, 9);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Save Message Preset")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay));
    let inner = block.inner(overlay);
    block.render(overlay, frame);
    if inner.height == 0 {
        return;
    }
    let name_marker = if active_field == SavePresetField::Name {
        ">"
    } else {
        " "
    };
    let desc_marker = if active_field == SavePresetField::Description {
        ">"
    } else {
        " "
    };
    let description = if description.is_empty() {
        "<optional>".to_string()
    } else {
        description.to_string()
    };
    let lines = vec![
        Line::from(Span::styled(
            "Enter save · Tab switch field · Esc cancel",
            crate::tui_theme::text_meta(&tp),
        )),
        Line::from(Span::raw(format!("{name_marker} Name: {name}"))),
        Line::from(Span::raw(format!(
            "{desc_marker} Description: {description}"
        ))),
    ];
    Paragraph::new(Text::from_lines(lines)).render(inner, frame);
}

fn render_load_preset_dialog(frame: &mut Frame<'_>, area: Rect, names: &[String], cursor: usize) {
    if area.width < 36 || area.height < 8 {
        return;
    }
    let overlay = preset_modal_rect(area, 64, 12);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Load Message Preset")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay));
    let inner = block.inner(overlay);
    block.render(overlay, frame);
    if inner.height == 0 {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        "Enter apply · Del delete · j/k move · Esc cancel",
        crate::tui_theme::text_meta(&tp),
    ))];
    if names.is_empty() {
        lines.push(Line::from(Span::styled(
            "No saved presets for Messages.",
            crate::tui_theme::text_warning(&tp),
        )));
    } else {
        let visible_rows = usize::from(inner.height.saturating_sub(2)).max(1);
        let start = cursor.saturating_sub(visible_rows.saturating_sub(1));
        let end = (start + visible_rows).min(names.len());
        for (idx, name) in names.iter().enumerate().take(end).skip(start) {
            let marker = if idx == cursor {
                crate::tui_theme::SELECTION_PREFIX
            } else {
                crate::tui_theme::SELECTION_PREFIX_EMPTY
            };
            lines.push(Line::from(Span::raw(format!("{marker}{name}"))));
        }
    }
    Paragraph::new(Text::from_lines(lines)).render(inner, frame);
}

fn compose_modal_rect(area: Rect) -> Rect {
    if area.width < 40 || area.height < 16 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let modal_width = ((u32::from(area.width) * 88) / 100).clamp(62, 116) as u16;
    let modal_height = ((u32::from(area.height) * 88) / 100).clamp(22, 36) as u16;
    let width = modal_width.min(area.width.saturating_sub(2));
    let height = modal_height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

#[allow(clippy::too_many_lines)]
fn render_compose_modal(frame: &mut Frame<'_>, area: Rect, form: &ComposeFormState) {
    if area.width < 40 || area.height < 16 {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    Paragraph::new("")
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay))
        .render(area, frame);

    let modal = compose_modal_rect(area);

    let modal_title = format!("Compose Message · project {}", form.project_slug);
    let block = Block::default()
        .title(&modal_title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.selection_indicator));
    let inner = block.inner(modal);
    block.render(modal, frame);
    if inner.height < 8 || inner.width < 16 {
        return;
    }

    let mut cursor_y = inner.y;
    let bottom = inner.y + inner.height;

    render_compose_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        "To*",
        matches!(form.focus, ComposeField::To),
        &tp,
    );
    if cursor_y < bottom {
        form.to_input
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    if matches!(form.focus, ComposeField::To) && !form.suggestions.is_empty() && cursor_y < bottom {
        let suggestions: Vec<String> = form
            .suggestions
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                if idx == form.suggestion_cursor {
                    format!("[{item}]")
                } else {
                    item.clone()
                }
            })
            .collect();
        let line = format!("Suggestions: {}", suggestions.join(" · "));
        Paragraph::new(truncate_str(&line, inner.width as usize))
            .style(crate::tui_theme::text_hint(&tp))
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    render_compose_error_line(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.to.as_deref(),
        &tp,
    );

    render_compose_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        "CC",
        matches!(form.focus, ComposeField::Cc),
        &tp,
    );
    if cursor_y < bottom {
        form.cc_input
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    if matches!(form.focus, ComposeField::Cc) && !form.suggestions.is_empty() && cursor_y < bottom {
        let suggestions: Vec<String> = form
            .suggestions
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                if idx == form.suggestion_cursor {
                    format!("[{item}]")
                } else {
                    item.clone()
                }
            })
            .collect();
        let line = format!("Suggestions: {}", suggestions.join(" · "));
        Paragraph::new(truncate_str(&line, inner.width as usize))
            .style(crate::tui_theme::text_hint(&tp))
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    render_compose_error_line(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.cc.as_deref(),
        &tp,
    );

    render_compose_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        "Subject*",
        matches!(form.focus, ComposeField::Subject),
        &tp,
    );
    if cursor_y < bottom {
        form.subject_input
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    render_compose_error_line(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.subject.as_deref(),
        &tp,
    );

    render_compose_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        "Thread ID",
        matches!(form.focus, ComposeField::ThreadId),
        &tp,
    );
    if cursor_y < bottom {
        form.thread_id_input
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }
    render_compose_error_line(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.thread_id.as_deref(),
        &tp,
    );

    if cursor_y < bottom {
        let importance = format!("Importance: {}", form.importance().to_ascii_uppercase());
        let ack = if form.ack_required {
            "Ack Required: [x]"
        } else {
            "Ack Required: [ ]"
        };
        let line = format!("{importance}  ·  {ack}");
        let style = if matches!(
            form.focus,
            ComposeField::Importance | ComposeField::AckRequired
        ) {
            Style::default().fg(tp.selection_indicator).bold()
        } else {
            Style::default().fg(tp.text_secondary)
        };
        Paragraph::new(truncate_str(&line, inner.width as usize))
            .style(style)
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }

    let body_label =
        format!("Body* (Markdown, min {COMPOSE_BODY_MIN_ROWS} rows on normal terminals)");
    render_compose_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        &body_label,
        matches!(form.focus, ComposeField::Body),
        &tp,
    );
    let footer_rows: u16 = 2;
    let max_body_rows = bottom.saturating_sub(cursor_y).saturating_sub(footer_rows);
    if max_body_rows > 0 {
        ftui_widgets::Widget::render(
            &form.body_input,
            Rect::new(inner.x, cursor_y, inner.width, max_body_rows),
            frame,
        );
        cursor_y = cursor_y.saturating_add(max_body_rows);
    }
    render_compose_error_line(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.body.as_deref(),
        &tp,
    );

    if let Some(error) = &form.errors.general
        && cursor_y < bottom
    {
        Paragraph::new(truncate_str(error, inner.width as usize))
            .style(crate::tui_theme::text_warning(&tp))
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    }

    let footer_y = bottom.saturating_sub(1);
    let footer = "Tab/Shift+Tab fields • ↑/↓ suggestions • F5 or Ctrl+Enter submit • Esc cancel";
    Paragraph::new(truncate_str(footer, inner.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, footer_y, inner.width, 1), frame);
}

#[must_use]
fn quick_reply_modal_rect(area: Rect) -> Rect {
    if area.width < 40 || area.height < 14 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let modal_width = ((u32::from(area.width) * 84) / 100).clamp(58, 108) as u16;
    let modal_height = ((u32::from(area.height) * 82) / 100).clamp(18, 34) as u16;
    let width = modal_width.min(area.width.saturating_sub(2));
    let height = modal_height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

#[allow(clippy::too_many_lines)]
fn render_quick_reply_modal(frame: &mut Frame<'_>, area: Rect, form: &QuickReplyFormState) {
    if area.width < 40 || area.height < 14 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    Paragraph::new("")
        .style(Style::default().fg(tp.text_primary).bg(tp.bg_overlay))
        .render(area, frame);

    let modal = quick_reply_modal_rect(area);
    let modal_title = format!("Quick Reply · message #{}", form.context.message_id);
    let block = Block::default()
        .title(&modal_title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.selection_indicator));
    let inner = block.inner(modal);
    block.render(modal, frame);
    if inner.height < 8 || inner.width < 16 {
        return;
    }

    let mut cursor_y = inner.y;
    let bottom = inner.y + inner.height;

    let to_line = format!("To: {}", form.context.to_agent);
    Paragraph::new(truncate_str(&to_line, inner.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    cursor_y = cursor_y.saturating_add(1);

    let thread_label = form.context.thread_id.as_deref().unwrap_or("(derived)");
    let thread_line = format!("Thread: {thread_label}");
    Paragraph::new(truncate_str(&thread_line, inner.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    cursor_y = cursor_y.saturating_add(1);

    let subject_line = format!("Subject: {}", form.context.subject);
    Paragraph::new(truncate_str(&subject_line, inner.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    cursor_y = cursor_y.saturating_add(1);

    let ack_line = if form.ack_required {
        "Ack Required: [x] (informational for reply_message)"
    } else {
        "Ack Required: [ ] (informational for reply_message)"
    };
    let ack_style = if matches!(form.focus, QuickReplyField::AckRequired) {
        Style::default().fg(tp.selection_indicator).bold()
    } else {
        Style::default().fg(tp.text_secondary)
    };
    Paragraph::new(truncate_str(ack_line, inner.width as usize))
        .style(ack_style)
        .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    cursor_y = cursor_y.saturating_add(1);

    let body_label =
        format!("Reply Body* (Markdown, min {QUICK_REPLY_BODY_MIN_ROWS} rows on normal terminals)");
    render_compose_label(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        &body_label,
        matches!(form.focus, QuickReplyField::Body),
        &tp,
    );

    let footer_rows: u16 = 2;
    let context_rows: u16 = 6;
    let body_rows = bottom
        .saturating_sub(cursor_y)
        .saturating_sub(footer_rows.saturating_add(context_rows))
        .max(3);
    if body_rows > 0 {
        ftui_widgets::Widget::render(
            &form.body_input,
            Rect::new(inner.x, cursor_y, inner.width, body_rows),
            frame,
        );
        cursor_y = cursor_y.saturating_add(body_rows);
    }

    render_compose_error_line(
        frame,
        inner,
        &mut cursor_y,
        bottom,
        form.errors.body.as_deref(),
        &tp,
    );

    if cursor_y < bottom {
        let from_line = format!(
            "Original from {} at {}",
            form.context.original_from_agent, form.context.original_timestamp_iso
        );
        Paragraph::new(truncate_str(&from_line, inner.width as usize))
            .style(crate::tui_theme::text_meta(&tp))
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
        cursor_y = cursor_y.saturating_add(1);
    }

    let preview_rows = bottom.saturating_sub(cursor_y).saturating_sub(1);
    if preview_rows > 0 {
        if form.context.original_body_md.trim().is_empty() {
            Paragraph::new("(empty body)")
                .style(crate::tui_theme::text_hint(&tp))
                .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
            cursor_y = cursor_y.saturating_add(1);
        } else {
            let body_hash = stable_hash(form.context.original_body_md.as_bytes());
            let width = inner.width;
            let theme_key = crate::tui_theme::current_theme_env_value();
            let rendered = QUICK_REPLY_PREVIEW_CACHE.with(|cache_cell| {
                let mut cache = cache_cell.borrow_mut();
                let miss = cache.as_ref().is_none_or(|cached| {
                    cached.body_hash != body_hash
                        || cached.width != width
                        || cached.theme_key != theme_key
                });
                if miss {
                    let md_theme = crate::tui_theme::markdown_theme();
                    let rendered = crate::tui_markdown::render_message_body(
                        &form.context.original_body_md,
                        &md_theme,
                    )
                    .map_or_else(Text::default, |text| {
                        wrap_markdown_for_message_panel(&text, width)
                    });
                    *cache = Some(QuickReplyPreviewCache {
                        body_hash,
                        width,
                        theme_key,
                        rendered,
                    });
                }
                cache
                    .as_ref()
                    .map_or_else(Text::default, |cached| cached.rendered.clone())
            });
            let preview_lines: Vec<Line<'static>> = rendered
                .lines()
                .iter()
                .filter(|line| !line.to_plain_text().trim().is_empty())
                .take(usize::from(preview_rows))
                .cloned()
                .collect();
            if preview_lines.is_empty() {
                Paragraph::new("(empty body)")
                    .style(crate::tui_theme::text_hint(&tp))
                    .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
                cursor_y = cursor_y.saturating_add(1);
            } else {
                let height = u16::try_from(preview_lines.len()).unwrap_or(preview_rows);
                Paragraph::new(Text::from_lines(preview_lines))
                    .wrap(ftui::text::WrapMode::None)
                    .render(Rect::new(inner.x, cursor_y, inner.width, height), frame);
                cursor_y = cursor_y.saturating_add(height);
            }
        }
    }

    if let Some(error) = &form.errors.general
        && cursor_y < bottom.saturating_sub(1)
    {
        Paragraph::new(truncate_str(error, inner.width as usize))
            .style(crate::tui_theme::text_warning(&tp))
            .render(Rect::new(inner.x, cursor_y, inner.width, 1), frame);
    }

    let footer_y = bottom.saturating_sub(1);
    let footer = "Tab/Shift+Tab field • Space/Enter toggles Ack • Ctrl+Enter submits • Esc cancel";
    Paragraph::new(truncate_str(footer, inner.width as usize))
        .style(crate::tui_theme::text_hint(&tp))
        .render(Rect::new(inner.x, footer_y, inner.width, 1), frame);
}

const fn spinner_glyph(phase: u8) -> &'static str {
    match phase % 8 {
        0 | 4 => "\u{25d0}",
        1 | 5 => "\u{25d3}",
        2 | 6 => "\u{25d1}",
        _ => "\u{25d2}",
    }
}

fn pulse_meter(phase: u8, width: usize) -> String {
    const BARS: [char; 8] = [
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    let w = width.max(4);
    let mut out = String::with_capacity(w);
    for idx in 0..w {
        let pos = (usize::from(phase) + idx) % BARS.len();
        out.push(BARS[pos]);
    }
    out
}

fn runtime_telemetry_line(state: &TuiSharedState, ui_phase: u8) -> String {
    let counters = state.request_counters();
    let err = counters.status_4xx.saturating_add(counters.status_5xx);
    let spark_raw = state.sparkline_snapshot();
    let spark = crate::tui_screens::dashboard::render_sparkline(&spark_raw, 12);
    let meter = pulse_meter(ui_phase, 6);
    let sparkline = if spark.is_empty() {
        "......".to_string()
    } else {
        spark
    };
    format!(
        "{meter} req:{} ok:{} err:{} avg:{}ms spark:{}",
        counters.total,
        counters.status_2xx,
        err,
        state.avg_latency_ms(),
        sparkline
    )
}

// ──────────────────────────────────────────────────────────────────────
// Utility helpers
// ──────────────────────────────────────────────────────────────────────

fn split_recipient_prefix(input: &str) -> (usize, String) {
    let start = input.rfind(',').map_or(0, |idx| idx + 1);
    let prefix = input[start..].trim_start().to_string();
    (start, prefix)
}

fn parse_recipient_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        if out.iter().all(|existing| existing != name) {
            out.push(name.to_string());
        }
    }
    out
}

fn prefixed_reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return "Re:".to_string();
    }
    if trimmed.len() >= 3 && trimmed.is_char_boundary(3) && trimmed[..3].eq_ignore_ascii_case("re:")
    {
        trimmed.to_string()
    } else {
        format!("Re: {trimmed}")
    }
}

fn is_valid_compose_thread_id(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return false;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn validate_compose_form(
    form: &ComposeFormState,
) -> Result<ComposePayload, Box<ComposeValidationErrors>> {
    let mut errors = ComposeValidationErrors::default();
    let to = parse_recipient_list(form.to_input.value());
    let cc = parse_recipient_list(form.cc_input.value());
    let known_agents: std::collections::HashSet<&str> =
        form.available_agents.iter().map(String::as_str).collect();
    if to.is_empty() {
        errors.to = Some("At least one recipient is required.".to_string());
    } else if let Some(invalid) = to.iter().find(|name| !known_agents.contains(name.as_str())) {
        errors.to = Some(format!("Unknown recipient: {invalid}"));
    }
    if let Some(invalid) = cc.iter().find(|name| !known_agents.contains(name.as_str())) {
        errors.cc = Some(format!("Unknown CC recipient: {invalid}"));
    }

    let subject = form.subject_input.value().trim().to_string();
    if subject.is_empty() {
        errors.subject = Some("Subject is required.".to_string());
    } else if subject.chars().count() > 200 {
        errors.subject = Some("Subject must be 1-200 characters.".to_string());
    }

    let body_md = form.body_input.text();
    if body_md.trim().is_empty() {
        errors.body = Some("Body is required.".to_string());
    }

    let thread_raw = form.thread_id_input.value().trim();
    let thread_id = if thread_raw.is_empty() {
        None
    } else if is_valid_compose_thread_id(thread_raw) {
        Some(thread_raw.to_string())
    } else {
        errors.thread_id = Some(
            "Thread ID must start with alphanumeric and contain only letters, digits, '.', '_' or '-'.".to_string(),
        );
        None
    };

    if errors.has_any() {
        return Err(Box::new(errors));
    }

    Ok(ComposePayload {
        project_slug: form.project_slug.clone(),
        to,
        cc,
        subject,
        thread_id,
        body_md,
        importance: form.importance().to_string(),
        ack_required: form.ack_required,
    })
}

fn validate_quick_reply_form(
    form: &QuickReplyFormState,
) -> Result<String, Box<QuickReplyValidationErrors>> {
    let mut errors = QuickReplyValidationErrors::default();
    let body_md = form.body_input.text();
    if body_md.trim().is_empty() {
        errors.body = Some("Reply body is required.".to_string());
    }
    if errors.has_any() {
        return Err(Box::new(errors));
    }
    Ok(body_md)
}

fn apply_compose_field_key(form: &mut ComposeFormState, event: &Event, code: KeyCode) {
    match form.focus {
        ComposeField::Importance => match code {
            KeyCode::Left | KeyCode::Char('h') => {
                if form.importance_idx == 0 {
                    form.importance_idx = COMPOSE_IMPORTANCE_LEVELS.len() - 1;
                } else {
                    form.importance_idx -= 1;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                form.importance_idx = (form.importance_idx + 1) % COMPOSE_IMPORTANCE_LEVELS.len();
            }
            KeyCode::Enter => form.cycle_focus_next(),
            _ => {}
        },
        ComposeField::AckRequired => match code {
            KeyCode::Char(' ') | KeyCode::Enter => {
                form.ack_required = !form.ack_required;
            }
            _ => {}
        },
        ComposeField::Body => {
            let _ = form.body_input.handle_event(event);
            form.errors.body = None;
            form.errors.general = None;
        }
        ComposeField::To | ComposeField::Cc => match code {
            KeyCode::Down => form.move_suggestion(1),
            KeyCode::Up => form.move_suggestion(-1),
            KeyCode::Enter => {
                if !form.apply_suggestion() {
                    form.cycle_focus_next();
                }
            }
            _ => {
                let changed = form.recipient_input_mut().is_some_and(|input| {
                    let before = input.value().to_string();
                    let _ = input.handle_event(event);
                    input.value() != before
                });
                if changed {
                    if matches!(form.focus, ComposeField::To) {
                        form.errors.to = None;
                    } else {
                        form.errors.cc = None;
                    }
                    form.errors.general = None;
                    form.refresh_suggestions();
                }
            }
        },
        ComposeField::Subject => {
            let before = form.subject_input.value().to_string();
            let _ = form.subject_input.handle_event(event);
            if form.subject_input.value() != before {
                form.errors.subject = None;
                form.errors.general = None;
            }
        }
        ComposeField::ThreadId => {
            let before = form.thread_id_input.value().to_string();
            let _ = form.thread_id_input.handle_event(event);
            if form.thread_id_input.value() != before {
                form.errors.thread_id = None;
                form.errors.general = None;
            }
        }
    }
}

const fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn unix_epoch_micros_now() -> Option<i64> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_micros();
    i64::try_from(micros).ok()
}

#[allow(clippy::cast_precision_loss)]
fn shimmer_progress_for_timestamp(now_micros: i64, timestamp_micros: i64) -> Option<f64> {
    if timestamp_micros <= 0 {
        return None;
    }
    let age = now_micros.saturating_sub(timestamp_micros);
    if !(0..=SHIMMER_WINDOW_MICROS).contains(&age) {
        return None;
    }
    Some((age as f64 / SHIMMER_WINDOW_MICROS as f64).clamp(0.0, 1.0))
}

fn compute_shimmer_progresses(results: &[MessageEntry], effects_enabled: bool) -> Vec<Option<f64>> {
    let mut progresses = vec![None; results.len()];
    if !effects_enabled || results.is_empty() {
        return progresses;
    }
    let Some(now_micros) = unix_epoch_micros_now() else {
        return progresses;
    };
    let mut shimmer_count = 0usize;
    for (idx, entry) in results.iter().enumerate() {
        if shimmer_count >= SHIMMER_MAX_ROWS {
            break;
        }
        if let Some(progress) = shimmer_progress_for_timestamp(now_micros, entry.timestamp_micros) {
            progresses[idx] = Some(progress);
            shimmer_count += 1;
        }
    }
    progresses
}

fn char_index_to_byte_offset(s: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn subject_shimmer_window(
    subject: &str,
    progress: f64,
    width_chars: usize,
) -> Option<(usize, usize)> {
    let len_chars = subject.chars().count();
    if len_chars == 0 {
        return None;
    }
    let clamped = progress.clamp(0.0, 1.0);
    let center = ((len_chars.saturating_sub(1)) as f64 * clamped).round() as usize;
    let width = width_chars.max(1).min(len_chars);
    let half = width / 2;
    let start = center.saturating_sub(half);
    let end = (start + width).min(len_chars);
    Some((start, end))
}

/// Compute the viewport [start, end) to keep cursor visible.
/// (Retained for test coverage; `VirtualizedList` handles this internally.)
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

/// Truncate a string to at most `max_width` columns, adding "..." if truncated.
fn truncate_str(s: &str, max_width: usize) -> String {
    let s_width = ftui::text::display_width(s);
    if s_width <= max_width {
        return s.to_string();
    }
    let mw_u16 = u16::try_from(max_width).unwrap_or(u16::MAX);
    if max_width <= 3 {
        return crate::tui_widgets::truncate_width(s, mw_u16).into_owned();
    }
    let target = mw_u16.saturating_sub(3);
    let mut result = crate::tui_widgets::truncate_width(s, target).into_owned();
    result.push_str("...");
    result
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    fn test_message_entry(id: i64, thread_id: &str, subject: &str) -> MessageEntry {
        MessageEntry {
            id,
            subject: subject.to_string(),
            from_agent: "GoldFox".to_string(),
            to_agents: "SilverWolf".to_string(),
            project_slug: "proj".to_string(),
            thread_id: thread_id.to_string(),
            timestamp_iso: "2026-02-06T12:00:00".to_string(),
            timestamp_micros: 0,
            body_md: "Body".to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        }
    }

    fn mouse_event(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(ftui::MouseEvent {
            kind,
            x,
            y,
            modifiers: ftui::Modifiers::empty(),
        })
    }

    fn write_archive_ahead_fixture(base: &Path) -> (PathBuf, PathBuf) {
        let storage_root = base.join("storage");
        let db_path = base.join("stale-messages.sqlite3");
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
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
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
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);
        (storage_root, db_path)
    }

    #[test]
    fn messages_screen_uses_archive_snapshot_when_live_db_is_stale() {
        let dir = tempdir().expect("tempdir");
        let (storage_root, db_path) = write_archive_ahead_fixture(dir.path());
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root,
            ..Default::default()
        });
        let mut screen = MessageBrowserScreen::new();

        screen.ensure_db_conn(&state);

        assert!(screen.db_conn.is_some(), "screen should bind a readable db");
        assert!(
            screen._db_snapshot_dir.is_some(),
            "screen should hold the archive snapshot directory when the live db lags"
        );
        assert_eq!(
            screen.latest_project_slug().as_deref(),
            Some("ahead-project")
        );
    }

    #[test]
    fn ensure_compose_sender_uses_live_mailbox_when_screen_reads_archive_snapshot() {
        let dir = tempdir().expect("tempdir");
        let (storage_root, db_path) = write_archive_ahead_fixture(dir.path());
        let live_conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        live_conn
            .execute_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
                &[
                    Value::BigInt(1),
                    Value::Text("ahead-project".to_string()),
                    Value::Text("/ahead-project".to_string()),
                    Value::BigInt(0),
                ],
            )
            .expect("insert live project");
        drop(live_conn);

        let state = TuiSharedState::new(&mcp_agent_mail_core::Config {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root: storage_root.clone(),
            ..Default::default()
        });
        let mut screen = MessageBrowserScreen::new();
        screen.ensure_db_conn(&state);
        assert!(
            screen._db_snapshot_dir.is_some(),
            "screen should be in archive-snapshot mode for this regression"
        );

        screen
            .ensure_compose_sender("ahead-project", Some(&state))
            .expect("ensure compose sender");

        let live_conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("reopen db");
        let sender_count = live_conn
            .query_sync(
                "SELECT COUNT(*) AS c FROM agents WHERE project_id = ? AND name = ?",
                &[
                    Value::BigInt(1),
                    Value::Text(COMPOSE_SENDER_NAME.to_string()),
                ],
            )
            .expect("count compose sender")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .unwrap_or_default();
        assert_eq!(
            sender_count, 1,
            "compose sender should be created in the live configured db, not only the archive snapshot"
        );

        let profile_path = storage_root
            .join("projects")
            .join("ahead-project")
            .join("agents")
            .join(COMPOSE_SENDER_NAME)
            .join("profile.json");
        assert!(
            profile_path.exists(),
            "compose sender profile should be mirrored into the archive"
        );
    }

    #[test]
    fn ensure_compose_sender_accepts_placeholder_project_identifier() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("compose-placeholder.db");
        let storage_root = dir.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(77),
                Value::Text("proj".to_string()),
                Value::Text("/proj".to_string()),
                Value::BigInt(0),
            ],
        )
        .expect("insert project");
        conn.execute_raw("DELETE FROM projects WHERE id = 77")
            .expect("delete project row");
        drop(conn);

        let state = TuiSharedState::new(&mcp_agent_mail_core::Config {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root,
            ..Default::default()
        });
        let screen = MessageBrowserScreen::new();

        screen
            .ensure_compose_sender("[unknown-project-77]", Some(&state))
            .expect("ensure compose sender");

        let live_conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("reopen db");
        let sender_count = live_conn
            .query_sync(
                "SELECT COUNT(*) AS c FROM agents WHERE project_id = ? AND name = ?",
                &[
                    Value::BigInt(77),
                    Value::Text(COMPOSE_SENDER_NAME.to_string()),
                ],
            )
            .expect("count compose sender")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .unwrap_or_default();
        assert_eq!(sender_count, 1, "compose sender should be created");
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
        let screen = MessageBrowserScreen::new();
        assert_eq!(screen.cursor, 0);
        assert_eq!(screen.detail_scroll.get(), 0);
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
        // Set a non-zero detail area so scroll_detail_by doesn't clamp to 0.
        screen.last_detail_area.set(Rect::new(0, 0, 80, 5));
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
        assert_eq!(screen.detail_scroll.get(), 1);

        let k_upper = Event::Key(ftui::KeyEvent::new(KeyCode::Char('K')));
        screen.update(&k_upper, &state);
        assert_eq!(screen.detail_scroll.get(), 0);

        // Can't go below 0
        screen.update(&k_upper, &state);
        assert_eq!(screen.detail_scroll.get(), 0);
    }

    #[test]
    fn detail_json_tree_toggle_and_expand_collapse() {
        let mut screen = MessageBrowserScreen::new();
        screen.last_detail_area.set(Rect::new(0, 0, 80, 12));
        screen.results.push(MessageEntry {
            id: 7,
            subject: "JSON".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "GreenCastle".to_string(),
            project_slug: "proj".to_string(),
            thread_id: "br-2bbt".to_string(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: r#"{"a":{"b":1},"list":[1,2]}"#.to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('J'))), &state);
        assert_eq!(screen.detail_view_mode.get(), DetailViewMode::JsonTree);
        assert!(screen.json_tree_state.borrow().is_available());

        let expanded_rows = screen.json_tree_state.borrow().rows().len();
        assert!(expanded_rows > 1, "root should be expanded initially");

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Enter)), &state);
        let collapsed_rows = screen.json_tree_state.borrow().rows().len();
        assert_eq!(collapsed_rows, 1, "enter collapses selected root node");

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Enter)), &state);
        assert!(
            screen.json_tree_state.borrow().rows().len() > 1,
            "enter toggles root back to expanded"
        );

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('J'))), &state);
        assert_eq!(screen.detail_view_mode.get(), DetailViewMode::Markdown);
    }

    #[test]
    fn detail_max_scroll_uses_json_tree_rows_when_tree_mode_active() {
        let mut screen = MessageBrowserScreen::new();
        screen.last_detail_area.set(Rect::new(0, 0, 80, 12));
        let dense_json = format!(
            "{{{}}}",
            (0..20)
                .map(|i| format!("\"k{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        screen.results.push(MessageEntry {
            id: 1,
            subject: "Dense JSON".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "GreenCastle".to_string(),
            project_slug: "proj".to_string(),
            thread_id: String::new(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: dense_json,
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('J'))), &state);
        assert_eq!(screen.detail_view_mode.get(), DetailViewMode::JsonTree);
        assert!(
            screen.detail_max_scroll() > 0,
            "json tree mode should account for expanded row count in max-scroll calculation"
        );
    }

    #[test]
    fn copyable_content_returns_json_node_payload_in_tree_mode() {
        let mut screen = MessageBrowserScreen::new();
        screen.last_detail_area.set(Rect::new(0, 0, 80, 12));
        screen.results.push(MessageEntry {
            id: 11,
            subject: "Copy JSON".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "GreenCastle".to_string(),
            project_slug: "proj".to_string(),
            thread_id: "br-2bbt.7".to_string(),
            timestamp_iso: "2026-03-04T00:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: r#"{"outer":{"inner":42}}"#.to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('J'))), &state);
        let payload = screen.copyable_content().expect("copy payload");
        assert!(
            payload.contains("path: $"),
            "payload should include JSON path"
        );
        assert!(payload.contains("value:"), "payload should include value");

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('j'))), &state);
        let child_payload = screen.copyable_content().expect("child payload");
        assert!(
            child_payload.contains("path: $/outer"),
            "child payload should include selected node path"
        );
    }

    #[test]
    fn contextual_actions_include_json_copy_entries_in_tree_mode() {
        let mut screen = MessageBrowserScreen::new();
        screen.last_detail_area.set(Rect::new(0, 0, 80, 12));
        screen.results.push(MessageEntry {
            id: 12,
            subject: "JSON actions".to_string(),
            from_agent: "BlueLake".to_string(),
            to_agents: "GreenCastle".to_string(),
            project_slug: "proj".to_string(),
            thread_id: String::new(),
            timestamp_iso: "2026-03-04T00:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: r#"{"k":"v"}"#.to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('J'))), &state);

        let (actions, _, _) = screen
            .contextual_actions()
            .expect("contextual actions should exist");
        let labels: Vec<&str> = actions.iter().map(|entry| entry.label.as_str()).collect();
        assert!(labels.contains(&"Copy JSON path"));
        assert!(labels.contains(&"Copy JSON value"));
    }

    // ── consumes_text_input ─────────────────────────────────────────

    #[test]
    fn consumes_text_input_when_searching() {
        let mut screen = MessageBrowserScreen::new();
        assert!(!screen.consumes_text_input());
        screen.focus = Focus::SearchBar;
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn compose_modal_consumes_text_input() {
        let mut screen = MessageBrowserScreen::new();
        screen.compose_form = Some(ComposeFormState::new(
            "proj".to_string(),
            None,
            vec!["BlueLake".to_string()],
        ));
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn quick_reply_modal_consumes_text_input() {
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(1, "thread-1", "Need follow-up");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn compose_modal_click_outside_dismisses_modal() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.last_search_area.set(Rect::new(0, 0, 120, 5));
        screen.last_content_area.set(Rect::new(0, 5, 120, 35));
        screen.compose_form = Some(ComposeFormState::new(
            "proj".to_string(),
            None,
            vec!["BlueLake".to_string()],
        ));

        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: 0,
            y: 0,
            modifiers: ftui::Modifiers::empty(),
        });
        let _ = screen.update(&click, &state);
        assert!(screen.compose_form.is_none());
    }

    #[test]
    fn compose_modal_click_inside_keeps_modal_open() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.last_search_area.set(Rect::new(0, 0, 120, 5));
        screen.last_content_area.set(Rect::new(0, 5, 120, 35));
        screen.compose_form = Some(ComposeFormState::new(
            "proj".to_string(),
            None,
            vec!["BlueLake".to_string()],
        ));

        let modal = compose_modal_rect(Rect::new(0, 0, 120, 40));
        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: modal.x.saturating_add(2),
            y: modal.y.saturating_add(2),
            modifiers: ftui::Modifiers::empty(),
        });
        let _ = screen.update(&click, &state);
        assert!(screen.compose_form.is_some());
    }

    #[test]
    fn quick_reply_modal_click_outside_dismisses_modal() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.last_search_area.set(Rect::new(0, 0, 120, 5));
        screen.last_content_area.set(Rect::new(0, 5, 120, 35));
        let entry = test_message_entry(2, "thread-2", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: 0,
            y: 0,
            modifiers: ftui::Modifiers::empty(),
        });
        let _ = screen.update(&click, &state);
        assert!(screen.quick_reply_form.is_none());
    }

    #[test]
    fn quick_reply_modal_click_inside_keeps_modal_open() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.last_search_area.set(Rect::new(0, 0, 120, 5));
        screen.last_content_area.set(Rect::new(0, 5, 120, 35));
        let entry = test_message_entry(3, "thread-3", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let modal = quick_reply_modal_rect(Rect::new(0, 0, 120, 40));
        let click = Event::Mouse(ftui::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            x: modal.x.saturating_add(2),
            y: modal.y.saturating_add(2),
            modifiers: ftui::Modifiers::empty(),
        });
        let _ = screen.update(&click, &state);
        assert!(screen.quick_reply_form.is_some());
    }

    #[test]
    fn quick_reply_escape_closes_modal() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(4, "thread-4", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let cmd = screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Escape)), &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(screen.quick_reply_form.is_none());
    }

    #[test]
    fn quick_reply_tab_and_backtab_cycle_focus() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(5, "thread-5", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let tab = Event::Key(ftui::KeyEvent::new(KeyCode::Tab));
        let _ = screen.update(&tab, &state);
        let form = screen.quick_reply_form.as_ref().expect("quick reply form");
        assert!(matches!(form.focus, QuickReplyField::AckRequired));

        let backtab = Event::Key(ftui::KeyEvent::new(KeyCode::BackTab));
        let _ = screen.update(&backtab, &state);
        let form = screen.quick_reply_form.as_ref().expect("quick reply form");
        assert!(matches!(form.focus, QuickReplyField::Body));
    }

    #[test]
    fn quick_reply_ack_focus_toggles_with_space() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(6, "thread-6", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);
        if let Some(form) = screen.quick_reply_form.as_mut() {
            form.set_focus(QuickReplyField::AckRequired);
            assert!(!form.ack_required);
        }

        let space = Event::Key(ftui::KeyEvent::new(KeyCode::Char(' ')));
        let _ = screen.update(&space, &state);
        let form = screen.quick_reply_form.as_ref().expect("quick reply form");
        assert!(form.ack_required);
    }

    #[test]
    fn quick_reply_submit_with_empty_body_sets_validation_error() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(7, "thread-7", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let cmd = screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::F(5))), &state);
        assert!(matches!(cmd, Cmd::None));
        let form = screen.quick_reply_form.as_ref().expect("quick reply form");
        assert_eq!(form.errors.body.as_deref(), Some("Reply body is required."));
    }

    #[test]
    fn quick_reply_ctrl_enter_with_empty_body_sets_validation_error() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(8, "thread-8", "Reply target");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let cmd = screen.update(&ctrl_key(KeyCode::Enter), &state);
        assert!(matches!(cmd, Cmd::None));
        let form = screen.quick_reply_form.as_ref().expect("quick reply form");
        assert_eq!(form.errors.body.as_deref(), Some("Reply body is required."));
    }

    #[test]
    fn set_cursor_from_results_click_ignores_bottom_border_row() {
        let mut screen = MessageBrowserScreen::new();
        screen.results = (0..20)
            .map(|idx| test_message_entry(idx, "thread-a", "Subject"))
            .collect();
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));
        screen.cursor = 0;

        // Bottom border row in the bordered list area should be ignored.
        screen.set_cursor_from_results_click(13);
        assert_eq!(screen.cursor, 0);
    }

    #[test]
    fn result_index_at_y_ignores_bottom_border_row() {
        let mut screen = MessageBrowserScreen::new();
        screen.results = (0..20)
            .map(|idx| test_message_entry(idx, "thread-a", "Subject"))
            .collect();
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));

        assert_eq!(screen.result_index_at_y(13), None);
    }

    #[test]
    fn mouse_drag_promotes_after_hold_delay() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5);
        let _ = screen.update(&down, &state);
        assert!(matches!(screen.message_drag, MessageDragState::Pending(_)));
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        screen.tick(1, &state);
        assert!(matches!(screen.message_drag, MessageDragState::Active(_)));
        let snapshot = state.message_drag_snapshot().expect("drag snapshot");
        assert_eq!(snapshot.message_id, 10);
        assert_eq!(snapshot.source_thread_id, "thread-a");
    }

    #[test]
    fn mouse_drag_over_invalid_target_sets_warning_snapshot() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 70, 2);
        let _ = screen.update(&drag, &state);
        let snapshot = state.message_drag_snapshot().expect("drag snapshot");
        assert!(snapshot.invalid_hover);
        assert!(!snapshot.hovered_is_valid);
        assert!(snapshot.hovered_thread_id.is_none());
    }

    #[test]
    fn mouse_drop_on_invalid_target_is_noop() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 70, 2);
        let _ = screen.update(&drag, &state);
        let up = mouse_event(MouseEventKind::Up(MouseButton::Left), 70, 2);
        let cmd = screen.update(&up, &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(state.message_drag_snapshot().is_none());
        assert!(matches!(screen.message_drag, MessageDragState::Idle));
    }

    #[test]
    fn mouse_drop_on_different_thread_dispatches_rethread_action() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![
            test_message_entry(10, "thread-a", "Subject A"),
            test_message_entry(11, "thread-b", "Subject B"),
        ];
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 6);
        let _ = screen.update(&drag, &state);
        let up = mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 6);
        let cmd = screen.update(&up, &state);

        match cmd {
            Cmd::Msg(MailScreenMsg::ActionExecute(op, ctx)) => {
                assert_eq!(op, "rethread_message:10:thread-b");
                assert_eq!(ctx, "thread-a");
            }
            _ => panic!("expected ActionExecute command"),
        }
        assert!(state.message_drag_snapshot().is_none());
        assert!(matches!(screen.message_drag, MessageDragState::Idle));
    }

    #[test]
    fn mouse_drop_on_same_thread_is_noop() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        screen.last_results_area.set(Rect::new(0, 4, 40, 10));

        let down = mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5);
        let _ = screen.update(&down, &state);
        if let MessageDragState::Pending(pending) = &mut screen.message_drag {
            let hold_plus = MESSAGE_DRAG_HOLD_DELAY + Duration::from_millis(1);
            pending.started_at = Instant::now()
                .checked_sub(hold_plus)
                .unwrap_or_else(Instant::now);
        }

        let drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 5);
        let _ = screen.update(&drag, &state);
        let up = mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 5);
        let cmd = screen.update(&up, &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(state.message_drag_snapshot().is_none());
        assert!(matches!(screen.message_drag, MessageDragState::Idle));
    }

    #[test]
    fn ctrl_m_marks_selected_message_for_keyboard_move() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        screen.cursor = 0;

        let cmd = screen.update(&ctrl_key(KeyCode::Char('m')), &state);
        assert!(matches!(cmd, Cmd::None));
        let marker = state
            .keyboard_move_snapshot()
            .expect("keyboard move marker");
        assert_eq!(marker.message_id, 10);
        assert_eq!(marker.source_thread_id, "thread-a");
    }

    #[test]
    fn ctrl_m_replaces_existing_keyboard_move_marker() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![
            test_message_entry(10, "thread-a", "Subject A"),
            test_message_entry(11, "thread-b", "Subject B"),
        ];

        let _ = screen.update(&ctrl_key(KeyCode::Char('m')), &state);
        screen.cursor = 1;
        let _ = screen.update(&ctrl_key(KeyCode::Char('m')), &state);

        let marker = state
            .keyboard_move_snapshot()
            .expect("keyboard move marker");
        assert_eq!(marker.message_id, 11);
        assert_eq!(marker.source_thread_id, "thread-b");
    }

    #[test]
    fn ctrl_v_dispatches_marked_message_to_selected_thread() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![
            test_message_entry(10, "thread-a", "Subject A"),
            test_message_entry(11, "thread-b", "Subject B"),
        ];
        screen.cursor = 1;
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: 10,
            subject: "Subject A".to_string(),
            source_thread_id: "thread-a".to_string(),
            source_project_slug: "proj".to_string(),
        }));

        let cmd = screen.update(&ctrl_key(KeyCode::Char('v')), &state);
        match cmd {
            Cmd::Msg(MailScreenMsg::ActionExecute(op, ctx)) => {
                assert_eq!(op, "rethread_message:10:thread-b");
                assert_eq!(ctx, "thread-a");
            }
            _ => panic!("expected ActionExecute command"),
        }
        assert!(state.keyboard_move_snapshot().is_none());
    }

    #[test]
    fn ctrl_v_same_thread_is_noop_and_preserves_marker() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        screen.cursor = 0;
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: 10,
            subject: "Subject A".to_string(),
            source_thread_id: "thread-a".to_string(),
            source_project_slug: "proj".to_string(),
        }));

        let cmd = screen.update(&ctrl_key(KeyCode::Char('v')), &state);
        assert!(matches!(cmd, Cmd::None));
        let marker = state
            .keyboard_move_snapshot()
            .expect("keyboard move marker should remain");
        assert_eq!(marker.message_id, 10);
        assert_eq!(marker.source_thread_id, "thread-a");
    }

    #[test]
    fn escape_clears_keyboard_move_marker() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;
        screen.results = vec![test_message_entry(10, "thread-a", "Subject A")];
        state.set_keyboard_move_snapshot(Some(KeyboardMoveSnapshot {
            message_id: 10,
            subject: "Subject A".to_string(),
            source_thread_id: "thread-a".to_string(),
            source_project_slug: "proj".to_string(),
        }));

        let cmd = screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Escape)), &state);
        assert!(matches!(cmd, Cmd::None));
        assert!(state.keyboard_move_snapshot().is_none());
    }

    // ── JSON helpers ───────────────────────────────────────────────

    #[test]
    fn looks_like_json_detects_objects_and_arrays() {
        assert!(looks_like_json("{\"ok\":true}"));
        assert!(looks_like_json("   [1,2,3]"));
        assert!(!looks_like_json("# heading"));
        assert!(!looks_like_json("plain text payload"));
    }

    #[test]
    fn looks_like_json_rejects_fenced_json_code_block() {
        let fenced = "```json\n{\"ok\":true}\n```";
        assert!(!looks_like_json(fenced));
    }

    #[test]
    fn table_line_detection_matches_rendered_box_drawing_rows() {
        assert!(is_rendered_markdown_table_line("│ Name │ Status │"));
        assert!(is_rendered_markdown_table_line("┌──────┬────────┐"));
        assert!(!is_rendered_markdown_table_line("plain paragraph text"));
    }

    #[test]
    fn wrap_markdown_for_message_panel_preserves_table_rows() {
        let rendered = Text::from_lines(vec![
            Line::raw("│ LongHeader │ AnotherLongHeader │"),
            Line::raw("regular prose line that should wrap"),
        ]);
        let wrapped = wrap_markdown_for_message_panel(&rendered, 12);
        let plain: Vec<String> = wrapped.lines().iter().map(Line::to_plain_text).collect();

        assert!(
            plain
                .iter()
                .any(|line| line.contains("LongHeader │ AnotherLongHeader")),
            "table row should remain intact"
        );
        assert!(
            wrapped.lines().len() >= 3,
            "non-table prose should still wrap across multiple lines"
        );
    }

    #[test]
    fn collect_markdown_image_refs_parses_alt_and_source() {
        let md = "before ![diagram](./fixtures/diagram.png) middle ![](./img.webp) after";
        let refs = collect_markdown_image_refs(md);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].alt, "diagram");
        assert_eq!(refs[0].source, "./fixtures/diagram.png");
        assert_eq!(refs[1].alt, "");
        assert_eq!(refs[1].source, "./img.webp");
    }

    #[test]
    fn build_inline_image_block_handles_missing_file_without_panicking() {
        let md = "![missing](./definitely-does-not-exist-image.png)";
        let block = build_inline_image_block_with_hints(md, 40, &DetectionHints::default());
        assert!(block.contains("[Image: missing | protocol="));
        assert!(block.contains("[Image unavailable: ./definitely-does-not-exist-image.png]"));
    }

    fn write_png_fixture_path() -> (tempfile::TempDir, String) {
        // 1x1 transparent PNG.
        const PNG_1X1_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("inline-preview.png");
        let bytes = STANDARD
            .decode(PNG_1X1_BASE64)
            .expect("decode PNG fixture bytes");
        std::fs::write(&path, bytes).expect("write PNG fixture");
        (dir, path.to_string_lossy().to_string())
    }

    #[test]
    fn build_inline_image_block_kitty_path_uses_protocol_dispatch() {
        let (_dir, image_path) = write_png_fixture_path();
        let markdown = format!("![fixture]({image_path})");
        let hints = DetectionHints::default()
            .with_kitty_graphics(true)
            .with_iterm2_inline(false)
            .with_sixel(false);

        let block = build_inline_image_block_with_hints(&markdown, 80, &hints);
        assert!(block.contains("protocol=kitty"));
        assert!(block.contains("[kitty inline payload prepared:"));
    }

    #[test]
    fn build_inline_image_block_iterm2_path_uses_protocol_dispatch() {
        let (_dir, image_path) = write_png_fixture_path();
        let markdown = format!("![fixture]({image_path})");
        let hints = DetectionHints::default()
            .with_kitty_graphics(false)
            .with_iterm2_inline(true)
            .with_sixel(false);

        let block = build_inline_image_block_with_hints(&markdown, 80, &hints);
        assert!(block.contains("protocol=iterm2"));
        assert!(block.contains("[iterm2 inline payload prepared:"));
    }

    #[test]
    fn build_inline_image_block_sixel_path_uses_ascii_fallback_note() {
        let (_dir, image_path) = write_png_fixture_path();
        let markdown = format!("![fixture]({image_path})");
        let hints = DetectionHints::default()
            .with_kitty_graphics(false)
            .with_iterm2_inline(false)
            .with_sixel(true);

        let block = build_inline_image_block_with_hints(&markdown, 80, &hints);
        assert!(block.contains("protocol=sixel"));
        assert!(block.contains("[sixel detected; using ASCII fallback preview"));
    }

    #[test]
    fn estimate_message_detail_lines_adds_image_headroom() {
        let msg = MessageEntry {
            id: -1,
            subject: "Image detail".to_string(),
            from_agent: "A".to_string(),
            to_agents: "B".to_string(),
            project_slug: "p".to_string(),
            thread_id: String::new(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: "hello\n![img](./missing.png)".to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        };
        let baseline = 6 + 2 + msg.body_md.lines().count().max(1);
        let estimate = estimate_message_detail_lines(&msg, 80);
        assert!(
            estimate >= baseline + 12,
            "expected image headroom in estimate, baseline={baseline}, estimate={estimate}"
        );
    }

    #[test]
    fn colorize_json_body_preserves_core_tokens() {
        let palette = crate::tui_theme::TuiThemePalette::current();
        let text = colorize_json_body(
            "{\n  \"ok\": true,\n  \"count\": 42,\n  \"name\": \"agent-mail\"\n}",
            &palette,
        );
        let rendered = text
            .lines()
            .iter()
            .map(|line| {
                line.spans()
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("ok"));
        assert!(rendered.contains("true"));
        assert!(rendered.contains("42"));
        assert!(rendered.contains("agent-mail"));
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
            "search-v3",
            "",
            "", // mode_label
            "",
            0,
            false,
            "",
        );
    }

    #[test]
    fn render_results_empty_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let mut list_state = VirtualizedListState::default();
        let selection = SelectionState::new();
        render_results_list(
            &mut frame,
            Rect::new(0, 0, 40, 20),
            &[],
            &mut list_state,
            true,
            true,
            false,
            None,
            None,
            &selection,
        );
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
        let selection = SelectionState::new();
        render_results_list(
            &mut frame,
            Rect::new(0, 0, 40, 20),
            &entries,
            &mut list_state,
            true,
            true,
            false,
            None,
            None,
            &selection,
        );
    }

    #[test]
    fn render_results_with_unicode_sender_and_project_no_panic() {
        let entries = vec![MessageEntry {
            id: 1,
            subject: "[review] Session 16 code review pass — fixed".to_string(),
            from_agent: "Ágent🚀Name—Wide".to_string(),
            to_agents: "Team".to_string(),
            project_slug: "proj—超長slug".to_string(),
            thread_id: "thread-1".to_string(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: "Hello world".to_string(),
            importance: "high".to_string(),
            ack_required: true,
            show_project: true,
        }];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let mut list_state = VirtualizedListState::default();
        list_state.select(Some(0));
        let selection = SelectionState::new();
        render_results_list(
            &mut frame,
            Rect::new(0, 0, 40, 20),
            &entries,
            &mut list_state,
            true,
            true,
            false,
            None,
            None,
            &selection,
        );
    }

    #[test]
    fn render_detail_no_message_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let cache = RefCell::new(None);
        render_detail_panel(
            &mut frame,
            Rect::new(40, 0, 40, 20),
            None,
            0,
            true,
            &cache,
            DetailViewMode::Markdown,
            None,
            0,
        );
    }

    #[test]
    fn estimate_wrapped_line_count_accounts_for_soft_wrap() {
        let lines = vec![Line::raw("123456789"), Line::raw("x")];
        assert_eq!(estimate_wrapped_line_count(&lines, 4), 4);
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
        let cache = RefCell::new(None);
        render_detail_panel(
            &mut frame,
            Rect::new(40, 0, 40, 20),
            Some(&msg),
            0,
            true,
            &cache,
            DetailViewMode::Markdown,
            None,
            0,
        );
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
        let cache = RefCell::new(None);
        render_detail_panel(
            &mut frame,
            Rect::new(40, 0, 40, 20),
            Some(&msg),
            10,
            true,
            &cache,
            DetailViewMode::Markdown,
            None,
            0,
        );
    }

    #[test]
    fn render_detail_with_json_body_no_panic() {
        let msg = MessageEntry {
            id: 2,
            subject: "JSON payload".to_string(),
            from_agent: "Agent".to_string(),
            to_agents: "Peer".to_string(),
            project_slug: "proj".to_string(),
            thread_id: String::new(),
            timestamp_iso: "2026-02-06T12:00:00Z".to_string(),
            timestamp_micros: 0,
            body_md: "{\n  \"ok\": true,\n  \"count\": 3\n}".to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let cache = RefCell::new(None);
        render_detail_panel(
            &mut frame,
            Rect::new(40, 0, 40, 20),
            Some(&msg),
            0,
            true,
            &cache,
            DetailViewMode::Markdown,
            None,
            0,
        );
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
    fn narrow_tall_layout_keeps_detail_visible_with_stacked_fallback() {
        let screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 20, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 20), &state);

        let detail = screen.last_detail_area.get();
        let content = screen.last_content_area.get();
        assert!(
            detail.width > 0 && detail.height > 0,
            "stacked fallback should keep detail panel visible at 60x20"
        );
        assert_eq!(
            detail.width, content.width,
            "stacked fallback should preserve full-width detail panel"
        );
        assert!(
            detail.y > content.y,
            "stacked fallback detail should appear below list content"
        );
    }

    #[test]
    fn wide_layout_expands_default_detail_ratio_for_dense_viewports() {
        let screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(220, 28, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 220, 28), &state);

        let detail = screen.last_detail_area.get();
        let list = screen.last_results_area.get();
        assert!(detail.width > 0 && list.width > 0);
        assert!(
            detail.width > list.width,
            "default wide layout should allocate more room to detail on very wide screens: list={} detail={}",
            list.width,
            detail.width
        );
    }

    #[test]
    fn wide_layout_preserves_user_tuned_dock_ratio() {
        let mut screen = MessageBrowserScreen::new();
        screen.dock.set_ratio(0.3);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(220, 28, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 220, 28), &state);

        let detail = screen.last_detail_area.get();
        let list = screen.last_results_area.get();
        assert!(detail.width > 0 && list.width > 0);
        assert!(
            detail.width < list.width,
            "manual dock ratio should remain authoritative: list={} detail={}",
            list.width,
            detail.width
        );
    }

    #[test]
    fn narrow_short_layout_hides_detail_when_vertical_space_is_too_small() {
        let screen = MessageBrowserScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 10, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 60, 10), &state);

        let detail = screen.last_detail_area.get();
        assert_eq!(
            detail,
            Rect::new(0, 0, 0, 0),
            "detail should be hidden when narrow layout is too short for a usable stack"
        );
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

    #[test]
    fn receive_deep_link_compose_prefills_recipient() {
        let mut screen = MessageBrowserScreen::new();
        let handled =
            screen.receive_deep_link(&DeepLinkTarget::ComposeToAgent("BlueLake".to_string()));
        assert!(handled);
        let form = screen.compose_form.expect("compose form");
        assert_eq!(form.to_input.value(), "BlueLake");
    }

    #[test]
    fn receive_deep_link_compose_clears_quick_reply_modal() {
        let mut screen = MessageBrowserScreen::new();
        let entry = test_message_entry(77, "thread-77", "Escalation update");
        screen.quick_reply_form = QuickReplyFormState::from_entry(&entry);

        let handled =
            screen.receive_deep_link(&DeepLinkTarget::ComposeToAgent("BlueLake".to_string()));

        assert!(handled);
        assert!(screen.quick_reply_form.is_none());
        let form = screen.compose_form.expect("compose form");
        assert_eq!(form.to_input.value(), "BlueLake");
    }

    #[test]
    fn load_agent_names_for_project_requires_database_connection() {
        let screen = MessageBrowserScreen::new();
        let err = screen
            .load_agent_names_for_project("proj")
            .expect_err("expected missing-connection error");
        assert_eq!(err, "Database connection unavailable");
    }

    #[test]
    fn load_agent_names_for_placeholder_project_keeps_orphaned_agents_visible() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("messages-screen.db");
        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(77),
                Value::Text("proj".to_string()),
                Value::Text("/proj".to_string()),
                Value::BigInt(0),
            ],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(77),
                Value::Text("BlueLake".to_string()),
                Value::Text("test".to_string()),
                Value::Text("test".to_string()),
                Value::Text(String::new()),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ],
        )
        .expect("insert agent");
        conn.execute_raw("DELETE FROM projects WHERE id = 77")
            .expect("delete project row");

        let mut screen = MessageBrowserScreen::new();
        screen.db_conn = Some(conn);

        let agents = screen
            .load_agent_names_for_project("[unknown-project-77]")
            .expect("load agents");
        assert_eq!(agents, vec!["BlueLake".to_string()]);
    }

    #[test]
    fn fetch_recent_messages_accepts_placeholder_project_identifier() {
        let conn = DbConn::open_memory().expect("open memory sqlite");
        conn.execute_raw("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create agents");
        conn.execute_raw("CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)")
            .expect("create projects");
        conn.execute_raw(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                subject TEXT,
                body_md TEXT,
                thread_id TEXT,
                importance TEXT,
                ack_required INTEGER,
                created_ts INTEGER,
                sender_id INTEGER,
                project_id INTEGER,
                recipients_json TEXT
            )",
        )
        .expect("create messages");
        conn.execute_raw(
            "INSERT INTO messages (
                id, subject, body_md, thread_id, importance, ack_required,
                created_ts, sender_id, project_id, recipients_json
            ) VALUES (
                42, 'Subject', 'Body', 'thread-42', 'normal', 0,
                1700000000000000, 99, 77, '{\"to\":[\"BlueLake\"],\"cc\":[],\"bcc\":[]}'
            )",
        )
        .expect("seed message");

        let (entries, total) =
            fetch_recent_messages(&conn, PAGE_SIZE, Some("[unknown-project-77]"), true);

        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 42);
        assert_eq!(entries[0].project_slug, "[unknown-project-77]");
    }

    #[test]
    fn open_compose_modal_without_project_context_and_db_reports_database_unavailable() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.db_conn_attempted = true;

        screen.open_compose_modal(Some(&state), None);

        let form = screen.compose_form.expect("compose form");
        assert_eq!(
            form.errors.general.as_deref(),
            Some("Database connection unavailable")
        );
        assert!(!screen.db_conn_attempted);
    }

    #[test]
    fn receive_deep_link_reply_opens_quick_reply_modal() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(77, "thread-77", "Escalation update"));

        let handled = screen.receive_deep_link(&DeepLinkTarget::ReplyToMessage(77));
        assert!(handled);
        let form = screen.quick_reply_form.expect("quick reply form");
        assert_eq!(form.context.message_id, 77);
        assert_eq!(form.context.to_agent, "GoldFox");
        assert_eq!(form.context.thread_id.as_deref(), Some("thread-77"));
        assert_eq!(form.context.subject, "Re: Escalation update");
    }

    #[test]
    fn c_key_opens_compose_modal() {
        let mut screen = MessageBrowserScreen::new();
        screen.results.push(MessageEntry {
            id: 1,
            subject: "Test".to_string(),
            from_agent: "RedFox".to_string(),
            to_agents: "BlueLake".to_string(),
            project_slug: "proj".to_string(),
            thread_id: String::new(),
            timestamp_iso: String::new(),
            timestamp_micros: 0,
            body_md: String::new(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        });
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let event = Event::Key(ftui::KeyEvent::new(KeyCode::Char('c')));
        let _ = screen.update(&event, &state);
        assert!(screen.compose_form.is_some());
    }

    #[test]
    fn r_key_opens_quick_reply_modal_for_selected_message() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(55, "thread-55", "Status report"));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let event = Event::Key(ftui::KeyEvent::new(KeyCode::Char('r')));
        let _ = screen.update(&event, &state);
        let form = screen.quick_reply_form.expect("quick reply form");
        assert_eq!(form.context.message_id, 55);
        assert_eq!(form.context.subject, "Re: Status report");
        assert_eq!(form.context.to_agent, "GoldFox");
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

    #[test]
    fn messages_saved_presets_save_load_delete_lifecycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("screen_filter_presets.json");
        let mut screen = MessageBrowserScreen::with_filter_presets_path_for_test(&path);

        screen.search_input.set_value("urgent ack");
        screen.preset_index = 3;
        screen.inbox_mode = InboxMode::Local("proj-alpha".to_string());
        assert!(screen.save_named_preset("triage", Some("desc".to_string())));

        let loaded = crate::tui_persist::load_screen_filter_presets(&path).expect("load presets");
        let preset = loaded
            .get(MESSAGES_PRESET_SCREEN_ID, "triage")
            .expect("saved preset");
        assert_eq!(
            preset.values.get("query").map(String::as_str),
            Some("urgent ack")
        );
        assert_eq!(
            preset.values.get("preset_index").map(String::as_str),
            Some("3")
        );
        assert_eq!(
            preset.values.get("inbox_mode").map(String::as_str),
            Some("local")
        );
        assert_eq!(
            preset.values.get("local_project").map(String::as_str),
            Some("proj-alpha")
        );

        screen.search_input.set_value("reset");
        screen.preset_index = 0;
        screen.inbox_mode = InboxMode::Global;
        screen.search_dirty = false;
        assert!(screen.apply_named_preset("triage"));
        assert_eq!(screen.search_input.value(), "urgent ack");
        assert_eq!(screen.preset_index, 3);
        assert!(screen.search_dirty);
        assert_eq!(screen.debounce_remaining, 0);
        match &screen.inbox_mode {
            InboxMode::Local(project) => assert_eq!(project, "proj-alpha"),
            InboxMode::Global => panic!("expected local mode from preset"),
        }

        assert!(screen.remove_named_preset("triage"));
        assert!(screen.preset_names().is_empty());
    }

    #[test]
    fn apply_preset_values_local_without_project_uses_global_mode() {
        let mut screen = MessageBrowserScreen::new();
        let mut values = BTreeMap::new();
        values.insert("inbox_mode".to_string(), "local".to_string());
        values.insert("query".to_string(), "urgent".to_string());

        screen.apply_preset_values(&values);

        assert!(matches!(screen.inbox_mode, InboxMode::Global));
    }

    #[test]
    fn ctrl_shortcuts_drive_message_preset_dialog_flow() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("screen_filter_presets.json");
        let mut screen = MessageBrowserScreen::with_filter_presets_path_for_test(&path);

        let ctrl_s = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('s'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::CTRL,
        });
        screen.update(&ctrl_s, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::Save);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Enter)), &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::None);
        assert!(!screen.preset_names().is_empty());

        let ctrl_l = Event::Key(ftui::KeyEvent {
            code: KeyCode::Char('l'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers::CTRL,
        });
        screen.update(&ctrl_l, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::Load);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Delete)), &state);
        assert!(screen.preset_names().is_empty());
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Escape)), &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::None);
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
        let _ = SearchMethod::Live;
        let _ = SearchMethod::Recent;
        let _ = SearchMethod::Unified;
    }

    #[test]
    fn execute_search_without_db_uses_live_method_and_results() {
        use crate::tui_events::{EventSource, MailEvent};

        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::with_event_capacity(&config, 8);
        let _ = state.push_event(MailEvent::MessageSent {
            seq: 0,
            timestamp_micros: 1_700_000_000_000_000,
            source: EventSource::Mail,
            redacted: false,
            id: 42,
            from: "GoldHawk".to_string(),
            to: vec!["SilverFox".to_string()],
            subject: "hello world".to_string(),
            thread_id: "t-1".to_string(),
            project: "myproj".to_string(),
            body_md: "Test body content".to_string(),
        });

        let mut screen = MessageBrowserScreen::new();
        screen.db_conn_attempted = true;

        screen.execute_search(&state);

        assert_eq!(screen.search_method, SearchMethod::Live);
        assert_eq!(screen.results.len(), 1);
        assert_eq!(screen.results[0].id, 42);
    }

    #[test]
    fn execute_search_without_db_preserves_previous_db_results_for_same_query() {
        use crate::tui_events::{EventSource, MailEvent};

        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::with_event_capacity(&config, 8);
        let _ = state.push_event(MailEvent::MessageSent {
            seq: 0,
            timestamp_micros: 1_700_000_000_000_000,
            source: EventSource::Mail,
            redacted: false,
            id: 77,
            from: "BluePeak".to_string(),
            to: vec!["RedLake".to_string()],
            subject: "fresh live".to_string(),
            thread_id: "t-live".to_string(),
            project: "proj".to_string(),
            body_md: "New body".to_string(),
        });

        let mut screen = MessageBrowserScreen::new();
        screen.db_conn_attempted = true;
        screen.search_method = SearchMethod::Recent;
        screen.total_results = 3;
        screen.results = vec![
            test_message_entry(1, "thread-a", "persisted one"),
            test_message_entry(2, "thread-b", "persisted two"),
        ];

        screen.execute_search(&state);

        assert_eq!(screen.search_method, SearchMethod::Live);
        assert_eq!(screen.total_results, 3);
        assert_eq!(screen.results.len(), 3);
        assert_eq!(screen.results[0].id, 77);
        assert!(screen.results.iter().any(|entry| entry.id == 1));
        assert!(screen.results.iter().any(|entry| entry.id == 2));
    }

    #[test]
    fn execute_search_without_db_does_not_preserve_previous_results_for_new_query() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::with_event_capacity(&config, 8);

        let mut screen = MessageBrowserScreen::new();
        screen.db_conn_attempted = true;
        screen.search_method = SearchMethod::Recent;
        screen.total_results = 1;
        screen.results = vec![test_message_entry(1, "thread-a", "persisted one")];
        screen.search_input.set_value("fresh");

        screen.execute_search(&state);

        assert_eq!(screen.search_method, SearchMethod::Live);
        assert!(screen.results.is_empty());
        assert_eq!(screen.total_results, 0);
    }

    #[test]
    fn execute_search_without_db_does_not_preserve_previous_results_across_scope_change() {
        use crate::tui_events::{EventSource, MailEvent};

        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::with_event_capacity(&config, 8);
        let _ = state.push_event(MailEvent::MessageSent {
            seq: 0,
            timestamp_micros: 1_700_000_000_000_000,
            source: EventSource::Mail,
            redacted: false,
            id: 88,
            from: "BluePeak".to_string(),
            to: vec!["RedLake".to_string()],
            subject: "fresh live".to_string(),
            thread_id: "t-live".to_string(),
            project: "proj-b".to_string(),
            body_md: "New body".to_string(),
        });

        let mut previous = test_message_entry(1, "thread-a", "persisted one");
        previous.project_slug = "proj-a".to_string();
        previous.show_project = true;

        let mut screen = MessageBrowserScreen::new();
        screen.db_conn_attempted = true;
        screen.search_method = SearchMethod::Recent;
        screen.total_results = 1;
        screen.results = vec![previous];
        screen.inbox_mode = InboxMode::Local("proj-b".to_string());

        screen.execute_search(&state);

        assert_eq!(screen.search_method, SearchMethod::Live);
        assert_eq!(screen.results.len(), 1);
        assert_eq!(screen.results[0].id, 88);
        assert_eq!(screen.results[0].project_slug, "proj-b");
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
            "search-v3",
            "Urgent",
            "", // mode_label
            "Right 40%",
            3,
            true,
            "req:1 ok:1 err:0 avg:1ms spark:......",
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
            "",
            0,
            false,
            "",
        );
    }

    #[test]
    fn keybindings_include_preset() {
        let screen = MessageBrowserScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.iter().any(|b| b.key == "p/P"));
        assert!(bindings.iter().any(|b| b.key == "Ctrl+S"));
        assert!(bindings.iter().any(|b| b.key == "Ctrl+L"));
    }

    #[test]
    fn keybindings_include_compose() {
        let screen = MessageBrowserScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.iter().any(|b| b.key == "c"));
        assert!(bindings.iter().any(|b| b.key == "r"));
        assert!(bindings.iter().any(|b| b.key == "F5/Ctrl+Enter"));
    }

    #[test]
    fn compose_validation_flags_required_fields() {
        let form = ComposeFormState::new(
            "proj".to_string(),
            None,
            vec!["BlueLake".to_string(), "RedFox".to_string()],
        );
        let err = validate_compose_form(&form).expect_err("expected validation error");
        assert!(err.to.is_some());
        assert!(err.subject.is_some());
        assert!(err.body.is_some());
    }

    #[test]
    fn quick_reply_validation_flags_required_body() {
        let entry = test_message_entry(90, "thread-90", "Follow-up");
        let form = QuickReplyFormState::from_entry(&entry).expect("quick reply form");
        let err = validate_quick_reply_form(&form).expect_err("expected validation error");
        assert_eq!(err.body.as_deref(), Some("Reply body is required."));
    }

    #[test]
    fn compose_autocomplete_applies_selected_agent() {
        let mut form = ComposeFormState::new(
            "proj".to_string(),
            None,
            vec!["BlueLake".to_string(), "BluePeak".to_string()],
        );
        form.to_input.set_value("Blue");
        form.set_focus(ComposeField::To);
        assert!(!form.suggestions.is_empty());
        assert!(form.apply_suggestion());
        assert_eq!(form.to_input.value(), "BlueLake");
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
        screen
            .results
            .push(test_message_entry(1, "thread-a", "One"));
        screen.cursor = 0;

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
    fn toggle_inbox_mode_stays_global_without_project_context() {
        let mut screen = MessageBrowserScreen::new();
        screen.search_dirty = false;

        screen.toggle_inbox_mode();

        assert!(matches!(screen.inbox_mode, InboxMode::Global));
        assert!(!screen.search_dirty);
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

    #[test]
    fn space_toggles_message_selection() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(1, "thread-a", "One"));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let space = Event::Key(ftui::KeyEvent::new(KeyCode::Char(' ')));
        let _ = screen.update(&space, &state);
        assert!(screen.selected_message_ids.contains(&1));

        let _ = screen.update(&space, &state);
        assert!(!screen.selected_message_ids.contains(&1));
    }

    #[test]
    fn shift_a_and_shift_c_manage_bulk_selection() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(1, "thread-a", "One"));
        screen
            .results
            .push(test_message_entry(2, "thread-b", "Two"));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let select_all = Event::Key(ftui::KeyEvent::new(KeyCode::Char('A')));
        let _ = screen.update(&select_all, &state);
        assert_eq!(screen.selected_message_ids.len(), 2);

        let clear = Event::Key(ftui::KeyEvent::new(KeyCode::Char('C')));
        let _ = screen.update(&clear, &state);
        assert!(screen.selected_message_ids.is_empty());
    }

    #[test]
    fn visual_mode_extends_selection_when_cursor_moves() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(1, "thread-a", "One"));
        screen
            .results
            .push(test_message_entry(2, "thread-b", "Two"));
        screen
            .results
            .push(test_message_entry(3, "thread-c", "Three"));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let visual = Event::Key(ftui::KeyEvent::new(KeyCode::Char('v')));
        let _ = screen.update(&visual, &state);
        assert!(screen.selected_message_ids.contains(&1));
        assert!(screen.selected_message_ids.visual_mode());

        let down = Event::Key(ftui::KeyEvent::new(KeyCode::Char('j')));
        let _ = screen.update(&down, &state);
        assert!(screen.selected_message_ids.contains(&2));
    }

    #[test]
    fn contextual_actions_switch_to_batch_when_multi_selected() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(11, "thread-a", "One"));
        screen
            .results
            .push(test_message_entry(22, "thread-b", "Two"));
        screen.selected_message_ids.select(11);
        screen.selected_message_ids.select(22);

        let (actions, _anchor, context_id) = screen
            .contextual_actions()
            .expect("batch contextual actions");
        assert!(context_id.starts_with("batch:"));
        assert!(
            actions
                .iter()
                .any(|entry| entry.label.starts_with("Acknowledge selected")),
            "batch action menu should include acknowledge selected"
        );
    }

    #[test]
    fn urgent_pulse_toggles_on_tick_boundary() {
        let mut screen = MessageBrowserScreen::new();
        screen.reduced_motion = false;

        screen.update_urgent_pulse(0);
        assert!(MESSAGE_URGENT_PULSE_ON.load(Ordering::Relaxed));

        screen.update_urgent_pulse(URGENT_PULSE_HALF_PERIOD_TICKS);
        assert!(!MESSAGE_URGENT_PULSE_ON.load(Ordering::Relaxed));
    }

    #[test]
    fn urgent_pulse_forces_on_when_reduced_motion_enabled() {
        let mut screen = MessageBrowserScreen::new();
        screen.reduced_motion = true;

        screen.update_urgent_pulse(URGENT_PULSE_HALF_PERIOD_TICKS);
        assert!(MESSAGE_URGENT_PULSE_ON.load(Ordering::Relaxed));
    }

    #[test]
    fn search_live_events_extracts_structured_fields() {
        use crate::tui_events::{EventSource, MailEvent};

        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::with_event_capacity(&config, 16);

        // Push a MessageSent event
        let _ = state.push_event(MailEvent::MessageSent {
            seq: 0,
            timestamp_micros: 1_700_000_000_000_000,
            source: EventSource::Mail,
            redacted: false,
            id: 42,
            from: "GoldHawk".to_string(),
            to: vec!["SilverFox".to_string()],
            subject: "hello world".to_string(),
            thread_id: "t-1".to_string(),
            project: "myproj".to_string(),
            body_md: "Test body content".to_string(),
        });
        // Push a MessageReceived event
        let _ = state.push_event(MailEvent::MessageReceived {
            seq: 0,
            timestamp_micros: 1_700_000_001_000_000,
            source: EventSource::Mail,
            redacted: false,
            id: 43,
            from: "SilverFox".to_string(),
            to: vec!["GoldHawk".to_string()],
            subject: "re: hello world".to_string(),
            thread_id: "t-1".to_string(),
            project: "myproj".to_string(),
            body_md: "Reply body content".to_string(),
        });
        // Push a non-message event (should be filtered out)
        let _ = state.push_event(MailEvent::http_request("GET", "/foo", 200, 1, "127.0.0.1"));

        // Empty query returns all message events
        let results = MessageBrowserScreen::search_live_events(&state, "", false);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].from_agent, "GoldHawk");
        assert_eq!(results[0].subject, "hello world");
        assert_eq!(results[0].id, 42);
        assert_eq!(results[1].from_agent, "SilverFox");

        // Query filters by subject
        let filtered = MessageBrowserScreen::search_live_events(&state, "re: hello", false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 43);

        // Query matches against sender + recipients + subject
        // "goldhawk" appears in both events (sender of #42, recipient of #43)
        let by_agent = MessageBrowserScreen::search_live_events(&state, "goldhawk", false);
        assert_eq!(by_agent.len(), 2);

        // Body text stays searchable in degraded live-event mode.
        let by_body = MessageBrowserScreen::search_live_events(&state, "reply body", false);
        assert_eq!(by_body.len(), 1);
        assert_eq!(by_body[0].id, 43);

        // Non-matching query returns empty
        let empty = MessageBrowserScreen::search_live_events(&state, "nonexistent", false);
        assert!(empty.is_empty());

        // show_project flag propagates
        let with_proj = MessageBrowserScreen::search_live_events(&state, "", true);
        assert!(with_proj.iter().all(|r| r.show_project));
    }

    #[test]
    fn live_search_retry_marks_degraded_search_dirty_after_interval() {
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let mut screen = MessageBrowserScreen::new();
        screen.search_input.set_value("stuck");
        screen.search_dirty = false;
        screen.search_method = SearchMethod::Live;
        screen.db_conn_attempted = true;
        screen.last_refresh = Some(
            Instant::now()
                .checked_sub(Duration::from_secs(6))
                .unwrap_or_else(Instant::now),
        );

        screen.tick(1, &state);

        assert!(screen.search_dirty);
        assert_eq!(screen.debounce_remaining, 0);
        assert!(!screen.db_conn_attempted);
    }

    #[test]
    fn shimmer_progress_returns_none_after_window() {
        let now = 1_700_000_000_000_000_i64;
        assert!(shimmer_progress_for_timestamp(now, now).is_some());
        assert!(shimmer_progress_for_timestamp(now, now - SHIMMER_WINDOW_MICROS - 1).is_none());
        assert!(shimmer_progress_for_timestamp(now, 0).is_none());
    }

    #[test]
    fn compute_shimmer_progresses_caps_at_five_rows() {
        let now = unix_epoch_micros_now().expect("system clock should provide unix micros");
        let entries: Vec<MessageEntry> = (0..8)
            .map(|idx| MessageEntry {
                id: i64::from(idx) + 1,
                subject: format!("msg-{idx}"),
                from_agent: "GoldFox".to_string(),
                to_agents: "SilverWolf".to_string(),
                project_slug: "proj".to_string(),
                thread_id: "thread".to_string(),
                timestamp_iso: micros_to_iso(now - (i64::from(idx) * 10_000)),
                timestamp_micros: now - (i64::from(idx) * 10_000),
                body_md: String::new(),
                importance: "normal".to_string(),
                ack_required: false,
                show_project: false,
            })
            .collect();
        let progresses = compute_shimmer_progresses(&entries, true);
        assert_eq!(
            progresses.iter().filter(|p| p.is_some()).count(),
            SHIMMER_MAX_ROWS
        );

        let disabled = compute_shimmer_progresses(&entries, false);
        assert!(disabled.iter().all(Option::is_none));
    }

    #[test]
    fn recipient_names_by_message_keeps_missing_agent_rows() {
        let conn = DbConn::open_memory().expect("open memory sqlite");
        conn.execute_raw("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .expect("create agents");
        conn.execute_raw(
            "CREATE TABLE message_recipients (
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL
            )",
        )
        .expect("create recipients");
        conn.execute_raw(
            "INSERT INTO agents (id, name) VALUES (1, 'BlueLake');
             INSERT INTO message_recipients (message_id, agent_id) VALUES
                (10, 1),
                (10, 99)",
        )
        .expect("seed recipients");

        let names = recipient_names_by_message(&conn, &[10]);
        let to_agents = names.get(&10).expect("message recipients");
        assert!(to_agents.contains("BlueLake"));
        assert!(to_agents.contains("[unknown-agent-99]"));
    }

    #[test]
    fn message_entries_from_rows_preserve_unknown_sender_and_project_labels() {
        let conn = DbConn::open_memory().expect("open memory sqlite");
        conn.execute_raw("CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create agents");
        conn.execute_raw("CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT)")
            .expect("create projects");

        let entries = message_entries_from_rows(
            &conn,
            vec![RawMessageRow {
                id: 42,
                subject: "Subject".to_string(),
                body_md: "Body".to_string(),
                thread_id: "thread-42".to_string(),
                importance: "normal".to_string(),
                ack_required: false,
                created_ts: 1_700_000_000_000_000,
                sender_id: 99,
                project_id: 77,
                recipients_json: r#"{"to":["BlueLake"],"cc":[],"bcc":[]}"#.to_string(),
            }],
            true,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].from_agent, "[unknown-agent-99]");
        assert_eq!(entries[0].project_slug, "[unknown-project-77]");
    }

    #[test]
    fn message_row_hierarchy_semantic_fields() {
        // Verify the MessageEntry struct carries all fields needed for
        // the redesigned row: importance, ack_required, from_agent, id polarity.
        let normal = MessageEntry {
            id: 10,
            subject: "Test".to_string(),
            from_agent: "GoldHawk".to_string(),
            to_agents: "SilverFox".to_string(),
            project_slug: "proj".to_string(),
            thread_id: "t-1".to_string(),
            timestamp_iso: "2026-02-15T12:00:00".to_string(),
            timestamp_micros: 1_000_000,
            body_md: String::new(),
            importance: "normal".to_string(),
            ack_required: false,
            show_project: false,
        };
        // Normal importance: no special badge
        assert_eq!(normal.importance, "normal");
        assert!(!normal.ack_required);
        assert!(normal.id >= 0); // DB entry shows #id

        let urgent_ack = MessageEntry {
            importance: "urgent".to_string(),
            ack_required: true,
            id: -1, // Live entry shows "LIVE"
            ..normal.clone()
        };
        assert_eq!(urgent_ack.importance, "urgent");
        assert!(urgent_ack.ack_required);
        assert!(urgent_ack.id < 0); // Live entry

        let high = MessageEntry {
            importance: "high".to_string(),
            from_agent: "LongSenderNameHere".to_string(),
            ..normal
        };
        assert_eq!(high.importance, "high");
        // Sender truncation in render is at 12 chars
        assert!(high.from_agent.len() > 12);
    }

    // ── Batch operation tests (br-2bbt.10) ──────────────────────────

    #[test]
    fn batch_acknowledge_dispatches_individual_actions() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(10, "thread-a", "One"));
        screen
            .results
            .push(test_message_entry(20, "thread-b", "Two"));
        screen
            .results
            .push(test_message_entry(30, "thread-c", "Three"));
        screen.selected_message_ids.select(10);
        screen.selected_message_ids.select(30);

        let cmd = screen.handle_action("batch_acknowledge", "batch:10,30");
        // Should clear selection
        assert!(screen.selected_message_ids.is_empty());
        // Should return a batch of individual acknowledge commands
        match cmd {
            Cmd::Batch(cmds) => {
                assert_eq!(cmds.len(), 2, "should dispatch 2 acknowledge actions");
            }
            other => panic!("expected Cmd::Batch, got {other:?}"),
        }
    }

    #[test]
    fn batch_mark_read_dispatches_individual_actions() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(5, "thread-a", "One"));
        screen
            .results
            .push(test_message_entry(6, "thread-b", "Two"));
        screen.selected_message_ids.select(5);
        screen.selected_message_ids.select(6);

        let cmd = screen.handle_action("batch_mark_read", "batch:5,6");
        assert!(screen.selected_message_ids.is_empty());
        match cmd {
            Cmd::Batch(cmds) => {
                assert_eq!(cmds.len(), 2, "should dispatch 2 mark_read actions");
            }
            other => panic!("expected Cmd::Batch, got {other:?}"),
        }
    }

    #[test]
    fn batch_mark_unread_dispatches_individual_actions() {
        let mut screen = MessageBrowserScreen::new();
        screen
            .results
            .push(test_message_entry(7, "thread-a", "One"));
        screen
            .results
            .push(test_message_entry(8, "thread-b", "Two"));
        screen
            .results
            .push(test_message_entry(9, "thread-c", "Three"));
        screen.selected_message_ids.select(7);
        screen.selected_message_ids.select(8);
        screen.selected_message_ids.select(9);

        let cmd = screen.handle_action("batch_mark_unread", "batch:7,8,9");
        assert!(screen.selected_message_ids.is_empty());
        match cmd {
            Cmd::Batch(cmds) => {
                assert_eq!(cmds.len(), 3, "should dispatch 3 mark_unread actions");
            }
            other => panic!("expected Cmd::Batch, got {other:?}"),
        }
    }

    #[test]
    fn batch_acknowledge_with_empty_selection_returns_noop() {
        let mut screen = MessageBrowserScreen::new();
        let cmd = screen.handle_action("batch_acknowledge", "batch:");
        // Cmd::batch with empty vec collapses to Cmd::None
        assert!(
            matches!(cmd, Cmd::None | Cmd::Batch(_)),
            "empty selection should produce noop or empty batch"
        );
    }

    #[test]
    fn unknown_action_returns_none() {
        let mut screen = MessageBrowserScreen::new();
        let cmd = screen.handle_action("nonexistent_action", "ctx");
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn recipient_names_from_json_combines_groups_in_stable_order() {
        let raw = r#"{"to":["BlueLake","GreenCastle"],"cc":["RedFox"],"bcc":["AmberOx"]}"#;
        assert_eq!(
            recipient_names_from_json(raw),
            "BlueLake, GreenCastle, RedFox, AmberOx"
        );
    }

    #[test]
    fn recipient_names_from_json_dedups_and_ignores_invalid_payloads() {
        let raw = r#"{"to":["BlueLake","bluelake",""],"cc":["BlueLake","  "],"bcc":["AmberOx"]}"#;
        assert_eq!(recipient_names_from_json(raw), "BlueLake, AmberOx");
        assert!(recipient_names_from_json("{not-json").is_empty());
    }
}
