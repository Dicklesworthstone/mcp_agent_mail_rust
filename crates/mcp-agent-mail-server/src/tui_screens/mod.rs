//! Screen abstraction and registry for the `AgentMailTUI`.
//!
//! Each screen implements [`MailScreen`] and is identified by a
//! [`MailScreenId`].  The [`MAIL_SCREEN_REGISTRY`] provides static
//! metadata used by the chrome shell (tab bar, help overlay).

pub mod agents;
pub mod analytics;
pub mod archive_browser;
pub mod attachments;
pub mod contacts;
pub mod dashboard;
pub mod explorer;
pub mod inspector;
pub mod messages;
pub mod projects;
pub mod reservations;
pub mod search;
pub mod system_health;
pub mod threads;
pub mod timeline;
pub mod tool_metrics;

use ftui::layout::Rect;
use ftui_runtime::program::Cmd;
use std::collections::HashSet;
use std::hash::Hash;

use crate::tui_action_menu::ActionEntry;
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
    Search,
    Reservations,
    ToolMetrics,
    SystemHealth,
    Timeline,
    Projects,
    Contacts,
    Explorer,
    Analytics,
    Attachments,
    ArchiveBrowser,
}

/// All screen IDs in display order.
pub const ALL_SCREEN_IDS: &[MailScreenId] = &[
    MailScreenId::Dashboard,
    MailScreenId::Messages,
    MailScreenId::Threads,
    MailScreenId::Agents,
    MailScreenId::Search,
    MailScreenId::Reservations,
    MailScreenId::ToolMetrics,
    MailScreenId::SystemHealth,
    MailScreenId::Timeline,
    MailScreenId::Projects,
    MailScreenId::Contacts,
    MailScreenId::Explorer,
    MailScreenId::Analytics,
    MailScreenId::Attachments,
    MailScreenId::ArchiveBrowser,
];

/// Shifted number-row symbols used for direct jump bindings beyond screen 10.
///
/// Mapping: `!`=11, `@`=12, `#`=13, `$`=14, ... `(`=19.
pub const SHIFTED_DIGIT_JUMP_KEYS: &[char] = &['!', '@', '#', '$', '%', '^', '&', '*', '('];

fn screen_from_display_index(idx: usize) -> Option<MailScreenId> {
    if idx == 0 || idx > ALL_SCREEN_IDS.len() {
        None
    } else {
        Some(ALL_SCREEN_IDS[idx - 1])
    }
}

/// Return the direct jump key label for a 1-based display index.
///
/// - `1..=9` map to `"1"`..`"9"`
/// - `10` maps to `"0"`
/// - `11+` map to shifted symbols (`!`, `@`, `#`, ...)
#[must_use]
pub const fn jump_key_label_for_display_index(display_index: usize) -> Option<&'static str> {
    match display_index {
        1 => Some("1"),
        2 => Some("2"),
        3 => Some("3"),
        4 => Some("4"),
        5 => Some("5"),
        6 => Some("6"),
        7 => Some("7"),
        8 => Some("8"),
        9 => Some("9"),
        10 => Some("0"),
        11 => Some("!"),
        12 => Some("@"),
        13 => Some("#"),
        14 => Some("$"),
        15 => Some("%"),
        16 => Some("^"),
        17 => Some("&"),
        18 => Some("*"),
        19 => Some("("),
        _ => None,
    }
}

/// Return the direct jump key label for a screen, if one exists.
#[must_use]
pub fn jump_key_label_for_screen(id: MailScreenId) -> Option<&'static str> {
    jump_key_label_for_display_index(id.index() + 1)
}

/// Parse a jump key character into the corresponding screen.
///
/// Supports numeric keys and shifted number-row symbols for 11+ screens.
#[must_use]
pub fn screen_from_jump_key(key: char) -> Option<MailScreenId> {
    if key.is_ascii_digit() {
        let n = key.to_digit(10).map_or(0, |d| d as usize);
        return MailScreenId::from_number(n);
    }

    let shifted_offset = SHIFTED_DIGIT_JUMP_KEYS.iter().position(|&c| c == key)?;
    screen_from_display_index(11 + shifted_offset)
}

/// Human-readable key legend for direct jump navigation.
#[must_use]
pub fn jump_key_legend() -> String {
    let mut labels = vec!["1-9".to_string(), "0".to_string()];
    let extra = ALL_SCREEN_IDS.len().saturating_sub(10);
    labels.extend((0..extra).filter_map(|offset| {
        jump_key_label_for_display_index(11 + offset).map(ToString::to_string)
    }));
    labels.join(",")
}

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

    /// Look up a screen by numeric jump index.
    ///
    /// Keys `1`-`9` map to screens 1-9; key `0` maps to screen 10.
    /// Values >10 are accepted for translated jump keys (e.g. `!` => 11).
    #[must_use]
    pub fn from_number(n: usize) -> Option<Self> {
        let idx = if n == 0 { 10 } else { n };
        screen_from_display_index(idx)
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
// SelectionState — reusable multi-selection helper
// ──────────────────────────────────────────────────────────────────────

/// Reusable selection state for list/table based screens.
#[derive(Debug, Clone)]
pub struct SelectionState<T>
where
    T: Eq + Hash + Clone,
{
    selected: HashSet<T>,
    visual_mode: bool,
}

impl<T> Default for SelectionState<T>
where
    T: Eq + Hash + Clone,
{
    fn default() -> Self {
        Self {
            selected: HashSet::new(),
            visual_mode: false,
        }
    }
}

impl<T> SelectionState<T>
where
    T: Eq + Hash + Clone,
{
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.visual_mode = false;
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.selected.len()
    }

    #[must_use]
    pub fn contains(&self, value: &T) -> bool {
        self.selected.contains(value)
    }

    pub fn select(&mut self, value: T) {
        self.selected.insert(value);
    }

    pub fn deselect(&mut self, value: &T) {
        self.selected.remove(value);
    }

    /// Toggle selection membership for `value`. Returns true if selected after toggle.
    pub fn toggle(&mut self, value: T) -> bool {
        if self.selected.contains(&value) {
            self.selected.remove(&value);
            false
        } else {
            self.selected.insert(value);
            true
        }
    }

    pub fn select_all<I>(&mut self, values: I)
    where
        I: IntoIterator<Item = T>,
    {
        self.selected.extend(values);
    }

    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.selected.retain(|value| keep(value));
    }

    #[must_use]
    pub fn selected_items(&self) -> Vec<T> {
        self.selected.iter().cloned().collect()
    }

    #[must_use]
    pub const fn visual_mode(&self) -> bool {
        self.visual_mode
    }

    pub const fn set_visual_mode(&mut self, enabled: bool) {
        self.visual_mode = enabled;
    }

    pub const fn toggle_visual_mode(&mut self) -> bool {
        self.visual_mode = !self.visual_mode;
        self.visual_mode
    }
}

// ──────────────────────────────────────────────────────────────────────
// MailScreen trait — screen abstraction
// ──────────────────────────────────────────────────────────────────────

/// The screen abstraction for `AgentMailTUI`.
///
/// Each screen implements this trait and plugs into [`crate::tui_app::MailAppModel`].
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

    /// Return a brief context-sensitive tip for the help overlay.
    ///
    /// Displayed at the top of the screen-specific section to orient the
    /// user about what the current screen does and how to use it.
    /// Returning `None` (the default) omits the description line.
    fn context_help_tip(&self) -> Option<&'static str> {
        None
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

    /// Return contextual actions for the currently selected item.
    ///
    /// Called when the user presses `.` (period) to open the action menu.
    /// Returns `(actions, anchor_row, context_id)` or `None` if no selection.
    fn contextual_actions(&self) -> Option<(Vec<ActionEntry>, u16, String)> {
        None
    }

    /// Return the currently focused item's text content for clipboard copy.
    ///
    /// Called when the user presses `y` (yank) to copy the focused item.
    /// Each screen should return a meaningful string representation of the
    /// currently selected/focused entity, or `None` if nothing is selected.
    fn copyable_content(&self) -> Option<String> {
        None
    }

    /// Handle an action dispatched from the action menu or macro engine.
    ///
    /// The default implementation returns `Cmd::None`.
    fn handle_action(&mut self, _operation: &str, _context: &str) -> Cmd<MailScreenMsg> {
        Cmd::None
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

// ──────────────────────────────────────────────────────────────────────
// Dirty-state / invalidation contract
// ──────────────────────────────────────────────────────────────────────

/// Snapshot of all data-channel generation counters.
///
/// Screens store this after each tick and later compare against a fresh
/// snapshot via [`dirty_since`] to determine which channels have new data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataGeneration {
    /// Event ring buffer `total_pushed` counter.
    pub event_total_pushed: u64,
    /// Console log sequence counter.
    pub console_log_seq: u64,
    /// DB stats mutation generation.
    pub db_stats_gen: u64,
    /// HTTP request recording generation.
    pub request_gen: u64,
}

impl DataGeneration {
    /// Sentinel value that is guaranteed to differ from any real generation
    /// snapshot (which starts at zero and only increments). Use this as the
    /// initial `last_data_gen` so the first tick always evaluates as dirty.
    #[must_use]
    pub const fn stale() -> Self {
        Self {
            event_total_pushed: u64::MAX,
            console_log_seq: u64::MAX,
            db_stats_gen: u64::MAX,
            request_gen: u64::MAX,
        }
    }
}

/// Bit-flags indicating which data channels have changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct DirtyFlags {
    /// New events were pushed to the event ring buffer.
    pub events: bool,
    /// New entries were added to the console log.
    pub console_log: bool,
    /// DB stats were updated.
    pub db_stats: bool,
    /// New HTTP requests were recorded.
    pub requests: bool,
}

impl DirtyFlags {
    /// Returns `true` if any flag is set.
    #[must_use]
    pub const fn any(self) -> bool {
        self.events || self.console_log || self.db_stats || self.requests
    }

    /// Returns flags with everything dirty (for backward compatibility).
    #[must_use]
    pub const fn all() -> Self {
        Self {
            events: true,
            console_log: true,
            db_stats: true,
            requests: true,
        }
    }
}

/// Compare two generation snapshots and return which channels changed.
#[must_use]
pub const fn dirty_since(prev: &DataGeneration, current: &DataGeneration) -> DirtyFlags {
    DirtyFlags {
        events: current.event_total_pushed != prev.event_total_pushed,
        console_log: current.console_log_seq != prev.console_log_seq,
        db_stats: current.db_stats_gen != prev.db_stats_gen,
        requests: current.request_gen != prev.request_gen,
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
    /// Execute an action-menu operation on the active screen.
    /// Contains `(operation, context_id)`.
    ActionExecute(String, String),
}

/// Context for deep-link navigation between screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLinkTarget {
    /// Jump to a specific timestamp in the Timeline screen.
    TimelineAtTime(i64),
    /// Jump to a specific message in the Messages screen.
    MessageById(i64),
    /// Open compose form in Messages screen with pre-filled recipient.
    ComposeToAgent(String),
    /// Open quick-reply form in Messages screen for a message id.
    ReplyToMessage(i64),
    /// Jump to a specific thread in the Thread Explorer.
    ThreadById(String),
    /// Jump to an agent in the Agent Roster screen.
    AgentByName(String),
    /// Jump to a tool in the Tool Metrics screen.
    ToolByName(String),
    /// Jump to a project in the Dashboard screen.
    ProjectBySlug(String),
    /// Jump to a specific file reservation in the Reservations screen.
    ReservationByAgent(String),
    /// Jump to a contact link between two agents.
    ContactByPair(String, String),
    /// Jump to the Explorer filtered for a specific agent.
    ExplorerForAgent(String),
    /// Open Search Cockpit with query bar focused (and optional pre-filled query).
    SearchFocused(String),
}

impl DeepLinkTarget {
    /// Returns the screen ID that this deep-link targets.
    #[must_use]
    pub const fn target_screen(&self) -> MailScreenId {
        match self {
            Self::TimelineAtTime(_) => MailScreenId::Timeline,
            Self::MessageById(_) | Self::ComposeToAgent(_) | Self::ReplyToMessage(_) => {
                MailScreenId::Messages
            }
            Self::ThreadById(_) => MailScreenId::Threads,
            Self::AgentByName(_) => MailScreenId::Agents,
            Self::ToolByName(_) => MailScreenId::ToolMetrics,
            Self::ProjectBySlug(_) => MailScreenId::Projects,
            Self::ReservationByAgent(_) => MailScreenId::Reservations,
            Self::ContactByPair(_, _) => MailScreenId::Contacts,
            Self::ExplorerForAgent(_) => MailScreenId::Explorer,
            Self::SearchFocused(_) => MailScreenId::Search,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Screen Registry — static metadata
// ──────────────────────────────────────────────────────────────────────

/// Screen category for grouping in the help overlay and chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenCategory {
    Overview,
    Communication,
    Operations,
    System,
}

impl ScreenCategory {
    /// Short display label (max 4 chars) for compact UI.
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Overview => "Over",
            Self::Communication => "Comm",
            Self::Operations => "Ops",
            Self::System => "Sys",
        }
    }

    /// Full display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Communication => "Communication",
            Self::Operations => "Operations",
            Self::System => "System",
        }
    }

    /// All category variants in display order.
    pub const ALL: &[Self] = &[
        Self::Overview,
        Self::Communication,
        Self::Operations,
        Self::System,
    ];
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
        id: MailScreenId::Search,
        title: "Search",
        short_label: "Find",
        category: ScreenCategory::Communication,
        description: "Unified search across messages, agents, and projects with facet filters",
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
    MailScreenMeta {
        id: MailScreenId::Timeline,
        title: "Timeline",
        short_label: "Time",
        category: ScreenCategory::Overview,
        description: "Chronological event timeline with cursor + inspector",
    },
    MailScreenMeta {
        id: MailScreenId::Projects,
        title: "Projects",
        short_label: "Proj",
        category: ScreenCategory::Overview,
        description: "Project browser with per-project stats and detail",
    },
    MailScreenMeta {
        id: MailScreenId::Contacts,
        title: "Contacts",
        short_label: "Links",
        category: ScreenCategory::Communication,
        description: "Cross-agent contact links and policy display",
    },
    MailScreenMeta {
        id: MailScreenId::Explorer,
        title: "Explorer",
        short_label: "Explore",
        category: ScreenCategory::Communication,
        description: "Unified inbox/outbox explorer with direction, grouping, and ack filters",
    },
    MailScreenMeta {
        id: MailScreenId::Analytics,
        title: "Analytics",
        short_label: "Insight",
        category: ScreenCategory::System,
        description: "Anomaly insight feed with confidence scoring and actionable next steps",
    },
    MailScreenMeta {
        id: MailScreenId::Attachments,
        title: "Attachments",
        short_label: "Attach",
        category: ScreenCategory::Communication,
        description: "Attachment browser with inline preview and source provenance trails",
    },
    MailScreenMeta {
        id: MailScreenId::ArchiveBrowser,
        title: "Archive Browser",
        short_label: "Archive",
        category: ScreenCategory::Operations,
        description: "Two-pane Git archive browser with directory tree and file content preview",
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
    use std::collections::HashSet;

    #[test]
    fn selection_state_toggle_and_clear() {
        let mut state = SelectionState::new();
        assert!(state.is_empty());
        assert!(state.toggle("msg-1"));
        assert!(state.contains(&"msg-1"));
        assert!(!state.toggle("msg-1"));
        assert!(!state.contains(&"msg-1"));
        state.select("msg-2");
        state.select("msg-3");
        assert_eq!(state.len(), 2);
        state.clear();
        assert!(state.is_empty());
        assert!(!state.visual_mode());
    }

    #[test]
    fn selection_state_select_all_and_retain() {
        let mut state = SelectionState::new();
        state.select_all(vec![1_i64, 2, 3, 4]);
        assert_eq!(state.len(), 4);
        state.retain(|id| *id % 2 == 0);
        let selected: HashSet<i64> = state.selected_items().into_iter().collect();
        assert_eq!(selected, HashSet::from([2, 4]));
    }

    #[test]
    fn selection_state_visual_mode_toggle() {
        let mut state = SelectionState::<i64>::new();
        assert!(!state.visual_mode());
        assert!(state.toggle_visual_mode());
        assert!(state.visual_mode());
        state.set_visual_mode(false);
        assert!(!state.visual_mode());
    }

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
        assert_eq!(ALL_SCREEN_IDS.len(), 15);
    }

    #[test]
    fn next_prev_wraps() {
        let first = ALL_SCREEN_IDS[0];
        let last = *ALL_SCREEN_IDS.last().unwrap();

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
        assert_eq!(MailScreenId::from_number(5), Some(MailScreenId::Search));
        assert_eq!(
            MailScreenId::from_number(8),
            Some(MailScreenId::SystemHealth)
        );
        assert_eq!(MailScreenId::from_number(9), Some(MailScreenId::Timeline));
        // 0 maps to screen 10
        assert_eq!(MailScreenId::from_number(0), Some(MailScreenId::Projects));
        assert_eq!(MailScreenId::from_number(11), Some(MailScreenId::Contacts));
    }

    #[test]
    fn from_number_invalid() {
        assert_eq!(MailScreenId::from_number(16), None);
        assert_eq!(MailScreenId::from_number(100), None);
    }

    #[test]
    fn jump_key_labels_cover_current_registry() {
        assert_eq!(
            jump_key_label_for_display_index(1),
            Some("1"),
            "screen 1 should use key 1"
        );
        assert_eq!(
            jump_key_label_for_display_index(10),
            Some("0"),
            "screen 10 should use key 0"
        );
        assert_eq!(
            jump_key_label_for_display_index(11),
            Some("!"),
            "screen 11 should use key !"
        );
        assert_eq!(
            jump_key_label_for_display_index(14),
            Some("$"),
            "screen 14 should use key $"
        );
    }

    #[test]
    fn screen_from_jump_key_supports_shifted_symbols() {
        assert_eq!(screen_from_jump_key('1'), Some(MailScreenId::Dashboard));
        assert_eq!(screen_from_jump_key('0'), Some(MailScreenId::Projects));
        assert_eq!(screen_from_jump_key('!'), Some(MailScreenId::Contacts));
        assert_eq!(screen_from_jump_key('@'), Some(MailScreenId::Explorer));
        assert_eq!(screen_from_jump_key('#'), Some(MailScreenId::Analytics));
        assert_eq!(screen_from_jump_key('$'), Some(MailScreenId::Attachments));
        assert_eq!(
            screen_from_jump_key('%'),
            Some(MailScreenId::ArchiveBrowser)
        );
        assert_eq!(screen_from_jump_key(')'), None);
    }

    #[test]
    fn jump_key_legend_reflects_screen_count() {
        let legend = jump_key_legend();
        assert_eq!(legend, "1-9,0,!,@,#,$,%");
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

    #[test]
    fn every_category_has_at_least_one_screen() {
        for &cat in ScreenCategory::ALL {
            let count = MAIL_SCREEN_REGISTRY
                .iter()
                .filter(|m| m.category == cat)
                .count();
            assert!(count > 0, "category {cat:?} has no screens in the registry");
        }
    }

    #[test]
    fn category_labels_are_non_empty() {
        for &cat in ScreenCategory::ALL {
            assert!(!cat.label().is_empty());
            assert!(!cat.short_label().is_empty());
            assert!(cat.short_label().len() <= 4);
        }
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
            assert_eq!(
                jump_key_label_for_screen(id),
                jump_key_label_for_display_index(i)
            );
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
        let _ = DeepLinkTarget::ComposeToAgent(String::new());
        let _ = DeepLinkTarget::ThreadById(String::new());
        let _ = DeepLinkTarget::AgentByName(String::new());
        let _ = DeepLinkTarget::ToolByName(String::new());
        let _ = DeepLinkTarget::ProjectBySlug(String::new());
        let _ = DeepLinkTarget::ReservationByAgent(String::new());
        let _ = DeepLinkTarget::ContactByPair(String::new(), String::new());
        let _ = DeepLinkTarget::ExplorerForAgent(String::new());
        let _ = DeepLinkTarget::SearchFocused(String::new());
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

    // ── Shell Navigation & Discoverability Contracts (br-1xt0m.1.13.6) ──

    #[test]
    fn jump_key_legend_reflects_15_screens() {
        let legend = jump_key_legend();
        assert!(legend.contains("1-9"), "legend should have digits");
        assert!(legend.contains('0'), "legend should have 0 for screen 10");
        // With 15 screens, expect shifted symbols for 11-15
        for sym in ['!', '@', '#', '$', '%'] {
            assert!(
                legend.contains(sym),
                "legend should contain '{sym}' for 14 screens, got: {legend}"
            );
        }
    }

    #[test]
    fn jump_key_coverage_all_screens() {
        // Every screen must be reachable by a jump key.
        for (i, &id) in ALL_SCREEN_IDS.iter().enumerate() {
            let key = jump_key_label_for_screen(id);
            assert!(key.is_some(), "screen {i} ({id:?}) has no jump key");
        }
    }

    #[test]
    fn short_labels_fit_in_tab_width() {
        for meta in MAIL_SCREEN_REGISTRY {
            assert!(
                meta.short_label.len() <= 12,
                "{:?} short_label '{}' exceeds 12 chars",
                meta.id,
                meta.short_label,
            );
        }
    }

    #[test]
    fn screen_descriptions_nonempty_and_actionable() {
        for meta in MAIL_SCREEN_REGISTRY {
            assert!(
                !meta.description.is_empty(),
                "{:?} has empty description",
                meta.id
            );
            // Description should be a full sentence (starts uppercase, ends with period)
            let first = meta.description.chars().next().unwrap();
            assert!(
                first.is_uppercase(),
                "{:?} description should start uppercase: '{}'",
                meta.id,
                meta.description,
            );
        }
    }

    #[test]
    fn every_category_represented() {
        let cats: std::collections::HashSet<_> =
            MAIL_SCREEN_REGISTRY.iter().map(|m| m.category).collect();
        for &cat in ScreenCategory::ALL {
            assert!(
                cats.contains(&cat),
                "ScreenCategory::{cat:?} has no assigned screens"
            );
        }
    }

    #[test]
    fn screen_ids_unique_in_registry() {
        let mut seen = std::collections::HashSet::new();
        for meta in MAIL_SCREEN_REGISTRY {
            assert!(
                seen.insert(meta.id),
                "duplicate screen id {:?} in MAIL_SCREEN_REGISTRY",
                meta.id,
            );
        }
    }

    #[test]
    fn jump_key_labels_unique() {
        let mut seen = std::collections::HashSet::new();
        for &id in ALL_SCREEN_IDS {
            if let Some(key) = jump_key_label_for_screen(id) {
                assert!(seen.insert(key), "duplicate jump key '{key}' for {id:?}");
            }
        }
    }

    #[test]
    fn screen_from_jump_key_roundtrips() {
        // For every screen with a jump key, screen_from_jump_key should return it.
        for &id in ALL_SCREEN_IDS {
            if let Some(key_str) = jump_key_label_for_screen(id) {
                let ch = key_str.chars().next().unwrap();
                let found = screen_from_jump_key(ch);
                assert_eq!(
                    found,
                    Some(id),
                    "screen_from_jump_key('{ch}') should return {id:?}"
                );
            }
        }
    }

    // ── Dirty-state / invalidation contract tests ────────────────

    #[test]
    fn dirty_since_identical_generations_produces_no_flags() {
        let data_gen = DataGeneration {
            event_total_pushed: 42,
            console_log_seq: 10,
            db_stats_gen: 3,
            request_gen: 99,
        };
        let flags = dirty_since(&data_gen, &data_gen);
        assert!(!flags.any(), "identical generations must produce zero dirty flags");
        assert!(!flags.events);
        assert!(!flags.console_log);
        assert!(!flags.db_stats);
        assert!(!flags.requests);
    }

    #[test]
    fn dirty_since_detects_event_change() {
        let prev = DataGeneration::default();
        let current = DataGeneration {
            event_total_pushed: 1,
            ..prev
        };
        let flags = dirty_since(&prev, &current);
        assert!(flags.events, "events flag must be set");
        assert!(!flags.console_log);
        assert!(!flags.db_stats);
        assert!(!flags.requests);
    }

    #[test]
    fn dirty_since_detects_console_log_change() {
        let prev = DataGeneration::default();
        let current = DataGeneration {
            console_log_seq: 5,
            ..prev
        };
        let flags = dirty_since(&prev, &current);
        assert!(!flags.events);
        assert!(flags.console_log);
        assert!(!flags.db_stats);
        assert!(!flags.requests);
    }

    #[test]
    fn dirty_since_detects_db_stats_change() {
        let prev = DataGeneration::default();
        let current = DataGeneration {
            db_stats_gen: 1,
            ..prev
        };
        let flags = dirty_since(&prev, &current);
        assert!(!flags.events);
        assert!(!flags.console_log);
        assert!(flags.db_stats);
        assert!(!flags.requests);
    }

    #[test]
    fn dirty_since_detects_request_change() {
        let prev = DataGeneration::default();
        let current = DataGeneration {
            request_gen: 7,
            ..prev
        };
        let flags = dirty_since(&prev, &current);
        assert!(!flags.events);
        assert!(!flags.console_log);
        assert!(!flags.db_stats);
        assert!(flags.requests);
    }

    #[test]
    fn dirty_since_detects_multiple_changes() {
        let prev = DataGeneration {
            event_total_pushed: 10,
            console_log_seq: 5,
            db_stats_gen: 2,
            request_gen: 100,
        };
        let current = DataGeneration {
            event_total_pushed: 15,
            console_log_seq: 5,  // unchanged
            db_stats_gen: 3,
            request_gen: 100,    // unchanged
        };
        let flags = dirty_since(&prev, &current);
        assert!(flags.events);
        assert!(!flags.console_log);
        assert!(flags.db_stats);
        assert!(!flags.requests);
        assert!(flags.any());
    }

    #[test]
    fn dirty_flags_all_sets_every_flag() {
        let flags = DirtyFlags::all();
        assert!(flags.events);
        assert!(flags.console_log);
        assert!(flags.db_stats);
        assert!(flags.requests);
        assert!(flags.any());
    }

    #[test]
    fn dirty_flags_default_is_clean() {
        let flags = DirtyFlags::default();
        assert!(!flags.any());
    }

    #[test]
    fn data_generation_default_is_zero() {
        let data_gen = DataGeneration::default();
        assert_eq!(data_gen.event_total_pushed, 0);
        assert_eq!(data_gen.console_log_seq, 0);
        assert_eq!(data_gen.db_stats_gen, 0);
        assert_eq!(data_gen.request_gen, 0);
    }
}
