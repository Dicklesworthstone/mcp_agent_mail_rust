//! Screen abstraction and registry for the `AgentMailTUI`.
//!
//! Each screen implements [`MailScreen`] and is identified by a
//! [`MailScreenId`].  The [`MAIL_SCREEN_REGISTRY`] provides static
//! metadata used by the chrome shell (tab bar, help overlay).

pub mod agents;
pub mod dashboard;
pub mod inspector;
pub mod messages;
pub mod reservations;
pub mod system_health;
pub mod threads;
pub mod timeline;
pub mod tool_metrics;

use ftui::layout::Rect;
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;

// Re-export the Event type that screens use
pub use ftui::Event;

// ──────────────────────────────────────────────────────────────────────
// MailScreenId — type-safe screen identifiers
// ──────────────────────────────────────────────────────────────────────

/// Identifies a TUI screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MailScreenId {
    Dashboard,
    Messages,
    Threads,
    Agents,
    Reservations,
    ToolMetrics,
    SystemHealth,
}

/// All screen IDs in display order.
pub const ALL_SCREEN_IDS: &[MailScreenId] = &[
    MailScreenId::Dashboard,
    MailScreenId::Messages,
    MailScreenId::Threads,
    MailScreenId::Agents,
    MailScreenId::Reservations,
    MailScreenId::ToolMetrics,
    MailScreenId::SystemHealth,
];

impl MailScreenId {
    /// Returns the 1-based display index.
    #[must_use]
    pub fn index(self) -> usize {
        ALL_SCREEN_IDS
            .iter()
            .position(|&id| id == self)
            .unwrap_or(0)
    }

    /// Return the next screen in tab order (wraps).
    #[must_use]
    pub fn next(self) -> Self {
        let idx = self.index();
        ALL_SCREEN_IDS[(idx + 1) % ALL_SCREEN_IDS.len()]
    }

    /// Return the previous screen in tab order (wraps).
    #[must_use]
    pub fn prev(self) -> Self {
        let idx = self.index();
        let len = ALL_SCREEN_IDS.len();
        ALL_SCREEN_IDS[(idx + len - 1) % len]
    }

    /// Look up a screen by 1-based number key (1..=N).
    #[must_use]
    pub fn from_number(n: usize) -> Option<Self> {
        if n == 0 || n > ALL_SCREEN_IDS.len() {
            None
        } else {
            Some(ALL_SCREEN_IDS[n - 1])
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// HelpEntry — keybinding documentation
// ──────────────────────────────────────────────────────────────────────

/// A keybinding entry for the help overlay.
#[derive(Debug, Clone)]
pub struct HelpEntry {
    pub key: &'static str,
    pub action: &'static str,
}

// ──────────────────────────────────────────────────────────────────────
// MailScreen trait — screen abstraction
// ──────────────────────────────────────────────────────────────────────

/// The screen abstraction for `AgentMailTUI`.
///
/// Each screen implements this trait and plugs into [`MailAppModel`].
/// The trait closely mirrors the ftui-demo-showcase `Screen` trait,
/// diverging only where `AgentMailTUI` semantics require (passing
/// `TuiSharedState` to `view` and `update`).
pub trait MailScreen {
    /// Handle a terminal event, returning a command.
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg>;

    /// Render the screen into the given area.
    fn view(&self, frame: &mut ftui::Frame<'_>, area: Rect, state: &TuiSharedState);

    /// Called on each tick (~100ms) with the global tick count.
    fn tick(&mut self, _tick_count: u64, _state: &TuiSharedState) {}

    /// Return screen-specific keybindings for the help overlay.
    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![]
    }

    /// Handle an incoming deep-link navigation request.
    ///
    /// Screens that support deep-linking should override this to jump
    /// to the relevant content.  Returns `true` if the link was handled.
    fn receive_deep_link(&mut self, _target: &DeepLinkTarget) -> bool {
        false
    }

    /// Whether this screen is currently consuming text input (search bar,
    /// filter field).  When true, single-character global shortcuts are
    /// suppressed.
    fn consumes_text_input(&self) -> bool {
        false
    }

    /// Return the currently focused/selected event, if any.
    ///
    /// Used by the command palette to inject context-aware quick actions
    /// based on the focused entity (agent, thread, tool, etc.).
    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        None
    }

    /// Title shown in the help overlay header.
    fn title(&self) -> &'static str;

    /// Short label for tab bar display (max ~12 chars).
    fn tab_label(&self) -> &'static str {
        self.title()
    }

    /// Reset the layout to factory defaults. Returns `true` if supported.
    fn reset_layout(&mut self) -> bool {
        false
    }

    /// Export the current layout as JSON to the standard export path.
    /// Returns the path on success.
    fn export_layout(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Import a layout from the standard export path. Returns `true` on success.
    fn import_layout(&mut self) -> bool {
        false
    }
}

/// Messages produced by individual screens, wrapped by `MailMsg`.
#[derive(Debug, Clone)]
pub enum MailScreenMsg {
    /// No action needed.
    Noop,
    /// Request navigation to another screen.
    Navigate(MailScreenId),
    /// Navigate to a screen with context (deep-link).
    DeepLink(DeepLinkTarget),
}

/// Context for deep-link navigation between screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLinkTarget {
    /// Jump to a specific timestamp in the Timeline screen.
    TimelineAtTime(i64),
    /// Jump to a specific message in the Messages screen.
    MessageById(i64),
    /// Jump to a specific thread in the Thread Explorer.
    ThreadById(String),
    /// Jump to an agent in the Agent Roster screen.
    AgentByName(String),
    /// Jump to a tool in the Tool Metrics screen.
    ToolByName(String),
    /// Jump to a project in the Dashboard screen.
    ProjectBySlug(String),
}

// ──────────────────────────────────────────────────────────────────────
// Screen Registry — static metadata
// ──────────────────────────────────────────────────────────────────────

/// Screen category for grouping in the help overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenCategory {
    Overview,
    Communication,
    Operations,
    System,
}

/// Static metadata for a screen.
#[derive(Debug, Clone)]
pub struct MailScreenMeta {
    pub id: MailScreenId,
    pub title: &'static str,
    pub short_label: &'static str,
    pub category: ScreenCategory,
    pub description: &'static str,
}

/// Static registry of all screens with their metadata.
pub const MAIL_SCREEN_REGISTRY: &[MailScreenMeta] = &[
    MailScreenMeta {
        id: MailScreenId::Dashboard,
        title: "Dashboard",
        short_label: "Dash",
        category: ScreenCategory::Overview,
        description: "Real-time operational overview with live event stream",
    },
    MailScreenMeta {
        id: MailScreenId::Messages,
        title: "Messages",
        short_label: "Msg",
        category: ScreenCategory::Communication,
        description: "Search and browse messages with detail panel",
    },
    MailScreenMeta {
        id: MailScreenId::Threads,
        title: "Threads",
        short_label: "Threads",
        category: ScreenCategory::Communication,
        description: "Thread explorer with conversation view",
    },
    MailScreenMeta {
        id: MailScreenId::Agents,
        title: "Agents",
        short_label: "Agents",
        category: ScreenCategory::Operations,
        description: "Agent roster with status and activity",
    },
    MailScreenMeta {
        id: MailScreenId::Reservations,
        title: "Reservations",
        short_label: "Reserv",
        category: ScreenCategory::Operations,
        description: "File reservation conflicts and status",
    },
    MailScreenMeta {
        id: MailScreenId::ToolMetrics,
        title: "Tool Metrics",
        short_label: "Tools",
        category: ScreenCategory::System,
        description: "Per-tool call counts, latency, and error rates",
    },
    MailScreenMeta {
        id: MailScreenId::SystemHealth,
        title: "System Health",
        short_label: "Health",
        category: ScreenCategory::System,
        description: "Database, queue, and connection diagnostics",
    },
];

/// Look up metadata for a screen ID.
#[must_use]
pub fn screen_meta(id: MailScreenId) -> &'static MailScreenMeta {
    MAIL_SCREEN_REGISTRY
        .iter()
        .find(|m| m.id == id)
        .expect("all screen IDs must be in registry")
}

/// All screen IDs in display order.
#[must_use]
pub const fn screen_ids() -> &'static [MailScreenId] {
    ALL_SCREEN_IDS
}

// ──────────────────────────────────────────────────────────────────────
// Placeholder screen for unimplemented screens
// ──────────────────────────────────────────────────────────────────────

/// Placeholder screen rendering a centered label.
pub struct PlaceholderScreen {
    id: MailScreenId,
}

impl PlaceholderScreen {
    #[must_use]
    pub const fn new(id: MailScreenId) -> Self {
        Self { id }
    }
}

impl MailScreen for PlaceholderScreen {
    fn update(&mut self, _event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        Cmd::None
    }

    fn view(&self, frame: &mut ftui::Frame<'_>, area: Rect, _state: &TuiSharedState) {
        use ftui::widgets::Widget;
        use ftui::widgets::paragraph::Paragraph;
        let meta = screen_meta(self.id);
        let text = format!("{} (coming soon)", meta.title);
        let p = Paragraph::new(text);
        p.render(area, frame);
    }

    fn title(&self) -> &'static str {
        screen_meta(self.id).title
    }

    fn tab_label(&self) -> &'static str {
        screen_meta(self.id).short_label
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_screen_ids_in_registry() {
        for &id in ALL_SCREEN_IDS {
            let meta = screen_meta(id);
            assert_eq!(meta.id, id);
            assert!(!meta.title.is_empty());
            assert!(!meta.short_label.is_empty());
        }
    }

    #[test]
    fn screen_count_matches() {
        assert_eq!(ALL_SCREEN_IDS.len(), MAIL_SCREEN_REGISTRY.len());
        assert_eq!(ALL_SCREEN_IDS.len(), 7);
    }

    #[test]
    fn next_prev_wraps() {
        let first = MailScreenId::Dashboard;
        let last = MailScreenId::SystemHealth;

        assert_eq!(last.next(), first);
        assert_eq!(first.prev(), last);
    }

    #[test]
    fn next_prev_round_trip() {
        for &id in ALL_SCREEN_IDS {
            assert_eq!(id.next().prev(), id);
            assert_eq!(id.prev().next(), id);
        }
    }

    #[test]
    fn from_number_valid() {
        assert_eq!(MailScreenId::from_number(1), Some(MailScreenId::Dashboard));
        assert_eq!(
            MailScreenId::from_number(7),
            Some(MailScreenId::SystemHealth)
        );
    }

    #[test]
    fn from_number_invalid() {
        assert_eq!(MailScreenId::from_number(0), None);
        assert_eq!(MailScreenId::from_number(8), None);
        assert_eq!(MailScreenId::from_number(100), None);
    }

    #[test]
    fn index_is_consistent() {
        for (i, &id) in ALL_SCREEN_IDS.iter().enumerate() {
            assert_eq!(id.index(), i);
        }
    }

    #[test]
    fn categories_are_assigned() {
        assert_eq!(
            screen_meta(MailScreenId::Dashboard).category,
            ScreenCategory::Overview
        );
        assert_eq!(
            screen_meta(MailScreenId::Messages).category,
            ScreenCategory::Communication
        );
        assert_eq!(
            screen_meta(MailScreenId::Agents).category,
            ScreenCategory::Operations
        );
        assert_eq!(
            screen_meta(MailScreenId::SystemHealth).category,
            ScreenCategory::System
        );
    }

    // ── Screen ID edge cases ────────────────────────────────────

    #[test]
    fn next_cycles_full_loop() {
        let mut id = MailScreenId::Dashboard;
        let mut visited = vec![id];
        for _ in 0..ALL_SCREEN_IDS.len() {
            id = id.next();
            visited.push(id);
        }
        // Should wrap back to start
        assert_eq!(visited[0], visited[ALL_SCREEN_IDS.len()]);
    }

    #[test]
    fn prev_cycles_full_loop() {
        let mut id = MailScreenId::Dashboard;
        let mut visited = vec![id];
        for _ in 0..ALL_SCREEN_IDS.len() {
            id = id.prev();
            visited.push(id);
        }
        assert_eq!(visited[0], visited[ALL_SCREEN_IDS.len()]);
    }

    #[test]
    fn from_number_covers_all_screens() {
        for i in 1..=ALL_SCREEN_IDS.len() {
            let id = MailScreenId::from_number(i).expect("valid index");
            assert_eq!(id, ALL_SCREEN_IDS[i - 1]);
        }
    }

    #[test]
    fn registry_descriptions_are_nonempty() {
        for meta in MAIL_SCREEN_REGISTRY {
            assert!(
                !meta.description.is_empty(),
                "{:?} has empty description",
                meta.id
            );
        }
    }

    #[test]
    fn screen_ids_returns_all_screen_ids() {
        assert_eq!(screen_ids().len(), ALL_SCREEN_IDS.len());
        assert_eq!(screen_ids(), ALL_SCREEN_IDS);
    }

    #[test]
    fn placeholder_screen_title_matches_meta() {
        for &id in &[
            MailScreenId::Agents,
            MailScreenId::Reservations,
            MailScreenId::ToolMetrics,
        ] {
            let screen = PlaceholderScreen::new(id);
            let meta = screen_meta(id);
            assert_eq!(screen.title(), meta.title);
            assert_eq!(screen.tab_label(), meta.short_label);
        }
    }

    #[test]
    fn placeholder_screen_update_is_noop() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = PlaceholderScreen::new(MailScreenId::Agents);
        let event = Event::Key(ftui::KeyEvent::new(ftui::KeyCode::Char('q')));
        let cmd = screen.update(&event, &state);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn placeholder_screen_renders_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let screen = PlaceholderScreen::new(MailScreenId::Agents);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        screen.view(&mut frame, ftui::layout::Rect::new(0, 0, 80, 24), &state);
    }

    #[test]
    fn deep_link_target_variants_exist() {
        // Ensure all variants can be constructed
        let _ = DeepLinkTarget::TimelineAtTime(0);
        let _ = DeepLinkTarget::MessageById(0);
        let _ = DeepLinkTarget::ThreadById(String::new());
        let _ = DeepLinkTarget::AgentByName(String::new());
        let _ = DeepLinkTarget::ToolByName(String::new());
        let _ = DeepLinkTarget::ProjectBySlug(String::new());
    }

    #[test]
    fn default_keybindings_and_deep_link_trait_defaults() {
        let config = mcp_agent_mail_core::Config::default();
        let _state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = PlaceholderScreen::new(MailScreenId::Agents);
        assert!(screen.keybindings().is_empty());
        assert!(!screen.receive_deep_link(&DeepLinkTarget::MessageById(1)));
        assert!(!screen.consumes_text_input());
    }
}
