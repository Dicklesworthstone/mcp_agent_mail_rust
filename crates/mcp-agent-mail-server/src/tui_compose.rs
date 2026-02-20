//! Human Overseer Message Compose Panel — floating overlay for composing
//! and sending messages from the TUI.
//!
//! Opened via `Ctrl+N` from any screen. The panel floats over the active
//! screen at 70% width / 80% height and provides fields for recipients,
//! subject, body, importance, and thread ID.
//!
//! ## Integration
//!
//! The [`MailAppModel`](crate::tui_app::MailAppModel) manages a
//! `Option<ComposeState>` and renders [`ComposePanel`] when present.
//! This module is deliberately self-contained so the overlay logic
//! can be developed and tested independently of `tui_app.rs`.

use ftui::layout::Rect;
use ftui::{Cell, Frame, KeyCode, KeyEvent, KeyEventKind, Modifiers, PackedRgba};

use crate::tui_bridge::TuiSharedState;

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Maximum subject length (matches server-side truncation).
const MAX_SUBJECT_LEN: usize = 200;

/// Maximum body length (reasonable upper bound for TUI compose).
const MAX_BODY_LEN: usize = 50_000;

/// Overseer agent name used as the sender identity.
pub const OVERSEER_AGENT_NAME: &str = "HumanOverseer";

// ── Palette ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct ComposePalette {
    bg: PackedRgba,
    border: PackedRgba,
    title_fg: PackedRgba,
    label_fg: PackedRgba,
    value_fg: PackedRgba,
    active_border: PackedRgba,
    placeholder_fg: PackedRgba,
    error_fg: PackedRgba,
    hint_fg: PackedRgba,
    selected_recipient_bg: PackedRgba,
    cursor_fg: PackedRgba,
}

fn compose_palette() -> ComposePalette {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let active_luma = 299_u32
        .saturating_mul(u32::from(tp.panel_border_focused.r()))
        .saturating_add(587_u32.saturating_mul(u32::from(tp.panel_border_focused.g())))
        .saturating_add(114_u32.saturating_mul(u32::from(tp.panel_border_focused.b())));
    ComposePalette {
        bg: tp.bg_overlay,
        border: tp.panel_border,
        title_fg: tp.panel_title_fg,
        label_fg: tp.text_muted,
        value_fg: tp.text_primary,
        active_border: tp.panel_border_focused,
        placeholder_fg: tp.text_disabled,
        error_fg: tp.severity_error,
        hint_fg: tp.text_secondary,
        selected_recipient_bg: tp.list_hover_bg,
        cursor_fg: if active_luma >= 150_000 {
            tp.bg_deep
        } else {
            tp.text_primary
        },
    }
}

// ──────────────────────────────────────────────────────────────────────
// ComposeField — which field is active
// ──────────────────────────────────────────────────────────────────────

/// Identifies the active input field in the compose panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComposeField {
    Recipients,
    Subject,
    Body,
    Importance,
    ThreadId,
}

impl ComposeField {
    /// All fields in tab order.
    pub const ALL: &[Self] = &[
        Self::Recipients,
        Self::Subject,
        Self::Body,
        Self::Importance,
        Self::ThreadId,
    ];

    /// Advance to the next field (wraps).
    #[must_use]
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Go to the previous field (wraps).
    #[must_use]
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        let len = Self::ALL.len();
        Self::ALL[(idx + len - 1) % len]
    }

    /// Human-readable label for this field.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recipients => "To",
            Self::Subject => "Subject",
            Self::Body => "Body",
            Self::Importance => "Importance",
            Self::ThreadId => "Thread ID",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Importance
// ──────────────────────────────────────────────────────────────────────

/// Message importance level, matching MCP tool schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    Low,
    Normal,
    High,
    Urgent,
}

impl Importance {
    /// All variants in cycle order.
    pub const ALL: &[Self] = &[Self::Low, Self::Normal, Self::High, Self::Urgent];

    /// Advance to next importance level (wraps).
    #[must_use]
    pub fn cycle_next(self) -> Self {
        let idx = Self::ALL.iter().position(|&i| i == self).unwrap_or(1);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// String representation for the MCP tool parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }

    /// Display label with icon.
    #[must_use]
    pub const fn display(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Normal => "Normal",
            Self::High => "High",
            Self::Urgent => "URGENT",
        }
    }

    /// Color for rendering.
    #[must_use]
    pub const fn color(self, tp: &crate::tui_theme::TuiThemePalette) -> PackedRgba {
        match self {
            Self::Low => tp.text_secondary,
            Self::Normal => tp.text_primary,
            Self::High => tp.severity_warn,
            Self::Urgent => tp.severity_error,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// RecipientEntry — a selectable agent in the recipient list
// ──────────────────────────────────────────────────────────────────────

/// A recipient entry in the compose panel's agent list.
#[derive(Debug, Clone)]
pub struct RecipientEntry {
    /// Agent name.
    pub name: String,
    /// Whether this agent is selected as a recipient.
    pub selected: bool,
    /// Recipient kind: To, Cc, or Bcc.
    pub kind: RecipientKind,
}

/// Recipient addressing kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientKind {
    To,
    Cc,
    Bcc,
}

impl RecipientKind {
    /// Cycle to the next kind.
    #[must_use]
    pub const fn cycle(self) -> Self {
        match self {
            Self::To => Self::Cc,
            Self::Cc => Self::Bcc,
            Self::Bcc => Self::To,
        }
    }

    /// Short label for display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::To => "To",
            Self::Cc => "Cc",
            Self::Bcc => "Bcc",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// ComposeState — full form state
// ──────────────────────────────────────────────────────────────────────

/// State for the message compose overlay.
pub struct ComposeState {
    /// Currently active field.
    pub active_field: ComposeField,
    /// Available agents for the recipient list.
    pub recipients: Vec<RecipientEntry>,
    /// Cursor position in the recipients list (for navigation).
    pub recipient_cursor: usize,
    /// Filter text for recipients (typing to narrow the list).
    pub recipient_filter: String,
    /// Subject line.
    pub subject: String,
    /// Subject cursor position.
    pub subject_cursor: usize,
    /// Body text (multi-line).
    pub body: String,
    /// Body cursor line.
    pub body_cursor_line: usize,
    /// Body cursor column.
    pub body_cursor_col: usize,
    /// Body scroll offset (first visible line).
    pub body_scroll: usize,
    /// Importance level.
    pub importance: Importance,
    /// Optional thread ID for replying.
    pub thread_id: String,
    /// Thread ID cursor position.
    pub thread_id_cursor: usize,
    /// Validation error, if any.
    pub error: Option<String>,
    /// Whether the panel has unsaved changes.
    pub dirty: bool,
    /// Whether a send is in flight.
    pub sending: bool,
}

impl ComposeState {
    /// Create a new empty compose state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active_field: ComposeField::Recipients,
            recipients: Vec::new(),
            recipient_cursor: 0,
            recipient_filter: String::new(),
            subject: String::new(),
            subject_cursor: 0,
            body: String::new(),
            body_cursor_line: 0,
            body_cursor_col: 0,
            body_scroll: 0,
            importance: Importance::Normal,
            thread_id: String::new(),
            thread_id_cursor: 0,
            error: None,
            dirty: false,
            sending: false,
        }
    }

    /// Create a compose state pre-filled for replying to an agent.
    #[must_use]
    pub fn reply_to(agent_name: &str) -> Self {
        let mut state = Self::new();
        state.recipients.push(RecipientEntry {
            name: agent_name.to_string(),
            selected: true,
            kind: RecipientKind::To,
        });
        state.active_field = ComposeField::Subject;
        state
    }

    /// Populate the agent list from known agents (fetched from DB).
    pub fn set_available_agents(&mut self, agents: Vec<String>) {
        // Preserve existing selections
        let selected: std::collections::HashSet<String> = self
            .recipients
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.name.clone())
            .collect();
        let kinds: std::collections::HashMap<String, RecipientKind> = self
            .recipients
            .iter()
            .filter(|r| r.selected)
            .map(|r| (r.name.clone(), r.kind))
            .collect();

        self.recipients = agents
            .into_iter()
            .map(|name| {
                let was_selected = selected.contains(&name);
                let kind = kinds.get(&name).copied().unwrap_or(RecipientKind::To);
                RecipientEntry {
                    name,
                    selected: was_selected,
                    kind,
                }
            })
            .collect();
    }

    /// Return the filtered list of recipient indices.
    #[must_use]
    pub fn filtered_recipients(&self) -> Vec<usize> {
        if self.recipient_filter.is_empty() {
            return (0..self.recipients.len()).collect();
        }
        let filter_lower = self.recipient_filter.to_ascii_lowercase();
        self.recipients
            .iter()
            .enumerate()
            .filter(|(_, r)| r.name.to_ascii_lowercase().contains(&filter_lower))
            .map(|(i, _)| i)
            .collect()
    }

    /// Toggle selection of the recipient at the current cursor position.
    pub fn toggle_recipient(&mut self) {
        let filtered = self.filtered_recipients();
        if let Some(&idx) = filtered.get(self.recipient_cursor) {
            self.recipients[idx].selected = !self.recipients[idx].selected;
            self.dirty = true;
        }
    }

    /// Cycle the recipient kind (To/Cc/Bcc) at the current cursor position.
    pub fn cycle_recipient_kind(&mut self) {
        let filtered = self.filtered_recipients();
        if let Some(&idx) = filtered.get(self.recipient_cursor)
            && self.recipients[idx].selected
        {
            self.recipients[idx].kind = self.recipients[idx].kind.cycle();
            self.dirty = true;
        }
    }

    /// Select all visible recipients.
    pub fn select_all_recipients(&mut self) {
        let filtered = self.filtered_recipients();
        for &idx in &filtered {
            self.recipients[idx].selected = true;
        }
        self.dirty = true;
    }

    /// Deselect all recipients.
    pub fn clear_all_recipients(&mut self) {
        for r in &mut self.recipients {
            r.selected = false;
        }
        self.dirty = true;
    }

    /// Count of selected "To" recipients.
    #[must_use]
    pub fn to_recipients(&self) -> Vec<&str> {
        self.recipients
            .iter()
            .filter(|r| r.selected && r.kind == RecipientKind::To)
            .map(|r| r.name.as_str())
            .collect()
    }

    /// Count of selected "Cc" recipients.
    #[must_use]
    pub fn cc_recipients(&self) -> Vec<&str> {
        self.recipients
            .iter()
            .filter(|r| r.selected && r.kind == RecipientKind::Cc)
            .map(|r| r.name.as_str())
            .collect()
    }

    /// Count of selected "Bcc" recipients.
    #[must_use]
    pub fn bcc_recipients(&self) -> Vec<&str> {
        self.recipients
            .iter()
            .filter(|r| r.selected && r.kind == RecipientKind::Bcc)
            .map(|r| r.name.as_str())
            .collect()
    }

    /// Validate the form before sending. Returns `Ok(())` if valid.
    pub fn validate(&mut self) -> Result<(), String> {
        self.error = None;

        let has_any_recipient = self.recipients.iter().any(|r| r.selected);
        if !has_any_recipient {
            let msg = "At least one recipient is required".to_string();
            self.error = Some(msg.clone());
            return Err(msg);
        }

        if self.subject.trim().is_empty() {
            let msg = "Subject is required".to_string();
            self.error = Some(msg.clone());
            return Err(msg);
        }

        if self.subject.len() > MAX_SUBJECT_LEN {
            let msg = format!(
                "Subject too long ({}/{})",
                self.subject.len(),
                MAX_SUBJECT_LEN
            );
            self.error = Some(msg.clone());
            return Err(msg);
        }

        if self.body.trim().is_empty() {
            let msg = "Body is required".to_string();
            self.error = Some(msg.clone());
            return Err(msg);
        }

        if self.body.len() > MAX_BODY_LEN {
            let msg = format!("Body too long ({}/{})", self.body.len(), MAX_BODY_LEN);
            self.error = Some(msg.clone());
            return Err(msg);
        }

        Ok(())
    }

    /// Whether the form has been modified from its initial state.
    #[must_use]
    pub fn has_unsaved_changes(&self) -> bool {
        self.dirty
            || !self.subject.is_empty()
            || !self.body.is_empty()
            || !self.thread_id.is_empty()
            || self.recipients.iter().any(|r| r.selected)
    }

    /// Body lines for rendering.
    #[must_use]
    pub fn body_lines(&self) -> Vec<&str> {
        if self.body.is_empty() {
            return vec![""];
        }
        self.body.split('\n').collect()
    }

    /// Handle a key event. Returns a [`ComposeAction`] describing what happened.
    pub fn handle_key(&mut self, key: &KeyEvent) -> ComposeAction {
        if key.kind != KeyEventKind::Press {
            return ComposeAction::Consumed;
        }

        // Global keybindings (work in any field)
        match (key.code, key.modifiers) {
            // Ctrl+Enter: send
            (KeyCode::Enter, m) if m.contains(Modifiers::CTRL) => {
                return ComposeAction::Send;
            }
            // Escape: close
            (KeyCode::Escape, _) => {
                return if self.has_unsaved_changes() {
                    ComposeAction::ConfirmClose
                } else {
                    ComposeAction::Close
                };
            }
            // Tab: next field
            (KeyCode::Tab, m) if !m.contains(Modifiers::SHIFT) => {
                self.active_field = self.active_field.next();
                return ComposeAction::Consumed;
            }
            // Shift+Tab: prev field
            (KeyCode::BackTab | KeyCode::Tab, _) => {
                self.active_field = self.active_field.prev();
                return ComposeAction::Consumed;
            }
            _ => {}
        }

        // Field-specific handling
        match self.active_field {
            ComposeField::Recipients => self.handle_recipients_key(key),
            ComposeField::Subject => self.handle_text_input_key(key, TextTarget::Subject),
            ComposeField::Body => self.handle_body_key(key),
            ComposeField::Importance => self.handle_importance_key(key),
            ComposeField::ThreadId => self.handle_text_input_key(key, TextTarget::ThreadId),
        }
    }

    fn handle_recipients_key(&mut self, key: &KeyEvent) -> ComposeAction {
        let filtered = self.filtered_recipients();
        let filtered_len = filtered.len();

        match key.code {
            KeyCode::Up => {
                if filtered_len > 0 && self.recipient_cursor > 0 {
                    self.recipient_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if filtered_len > 0 && self.recipient_cursor + 1 < filtered_len {
                    self.recipient_cursor += 1;
                }
            }
            KeyCode::Char(' ') => {
                self.toggle_recipient();
            }
            KeyCode::Char('t') if key.modifiers.contains(Modifiers::CTRL) => {
                self.cycle_recipient_kind();
            }
            KeyCode::Char('a') if key.modifiers.contains(Modifiers::CTRL) => {
                self.select_all_recipients();
            }
            KeyCode::Char('d') if key.modifiers.contains(Modifiers::CTRL) => {
                self.clear_all_recipients();
            }
            KeyCode::Backspace => {
                self.recipient_filter.pop();
                self.recipient_cursor = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(Modifiers::CTRL) => {
                self.recipient_filter.push(c);
                self.recipient_cursor = 0;
            }
            _ => return ComposeAction::Ignored,
        }
        ComposeAction::Consumed
    }

    fn handle_text_input_key(&mut self, key: &KeyEvent, target: TextTarget) -> ComposeAction {
        let (text, cursor) = match target {
            TextTarget::Subject => (&mut self.subject, &mut self.subject_cursor),
            TextTarget::ThreadId => (&mut self.thread_id, &mut self.thread_id_cursor),
        };

        match key.code {
            KeyCode::Left => {
                *cursor = cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                if *cursor < char_count(text) {
                    *cursor += 1;
                }
            }
            KeyCode::Home => {
                *cursor = 0;
            }
            KeyCode::End => {
                *cursor = char_count(text);
            }
            KeyCode::Backspace => {
                if *cursor > 0 {
                    let byte_idx = byte_index_from_char_index(text, *cursor - 1);
                    text.remove(byte_idx);
                    *cursor -= 1;
                    self.dirty = true;
                }
            }
            KeyCode::Delete => {
                if *cursor < char_count(text) {
                    let byte_idx = byte_index_from_char_index(text, *cursor);
                    text.remove(byte_idx);
                    self.dirty = true;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(Modifiers::CTRL) => {
                let max = match target {
                    TextTarget::Subject => MAX_SUBJECT_LEN,
                    TextTarget::ThreadId => 200,
                };
                if char_count(text) < max {
                    let byte_idx = byte_index_from_char_index(text, *cursor);
                    text.insert(byte_idx, c);
                    *cursor += 1;
                    self.dirty = true;
                }
            }
            _ => return ComposeAction::Ignored,
        }
        ComposeAction::Consumed
    }

    fn handle_body_key(&mut self, key: &KeyEvent) -> ComposeAction {
        match key.code {
            KeyCode::Enter => {
                if self.body.len() < MAX_BODY_LEN {
                    let offset = self.body_offset();
                    self.body.insert(offset, '\n');
                    self.body_cursor_line += 1;
                    self.body_cursor_col = 0;
                    self.dirty = true;
                }
            }
            KeyCode::Backspace => {
                let offset = self.body_offset();
                if offset > 0 {
                    if self.body_cursor_col > 0 {
                        // Remove char before cursor on current line
                        // body_offset points to char *at* cursor, so we need char *before*
                        // But finding byte index of char before is hard from global offset without scanning back.
                        // Easier: use line-local logic.
                        let lines: Vec<&str> = self.body.split('\n').collect();
                        if let Some(line) = lines.get(self.body_cursor_line) {
                            let byte_in_line =
                                byte_index_from_char_index(line, self.body_cursor_col - 1);
                            // Global offset of that char
                            let line_start = self.body_offset()
                                - byte_index_from_char_index(line, self.body_cursor_col);
                            self.body.remove(line_start + byte_in_line);
                            self.body_cursor_col -= 1;
                        }
                    } else if self.body_cursor_line > 0 {
                        // Joining lines: capture prev line length *before* removal.
                        let prev_line_len = self
                            .body
                            .split('\n')
                            .nth(self.body_cursor_line - 1)
                            .map_or(0, char_count);
                        self.body.remove(offset - 1);
                        self.body_cursor_line -= 1;
                        self.body_cursor_col = prev_line_len;
                    } else {
                        // Start of body, nothing before it (offset > 0 check handles empty body)
                        self.body.remove(offset - 1);
                    }
                    self.dirty = true;
                }
            }
            KeyCode::Left => {
                if self.body_cursor_col > 0 {
                    self.body_cursor_col -= 1;
                } else if self.body_cursor_line > 0 {
                    self.body_cursor_line -= 1;
                    let lines = self.body_lines();
                    self.body_cursor_col = lines
                        .get(self.body_cursor_line)
                        .map_or(0, |l| char_count(l));
                }
            }
            KeyCode::Right => {
                let lines = self.body_lines();
                let line_len = lines
                    .get(self.body_cursor_line)
                    .map_or(0, |l| char_count(l));
                if self.body_cursor_col < line_len {
                    self.body_cursor_col += 1;
                } else if self.body_cursor_line + 1 < lines.len() {
                    self.body_cursor_line += 1;
                    self.body_cursor_col = 0;
                }
            }
            KeyCode::Up => {
                if self.body_cursor_line > 0 {
                    self.body_cursor_line -= 1;
                    let lines = self.body_lines();
                    let line_len = lines
                        .get(self.body_cursor_line)
                        .map_or(0, |l| char_count(l));
                    self.body_cursor_col = self.body_cursor_col.min(line_len);
                }
            }
            KeyCode::Down => {
                let line_count = self.body.split('\n').count();
                if self.body_cursor_line + 1 < line_count {
                    self.body_cursor_line += 1;
                    let line_len = self
                        .body
                        .split('\n')
                        .nth(self.body_cursor_line)
                        .map_or(0, char_count);
                    self.body_cursor_col = self.body_cursor_col.min(line_len);
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(Modifiers::CTRL) => {
                if self.body.len() < MAX_BODY_LEN {
                    let offset = self.body_offset();
                    self.body.insert(offset, c);
                    self.body_cursor_col += 1;
                    self.dirty = true;
                }
            }
            _ => return ComposeAction::Ignored,
        }
        ComposeAction::Consumed
    }

    fn handle_importance_key(&mut self, key: &KeyEvent) -> ComposeAction {
        match key.code {
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Right => {
                self.importance = self.importance.cycle_next();
                self.dirty = true;
                ComposeAction::Consumed
            }
            _ => ComposeAction::Ignored,
        }
    }

    /// Compute the byte offset into `self.body` for the current cursor position.
    fn body_offset(&self) -> usize {
        let mut offset = 0;
        for (i, line) in self.body.split('\n').enumerate() {
            if i == self.body_cursor_line {
                // Convert char col to byte col for this line
                let byte_col = byte_index_from_char_index(line, self.body_cursor_col);
                return offset + byte_col;
            }
            offset += line.len() + 1; // +1 for the '\n'
        }
        self.body.len()
    }
}

impl Default for ComposeState {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Internal marker for which single-line text field is being edited.
#[derive(Clone, Copy)]
enum TextTarget {
    Subject,
    ThreadId,
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn byte_index_from_char_index(s: &str, char_idx: usize) -> usize {
    s.chars().take(char_idx).map(char::len_utf8).sum()
}

// ──────────────────────────────────────────────────────────────────────
// ComposeAction — result of key handling
// ──────────────────────────────────────────────────────────────────────

/// Action produced by [`ComposeState::handle_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeAction {
    /// The key was handled; no further propagation.
    Consumed,
    /// The key was not relevant to the compose panel.
    Ignored,
    /// User wants to send the message (Ctrl+Enter).
    Send,
    /// User wants to close without sending (Esc, no unsaved changes).
    Close,
    /// User wants to close but has unsaved changes — show confirmation.
    ConfirmClose,
}

// ──────────────────────────────────────────────────────────────────────
// ComposeEnvelope — validated send payload
// ──────────────────────────────────────────────────────────────────────

/// Validated message payload ready for dispatch via `send_message`.
///
/// Built from a validated [`ComposeState`] via [`ComposeState::build_envelope`].
/// The caller (typically `tui_app`) passes this to the async send path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeEnvelope {
    /// Sender agent name (always [`OVERSEER_AGENT_NAME`]).
    pub sender_name: String,
    /// Primary recipients (To).
    pub to: Vec<String>,
    /// Carbon-copy recipients.
    pub cc: Vec<String>,
    /// Blind carbon-copy recipients.
    pub bcc: Vec<String>,
    /// Message subject line.
    pub subject: String,
    /// Markdown body.
    pub body_md: String,
    /// Importance level (`"low"`, `"normal"`, `"high"`, `"urgent"`).
    pub importance: String,
    /// Optional thread ID for threading.
    pub thread_id: Option<String>,
}

impl ComposeState {
    /// Validate the form and build a [`ComposeEnvelope`] ready for dispatch.
    ///
    /// Returns `Err(message)` if validation fails. On success the envelope
    /// captures all fields needed by `send_message` in the tools layer.
    pub fn build_envelope(&mut self) -> Result<ComposeEnvelope, String> {
        self.validate()?;

        let to: Vec<String> = self
            .to_recipients()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let cc: Vec<String> = self
            .cc_recipients()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let bcc: Vec<String> = self
            .bcc_recipients()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        let thread_id = if self.thread_id.trim().is_empty() {
            None
        } else {
            Some(self.thread_id.trim().to_owned())
        };

        Ok(ComposeEnvelope {
            sender_name: OVERSEER_AGENT_NAME.to_owned(),
            to,
            cc,
            bcc,
            subject: self.subject.clone(),
            body_md: self.body.clone(),
            importance: self.importance.as_str().to_owned(),
            thread_id,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────
// ComposePanel — rendering
// ──────────────────────────────────────────────────────────────────────

/// Renders the compose panel overlay.
pub struct ComposePanel<'a> {
    state: &'a ComposeState,
}

impl<'a> ComposePanel<'a> {
    /// Create a new compose panel widget.
    #[must_use]
    pub const fn new(state: &'a ComposeState) -> Self {
        Self { state }
    }

    /// Calculate the overlay area (70% width, 80% height, centered).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn overlay_area(terminal: Rect) -> Rect {
        let width = ((u32::from(terminal.width) * 70) / 100) as u16;
        let height = ((u32::from(terminal.height) * 80) / 100) as u16;
        let width = width.max(40).min(terminal.width);
        let height = height.max(15).min(terminal.height);
        let x = (terminal.width.saturating_sub(width)) / 2;
        let y = (terminal.height.saturating_sub(height)) / 2;
        Rect::new(x, y, width, height)
    }

    /// Render the compose panel into the frame.
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn render(&self, terminal_area: Rect, frame: &mut Frame<'_>, _state: &TuiSharedState) {
        let area = Self::overlay_area(terminal_area);
        let cp = compose_palette();
        let tp = crate::tui_theme::TuiThemePalette::current();

        // Fill background
        for row in area.y..area.bottom() {
            for col in area.x..area.right() {
                let mut cell = Cell::from_char(' ');
                cell.bg = cp.bg;
                frame.buffer.set_fast(col, row, cell);
            }
        }

        // Draw border
        self.draw_border(area, &cp, frame);

        // Title bar
        let title = " Compose Message (Ctrl+Enter: Send | Esc: Cancel) ";
        self.draw_text(
            area.x + 2,
            area.y,
            title,
            cp.title_fg,
            cp.bg,
            area.right(),
            frame,
        );

        // Inner content area
        let inner = Rect::new(
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(4),
            area.height.saturating_sub(2),
        );

        let mut y = inner.y;
        let max_y = inner.bottom();
        let inner_w = inner.width as usize;

        // ── Recipients field ───────────────────────────────────
        if y < max_y {
            let is_active = self.state.active_field == ComposeField::Recipients;
            let label_color = if is_active {
                cp.active_border
            } else {
                cp.label_fg
            };

            // Label + selected count
            let selected_count = self.state.recipients.iter().filter(|r| r.selected).count();
            let label = format!("To ({selected_count} selected):");
            self.draw_text(inner.x, y, &label, label_color, cp.bg, inner.right(), frame);
            y += 1;

            // Filter bar (when active)
            if is_active && y < max_y {
                let filter_display = if self.state.recipient_filter.is_empty() {
                    "type to filter..."
                } else {
                    &self.state.recipient_filter
                };
                let filter_color = if self.state.recipient_filter.is_empty() {
                    cp.placeholder_fg
                } else {
                    cp.value_fg
                };
                self.draw_text(
                    inner.x,
                    y,
                    filter_display,
                    filter_color,
                    cp.bg,
                    inner.right(),
                    frame,
                );
                y += 1;
            }

            // Recipient list (show up to 4 rows)
            let filtered = self.state.filtered_recipients();
            let max_rows = if is_active { 4 } else { 2 };
            for (vi, &ri) in filtered.iter().enumerate().take(max_rows) {
                if y >= max_y {
                    break;
                }
                let r = &self.state.recipients[ri];
                let checkbox = if r.selected { "[x]" } else { "[ ]" };
                let kind_label = if r.selected {
                    format!(" ({})", r.kind.label())
                } else {
                    String::new()
                };
                let line = format!(" {checkbox} {}{kind_label}", r.name);

                let (fg, bg) = if is_active && vi == self.state.recipient_cursor {
                    (cp.value_fg, cp.selected_recipient_bg)
                } else if r.selected {
                    (cp.value_fg, cp.bg)
                } else {
                    (cp.placeholder_fg, cp.bg)
                };

                self.draw_text(inner.x, y, &line, fg, bg, inner.right(), frame);
                y += 1;
            }

            if filtered.len() > max_rows && y < max_y {
                let more = format!("  ... +{} more", filtered.len() - max_rows);
                self.draw_text(inner.x, y, &more, cp.hint_fg, cp.bg, inner.right(), frame);
                y += 1;
            }

            y += 1; // spacing
        }

        // ── Subject field ──────────────────────────────────────
        if y < max_y {
            let is_active = self.state.active_field == ComposeField::Subject;
            let label_color = if is_active {
                cp.active_border
            } else {
                cp.label_fg
            };
            let counter = format!(
                "Subject ({}/{}):",
                char_count(&self.state.subject),
                MAX_SUBJECT_LEN
            );
            self.draw_text(
                inner.x,
                y,
                &counter,
                label_color,
                cp.bg,
                inner.right(),
                frame,
            );
            y += 1;

            if y < max_y {
                let display = if self.state.subject.is_empty() && !is_active {
                    "Enter subject..."
                } else if self.state.subject.is_empty() {
                    ""
                } else {
                    &self.state.subject
                };
                let fg = if self.state.subject.is_empty() && !is_active {
                    cp.placeholder_fg
                } else {
                    cp.value_fg
                };
                let border_fg = if is_active {
                    cp.active_border
                } else {
                    cp.border
                };

                // Draw bordered input
                self.draw_bordered_line(inner.x, y, inner_w, display, fg, border_fg, cp.bg, frame);

                // Draw cursor
                if is_active {
                    let cursor_x = inner.x + 1 + self.state.subject_cursor as u16;
                    if cursor_x < inner.right() - 1 {
                        self.set_cursor_cell(cursor_x, y, &cp, frame);
                    }
                }
                y += 1;
            }
            y += 1; // spacing
        }

        // ── Body field ─────────────────────────────────────────
        if y < max_y {
            let is_active = self.state.active_field == ComposeField::Body;
            let label_color = if is_active {
                cp.active_border
            } else {
                cp.label_fg
            };
            let body_label = format!("Body ({} bytes):", self.state.body.len());
            self.draw_text(
                inner.x,
                y,
                &body_label,
                label_color,
                cp.bg,
                inner.right(),
                frame,
            );
            y += 1;

            // Body area: use remaining space minus 4 lines (for importance, thread, hints, error)
            let body_rows = (max_y.saturating_sub(y).saturating_sub(5)) as usize;
            let body_rows = body_rows.max(3);
            let lines = self.state.body_lines();

            let border_fg = if is_active {
                cp.active_border
            } else {
                cp.border
            };

            // Draw body border
            if y < max_y {
                self.draw_horizontal_border(inner.x, y, inner_w, border_fg, cp.bg, frame);
                y += 1;
            }

            for vi in 0..body_rows {
                if y >= max_y {
                    break;
                }
                let line_idx = self.state.body_scroll + vi;
                let line_text = lines.get(line_idx).unwrap_or(&"");
                let truncated: String = line_text.chars().take(inner_w.saturating_sub(2)).collect();
                self.draw_text(
                    inner.x + 1,
                    y,
                    &truncated,
                    cp.value_fg,
                    cp.bg,
                    inner.right() - 1,
                    frame,
                );

                // Cursor in body
                if is_active && line_idx == self.state.body_cursor_line {
                    let cursor_x = inner.x + 1 + self.state.body_cursor_col as u16;
                    if cursor_x < inner.right() - 1 {
                        self.set_cursor_cell(cursor_x, y, &cp, frame);
                    }
                }

                y += 1;
            }

            if y < max_y {
                self.draw_horizontal_border(inner.x, y, inner_w, border_fg, cp.bg, frame);
                y += 1;
            }

            // Placeholder
            if self.state.body.is_empty() && !is_active && body_rows > 0 {
                let placeholder_y = y.saturating_sub(body_rows as u16);
                self.draw_text(
                    inner.x + 1,
                    placeholder_y,
                    "Enter message body...",
                    cp.placeholder_fg,
                    cp.bg,
                    inner.right() - 1,
                    frame,
                );
            }
        }

        // ── Importance field ───────────────────────────────────
        if y < max_y {
            let is_active = self.state.active_field == ComposeField::Importance;
            let label_color = if is_active {
                cp.active_border
            } else {
                cp.label_fg
            };
            self.draw_text(
                inner.x,
                y,
                "Importance: ",
                label_color,
                cp.bg,
                inner.right(),
                frame,
            );
            let imp_x = inner.x + 12;
            let imp_display = self.state.importance.display();
            let imp_color = self.state.importance.color(&tp);
            self.draw_text(
                imp_x,
                y,
                imp_display,
                imp_color,
                cp.bg,
                inner.right(),
                frame,
            );
            if is_active {
                let hint = "  (Space/Enter to cycle)";
                let hint_x = imp_x + imp_display.len() as u16;
                self.draw_text(hint_x, y, hint, cp.hint_fg, cp.bg, inner.right(), frame);
            }
            y += 1;
        }

        // ── Thread ID field ────────────────────────────────────
        if y < max_y {
            let is_active = self.state.active_field == ComposeField::ThreadId;
            let label_color = if is_active {
                cp.active_border
            } else {
                cp.label_fg
            };
            self.draw_text(
                inner.x,
                y,
                "Thread ID (optional): ",
                label_color,
                cp.bg,
                inner.right(),
                frame,
            );
            y += 1;

            if y < max_y {
                let display = if self.state.thread_id.is_empty() && !is_active {
                    "Leave empty for new thread"
                } else if self.state.thread_id.is_empty() {
                    ""
                } else {
                    &self.state.thread_id
                };
                let fg = if self.state.thread_id.is_empty() && !is_active {
                    cp.placeholder_fg
                } else {
                    cp.value_fg
                };
                let border_fg = if is_active {
                    cp.active_border
                } else {
                    cp.border
                };

                self.draw_bordered_line(inner.x, y, inner_w, display, fg, border_fg, cp.bg, frame);

                if is_active {
                    let cursor_x = inner.x + 1 + self.state.thread_id_cursor as u16;
                    if cursor_x < inner.right() - 1 {
                        self.set_cursor_cell(cursor_x, y, &cp, frame);
                    }
                }
                y += 1;
            }
        }

        // ── Error / hints ──────────────────────────────────────
        if y < max_y {
            if let Some(err) = &self.state.error {
                self.draw_text(inner.x, y, err, cp.error_fg, cp.bg, inner.right(), frame);
            } else if self.state.sending {
                self.draw_text(
                    inner.x,
                    y,
                    "Sending...",
                    cp.hint_fg,
                    cp.bg,
                    inner.right(),
                    frame,
                );
            } else {
                let hint = "Tab: next field | Ctrl+Enter: send | Esc: cancel";
                self.draw_text(inner.x, y, hint, cp.hint_fg, cp.bg, inner.right(), frame);
            }
        }
    }

    // ── Drawing helpers ────────────────────────────────────────

    fn draw_border(&self, area: Rect, cp: &ComposePalette, frame: &mut Frame<'_>) {
        let right = area.right().saturating_sub(1);
        let bottom = area.bottom().saturating_sub(1);

        for col in area.x..=right {
            self.set_border_cell(col, area.y, cp, frame);
            self.set_border_cell(col, bottom, cp, frame);
        }
        for row in area.y..=bottom {
            self.set_border_cell(area.x, row, cp, frame);
            self.set_border_cell(right, row, cp, frame);
        }
    }

    #[allow(clippy::unused_self)]
    fn set_border_cell(&self, x: u16, y: u16, cp: &ComposePalette, frame: &mut Frame<'_>) {
        let mut cell = Cell::from_char(' ');
        cell.fg = cp.border;
        cell.bg = cp.border;
        frame.buffer.set_fast(x, y, cell);
    }

    #[allow(clippy::unused_self)]
    fn set_cursor_cell(&self, x: u16, y: u16, cp: &ComposePalette, frame: &mut Frame<'_>) {
        let mut cell = frame
            .buffer
            .get(x, y)
            .copied()
            .unwrap_or_else(|| Cell::from_char(' '));
        cell.bg = cp.active_border;
        cell.fg = cp.cursor_fg;
        frame.buffer.set_fast(x, y, cell);
    }

    #[allow(clippy::too_many_arguments, clippy::unused_self)]
    fn draw_text(
        &self,
        x: u16,
        y: u16,
        text: &str,
        fg: PackedRgba,
        bg: PackedRgba,
        max_x: u16,
        frame: &mut Frame<'_>,
    ) {
        let mut col = x;
        for ch in text.chars() {
            if col >= max_x {
                break;
            }
            let mut cell = Cell::from_char(ch);
            cell.fg = fg;
            cell.bg = bg;
            frame.buffer.set_fast(col, y, cell);
            col += 1;
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::unused_self,
        clippy::cast_possible_truncation
    )]
    fn draw_bordered_line(
        &self,
        x: u16,
        y: u16,
        width: usize,
        text: &str,
        fg: PackedRgba,
        border_fg: PackedRgba,
        bg: PackedRgba,
        frame: &mut Frame<'_>,
    ) {
        let right = x + width as u16;

        // Left border
        let mut cell = Cell::from_char('[');
        cell.fg = border_fg;
        cell.bg = bg;
        frame.buffer.set_fast(x, y, cell);

        // Content
        let content_width = width.saturating_sub(2);
        let mut col = x + 1;
        for ch in text.chars().take(content_width) {
            let mut c = Cell::from_char(ch);
            c.fg = fg;
            c.bg = bg;
            frame.buffer.set_fast(col, y, c);
            col += 1;
        }
        // Pad remaining
        while col < right.saturating_sub(1) {
            let mut c = Cell::from_char(' ');
            c.bg = bg;
            frame.buffer.set_fast(col, y, c);
            col += 1;
        }

        // Right border
        let mut cell = Cell::from_char(']');
        cell.fg = border_fg;
        cell.bg = bg;
        frame.buffer.set_fast(right.saturating_sub(1), y, cell);
    }

    #[allow(clippy::unused_self, clippy::cast_possible_truncation)]
    fn draw_horizontal_border(
        &self,
        x: u16,
        y: u16,
        width: usize,
        fg: PackedRgba,
        bg: PackedRgba,
        frame: &mut Frame<'_>,
    ) {
        for col in x..x + width as u16 {
            let mut cell = Cell::from_char('-');
            cell.fg = fg;
            cell.bg = bg;
            frame.buffer.set_fast(col, y, cell);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code)
    }

    fn make_key_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code).with_modifiers(Modifiers::CTRL)
    }

    fn state_with_agents(names: &[&str]) -> ComposeState {
        let mut s = ComposeState::new();
        s.set_available_agents(names.iter().map(std::string::ToString::to_string).collect());
        s
    }

    // ── ComposeField ───────────────────────────────────────────

    #[test]
    fn field_next_cycles_through_all() {
        let mut f = ComposeField::Recipients;
        let mut visited = vec![f];
        for _ in 0..ComposeField::ALL.len() {
            f = f.next();
            visited.push(f);
        }
        assert_eq!(visited.first(), visited.last());
        assert_eq!(visited.len(), ComposeField::ALL.len() + 1);
    }

    #[test]
    fn field_prev_cycles_through_all() {
        let mut f = ComposeField::Recipients;
        let mut visited = vec![f];
        for _ in 0..ComposeField::ALL.len() {
            f = f.prev();
            visited.push(f);
        }
        assert_eq!(visited.first(), visited.last());
    }

    #[test]
    fn field_next_prev_roundtrip() {
        for &f in ComposeField::ALL {
            assert_eq!(f.next().prev(), f);
            assert_eq!(f.prev().next(), f);
        }
    }

    #[test]
    fn field_labels_non_empty() {
        for &f in ComposeField::ALL {
            assert!(!f.label().is_empty());
        }
    }

    // ── Importance ─────────────────────────────────────────────

    #[test]
    fn importance_cycle_wraps() {
        let mut imp = Importance::Low;
        for _ in 0..Importance::ALL.len() {
            imp = imp.cycle_next();
        }
        assert_eq!(imp, Importance::Low);
    }

    #[test]
    fn importance_as_str_values() {
        assert_eq!(Importance::Low.as_str(), "low");
        assert_eq!(Importance::Normal.as_str(), "normal");
        assert_eq!(Importance::High.as_str(), "high");
        assert_eq!(Importance::Urgent.as_str(), "urgent");
    }

    #[test]
    fn importance_display_values() {
        assert_eq!(Importance::Urgent.display(), "URGENT");
        assert_eq!(Importance::Normal.display(), "Normal");
    }

    // ── RecipientKind ──────────────────────────────────────────

    #[test]
    fn recipient_kind_cycles() {
        assert_eq!(RecipientKind::To.cycle(), RecipientKind::Cc);
        assert_eq!(RecipientKind::Cc.cycle(), RecipientKind::Bcc);
        assert_eq!(RecipientKind::Bcc.cycle(), RecipientKind::To);
    }

    // ── ComposeState basics ────────────────────────────────────

    #[test]
    fn new_state_is_clean() {
        let s = ComposeState::new();
        assert_eq!(s.active_field, ComposeField::Recipients);
        assert!(!s.dirty);
        assert!(!s.sending);
        assert!(s.subject.is_empty());
        assert!(s.body.is_empty());
        assert_eq!(s.importance, Importance::Normal);
        assert!(!s.has_unsaved_changes());
    }

    #[test]
    fn reply_to_preselects_agent() {
        let s = ComposeState::reply_to("GoldHawk");
        assert_eq!(s.recipients.len(), 1);
        assert!(s.recipients[0].selected);
        assert_eq!(s.recipients[0].name, "GoldHawk");
        assert_eq!(s.active_field, ComposeField::Subject);
    }

    #[test]
    fn set_available_agents_preserves_selections() {
        let mut s = ComposeState::new();
        s.set_available_agents(vec!["Red".into(), "Blue".into(), "Green".into()]);
        s.recipients[1].selected = true; // Select Blue
        s.recipients[1].kind = RecipientKind::Cc;

        // Re-set with different order
        s.set_available_agents(vec![
            "Green".into(),
            "Blue".into(),
            "Red".into(),
            "Gold".into(),
        ]);
        assert_eq!(s.recipients.len(), 4);
        let blue = s.recipients.iter().find(|r| r.name == "Blue").unwrap();
        assert!(blue.selected);
        assert_eq!(blue.kind, RecipientKind::Cc);
        let gold = s.recipients.iter().find(|r| r.name == "Gold").unwrap();
        assert!(!gold.selected);
    }

    // ── Recipient filtering ────────────────────────────────────

    #[test]
    fn filter_narrows_recipient_list() {
        let mut s = state_with_agents(&["GoldHawk", "SilverFox", "GreenLake"]);
        s.recipient_filter = "g".into();
        let filtered = s.filtered_recipients();
        assert_eq!(filtered.len(), 2); // GoldHawk, GreenLake
    }

    #[test]
    fn filter_is_case_insensitive() {
        let mut s = state_with_agents(&["GoldHawk", "SilverFox"]);
        s.recipient_filter = "GOLD".into();
        let filtered = s.filtered_recipients();
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn empty_filter_shows_all() {
        let s = state_with_agents(&["A", "B", "C"]);
        assert_eq!(s.filtered_recipients().len(), 3);
    }

    // ── Toggle/select/clear ────────────────────────────────────

    #[test]
    fn toggle_recipient_selects_and_deselects() {
        let mut s = state_with_agents(&["GoldHawk", "SilverFox"]);
        s.recipient_cursor = 0;
        s.toggle_recipient();
        assert!(s.recipients[0].selected);
        assert!(s.dirty);

        s.toggle_recipient();
        assert!(!s.recipients[0].selected);
    }

    #[test]
    fn select_all_and_clear_all() {
        let mut s = state_with_agents(&["A", "B", "C"]);
        s.select_all_recipients();
        assert!(s.recipients.iter().all(|r| r.selected));

        s.clear_all_recipients();
        assert!(s.recipients.iter().all(|r| !r.selected));
    }

    // ── Recipient extraction ───────────────────────────────────

    #[test]
    fn to_cc_bcc_extraction() {
        let mut s = state_with_agents(&["A", "B", "C", "D"]);
        s.recipients[0].selected = true;
        s.recipients[0].kind = RecipientKind::To;
        s.recipients[1].selected = true;
        s.recipients[1].kind = RecipientKind::Cc;
        s.recipients[2].selected = true;
        s.recipients[2].kind = RecipientKind::Bcc;
        // D not selected

        assert_eq!(s.to_recipients(), vec!["A"]);
        assert_eq!(s.cc_recipients(), vec!["B"]);
        assert_eq!(s.bcc_recipients(), vec!["C"]);
    }

    // ── Validation ─────────────────────────────────────────────

    #[test]
    fn validate_requires_recipient() {
        let mut s = ComposeState::new();
        s.subject = "Hello".into();
        s.body = "World".into();
        assert!(s.validate().is_err());
        assert!(s.error.as_ref().unwrap().contains("recipient"));
    }

    #[test]
    fn validate_requires_subject() {
        let mut s = state_with_agents(&["A"]);
        s.recipients[0].selected = true;
        s.body = "World".into();
        assert!(s.validate().is_err());
        assert!(s.error.as_ref().unwrap().contains("Subject"));
    }

    #[test]
    fn validate_requires_body() {
        let mut s = state_with_agents(&["A"]);
        s.recipients[0].selected = true;
        s.subject = "Hello".into();
        assert!(s.validate().is_err());
        assert!(s.error.as_ref().unwrap().contains("Body"));
    }

    #[test]
    fn validate_subject_length() {
        let mut s = state_with_agents(&["A"]);
        s.recipients[0].selected = true;
        s.subject = "x".repeat(MAX_SUBJECT_LEN + 1);
        s.body = "body".into();
        assert!(s.validate().is_err());
        assert!(s.error.as_ref().unwrap().contains("too long"));
    }

    #[test]
    fn validate_success() {
        let mut s = state_with_agents(&["A"]);
        s.recipients[0].selected = true;
        s.subject = "Hello".into();
        s.body = "World".into();
        assert!(s.validate().is_ok());
        assert!(s.error.is_none());
    }

    // ── Key handling: global ───────────────────────────────────

    #[test]
    fn ctrl_enter_produces_send() {
        let mut s = ComposeState::new();
        let action = s.handle_key(&make_key_ctrl(KeyCode::Enter));
        assert_eq!(action, ComposeAction::Send);
    }

    #[test]
    fn esc_on_clean_state_produces_close() {
        let mut s = ComposeState::new();
        let action = s.handle_key(&make_key(KeyCode::Escape));
        assert_eq!(action, ComposeAction::Close);
    }

    #[test]
    fn esc_on_dirty_state_produces_confirm_close() {
        let mut s = ComposeState::new();
        s.subject = "something".into();
        let action = s.handle_key(&make_key(KeyCode::Escape));
        assert_eq!(action, ComposeAction::ConfirmClose);
    }

    #[test]
    fn tab_advances_field() {
        let mut s = ComposeState::new();
        assert_eq!(s.active_field, ComposeField::Recipients);
        s.handle_key(&make_key(KeyCode::Tab));
        assert_eq!(s.active_field, ComposeField::Subject);
        s.handle_key(&make_key(KeyCode::Tab));
        assert_eq!(s.active_field, ComposeField::Body);
    }

    #[test]
    fn backtab_goes_to_previous_field() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.handle_key(&make_key(KeyCode::BackTab));
        assert_eq!(s.active_field, ComposeField::Subject);
    }

    // ── Key handling: subject ──────────────────────────────────

    #[test]
    fn subject_typing() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Subject;
        s.handle_key(&make_key(KeyCode::Char('H')));
        s.handle_key(&make_key(KeyCode::Char('i')));
        assert_eq!(s.subject, "Hi");
        assert_eq!(s.subject_cursor, 2);
        assert!(s.dirty);
    }

    #[test]
    fn subject_backspace() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Subject;
        s.subject = "Hello".into();
        s.subject_cursor = 5;
        s.handle_key(&make_key(KeyCode::Backspace));
        assert_eq!(s.subject, "Hell");
        assert_eq!(s.subject_cursor, 4);
    }

    #[test]
    fn subject_cursor_navigation() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Subject;
        s.subject = "Hello".into();
        s.subject_cursor = 3;

        s.handle_key(&make_key(KeyCode::Home));
        assert_eq!(s.subject_cursor, 0);

        s.handle_key(&make_key(KeyCode::End));
        assert_eq!(s.subject_cursor, 5);

        s.handle_key(&make_key(KeyCode::Left));
        assert_eq!(s.subject_cursor, 4);

        s.handle_key(&make_key(KeyCode::Right));
        assert_eq!(s.subject_cursor, 5);
    }

    #[test]
    fn subject_respects_max_length() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Subject;
        s.subject = "x".repeat(MAX_SUBJECT_LEN);
        s.subject_cursor = MAX_SUBJECT_LEN;
        s.handle_key(&make_key(KeyCode::Char('a')));
        assert_eq!(s.subject.len(), MAX_SUBJECT_LEN);
    }

    // ── Key handling: body ─────────────────────────────────────

    #[test]
    fn body_typing_and_newline() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.handle_key(&make_key(KeyCode::Char('a')));
        s.handle_key(&make_key(KeyCode::Char('b')));
        s.handle_key(&make_key(KeyCode::Enter));
        s.handle_key(&make_key(KeyCode::Char('c')));
        assert_eq!(s.body, "ab\nc");
        assert_eq!(s.body_cursor_line, 1);
        assert_eq!(s.body_cursor_col, 1);
    }

    #[test]
    fn body_cursor_up_down() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "line1\nline2\nline3".into();
        s.body_cursor_line = 1;
        s.body_cursor_col = 3;

        s.handle_key(&make_key(KeyCode::Up));
        assert_eq!(s.body_cursor_line, 0);
        assert_eq!(s.body_cursor_col, 3);

        s.handle_key(&make_key(KeyCode::Down));
        assert_eq!(s.body_cursor_line, 1);
    }

    #[test]
    fn body_cursor_clamps_to_shorter_line() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "long line\nhi".into();
        s.body_cursor_line = 0;
        s.body_cursor_col = 8; // past end of "hi"

        s.handle_key(&make_key(KeyCode::Down));
        assert_eq!(s.body_cursor_line, 1);
        assert_eq!(s.body_cursor_col, 2); // clamped to len of "hi"
    }

    // ── Key handling: importance ────────────────────────────────

    #[test]
    fn importance_cycles_on_space() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Importance;
        assert_eq!(s.importance, Importance::Normal);

        s.handle_key(&make_key(KeyCode::Char(' ')));
        assert_eq!(s.importance, Importance::High);

        s.handle_key(&make_key(KeyCode::Char(' ')));
        assert_eq!(s.importance, Importance::Urgent);
    }

    // ── Key handling: recipients ────────────────────────────────

    #[test]
    fn recipient_navigation_and_toggle() {
        let mut s = state_with_agents(&["A", "B", "C"]);
        s.active_field = ComposeField::Recipients;

        s.handle_key(&make_key(KeyCode::Down));
        assert_eq!(s.recipient_cursor, 1);

        s.handle_key(&make_key(KeyCode::Char(' ')));
        assert!(s.recipients[1].selected);

        s.handle_key(&make_key(KeyCode::Up));
        assert_eq!(s.recipient_cursor, 0);
    }

    #[test]
    fn recipient_filter_typing() {
        let mut s = state_with_agents(&["GoldHawk", "SilverFox"]);
        s.active_field = ComposeField::Recipients;
        s.handle_key(&make_key(KeyCode::Char('g')));
        assert_eq!(s.recipient_filter, "g");
        assert_eq!(s.filtered_recipients().len(), 1);

        s.handle_key(&make_key(KeyCode::Backspace));
        assert_eq!(s.recipient_filter, "");
        assert_eq!(s.filtered_recipients().len(), 2);
    }

    #[test]
    fn recipient_ctrl_t_cycles_kind() {
        let mut s = state_with_agents(&["A"]);
        s.active_field = ComposeField::Recipients;
        s.recipients[0].selected = true;
        s.handle_key(&make_key_ctrl(KeyCode::Char('t')));
        assert_eq!(s.recipients[0].kind, RecipientKind::Cc);
    }

    // ── Body offset calculation ────────────────────────────────

    #[test]
    fn body_offset_basic() {
        let mut s = ComposeState::new();
        s.body = "abc\ndef".into();
        s.body_cursor_line = 0;
        s.body_cursor_col = 2;
        assert_eq!(s.body_offset(), 2);

        s.body_cursor_line = 1;
        s.body_cursor_col = 1;
        assert_eq!(s.body_offset(), 5); // "abc\n" = 4, then 'd' at 4, 'e' cursor at 5
    }

    // ── Rendering ──────────────────────────────────────────────

    #[test]
    fn overlay_area_centered() {
        let terminal = Rect::new(0, 0, 100, 50);
        let area = ComposePanel::overlay_area(terminal);
        assert_eq!(area.width, 70);
        assert_eq!(area.height, 40);
        assert_eq!(area.x, 15);
        assert_eq!(area.y, 5);
    }

    #[test]
    fn overlay_area_minimum_size() {
        let terminal = Rect::new(0, 0, 30, 10);
        let area = ComposePanel::overlay_area(terminal);
        assert!(area.width >= 21);
        assert!(area.height >= 8);
    }

    #[test]
    fn render_does_not_panic() {
        let state = ComposeState::new();
        let panel = ComposePanel::new(&state);
        let config = mcp_agent_mail_core::Config::default();
        let shared = crate::tui_bridge::TuiSharedState::new(&config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let terminal = Rect::new(0, 0, 80, 24);
        panel.render(terminal, &mut frame, &shared);
    }

    #[test]
    fn render_with_content_does_not_panic() {
        let mut state = state_with_agents(&["GoldHawk", "SilverFox", "RedLake"]);
        state.recipients[0].selected = true;
        state.subject = "Test subject".into();
        state.body = "Line 1\nLine 2\nLine 3".into();
        state.importance = Importance::High;
        state.thread_id = "br-123".into();

        let panel = ComposePanel::new(&state);
        let config = mcp_agent_mail_core::Config::default();
        let shared = crate::tui_bridge::TuiSharedState::new(&config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let terminal = Rect::new(0, 0, 120, 40);
        panel.render(terminal, &mut frame, &shared);
    }

    #[test]
    fn render_at_minimum_terminal_does_not_panic() {
        let state = ComposeState::new();
        let panel = ComposePanel::new(&state);
        let config = mcp_agent_mail_core::Config::default();
        let shared = crate::tui_bridge::TuiSharedState::new(&config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 15, &mut pool);
        let terminal = Rect::new(0, 0, 40, 15);
        panel.render(terminal, &mut frame, &shared);
    }

    // ── has_unsaved_changes ────────────────────────────────────

    #[test]
    fn unsaved_changes_detected() {
        let mut s = ComposeState::new();
        assert!(!s.has_unsaved_changes());

        s.subject = "x".into();
        assert!(s.has_unsaved_changes());

        s.subject.clear();
        s.body = "y".into();
        assert!(s.has_unsaved_changes());
    }

    // ── ComposeEnvelope tests ─────────────────────────────────────────

    #[test]
    fn envelope_from_valid_state() {
        let mut s = state_with_agents(&["RedLake", "BluePeak"]);
        s.recipients[0].selected = true;
        s.subject = "Test subject".into();
        s.body = "Hello world".into();
        s.importance = Importance::High;
        s.thread_id = "br-123".into();

        let env = s.build_envelope().unwrap();
        assert_eq!(env.sender_name, OVERSEER_AGENT_NAME);
        assert_eq!(env.to, vec!["RedLake"]);
        assert!(env.cc.is_empty());
        assert!(env.bcc.is_empty());
        assert_eq!(env.subject, "Test subject");
        assert_eq!(env.body_md, "Hello world");
        assert_eq!(env.importance, "high");
        assert_eq!(env.thread_id, Some("br-123".to_owned()));
    }

    #[test]
    fn envelope_fails_without_recipient() {
        let mut s = state_with_agents(&["RedLake"]);
        s.subject = "Test".into();
        s.body = "Body".into();
        assert!(s.build_envelope().is_err());
    }

    #[test]
    fn envelope_cc_and_bcc_routing() {
        let mut s = state_with_agents(&["RedLake", "BluePeak", "GoldFox"]);
        s.recipients[0].selected = true;
        s.recipients[0].kind = RecipientKind::To;
        s.recipients[1].selected = true;
        s.recipients[1].kind = RecipientKind::Cc;
        s.recipients[2].selected = true;
        s.recipients[2].kind = RecipientKind::Bcc;
        s.subject = "Routed".into();
        s.body = "Body".into();

        let env = s.build_envelope().unwrap();
        assert_eq!(env.to, vec!["RedLake"]);
        assert_eq!(env.cc, vec!["BluePeak"]);
        assert_eq!(env.bcc, vec!["GoldFox"]);
    }

    #[test]
    fn envelope_empty_thread_id_becomes_none() {
        let mut s = state_with_agents(&["RedLake"]);
        s.recipients[0].selected = true;
        s.subject = "Test".into();
        s.body = "Body".into();
        s.thread_id = "   ".into();

        let env = s.build_envelope().unwrap();
        assert!(env.thread_id.is_none());
    }

    #[test]
    fn envelope_importance_defaults_to_normal() {
        let mut s = state_with_agents(&["RedLake"]);
        s.recipients[0].selected = true;
        s.subject = "Test".into();
        s.body = "Body".into();

        let env = s.build_envelope().unwrap();
        assert_eq!(env.importance, "normal");
    }

    // ── Edge-case: body backspace joins lines ─────────────────

    #[test]
    fn body_backspace_at_line_start_joins_lines() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "hello\nworld".into();
        s.body_cursor_line = 1;
        s.body_cursor_col = 0;

        s.handle_key(&make_key(KeyCode::Backspace));
        assert_eq!(s.body, "helloworld");
        assert_eq!(s.body_cursor_line, 0);
        // Cursor lands at the join point (end of previous line before join).
        assert_eq!(s.body_cursor_col, 5);
    }

    #[test]
    fn body_backspace_at_start_does_nothing() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "hello".into();
        s.body_cursor_line = 0;
        s.body_cursor_col = 0;

        s.handle_key(&make_key(KeyCode::Backspace));
        assert_eq!(s.body, "hello");
    }

    // ── Edge-case: body cursor wrapping at line ends ──────────

    #[test]
    fn body_left_at_line_start_wraps_to_prev_line_end() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "abc\nde".into();
        s.body_cursor_line = 1;
        s.body_cursor_col = 0;

        s.handle_key(&make_key(KeyCode::Left));
        assert_eq!(s.body_cursor_line, 0);
        assert_eq!(s.body_cursor_col, 3); // end of "abc"
    }

    #[test]
    fn body_right_at_line_end_wraps_to_next_line() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "abc\nde".into();
        s.body_cursor_line = 0;
        s.body_cursor_col = 3;

        s.handle_key(&make_key(KeyCode::Right));
        assert_eq!(s.body_cursor_line, 1);
        assert_eq!(s.body_cursor_col, 0);
    }

    #[test]
    fn body_right_at_last_line_end_stays() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "abc".into();
        s.body_cursor_line = 0;
        s.body_cursor_col = 3;

        s.handle_key(&make_key(KeyCode::Right));
        // No next line, stay put
        assert_eq!(s.body_cursor_line, 0);
        assert_eq!(s.body_cursor_col, 3);
    }

    // ── Edge-case: subject delete key ─────────────────────────

    #[test]
    fn subject_delete_removes_char_at_cursor() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Subject;
        s.subject = "Hello".into();
        s.subject_cursor = 2;
        s.handle_key(&make_key(KeyCode::Delete));
        assert_eq!(s.subject, "Helo");
        assert_eq!(s.subject_cursor, 2);
    }

    #[test]
    fn subject_delete_at_end_does_nothing() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Subject;
        s.subject = "Hi".into();
        s.subject_cursor = 2;
        s.handle_key(&make_key(KeyCode::Delete));
        assert_eq!(s.subject, "Hi");
    }

    // ── Edge-case: thread ID field ────────────────────────────

    #[test]
    fn thread_id_typing() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::ThreadId;
        s.handle_key(&make_key(KeyCode::Char('b')));
        s.handle_key(&make_key(KeyCode::Char('r')));
        s.handle_key(&make_key(KeyCode::Char('-')));
        s.handle_key(&make_key(KeyCode::Char('1')));
        assert_eq!(s.thread_id, "br-1");
        assert_eq!(s.thread_id_cursor, 4);
    }

    #[test]
    fn thread_id_cursor_navigation() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::ThreadId;
        s.thread_id = "br-123".into();
        s.thread_id_cursor = 3;

        s.handle_key(&make_key(KeyCode::Home));
        assert_eq!(s.thread_id_cursor, 0);

        s.handle_key(&make_key(KeyCode::End));
        assert_eq!(s.thread_id_cursor, 6);
    }

    // ── Edge-case: importance from Enter/Right ────────────────

    #[test]
    fn importance_cycles_on_enter() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Importance;
        assert_eq!(s.importance, Importance::Normal);

        s.handle_key(&make_key(KeyCode::Enter));
        assert_eq!(s.importance, Importance::High);
    }

    #[test]
    fn importance_cycles_on_right() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Importance;
        s.handle_key(&make_key(KeyCode::Right));
        assert_eq!(s.importance, Importance::High);
    }

    // ── Edge-case: body at max length ─────────────────────────

    #[test]
    fn body_rejects_input_at_max_length() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "x".repeat(MAX_BODY_LEN);
        s.body_cursor_col = MAX_BODY_LEN;
        s.handle_key(&make_key(KeyCode::Char('a')));
        assert_eq!(s.body.len(), MAX_BODY_LEN);
    }

    #[test]
    fn body_rejects_newline_at_max_length() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.body = "x".repeat(MAX_BODY_LEN);
        s.body_cursor_col = MAX_BODY_LEN;
        s.handle_key(&make_key(KeyCode::Enter));
        assert_eq!(s.body.len(), MAX_BODY_LEN);
    }

    // ── Edge-case: overlay area extremes ──────────────────────

    #[test]
    fn overlay_area_large_terminal() {
        let terminal = Rect::new(0, 0, 300, 100);
        let area = ComposePanel::overlay_area(terminal);
        assert_eq!(area.width, 210);
        assert_eq!(area.height, 80);
        // Centered
        assert_eq!(area.x, 45);
        assert_eq!(area.y, 10);
    }

    #[test]
    fn overlay_area_very_small_terminal() {
        let terminal = Rect::new(0, 0, 20, 5);
        let area = ComposePanel::overlay_area(terminal);
        // Min dimensions should be clamped to terminal size
        assert!(area.width <= terminal.width);
        assert!(area.height <= terminal.height);
    }

    // ── Edge-case: recipient cursor bounds ────────────────────

    #[test]
    fn recipient_cursor_stays_at_zero_on_up() {
        let mut s = state_with_agents(&["A", "B"]);
        s.active_field = ComposeField::Recipients;
        s.recipient_cursor = 0;
        s.handle_key(&make_key(KeyCode::Up));
        assert_eq!(s.recipient_cursor, 0);
    }

    #[test]
    fn recipient_cursor_stays_at_end_on_down() {
        let mut s = state_with_agents(&["A", "B"]);
        s.active_field = ComposeField::Recipients;
        s.recipient_cursor = 1;
        s.handle_key(&make_key(KeyCode::Down));
        assert_eq!(s.recipient_cursor, 1);
    }

    // ── Edge-case: multiple recipients same kind ──────────────

    #[test]
    fn envelope_multiple_to_recipients() {
        let mut s = state_with_agents(&["RedLake", "BluePeak", "GoldFox"]);
        s.recipients[0].selected = true;
        s.recipients[1].selected = true;
        s.recipients[2].selected = true;
        // All default to To
        s.subject = "Group msg".into();
        s.body = "Hello all".into();

        let env = s.build_envelope().unwrap();
        assert_eq!(env.to, vec!["RedLake", "BluePeak", "GoldFox"]);
        assert!(env.cc.is_empty());
        assert!(env.bcc.is_empty());
    }

    // ── Edge-case: ctrl+a select all, ctrl+d clear all via keys ──

    #[test]
    fn ctrl_a_selects_all_recipients() {
        let mut s = state_with_agents(&["A", "B", "C"]);
        s.active_field = ComposeField::Recipients;
        s.handle_key(&make_key_ctrl(KeyCode::Char('a')));
        assert!(s.recipients.iter().all(|r| r.selected));
    }

    #[test]
    fn ctrl_d_clears_all_recipients() {
        let mut s = state_with_agents(&["A", "B"]);
        s.active_field = ComposeField::Recipients;
        s.recipients[0].selected = true;
        s.recipients[1].selected = true;
        s.handle_key(&make_key_ctrl(KeyCode::Char('d')));
        assert!(s.recipients.iter().all(|r| !r.selected));
    }

    // ── Edge-case: default impl ───────────────────────────────

    #[test]
    fn compose_state_default_matches_new() {
        let d = ComposeState::default();
        let n = ComposeState::new();
        assert_eq!(d.active_field, n.active_field);
        assert_eq!(d.subject, n.subject);
        assert_eq!(d.body, n.body);
        assert_eq!(d.importance, n.importance);
    }

    // ── Edge-case: body_offset past all lines ─────────────────

    #[test]
    fn body_offset_past_end_returns_body_len() {
        let mut s = ComposeState::new();
        s.body = "ab\ncd".into();
        s.body_cursor_line = 5; // way past end
        s.body_cursor_col = 0;
        assert_eq!(s.body_offset(), s.body.len());
    }

    // ── Edge-case: empty body operations ──────────────────────

    #[test]
    fn body_up_on_empty_body_does_nothing() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.handle_key(&make_key(KeyCode::Up));
        assert_eq!(s.body_cursor_line, 0);
        assert_eq!(s.body_cursor_col, 0);
    }

    #[test]
    fn body_down_on_empty_body_does_nothing() {
        let mut s = ComposeState::new();
        s.active_field = ComposeField::Body;
        s.handle_key(&make_key(KeyCode::Down));
        assert_eq!(s.body_cursor_line, 0);
    }

    // ── Edge-case: render with error state ────────────────────

    #[test]
    fn render_with_error_does_not_panic() {
        let mut state = ComposeState::new();
        state.error = Some("Something went wrong".into());
        let panel = ComposePanel::new(&state);
        let config = mcp_agent_mail_core::Config::default();
        let shared = crate::tui_bridge::TuiSharedState::new(&config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let terminal = Rect::new(0, 0, 80, 24);
        panel.render(terminal, &mut frame, &shared);
    }

    // ── Edge-case: render with sending state ──────────────────

    #[test]
    fn render_while_sending_does_not_panic() {
        let mut state = state_with_agents(&["RedLake"]);
        state.recipients[0].selected = true;
        state.subject = "Test".into();
        state.body = "Body".into();
        state.sending = true;
        let panel = ComposePanel::new(&state);
        let config = mcp_agent_mail_core::Config::default();
        let shared = crate::tui_bridge::TuiSharedState::new(&config);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let terminal = Rect::new(0, 0, 80, 24);
        panel.render(terminal, &mut frame, &shared);
    }
}
