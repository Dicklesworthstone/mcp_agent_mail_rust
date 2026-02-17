//! Chronological timeline pane with dense navigation affordances.
//!
//! [`TimelinePane`] provides a cursor-based, scrollable event timeline
//! designed for deep diagnosis.  It renders each event as a compact row
//! with sequence number, timestamp, source badge, icon, and summary,
//! and exposes cursor position so a parent screen can render an
//! inspector detail panel alongside.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, Modifiers, MouseButton, MouseEventKind, Style};
use ftui_runtime::program::Cmd;
use ftui_widgets::StatefulWidget;
use ftui_widgets::virtualized::{RenderItem, VirtualizedList, VirtualizedListState};

use crate::tui_action_menu::{ActionEntry, ActionKind, timeline_actions, timeline_batch_actions};
use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{EventSeverity, EventSource, MailEvent, MailEventKind, VerbosityTier};
use crate::tui_layout::{DockLayout, DockPreset};
use crate::tui_persist::{
    PreferencePersister, ScreenFilterPresetStore, TuiPreferences,
    console_persist_path_from_env_or_default, load_screen_filter_presets_or_default,
    save_screen_filter_presets, screen_filter_presets_path,
};
use crate::tui_screens::{
    DeepLinkTarget, HelpEntry, MailScreen, MailScreenId, MailScreenMsg, SelectionState,
};

// Re-use dashboard formatting helpers.
use super::dashboard::{EventEntry, format_event};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max event entries retained in the timeline scroll-back.
const TIMELINE_CAPACITY: usize = 5000;

/// Page-up/down scroll amount in lines.
const PAGE_SIZE: usize = 20;
const SHIMMER_WINDOW_MICROS: i64 = 500_000;
const SHIMMER_MAX_ROWS: usize = 5;
const SHIMMER_HIGHLIGHT_WIDTH: usize = 5;
const COMMIT_REFRESH_EVERY_TICKS: u64 = 20;
const COMMIT_LIMIT_PER_PROJECT: usize = 200;
const TIMELINE_PRESET_SCREEN_ID: &str = "timeline";

/// Result of a background commit refresh operation.
struct CommitRefreshResult {
    commits: Vec<CommitTimelineEntry>,
    stats: CommitTimelineStats,
}

// ──────────────────────────────────────────────────────────────────────
// TimelinePane
// ──────────────────────────────────────────────────────────────────────

/// A cursor-based, filterable, chronological event timeline.
///
/// Unlike the dashboard event log (scroll-offset based, auto-follow),
/// the timeline uses an explicit cursor position for event selection,
/// and defaults to *not* auto-following so the operator can inspect
/// historical events.
pub struct TimelinePane {
    /// All ingested event entries.
    entries: Vec<TimelineEntry>,
    /// Last consumed sequence number.
    last_seq: u64,
    /// Cursor position in the *filtered* view (0 = first visible entry).
    cursor: usize,
    /// Whether the cursor tracks new events automatically.
    follow: bool,
    /// Kind filter (empty = show all).
    kind_filter: HashSet<MailEventKind>,
    /// Source filter (empty = show all).
    source_filter: HashSet<EventSource>,
    /// Verbosity tier controlling minimum severity shown.
    verbosity: VerbosityTier,
    /// Total events ingested (including trimmed).
    total_ingested: u64,
}

/// Extended entry that retains the raw event for inspector access.
#[derive(Debug, Clone)]
pub(crate) struct TimelineEntry {
    /// Formatted display entry.
    pub display: EventEntry,
    /// Raw sequence number.
    pub seq: u64,
    /// Raw timestamp (microseconds).
    pub timestamp_micros: i64,
    /// Event source (for source filtering).
    pub source: EventSource,
    /// Derived severity (for verbosity filtering).
    pub severity: EventSeverity,
    /// Raw event for the inspector detail panel (br-10wc.7.2).
    pub raw: MailEvent,
}

impl RenderItem for TimelineEntry {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 10 {
            return;
        }

        let sev = self.severity;
        let src_badge = source_badge(self.source);
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
        let meta_style = crate::tui_theme::text_meta(&tp);

        // Severity-aware summary style: errors/warnings inherit severity
        // color for rapid triage; trace events are dim to reduce noise.
        let summary_style = match sev {
            EventSeverity::Error | EventSeverity::Warn => sev.style(),
            EventSeverity::Trace => Style::default().fg(tp.text_disabled).dim(),
            _ => Style::default(),
        };

        let mut line = Line::from_spans([
            Span::styled(
                format!("{marker}{:>6} {} ", self.seq, self.display.timestamp),
                meta_style,
            ),
            sev.styled_badge(),
            Span::styled(format!(" [{src_badge}] "), meta_style),
            Span::styled(format!("{}", self.display.icon), sev.style()),
            Span::styled(
                format!(" {:<10} ", self.display.kind.compact_label()),
                meta_style,
            ),
            Span::styled(self.display.summary.clone(), summary_style),
        ]);
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

#[derive(Debug, Clone)]
struct CommitTimelineEntry {
    project_slug: String,
    short_sha: String,
    timestamp_micros: i64,
    timestamp_label: String,
    subject: String,
    commit_type: String,
    sender: Option<String>,
    recipients: Vec<String>,
    author: String,
}

impl CommitTimelineEntry {
    fn from_storage(project_slug: String, entry: mcp_agent_mail_storage::TimelineEntry) -> Self {
        let timestamp_micros = entry.timestamp.saturating_mul(1_000_000);
        Self {
            project_slug,
            short_sha: entry.short_sha,
            timestamp_micros,
            timestamp_label: crate::tui_events::format_event_timestamp(timestamp_micros),
            subject: entry.subject,
            commit_type: entry.commit_type,
            sender: entry.sender,
            recipients: entry.recipients,
            author: entry.author,
        }
    }

    fn type_label(&self) -> &'static str {
        match self.commit_type.as_str() {
            "message" => "CommitMsg",
            "file_reservation" => "FileResv",
            "chore" => "Chore",
            _ => "Commit",
        }
    }

    fn detail_summary(&self) -> String {
        if let Some(sender) = &self.sender {
            if self.recipients.is_empty() {
                return format!("{sender} · {}", self.subject);
            }
            return format!(
                "{sender} -> {} · {}",
                self.recipients.join(", "),
                self.subject
            );
        }
        format!("{} · {}", self.author, self.subject)
    }
}

impl RenderItem for CommitTimelineEntry {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 10 {
            return;
        }

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
        let meta_style = crate::tui_theme::text_meta(&tp);
        let type_style = match self.commit_type.as_str() {
            "message" => Style::default().fg(tp.status_accent),
            "file_reservation" => Style::default().fg(tp.severity_warn),
            "chore" => Style::default().fg(tp.text_secondary),
            _ => Style::default().fg(tp.text_muted),
        };

        let mut line = Line::from_spans([
            Span::styled(
                format!(
                    "{marker}{:<8} {} [{}] ",
                    self.short_sha, self.timestamp_label, self.project_slug
                ),
                meta_style,
            ),
            Span::styled(format!(" {:<10} ", self.type_label()), type_style),
            Span::styled(self.detail_summary(), Style::default()),
        ]);

        if selected {
            line.apply_base_style(cursor_style);
        }

        Paragraph::new(Text::from_line(line)).render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

#[derive(Debug, Clone)]
enum CombinedTimelineRow {
    Event(TimelineEntry),
    Commit(CommitTimelineEntry),
}

impl CombinedTimelineRow {
    const fn timestamp_micros(&self) -> i64 {
        match self {
            Self::Event(entry) => entry.timestamp_micros,
            Self::Commit(entry) => entry.timestamp_micros,
        }
    }
}

impl RenderItem for CombinedTimelineRow {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool) {
        match self {
            Self::Event(entry) => entry.render(area, frame, selected),
            Self::Commit(entry) => entry.render(area, frame, selected),
        }
    }

    fn height(&self) -> u16 {
        1
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TimelineSelectionKey {
    Event {
        seq: u64,
    },
    Commit {
        project_slug: String,
        short_sha: String,
        timestamp_micros: i64,
    },
}

impl TimelineSelectionKey {
    fn for_event(entry: &TimelineEntry) -> Self {
        Self::Event { seq: entry.seq }
    }

    fn for_commit(entry: &CommitTimelineEntry) -> Self {
        Self::Commit {
            project_slug: entry.project_slug.clone(),
            short_sha: entry.short_sha.clone(),
            timestamp_micros: entry.timestamp_micros,
        }
    }
}

#[derive(Debug, Clone)]
struct TimelineActionRow {
    key: TimelineSelectionKey,
    copy_text: String,
    event_kind: String,
    event_source: String,
}

#[derive(Debug, Clone, Default)]
struct CommitTimelineStats {
    total_commits: usize,
    unique_authors: usize,
    active_projects: usize,
    message_commits: usize,
    reservation_commits: usize,
    churn_insertions: usize,
    churn_deletions: usize,
    refresh_errors: usize,
}

impl CommitTimelineStats {
    fn from_entries(entries: &[CommitTimelineEntry], refresh_errors: usize) -> Self {
        let mut authors = HashSet::new();
        let mut projects = HashSet::new();
        let mut message_commits = 0usize;
        let mut reservation_commits = 0usize;

        for entry in entries {
            authors.insert(entry.author.clone());
            projects.insert(entry.project_slug.clone());
            match entry.commit_type.as_str() {
                "message" => message_commits += 1,
                "file_reservation" => reservation_commits += 1,
                _ => {}
            }
        }

        Self {
            total_commits: entries.len(),
            unique_authors: authors.len(),
            active_projects: projects.len(),
            message_commits,
            reservation_commits,
            churn_insertions: 0,
            churn_deletions: 0,
            refresh_errors,
        }
    }

    fn summary_line(&self) -> String {
        let mut summary = format!(
            "Stats commits:{} authors:{} projects:{} +{} -{} msg:{} resv:{}",
            self.total_commits,
            self.unique_authors,
            self.active_projects,
            self.churn_insertions,
            self.churn_deletions,
            self.message_commits,
            self.reservation_commits
        );
        if self.refresh_errors > 0 {
            summary.push_str(" errs:");
            summary.push_str(&self.refresh_errors.to_string());
        }
        summary
    }
}

impl TimelinePane {
    /// Create a new empty timeline pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(TIMELINE_CAPACITY),
            last_seq: 0,
            cursor: 0,
            follow: false,
            kind_filter: HashSet::new(),
            source_filter: HashSet::new(),
            verbosity: VerbosityTier::default(),
            total_ingested: 0,
        }
    }

    /// Ingest new events from the shared state ring buffer.
    pub fn ingest(&mut self, state: &TuiSharedState) {
        let new_events = state.events_since(self.last_seq);
        for event in &new_events {
            self.last_seq = event.seq().max(self.last_seq);
            self.total_ingested += 1;
            self.entries.push(TimelineEntry {
                display: format_event(event),
                seq: event.seq(),
                timestamp_micros: event.timestamp_micros(),
                source: event.source(),
                severity: event.severity(),
                raw: event.clone(),
            });
        }
        // Trim to capacity.
        if self.entries.len() > TIMELINE_CAPACITY {
            let excess = self.entries.len() - TIMELINE_CAPACITY;
            self.entries.drain(..excess);
            // Adjust cursor if it pointed at drained entries.
            self.cursor = self.cursor.saturating_sub(excess);
        }
        // Auto-follow: move cursor to end.
        if self.follow && !new_events.is_empty() {
            let filtered_len = self.filtered_len();
            if filtered_len > 0 {
                self.cursor = filtered_len - 1;
            }
        }
    }

    /// Return the currently selected raw event (if any).
    #[must_use]
    pub fn selected_event(&self) -> Option<&MailEvent> {
        let filtered = self.filtered_entries();
        filtered.get(self.cursor).map(|e| &e.raw)
    }

    /// Return the currently selected timeline entry (if any).
    ///
    /// Pre-wired for the inspector panel (br-10wc.7.2).
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn selected_entry(&self) -> Option<&TimelineEntry> {
        let filtered = self.filtered_entries();
        filtered.into_iter().nth(self.cursor)
    }

    /// Cursor position in the filtered view.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether follow mode is active.
    #[must_use]
    pub const fn follow(&self) -> bool {
        self.follow
    }

    /// Toggle a kind filter on/off.
    pub fn toggle_kind_filter(&mut self, kind: MailEventKind) {
        if self.kind_filter.contains(&kind) {
            self.kind_filter.remove(&kind);
        } else {
            self.kind_filter.insert(kind);
        }
        self.clamp_cursor();
    }

    /// Toggle a source filter on/off.
    pub fn toggle_source_filter(&mut self, source: EventSource) {
        if self.source_filter.contains(&source) {
            self.source_filter.remove(&source);
        } else {
            self.source_filter.insert(source);
        }
        self.clamp_cursor();
    }

    /// Clear all filters and reset verbosity to default.
    pub fn clear_filters(&mut self) {
        self.kind_filter.clear();
        self.source_filter.clear();
        self.verbosity = VerbosityTier::default();
        self.clamp_cursor();
    }

    /// Jump to the entry closest to the given timestamp (microseconds).
    pub fn jump_to_time(&mut self, target_micros: i64) {
        let filtered = self.filtered_entries();
        if filtered.is_empty() {
            return;
        }
        // Binary search for closest entry.
        let idx = filtered
            .binary_search_by_key(&target_micros, |e| e.timestamp_micros)
            .unwrap_or_else(|i| i.min(filtered.len() - 1));
        self.cursor = idx;
        self.follow = false;
    }

    /// Move cursor up by `n` lines.
    pub const fn cursor_up(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
        self.follow = false;
    }

    /// Move cursor down by `n` lines.
    pub fn cursor_down(&mut self, n: usize) {
        let max = self.filtered_len().saturating_sub(1);
        self.cursor = (self.cursor + n).min(max);
    }

    /// Jump to first entry.
    pub const fn cursor_home(&mut self) {
        self.cursor = 0;
        self.follow = false;
    }

    /// Jump to last entry.
    pub fn cursor_end(&mut self) {
        let max = self.filtered_len().saturating_sub(1);
        self.cursor = max;
    }

    /// Toggle follow mode.
    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.cursor_end();
        }
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn filtered_entries(&self) -> Vec<&TimelineEntry> {
        self.entries
            .iter()
            .filter(|e| {
                self.verbosity.includes(e.severity)
                    && (self.kind_filter.is_empty() || self.kind_filter.contains(&e.display.kind))
                    && (self.source_filter.is_empty() || self.source_filter.contains(&e.source))
            })
            .collect()
    }

    fn filtered_len(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| {
                self.verbosity.includes(e.severity)
                    && (self.kind_filter.is_empty() || self.kind_filter.contains(&e.display.kind))
                    && (self.source_filter.is_empty() || self.source_filter.contains(&e.source))
            })
            .count()
    }

    fn clamp_cursor(&mut self) {
        let max = self.filtered_len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
    }
}

impl Default for TimelinePane {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// TimelineScreen — wraps TimelinePane as a full MailScreen
// ──────────────────────────────────────────────────────────────────────

/// Drag state for interactive dock resizing via mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockDragState {
    /// No drag in progress.
    Idle,
    /// Actively dragging the dock border.
    Dragging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineViewMode {
    Events,
    Commits,
    Combined,
    #[allow(dead_code)] // Will be constructed via keybinding in a future bead.
    LogViewer,
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

impl TimelineViewMode {
    const fn next_primary(self) -> Self {
        match self {
            Self::Events => Self::Commits,
            Self::Commits => Self::Combined,
            Self::Combined | Self::LogViewer => Self::Events,
        }
    }
}

/// A full TUI screen backed by [`TimelinePane`].
///
/// This provides the "Messages" tab experience: a scrollable
/// timeline with cursor-based selection and an inspector detail pane.
pub struct TimelineScreen {
    pane: TimelinePane,
    /// Multi-selection state across visible timeline rows.
    selected_timeline_keys: SelectionState<TimelineSelectionKey>,
    /// State for virtualized list rendering (`RefCell` for interior mutability in `view()`).
    list_state: RefCell<VirtualizedListState>,
    /// Dock layout controlling inspector panel position and size.
    dock: DockLayout,
    /// Current mouse drag state for dock resizing.
    dock_drag: DockDragState,
    /// Last known content area (updated each view call) for mouse hit-testing.
    /// Uses `Cell` for interior mutability since `view()` takes `&self`.
    last_area: Cell<Rect>,
    /// Active render mode.
    view_mode: TimelineViewMode,
    /// Cached commit timeline rows for commit/combined views.
    commit_entries: Vec<CommitTimelineEntry>,
    /// Aggregated commit diagnostics and summary counters.
    commit_stats: CommitTimelineStats,
    /// Last tick when commit rows were refreshed from storage.
    last_commit_refresh_tick: u64,
    /// Receiver for background commit refresh results (non-blocking).
    commit_refresh_rx: Option<std::sync::mpsc::Receiver<CommitRefreshResult>>,
    /// Log viewer pane used when `view_mode == LogViewer`.
    log_viewer: RefCell<crate::console::LogPane>,
    /// Debounced preference persister (auto-saves dock layout to envfile).
    persister: Option<PreferencePersister>,
    /// On-disk path for persisted screen filter presets.
    filter_presets_path: PathBuf,
    /// Preset store loaded from `filter_presets_path`.
    filter_presets: ScreenFilterPresetStore,
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

impl TimelineScreen {
    fn build(dock: DockLayout, persister: Option<PreferencePersister>) -> Self {
        let console_path = console_persist_path_from_env_or_default();
        let filter_presets_path = screen_filter_presets_path(&console_path);
        let filter_presets = load_screen_filter_presets_or_default(&filter_presets_path);
        Self {
            pane: TimelinePane::new(),
            selected_timeline_keys: SelectionState::new(),
            list_state: RefCell::new(VirtualizedListState::new().with_persistence_id("timeline")),
            dock,
            dock_drag: DockDragState::Idle,
            last_area: Cell::new(Rect::new(0, 0, 0, 0)),
            view_mode: TimelineViewMode::Events,
            commit_entries: Vec::new(),
            commit_stats: CommitTimelineStats::default(),
            last_commit_refresh_tick: 0,
            commit_refresh_rx: None,
            log_viewer: RefCell::new(crate::console::LogPane::new()),
            persister,
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
    fn with_filter_presets_path_for_test(path: PathBuf) -> Self {
        let mut screen = Self::build(DockLayout::right_40(), None);
        screen.filter_presets_path = path.clone();
        screen.filter_presets = load_screen_filter_presets_or_default(&path);
        screen
    }

    /// Create with default layout (no persistence).
    #[must_use]
    pub fn new() -> Self {
        Self::build(DockLayout::right_40(), None)
    }

    /// Create with layout loaded from config and auto-persistence.
    #[must_use]
    pub fn with_config(config: &mcp_agent_mail_core::Config) -> Self {
        let prefs = TuiPreferences::from_config(config);
        Self::build(prefs.dock, Some(PreferencePersister::new(config)))
    }

    /// Sync `VirtualizedListState` with `TimelinePane` cursor.
    fn sync_list_state(&self) {
        let total = self.active_row_count();
        let cursor = self.pane.cursor().min(total.saturating_sub(1));
        let mut state = self.list_state.borrow_mut();
        state.select(if total > 0 { Some(cursor) } else { None });
    }

    /// Mark dock layout as changed (triggers debounced auto-save).
    fn dock_changed(&mut self) {
        if let Some(ref mut p) = self.persister {
            p.mark_dirty();
        }
    }

    fn active_row_count(&self) -> usize {
        match self.view_mode {
            TimelineViewMode::Events | TimelineViewMode::LogViewer => self.pane.filtered_len(),
            TimelineViewMode::Commits => self.commit_entries.len(),
            TimelineViewMode::Combined => self.pane.filtered_len() + self.commit_entries.len(),
        }
    }

    fn cursor_down(&mut self, n: usize) {
        let max = self.active_row_count().saturating_sub(1);
        self.pane.cursor = (self.pane.cursor + n).min(max);
    }

    fn cursor_up(&mut self, n: usize) {
        self.pane.cursor = self.pane.cursor.saturating_sub(n);
        self.pane.follow = false;
    }

    fn cursor_home(&mut self) {
        self.pane.cursor = 0;
        self.pane.follow = false;
    }

    fn cursor_end(&mut self) {
        self.pane.cursor = self.active_row_count().saturating_sub(1);
    }

    fn clamp_cursor_for_mode(&mut self) {
        self.pane.cursor = self
            .pane
            .cursor
            .min(self.active_row_count().saturating_sub(1));
    }

    fn visible_action_rows(&self) -> Vec<TimelineActionRow> {
        match self.view_mode {
            TimelineViewMode::Events | TimelineViewMode::LogViewer => self
                .pane
                .filtered_entries()
                .into_iter()
                .map(|entry| TimelineActionRow {
                    key: TimelineSelectionKey::for_event(entry),
                    copy_text: entry.display.summary.clone(),
                    event_kind: entry.display.kind.compact_label().to_ascii_lowercase(),
                    event_source: source_badge(entry.source).trim().to_ascii_lowercase(),
                })
                .collect(),
            TimelineViewMode::Commits => self
                .commit_entries
                .iter()
                .map(|entry| TimelineActionRow {
                    key: TimelineSelectionKey::for_commit(entry),
                    copy_text: entry.detail_summary(),
                    event_kind: "commit".to_string(),
                    event_source: "storage".to_string(),
                })
                .collect(),
            TimelineViewMode::Combined => self
                .combined_rows()
                .into_iter()
                .map(|row| match row {
                    CombinedTimelineRow::Event(entry) => TimelineActionRow {
                        key: TimelineSelectionKey::for_event(&entry),
                        copy_text: entry.display.summary.clone(),
                        event_kind: entry.display.kind.compact_label().to_ascii_lowercase(),
                        event_source: source_badge(entry.source).trim().to_ascii_lowercase(),
                    },
                    CombinedTimelineRow::Commit(entry) => TimelineActionRow {
                        key: TimelineSelectionKey::for_commit(&entry),
                        copy_text: entry.detail_summary(),
                        event_kind: "commit".to_string(),
                        event_source: "storage".to_string(),
                    },
                })
                .collect(),
        }
    }

    fn selected_rows_for_context(&self, rows: &[TimelineActionRow]) -> Vec<TimelineActionRow> {
        rows.iter()
            .filter(|row| self.selected_timeline_keys.contains(&row.key))
            .cloned()
            .collect()
    }

    fn prune_selection_to_visible(&mut self) {
        let visible: HashSet<TimelineSelectionKey> = self
            .visible_action_rows()
            .into_iter()
            .map(|row| row.key)
            .collect();
        self.selected_timeline_keys
            .retain(|key| visible.contains(key));
    }

    fn clear_timeline_selection(&mut self) {
        self.selected_timeline_keys.clear();
    }

    fn toggle_selection_for_cursor(&mut self) {
        if let Some(key) = self
            .visible_action_rows()
            .get(self.pane.cursor)
            .map(|row| row.key.clone())
        {
            self.selected_timeline_keys.toggle(key);
        }
    }

    fn select_all_visible_rows(&mut self) {
        self.selected_timeline_keys
            .select_all(self.visible_action_rows().into_iter().map(|row| row.key));
    }

    fn extend_visual_selection_to_cursor(&mut self) {
        if !self.selected_timeline_keys.visual_mode() {
            return;
        }
        if let Some(key) = self
            .visible_action_rows()
            .get(self.pane.cursor)
            .map(|row| row.key.clone())
        {
            self.selected_timeline_keys.select(key);
        }
    }

    fn refresh_commit_entries(&mut self, tick_count: u64, state: &TuiSharedState) {
        // Check for completed background refresh first.
        if let Some(ref rx) = self.commit_refresh_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.commit_entries = result.commits;
                    self.commit_stats = result.stats;
                    self.commit_refresh_rx = None;
                    if self.pane.follow {
                        self.cursor_end();
                    } else {
                        self.clamp_cursor_for_mode();
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still running in background, skip.
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Worker thread panicked or dropped; allow retry.
                    self.commit_refresh_rx = None;
                }
            }
        }

        // Debounce: skip if recently refreshed and data exists.
        if !self.commit_entries.is_empty()
            && tick_count.saturating_sub(self.last_commit_refresh_tick) < COMMIT_REFRESH_EVERY_TICKS
        {
            return;
        }
        self.last_commit_refresh_tick = tick_count;

        let cfg = state.config_snapshot();
        if cfg.storage_root.is_empty() {
            self.commit_entries.clear();
            self.commit_stats = CommitTimelineStats::default();
            self.clamp_cursor_for_mode();
            return;
        }

        let db = state.db_stats_snapshot().unwrap_or_default();
        let mut project_slugs: Vec<String> = db.projects_list.into_iter().map(|p| p.slug).collect();
        project_slugs.sort_unstable();
        project_slugs.dedup();
        if project_slugs.is_empty() {
            self.commit_entries.clear();
            self.commit_stats = CommitTimelineStats::default();
            self.clamp_cursor_for_mode();
            return;
        }

        // Spawn the heavy git work on a background thread.
        let (tx, rx) = std::sync::mpsc::channel();
        let storage_root = cfg.storage_root.clone();
        std::thread::Builder::new()
            .name("timeline-commit-refresh".to_string())
            .spawn(move || {
                let root = Path::new(&storage_root);
                let mut commits = Vec::new();
                let mut refresh_errors = 0usize;
                for slug in project_slugs {
                    match mcp_agent_mail_storage::get_timeline_commits(
                        root,
                        &slug,
                        COMMIT_LIMIT_PER_PROJECT,
                    ) {
                        Ok(rows) => {
                            commits.extend(rows.into_iter().map(|entry| {
                                CommitTimelineEntry::from_storage(slug.clone(), entry)
                            }));
                        }
                        Err(_) => {
                            refresh_errors = refresh_errors.saturating_add(1);
                        }
                    }
                }

                commits.sort_by_key(|entry| entry.timestamp_micros);
                if commits.len() > TIMELINE_CAPACITY {
                    let keep_from = commits.len() - TIMELINE_CAPACITY;
                    commits.drain(..keep_from);
                }

                let mut stats = CommitTimelineStats::from_entries(&commits, refresh_errors);
                let churn_limit = commits.len().max(COMMIT_LIMIT_PER_PROJECT);
                match mcp_agent_mail_storage::get_recent_commits_extended(root, churn_limit) {
                    Ok(churn_rows) => {
                        stats.churn_insertions = churn_rows.iter().map(|c| c.insertions).sum();
                        stats.churn_deletions = churn_rows.iter().map(|c| c.deletions).sum();
                    }
                    Err(_) => {
                        stats.refresh_errors = stats.refresh_errors.saturating_add(1);
                    }
                }

                let _ = tx.send(CommitRefreshResult { commits, stats });
            })
            .ok();
        self.commit_refresh_rx = Some(rx);
    }

    fn combined_rows(&self) -> Vec<CombinedTimelineRow> {
        let mut rows: Vec<CombinedTimelineRow> = self
            .pane
            .filtered_entries()
            .into_iter()
            .cloned()
            .map(CombinedTimelineRow::Event)
            .collect();
        rows.extend(
            self.commit_entries
                .iter()
                .cloned()
                .map(CombinedTimelineRow::Commit),
        );
        rows.sort_by_key(CombinedTimelineRow::timestamp_micros);
        rows
    }

    fn selected_combined_is_commit(&self) -> bool {
        let rows = self.combined_rows();
        matches!(
            rows.get(self.pane.cursor),
            Some(CombinedTimelineRow::Commit(_))
        )
    }

    fn selected_combined_event(&self) -> Option<MailEvent> {
        let rows = self.combined_rows();
        match rows.get(self.pane.cursor) {
            Some(CombinedTimelineRow::Event(entry)) => Some(entry.raw.clone()),
            _ => None,
        }
    }

    fn preset_names(&self) -> Vec<String> {
        self.filter_presets.list_names(TIMELINE_PRESET_SCREEN_ID)
    }

    fn persist_filter_presets(&self) {
        if let Err(err) =
            save_screen_filter_presets(&self.filter_presets_path, &self.filter_presets)
        {
            eprintln!(
                "timeline: failed to save presets to {}: {err}",
                self.filter_presets_path.display()
            );
        }
    }

    fn snapshot_filter_values(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        values.insert(
            "verbosity".to_string(),
            verbosity_token(self.pane.verbosity).to_string(),
        );
        if !self.pane.kind_filter.is_empty() {
            let mut tokens: Vec<_> = self
                .pane
                .kind_filter
                .iter()
                .map(|k| event_kind_token(*k))
                .collect();
            tokens.sort_unstable();
            values.insert("kind".to_string(), tokens.join(","));
        }
        if !self.pane.source_filter.is_empty() {
            let mut tokens: Vec<_> = self
                .pane
                .source_filter
                .iter()
                .map(|s| event_source_token(*s))
                .collect();
            tokens.sort_unstable();
            values.insert("source".to_string(), tokens.join(","));
        }
        values
    }

    fn save_named_preset(&mut self, name: String, description: Option<String>) -> bool {
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
        let values = self.snapshot_filter_values();
        self.filter_presets.upsert(
            TIMELINE_PRESET_SCREEN_ID,
            trimmed_name.to_string(),
            trimmed_description,
            values,
        );
        self.persist_filter_presets();
        true
    }

    fn apply_preset_values(&mut self, values: &BTreeMap<String, String>) {
        self.pane.kind_filter.clear();
        self.pane.source_filter.clear();
        self.pane.verbosity = values
            .get("verbosity")
            .and_then(|raw| parse_verbosity_token(raw))
            .unwrap_or_default();

        if let Some(kind_raw) = values.get("kind") {
            for token in kind_raw
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                if let Some(kind) = parse_event_kind_token(token) {
                    self.pane.kind_filter.insert(kind);
                }
            }
        }
        if let Some(source_raw) = values.get("source") {
            for token in source_raw
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                if let Some(source) = parse_event_source_token(token) {
                    self.pane.source_filter.insert(source);
                }
            }
        }
        self.clamp_cursor_for_mode();
        self.prune_selection_to_visible();
        self.sync_list_state();
    }

    fn apply_named_preset(&mut self, name: &str) -> bool {
        let Some(preset) = self
            .filter_presets
            .get(TIMELINE_PRESET_SCREEN_ID, name)
            .cloned()
        else {
            return false;
        };
        self.apply_preset_values(&preset.values);
        true
    }

    fn remove_named_preset(&mut self, name: &str) -> bool {
        let removed = self.filter_presets.remove(TIMELINE_PRESET_SCREEN_ID, name);
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
            self.save_preset_name = "timeline-preset".to_string();
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
                if self.save_named_preset(
                    self.save_preset_name.clone(),
                    Some(self.save_preset_description.clone()),
                ) {
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
            KeyCode::Char('j') | KeyCode::Down => {
                if !names.is_empty() {
                    self.load_preset_cursor = (self.load_preset_cursor + 1).min(names.len() - 1);
                }
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
}

impl Default for TimelineScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for TimelineScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        let dock_before = self.dock;
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if self.preset_dialog_mode != PresetDialogMode::None {
                    match self.preset_dialog_mode {
                        PresetDialogMode::Save => self.handle_save_dialog_key(key),
                        PresetDialogMode::Load => self.handle_load_dialog_key(key),
                        PresetDialogMode::None => {}
                    }
                    self.sync_list_state();
                    return Cmd::None;
                }

                if key.modifiers.contains(Modifiers::CTRL) {
                    match key.code {
                        KeyCode::Char('s') => {
                            self.open_save_preset_dialog();
                            self.sync_list_state();
                            return Cmd::None;
                        }
                        KeyCode::Char('l') => {
                            self.open_load_preset_dialog();
                            self.sync_list_state();
                            return Cmd::None;
                        }
                        _ => {}
                    }
                }

                match key.code {
                    // Cursor navigation
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.cursor_down(1);
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.cursor_up(1);
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::PageDown | KeyCode::Char('d') => {
                        self.cursor_down(PAGE_SIZE);
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::PageUp | KeyCode::Char('u') => {
                        self.cursor_up(PAGE_SIZE);
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        self.cursor_end();
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        self.cursor_home();
                        self.extend_visual_selection_to_cursor();
                    }
                    KeyCode::Char(' ') => self.toggle_selection_for_cursor(),
                    KeyCode::Char('A') => self.select_all_visible_rows(),
                    KeyCode::Char('C') => self.clear_timeline_selection(),

                    // Follow mode
                    KeyCode::Char('f') => {
                        self.pane.toggle_follow();
                        if self.pane.follow {
                            self.cursor_end();
                            self.extend_visual_selection_to_cursor();
                        }
                    }

                    // Capital-V cycles timeline views (Events -> Commits -> Combined).
                    KeyCode::Char('V') => {
                        self.view_mode = self.view_mode.next_primary();
                        self.clamp_cursor_for_mode();
                        self.prune_selection_to_visible();
                    }

                    // Lowercase `v` toggles visual selection mode.
                    KeyCode::Char('v') => {
                        let enabled = self.selected_timeline_keys.toggle_visual_mode();
                        if enabled {
                            self.extend_visual_selection_to_cursor();
                        }
                    }

                    // Cycle verbosity tier.
                    KeyCode::Char('Z') => {
                        self.pane.verbosity = self.pane.verbosity.next();
                        self.clamp_cursor_for_mode();
                        self.prune_selection_to_visible();
                    }

                    // Cycle kind filter
                    KeyCode::Char('t') => {
                        cycle_kind_filter(&mut self.pane.kind_filter);
                        self.clamp_cursor_for_mode();
                        self.prune_selection_to_visible();
                    }

                    // Cycle source filter
                    KeyCode::Char('s') => {
                        cycle_source_filter(&mut self.pane.source_filter);
                        self.clamp_cursor_for_mode();
                        self.prune_selection_to_visible();
                    }

                    // Clear all filters
                    KeyCode::Char('c') => {
                        self.pane.clear_filters();
                        self.clamp_cursor_for_mode();
                        self.prune_selection_to_visible();
                    }

                    // Toggle inspector panel (dock)
                    KeyCode::Char('i') => {
                        self.dock.toggle_visible();
                    }
                    KeyCode::Enter => {
                        let open_archive = match self.view_mode {
                            TimelineViewMode::Commits => !self.commit_entries.is_empty(),
                            TimelineViewMode::Combined => self.selected_combined_is_commit(),
                            _ => false,
                        };
                        if open_archive {
                            if self.dock != dock_before {
                                self.dock_changed();
                            }
                            return Cmd::Msg(MailScreenMsg::Navigate(MailScreenId::ArchiveBrowser));
                        }
                        self.dock.toggle_visible();
                    }

                    // Dock layout controls
                    KeyCode::Char(']') => self.dock.grow_dock(),
                    KeyCode::Char('[') => self.dock.shrink_dock(),
                    KeyCode::Char('}') => self.dock.cycle_position(),
                    KeyCode::Char('{') => self.dock.cycle_position_prev(),

                    // Dock ratio presets (p cycles through presets)
                    KeyCode::Char('p') => {
                        self.dock
                            .apply_preset(preset_for_ratio(self.dock.ratio).next());
                        self.dock.visible = true;
                    }

                    // Correlation link navigation (1-9 when dock is visible)
                    KeyCode::Char(c @ '1'..='9') if self.dock.visible => {
                        if self.view_mode == TimelineViewMode::Events {
                            if let Some(event) = self.pane.selected_event() {
                                let idx = (c as u8 - b'0') as usize;
                                if let Some(target) = super::inspector::resolve_link(event, idx) {
                                    // Auto-save if needed before navigating away.
                                    if self.dock != dock_before {
                                        self.dock_changed();
                                    }
                                    return Cmd::Msg(MailScreenMsg::DeepLink(target));
                                }
                            }
                        } else if self.view_mode == TimelineViewMode::Combined {
                            let idx = (c as u8 - b'0') as usize;
                            if let Some(event) = self.selected_combined_event() {
                                if let Some(target) = super::inspector::resolve_link(&event, idx) {
                                    if self.dock != dock_before {
                                        self.dock_changed();
                                    }
                                    return Cmd::Msg(MailScreenMsg::DeepLink(target));
                                }
                            }
                        }
                    }

                    _ => {}
                }
            }

            // ── Mouse events for dock border drag ──────────────────
            Event::Mouse(mouse) => {
                let area = self.last_area.get();
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.dock.hit_test_border(area, mouse.x, mouse.y) {
                            self.dock_drag = DockDragState::Dragging;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if self.dock_drag == DockDragState::Dragging {
                            self.dock.drag_to(area, mouse.x, mouse.y);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        self.dock_drag = DockDragState::Idle;
                    }
                    _ => {}
                }
            }

            _ => {}
        }
        // Auto-save dock layout when it changes.
        if self.dock != dock_before {
            self.dock_changed();
        }
        // Sync list state with pane cursor after any changes.
        self.sync_list_state();
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.pane.ingest(state);
        self.refresh_commit_entries(tick_count, state);
        self.prune_selection_to_visible();
        // Sync list state after ingesting new events.
        self.sync_list_state();
        // Flush debounced preference save.
        if let Some(ref mut p) = self.persister {
            let prefs = TuiPreferences {
                dock: self.dock,
                ..Default::default()
            };
            p.flush_if_due(&prefs);
        }
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        match target {
            DeepLinkTarget::TimelineAtTime(micros) => {
                self.pane.jump_to_time(*micros);
                self.dock.visible = true;
                true
            }
            _ => false,
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        self.last_area.set(area);
        let split = self.dock.split(area);
        let effects_enabled = state.config_snapshot().tui_effects;
        let selected_key_set: HashSet<TimelineSelectionKey> = self
            .selected_timeline_keys
            .selected_items()
            .into_iter()
            .collect();
        let selected_count = selected_key_set.len();
        let mut combined_selected_event: Option<MailEvent> = None;
        match self.view_mode {
            TimelineViewMode::Events => {
                let mut list_state = self.list_state.borrow_mut();
                render_timeline(
                    frame,
                    split.primary,
                    &self.pane,
                    self.dock,
                    &mut list_state,
                    effects_enabled,
                    &selected_key_set,
                    selected_count,
                );
            }
            TimelineViewMode::Commits => {
                let mut list_state = self.list_state.borrow_mut();
                render_commit_timeline(
                    frame,
                    split.primary,
                    &self.commit_entries,
                    &self.commit_stats,
                    self.pane.cursor,
                    self.pane.follow,
                    self.dock,
                    &mut list_state,
                    &selected_key_set,
                    selected_count,
                );
            }
            TimelineViewMode::Combined => {
                let rows = self.combined_rows();
                combined_selected_event = match rows.get(self.pane.cursor) {
                    Some(CombinedTimelineRow::Event(entry)) => Some(entry.raw.clone()),
                    _ => None,
                };
                let mut list_state = self.list_state.borrow_mut();
                render_combined_timeline(
                    frame,
                    split.primary,
                    &rows,
                    &self.commit_stats,
                    self.pane.cursor,
                    self.pane.follow,
                    self.dock,
                    &mut list_state,
                    &selected_key_set,
                    selected_count,
                );
            }
            TimelineViewMode::LogViewer => {
                let mut viewer = self.log_viewer.borrow_mut();
                render_timeline_log_viewer(
                    frame,
                    split.primary,
                    &self.pane,
                    self.dock,
                    &mut viewer,
                );
            }
        }
        if let Some(dock_area) = split.dock {
            let event_ref = if self.view_mode == TimelineViewMode::Events {
                self.pane.selected_event()
            } else {
                combined_selected_event.as_ref()
            };
            super::inspector::render_inspector(frame, dock_area, event_ref);
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
                key: "j/k",
                action: "Move cursor",
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
                key: "Space",
                action: "Toggle selected row",
            },
            HelpEntry {
                key: "v / A / C",
                action: "Visual mode, select all, clear selection",
            },
            HelpEntry {
                key: "f",
                action: "Toggle follow",
            },
            HelpEntry {
                key: "V",
                action: "Cycle Events/Commits/Combined",
            },
            HelpEntry {
                key: "Z",
                action: "Cycle verbosity tier",
            },
            HelpEntry {
                key: "t",
                action: "Cycle kind filter",
            },
            HelpEntry {
                key: "s",
                action: "Cycle source filter",
            },
            HelpEntry {
                key: "c",
                action: "Clear all filters",
            },
            HelpEntry {
                key: "Ctrl+S",
                action: "Save filter preset",
            },
            HelpEntry {
                key: "Ctrl+L",
                action: "Load preset list",
            },
            HelpEntry {
                key: "Del",
                action: "Delete selected preset (load dialog)",
            },
            HelpEntry {
                key: "i/Enter",
                action: "Toggle inspector (Enter opens archive from commit rows)",
            },
            HelpEntry {
                key: "[/]",
                action: "Shrink/grow dock",
            },
            HelpEntry {
                key: "{/}",
                action: "Cycle dock position",
            },
            HelpEntry {
                key: "p",
                action: "Cycle dock preset",
            },
            HelpEntry {
                key: "1-9",
                action: "Navigate to correlation link",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some(
            "Timeline views: V cycles modes while v controls visual selection. Space/v/A/C manage multi-select; Ctrl+S/Ctrl+L manage presets.",
        )
    }

    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        let rows = self.visible_action_rows();
        let current = rows.get(self.pane.cursor)?;
        let selected_rows = self.selected_rows_for_context(&rows);

        #[allow(clippy::cast_possible_truncation)]
        let anchor_row = (self.pane.cursor as u16).saturating_add(2);

        if selected_rows.len() > 1 {
            let copy_payload = selected_rows
                .iter()
                .map(|row| row.copy_text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let actions = timeline_batch_actions(selected_rows.len(), copy_payload);
            let context_id = format!("batch:{}", selected_rows.len());
            return Some((actions, anchor_row, context_id));
        }

        let mut actions = timeline_actions(&current.event_kind, &current.event_source);
        for entry in &mut actions {
            if entry.label == "Copy event" {
                entry.action = ActionKind::CopyToClipboard(current.copy_text.clone());
                entry.keybinding = Some("y".to_string());
            }
        }
        let context_id = match &current.key {
            TimelineSelectionKey::Event { seq } => format!("event:{seq}"),
            TimelineSelectionKey::Commit {
                project_slug,
                short_sha,
                timestamp_micros,
            } => format!("commit:{project_slug}:{short_sha}:{timestamp_micros}"),
        };
        Some((actions, anchor_row, context_id))
    }

    fn copyable_content(&self) -> Option<String> {
        let rows = self.visible_action_rows();
        let selected_rows = self.selected_rows_for_context(&rows);
        if selected_rows.len() > 1 {
            return Some(
                selected_rows
                    .iter()
                    .map(|row| row.copy_text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        rows.get(self.pane.cursor).map(|row| row.copy_text.clone())
    }

    fn title(&self) -> &'static str {
        "Event Timeline"
    }

    fn tab_label(&self) -> &'static str {
        "Timeline"
    }

    fn reset_layout(&mut self) -> bool {
        let defaults = TuiPreferences::default();
        self.dock = defaults.dock;
        if let Some(ref mut p) = self.persister {
            p.save_now(&defaults);
        }
        true
    }

    fn export_layout(&self) -> Option<std::path::PathBuf> {
        let prefs = TuiPreferences {
            dock: self.dock,
            ..Default::default()
        };
        self.persister
            .as_ref()
            .and_then(|p| p.export_json(&prefs).ok())
    }

    fn import_layout(&mut self) -> bool {
        let Some(ref p) = self.persister else {
            return false;
        };
        match p.import_json() {
            Ok(prefs) => {
                self.dock = prefs.dock;
                self.dock_changed();
                true
            }
            Err(_) => false,
        }
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.pane.selected_event()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Filter cycling
// ──────────────────────────────────────────────────────────────────────

/// Cycle kind filter: empty → Tool → Message → Http → Reservation → Health → Lifecycle → clear.
fn cycle_kind_filter(filter: &mut HashSet<MailEventKind>) {
    if filter.is_empty() {
        filter.insert(MailEventKind::ToolCallStart);
        filter.insert(MailEventKind::ToolCallEnd);
    } else if filter.contains(&MailEventKind::ToolCallEnd) {
        filter.clear();
        filter.insert(MailEventKind::MessageSent);
        filter.insert(MailEventKind::MessageReceived);
    } else if filter.contains(&MailEventKind::MessageSent) {
        filter.clear();
        filter.insert(MailEventKind::HttpRequest);
    } else if filter.contains(&MailEventKind::HttpRequest) {
        filter.clear();
        filter.insert(MailEventKind::ReservationGranted);
        filter.insert(MailEventKind::ReservationReleased);
    } else if filter.contains(&MailEventKind::ReservationGranted) {
        filter.clear();
        filter.insert(MailEventKind::HealthPulse);
    } else if filter.contains(&MailEventKind::HealthPulse) {
        filter.clear();
        filter.insert(MailEventKind::AgentRegistered);
        filter.insert(MailEventKind::ServerStarted);
        filter.insert(MailEventKind::ServerShutdown);
    } else {
        filter.clear();
    }
}

/// Cycle source filter: empty → Tooling → Http → Mail → Reservations → Lifecycle → Database → clear.
fn cycle_source_filter(filter: &mut HashSet<EventSource>) {
    if filter.is_empty() {
        filter.insert(EventSource::Tooling);
    } else if filter.contains(&EventSource::Tooling) {
        filter.clear();
        filter.insert(EventSource::Http);
    } else if filter.contains(&EventSource::Http) {
        filter.clear();
        filter.insert(EventSource::Mail);
    } else if filter.contains(&EventSource::Mail) {
        filter.clear();
        filter.insert(EventSource::Reservations);
    } else if filter.contains(&EventSource::Reservations) {
        filter.clear();
        filter.insert(EventSource::Lifecycle);
    } else if filter.contains(&EventSource::Lifecycle) {
        filter.clear();
        filter.insert(EventSource::Database);
    } else {
        filter.clear();
    }
}

const fn event_kind_token(kind: MailEventKind) -> &'static str {
    match kind {
        MailEventKind::ToolCallStart => "tool_call_start",
        MailEventKind::ToolCallEnd => "tool_call_end",
        MailEventKind::MessageSent => "message_sent",
        MailEventKind::MessageReceived => "message_received",
        MailEventKind::ReservationGranted => "reservation_granted",
        MailEventKind::ReservationReleased => "reservation_released",
        MailEventKind::AgentRegistered => "agent_registered",
        MailEventKind::HttpRequest => "http_request",
        MailEventKind::HealthPulse => "health_pulse",
        MailEventKind::ServerStarted => "server_started",
        MailEventKind::ServerShutdown => "server_shutdown",
    }
}

fn parse_event_kind_token(token: &str) -> Option<MailEventKind> {
    match token {
        "tool_call_start" => Some(MailEventKind::ToolCallStart),
        "tool_call_end" => Some(MailEventKind::ToolCallEnd),
        "message_sent" => Some(MailEventKind::MessageSent),
        "message_received" => Some(MailEventKind::MessageReceived),
        "reservation_granted" => Some(MailEventKind::ReservationGranted),
        "reservation_released" => Some(MailEventKind::ReservationReleased),
        "agent_registered" => Some(MailEventKind::AgentRegistered),
        "http_request" => Some(MailEventKind::HttpRequest),
        "health_pulse" => Some(MailEventKind::HealthPulse),
        "server_started" => Some(MailEventKind::ServerStarted),
        "server_shutdown" => Some(MailEventKind::ServerShutdown),
        _ => None,
    }
}

const fn event_source_token(source: EventSource) -> &'static str {
    match source {
        EventSource::Tooling => "tooling",
        EventSource::Http => "http",
        EventSource::Mail => "mail",
        EventSource::Reservations => "reservations",
        EventSource::Lifecycle => "lifecycle",
        EventSource::Database => "database",
        EventSource::Unknown => "unknown",
    }
}

fn parse_event_source_token(token: &str) -> Option<EventSource> {
    match token {
        "tooling" => Some(EventSource::Tooling),
        "http" => Some(EventSource::Http),
        "mail" => Some(EventSource::Mail),
        "reservations" => Some(EventSource::Reservations),
        "lifecycle" => Some(EventSource::Lifecycle),
        "database" => Some(EventSource::Database),
        "unknown" => Some(EventSource::Unknown),
        _ => None,
    }
}

const fn verbosity_token(verbosity: VerbosityTier) -> &'static str {
    match verbosity {
        VerbosityTier::Minimal => "minimal",
        VerbosityTier::Standard => "standard",
        VerbosityTier::Verbose => "verbose",
        VerbosityTier::All => "all",
    }
}

fn parse_verbosity_token(token: &str) -> Option<VerbosityTier> {
    match token {
        "minimal" => Some(VerbosityTier::Minimal),
        "standard" => Some(VerbosityTier::Standard),
        "verbose" => Some(VerbosityTier::Verbose),
        "all" => Some(VerbosityTier::All),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Find the closest preset for a given ratio (used when cycling presets).
fn preset_for_ratio(ratio: f32) -> DockPreset {
    let presets = [
        DockPreset::Compact,
        DockPreset::Third,
        DockPreset::Balanced,
        DockPreset::Half,
        DockPreset::Wide,
    ];
    let mut best = DockPreset::Balanced;
    let mut best_diff = f32::MAX;
    for p in presets {
        let diff = (p.ratio() - ratio).abs();
        if diff < best_diff {
            best = p;
            best_diff = diff;
        }
    }
    best
}

/// Source badge abbreviation.
const fn source_badge(src: EventSource) -> &'static str {
    match src {
        EventSource::Tooling => "Tool",
        EventSource::Http => "HTTP",
        EventSource::Mail => "Mail",
        EventSource::Reservations => "Resv",
        EventSource::Lifecycle => "Life",
        EventSource::Database => " DB ",
        EventSource::Unknown => " ?? ",
    }
}

fn centered_overlay_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .clamp(34, area.width.saturating_sub(2));
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
    let overlay = centered_overlay_rect(area, 64, 9);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Save Timeline Preset")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().bg(tp.bg_overlay));
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
            "Enter to save · Tab to switch field · Esc to cancel",
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
    let overlay = centered_overlay_rect(area, 64, 12);
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Load Timeline Preset")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border))
        .style(Style::default().bg(tp.bg_overlay));
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
            "No saved presets for Timeline.",
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

/// Render the timeline pane into the given area using `VirtualizedList`.
fn render_timeline(
    frame: &mut Frame<'_>,
    area: Rect,
    pane: &TimelinePane,
    dock: DockLayout,
    list_state: &mut VirtualizedListState,
    effects_enabled: bool,
    selected_keys: &HashSet<TimelineSelectionKey>,
    selected_count: usize,
) {
    let inner_height = area.height.saturating_sub(2) as usize; // borders
    if inner_height == 0 {
        return;
    }

    // Collect filtered entries (clones for VirtualizedList).
    let mut filtered: Vec<TimelineEntry> = pane.filtered_entries().into_iter().cloned().collect();
    let shimmer_progresses = timeline_shimmer_progresses(&filtered, effects_enabled);
    for (idx, shimmer_progress) in shimmer_progresses.into_iter().enumerate() {
        if let Some(progress) = shimmer_progress {
            filtered[idx].display.summary = shimmerize_plain_text(
                &filtered[idx].display.summary,
                progress,
                SHIMMER_HIGHLIGHT_WIDTH,
            );
        }
    }
    for entry in &mut filtered {
        let marker = if selected_keys.contains(&TimelineSelectionKey::for_event(entry)) {
            "[x]"
        } else {
            "[ ]"
        };
        entry.display.summary = format!("{marker} {}", entry.display.summary);
    }
    let total = filtered.len();
    let cursor = pane.cursor.min(total.saturating_sub(1));

    // Title with position info.
    let pos = if total == 0 {
        "empty".to_string()
    } else {
        format!("{}/{total}", cursor + 1)
    };
    let follow_tag = if pane.follow { " [FOLLOW]" } else { "" };
    let verbosity_tag = format!(" [{}]", pane.verbosity.label());
    let filter_tag = build_filter_tag(&pane.kind_filter, &pane.source_filter);
    let dock_tag = if dock.visible {
        format!(" [{}]", dock.state_label())
    } else {
        String::new()
    };
    let selected_tag = if selected_count > 0 {
        format!(" [selected:{selected_count}]")
    } else {
        String::new()
    };
    let title =
        format!("Timeline ({pos}){follow_tag}{verbosity_tag}{filter_tag}{selected_tag}{dock_tag}",);

    // Render block/border first.
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner_area = block.inner(area);
    block.render(area, frame);

    // Update list state selection to match pane cursor.
    list_state.select(if total > 0 { Some(cursor) } else { None });

    // Render VirtualizedList into inner area.
    let list = VirtualizedList::new(&filtered)
        .style(Style::default())
        .highlight_style(
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold(),
        )
        .show_scrollbar(true);
    StatefulWidget::render(&list, inner_area, frame, list_state);
}

fn render_commit_timeline(
    frame: &mut Frame<'_>,
    area: Rect,
    commits: &[CommitTimelineEntry],
    stats: &CommitTimelineStats,
    cursor: usize,
    follow: bool,
    dock: DockLayout,
    list_state: &mut VirtualizedListState,
    selected_keys: &HashSet<TimelineSelectionKey>,
    selected_count: usize,
) {
    let mut render_commits: Vec<CommitTimelineEntry> = commits.to_vec();
    for row in &mut render_commits {
        let marker = if selected_keys.contains(&TimelineSelectionKey::for_commit(row)) {
            "[x]"
        } else {
            "[ ]"
        };
        row.subject = format!("{marker} {}", row.subject);
    }
    let pos = if commits.is_empty() {
        "empty".to_string()
    } else {
        format!(
            "{}/{}",
            cursor.min(commits.len().saturating_sub(1)) + 1,
            commits.len()
        )
    };
    let follow_tag = if follow { " [FOLLOW]" } else { "" };
    let dock_tag = if dock.visible {
        format!(" [{}]", dock.state_label())
    } else {
        String::new()
    };
    let selected_tag = if selected_count > 0 {
        format!(" [selected:{selected_count}]")
    } else {
        String::new()
    };
    let title = format!("Timeline Commits ({pos}){follow_tag}{selected_tag}{dock_tag}");
    let summary = stats.summary_line();

    render_virtualized_rows(
        frame,
        area,
        &render_commits,
        cursor,
        &title,
        Some(&summary),
        list_state,
    );
}

fn render_combined_timeline(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[CombinedTimelineRow],
    stats: &CommitTimelineStats,
    cursor: usize,
    follow: bool,
    dock: DockLayout,
    list_state: &mut VirtualizedListState,
    selected_keys: &HashSet<TimelineSelectionKey>,
    selected_count: usize,
) {
    let mut render_rows: Vec<CombinedTimelineRow> = rows.to_vec();
    for row in &mut render_rows {
        match row {
            CombinedTimelineRow::Event(entry) => {
                let marker = if selected_keys.contains(&TimelineSelectionKey::for_event(entry)) {
                    "[x]"
                } else {
                    "[ ]"
                };
                entry.display.summary = format!("{marker} {}", entry.display.summary);
            }
            CombinedTimelineRow::Commit(entry) => {
                let marker = if selected_keys.contains(&TimelineSelectionKey::for_commit(entry)) {
                    "[x]"
                } else {
                    "[ ]"
                };
                entry.subject = format!("{marker} {}", entry.subject);
            }
        }
    }
    let pos = if rows.is_empty() {
        "empty".to_string()
    } else {
        format!(
            "{}/{}",
            cursor.min(rows.len().saturating_sub(1)) + 1,
            rows.len()
        )
    };
    let follow_tag = if follow { " [FOLLOW]" } else { "" };
    let dock_tag = if dock.visible {
        format!(" [{}]", dock.state_label())
    } else {
        String::new()
    };
    let selected_tag = if selected_count > 0 {
        format!(" [selected:{selected_count}]")
    } else {
        String::new()
    };
    let title = format!("Timeline Combined ({pos}){follow_tag}{selected_tag}{dock_tag}");
    let summary = stats.summary_line();

    render_virtualized_rows(
        frame,
        area,
        &render_rows,
        cursor,
        &title,
        Some(&summary),
        list_state,
    );
}

fn render_virtualized_rows<T: RenderItem>(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[T],
    cursor: usize,
    title: &str,
    summary_line: Option<&str>,
    list_state: &mut VirtualizedListState,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner_area = block.inner(area);
    block.render(area, frame);

    let mut list_area = inner_area;
    if let Some(summary) = summary_line.filter(|line| !line.is_empty()) {
        if inner_area.height == 0 {
            return;
        }
        let summary_area = Rect::new(inner_area.x, inner_area.y, inner_area.width, 1);
        let summary_style = crate::tui_theme::text_meta(&tp);
        Paragraph::new(Text::from_line(Line::from(Span::styled(
            summary,
            summary_style,
        ))))
        .render(summary_area, frame);
        list_area = Rect::new(
            inner_area.x,
            inner_area.y.saturating_add(1),
            inner_area.width,
            inner_area.height.saturating_sub(1),
        );
    }

    if list_area.height == 0 {
        return;
    }

    list_state.select(if rows.is_empty() {
        None
    } else {
        Some(cursor.min(rows.len().saturating_sub(1)))
    });

    let list = VirtualizedList::new(rows)
        .style(Style::default())
        .highlight_style(
            Style::default()
                .fg(tp.selection_fg)
                .bg(tp.selection_bg)
                .bold(),
        )
        .show_scrollbar(true);
    StatefulWidget::render(&list, list_area, frame, list_state);
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
    let age = now_micros - timestamp_micros;
    if !(0..=SHIMMER_WINDOW_MICROS).contains(&age) {
        return None;
    }
    Some((age as f64 / SHIMMER_WINDOW_MICROS as f64).clamp(0.0, 1.0))
}

fn timeline_shimmer_progresses(
    entries: &[TimelineEntry],
    effects_enabled: bool,
) -> Vec<Option<f64>> {
    let mut progresses = vec![None; entries.len()];
    if !effects_enabled || entries.is_empty() {
        return progresses;
    }
    let Some(now_micros) = unix_epoch_micros_now() else {
        return progresses;
    };
    let mut shimmer_count = 0usize;
    for idx in (0..entries.len()).rev() {
        if shimmer_count >= SHIMMER_MAX_ROWS {
            break;
        }
        if let Some(progress) =
            shimmer_progress_for_timestamp(now_micros, entries[idx].timestamp_micros)
        {
            progresses[idx] = Some(progress);
            shimmer_count += 1;
        }
    }
    progresses
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn shimmer_window_indices(
    len_chars: usize,
    progress: f64,
    width_chars: usize,
) -> Option<(usize, usize)> {
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

fn shimmerize_plain_text(text: &str, progress: f64, width_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let Some((start, end)) = shimmer_window_indices(chars.len(), progress, width_chars) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len() + 2);
    for (idx, ch) in chars.into_iter().enumerate() {
        if idx == start {
            out.push('[');
        }
        if idx >= start && idx < end {
            if ch.is_ascii_lowercase() {
                out.push(ch.to_ascii_uppercase());
            } else if ch == ' ' {
                out.push('·');
            } else {
                out.push(ch);
            }
        } else {
            out.push(ch);
        }
        if idx + 1 == end {
            out.push(']');
        }
    }
    out
}

fn render_timeline_log_viewer(
    frame: &mut Frame<'_>,
    area: Rect,
    pane: &TimelinePane,
    dock: DockLayout,
    viewer: &mut crate::console::LogPane,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }

    let filtered: Vec<TimelineEntry> = pane.filtered_entries().into_iter().cloned().collect();
    let total = filtered.len();
    let cursor = pane.cursor.min(total.saturating_sub(1));
    let pos = if total == 0 {
        "empty".to_string()
    } else {
        format!("{}/{total}", cursor + 1)
    };
    let follow_tag = if pane.follow { " [FOLLOW]" } else { "" };
    let verbosity_tag = format!(" [{}]", pane.verbosity.label());
    let filter_tag = build_filter_tag(&pane.kind_filter, &pane.source_filter);
    let dock_tag = if dock.visible {
        format!(" [{}]", dock.state_label())
    } else {
        String::new()
    };
    let title =
        format!("Timeline LogViewer ({pos}){follow_tag}{verbosity_tag}{filter_tag}{dock_tag}",);

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.height == 0 {
        return;
    }

    viewer.clear();
    viewer.push_many(filtered.iter().map(|entry| {
        format!(
            "{:>6} {} {:<3} [{:<4}] {:<10} {}",
            entry.seq,
            entry.display.timestamp,
            entry.severity.badge(),
            source_badge(entry.source),
            entry.display.kind.compact_label(),
            entry.display.summary
        )
    }));

    viewer.scroll_to_bottom();
    if !pane.follow && total > 0 {
        let offset = total.saturating_sub(1).saturating_sub(cursor);
        if offset > 0 {
            viewer.scroll_up(offset);
        }
    }
    viewer.render(inner, frame);
}

/// Compute the viewport [start, end) to keep cursor visible.
/// Note: `VirtualizedList` now handles this internally, but kept for tests.
#[allow(dead_code)]
fn viewport_range(total: usize, height: usize, cursor: usize) -> (usize, usize) {
    if total <= height {
        return (0, total);
    }
    // Keep cursor roughly centered, but clamp to bounds.
    let half = height / 2;
    let ideal_start = cursor.saturating_sub(half);
    let start = ideal_start.min(total - height);
    let end = (start + height).min(total);
    (start, end)
}

/// Build a compact filter tag string.
fn build_filter_tag(
    kind_filter: &HashSet<MailEventKind>,
    source_filter: &HashSet<EventSource>,
) -> String {
    let mut parts = Vec::new();
    if !kind_filter.is_empty() {
        let mut kinds: Vec<_> = kind_filter.iter().map(|k| k.compact_label()).collect();
        kinds.sort_unstable();
        parts.push(format!("kind:{}", kinds.join(",")));
    }
    if !source_filter.is_empty() {
        let mut sources: Vec<_> = source_filter
            .iter()
            .map(|s| source_badge(*s).trim())
            .collect();
        sources.sort_unstable();
        parts.push(format!("src:{}", sources.join(",")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join(" "))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_layout::DockPosition;
    use ftui_harness::buffer_to_text;

    fn make_event(_seq: u64) -> MailEvent {
        MailEvent::http_request("GET", "/test", 200, 10, "127.0.0.1")
    }

    /// Create a pane with All verbosity so Debug-level test entries are visible.
    fn test_pane() -> TimelinePane {
        let mut pane = TimelinePane::new();
        pane.verbosity = VerbosityTier::All;
        pane
    }

    fn push_event_entry(pane: &mut TimelinePane, seq: u64, event: MailEvent) {
        let ts = i64::try_from(seq).expect("test seq fits i64") * 1_000_000;
        pane.entries.push(TimelineEntry {
            display: format_event(&event),
            seq,
            timestamp_micros: ts,
            source: event.source(),
            severity: event.severity(),
            raw: event,
        });
    }

    fn make_commit_entry(
        project_slug: &str,
        author: &str,
        commit_type: &str,
        timestamp_micros: i64,
    ) -> CommitTimelineEntry {
        CommitTimelineEntry {
            project_slug: project_slug.to_string(),
            short_sha: "deadbeef".to_string(),
            timestamp_micros,
            timestamp_label: "00:00:02.000".to_string(),
            subject: "mail: test".to_string(),
            commit_type: commit_type.to_string(),
            sender: Some(author.to_string()),
            recipients: vec!["SilentOwl".to_string()],
            author: author.to_string(),
        }
    }

    fn seed_log_filter_fixture(pane: &mut TimelinePane) {
        push_event_entry(
            pane,
            1,
            MailEvent::tool_call_start("fetch_inbox", serde_json::Value::Null, None, None),
        );
        push_event_entry(
            pane,
            2,
            MailEvent::tool_call_end("fetch_inbox", 12, None, 0, 0.0, vec![], None, None),
        );
        push_event_entry(
            pane,
            3,
            MailEvent::message_sent(
                1,
                "GoldFox",
                vec!["SilverWolf".to_string()],
                "hello",
                "t",
                "p",
            ),
        );
        push_event_entry(
            pane,
            4,
            MailEvent::message_received(
                2,
                "SilverWolf",
                vec!["GoldFox".to_string()],
                "re: hello",
                "t",
                "p",
            ),
        );
        push_event_entry(
            pane,
            5,
            MailEvent::reservation_granted("GoldFox", vec!["src/**".to_string()], true, 120, "p"),
        );
        push_event_entry(
            pane,
            6,
            MailEvent::reservation_released("GoldFox", vec!["src/**".to_string()], "p"),
        );
        push_event_entry(
            pane,
            7,
            MailEvent::agent_registered("GoldFox", "codex-cli", "gpt-5-codex", "p"),
        );
        push_event_entry(
            pane,
            8,
            MailEvent::http_request("GET", "/mcp", 200, 3, "127.0.0.1"),
        );
        push_event_entry(
            pane,
            9,
            MailEvent::health_pulse(crate::tui_events::DbStatSnapshot::default()),
        );
        push_event_entry(
            pane,
            10,
            MailEvent::server_started("http://127.0.0.1:8765", "cfg"),
        );
        push_event_entry(pane, 11, MailEvent::server_shutdown());
    }

    #[test]
    fn new_pane_is_empty() {
        let pane = TimelinePane::new();
        assert_eq!(pane.entries.len(), 0);
        assert_eq!(pane.cursor, 0);
        assert!(!pane.follow);
        assert!(pane.selected_event().is_none());
    }

    #[test]
    fn cursor_navigation() {
        let mut pane = test_pane();
        // Manually push entries.
        for i in 0..10 {
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i64::try_from(i)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000),
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        assert_eq!(pane.cursor, 0);

        pane.cursor_down(3);
        assert_eq!(pane.cursor, 3);

        pane.cursor_up(1);
        assert_eq!(pane.cursor, 2);

        pane.cursor_end();
        assert_eq!(pane.cursor, 9);

        pane.cursor_home();
        assert_eq!(pane.cursor, 0);
    }

    #[test]
    fn cursor_clamps_at_bounds() {
        let mut pane = test_pane();
        for i in 0..5 {
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i64::try_from(i)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000),
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }

        pane.cursor_down(100);
        assert_eq!(pane.cursor, 4);

        pane.cursor_up(100);
        assert_eq!(pane.cursor, 0);
    }

    #[test]
    fn follow_mode_tracks_end() {
        let mut pane = test_pane();
        pane.follow = true;

        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let _ = state.push_event(MailEvent::http_request("GET", "/a", 200, 5, "127.0.0.1"));
        pane.ingest(&state);
        assert_eq!(pane.cursor, 0); // First entry, idx 0.

        let _ = state.push_event(MailEvent::http_request("POST", "/b", 201, 3, "127.0.0.1"));
        pane.ingest(&state);
        assert_eq!(pane.cursor, 1); // Followed to end.
    }

    #[test]
    fn kind_filter_restricts_view() {
        let mut pane = test_pane();
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: crate::tui_events::EventSeverity::Debug,
                seq: 1,
                timestamp_micros: 1_000_000,
                timestamp: "00:00:00.000".to_string(),
                icon: '↔',
                summary: "GET /x".to_string(),
            },
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: make_event(1),
        });
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::MessageSent,
                severity: crate::tui_events::EventSeverity::Info,
                seq: 2,
                timestamp_micros: 2_000_000,
                timestamp: "00:00:01.000".to_string(),
                icon: '✉',
                summary: "msg sent".to_string(),
            },
            seq: 2,
            timestamp_micros: 2_000_000,
            source: EventSource::Mail,
            severity: EventSeverity::Info,
            raw: make_event(2),
        });

        assert_eq!(pane.filtered_len(), 2);

        pane.toggle_kind_filter(MailEventKind::HttpRequest);
        assert_eq!(pane.filtered_len(), 1);

        pane.toggle_kind_filter(MailEventKind::HttpRequest);
        assert_eq!(pane.filtered_len(), 2);
    }

    #[test]
    fn source_filter_restricts_view() {
        let mut pane = test_pane();
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: crate::tui_events::EventSeverity::Debug,
                seq: 1,
                timestamp_micros: 1_000_000,
                timestamp: "00:00:00.000".to_string(),
                icon: '↔',
                summary: "GET /x".to_string(),
            },
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: make_event(1),
        });
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::ToolCallEnd,
                severity: crate::tui_events::EventSeverity::Debug,
                seq: 2,
                timestamp_micros: 2_000_000,
                timestamp: "00:00:01.000".to_string(),
                icon: '⚙',
                summary: "tool done".to_string(),
            },
            seq: 2,
            timestamp_micros: 2_000_000,
            source: EventSource::Tooling,
            severity: EventSeverity::Debug,
            raw: make_event(2),
        });

        pane.toggle_source_filter(EventSource::Http);
        assert_eq!(pane.filtered_len(), 1);

        pane.clear_filters();
        // `clear_filters` also resets verbosity to default (Standard),
        // which hides Debug rows in this fixture.
        assert_eq!(pane.filtered_len(), 0);
        assert_eq!(pane.verbosity, VerbosityTier::default());
    }

    #[test]
    fn jump_to_time_positions_cursor() {
        let mut pane = test_pane();
        for i in 0..100 {
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i64::try_from(i)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000),
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }

        pane.jump_to_time(50_000_000); // 50 seconds.
        assert_eq!(pane.cursor, 50);
        assert!(!pane.follow);

        pane.jump_to_time(999_000_000); // Beyond last.
        assert_eq!(pane.cursor, 99);
    }

    #[test]
    fn viewport_range_small_list() {
        let (start, end) = viewport_range(5, 20, 3);
        assert_eq!(start, 0);
        assert_eq!(end, 5);
    }

    #[test]
    fn viewport_range_keeps_cursor_visible() {
        // 100 entries, 20 visible, cursor at 80.
        let (start, end) = viewport_range(100, 20, 80);
        assert!(start <= 80);
        assert!(end > 80);
        assert_eq!(end - start, 20);
    }

    #[test]
    fn viewport_range_cursor_at_start() {
        let (start, end) = viewport_range(100, 20, 0);
        assert_eq!(start, 0);
        assert_eq!(end, 20);
    }

    #[test]
    fn viewport_range_cursor_at_end() {
        let (start, end) = viewport_range(100, 20, 99);
        assert_eq!(start, 80);
        assert_eq!(end, 100);
    }

    #[test]
    fn source_badge_values() {
        assert_eq!(source_badge(EventSource::Tooling), "Tool");
        assert_eq!(source_badge(EventSource::Http), "HTTP");
        assert_eq!(source_badge(EventSource::Mail), "Mail");
        assert_eq!(source_badge(EventSource::Reservations), "Resv");
        assert_eq!(source_badge(EventSource::Lifecycle), "Life");
        assert_eq!(source_badge(EventSource::Database), " DB ");
        assert_eq!(source_badge(EventSource::Unknown), " ?? ");
    }

    #[test]
    fn build_filter_tag_empty() {
        let tag = build_filter_tag(&HashSet::new(), &HashSet::new());
        assert!(tag.is_empty());
    }

    #[test]
    fn build_filter_tag_with_kind() {
        let mut kinds = HashSet::new();
        kinds.insert(MailEventKind::HttpRequest);
        let tag = build_filter_tag(&kinds, &HashSet::new());
        assert!(tag.contains("kind:"));
        assert!(tag.contains("HTTP"));
    }

    #[test]
    fn cycle_kind_filter_round_trips() {
        let mut filter = HashSet::new();
        // empty → Tool
        cycle_kind_filter(&mut filter);
        assert!(filter.contains(&MailEventKind::ToolCallEnd));
        // Tool → Message
        cycle_kind_filter(&mut filter);
        assert!(filter.contains(&MailEventKind::MessageSent));
        // Message → Http
        cycle_kind_filter(&mut filter);
        assert!(filter.contains(&MailEventKind::HttpRequest));
        // Http → Reservation
        cycle_kind_filter(&mut filter);
        assert!(filter.contains(&MailEventKind::ReservationGranted));
        // Reservation → Health
        cycle_kind_filter(&mut filter);
        assert!(filter.contains(&MailEventKind::HealthPulse));
        // Health → Lifecycle
        cycle_kind_filter(&mut filter);
        assert!(filter.contains(&MailEventKind::AgentRegistered));
        // Lifecycle → clear
        cycle_kind_filter(&mut filter);
        assert!(filter.is_empty());
    }

    #[test]
    fn filter_preset_all_shows_all_events() {
        let mut pane = test_pane();
        seed_log_filter_fixture(&mut pane);

        assert_eq!(pane.filtered_len(), 11);
    }

    #[test]
    fn filter_preset_messages_shows_only_message_events() {
        let mut pane = test_pane();
        seed_log_filter_fixture(&mut pane);
        pane.kind_filter.insert(MailEventKind::MessageSent);
        pane.kind_filter.insert(MailEventKind::MessageReceived);

        let filtered = pane.filtered_entries();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|entry| matches!(
            entry.display.kind,
            MailEventKind::MessageSent | MailEventKind::MessageReceived
        )));
    }

    #[test]
    fn filter_preset_tools_shows_only_tool_events() {
        let mut pane = test_pane();
        seed_log_filter_fixture(&mut pane);
        pane.kind_filter.insert(MailEventKind::ToolCallStart);
        pane.kind_filter.insert(MailEventKind::ToolCallEnd);

        let filtered = pane.filtered_entries();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|entry| matches!(
            entry.display.kind,
            MailEventKind::ToolCallStart | MailEventKind::ToolCallEnd
        )));
    }

    #[test]
    fn filter_preset_reservations_shows_only_reservation_events() {
        let mut pane = test_pane();
        seed_log_filter_fixture(&mut pane);
        pane.kind_filter.insert(MailEventKind::ReservationGranted);
        pane.kind_filter.insert(MailEventKind::ReservationReleased);

        let filtered = pane.filtered_entries();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|entry| matches!(
            entry.display.kind,
            MailEventKind::ReservationGranted | MailEventKind::ReservationReleased
        )));
    }

    #[test]
    fn filter_preset_health_shows_health_and_lifecycle_events() {
        let mut pane = test_pane();
        seed_log_filter_fixture(&mut pane);
        pane.kind_filter.insert(MailEventKind::HealthPulse);
        pane.kind_filter.insert(MailEventKind::ServerStarted);
        pane.kind_filter.insert(MailEventKind::ServerShutdown);

        let filtered = pane.filtered_entries();
        assert_eq!(filtered.len(), 3);
        assert!(filtered.iter().all(|entry| matches!(
            entry.display.kind,
            MailEventKind::HealthPulse
                | MailEventKind::ServerStarted
                | MailEventKind::ServerShutdown
        )));
    }

    #[test]
    fn cycle_source_filter_round_trips() {
        let mut filter = HashSet::new();
        cycle_source_filter(&mut filter);
        assert!(filter.contains(&EventSource::Tooling));
        cycle_source_filter(&mut filter);
        assert!(filter.contains(&EventSource::Http));
        cycle_source_filter(&mut filter);
        assert!(filter.contains(&EventSource::Mail));
        cycle_source_filter(&mut filter);
        assert!(filter.contains(&EventSource::Reservations));
        cycle_source_filter(&mut filter);
        assert!(filter.contains(&EventSource::Lifecycle));
        cycle_source_filter(&mut filter);
        assert!(filter.contains(&EventSource::Database));
        cycle_source_filter(&mut filter);
        assert!(filter.is_empty());
    }

    #[test]
    fn render_timeline_no_panic_empty() {
        let pane = TimelinePane::new();
        let dock = DockLayout::right_40();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let mut list_state = VirtualizedListState::new();
        render_timeline(
            &mut frame,
            Rect::new(0, 0, 80, 24),
            &pane,
            dock,
            &mut list_state,
            false,
            &HashSet::new(),
            0,
        );
    }

    #[test]
    fn render_timeline_no_panic_with_entries() {
        let mut pane = TimelinePane::new();
        for i in 0..50 {
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i64::try_from(i)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000),
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        pane.cursor = 25;

        let dock = DockLayout::right_40();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        let mut list_state = VirtualizedListState::new();
        render_timeline(
            &mut frame,
            Rect::new(0, 0, 120, 30),
            &pane,
            dock,
            &mut list_state,
            false,
            &HashSet::new(),
            0,
        );
    }

    #[test]
    fn render_timeline_minimum_size() {
        let pane = TimelinePane::new();
        let dock = DockLayout::right_40();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(40, 5, &mut pool);
        let mut list_state = VirtualizedListState::new();
        render_timeline(
            &mut frame,
            Rect::new(0, 0, 40, 5),
            &pane,
            dock,
            &mut list_state,
            false,
            &HashSet::new(),
            0,
        );
    }

    #[test]
    fn trim_to_capacity() {
        let mut pane = TimelinePane::new();
        // Push more than TIMELINE_CAPACITY entries.
        for i in 0..(TIMELINE_CAPACITY + 100) {
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i as u64)),
                seq: i as u64,
                timestamp_micros: i64::try_from(i)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000),
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i as u64),
            });
        }
        pane.cursor = TIMELINE_CAPACITY + 50;

        // Simulate trim logic.
        if pane.entries.len() > TIMELINE_CAPACITY {
            let excess = pane.entries.len() - TIMELINE_CAPACITY;
            pane.entries.drain(..excess);
            pane.cursor = pane.cursor.saturating_sub(excess);
        }

        assert_eq!(pane.entries.len(), TIMELINE_CAPACITY);
        assert!(pane.cursor < TIMELINE_CAPACITY);
    }

    #[test]
    fn selected_event_returns_correct_entry() {
        let mut pane = test_pane();
        let event = MailEvent::http_request("DELETE", "/api/test", 204, 42, "127.0.0.1");
        pane.entries.push(TimelineEntry {
            display: format_event(&event),
            seq: 99,
            timestamp_micros: 99_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: event,
        });
        pane.cursor = 0;

        let selected = pane.selected_event().unwrap();
        assert_eq!(selected.kind(), MailEventKind::HttpRequest);
    }

    #[test]
    fn page_navigation_via_screen() {
        let mut screen = TimelineScreen::new();
        screen.pane.verbosity = VerbosityTier::All;
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Push 50 events (HTTP 200 = Debug severity).
        for _ in 0..50 {
            let _ = state.push_event(MailEvent::http_request("GET", "/x", 200, 1, "127.0.0.1"));
        }
        screen.pane.ingest(&state);

        // Page down.
        let key_event = Event::Key(ftui::KeyEvent::new(KeyCode::Char('d')));
        screen.update(&key_event, &state);
        assert_eq!(screen.pane.cursor, PAGE_SIZE);

        // Page up.
        let key_event = Event::Key(ftui::KeyEvent::new(KeyCode::Char('u')));
        screen.update(&key_event, &state);
        assert_eq!(screen.pane.cursor, 0);
    }

    #[test]
    fn deep_link_timeline_at_time() {
        let mut screen = TimelineScreen::new();
        screen.pane.verbosity = VerbosityTier::All;
        // Populate with events spanning 0..100 seconds.
        for i in 0..100u64 {
            screen.pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i64::try_from(i)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000_000),
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        assert_eq!(screen.pane.cursor, 0);

        // Deep-link to 50 seconds
        let target = DeepLinkTarget::TimelineAtTime(50_000_000);
        let handled = screen.receive_deep_link(&target);
        assert!(handled);
        assert_eq!(screen.pane.cursor, 50);
        assert!(screen.dock.visible);
    }

    #[test]
    fn deep_link_unrelated_returns_false() {
        let mut screen = TimelineScreen::new();
        let target = DeepLinkTarget::MessageById(42);
        assert!(!screen.receive_deep_link(&target));
    }

    #[test]
    fn default_verbosity_is_standard() {
        let pane = TimelinePane::new();
        assert_eq!(pane.verbosity, VerbosityTier::Standard);
    }

    #[test]
    fn verbosity_filters_by_severity() {
        let mut pane = test_pane();
        // Add entries at different severity levels
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HealthPulse,
                severity: EventSeverity::Trace,
                seq: 1,
                timestamp_micros: 1_000_000,
                timestamp: "00:00:00.000".to_string(),
                icon: '♥',
                summary: "pulse".to_string(),
            },
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Database,
            severity: EventSeverity::Trace,
            raw: MailEvent::health_pulse(crate::tui_events::DbStatSnapshot::default()),
        });
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: EventSeverity::Debug,
                seq: 2,
                timestamp_micros: 2_000_000,
                timestamp: "00:00:00.001".to_string(),
                icon: '↔',
                summary: "GET / 200".to_string(),
            },
            seq: 2,
            timestamp_micros: 2_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: make_event(2),
        });
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::MessageSent,
                severity: EventSeverity::Info,
                seq: 3,
                timestamp_micros: 3_000_000,
                timestamp: "00:00:00.002".to_string(),
                icon: '✉',
                summary: "msg".to_string(),
            },
            seq: 3,
            timestamp_micros: 3_000_000,
            source: EventSource::Mail,
            severity: EventSeverity::Info,
            raw: MailEvent::message_sent(1, "A", vec![], "s", "t", "p"),
        });
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: EventSeverity::Error,
                seq: 4,
                timestamp_micros: 4_000_000,
                timestamp: "00:00:00.003".to_string(),
                icon: '↔',
                summary: "POST / 500".to_string(),
            },
            seq: 4,
            timestamp_micros: 4_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Error,
            raw: MailEvent::http_request("POST", "/", 500, 10, "127.0.0.1"),
        });

        // All: everything visible
        assert_eq!(pane.verbosity, VerbosityTier::All);
        assert_eq!(pane.filtered_len(), 4);

        // Verbose: Trace hidden
        pane.verbosity = VerbosityTier::Verbose;
        assert_eq!(pane.filtered_len(), 3);

        // Standard: Trace + Debug hidden
        pane.verbosity = VerbosityTier::Standard;
        assert_eq!(pane.filtered_len(), 2);

        // Minimal: only Warn + Error
        pane.verbosity = VerbosityTier::Minimal;
        assert_eq!(pane.filtered_len(), 1);
    }

    #[test]
    fn verbosity_cycles_on_z_key() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        assert_eq!(screen.pane.verbosity, VerbosityTier::Standard);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('Z')));
        screen.update(&key, &state);
        assert_eq!(screen.pane.verbosity, VerbosityTier::Verbose);

        screen.update(&key, &state);
        assert_eq!(screen.pane.verbosity, VerbosityTier::All);

        screen.update(&key, &state);
        assert_eq!(screen.pane.verbosity, VerbosityTier::Minimal);

        screen.update(&key, &state);
        assert_eq!(screen.pane.verbosity, VerbosityTier::Standard);
    }

    #[test]
    fn capital_v_cycles_events_commits_combined_views() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        assert_eq!(screen.view_mode, TimelineViewMode::Events);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('V')));
        screen.update(&key, &state);
        assert_eq!(screen.view_mode, TimelineViewMode::Commits);

        screen.update(&key, &state);
        assert_eq!(screen.view_mode, TimelineViewMode::Combined);

        screen.update(&key, &state);
        assert_eq!(screen.view_mode, TimelineViewMode::Events);
    }

    #[test]
    fn lowercase_v_toggles_visual_selection_mode() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        push_event_entry(
            &mut screen.pane,
            1,
            MailEvent::http_request("GET", "/one", 200, 1, "127.0.0.1"),
        );
        push_event_entry(
            &mut screen.pane,
            2,
            MailEvent::http_request("GET", "/two", 200, 1, "127.0.0.1"),
        );
        screen.pane.verbosity = VerbosityTier::All;
        screen.pane.cursor = 0;

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('v')));
        screen.update(&key, &state);
        assert!(screen.selected_timeline_keys.visual_mode());
        assert_eq!(screen.selected_timeline_keys.len(), 1);
    }

    #[test]
    fn visual_mode_extends_selection_when_cursor_moves() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        push_event_entry(
            &mut screen.pane,
            1,
            MailEvent::http_request("GET", "/one", 200, 1, "127.0.0.1"),
        );
        push_event_entry(
            &mut screen.pane,
            2,
            MailEvent::http_request("GET", "/two", 200, 1, "127.0.0.1"),
        );
        screen.pane.verbosity = VerbosityTier::All;
        screen.pane.cursor = 0;

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('v'))), &state);
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Down)), &state);

        assert_eq!(screen.selected_timeline_keys.len(), 2);
    }

    #[test]
    fn shift_a_and_shift_c_manage_timeline_selection() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        push_event_entry(
            &mut screen.pane,
            1,
            MailEvent::http_request("GET", "/one", 200, 1, "127.0.0.1"),
        );
        push_event_entry(
            &mut screen.pane,
            2,
            MailEvent::http_request("GET", "/two", 200, 1, "127.0.0.1"),
        );
        screen.pane.verbosity = VerbosityTier::All;
        screen.pane.cursor = 0;

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('A'))), &state);
        assert_eq!(screen.selected_timeline_keys.len(), 2);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('C'))), &state);
        assert!(screen.selected_timeline_keys.is_empty());
        assert!(!screen.selected_timeline_keys.visual_mode());
    }

    #[test]
    fn filters_persist_across_view_cycle() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        screen.pane.kind_filter.insert(MailEventKind::HttpRequest);
        screen.pane.source_filter.insert(EventSource::Http);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('V')));
        screen.update(&key, &state);
        assert_eq!(screen.view_mode, TimelineViewMode::Commits);
        assert!(
            screen
                .pane
                .kind_filter
                .contains(&MailEventKind::HttpRequest)
        );
        assert!(screen.pane.source_filter.contains(&EventSource::Http));

        screen.update(&key, &state);
        assert_eq!(screen.view_mode, TimelineViewMode::Combined);
        assert!(
            screen
                .pane
                .kind_filter
                .contains(&MailEventKind::HttpRequest)
        );
        assert!(screen.pane.source_filter.contains(&EventSource::Http));
    }

    #[test]
    fn contextual_actions_switch_to_batch_copy_for_multi_selected_rows() {
        let mut screen = TimelineScreen::new();
        screen.pane.verbosity = VerbosityTier::All;
        push_event_entry(
            &mut screen.pane,
            1,
            MailEvent::http_request("GET", "/one", 200, 1, "127.0.0.1"),
        );
        push_event_entry(
            &mut screen.pane,
            2,
            MailEvent::http_request("GET", "/two", 200, 1, "127.0.0.1"),
        );
        screen
            .selected_timeline_keys
            .select(TimelineSelectionKey::Event { seq: 1 });
        screen
            .selected_timeline_keys
            .select(TimelineSelectionKey::Event { seq: 2 });

        let (actions, _, ctx) = screen
            .contextual_actions()
            .expect("contextual actions should exist");
        assert!(ctx.starts_with("batch:"));
        assert_eq!(actions.len(), 1);
        match &actions[0].action {
            ActionKind::CopyToClipboard(payload) => {
                assert!(payload.contains("/one") || payload.contains("/two"));
            }
            other => panic!("expected CopyToClipboard action, got {other:?}"),
        }
    }

    #[test]
    fn copyable_content_joins_multi_selected_rows() {
        let mut screen = TimelineScreen::new();
        screen.pane.verbosity = VerbosityTier::All;
        push_event_entry(
            &mut screen.pane,
            1,
            MailEvent::http_request("GET", "/one", 200, 1, "127.0.0.1"),
        );
        push_event_entry(
            &mut screen.pane,
            2,
            MailEvent::http_request("GET", "/two", 200, 1, "127.0.0.1"),
        );
        screen
            .selected_timeline_keys
            .select(TimelineSelectionKey::Event { seq: 1 });
        screen
            .selected_timeline_keys
            .select(TimelineSelectionKey::Event { seq: 2 });

        let payload = screen.copyable_content().expect("copy payload");
        assert!(payload.contains('\n'));
    }

    #[test]
    fn commit_timeline_entry_maps_storage_fields() {
        let storage_entry = mcp_agent_mail_storage::TimelineEntry {
            sha: "0123456789abcdef".to_string(),
            short_sha: "01234567".to_string(),
            date: "2026-02-16T15:00:00Z".to_string(),
            timestamp: 1_700_000_000,
            subject: "mail: BluePuma -> SilentOwl | hello".to_string(),
            commit_type: "message".to_string(),
            sender: Some("BluePuma".to_string()),
            recipients: vec!["SilentOwl".to_string()],
            author: "BluePuma".to_string(),
        };
        let mapped = CommitTimelineEntry::from_storage("proj-a".to_string(), storage_entry);
        assert_eq!(mapped.project_slug, "proj-a");
        assert_eq!(mapped.type_label(), "CommitMsg");
        assert!(mapped.detail_summary().contains("BluePuma -> SilentOwl"));
        assert!(mapped.timestamp_micros > 0);
    }

    #[test]
    fn commit_stats_aggregate_projects_authors_and_types() {
        let entries = vec![
            make_commit_entry("alpha", "BluePuma", "message", 1_000_000),
            make_commit_entry("beta", "BluePuma", "chore", 2_000_000),
            make_commit_entry("beta", "SilentOwl", "file_reservation", 3_000_000),
        ];
        let stats = CommitTimelineStats::from_entries(&entries, 2);

        assert_eq!(stats.total_commits, 3);
        assert_eq!(stats.unique_authors, 2);
        assert_eq!(stats.active_projects, 2);
        assert_eq!(stats.message_commits, 1);
        assert_eq!(stats.reservation_commits, 1);
        assert_eq!(stats.refresh_errors, 2);
        assert_eq!(stats.churn_insertions, 0);
        assert_eq!(stats.churn_deletions, 0);
    }

    #[test]
    fn commit_stats_summary_line_includes_churn_and_errors() {
        let mut stats = CommitTimelineStats {
            total_commits: 9,
            unique_authors: 3,
            active_projects: 2,
            message_commits: 7,
            reservation_commits: 1,
            churn_insertions: 42,
            churn_deletions: 17,
            refresh_errors: 1,
        };
        let line = stats.summary_line();
        assert!(line.contains("commits:9"));
        assert!(line.contains("authors:3"));
        assert!(line.contains("projects:2"));
        assert!(line.contains("+42 -17"));
        assert!(line.contains("msg:7"));
        assert!(line.contains("resv:1"));
        assert!(line.contains("errs:1"));

        stats.refresh_errors = 0;
        let no_err_line = stats.summary_line();
        assert!(!no_err_line.contains("errs:"));
    }

    #[test]
    fn combined_rows_sort_by_timestamp_across_event_and_commit_streams() {
        let mut screen = TimelineScreen::new();
        screen.pane.verbosity = VerbosityTier::All;
        push_event_entry(
            &mut screen.pane,
            1,
            MailEvent::http_request("GET", "/first", 200, 1, "127.0.0.1"),
        );
        push_event_entry(
            &mut screen.pane,
            3,
            MailEvent::http_request("GET", "/third", 200, 1, "127.0.0.1"),
        );
        screen
            .commit_entries
            .push(make_commit_entry("proj", "BluePuma", "message", 2_000_000));

        let rows = screen.combined_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].timestamp_micros(), 1_000_000);
        assert_eq!(rows[1].timestamp_micros(), 2_000_000);
        assert_eq!(rows[2].timestamp_micros(), 3_000_000);
    }

    #[test]
    fn enter_on_commit_view_navigates_to_archive_browser() {
        let mut screen = TimelineScreen::new();
        screen.view_mode = TimelineViewMode::Commits;
        screen
            .commit_entries
            .push(make_commit_entry("proj", "BluePuma", "message", 2_000_000));
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let cmd = screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Enter)), &state);
        assert!(
            matches!(
                cmd,
                Cmd::Msg(MailScreenMsg::Navigate(MailScreenId::ArchiveBrowser))
            ),
            "Expected Navigate to ArchiveBrowser"
        );
    }

    #[test]
    fn clear_filters_resets_verbosity() {
        let mut pane = test_pane();
        pane.verbosity = VerbosityTier::All;
        pane.kind_filter.insert(MailEventKind::HttpRequest);
        pane.source_filter.insert(EventSource::Http);

        pane.clear_filters();
        assert!(pane.kind_filter.is_empty());
        assert!(pane.source_filter.is_empty());
        assert_eq!(pane.verbosity, VerbosityTier::Standard);
    }

    #[test]
    fn verbosity_and_kind_filter_combine() {
        let mut pane = test_pane();
        // Add Info-level message and Debug-level HTTP
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::MessageSent,
                severity: EventSeverity::Info,
                seq: 1,
                timestamp_micros: 1_000_000,
                timestamp: "00:00:00.000".to_string(),
                icon: '✉',
                summary: "msg".to_string(),
            },
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Mail,
            severity: EventSeverity::Info,
            raw: MailEvent::message_sent(1, "A", vec![], "s", "t", "p"),
        });
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: EventSeverity::Debug,
                seq: 2,
                timestamp_micros: 2_000_000,
                timestamp: "00:00:00.001".to_string(),
                icon: '↔',
                summary: "GET /".to_string(),
            },
            seq: 2,
            timestamp_micros: 2_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: make_event(2),
        });

        // All verbosity, no kind filter: both visible
        assert_eq!(pane.filtered_len(), 2);

        // Standard verbosity hides Debug: only Info visible
        pane.verbosity = VerbosityTier::Standard;
        assert_eq!(pane.filtered_len(), 1);

        // Verbose + kind filter for HttpRequest only
        pane.verbosity = VerbosityTier::Verbose;
        pane.kind_filter.insert(MailEventKind::HttpRequest);
        assert_eq!(pane.filtered_len(), 1);
    }

    // ── Dock layout integration tests ────────────────────────────────

    #[test]
    fn dock_toggle_via_i_key() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        assert!(screen.dock.visible);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('i')));
        screen.update(&key, &state);
        assert!(!screen.dock.visible);

        screen.update(&key, &state);
        assert!(screen.dock.visible);
    }

    #[test]
    fn dock_grow_shrink_via_brackets() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        let initial_ratio = screen.dock.ratio;

        let grow = Event::Key(ftui::KeyEvent::new(KeyCode::Char(']')));
        screen.update(&grow, &state);
        assert!(screen.dock.ratio > initial_ratio);

        let shrink = Event::Key(ftui::KeyEvent::new(KeyCode::Char('[')));
        screen.update(&shrink, &state);
        screen.update(&shrink, &state);
        assert!(screen.dock.ratio < initial_ratio);
    }

    #[test]
    fn dock_cycle_position_via_braces() {
        use crate::tui_layout::DockPosition;
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        assert_eq!(screen.dock.position, DockPosition::Right);

        let next = Event::Key(ftui::KeyEvent::new(KeyCode::Char('}')));
        screen.update(&next, &state);
        assert_eq!(screen.dock.position, DockPosition::Top);

        let prev = Event::Key(ftui::KeyEvent::new(KeyCode::Char('{')));
        screen.update(&prev, &state);
        assert_eq!(screen.dock.position, DockPosition::Right);
    }

    #[test]
    fn dock_split_used_in_view() {
        let screen = TimelineScreen::new();
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 40, &mut pool);
        // Should not panic with dock visible
        screen.view(&mut frame, Rect::new(0, 0, 120, 40), &state);
        // Verify last_area was cached
        assert_eq!(screen.last_area.get().width, 120);

        // Should not panic with dock hidden
        let mut screen2 = TimelineScreen::new();
        screen2.dock.visible = false;
        let mut pool2 = ftui::GraphemePool::new();
        let mut frame2 = Frame::new(120, 40, &mut pool2);
        screen2.view(&mut frame2, Rect::new(0, 0, 120, 40), &state);
    }

    // ── Mouse drag tests ────────────────────────────────────────────

    #[test]
    fn mouse_down_on_border_starts_drag() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Set last_area so hit_test_border works
        screen.last_area.set(Rect::new(0, 0, 100, 40));

        // For Right dock at 40%, the border is at x=60
        let split = screen.dock.split(screen.last_area.get());
        let border_x = split.dock.unwrap().x;

        let mouse_down = Event::Mouse(ftui::MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            border_x,
            20,
        ));
        screen.update(&mouse_down, &state);
        assert_eq!(screen.dock_drag, DockDragState::Dragging);

        // Mouse up ends drag
        let mouse_up = Event::Mouse(ftui::MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            border_x,
            20,
        ));
        screen.update(&mouse_up, &state);
        assert_eq!(screen.dock_drag, DockDragState::Idle);
    }

    #[test]
    fn mouse_drag_resizes_dock() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        screen.last_area.set(Rect::new(0, 0, 100, 40));

        let initial_ratio = screen.dock.ratio;

        // Start drag on the border
        let split = screen.dock.split(screen.last_area.get());
        let border_x = split.dock.unwrap().x;
        let mouse_down = Event::Mouse(ftui::MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            border_x,
            20,
        ));
        screen.update(&mouse_down, &state);

        // Drag to x=40 (makes dock bigger: 100-40=60 → 60%)
        let mouse_drag = Event::Mouse(ftui::MouseEvent::new(
            MouseEventKind::Drag(MouseButton::Left),
            40,
            20,
        ));
        screen.update(&mouse_drag, &state);
        assert!(screen.dock.ratio > initial_ratio);

        // Release
        let mouse_up = Event::Mouse(ftui::MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            40,
            20,
        ));
        screen.update(&mouse_up, &state);
        assert_eq!(screen.dock_drag, DockDragState::Idle);
    }

    #[test]
    fn mouse_down_away_from_border_no_drag() {
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        screen.last_area.set(Rect::new(0, 0, 100, 40));

        // Click far from border
        let mouse_down = Event::Mouse(ftui::MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            10,
            20,
        ));
        screen.update(&mouse_down, &state);
        assert_eq!(screen.dock_drag, DockDragState::Idle);
    }

    // ── Preset cycling ──────────────────────────────────────────────

    #[test]
    fn preset_cycling_via_p_key() {
        use crate::tui_layout::DockPreset;
        let mut screen = TimelineScreen::new();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        // Default is 0.4 (Balanced). Pressing p should cycle to next: Half (0.5)
        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('p')));
        screen.update(&key, &state);
        assert!((screen.dock.ratio - DockPreset::Half.ratio()).abs() < f32::EPSILON);
        assert!(screen.dock.visible);

        // Next: Wide (0.6)
        screen.update(&key, &state);
        assert!((screen.dock.ratio - DockPreset::Wide.ratio()).abs() < f32::EPSILON);

        // Next: Compact (0.2)
        screen.update(&key, &state);
        assert!((screen.dock.ratio - DockPreset::Compact.ratio()).abs() < f32::EPSILON);
    }

    #[test]
    fn preset_for_ratio_finds_closest() {
        assert_eq!(preset_for_ratio(0.4), DockPreset::Balanced);
        assert_eq!(preset_for_ratio(0.19), DockPreset::Compact);
        assert_eq!(preset_for_ratio(0.51), DockPreset::Half);
        assert_eq!(preset_for_ratio(0.61), DockPreset::Wide);
        assert_eq!(preset_for_ratio(0.34), DockPreset::Third);
    }

    // ── Timeline state-machine edge cases ────────────────────────

    #[test]
    fn cursor_after_trim_is_clamped() {
        let mut pane = test_pane();
        // Fill to capacity + extra
        for i in 0..(TIMELINE_CAPACITY + 200) {
            let seq = u64::try_from(i).expect("test index fits u64");
            let ts = i64::try_from(i).expect("test index fits i64");
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(seq)),
                seq,
                timestamp_micros: ts * 1_000_000,
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(seq),
            });
        }
        // Set cursor to middle of data
        pane.cursor = TIMELINE_CAPACITY + 100;

        // Simulate trim
        let excess = pane.entries.len() - TIMELINE_CAPACITY;
        pane.entries.drain(..excess);
        pane.cursor = pane.cursor.saturating_sub(excess);

        // Cursor should be within range
        assert!(pane.cursor < pane.entries.len());
    }

    #[test]
    fn cursor_after_filter_toggle_is_clamped() {
        let mut pane = test_pane();
        // Add 5 HTTP entries and 5 Mail entries
        for i in 0_u64..5 {
            let i_i64 = i64::try_from(i).expect("test index fits i64");
            pane.entries.push(TimelineEntry {
                display: EventEntry {
                    kind: MailEventKind::HttpRequest,
                    severity: EventSeverity::Debug,
                    seq: i,
                    timestamp_micros: i_i64 * 1_000_000,
                    timestamp: format!("00:00:0{i}.000"),
                    icon: '↔',
                    summary: format!("GET /{i}"),
                },
                seq: i,
                timestamp_micros: i_i64 * 1_000_000,
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        for i in 5_u64..10 {
            let i_i64 = i64::try_from(i).expect("test index fits i64");
            pane.entries.push(TimelineEntry {
                display: EventEntry {
                    kind: MailEventKind::MessageSent,
                    severity: EventSeverity::Info,
                    seq: i,
                    timestamp_micros: i_i64 * 1_000_000,
                    timestamp: format!("00:00:0{i}.000"),
                    icon: '✉',
                    summary: format!("msg {i}"),
                },
                seq: i,
                timestamp_micros: i_i64 * 1_000_000,
                source: EventSource::Mail,
                severity: EventSeverity::Info,
                raw: MailEvent::message_sent(i_i64, "A", vec![], "s", "t", "p"),
            });
        }

        // All 10 visible, set cursor to index 8
        assert_eq!(pane.filtered_len(), 10);
        pane.cursor = 8;

        // Enable kind filter for HTTP only (5 items)
        pane.toggle_kind_filter(MailEventKind::HttpRequest);
        assert_eq!(pane.filtered_len(), 5);
        // Cursor should be clamped to max valid index (4)
        assert!(pane.cursor <= 4);
    }

    #[test]
    fn multiple_filters_combined_kind_source_verbosity() {
        let mut pane = test_pane();
        // HTTP Debug from Http source
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: EventSeverity::Debug,
                seq: 1,
                timestamp_micros: 1_000_000,
                timestamp: "00:00:00.000".to_string(),
                icon: '↔',
                summary: "GET /".to_string(),
            },
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: make_event(1),
        });
        // Tool Debug from Tooling source
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::ToolCallEnd,
                severity: EventSeverity::Debug,
                seq: 2,
                timestamp_micros: 2_000_000,
                timestamp: "00:00:00.001".to_string(),
                icon: '⚙',
                summary: "tool done".to_string(),
            },
            seq: 2,
            timestamp_micros: 2_000_000,
            source: EventSource::Tooling,
            severity: EventSeverity::Debug,
            raw: make_event(2),
        });
        // Message Info from Mail source
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::MessageSent,
                severity: EventSeverity::Info,
                seq: 3,
                timestamp_micros: 3_000_000,
                timestamp: "00:00:00.002".to_string(),
                icon: '✉',
                summary: "msg".to_string(),
            },
            seq: 3,
            timestamp_micros: 3_000_000,
            source: EventSource::Mail,
            severity: EventSeverity::Info,
            raw: MailEvent::message_sent(1, "A", vec![], "s", "t", "p"),
        });

        // All verbosity, no filters: all 3 visible
        assert_eq!(pane.filtered_len(), 3);

        // Kind filter: only HttpRequest
        pane.toggle_kind_filter(MailEventKind::HttpRequest);
        assert_eq!(pane.filtered_len(), 1);

        // Remove kind filter, add source filter: only Tooling
        pane.toggle_kind_filter(MailEventKind::HttpRequest);
        pane.toggle_source_filter(EventSource::Tooling);
        assert_eq!(pane.filtered_len(), 1);

        // Combine: source=Tooling + verbosity=Standard (hides Debug)
        pane.verbosity = VerbosityTier::Standard;
        assert_eq!(pane.filtered_len(), 0); // Tooling entry is Debug, hidden by Standard
    }

    #[test]
    fn empty_filter_results_cursor_stays_at_zero() {
        let mut pane = test_pane();
        pane.entries.push(TimelineEntry {
            display: EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: EventSeverity::Debug,
                seq: 1,
                timestamp_micros: 1_000_000,
                timestamp: "00:00:00.000".to_string(),
                icon: '↔',
                summary: "GET /".to_string(),
            },
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Debug,
            raw: make_event(1),
        });

        pane.cursor = 0;
        // Filter to something that matches nothing
        pane.toggle_kind_filter(MailEventKind::MessageSent);
        assert_eq!(pane.filtered_len(), 0);
        assert_eq!(pane.cursor, 0);
        assert!(pane.selected_event().is_none());
    }

    #[test]
    fn follow_mode_plus_filter_toggle() {
        let mut pane = test_pane();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        // Push 5 HTTP + 5 tool events
        for _ in 0..5 {
            let _ = state.push_event(MailEvent::http_request("GET", "/x", 200, 1, "127.0.0.1"));
        }
        for _ in 0..5 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "t",
                1,
                None,
                0,
                0.0,
                vec![],
                None,
                None,
            ));
        }

        pane.follow = true;
        pane.ingest(&state);

        // Follow should be at the end
        assert_eq!(pane.cursor, pane.filtered_len() - 1);

        // Now toggle kind filter to only show HttpRequest
        pane.toggle_kind_filter(MailEventKind::HttpRequest);
        // Cursor should be clamped to the new filtered view
        assert!(pane.cursor < pane.filtered_len());
    }

    #[test]
    fn jump_to_time_empty_pane() {
        let mut pane = test_pane();
        // Should not panic on empty data
        pane.jump_to_time(50_000_000);
        assert_eq!(pane.cursor, 0);
    }

    #[test]
    fn jump_to_time_before_first_entry() {
        let mut pane = test_pane();
        for i in 10..20u64 {
            let i_i64 = i64::try_from(i).expect("test index fits i64");
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i_i64 * 1_000_000,
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        pane.jump_to_time(0);
        assert_eq!(pane.cursor, 0);
    }

    #[test]
    fn toggle_follow_jumps_to_end() {
        let mut pane = test_pane();
        for i in 0_u64..10 {
            let i_i64 = i64::try_from(i).expect("test index fits i64");
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i_i64 * 1_000_000,
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        pane.cursor = 0;
        assert!(!pane.follow);

        pane.toggle_follow();
        assert!(pane.follow);
        assert_eq!(pane.cursor, 9);

        pane.toggle_follow();
        assert!(!pane.follow);
    }

    #[test]
    fn cursor_up_disables_follow() {
        let mut pane = test_pane();
        pane.follow = true;
        for i in 0_u64..5 {
            let i_i64 = i64::try_from(i).expect("test index fits i64");
            pane.entries.push(TimelineEntry {
                display: format_event(&make_event(i)),
                seq: i,
                timestamp_micros: i_i64 * 1_000_000,
                source: EventSource::Http,
                severity: EventSeverity::Debug,
                raw: make_event(i),
            });
        }
        pane.cursor = 4;
        pane.cursor_up(1);
        assert!(!pane.follow);
        assert_eq!(pane.cursor, 3);
    }

    #[test]
    fn render_timeline_at_extreme_width() {
        let pane = TimelinePane::new();
        let dock = DockLayout::right_40();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 5, &mut pool);
        let mut list_state = VirtualizedListState::new();
        // Should not panic at very narrow width
        render_timeline(
            &mut frame,
            Rect::new(0, 0, 10, 5),
            &pane,
            dock,
            &mut list_state,
            false,
            &HashSet::new(),
            0,
        );
    }

    #[test]
    fn render_timeline_height_one() {
        let pane = TimelinePane::new();
        let dock = DockLayout::right_40();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 1, &mut pool);
        let mut list_state = VirtualizedListState::new();
        // Should not panic at minimum height
        render_timeline(
            &mut frame,
            Rect::new(0, 0, 80, 1),
            &pane,
            dock,
            &mut list_state,
            false,
            &HashSet::new(),
            0,
        );
    }

    #[test]
    fn deep_link_thread_by_id_returns_false() {
        let mut screen = TimelineScreen::new();
        // ThreadById is not handled by Timeline (it handles TimelineAtTime)
        let target = DeepLinkTarget::ThreadById("test-thread".to_string());
        assert!(!screen.receive_deep_link(&target));
    }

    #[test]
    fn total_ingested_tracks_all_events() {
        let mut pane = test_pane();
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());
        for _ in 0..10 {
            let _ = state.push_event(MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1"));
        }
        pane.ingest(&state);
        assert_eq!(pane.total_ingested, 10);
    }

    // ── Layout operations ──────────────────────────────────────────

    #[test]
    fn reset_layout_restores_defaults() {
        let mut screen = TimelineScreen::new();
        screen.dock = DockLayout::new(DockPosition::Left, 0.6).with_visible(false);
        assert!(screen.reset_layout());
        assert_eq!(screen.dock, DockLayout::default());
    }

    #[test]
    fn reset_layout_with_config_restores_and_saves() {
        let dir = tempfile::tempdir().unwrap();
        let config = mcp_agent_mail_core::Config {
            console_persist_path: dir.path().join("config.env"),
            console_auto_save: true,
            tui_dock_position: "left".to_string(),
            tui_dock_ratio_percent: 60,
            tui_dock_visible: false,
            ..mcp_agent_mail_core::Config::default()
        };
        let mut screen = TimelineScreen::with_config(&config);
        assert_eq!(screen.dock.position, DockPosition::Left);
        assert!(!screen.dock.visible);

        assert!(screen.reset_layout());
        assert_eq!(screen.dock.position, DockPosition::Right);
        assert!(screen.dock.visible);
    }

    #[test]
    fn export_layout_returns_none_without_persister() {
        let screen = TimelineScreen::new();
        assert!(screen.export_layout().is_none());
    }

    #[test]
    fn import_layout_returns_false_without_persister() {
        let mut screen = TimelineScreen::new();
        assert!(!screen.import_layout());
    }

    #[test]
    fn export_import_layout_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = mcp_agent_mail_core::Config {
            console_persist_path: dir.path().join("config.env"),
            console_auto_save: true,
            tui_dock_position: "left".to_string(),
            tui_dock_ratio_percent: 55,
            tui_dock_visible: false,
            ..mcp_agent_mail_core::Config::default()
        };

        // Export from screen with custom layout
        let screen = TimelineScreen::with_config(&config);
        let original_dock = screen.dock;
        let path = screen.export_layout().unwrap();
        assert!(path.exists());
        assert!(path.to_str().unwrap().ends_with("layout.json"));

        // Import into a fresh screen with defaults
        let config2 = mcp_agent_mail_core::Config {
            console_persist_path: dir.path().join("config.env"),
            console_auto_save: true,
            ..mcp_agent_mail_core::Config::default()
        };
        let mut screen2 = TimelineScreen::with_config(&config2);
        assert_ne!(screen2.dock, original_dock);
        assert!(screen2.import_layout());
        assert_eq!(screen2.dock, original_dock);
    }

    #[test]
    fn import_layout_fails_with_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = mcp_agent_mail_core::Config {
            console_persist_path: dir.path().join("config.env"),
            console_auto_save: true,
            ..mcp_agent_mail_core::Config::default()
        };
        let mut screen = TimelineScreen::with_config(&config);
        assert!(!screen.import_layout());
    }

    // ──────────────────────────────────────────────────────────────────
    // br-1xt0m.1.10.4: Timeline semantic encoding and readability
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn source_badges_are_readable_mixed_case() {
        // Verify source badges use readable mixed-case, not all-caps cryptic abbreviations
        assert_eq!(source_badge(EventSource::Tooling), "Tool");
        assert_eq!(source_badge(EventSource::Mail), "Mail");
        assert_eq!(source_badge(EventSource::Reservations), "Resv");
        assert_eq!(source_badge(EventSource::Lifecycle), "Life");
    }

    #[test]
    fn build_filter_tag_uses_compact_labels() {
        let mut kinds = HashSet::new();
        kinds.insert(MailEventKind::MessageSent);
        let tag = build_filter_tag(&kinds, &HashSet::new());
        // Should use compact_label ("MsgSent"), not Debug format ("MessageSent")
        assert!(
            tag.contains("MsgSent"),
            "tag should use compact_label: {tag}"
        );

        let mut sources = HashSet::new();
        sources.insert(EventSource::Tooling);
        let tag2 = build_filter_tag(&HashSet::new(), &sources);
        assert!(tag2.contains("Tool"), "tag should use source badge: {tag2}");
    }

    #[test]
    fn timeline_row_renders_without_panic_on_error_event() {
        // Error events should render with severity styling (no panic)
        let event = MailEvent::http_request("GET", "/fail", 500, 10, "127.0.0.1");
        let display = format_event(&event);
        let entry = TimelineEntry {
            seq: 1,
            timestamp_micros: 1_000_000,
            source: EventSource::Http,
            severity: EventSeverity::Error,
            display,
            raw: event,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 1, &mut pool);
        entry.render(Rect::new(0, 0, 80, 1), &mut frame, false);
        // Should contain HTTP source badge and Error badge
        let text = buffer_to_text(&frame.buffer);
        assert!(text.contains("HTTP"), "row text: {text}");
        assert!(text.contains("ERR"), "row text: {text}");
    }

    #[test]
    fn timeline_shimmer_progress_expires_after_window() {
        let now = 1_700_000_000_000_000_i64;
        assert!(shimmer_progress_for_timestamp(now, now).is_some());
        assert!(shimmer_progress_for_timestamp(now, now - SHIMMER_WINDOW_MICROS - 1).is_none());
    }

    #[test]
    fn timeline_shimmer_progress_caps_at_five_entries() {
        let now = unix_epoch_micros_now().expect("system clock should provide unix micros");
        let entries: Vec<TimelineEntry> = (0..8_u64)
            .map(|idx| {
                let event = make_event(idx);
                TimelineEntry {
                    display: format_event(&event),
                    seq: idx,
                    timestamp_micros: now
                        - (i64::try_from(idx).expect("test idx should fit i64") * 10_000),
                    source: EventSource::Mail,
                    severity: EventSeverity::Info,
                    raw: event,
                }
            })
            .collect();
        let shimmer = timeline_shimmer_progresses(&entries, true);
        assert_eq!(
            shimmer.iter().filter(|p| p.is_some()).count(),
            SHIMMER_MAX_ROWS
        );
        assert!(
            timeline_shimmer_progresses(&entries, false)
                .iter()
                .all(Option::is_none)
        );
    }

    // ── Screen logic, density heuristics, and failure paths (br-1xt0m.1.13.8) ──

    #[test]
    fn verbosity_tier_cycle_round_trips() {
        let start = VerbosityTier::default();
        let mut tier = start;
        for _ in 0..4 {
            tier = tier.next();
        }
        assert_eq!(tier, start, "4 next() should round-trip");
    }

    #[test]
    fn verbosity_includes_severity_correctness() {
        // Standard includes Info, Warn, Error; excludes Debug, Trace.
        assert!(VerbosityTier::Standard.includes(EventSeverity::Info));
        assert!(VerbosityTier::Standard.includes(EventSeverity::Warn));
        assert!(VerbosityTier::Standard.includes(EventSeverity::Error));
        assert!(!VerbosityTier::Standard.includes(EventSeverity::Debug));
        assert!(!VerbosityTier::Standard.includes(EventSeverity::Trace));

        // Minimal: only Warn + Error.
        assert!(!VerbosityTier::Minimal.includes(EventSeverity::Info));
        assert!(VerbosityTier::Minimal.includes(EventSeverity::Warn));

        // All: everything passes.
        assert!(VerbosityTier::All.includes(EventSeverity::Trace));
    }

    #[test]
    fn clear_filters_resets_verbosity_to_standard() {
        let mut pane = TimelinePane::new();
        pane.verbosity = VerbosityTier::Minimal;
        pane.kind_filter.insert(MailEventKind::MessageSent);
        pane.source_filter.insert(EventSource::Http);
        pane.clear_filters();
        assert!(pane.kind_filter.is_empty());
        assert!(pane.source_filter.is_empty());
        assert_eq!(pane.verbosity, VerbosityTier::Standard);
    }

    #[test]
    fn toggle_kind_filter_add_and_remove() {
        let mut pane = TimelinePane::new();
        assert!(pane.kind_filter.is_empty());
        pane.toggle_kind_filter(MailEventKind::MessageSent);
        assert!(pane.kind_filter.contains(&MailEventKind::MessageSent));
        pane.toggle_kind_filter(MailEventKind::MessageSent);
        assert!(!pane.kind_filter.contains(&MailEventKind::MessageSent));
    }

    #[test]
    fn toggle_source_filter_add_and_remove() {
        let mut pane = TimelinePane::new();
        pane.toggle_source_filter(EventSource::Http);
        assert!(pane.source_filter.contains(&EventSource::Http));
        pane.toggle_source_filter(EventSource::Http);
        assert!(!pane.source_filter.contains(&EventSource::Http));
    }

    #[test]
    fn cursor_up_multi_step_saturates() {
        let mut pane = TimelinePane::new();
        pane.cursor = 3;
        pane.cursor_up(10);
        assert_eq!(pane.cursor, 0, "saturating_sub should clamp to 0");
    }

    #[test]
    fn jump_to_time_on_empty_is_noop() {
        let mut pane = TimelinePane::new();
        pane.follow = true;
        pane.jump_to_time(1_000_000);
        // With no entries, follow stays unchanged (function returns early).
        assert!(pane.follow);
    }

    #[test]
    fn new_pane_defaults() {
        let pane = TimelinePane::new();
        assert_eq!(pane.verbosity, VerbosityTier::Standard);
        assert!(!pane.follow());
        assert_eq!(pane.cursor(), 0);
        assert_eq!(pane.total_ingested, 0);
    }

    #[test]
    fn preset_tokens_round_trip() {
        assert_eq!(
            parse_event_kind_token(event_kind_token(MailEventKind::MessageSent)),
            Some(MailEventKind::MessageSent)
        );
        assert_eq!(
            parse_event_source_token(event_source_token(EventSource::Reservations)),
            Some(EventSource::Reservations)
        );
        assert_eq!(
            parse_verbosity_token(verbosity_token(VerbosityTier::Verbose)),
            Some(VerbosityTier::Verbose)
        );
    }

    #[test]
    fn timeline_presets_save_load_delete_lifecycle() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("screen_filter_presets.json");
        let mut screen = TimelineScreen::with_filter_presets_path_for_test(path.clone());

        screen.pane.verbosity = VerbosityTier::Minimal;
        screen.pane.kind_filter.insert(MailEventKind::HttpRequest);
        screen.pane.source_filter.insert(EventSource::Http);
        assert!(screen.save_named_preset(
            "Errors".to_string(),
            Some("Only error-ish traffic".to_string())
        ));

        assert!(path.exists());
        let loaded = crate::tui_persist::load_screen_filter_presets(&path).expect("load presets");
        assert_eq!(
            loaded.list_names(TIMELINE_PRESET_SCREEN_ID),
            vec!["Errors".to_string()]
        );

        screen.pane.clear_filters();
        assert_eq!(screen.pane.verbosity, VerbosityTier::Standard);
        assert!(screen.pane.kind_filter.is_empty());
        assert!(screen.pane.source_filter.is_empty());

        assert!(screen.apply_named_preset("Errors"));
        assert_eq!(screen.pane.verbosity, VerbosityTier::Minimal);
        assert!(
            screen
                .pane
                .kind_filter
                .contains(&MailEventKind::HttpRequest)
        );
        assert!(screen.pane.source_filter.contains(&EventSource::Http));

        assert!(screen.remove_named_preset("Errors"));
        assert!(screen.preset_names().is_empty());
    }

    #[test]
    fn ctrl_shortcuts_drive_save_and_load_dialogs() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("screen_filter_presets.json");
        let mut screen = TimelineScreen::with_filter_presets_path_for_test(path);
        let state = TuiSharedState::new(&mcp_agent_mail_core::Config::default());

        let ctrl_s =
            Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')).with_modifiers(Modifiers::CTRL));
        screen.update(&ctrl_s, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::Save);

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        screen.update(&enter, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::None);
        assert!(!screen.preset_names().is_empty());

        let ctrl_l =
            Event::Key(ftui::KeyEvent::new(KeyCode::Char('l')).with_modifiers(Modifiers::CTRL));
        screen.update(&ctrl_l, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::Load);

        let delete = Event::Key(ftui::KeyEvent::new(KeyCode::Delete));
        screen.update(&delete, &state);
        assert!(screen.preset_names().is_empty());

        let escape = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&escape, &state);
        assert_eq!(screen.preset_dialog_mode, PresetDialogMode::None);
    }
}
