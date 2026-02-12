//! ActionMenu — contextual per-item action overlay for TUI screens.
//!
//! Each screen can provide a set of actions relevant to the currently focused item.
//! The action menu appears as a floating overlay near the selected row, triggered
//! by pressing `.` (period).

use ftui::layout::Rect;
use ftui::{Cell, Event, Frame, KeyCode, KeyEventKind, PackedRgba};

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
    pub fn destructive(mut self) -> Self {
        self.is_destructive = true;
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
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Move selection up.
    pub fn move_up(&mut self) {
        if !self.entries.is_empty() && self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    pub fn move_down(&mut self) {
        if !self.entries.is_empty() && self.selected < self.entries.len() - 1 {
            self.selected += 1;
        }
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

/// Colors for action menu rendering.
const ACTION_MENU_BG: PackedRgba = PackedRgba::rgb(30, 30, 35);
const ACTION_MENU_BORDER: PackedRgba = PackedRgba::rgb(80, 80, 100);
const ACTION_MENU_SELECTED_BG: PackedRgba = PackedRgba::rgb(60, 80, 120);
const ACTION_MENU_DESTRUCTIVE: PackedRgba = PackedRgba::rgb(255, 100, 100);
#[allow(dead_code)] // Reserved for future use when rendering keybinding hints
const ACTION_MENU_KEYBINDING: PackedRgba = PackedRgba::rgb(150, 150, 180);

impl<'a> ActionMenu<'a> {
    /// Create a new action menu widget.
    #[must_use]
    pub const fn new(state: &'a ActionMenuState) -> Self {
        Self { state }
    }

    /// Render the action menu as a floating overlay.
    pub fn render(&self, terminal_area: Rect, frame: &mut Frame) {
        if self.state.is_empty() {
            return;
        }

        // Calculate menu dimensions
        let max_label_len = self
            .state
            .entries
            .iter()
            .map(|e| e.label.len() + e.keybinding.as_ref().map_or(0, |k| k.len() + 3))
            .max()
            .unwrap_or(10);
        let width = (max_label_len + 4).min(40).max(20) as u16;
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
                cell.bg = ACTION_MENU_BG;
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

            // Render each character
            for (j, ch) in text.chars().enumerate() {
                let col = inner.x + j as u16;
                if col >= inner.right() {
                    break;
                }
                let mut cell = Cell::from_char(ch);
                if entry.is_destructive {
                    cell.fg = ACTION_MENU_DESTRUCTIVE;
                } else if is_selected {
                    cell.fg = PackedRgba::rgb(255, 255, 255);
                } else {
                    cell.fg = PackedRgba::rgb(200, 200, 200);
                }
                if is_selected {
                    cell.bg = ACTION_MENU_SELECTED_BG;
                } else {
                    cell.bg = ACTION_MENU_BG;
                }
                frame.buffer.set_fast(col, row, cell);
            }

            // Fill rest of line with background
            let text_len = text.len() as u16;
            for col in (inner.x + text_len)..inner.right() {
                let mut cell = Cell::from_char(' ');
                if is_selected {
                    cell.bg = ACTION_MENU_SELECTED_BG;
                } else {
                    cell.bg = ACTION_MENU_BG;
                }
                frame.buffer.set_fast(col, row, cell);
            }
        }

        // Draw border - helper to create a border cell
        let border_cell = |ch: char| -> Cell {
            let mut cell = Cell::from_char(ch);
            cell.fg = ACTION_MENU_BORDER;
            cell.bg = ACTION_MENU_BG;
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
                frame.buffer.set_fast(col, area.y, border_cell(ch));
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
    pub fn is_active(&self) -> bool {
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
                    let action = entry.action.clone();
                    let context = state.context_id().to_string();
                    self.active = None;
                    Some(ActionMenuResult::Selected(action, context))
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

/// Build actions for the Reservations screen.
#[must_use]
pub fn reservations_actions(
    reservation_id: i64,
    agent_name: &str,
    path_pattern: &str,
) -> Vec<ActionEntry> {
    vec![
        ActionEntry::new(
            "Renew",
            ActionKind::Execute(format!("renew:{reservation_id}")),
        )
        .with_keybinding("r")
        .with_description("Extend reservation TTL"),
        ActionEntry::new(
            "Release",
            ActionKind::Execute(format!("release:{reservation_id}")),
        )
        .with_keybinding("e")
        .with_description("Release this reservation"),
        ActionEntry::new(
            "Force-release",
            ActionKind::ConfirmThenExecute {
                title: "Force Release".into(),
                message: format!("Force-release reservation on {path_pattern}?"),
                operation: format!("force_release:{reservation_id}"),
            },
        )
        .with_keybinding("f")
        .with_description("Force-release (destructive)")
        .destructive(),
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

        state.move_down(); // Should stay at 2
        assert_eq!(state.selected, 2);

        state.move_up();
        assert_eq!(state.selected, 1);

        state.move_up();
        assert_eq!(state.selected, 0);

        state.move_up(); // Should stay at 0
        assert_eq!(state.selected, 0);
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
        let actions = reservations_actions(456, "TestAgent", "src/**");
        let force_release = actions.iter().find(|a| a.label == "Force-release");
        assert!(force_release.is_some());
        assert!(force_release.unwrap().is_destructive);
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
}
