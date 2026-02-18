//! `ActionMenu` — contextual per-item action overlay for TUI screens.
//!
//! Each screen can provide a set of actions relevant to the currently focused item.
//! The action menu appears as a floating overlay near the selected row, triggered
//! by pressing `.` (period).

use ftui::layout::Rect;
use ftui::text::display_width;
use ftui::{Cell, Event, Frame, KeyCode, KeyEventKind};

use crate::tui_screens::{DeepLinkTarget, MailScreenId};

// ──────────────────────────────────────────────────────────────────────
// ActionEntry — a single action in the menu
// ──────────────────────────────────────────────────────────────────────

/// A single action entry in the action menu.
#[derive(Clone)]
pub struct ActionEntry {
    /// Display label for the action (e.g., "View body", "Acknowledge").
    pub label: String,
    /// Optional description shown next to label.
    pub description: Option<String>,
    /// Optional keybinding shortcut (e.g., "a" for Acknowledge).
    pub keybinding: Option<String>,
    /// The action to perform when selected.
    pub action: ActionKind,
    /// Whether this action is destructive (shows red, triggers modal).
    pub is_destructive: bool,
    /// Whether this action is currently available. Disabled actions are
    /// rendered dimmed and cannot be invoked.
    pub enabled: bool,
    /// Human-readable reason why this action is disabled (shown in tooltip/toast).
    pub disabled_reason: Option<String>,
}

impl ActionEntry {
    /// Create a new action entry.
    #[must_use]
    pub fn new(label: impl Into<String>, action: ActionKind) -> Self {
        Self {
            label: label.into(),
            description: None,
            keybinding: None,
            action,
            is_destructive: false,
            enabled: true,
            disabled_reason: None,
        }
    }

    /// Add a description.
    #[must_use]
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Add a keybinding hint.
    #[must_use]
    pub fn with_keybinding(mut self, key: impl Into<String>) -> Self {
        self.keybinding = Some(key.into());
        self
    }

    /// Mark as destructive.
    #[must_use]
    pub const fn destructive(mut self) -> Self {
        self.is_destructive = true;
        self
    }

    /// Mark as disabled with an explanation.
    #[must_use]
    pub fn disabled(mut self, reason: impl Into<String>) -> Self {
        self.enabled = false;
        self.disabled_reason = Some(reason.into());
        self
    }

    /// First character for quick-jump navigation.
    fn first_char(&self) -> Option<char> {
        self.label.chars().next().map(|c| c.to_ascii_lowercase())
    }
}

// ──────────────────────────────────────────────────────────────────────
// ActionKind — what happens when an action is executed
// ──────────────────────────────────────────────────────────────────────

/// The kind of action to perform.
#[derive(Clone, Debug)]
pub enum ActionKind {
    /// Navigate to a screen.
    Navigate(MailScreenId),
    /// Navigate with a deep-link target.
    DeepLink(DeepLinkTarget),
    /// Execute a named operation (handled by the screen or app).
    Execute(String),
    /// Show a confirmation modal before executing.
    ConfirmThenExecute {
        title: String,
        message: String,
        operation: String,
    },
    /// Copy text to clipboard (if supported).
    CopyToClipboard(String),
    /// Close the menu without action.
    Dismiss,
}

// ──────────────────────────────────────────────────────────────────────
// ActionMenuState — tracks menu visibility and selection
// ──────────────────────────────────────────────────────────────────────

/// State for the action menu overlay.
pub struct ActionMenuState {
    /// The entries in the menu.
    entries: Vec<ActionEntry>,
    /// Currently selected entry index.
    selected: usize,
    /// Anchor position (row where the menu appears).
    anchor_row: u16,
    /// Context ID (e.g., message ID, agent name) for the focused item.
    context_id: String,
}

impl ActionMenuState {
    /// Create a new action menu state.
    #[must_use]
    pub fn new(entries: Vec<ActionEntry>, anchor_row: u16, context_id: impl Into<String>) -> Self {
        Self {
            entries,
            selected: 0,
            anchor_row,
            context_id: context_id.into(),
        }
    }

    /// Returns the selected entry, if any.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ActionEntry> {
        self.entries.get(self.selected)
    }

    /// Returns the context ID for the focused item.
    #[must_use]
    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Move selection up (wraps from first to last).
    pub const fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.entries.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    /// Move selection down (wraps from last to first).
    pub const fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.entries.len();
    }

    /// Jump to the first entry starting with the given character.
    pub fn jump_to_char(&mut self, c: char) -> bool {
        let lower = c.to_ascii_lowercase();
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.first_char() == Some(lower) {
                self.selected = i;
                return true;
            }
        }
        false
    }
}

// ──────────────────────────────────────────────────────────────────────
// ActionMenu — the overlay widget
// ──────────────────────────────────────────────────────────────────────

/// The action menu overlay widget.
pub struct ActionMenu<'a> {
    state: &'a ActionMenuState,
}

impl<'a> ActionMenu<'a> {
    /// Create a new action menu widget.
    #[must_use]
    pub const fn new(state: &'a ActionMenuState) -> Self {
        Self { state }
    }

    /// Render the action menu as a floating overlay.
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub fn render(&self, terminal_area: Rect, frame: &mut Frame) {
        if self.state.is_empty() {
            return;
        }
        let tp = crate::tui_theme::TuiThemePalette::current();
        let action_menu_bg = tp.panel_bg;
        let action_menu_border = tp.panel_border;
        let row_highlight_bg = tp.selection_bg;
        let row_focus_fg = tp.selection_fg;
        let action_menu_destructive = tp.severity_critical;
        let action_menu_disabled_fg = tp.text_disabled;
        let action_menu_disabled_reason = tp.text_muted;
        let action_menu_normal_fg = tp.text_primary;

        // Calculate menu dimensions
        let max_label_len = self
            .state
            .entries
            .iter()
            .map(|e| {
                let mut w = display_width(&e.label)
                    + e.keybinding.as_ref().map_or(0, |k| display_width(k) + 3);
                if !e.enabled {
                    w += e
                        .disabled_reason
                        .as_ref()
                        .map_or(0, |r| display_width(r) + 2);
                }
                w
            })
            .max()
            .unwrap_or(10);
        let width = (max_label_len + 4).clamp(20, 50) as u16;
        let height = (self.state.len() + 2).min(12) as u16;

        // Position near anchor row, biased to the right side
        let x = terminal_area.width.saturating_sub(width + 4);
        let y = self
            .state
            .anchor_row
            .min(terminal_area.height.saturating_sub(height + 1));
        let area = Rect::new(x, y, width, height);

        // Clear the area by filling with background color
        for row in area.y..area.bottom() {
            for col in area.x..area.right() {
                let mut cell = Cell::from_char(' ');
                cell.bg = action_menu_bg;
                frame.buffer.set_fast(col, row, cell);
            }
        }

        // Build text lines manually
        let inner = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );

        for (i, entry) in self.state.entries.iter().enumerate() {
            if i >= inner.height as usize {
                break;
            }
            let row = inner.y + i as u16;
            let is_selected = i == self.state.selected;

            // Build the line text
            let mut text = entry.label.clone();
            if let Some(ref kb) = entry.keybinding {
                text.push_str("  [");
                text.push_str(kb);
                text.push(']');
            }
            // Append disabled reason as dimmed suffix.
            let label_char_count = text.chars().count();
            if !entry.enabled
                && let Some(ref reason) = entry.disabled_reason
            {
                text.push_str("  ");
                text.push_str(reason);
            }

            // Render each character, advancing by display width for Unicode safety.
            let mut col = inner.x;
            for (j, ch) in text.chars().enumerate() {
                if col >= inner.right() {
                    break;
                }
                let mut utf8 = [0_u8; 4];
                let ch_str = ch.encode_utf8(&mut utf8);
                let advance = u16::try_from(display_width(ch_str)).unwrap_or(1).max(1);
                if col.saturating_add(advance) > inner.right() {
                    break;
                }
                let mut cell = Cell::from_char(ch);
                if !entry.enabled {
                    // Disabled: dimmed text; reason part even dimmer.
                    cell.fg = if j >= label_char_count {
                        action_menu_disabled_reason
                    } else {
                        action_menu_disabled_fg
                    };
                } else if entry.is_destructive {
                    cell.fg = action_menu_destructive;
                } else if is_selected {
                    cell.fg = row_focus_fg;
                } else {
                    cell.fg = action_menu_normal_fg;
                }
                if is_selected {
                    cell.bg = row_highlight_bg;
                } else {
                    cell.bg = action_menu_bg;
                }
                frame.buffer.set_fast(col, row, cell);
                // Keep trailing cells of wide glyphs styled consistently.
                for fill_col in col.saturating_add(1)..col.saturating_add(advance) {
                    if fill_col >= inner.right() {
                        break;
                    }
                    let mut filler = Cell::from_char(' ');
                    if is_selected {
                        filler.bg = row_highlight_bg;
                    } else {
                        filler.bg = action_menu_bg;
                    }
                    frame.buffer.set_fast(fill_col, row, filler);
                }
                col = col.saturating_add(advance);
            }

            // Fill rest of line with background
            for col in col..inner.right() {
                let mut cell = Cell::from_char(' ');
                if is_selected {
                    cell.bg = row_highlight_bg;
                } else {
                    cell.bg = action_menu_bg;
                }
                frame.buffer.set_fast(col, row, cell);
            }
        }

        // Draw border - helper to create a border cell
        let border_cell = |ch: char| -> Cell {
            let mut cell = Cell::from_char(ch);
            cell.fg = action_menu_border;
            cell.bg = action_menu_bg;
            cell
        };

        // Top border
        frame.buffer.set_fast(area.x, area.y, border_cell('┌'));
        for col in (area.x + 1)..area.right().saturating_sub(1) {
            frame.buffer.set_fast(col, area.y, border_cell('─'));
        }
        frame
            .buffer
            .set_fast(area.right().saturating_sub(1), area.y, border_cell('┐'));

        // Side borders
        for row in (area.y + 1)..area.bottom().saturating_sub(1) {
            frame.buffer.set_fast(area.x, row, border_cell('│'));
            frame
                .buffer
                .set_fast(area.right().saturating_sub(1), row, border_cell('│'));
        }

        // Bottom border
        frame
            .buffer
            .set_fast(area.x, area.bottom().saturating_sub(1), border_cell('└'));
        for col in (area.x + 1)..area.right().saturating_sub(1) {
            frame
                .buffer
                .set_fast(col, area.bottom().saturating_sub(1), border_cell('─'));
        }
        frame.buffer.set_fast(
            area.right().saturating_sub(1),
            area.bottom().saturating_sub(1),
            border_cell('┘'),
        );

        // Title
        let title = " Actions ";
        let title_x = area.x + 2;
        for (i, ch) in title.chars().enumerate() {
            let col = title_x + i as u16;
            if col < area.right().saturating_sub(1) {
                let mut cell = Cell::from_char(ch);
                cell.fg = tp.panel_title_fg;
                cell.bg = action_menu_bg;
                frame.buffer.set_fast(col, area.y, cell);
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// ActionMenuManager — manages action menu lifecycle
// ──────────────────────────────────────────────────────────────────────

/// Result of handling an event in the action menu.
#[derive(Debug, Clone)]
pub enum ActionMenuResult {
    /// Event was consumed, menu stays open.
    Consumed,
    /// Menu was dismissed without action.
    Dismissed,
    /// An action was selected.
    Selected(ActionKind, String),
    /// A disabled action was attempted. Contains the reason string.
    DisabledAttempt(String),
}

/// Manages the action menu lifecycle.
pub struct ActionMenuManager {
    /// The active menu state, if any.
    active: Option<ActionMenuState>,
}

impl Default for ActionMenuManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionMenuManager {
    /// Create a new action menu manager.
    #[must_use]
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// Returns `true` if a menu is currently active.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Open the action menu with the given entries.
    pub fn open(
        &mut self,
        entries: Vec<ActionEntry>,
        anchor_row: u16,
        context_id: impl Into<String>,
    ) {
        if !entries.is_empty() {
            self.active = Some(ActionMenuState::new(entries, anchor_row, context_id));
        }
    }

    /// Close the action menu.
    pub fn close(&mut self) {
        self.active = None;
    }

    /// Handle an event, returning the result.
    ///
    /// When the menu is active, all events are routed to it (focus trapping).
    pub fn handle_event(&mut self, event: &Event) -> Option<ActionMenuResult> {
        let state = self.active.as_mut()?;

        let Event::Key(key) = event else {
            return Some(ActionMenuResult::Consumed);
        };

        if key.kind != KeyEventKind::Press {
            return Some(ActionMenuResult::Consumed);
        }

        match key.code {
            KeyCode::Escape => {
                self.active = None;
                Some(ActionMenuResult::Dismissed)
            }
            KeyCode::Enter => {
                if let Some(entry) = state.selected_entry() {
                    if entry.enabled {
                        let action = entry.action.clone();
                        let context = state.context_id().to_string();
                        self.active = None;
                        Some(ActionMenuResult::Selected(action, context))
                    } else {
                        // Disabled: report reason via DisabledAttempt result.
                        let reason = entry
                            .disabled_reason
                            .clone()
                            .unwrap_or_else(|| "Action unavailable".into());
                        Some(ActionMenuResult::DisabledAttempt(reason))
                    }
                } else {
                    Some(ActionMenuResult::Consumed)
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.move_up();
                Some(ActionMenuResult::Consumed)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.move_down();
                Some(ActionMenuResult::Consumed)
            }
            KeyCode::Char(c) if c.is_alphabetic() => {
                state.jump_to_char(c);
                Some(ActionMenuResult::Consumed)
            }
            _ => Some(ActionMenuResult::Consumed),
        }
    }

    /// Render the action menu if active.
    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if let Some(ref state) = self.active {
            ActionMenu::new(state).render(area, frame);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Per-screen action builders
// ──────────────────────────────────────────────────────────────────────

/// Build actions for the Messages screen.
#[must_use]
pub fn messages_actions(
    message_id: i64,
    thread_id: Option<&str>,
    sender: &str,
) -> Vec<ActionEntry> {
    let mut actions = vec![
        ActionEntry::new("View body", ActionKind::Execute("view_body".into()))
            .with_keybinding("v")
            .with_description("Show full message content"),
        ActionEntry::new(
            "Acknowledge",
            ActionKind::Execute(format!("acknowledge:{message_id}")),
        )
        .with_keybinding("a")
        .with_description("Mark as acknowledged"),
        ActionEntry::new(
            "Mark read",
            ActionKind::Execute(format!("mark_read:{message_id}")),
        )
        .with_keybinding("r")
        .with_description("Mark as read"),
    ];

    if let Some(tid) = thread_id {
        actions.push(
            ActionEntry::new(
                "Jump to thread",
                ActionKind::DeepLink(DeepLinkTarget::ThreadById(tid.to_string())),
            )
            .with_keybinding("t")
            .with_description("View thread conversation"),
        );
    }

    actions.push(
        ActionEntry::new(
            "Jump to sender",
            ActionKind::DeepLink(DeepLinkTarget::AgentByName(sender.to_string())),
        )
        .with_keybinding("s")
        .with_description("View sender profile"),
    );

    actions
}

/// Build batch actions for the Messages screen when multiple messages are selected.
#[must_use]
pub fn messages_batch_actions(selected_count: usize) -> Vec<ActionEntry> {
    let count = selected_count.max(1);
    vec![
        ActionEntry::new(
            format!("Acknowledge selected ({count})"),
            ActionKind::Execute("batch_acknowledge".into()),
        )
        .with_keybinding("a")
        .with_description("Acknowledge all selected messages"),
        ActionEntry::new(
            format!("Mark read selected ({count})"),
            ActionKind::Execute("batch_mark_read".into()),
        )
        .with_keybinding("r")
        .with_description("Mark all selected messages as read"),
        ActionEntry::new(
            format!("Mark unread selected ({count})"),
            ActionKind::Execute("batch_mark_unread".into()),
        )
        .with_keybinding("u")
        .with_description("Mark all selected messages as unread"),
    ]
}

/// Build batch actions for the Reservations screen when multiple rows are selected.
#[must_use]
pub fn reservations_batch_actions(
    selected_count: usize,
    reservation_ids: &[i64],
) -> Vec<ActionEntry> {
    let count = selected_count.max(1);
    let mut ids: Vec<i64> = reservation_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let ids_csv = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let missing = count.saturating_sub(ids.len());
    let missing_reason = if missing > 0 {
        Some(format!(
            "{missing} selected reservation(s) missing DB id; wait for snapshot refresh",
        ))
    } else {
        None
    };

    let renew = if missing_reason.is_none() && !ids_csv.is_empty() {
        ActionEntry::new(
            format!("Renew selected ({count})"),
            ActionKind::ConfirmThenExecute {
                title: "Renew Reservations".into(),
                message: format!("Renew TTL for {count} selected reservations?"),
                operation: format!("renew:{ids_csv}"),
            },
        )
        .with_keybinding("n")
        .with_description("Extend TTL for all selected reservations")
    } else {
        ActionEntry::new(
            format!("Renew selected ({count})"),
            ActionKind::Execute("renew:pending".to_string()),
        )
        .with_keybinding("n")
        .with_description("Extend TTL for all selected reservations")
        .disabled(
            missing_reason
                .as_deref()
                .unwrap_or("No selected reservation IDs available"),
        )
    };

    let release = if missing_reason.is_none() && !ids_csv.is_empty() {
        ActionEntry::new(
            format!("Release selected ({count})"),
            ActionKind::ConfirmThenExecute {
                title: "Release Reservations".into(),
                message: format!("Release {count} selected reservations?"),
                operation: format!("release:{ids_csv}"),
            },
        )
        .with_keybinding("r")
        .with_description("Release all selected reservations")
        .destructive()
    } else {
        ActionEntry::new(
            format!("Release selected ({count})"),
            ActionKind::Execute("release:pending".to_string()),
        )
        .with_keybinding("r")
        .with_description("Release all selected reservations")
        .destructive()
        .disabled(
            missing_reason
                .as_deref()
                .unwrap_or("No selected reservation IDs available"),
        )
    };

    vec![renew, release]
}

/// Build actions for the Reservations screen.
#[must_use]
pub fn reservations_actions(
    reservation_id: Option<i64>,
    agent_name: &str,
    path_pattern: &str,
) -> Vec<ActionEntry> {
    let id_missing_reason = "Reservation ID unavailable; wait for DB snapshot";
    let renew_action = reservation_id.map_or_else(
        || {
            ActionEntry::new("Renew", ActionKind::Execute("renew:pending".to_string()))
                .with_keybinding("r")
                .with_description("Extend reservation TTL")
                .disabled(id_missing_reason)
        },
        |id| {
            ActionEntry::new("Renew", ActionKind::Execute(format!("renew:{id}")))
                .with_keybinding("r")
                .with_description("Extend reservation TTL")
        },
    );
    let release_action = reservation_id.map_or_else(
        || {
            ActionEntry::new(
                "Release",
                ActionKind::Execute("release:pending".to_string()),
            )
            .with_keybinding("e")
            .with_description("Release this reservation")
            .disabled(id_missing_reason)
        },
        |id| {
            ActionEntry::new("Release", ActionKind::Execute(format!("release:{id}")))
                .with_keybinding("e")
                .with_description("Release this reservation")
        },
    );
    let force_release_action = reservation_id.map_or_else(
        || {
            ActionEntry::new(
                "Force-release",
                ActionKind::ConfirmThenExecute {
                    title: "Force Release".into(),
                    message: format!("Force-release reservation on {path_pattern}?"),
                    operation: "force_release:pending".into(),
                },
            )
            .with_keybinding("f")
            .with_description("Force-release (destructive)")
            .destructive()
            .disabled(id_missing_reason)
        },
        |id| {
            ActionEntry::new(
                "Force-release",
                ActionKind::ConfirmThenExecute {
                    title: "Force Release".into(),
                    message: format!("Force-release reservation on {path_pattern}?"),
                    operation: format!("force_release:{id}"),
                },
            )
            .with_keybinding("f")
            .with_description("Force-release (destructive)")
            .destructive()
        },
    );

    vec![
        renew_action,
        release_action,
        force_release_action,
        ActionEntry::new(
            "View holder",
            ActionKind::DeepLink(DeepLinkTarget::AgentByName(agent_name.to_string())),
        )
        .with_keybinding("h")
        .with_description("View holding agent"),
    ]
}

/// Build actions for the Agents screen.
#[must_use]
pub fn agents_actions(agent_name: &str) -> Vec<ActionEntry> {
    vec![
        ActionEntry::new(
            "View profile",
            ActionKind::Execute(format!("view_profile:{agent_name}")),
        )
        .with_keybinding("p")
        .with_description("Show agent details"),
        ActionEntry::new(
            "View inbox",
            ActionKind::DeepLink(DeepLinkTarget::ExplorerForAgent(agent_name.to_string())),
        )
        .with_keybinding("i")
        .with_description("View agent's inbox"),
        ActionEntry::new(
            "View reservations",
            ActionKind::DeepLink(DeepLinkTarget::ReservationByAgent(agent_name.to_string())),
        )
        .with_keybinding("r")
        .with_description("View agent's file locks"),
        ActionEntry::new(
            "Send message",
            ActionKind::Execute(format!("compose_to:{agent_name}")),
        )
        .with_keybinding("m")
        .with_description("Compose message to agent"),
    ]
}

/// Build actions for the Threads screen.
#[must_use]
pub fn threads_actions(thread_id: &str) -> Vec<ActionEntry> {
    vec![
        ActionEntry::new("View messages", ActionKind::Execute("view_messages".into()))
            .with_keybinding("v")
            .with_description("Show all messages in thread"),
        ActionEntry::new(
            "Summarize",
            ActionKind::Execute(format!("summarize:{thread_id}")),
        )
        .with_keybinding("s")
        .with_description("Generate thread summary"),
        ActionEntry::new(
            "Search in thread",
            ActionKind::Execute(format!("search_in:{thread_id}")),
        )
        .with_keybinding("/")
        .with_description("Search within thread"),
    ]
}

/// Build actions for the Timeline screen.
#[must_use]
pub fn timeline_actions(event_kind: &str, event_source: &str) -> Vec<ActionEntry> {
    vec![
        ActionEntry::new("View details", ActionKind::Execute("view_details".into()))
            .with_keybinding("v")
            .with_description("Show full event details"),
        ActionEntry::new(
            "Filter by type",
            ActionKind::Execute(format!("filter_kind:{event_kind}")),
        )
        .with_keybinding("t")
        .with_description("Show only this event type"),
        ActionEntry::new(
            "Filter by source",
            ActionKind::Execute(format!("filter_source:{event_source}")),
        )
        .with_keybinding("s")
        .with_description("Show only this source"),
        ActionEntry::new("Copy event", ActionKind::Execute("copy_event".into()))
            .with_keybinding("c")
            .with_description("Copy event text"),
    ]
}

/// Build batch actions for the Timeline screen when multiple rows are selected.
#[must_use]
pub fn timeline_batch_actions(selected_count: usize, copy_payload: String) -> Vec<ActionEntry> {
    let count = selected_count.max(1);
    if copy_payload.trim().is_empty() {
        return vec![
            ActionEntry::new(
                format!("Copy selected ({count})"),
                ActionKind::CopyToClipboard(String::new()),
            )
            .with_keybinding("y")
            .with_description("Copy selected timeline entries")
            .disabled("No selected timeline rows available to copy"),
        ];
    }

    vec![
        ActionEntry::new(
            format!("Copy selected ({count})"),
            ActionKind::CopyToClipboard(copy_payload),
        )
        .with_keybinding("y")
        .with_description("Copy selected timeline entries"),
    ]
}

/// Build actions for the Contacts screen.
#[must_use]
pub fn contacts_actions(from_agent: &str, to_agent: &str, status: &str) -> Vec<ActionEntry> {
    let mut actions = vec![
        ActionEntry::new(
            "View agent",
            ActionKind::DeepLink(DeepLinkTarget::AgentByName(to_agent.to_string())),
        )
        .with_keybinding("v")
        .with_description("View target agent profile"),
    ];

    if status == "pending" {
        actions.push(
            ActionEntry::new(
                "Approve",
                ActionKind::Execute(format!("approve_contact:{from_agent}:{to_agent}")),
            )
            .with_keybinding("a")
            .with_description("Approve contact request"),
        );
        actions.push(
            ActionEntry::new(
                "Deny",
                ActionKind::Execute(format!("deny_contact:{from_agent}:{to_agent}")),
            )
            .with_keybinding("d")
            .with_description("Deny contact request"),
        );
    }

    if status != "blocked" {
        actions.push(
            ActionEntry::new(
                "Block",
                ActionKind::ConfirmThenExecute {
                    title: "Block Contact".into(),
                    message: format!("Block contact from {from_agent}?"),
                    operation: format!("block_contact:{from_agent}:{to_agent}"),
                },
            )
            .with_keybinding("b")
            .with_description("Block this contact")
            .destructive(),
        );
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_entry_first_char() {
        let entry = ActionEntry::new("View body", ActionKind::Dismiss);
        assert_eq!(entry.first_char(), Some('v'));

        let entry = ActionEntry::new("Acknowledge", ActionKind::Dismiss);
        assert_eq!(entry.first_char(), Some('a'));
    }

    #[test]
    fn test_action_menu_state_navigation() {
        let entries = vec![
            ActionEntry::new("First", ActionKind::Dismiss),
            ActionEntry::new("Second", ActionKind::Dismiss),
            ActionEntry::new("Third", ActionKind::Dismiss),
        ];
        let mut state = ActionMenuState::new(entries, 5, "test");

        assert_eq!(state.selected, 0);

        state.move_down();
        assert_eq!(state.selected, 1);

        state.move_down();
        assert_eq!(state.selected, 2);

        state.move_down(); // Wrap to first
        assert_eq!(state.selected, 0);

        state.move_up();
        assert_eq!(state.selected, 2);

        state.move_up();
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn test_action_menu_state_jump_to_char() {
        let entries = vec![
            ActionEntry::new("Alpha", ActionKind::Dismiss),
            ActionEntry::new("Beta", ActionKind::Dismiss),
            ActionEntry::new("Charlie", ActionKind::Dismiss),
        ];
        let mut state = ActionMenuState::new(entries, 5, "test");

        assert!(state.jump_to_char('b'));
        assert_eq!(state.selected, 1);

        assert!(state.jump_to_char('C'));
        assert_eq!(state.selected, 2);

        assert!(!state.jump_to_char('z'));
        assert_eq!(state.selected, 2); // Should not change
    }

    #[test]
    fn test_messages_actions_has_correct_entries() {
        let actions = messages_actions(123, Some("thread-1"), "TestAgent");
        assert_eq!(actions.len(), 5);
        assert!(actions.iter().any(|a| a.label == "View body"));
        assert!(actions.iter().any(|a| a.label == "Acknowledge"));
        assert!(actions.iter().any(|a| a.label == "Jump to thread"));
        assert!(actions.iter().any(|a| a.label == "Jump to sender"));
    }

    #[test]
    fn test_reservations_actions_destructive_flag() {
        let actions = reservations_actions(Some(456), "TestAgent", "src/**");
        let force_release = actions.iter().find(|a| a.label == "Force-release");
        assert!(force_release.is_some());
        assert!(force_release.unwrap().is_destructive);
    }

    #[test]
    fn test_reservations_actions_without_id_disable_mutating_ops() {
        let actions = reservations_actions(None, "TestAgent", "src/**");

        for label in ["Renew", "Release", "Force-release"] {
            let entry = actions
                .iter()
                .find(|a| a.label == label)
                .expect("missing reservation action");
            assert!(!entry.enabled, "{label} should be disabled without id");
        }

        let holder = actions
            .iter()
            .find(|a| a.label == "View holder")
            .expect("missing view holder action");
        assert!(holder.enabled);
    }

    #[test]
    fn test_contacts_actions_pending_status() {
        let actions = contacts_actions("AgentA", "AgentB", "pending");
        assert!(actions.iter().any(|a| a.label == "Approve"));
        assert!(actions.iter().any(|a| a.label == "Deny"));
    }

    #[test]
    fn test_contacts_actions_approved_status() {
        let actions = contacts_actions("AgentA", "AgentB", "approved");
        assert!(!actions.iter().any(|a| a.label == "Approve"));
        assert!(!actions.iter().any(|a| a.label == "Deny"));
        assert!(actions.iter().any(|a| a.label == "Block"));
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(ftui::KeyEvent::new(code))
    }

    #[test]
    fn test_action_menu_manager_open_ignores_empty_entries() {
        let mut manager = ActionMenuManager::new();
        manager.open(Vec::new(), 0, "ctx");
        assert!(!manager.is_active());
        assert!(manager.handle_event(&key_event(KeyCode::Enter)).is_none());
    }

    #[test]
    fn test_action_menu_manager_enter_selects_and_closes() {
        let mut manager = ActionMenuManager::new();
        manager.open(
            vec![ActionEntry::new(
                "Run",
                ActionKind::Execute("run_action".to_string()),
            )],
            1,
            "message:42",
        );
        assert!(manager.is_active());

        let result = manager.handle_event(&key_event(KeyCode::Enter));
        assert!(!manager.is_active());
        match result {
            Some(ActionMenuResult::Selected(ActionKind::Execute(op), context)) => {
                assert_eq!(op, "run_action");
                assert_eq!(context, "message:42");
            }
            other => panic!("expected selected execute result, got {other:?}"),
        }
    }

    #[test]
    fn test_action_menu_manager_escape_dismisses() {
        let mut manager = ActionMenuManager::new();
        manager.open(
            vec![ActionEntry::new("Dismiss", ActionKind::Dismiss)],
            1,
            "ctx",
        );
        let result = manager.handle_event(&key_event(KeyCode::Escape));
        assert!(!manager.is_active());
        assert!(matches!(result, Some(ActionMenuResult::Dismissed)));
    }

    #[test]
    fn test_action_menu_manager_alpha_jump_then_enter() {
        let mut manager = ActionMenuManager::new();
        manager.open(
            vec![
                ActionEntry::new("Alpha", ActionKind::Execute("a".to_string())),
                ActionEntry::new("Beta", ActionKind::Execute("b".to_string())),
                ActionEntry::new("Charlie", ActionKind::Execute("c".to_string())),
            ],
            0,
            "ctx-jump",
        );

        let _ = manager.handle_event(&key_event(KeyCode::Char('c')));
        let result = manager.handle_event(&key_event(KeyCode::Enter));
        match result {
            Some(ActionMenuResult::Selected(ActionKind::Execute(op), context)) => {
                assert_eq!(op, "c");
                assert_eq!(context, "ctx-jump");
            }
            other => panic!("expected char-jump selected result, got {other:?}"),
        }
    }

    #[test]
    fn test_action_menu_manager_noop_when_inactive() {
        let mut manager = ActionMenuManager::new();
        assert!(manager.handle_event(&key_event(KeyCode::Enter)).is_none());
    }

    #[test]
    fn test_agents_threads_timeline_actions_have_core_entries() {
        let agents = agents_actions("BlueLake");
        assert!(agents.iter().any(|a| a.label == "View profile"));
        assert!(agents.iter().any(|a| a.label == "Send message"));

        let threads = threads_actions("th-1");
        assert!(threads.iter().any(|a| a.label == "View messages"));
        assert!(threads.iter().any(|a| a.label == "Summarize"));

        let timeline = timeline_actions("tool_call_end", "http");
        assert!(timeline.iter().any(|a| a.label == "View details"));
        assert!(timeline.iter().any(|a| a.label == "Filter by source"));
    }

    #[test]
    fn test_contacts_actions_blocked_status_omits_block_action() {
        let actions = contacts_actions("AgentA", "AgentB", "blocked");
        assert!(!actions.iter().any(|a| a.label == "Block"));
        assert!(!actions.iter().any(|a| a.label == "Approve"));
        assert!(!actions.iter().any(|a| a.label == "Deny"));
        assert!(actions.iter().any(|a| a.label == "View agent"));
    }

    // ── disabled entry tests ────────────────────────────────────

    #[test]
    fn disabled_entry_builder() {
        let entry =
            ActionEntry::new("Release", ActionKind::Dismiss).disabled("No active reservation");
        assert!(!entry.enabled);
        assert_eq!(
            entry.disabled_reason.as_deref(),
            Some("No active reservation")
        );
    }

    #[test]
    fn enabled_entry_by_default() {
        let entry = ActionEntry::new("View", ActionKind::Dismiss);
        assert!(entry.enabled);
        assert!(entry.disabled_reason.is_none());
    }

    #[test]
    fn disabled_entry_blocks_selection() {
        let entries = vec![
            ActionEntry::new("Enabled", ActionKind::Execute("ok".into())),
            ActionEntry::new("Disabled", ActionKind::Execute("nope".into()))
                .disabled("Not available"),
        ];
        let mut mgr = ActionMenuManager::new();
        mgr.open(entries, 0, "ctx");

        // Select second (disabled) entry.
        let event = Event::Key(ftui::KeyEvent::new(KeyCode::Down));
        mgr.handle_event(&event);

        // Try Enter on disabled entry.
        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let result = mgr.handle_event(&enter);
        assert!(
            matches!(result, Some(ActionMenuResult::DisabledAttempt(ref r)) if r == "Not available"),
            "Enter on disabled should return DisabledAttempt, got {result:?}",
        );
        // Menu should still be open.
        assert!(mgr.is_active());
    }

    #[test]
    fn enabled_entry_allows_selection() {
        let entries = vec![ActionEntry::new("Go", ActionKind::Execute("go".into()))];
        let mut mgr = ActionMenuManager::new();
        mgr.open(entries, 0, "ctx");

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let result = mgr.handle_event(&enter);
        assert!(matches!(result, Some(ActionMenuResult::Selected(_, _))));
        assert!(!mgr.is_active());
    }

    // ── Hit regions / dispatch / state machine edge cases (br-1xt0m.1.13.7) ──

    #[test]
    fn empty_state_move_up_down_noop() {
        let mut state = ActionMenuState::new(Vec::new(), 0, "ctx");
        state.move_up();
        assert_eq!(state.selected, 0);
        state.move_down();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn single_entry_wraps_to_self() {
        let entries = vec![ActionEntry::new("Only", ActionKind::Dismiss)];
        let mut state = ActionMenuState::new(entries, 0, "ctx");
        state.move_down();
        assert_eq!(state.selected, 0);
        state.move_up();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn navigation_through_mixed_enabled_disabled() {
        let entries = vec![
            ActionEntry::new("First", ActionKind::Dismiss),
            ActionEntry::new("Disabled1", ActionKind::Dismiss).disabled("reason1"),
            ActionEntry::new("Third", ActionKind::Dismiss),
            ActionEntry::new("Disabled2", ActionKind::Dismiss).disabled("reason2"),
        ];
        let mut state = ActionMenuState::new(entries, 0, "ctx");
        assert_eq!(state.selected, 0);

        // Navigation doesn't skip disabled entries — it just moves to them.
        state.move_down();
        assert_eq!(state.selected, 1);
        assert!(!state.selected_entry().unwrap().enabled);

        state.move_down();
        assert_eq!(state.selected, 2);
        assert!(state.selected_entry().unwrap().enabled);
    }

    #[test]
    fn jump_to_char_on_disabled_entry() {
        let entries = vec![
            ActionEntry::new("Alpha", ActionKind::Dismiss),
            ActionEntry::new("Beta", ActionKind::Dismiss).disabled("unavailable"),
        ];
        let mut state = ActionMenuState::new(entries, 0, "ctx");
        // Jump to 'b' should land on disabled entry.
        assert!(state.jump_to_char('b'));
        assert_eq!(state.selected, 1);
        assert!(!state.selected_entry().unwrap().enabled);
    }

    #[test]
    fn disabled_attempt_preserves_context_id() {
        let entries = vec![
            ActionEntry::new("Disabled", ActionKind::Execute("nope".into()))
                .disabled("Not available"),
        ];
        let mut mgr = ActionMenuManager::new();
        mgr.open(entries, 0, "my-context-123");

        let result = mgr.handle_event(&key_event(KeyCode::Enter));
        assert!(
            matches!(result, Some(ActionMenuResult::DisabledAttempt(ref r)) if r == "Not available"),
        );
        // Menu stays open — context ID should be preserved.
        assert!(mgr.is_active());
    }

    #[test]
    fn disabled_attempt_default_reason() {
        let mut entry = ActionEntry::new("NoReason", ActionKind::Execute("x".into()));
        entry.enabled = false;
        // No disabled_reason set — should get default message.
        let entries = vec![entry];
        let mut mgr = ActionMenuManager::new();
        mgr.open(entries, 0, "ctx");

        let result = mgr.handle_event(&key_event(KeyCode::Enter));
        assert!(
            matches!(result, Some(ActionMenuResult::DisabledAttempt(ref r)) if r == "Action unavailable"),
        );
    }

    #[test]
    fn messages_actions_without_thread_omits_jump_to_thread() {
        let actions = messages_actions(42, None, "TestAgent");
        assert!(!actions.iter().any(|a| a.label == "Jump to thread"));
        // Should still have other actions.
        assert!(actions.iter().any(|a| a.label == "View body"));
        assert!(actions.iter().any(|a| a.label == "Jump to sender"));
    }

    #[test]
    fn messages_batch_actions_include_expected_operations() {
        let actions = messages_batch_actions(3);
        assert_eq!(actions.len(), 3);

        let ops: Vec<String> = actions
            .iter()
            .filter_map(|entry| match &entry.action {
                ActionKind::Execute(op) => Some(op.clone()),
                _ => None,
            })
            .collect();

        assert!(ops.iter().any(|op| op == "batch_acknowledge"));
        assert!(ops.iter().any(|op| op == "batch_mark_read"));
        assert!(ops.iter().any(|op| op == "batch_mark_unread"));
    }

    #[test]
    fn reservations_batch_actions_encode_id_list_and_confirm() {
        let actions = reservations_batch_actions(2, &[22, 11]);
        assert_eq!(actions.len(), 2);

        let release = actions
            .iter()
            .find(|entry| entry.label.starts_with("Release selected"))
            .expect("release action");
        match &release.action {
            ActionKind::ConfirmThenExecute { operation, .. } => {
                assert_eq!(operation, "release:11,22");
            }
            other => panic!("expected ConfirmThenExecute, got {other:?}"),
        }
    }

    #[test]
    fn reservations_batch_actions_disable_when_any_id_missing() {
        let actions = reservations_batch_actions(3, &[1, 2]);
        assert_eq!(actions.len(), 2);
        assert!(actions.iter().all(|entry| !entry.enabled));
    }

    #[test]
    fn timeline_batch_actions_copy_payload_wired() {
        let actions = timeline_batch_actions(2, "line1\nline2".to_string());
        assert_eq!(actions.len(), 1);
        match &actions[0].action {
            ActionKind::CopyToClipboard(payload) => assert!(payload.contains("line1")),
            other => panic!("expected CopyToClipboard, got {other:?}"),
        }
    }

    #[test]
    fn non_key_events_consumed_when_active() {
        let mut mgr = ActionMenuManager::new();
        mgr.open(vec![ActionEntry::new("Go", ActionKind::Dismiss)], 0, "ctx");

        // Mouse event should be consumed (not forwarded), menu stays open.
        let mouse = Event::Mouse(ftui::MouseEvent {
            kind: ftui::MouseEventKind::Down(ftui::MouseButton::Left),
            x: 10,
            y: 10,
            modifiers: ftui::Modifiers::empty(),
        });
        let result = mgr.handle_event(&mouse);
        assert!(matches!(result, Some(ActionMenuResult::Consumed)));
        assert!(mgr.is_active());
    }

    #[test]
    fn close_then_reopen_resets_selection() {
        let mut mgr = ActionMenuManager::new();
        mgr.open(
            vec![
                ActionEntry::new("A", ActionKind::Dismiss),
                ActionEntry::new("B", ActionKind::Dismiss),
            ],
            0,
            "ctx",
        );
        // Move to B.
        mgr.handle_event(&key_event(KeyCode::Down));
        mgr.close();
        assert!(!mgr.is_active());

        // Reopen — selection should be back at 0.
        mgr.open(
            vec![
                ActionEntry::new("X", ActionKind::Dismiss),
                ActionEntry::new("Y", ActionKind::Dismiss),
            ],
            0,
            "ctx2",
        );
        assert!(mgr.is_active());
        let result = mgr.handle_event(&key_event(KeyCode::Enter));
        match result {
            Some(ActionMenuResult::Selected(_, ctx)) => assert_eq!(ctx, "ctx2"),
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn action_entry_builder_chain() {
        let entry = ActionEntry::new("Delete", ActionKind::Execute("delete".into()))
            .with_description("Permanently delete")
            .with_keybinding("d")
            .destructive();
        assert_eq!(entry.label, "Delete");
        assert_eq!(entry.description.as_deref(), Some("Permanently delete"));
        assert_eq!(entry.keybinding.as_deref(), Some("d"));
        assert!(entry.is_destructive);
        assert!(entry.enabled);
    }
}
