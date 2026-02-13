//! Centralized focus management for keyboard navigation.
//!
//! Provides a unified interface for tracking and managing keyboard focus
//! across the TUI. The [`FocusManager`] handles Tab/BackTab navigation,
//! focus trapping for modals, and focus indicator styling.
//!
//! # Focus Model
//!
//! Focus is organized hierarchically:
//! - **FocusContext**: The current focus context (screen, modal, dialog)
//! - **FocusTarget**: A specific focusable element within a context
//! - **FocusRing**: The ordered list of focusable elements for Tab navigation
//!
//! # Usage
//!
//! ```ignore
//! let mut focus = FocusManager::new();
//!
//! // Set up focus ring for a screen
//! focus.set_focus_ring(vec![
//!     FocusTarget::TextInput(0),  // Search bar
//!     FocusTarget::List(0),       // Result list
//!     FocusTarget::DetailPanel,
//! ]);
//!
//! // Handle Tab navigation
//! if focus.handle_tab(false) {
//!     // Focus moved to next element
//! }
//!
//! // Check current focus
//! if focus.is_focused(FocusTarget::TextInput(0)) {
//!     // Handle search bar input
//! }
//! ```

use ftui::Style;
use ftui_extras::theme;

// ──────────────────────────────────────────────────────────────────────
// FocusTarget — identifies a focusable element
// ──────────────────────────────────────────────────────────────────────

/// Identifies a focusable element within a screen or modal.
///
/// Each screen defines its own set of focus targets. Common targets
/// include search bars, result lists, detail panels, and filter rails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusTarget {
    /// Text input field (search bar, filter input, etc.)
    TextInput(u8),
    /// Scrollable list of items (results, messages, agents, etc.)
    List(u8),
    /// Detail/preview panel
    DetailPanel,
    /// Filter/facet rail
    FilterRail,
    /// Action button or button group
    Button(u8),
    /// Tab bar or navigation element
    TabBar,
    /// Modal dialog content
    ModalContent,
    /// Modal dialog actions (buttons)
    ModalActions,
    /// Custom target with identifier
    Custom(u8),
    /// No focus (used for initial state or after blur)
    None,
}

impl FocusTarget {
    /// Check if this target accepts text input.
    #[must_use]
    pub const fn accepts_text_input(self) -> bool {
        matches!(self, Self::TextInput(_))
    }

    /// Check if this target is a list that supports j/k navigation.
    #[must_use]
    pub const fn is_list(self) -> bool {
        matches!(self, Self::List(_))
    }

    /// Check if this is a modal-related focus target.
    #[must_use]
    pub const fn is_modal(self) -> bool {
        matches!(self, Self::ModalContent | Self::ModalActions)
    }
}

impl Default for FocusTarget {
    fn default() -> Self {
        Self::None
    }
}

// ──────────────────────────────────────────────────────────────────────
// FocusContext — the current focus scope
// ──────────────────────────────────────────────────────────────────────

/// The current focus scope/context.
///
/// Focus contexts form a stack: when a modal opens, it becomes the
/// active context, and when it closes, focus returns to the previous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FocusContext {
    /// Normal screen focus
    #[default]
    Screen,
    /// Command palette is active (traps all focus)
    CommandPalette,
    /// Modal dialog is active
    Modal,
    /// Action menu is active
    ActionMenu,
    /// Toast panel has focus
    ToastPanel,
}

impl FocusContext {
    /// Check if this context traps focus (blocks screen input).
    #[must_use]
    pub const fn traps_focus(self) -> bool {
        !matches!(self, Self::Screen)
    }

    /// Check if this context allows single-char shortcuts.
    #[must_use]
    pub const fn allows_shortcuts(self) -> bool {
        matches!(self, Self::Screen)
    }
}

#[derive(Debug, Clone, Copy)]
struct FocusSnapshot {
    context: FocusContext,
    target: FocusTarget,
}

// ──────────────────────────────────────────────────────────────────────
// FocusManager — centralized focus tracking
// ──────────────────────────────────────────────────────────────────────

/// Centralized focus manager for keyboard navigation.
///
/// Tracks the current focus target, manages Tab navigation through
/// a focus ring, and handles focus context switching for modals.
#[derive(Debug, Clone)]
pub struct FocusManager {
    /// Current focus context (screen, modal, etc.)
    context: FocusContext,
    /// Currently focused element
    current: FocusTarget,
    /// Ordered list of focusable elements for Tab navigation
    focus_ring: Vec<FocusTarget>,
    /// Index into focus_ring for current focus
    ring_index: usize,
    /// Stack of context/target snapshots for nested focus traps.
    snapshot_stack: Vec<FocusSnapshot>,
    /// Whether focus indicator should be visible
    indicator_visible: bool,
}

impl Default for FocusManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusManager {
    /// Create a new focus manager with default state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            context: FocusContext::Screen,
            current: FocusTarget::None,
            focus_ring: Vec::new(),
            ring_index: 0,
            snapshot_stack: Vec::new(),
            indicator_visible: true,
        }
    }

    /// Create a focus manager with an initial focus ring.
    #[must_use]
    pub fn with_ring(ring: Vec<FocusTarget>) -> Self {
        let current = ring.first().copied().unwrap_or(FocusTarget::None);
        Self {
            context: FocusContext::Screen,
            current,
            focus_ring: ring,
            ring_index: 0,
            snapshot_stack: Vec::new(),
            indicator_visible: true,
        }
    }

    fn ring_index_of(&self, target: FocusTarget) -> Option<usize> {
        self.focus_ring.iter().position(|&t| t == target)
    }

    fn restore_target(&mut self, target: FocusTarget) {
        self.current = target;
        if let Some(idx) = self.ring_index_of(target) {
            self.ring_index = idx;
        }
    }

    fn default_target_for_context(context: FocusContext) -> Option<FocusTarget> {
        match context {
            FocusContext::Modal => Some(FocusTarget::ModalContent),
            FocusContext::CommandPalette => Some(FocusTarget::TextInput(0)),
            FocusContext::ActionMenu => Some(FocusTarget::List(0)),
            FocusContext::ToastPanel => Some(FocusTarget::List(0)),
            FocusContext::Screen => None,
        }
    }

    // ── Getters ──────────────────────────────────────────────────────

    /// Get the current focus context.
    #[must_use]
    pub const fn context(&self) -> FocusContext {
        self.context
    }

    /// Get the currently focused target.
    #[must_use]
    pub const fn current(&self) -> FocusTarget {
        self.current
    }

    /// Check if a specific target is currently focused.
    #[must_use]
    pub fn is_focused(&self, target: FocusTarget) -> bool {
        self.current == target
    }

    /// Check if focus indicator should be visible.
    #[must_use]
    pub const fn indicator_visible(&self) -> bool {
        self.indicator_visible
    }

    /// Check if the current focus target accepts text input.
    #[must_use]
    pub fn consumes_text_input(&self) -> bool {
        self.current.accepts_text_input()
    }

    /// Check if focus is trapped (modal, palette, etc.).
    #[must_use]
    pub fn is_trapped(&self) -> bool {
        self.context.traps_focus()
    }

    // ── Focus Ring Management ────────────────────────────────────────

    /// Set the focus ring (ordered list of focusable elements).
    ///
    /// The first element becomes the default focus target.
    pub fn set_focus_ring(&mut self, ring: Vec<FocusTarget>) {
        self.focus_ring = ring;
        if self.focus_ring.is_empty() {
            self.ring_index = 0;
            if self.context == FocusContext::Screen {
                self.current = FocusTarget::None;
            }
            return;
        }

        if let Some(idx) = self.ring_index_of(self.current) {
            self.ring_index = idx;
            return;
        }

        self.ring_index = 0;
        if self.context == FocusContext::Screen {
            if let Some(&first) = self.focus_ring.first() {
                self.current = first;
            }
        }
    }

    /// Get the focus ring.
    #[must_use]
    pub fn focus_ring(&self) -> &[FocusTarget] {
        &self.focus_ring
    }

    // ── Focus Navigation ─────────────────────────────────────────────

    /// Move focus to a specific target.
    ///
    /// Returns `true` if focus changed.
    pub fn focus(&mut self, target: FocusTarget) -> bool {
        if self.current == target {
            return false;
        }
        self.current = target;

        // Update ring index if target is in the ring
        if let Some(idx) = self.ring_index_of(target) {
            self.ring_index = idx;
        }
        true
    }

    /// Move focus to the next element in the focus ring (Tab).
    ///
    /// Returns `true` if focus changed.
    pub fn focus_next(&mut self) -> bool {
        if self.focus_ring.is_empty() {
            return false;
        }
        let len = self.focus_ring.len();
        self.ring_index = if let Some(idx) = self.ring_index_of(self.current) {
            idx
        } else {
            len - 1
        };
        self.ring_index = (self.ring_index + 1) % len;
        let target = self.focus_ring[self.ring_index];
        self.focus(target)
    }

    /// Move focus to the previous element in the focus ring (BackTab).
    ///
    /// Returns `true` if focus changed.
    pub fn focus_prev(&mut self) -> bool {
        if self.focus_ring.is_empty() {
            return false;
        }
        let len = self.focus_ring.len();
        self.ring_index = self.ring_index_of(self.current).unwrap_or(0);
        self.ring_index = (self.ring_index + len - 1) % len;
        let target = self.focus_ring[self.ring_index];
        self.focus(target)
    }

    /// Handle Tab key press.
    ///
    /// - `shift`: If true, move backwards (Shift+Tab/BackTab)
    ///
    /// Returns `true` if the event was handled.
    pub fn handle_tab(&mut self, shift: bool) -> bool {
        if shift {
            self.focus_prev()
        } else {
            self.focus_next()
        }
    }

    /// Restore focus to the previous context/target snapshot.
    ///
    /// Used when closing nested modals/menus or canceling operations.
    /// No-op when no snapshot exists.
    pub fn restore(&mut self) {
        if let Some(snapshot) = self.snapshot_stack.pop() {
            self.context = snapshot.context;
            self.restore_target(snapshot.target);
        }
    }

    // ── Context Management ───────────────────────────────────────────

    /// Push a new focus context (e.g., opening a modal).
    ///
    /// Saves the current focus target for later restoration.
    pub fn push_context(&mut self, context: FocusContext) {
        self.snapshot_stack.push(FocusSnapshot {
            context: self.context,
            target: self.current,
        });
        self.context = context;

        // Set appropriate default focus for the context
        if let Some(target) = Self::default_target_for_context(context) {
            self.restore_target(target);
        }
    }

    /// Pop the current focus context (e.g., closing a modal).
    ///
    /// Restores the previous focus context/target snapshot.
    pub fn pop_context(&mut self) {
        if self.snapshot_stack.is_empty() {
            self.context = FocusContext::Screen;
            return;
        }
        self.restore();
    }

    // ── Indicator Visibility ─────────────────────────────────────────

    /// Show the focus indicator.
    pub fn show_indicator(&mut self) {
        self.indicator_visible = true;
    }

    /// Hide the focus indicator.
    pub fn hide_indicator(&mut self) {
        self.indicator_visible = false;
    }

    /// Toggle focus indicator visibility.
    pub fn toggle_indicator(&mut self) {
        self.indicator_visible = !self.indicator_visible;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Focus Indicator Styling
// ──────────────────────────────────────────────────────────────────────

/// Style for a focused element.
///
/// Uses the current theme's accent color with optional bold/underline.
#[must_use]
pub fn focus_style() -> Style {
    let p = theme::current_palette();
    Style::default().fg(p.accent_primary).bold()
}

/// Style for a focused text input (search bar, filter, etc.).
#[must_use]
pub fn focus_input_style() -> Style {
    let p = theme::current_palette();
    Style::default().fg(p.fg_primary).bg(p.bg_highlight)
}

/// Style for a focused list item.
#[must_use]
pub fn focus_list_style() -> Style {
    let p = theme::current_palette();
    Style::default().fg(p.fg_primary).bg(p.bg_surface).bold()
}

/// Style for the focus indicator border.
#[must_use]
pub fn focus_border_style() -> Style {
    let p = theme::current_palette();
    Style::default().fg(p.accent_primary)
}

/// Get the focus indicator character (used in margins/borders).
#[must_use]
pub const fn focus_indicator_char() -> char {
    '▶'
}

/// Get the unfocused indicator character.
#[must_use]
pub const fn unfocused_indicator_char() -> char {
    ' '
}

// ──────────────────────────────────────────────────────────────────────
// FocusRing Builder
// ──────────────────────────────────────────────────────────────────────

/// Builder for creating focus rings with common patterns.
#[derive(Debug, Default)]
pub struct FocusRingBuilder {
    targets: Vec<FocusTarget>,
}

impl FocusRingBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a text input target.
    #[must_use]
    pub fn text_input(mut self, id: u8) -> Self {
        self.targets.push(FocusTarget::TextInput(id));
        self
    }

    /// Add the primary search bar (TextInput 0).
    #[must_use]
    pub fn search_bar(self) -> Self {
        self.text_input(0)
    }

    /// Add a list target.
    #[must_use]
    pub fn list(mut self, id: u8) -> Self {
        self.targets.push(FocusTarget::List(id));
        self
    }

    /// Add the primary result list (List 0).
    #[must_use]
    pub fn result_list(self) -> Self {
        self.list(0)
    }

    /// Add the detail panel.
    #[must_use]
    pub fn detail_panel(mut self) -> Self {
        self.targets.push(FocusTarget::DetailPanel);
        self
    }

    /// Add the filter rail.
    #[must_use]
    pub fn filter_rail(mut self) -> Self {
        self.targets.push(FocusTarget::FilterRail);
        self
    }

    /// Add a button target.
    #[must_use]
    pub fn button(mut self, id: u8) -> Self {
        self.targets.push(FocusTarget::Button(id));
        self
    }

    /// Add a custom target.
    #[must_use]
    pub fn custom(mut self, id: u8) -> Self {
        self.targets.push(FocusTarget::Custom(id));
        self
    }

    /// Build the focus ring.
    #[must_use]
    pub fn build(self) -> Vec<FocusTarget> {
        self.targets
    }

    /// Build and create a FocusManager with this ring.
    #[must_use]
    pub fn into_manager(self) -> FocusManager {
        FocusManager::with_ring(self.build())
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_manager_default_state() {
        let fm = FocusManager::new();
        assert_eq!(fm.context(), FocusContext::Screen);
        assert_eq!(fm.current(), FocusTarget::None);
        assert!(fm.indicator_visible());
        assert!(!fm.is_trapped());
    }

    #[test]
    fn focus_manager_with_ring() {
        let fm = FocusManager::with_ring(vec![FocusTarget::TextInput(0), FocusTarget::List(0)]);
        assert_eq!(fm.current(), FocusTarget::TextInput(0));
    }

    #[test]
    fn focus_ring_tab_navigation() {
        let mut fm = FocusManager::with_ring(vec![
            FocusTarget::TextInput(0),
            FocusTarget::List(0),
            FocusTarget::DetailPanel,
        ]);

        assert_eq!(fm.current(), FocusTarget::TextInput(0));

        // Tab forward
        assert!(fm.handle_tab(false));
        assert_eq!(fm.current(), FocusTarget::List(0));

        assert!(fm.handle_tab(false));
        assert_eq!(fm.current(), FocusTarget::DetailPanel);

        // Wrap around
        assert!(fm.handle_tab(false));
        assert_eq!(fm.current(), FocusTarget::TextInput(0));

        // Tab backward (Shift+Tab)
        assert!(fm.handle_tab(true));
        assert_eq!(fm.current(), FocusTarget::DetailPanel);
    }

    #[test]
    fn focus_specific_target() {
        let mut fm = FocusManager::with_ring(vec![
            FocusTarget::TextInput(0),
            FocusTarget::List(0),
            FocusTarget::DetailPanel,
        ]);

        assert!(fm.focus(FocusTarget::DetailPanel));
        assert_eq!(fm.current(), FocusTarget::DetailPanel);

        // Same target should return false
        assert!(!fm.focus(FocusTarget::DetailPanel));
    }

    #[test]
    fn focus_context_push_pop() {
        let mut fm = FocusManager::with_ring(vec![FocusTarget::TextInput(0), FocusTarget::List(0)]);

        fm.focus(FocusTarget::List(0));
        assert_eq!(fm.current(), FocusTarget::List(0));
        assert_eq!(fm.context(), FocusContext::Screen);

        // Open modal
        fm.push_context(FocusContext::Modal);
        assert_eq!(fm.context(), FocusContext::Modal);
        assert_eq!(fm.current(), FocusTarget::ModalContent);
        assert!(fm.is_trapped());

        // Close modal
        fm.pop_context();
        assert_eq!(fm.context(), FocusContext::Screen);
        assert_eq!(fm.current(), FocusTarget::List(0));
        assert!(!fm.is_trapped());
    }

    #[test]
    fn nested_focus_context_pop_restores_lifo() {
        let mut fm = FocusManager::with_ring(vec![FocusTarget::TextInput(0), FocusTarget::List(0)]);
        fm.focus(FocusTarget::List(0));

        fm.push_context(FocusContext::Modal);
        assert_eq!(fm.context(), FocusContext::Modal);
        assert_eq!(fm.current(), FocusTarget::ModalContent);

        fm.push_context(FocusContext::ActionMenu);
        assert_eq!(fm.context(), FocusContext::ActionMenu);
        assert_eq!(fm.current(), FocusTarget::List(0));

        fm.pop_context();
        assert_eq!(fm.context(), FocusContext::Modal);
        assert_eq!(fm.current(), FocusTarget::ModalContent);

        fm.pop_context();
        assert_eq!(fm.context(), FocusContext::Screen);
        assert_eq!(fm.current(), FocusTarget::List(0));
    }

    #[test]
    fn focus_target_properties() {
        assert!(FocusTarget::TextInput(0).accepts_text_input());
        assert!(!FocusTarget::List(0).accepts_text_input());

        assert!(FocusTarget::List(0).is_list());
        assert!(!FocusTarget::TextInput(0).is_list());

        assert!(FocusTarget::ModalContent.is_modal());
        assert!(FocusTarget::ModalActions.is_modal());
        assert!(!FocusTarget::List(0).is_modal());
    }

    #[test]
    fn focus_ring_builder() {
        let ring = FocusRingBuilder::new()
            .search_bar()
            .filter_rail()
            .result_list()
            .detail_panel()
            .build();

        assert_eq!(ring.len(), 4);
        assert_eq!(ring[0], FocusTarget::TextInput(0));
        assert_eq!(ring[1], FocusTarget::FilterRail);
        assert_eq!(ring[2], FocusTarget::List(0));
        assert_eq!(ring[3], FocusTarget::DetailPanel);
    }

    #[test]
    fn focus_indicator_toggle() {
        let mut fm = FocusManager::new();
        assert!(fm.indicator_visible());

        fm.hide_indicator();
        assert!(!fm.indicator_visible());

        fm.show_indicator();
        assert!(fm.indicator_visible());

        fm.toggle_indicator();
        assert!(!fm.indicator_visible());
    }

    #[test]
    fn empty_focus_ring_navigation() {
        let mut fm = FocusManager::new();
        assert!(!fm.handle_tab(false));
        assert!(!fm.handle_tab(true));
    }

    #[test]
    fn tab_from_non_ring_focus_moves_to_ring_edges() {
        let mut fm = FocusManager::with_ring(vec![
            FocusTarget::TextInput(0),
            FocusTarget::List(0),
            FocusTarget::DetailPanel,
        ]);

        fm.focus(FocusTarget::ModalContent);
        assert!(fm.handle_tab(false));
        assert_eq!(fm.current(), FocusTarget::TextInput(0));

        fm.focus(FocusTarget::ModalContent);
        assert!(fm.handle_tab(true));
        assert_eq!(fm.current(), FocusTarget::DetailPanel);
    }

    #[test]
    fn set_focus_ring_adopts_first_target_when_current_missing_on_screen() {
        let mut fm = FocusManager::with_ring(vec![FocusTarget::TextInput(0), FocusTarget::List(0)]);
        fm.focus(FocusTarget::DetailPanel);

        fm.set_focus_ring(vec![FocusTarget::Button(0), FocusTarget::List(1)]);
        assert_eq!(fm.current(), FocusTarget::Button(0));
        assert!(fm.handle_tab(false));
        assert_eq!(fm.current(), FocusTarget::List(1));
    }

    #[test]
    fn set_focus_ring_does_not_clobber_modal_focus() {
        let mut fm = FocusManager::with_ring(vec![FocusTarget::TextInput(0), FocusTarget::List(0)]);
        fm.push_context(FocusContext::Modal);
        assert_eq!(fm.current(), FocusTarget::ModalContent);

        fm.set_focus_ring(vec![FocusTarget::Button(1), FocusTarget::List(1)]);
        assert_eq!(fm.context(), FocusContext::Modal);
        assert_eq!(fm.current(), FocusTarget::ModalContent);
    }

    #[test]
    fn consumes_text_input() {
        let mut fm = FocusManager::new();
        assert!(!fm.consumes_text_input());

        fm.focus(FocusTarget::TextInput(0));
        assert!(fm.consumes_text_input());

        fm.focus(FocusTarget::List(0));
        assert!(!fm.consumes_text_input());
    }
}
