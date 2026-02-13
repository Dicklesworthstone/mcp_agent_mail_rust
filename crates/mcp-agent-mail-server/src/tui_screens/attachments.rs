//! Attachments screen — browse attachments across messages with provenance trails.

use ftui::layout::{Constraint, Rect};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, Style};
use ftui_runtime::program::Cmd;

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel::Value;
use mcp_agent_mail_db::timestamps::micros_to_iso;

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

const COL_MEDIA: usize = 0;
const COL_SIZE: usize = 1;
const COL_SENDER: usize = 2;
const COL_SUBJECT: usize = 3;
const COL_DATE: usize = 4;
const COL_PROJECT: usize = 5;

const SORT_LABELS: &[&str] = &["Type", "Size", "Sender", "Subject", "Date", "Project"];

/// Debounce ticks before reloading data.
const RELOAD_INTERVAL_TICKS: u64 = 50;

/// Maximum attachments to fetch from DB.
const FETCH_LIMIT: usize = 500;

// ──────────────────────────────────────────────────────────────────────
// AttachmentEntry — parsed attachment with provenance
// ──────────────────────────────────────────────────────────────────────

/// A single attachment entry with its source message provenance.
#[derive(Debug, Clone)]
struct AttachmentEntry {
    /// Media type (e.g. "image/webp", "application/pdf").
    media_type: String,
    /// Size in bytes.
    bytes: u64,
    /// SHA-1 hash of the attachment content.
    sha1: String,
    /// Dimensions (width x height), zero if not an image.
    width: u32,
    height: u32,
    /// Storage mode: "inline" or "file".
    mode: String,
    /// Relative path in archive (file mode only).
    path: Option<String>,

    // Provenance fields
    message_id: i64,
    sender_name: String,
    subject: String,
    thread_id: Option<String>,
    created_ts: i64,
    project_slug: String,
}

impl AttachmentEntry {
    /// Human-readable size string.
    #[allow(clippy::cast_precision_loss)]
    fn size_display(&self) -> String {
        if self.bytes < 1024 {
            format!("{} B", self.bytes)
        } else if self.bytes < 1_048_576 {
            format!("{:.1} KB", self.bytes as f64 / 1024.0)
        } else {
            format!("{:.1} MB", self.bytes as f64 / 1_048_576.0)
        }
    }

    /// Short type label from `media_type`.
    fn type_label(&self) -> &str {
        // Show subtype only (e.g. "webp" from "image/webp")
        self.media_type
            .split('/')
            .nth(1)
            .unwrap_or(&self.media_type)
    }

    /// Dimensions display, if available.
    fn dims_display(&self) -> String {
        if self.width > 0 && self.height > 0 {
            format!("{}x{}", self.width, self.height)
        } else {
            String::new()
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Filter for media type categories
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaFilter {
    All,
    Images,
    Documents,
    Other,
}

impl MediaFilter {
    const fn next(self) -> Self {
        match self {
            Self::All => Self::Images,
            Self::Images => Self::Documents,
            Self::Documents => Self::Other,
            Self::Other => Self::All,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Images => "Images",
            Self::Documents => "Docs",
            Self::Other => "Other",
        }
    }

    fn matches(self, media_type: &str) -> bool {
        match self {
            Self::All => true,
            Self::Images => media_type.starts_with("image/"),
            Self::Documents => {
                media_type.starts_with("application/pdf")
                    || media_type.starts_with("text/")
                    || media_type.contains("document")
            }
            Self::Other => {
                !media_type.starts_with("image/")
                    && !media_type.starts_with("application/pdf")
                    && !media_type.starts_with("text/")
                    && !media_type.contains("document")
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// AttachmentExplorerScreen
// ──────────────────────────────────────────────────────────────────────

#[allow(clippy::struct_excessive_bools)]
pub struct AttachmentExplorerScreen {
    table_state: TableState,
    /// All loaded attachment entries.
    entries: Vec<AttachmentEntry>,
    /// Filtered + sorted display indices into `entries`.
    display_indices: Vec<usize>,
    sort_col: usize,
    sort_asc: bool,
    media_filter: MediaFilter,
    text_filter: String,
    text_filter_active: bool,
    /// Detail panel scroll offset.
    detail_scroll: usize,

    // DB state
    db_conn: Option<DbConn>,
    db_conn_attempted: bool,
    last_error: Option<String>,
    data_dirty: bool,
    last_reload_tick: u64,

    /// Synthetic event for the focused attachment's source message.
    focused_synthetic: Option<crate::tui_events::MailEvent>,
}

impl AttachmentExplorerScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            entries: Vec::new(),
            display_indices: Vec::new(),
            sort_col: COL_DATE,
            sort_asc: false,
            media_filter: MediaFilter::All,
            text_filter: String::new(),
            text_filter_active: false,
            detail_scroll: 0,
            db_conn: None,
            db_conn_attempted: false,
            last_error: None,
            data_dirty: true,
            last_reload_tick: 0,
            focused_synthetic: None,
        }
    }

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

    fn load_attachments(&mut self, state: &TuiSharedState) {
        self.ensure_db_conn(state);
        let Some(conn) = self.db_conn.take() else {
            return;
        };

        let sql = "SELECT m.id AS message_id, m.subject, m.attachments, m.created_ts, \
                   m.thread_id, a.name AS sender_name, p.slug AS project_slug \
                   FROM messages m \
                   JOIN agents a ON a.id = m.sender_id \
                   JOIN projects p ON p.id = m.project_id \
                   WHERE m.attachments != '[]' AND length(m.attachments) > 2 \
                   ORDER BY m.created_ts DESC \
                   LIMIT ?1";

        #[allow(clippy::cast_possible_wrap)]
        let params = [Value::BigInt(FETCH_LIMIT as i64)];

        match conn.query_sync(sql, &params) {
            Ok(rows) => {
                self.entries.clear();
                for row in &rows {
                    let message_id: i64 = row.get_named("message_id").unwrap_or(0);
                    let subject: String = row.get_named("subject").unwrap_or_default();
                    let attachments_json: String = row.get_named("attachments").unwrap_or_default();
                    let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
                    let thread_id: Option<String> = row.get_named("thread_id").ok();
                    let sender_name: String = row.get_named("sender_name").unwrap_or_default();
                    let project_slug: String = row.get_named("project_slug").unwrap_or_default();

                    // Parse attachment JSON array
                    if let Ok(attachments) =
                        serde_json::from_str::<Vec<serde_json::Value>>(&attachments_json)
                    {
                        for att in &attachments {
                            let media_type = att
                                .get("media_type")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            let bytes = att
                                .get("bytes")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let sha1 = att
                                .get("sha1")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            #[allow(clippy::cast_possible_truncation)]
                            let width = att
                                .get("width")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0) as u32;
                            #[allow(clippy::cast_possible_truncation)]
                            let height = att
                                .get("height")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0) as u32;
                            let mode = att
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            let path = att
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .map(String::from);

                            self.entries.push(AttachmentEntry {
                                media_type,
                                bytes,
                                sha1,
                                width,
                                height,
                                mode,
                                path,
                                message_id,
                                sender_name: sender_name.clone(),
                                subject: subject.clone(),
                                thread_id: thread_id.clone(),
                                created_ts,
                                project_slug: project_slug.clone(),
                            });
                        }
                    }
                }
                self.last_error = None;
                self.rebuild_display();
            }
            Err(e) => {
                self.last_error = Some(format!("Query failed: {e}"));
            }
        }

        self.db_conn = Some(conn);
        self.data_dirty = false;
    }

    fn rebuild_display(&mut self) {
        let filter = &self.text_filter;
        let media = self.media_filter;

        self.display_indices = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if !media.matches(&e.media_type) {
                    return false;
                }
                if !filter.is_empty() {
                    let lower = filter.to_lowercase();
                    let matches = e.media_type.to_lowercase().contains(&lower)
                        || e.sender_name.to_lowercase().contains(&lower)
                        || e.subject.to_lowercase().contains(&lower)
                        || e.project_slug.to_lowercase().contains(&lower)
                        || e.sha1.contains(&lower);
                    if !matches {
                        return false;
                    }
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        // Sort
        let col = self.sort_col;
        let asc = self.sort_asc;
        let entries = &self.entries;
        self.display_indices.sort_by(|&a, &b| {
            let ea = &entries[a];
            let eb = &entries[b];
            let cmp = match col {
                COL_MEDIA => ea.media_type.cmp(&eb.media_type),
                COL_SIZE => ea.bytes.cmp(&eb.bytes),
                COL_SENDER => ea
                    .sender_name
                    .to_lowercase()
                    .cmp(&eb.sender_name.to_lowercase()),
                COL_SUBJECT => ea.subject.to_lowercase().cmp(&eb.subject.to_lowercase()),
                COL_DATE => ea.created_ts.cmp(&eb.created_ts),
                COL_PROJECT => ea
                    .project_slug
                    .to_lowercase()
                    .cmp(&eb.project_slug.to_lowercase()),
                _ => std::cmp::Ordering::Equal,
            };
            if asc { cmp } else { cmp.reverse() }
        });

        // Clamp selection
        if let Some(sel) = self.table_state.selected {
            if sel >= self.display_indices.len() {
                self.table_state.selected = if self.display_indices.is_empty() {
                    None
                } else {
                    Some(self.display_indices.len() - 1)
                };
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.display_indices.is_empty() {
            return;
        }
        let len = self.display_indices.len();
        let current = self.table_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
        self.detail_scroll = 0;
    }

    fn selected_entry(&self) -> Option<&AttachmentEntry> {
        self.table_state
            .selected
            .and_then(|i| self.display_indices.get(i))
            .map(|&idx| &self.entries[idx])
    }

    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self.selected_entry().map(|e| {
            crate::tui_events::MailEvent::message_sent(
                e.message_id,
                &e.sender_name,
                Vec::new(),
                &e.subject,
                e.thread_id.as_deref().unwrap_or(""),
                &e.project_slug,
            )
        });
    }

    /// Summary statistics for the header line.
    fn summary(&self) -> (usize, u64) {
        let total = self.display_indices.len();
        let total_bytes: u64 = self
            .display_indices
            .iter()
            .map(|&i| self.entries[i].bytes)
            .sum();
        (total, total_bytes)
    }

    #[allow(clippy::cast_precision_loss)]
    fn format_total_size(bytes: u64) -> String {
        if bytes < 1024 {
            format!("{bytes} B")
        } else if bytes < 1_048_576 {
            format!("{:.1} KB", bytes as f64 / 1024.0)
        } else if bytes < 1_073_741_824 {
            format!("{:.1} MB", bytes as f64 / 1_048_576.0)
        } else {
            format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
        }
    }

    /// Build table rows from the current display indices.
    fn build_table_rows(&self) -> Vec<Row> {
        self.display_indices
            .iter()
            .enumerate()
            .map(|(i, &idx)| {
                let e = &self.entries[idx];
                let date = micros_to_iso(e.created_ts);
                let date_short = if date.len() > 19 { &date[..19] } else { &date };
                let subject_trunc: String = if e.subject.chars().count() > 40 {
                    let head: String = e.subject.chars().take(37).collect();
                    format!("{head}...")
                } else {
                    e.subject.clone()
                };

                let tp = crate::tui_theme::TuiThemePalette::current();
                let style = if Some(i) == self.table_state.selected {
                    Style::default()
                        .fg(tp.selection_fg)
                        .bg(tp.selection_bg)
                } else {
                    Style::default()
                };

                Row::new([
                    e.type_label().to_string(),
                    e.size_display(),
                    e.sender_name.clone(),
                    subject_trunc,
                    date_short.to_string(),
                    e.project_slug.clone(),
                ])
                .style(style)
            })
            .collect()
    }

    /// Render the summary header line.
    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let (count, total_bytes) = self.summary();
        let sort_indicator = if self.sort_asc {
            "\u{25b2}"
        } else {
            "\u{25bc}"
        };
        let sort_label = SORT_LABELS.get(self.sort_col).unwrap_or(&"?");
        let filter_label = self.media_filter.label();
        let filter_text = if self.text_filter.is_empty() {
            String::new()
        } else if self.text_filter_active {
            format!(" | Search: {}|", self.text_filter)
        } else {
            format!(" | Search: {}", self.text_filter)
        };

        let summary = format!(
            " {count} attachments ({}) | Filter: {filter_label} | Sort: {sort_label}{sort_indicator}{filter_text}",
            Self::format_total_size(total_bytes),
        );

        let summary_style = if self.text_filter_active {
            let tp = crate::tui_theme::TuiThemePalette::current();
            Style::default().fg(tp.status_accent)
        } else {
            Style::default()
        };
        let p = Paragraph::new(summary).style(summary_style);
        p.render(area, frame);
    }

    /// Render the detail panel for the selected attachment.
    fn render_detail(&self, frame: &mut Frame<'_>, area: Rect, entry: &AttachmentEntry) {
        if area.height < 2 || area.width < 20 {
            return;
        }

        let mut lines = Vec::new();
        lines.push(format!("Type: {}", entry.media_type));
        lines.push(format!("Size: {}", entry.size_display()));
        lines.push(format!("Mode: {}", entry.mode));
        lines.push(format!("SHA-1: {}", entry.sha1));
        let dims = entry.dims_display();
        if !dims.is_empty() {
            lines.push(format!("Dimensions: {dims}"));
        }
        if let Some(p) = &entry.path {
            lines.push(format!("Path: {p}"));
        }
        lines.push(String::new());
        lines.push("--- Provenance ---".to_string());
        lines.push(format!("Message ID: {}", entry.message_id));
        lines.push(format!("Sender: {}", entry.sender_name));
        lines.push(format!("Subject: {}", entry.subject));
        if let Some(tid) = &entry.thread_id {
            lines.push(format!("Thread: {tid}"));
        }
        lines.push(format!("Date: {}", micros_to_iso(entry.created_ts)));
        lines.push(format!("Project: {}", entry.project_slug));

        // Apply scroll offset
        let visible: Vec<String> = lines
            .into_iter()
            .skip(self.detail_scroll)
            .take(area.height as usize)
            .collect();
        let text = visible.join("\n");

        let block = Block::default()
            .title("Attachment Detail")
            .border_type(BorderType::Rounded);
        let p = Paragraph::new(text).block(block);
        p.render(area, frame);
    }
}

impl Default for AttachmentExplorerScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for AttachmentExplorerScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Press {
                return Cmd::None;
            }

            // Text filter input mode
            if self.text_filter_active {
                match key.code {
                    KeyCode::Escape => {
                        self.text_filter_active = false;
                    }
                    KeyCode::Enter => {
                        self.text_filter_active = false;
                        self.rebuild_display();
                    }
                    KeyCode::Backspace => {
                        self.text_filter.pop();
                        self.rebuild_display();
                    }
                    KeyCode::Char(c) => {
                        self.text_filter.push(c);
                        self.rebuild_display();
                    }
                    _ => {}
                }
                return Cmd::None;
            }

            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Char('G') | KeyCode::End => {
                    if !self.display_indices.is_empty() {
                        self.table_state.selected = Some(self.display_indices.len() - 1);
                        self.detail_scroll = 0;
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if !self.display_indices.is_empty() {
                        self.table_state.selected = Some(0);
                        self.detail_scroll = 0;
                    }
                }
                KeyCode::Char('/') => {
                    self.text_filter_active = true;
                }
                KeyCode::Char('s') => {
                    self.sort_col = (self.sort_col + 1) % SORT_LABELS.len();
                    self.rebuild_display();
                }
                KeyCode::Char('S') => {
                    self.sort_asc = !self.sort_asc;
                    self.rebuild_display();
                }
                KeyCode::Char('f') => {
                    self.media_filter = self.media_filter.next();
                    self.rebuild_display();
                }
                KeyCode::Char('r') => {
                    self.data_dirty = true;
                }
                KeyCode::Char('J') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Enter => {
                    // Deep-link to source message
                    if let Some(entry) = self.selected_entry() {
                        return Cmd::msg(MailScreenMsg::DeepLink(DeepLinkTarget::MessageById(
                            entry.message_id,
                        )));
                    }
                }
                KeyCode::Char('t') => {
                    // Deep-link to source thread
                    if let Some(entry) = self.selected_entry() {
                        if let Some(tid) = &entry.thread_id {
                            return Cmd::msg(MailScreenMsg::DeepLink(DeepLinkTarget::ThreadById(
                                tid.clone(),
                            )));
                        }
                    }
                }
                KeyCode::Escape => {
                    if !self.text_filter.is_empty() {
                        self.text_filter.clear();
                        self.rebuild_display();
                    }
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        if self.data_dirty
            || tick_count.saturating_sub(self.last_reload_tick) >= RELOAD_INTERVAL_TICKS
        {
            self.load_attachments(state);
            self.last_reload_tick = tick_count;
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 3 || area.width < 40 {
            return;
        }

        let header_h = 1_u16;
        let has_detail = self.selected_entry().is_some();
        let detail_h = if has_detail && area.height > 12 {
            area.height.min(40) / 3
        } else {
            0
        };
        let table_h = area
            .height
            .saturating_sub(header_h)
            .saturating_sub(detail_h);

        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let table_area = Rect::new(area.x, area.y + header_h, area.width, table_h);
        let detail_area = Rect::new(area.x, area.y + header_h + table_h, area.width, detail_h);

        self.render_header(frame, header_area);

        if let Some(err) = &self.last_error {
            let tp = crate::tui_theme::TuiThemePalette::current();
            let err_p = Paragraph::new(format!(" Error: {err}"))
                .style(Style::default().fg(tp.severity_error));
            err_p.render(table_area, frame);
            return;
        }

        let header_row = Row::new(["Type", "Size", "Sender", "Subject", "Date", "Project"])
            .style(Style::default().bold());
        let rows = self.build_table_rows();

        let widths = [
            Constraint::Percentage(10.0),
            Constraint::Percentage(10.0),
            Constraint::Percentage(15.0),
            Constraint::Percentage(30.0),
            Constraint::Percentage(20.0),
            Constraint::Percentage(15.0),
        ];

        let block = Block::default()
            .title("Attachments")
            .border_type(BorderType::Rounded);

        let table = Table::new(rows, widths)
            .header(header_row)
            .block(block)
            .highlight_style({
                let tp = crate::tui_theme::TuiThemePalette::current();
                Style::default()
                    .fg(tp.selection_fg)
                    .bg(tp.selection_bg)
            });

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);

        // ── Detail panel ──────────────────────────────────────────
        if detail_h > 0 {
            if let Some(entry) = self.selected_entry() {
                let entry_clone = entry.clone();
                self.render_detail(frame, detail_area, &entry_clone);
            }
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate attachments",
            },
            HelpEntry {
                key: "/",
                action: "Search/filter",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort column",
            },
            HelpEntry {
                key: "S",
                action: "Toggle sort order",
            },
            HelpEntry {
                key: "f",
                action: "Cycle media filter",
            },
            HelpEntry {
                key: "Enter",
                action: "Go to source message",
            },
            HelpEntry {
                key: "t",
                action: "Go to source thread",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail panel",
            },
            HelpEntry {
                key: "r",
                action: "Reload data",
            },
        ]
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::MessageById(msg_id) = target {
            // Find the first attachment from this message
            if let Some(pos) = self
                .display_indices
                .iter()
                .position(|&idx| self.entries[idx].message_id == *msg_id)
            {
                self.table_state.selected = Some(pos);
                self.detail_scroll = 0;
                return true;
            }
        }
        false
    }

    fn consumes_text_input(&self) -> bool {
        self.text_filter_active
    }

    fn title(&self) -> &'static str {
        "Attachments"
    }

    fn tab_label(&self) -> &'static str {
        "Attach"
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::Config;

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
    }

    #[test]
    fn new_screen_defaults() {
        let screen = AttachmentExplorerScreen::new();
        assert!(screen.entries.is_empty());
        assert!(screen.display_indices.is_empty());
        assert_eq!(screen.sort_col, COL_DATE);
        assert!(!screen.sort_asc);
        assert_eq!(screen.media_filter, MediaFilter::All);
        assert!(screen.text_filter.is_empty());
        assert!(!screen.text_filter_active);
    }

    #[test]
    fn default_impl() {
        let screen = AttachmentExplorerScreen::default();
        assert!(screen.entries.is_empty());
    }

    #[test]
    fn title_and_label() {
        let screen = AttachmentExplorerScreen::new();
        assert_eq!(screen.title(), "Attachments");
        assert_eq!(screen.tab_label(), "Attach");
    }

    #[test]
    fn keybindings_documented() {
        let screen = AttachmentExplorerScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 5);
        assert!(bindings.iter().any(|b| b.key == "/"));
        assert!(bindings.iter().any(|b| b.key == "f"));
        assert!(bindings.iter().any(|b| b.key == "Enter"));
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = AttachmentExplorerScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = AttachmentExplorerScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(40, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 40, 3), &state);
    }

    #[test]
    fn renders_tiny_without_panic() {
        let state = test_state();
        let screen = AttachmentExplorerScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn media_filter_cycles() {
        assert_eq!(MediaFilter::All.next(), MediaFilter::Images);
        assert_eq!(MediaFilter::Images.next(), MediaFilter::Documents);
        assert_eq!(MediaFilter::Documents.next(), MediaFilter::Other);
        assert_eq!(MediaFilter::Other.next(), MediaFilter::All);
    }

    #[test]
    fn media_filter_matches() {
        assert!(MediaFilter::All.matches("image/webp"));
        assert!(MediaFilter::All.matches("application/pdf"));
        assert!(MediaFilter::Images.matches("image/webp"));
        assert!(MediaFilter::Images.matches("image/png"));
        assert!(!MediaFilter::Images.matches("application/pdf"));
        assert!(MediaFilter::Documents.matches("application/pdf"));
        assert!(MediaFilter::Documents.matches("text/plain"));
        assert!(!MediaFilter::Documents.matches("image/webp"));
        assert!(MediaFilter::Other.matches("application/octet-stream"));
        assert!(!MediaFilter::Other.matches("image/webp"));
    }

    #[test]
    fn media_filter_labels() {
        assert_eq!(MediaFilter::All.label(), "All");
        assert_eq!(MediaFilter::Images.label(), "Images");
        assert_eq!(MediaFilter::Documents.label(), "Docs");
        assert_eq!(MediaFilter::Other.label(), "Other");
    }

    #[test]
    fn size_display_formatting() {
        let entry = AttachmentEntry {
            media_type: "image/webp".to_string(),
            bytes: 500,
            sha1: String::new(),
            width: 0,
            height: 0,
            mode: "inline".to_string(),
            path: None,
            message_id: 1,
            sender_name: String::new(),
            subject: String::new(),
            thread_id: None,
            created_ts: 0,
            project_slug: String::new(),
        };
        assert_eq!(entry.size_display(), "500 B");

        let kb_entry = AttachmentEntry {
            bytes: 2048,
            ..entry.clone()
        };
        assert_eq!(kb_entry.size_display(), "2.0 KB");

        let mb_entry = AttachmentEntry {
            bytes: 2_097_152,
            ..entry
        };
        assert_eq!(mb_entry.size_display(), "2.0 MB");
    }

    #[test]
    fn type_label_extraction() {
        let entry = AttachmentEntry {
            media_type: "image/webp".to_string(),
            bytes: 0,
            sha1: String::new(),
            width: 0,
            height: 0,
            mode: "inline".to_string(),
            path: None,
            message_id: 1,
            sender_name: String::new(),
            subject: String::new(),
            thread_id: None,
            created_ts: 0,
            project_slug: String::new(),
        };
        assert_eq!(entry.type_label(), "webp");

        let pdf = AttachmentEntry {
            media_type: "application/pdf".to_string(),
            ..entry
        };
        assert_eq!(pdf.type_label(), "pdf");
    }

    #[test]
    fn dims_display() {
        let entry = AttachmentEntry {
            media_type: String::new(),
            bytes: 0,
            sha1: String::new(),
            width: 800,
            height: 600,
            mode: "inline".to_string(),
            path: None,
            message_id: 1,
            sender_name: String::new(),
            subject: String::new(),
            thread_id: None,
            created_ts: 0,
            project_slug: String::new(),
        };
        assert_eq!(entry.dims_display(), "800x600");

        let no_dims = AttachmentEntry {
            width: 0,
            height: 0,
            ..entry
        };
        assert_eq!(no_dims.dims_display(), "");
    }

    #[test]
    fn f_cycles_media_filter() {
        let state = test_state();
        let mut screen = AttachmentExplorerScreen::new();
        assert_eq!(screen.media_filter, MediaFilter::All);
        let f = Event::Key(ftui::KeyEvent::new(KeyCode::Char('f')));
        screen.update(&f, &state);
        assert_eq!(screen.media_filter, MediaFilter::Images);
        screen.update(&f, &state);
        assert_eq!(screen.media_filter, MediaFilter::Documents);
    }

    #[test]
    fn s_cycles_sort_column() {
        let state = test_state();
        let mut screen = AttachmentExplorerScreen::new();
        let initial = screen.sort_col;
        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn slash_activates_text_filter() {
        let state = test_state();
        let mut screen = AttachmentExplorerScreen::new();
        assert!(!screen.text_filter_active);
        assert!(!screen.consumes_text_input());
        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);
        assert!(screen.text_filter_active);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn text_filter_input_and_escape() {
        let state = test_state();
        let mut screen = AttachmentExplorerScreen::new();
        let slash = Event::Key(ftui::KeyEvent::new(KeyCode::Char('/')));
        screen.update(&slash, &state);

        let a = Event::Key(ftui::KeyEvent::new(KeyCode::Char('a')));
        screen.update(&a, &state);
        assert_eq!(screen.text_filter, "a");

        let esc = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc, &state);
        assert!(!screen.text_filter_active);
    }

    #[test]
    fn rebuild_display_with_entries() {
        let mut screen = AttachmentExplorerScreen::new();
        screen.entries.push(AttachmentEntry {
            media_type: "image/webp".to_string(),
            bytes: 1000,
            sha1: "abc123".to_string(),
            width: 100,
            height: 100,
            mode: "inline".to_string(),
            path: None,
            message_id: 1,
            sender_name: "TestAgent".to_string(),
            subject: "Test subject".to_string(),
            thread_id: Some("thread-1".to_string()),
            created_ts: 1_000_000,
            project_slug: "proj".to_string(),
        });
        screen.entries.push(AttachmentEntry {
            media_type: "application/pdf".to_string(),
            bytes: 5000,
            sha1: "def456".to_string(),
            width: 0,
            height: 0,
            mode: "file".to_string(),
            path: Some("docs/test.pdf".to_string()),
            message_id: 2,
            sender_name: "OtherAgent".to_string(),
            subject: "Another subject".to_string(),
            thread_id: None,
            created_ts: 2_000_000,
            project_slug: "proj2".to_string(),
        });

        screen.rebuild_display();
        assert_eq!(screen.display_indices.len(), 2);

        // Filter to images only
        screen.media_filter = MediaFilter::Images;
        screen.rebuild_display();
        assert_eq!(screen.display_indices.len(), 1);
        assert_eq!(
            screen.entries[screen.display_indices[0]].media_type,
            "image/webp"
        );
    }

    #[test]
    fn text_filter_narrows_results() {
        let mut screen = AttachmentExplorerScreen::new();
        screen.entries.push(AttachmentEntry {
            media_type: "image/webp".to_string(),
            bytes: 1000,
            sha1: "abc".to_string(),
            width: 0,
            height: 0,
            mode: "inline".to_string(),
            path: None,
            message_id: 1,
            sender_name: "Alice".to_string(),
            subject: "Hello".to_string(),
            thread_id: None,
            created_ts: 1_000_000,
            project_slug: "proj".to_string(),
        });
        screen.entries.push(AttachmentEntry {
            media_type: "image/png".to_string(),
            bytes: 2000,
            sha1: "def".to_string(),
            width: 0,
            height: 0,
            mode: "file".to_string(),
            path: None,
            message_id: 2,
            sender_name: "Bob".to_string(),
            subject: "World".to_string(),
            thread_id: None,
            created_ts: 2_000_000,
            project_slug: "proj".to_string(),
        });

        screen.text_filter = "alice".to_string();
        screen.rebuild_display();
        assert_eq!(screen.display_indices.len(), 1);
    }

    #[test]
    fn format_total_size_values() {
        assert_eq!(AttachmentExplorerScreen::format_total_size(500), "500 B");
        assert_eq!(AttachmentExplorerScreen::format_total_size(2048), "2.0 KB");
        assert_eq!(
            AttachmentExplorerScreen::format_total_size(2_097_152),
            "2.0 MB"
        );
        assert_eq!(
            AttachmentExplorerScreen::format_total_size(2_147_483_648),
            "2.00 GB"
        );
    }

    #[test]
    fn deep_link_message_by_id() {
        let mut screen = AttachmentExplorerScreen::new();
        screen.entries.push(AttachmentEntry {
            media_type: "image/webp".to_string(),
            bytes: 100,
            sha1: String::new(),
            width: 0,
            height: 0,
            mode: "inline".to_string(),
            path: None,
            message_id: 42,
            sender_name: "Agent".to_string(),
            subject: "Test".to_string(),
            thread_id: None,
            created_ts: 1_000_000,
            project_slug: "proj".to_string(),
        });
        screen.display_indices = vec![0];

        let handled = screen.receive_deep_link(&DeepLinkTarget::MessageById(42));
        assert!(handled);
        assert_eq!(screen.table_state.selected, Some(0));

        let not_handled = screen.receive_deep_link(&DeepLinkTarget::MessageById(99));
        assert!(!not_handled);
    }

    #[test]
    fn move_selection_clamps() {
        let mut screen = AttachmentExplorerScreen::new();
        screen.display_indices = vec![0, 1, 2];
        screen.table_state.selected = Some(0);

        screen.move_selection(-1);
        assert_eq!(screen.table_state.selected, Some(0));

        screen.move_selection(10);
        assert_eq!(screen.table_state.selected, Some(2));
    }

    #[test]
    fn move_selection_empty() {
        let mut screen = AttachmentExplorerScreen::new();
        screen.move_selection(1);
        assert_eq!(screen.table_state.selected, None);
    }
}
