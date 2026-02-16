//! Centralized focus management for keyboard navigation.
//!
//! Provides a unified interface for tracking and managing keyboard focus
//! across the TUI. The [`FocusManager`] handles Tab/BackTab navigation,
//! focus trapping for modals, and focus indicator styling.
//!
//! # Focus Model
//!
//! Focus is organized hierarchically:
//! - **`FocusContext`**: The current focus context (screen, modal, dialog)
//! - **`FocusTarget`**: A specific focusable element within a context
//! - **`FocusRing`**: The ordered list of focusable elements for Tab navigation
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

use ftui::{Style, layout::Rect};
use ftui_extras::theme;

#[cfg(test)]
use crate::tui_screens::ALL_SCREEN_IDS;
use crate::tui_screens::MailScreenId;

// ──────────────────────────────────────────────────────────────────────
// FocusTarget — identifies a focusable element
// ──────────────────────────────────────────────────────────────────────

/// Identifies a focusable element within a screen or modal.
///
/// Each screen defines its own set of focus targets. Common targets
/// include search bars, result lists, detail panels, and filter rails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
    #[default]
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

// ──────────────────────────────────────────────────────────────────────
// FocusGraph — per-screen spatial focus topology
// ──────────────────────────────────────────────────────────────────────

/// Directional neighbors for a [`FocusNode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FocusNeighbors {
    /// Closest node above this node.
    pub up: Option<usize>,
    /// Closest node below this node.
    pub down: Option<usize>,
    /// Closest node to the left of this node.
    pub left: Option<usize>,
    /// Closest node to the right of this node.
    pub right: Option<usize>,
}

/// A focusable panel node for a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusNode {
    /// Stable identifier used across re-renders and layout changes.
    pub id: &'static str,
    /// Focus target used by [`FocusManager`].
    pub target: FocusTarget,
    /// Rendered panel rectangle.
    pub rect: Rect,
    /// Tab traversal order.
    pub tab_index: usize,
    /// Spatially derived directional neighbors.
    pub neighbors: FocusNeighbors,
}

/// Spatial focus graph for a single screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusGraph {
    screen: MailScreenId,
    nodes: Vec<FocusNode>,
}

impl FocusGraph {
    /// Build a focus graph for a specific screen and render area.
    #[must_use]
    pub fn for_screen(screen: MailScreenId, area: Rect) -> Self {
        let templates = focus_templates_for_screen_with_area(screen, area);
        let rects: Vec<Rect> = templates
            .iter()
            .map(|template| rect_from_template(area, *template))
            .collect();

        let mut nodes = Vec::with_capacity(templates.len());
        for (idx, template) in templates.iter().copied().enumerate() {
            nodes.push(FocusNode {
                id: template.id,
                target: template.target,
                rect: rects[idx],
                tab_index: template.tab_index,
                neighbors: FocusNeighbors {
                    up: best_neighbor(&rects, idx, Direction::Up),
                    down: best_neighbor(&rects, idx, Direction::Down),
                    left: best_neighbor(&rects, idx, Direction::Left),
                    right: best_neighbor(&rects, idx, Direction::Right),
                },
            });
        }

        Self { screen, nodes }
    }

    /// The screen this graph belongs to.
    #[must_use]
    pub const fn screen(&self) -> MailScreenId {
        self.screen
    }

    /// Focus nodes in tab-order declaration order.
    #[must_use]
    pub fn nodes(&self) -> &[FocusNode] {
        &self.nodes
    }

    /// Lookup node index by focus target.
    #[must_use]
    pub fn node_index(&self, target: FocusTarget) -> Option<usize> {
        self.nodes.iter().position(|node| node.target == target)
    }

    /// Lookup node by focus target.
    #[must_use]
    pub fn node(&self, target: FocusTarget) -> Option<&FocusNode> {
        self.node_index(target).map(|idx| &self.nodes[idx])
    }
}

/// Convenience wrapper for callers that only need the graph.
#[must_use]
pub fn focus_graph_for_screen(screen: MailScreenId, area: Rect) -> FocusGraph {
    FocusGraph::for_screen(screen, area)
}

/// Tab-focus ring declaration for a screen (stable order, no geometry).
#[must_use]
pub fn focus_ring_for_screen(screen: MailScreenId) -> Vec<FocusTarget> {
    focus_templates_for_screen(screen)
        .iter()
        .map(|template| template.target)
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct FocusNodeTemplate {
    id: &'static str,
    target: FocusTarget,
    tab_index: usize,
    x_permille: u16,
    y_permille: u16,
    w_permille: u16,
    h_permille: u16,
}

const fn tpl(
    id: &'static str,
    target: FocusTarget,
    tab_index: usize,
    x_permille: u16,
    y_permille: u16,
    w_permille: u16,
    h_permille: u16,
) -> FocusNodeTemplate {
    FocusNodeTemplate {
        id,
        target,
        tab_index,
        x_permille,
        y_permille,
        w_permille,
        h_permille,
    }
}

const DASHBOARD_FOCUS: [FocusNodeTemplate; 5] = [
    tpl(
        "dashboard.throughput",
        FocusTarget::Custom(0),
        0,
        0,
        0,
        650,
        280,
    ),
    tpl(
        "dashboard.events",
        FocusTarget::List(0),
        1,
        0,
        280,
        650,
        720,
    ),
    tpl(
        "dashboard.metrics",
        FocusTarget::Custom(1),
        2,
        650,
        0,
        350,
        250,
    ),
    tpl(
        "dashboard.heatmap",
        FocusTarget::Custom(2),
        3,
        650,
        250,
        350,
        350,
    ),
    tpl(
        "dashboard.anomalies",
        FocusTarget::Custom(3),
        4,
        650,
        600,
        350,
        400,
    ),
];

const MESSAGES_FOCUS: [FocusNodeTemplate; 3] = [
    tpl(
        "messages.search",
        FocusTarget::TextInput(0),
        0,
        0,
        0,
        1000,
        120,
    ),
    tpl("messages.list", FocusTarget::List(0), 1, 0, 120, 450, 880),
    tpl(
        "messages.preview",
        FocusTarget::DetailPanel,
        2,
        450,
        120,
        550,
        880,
    ),
];

const MESSAGES_FOCUS_COMPACT: [FocusNodeTemplate; 2] = [
    tpl(
        "messages.search",
        FocusTarget::TextInput(0),
        0,
        0,
        0,
        1000,
        120,
    ),
    tpl("messages.list", FocusTarget::List(0), 1, 0, 120, 1000, 880),
];

const THREADS_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("threads.list", FocusTarget::List(0), 0, 0, 0, 420, 1000),
    tpl(
        "threads.detail",
        FocusTarget::DetailPanel,
        1,
        420,
        0,
        580,
        1000,
    ),
];

const AGENTS_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("agents.list", FocusTarget::List(0), 0, 0, 0, 430, 1000),
    tpl(
        "agents.detail",
        FocusTarget::DetailPanel,
        1,
        430,
        0,
        570,
        1000,
    ),
];

const SEARCH_FOCUS: [FocusNodeTemplate; 3] = [
    tpl(
        "search.query",
        FocusTarget::TextInput(0),
        0,
        0,
        0,
        1000,
        140,
    ),
    tpl(
        "search.facets",
        FocusTarget::FilterRail,
        1,
        0,
        140,
        260,
        860,
    ),
    tpl(
        "search.results",
        FocusTarget::List(0),
        2,
        260,
        140,
        740,
        860,
    ),
];

const RESERVATIONS_FOCUS: [FocusNodeTemplate; 2] = [
    tpl(
        "reservations.list",
        FocusTarget::List(0),
        0,
        0,
        0,
        500,
        1000,
    ),
    tpl(
        "reservations.detail",
        FocusTarget::DetailPanel,
        1,
        500,
        0,
        500,
        1000,
    ),
];

const TOOL_METRICS_FOCUS: [FocusNodeTemplate; 3] = [
    tpl(
        "tool_metrics.list",
        FocusTarget::List(0),
        0,
        0,
        0,
        320,
        1000,
    ),
    tpl(
        "tool_metrics.chart",
        FocusTarget::Custom(0),
        1,
        320,
        0,
        380,
        500,
    ),
    tpl(
        "tool_metrics.detail",
        FocusTarget::DetailPanel,
        2,
        700,
        0,
        300,
        1000,
    ),
];

const SYSTEM_HEALTH_FOCUS: [FocusNodeTemplate; 4] = [
    tpl(
        "system_health.probes",
        FocusTarget::Custom(0),
        0,
        0,
        0,
        300,
        500,
    ),
    tpl(
        "system_health.resources",
        FocusTarget::Custom(1),
        1,
        300,
        0,
        350,
        500,
    ),
    tpl(
        "system_health.anomalies",
        FocusTarget::Custom(2),
        2,
        650,
        0,
        350,
        500,
    ),
    tpl(
        "system_health.events",
        FocusTarget::List(0),
        3,
        0,
        500,
        1000,
        500,
    ),
];

const TIMELINE_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("timeline.events", FocusTarget::List(0), 0, 0, 0, 450, 1000),
    tpl(
        "timeline.inspector",
        FocusTarget::DetailPanel,
        1,
        450,
        0,
        550,
        1000,
    ),
];

const PROJECTS_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("projects.list", FocusTarget::List(0), 0, 0, 0, 400, 1000),
    tpl(
        "projects.detail",
        FocusTarget::DetailPanel,
        1,
        400,
        0,
        600,
        1000,
    ),
];

const CONTACTS_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("contacts.list", FocusTarget::List(0), 0, 0, 0, 420, 1000),
    tpl(
        "contacts.detail",
        FocusTarget::DetailPanel,
        1,
        420,
        0,
        580,
        1000,
    ),
];

const EXPLORER_FOCUS: [FocusNodeTemplate; 4] = [
    tpl(
        "explorer.search",
        FocusTarget::TextInput(0),
        0,
        0,
        0,
        1000,
        140,
    ),
    tpl(
        "explorer.filters",
        FocusTarget::FilterRail,
        1,
        0,
        140,
        250,
        860,
    ),
    tpl("explorer.list", FocusTarget::List(0), 2, 250, 140, 450, 860),
    tpl(
        "explorer.preview",
        FocusTarget::DetailPanel,
        3,
        700,
        140,
        300,
        860,
    ),
];

const ANALYTICS_FOCUS: [FocusNodeTemplate; 4] = [
    tpl(
        "analytics.charts",
        FocusTarget::Custom(0),
        0,
        0,
        0,
        650,
        500,
    ),
    tpl("analytics.table", FocusTarget::List(0), 1, 0, 500, 650, 500),
    tpl(
        "analytics.filters",
        FocusTarget::FilterRail,
        2,
        650,
        0,
        350,
        300,
    ),
    tpl(
        "analytics.summary",
        FocusTarget::DetailPanel,
        3,
        650,
        300,
        350,
        700,
    ),
];

const ATTACHMENTS_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("attachments.list", FocusTarget::List(0), 0, 0, 0, 420, 1000),
    tpl(
        "attachments.preview",
        FocusTarget::DetailPanel,
        1,
        420,
        0,
        580,
        1000,
    ),
];

const ARCHIVE_BROWSER_FOCUS: [FocusNodeTemplate; 2] = [
    tpl("archive.tree", FocusTarget::List(0), 0, 0, 0, 400, 1000),
    tpl(
        "archive.preview",
        FocusTarget::DetailPanel,
        1,
        400,
        0,
        600,
        1000,
    ),
];

const fn focus_templates_for_screen(screen: MailScreenId) -> &'static [FocusNodeTemplate] {
    match screen {
        MailScreenId::Dashboard => &DASHBOARD_FOCUS,
        MailScreenId::Messages => &MESSAGES_FOCUS,
        MailScreenId::Threads => &THREADS_FOCUS,
        MailScreenId::Agents => &AGENTS_FOCUS,
        MailScreenId::Search => &SEARCH_FOCUS,
        MailScreenId::Reservations => &RESERVATIONS_FOCUS,
        MailScreenId::ToolMetrics => &TOOL_METRICS_FOCUS,
        MailScreenId::SystemHealth => &SYSTEM_HEALTH_FOCUS,
        MailScreenId::Timeline => &TIMELINE_FOCUS,
        MailScreenId::Projects => &PROJECTS_FOCUS,
        MailScreenId::Contacts => &CONTACTS_FOCUS,
        MailScreenId::Explorer => &EXPLORER_FOCUS,
        MailScreenId::Analytics => &ANALYTICS_FOCUS,
        MailScreenId::Attachments => &ATTACHMENTS_FOCUS,
        MailScreenId::ArchiveBrowser => &ARCHIVE_BROWSER_FOCUS,
    }
}

const fn focus_templates_for_screen_with_area(
    screen: MailScreenId,
    area: Rect,
) -> &'static [FocusNodeTemplate] {
    match screen {
        // Message screen hides the detail split on compact terminals.
        MailScreenId::Messages if area.width < 68 || area.height < 8 => &MESSAGES_FOCUS_COMPACT,
        _ => focus_templates_for_screen(screen),
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn scale_permille(total: u16, permille: u16) -> u16 {
    let scaled = (u32::from(total) * u32::from(permille)) / 1_000;
    u16::try_from(scaled).unwrap_or(u16::MAX)
}

fn rect_from_template(area: Rect, template: FocusNodeTemplate) -> Rect {
    let total_w = area.width.max(1);
    let total_h = area.height.max(1);

    let area_x = area.x;
    let area_y = area.y;
    let area_right = area_x.saturating_add(total_w);
    let area_bottom = area_y.saturating_add(total_h);

    let mut x = area_x.saturating_add(scale_permille(total_w, template.x_permille));
    let mut y = area_y.saturating_add(scale_permille(total_h, template.y_permille));

    if x >= area_right {
        x = area_right.saturating_sub(1);
    }
    if y >= area_bottom {
        y = area_bottom.saturating_sub(1);
    }

    let mut width = scale_permille(total_w, template.w_permille).max(1);
    let mut height = scale_permille(total_h, template.h_permille).max(1);

    if x.saturating_add(width) > area_right {
        width = area_right.saturating_sub(x).max(1);
    }
    if y.saturating_add(height) > area_bottom {
        height = area_bottom.saturating_sub(y).max(1);
    }

    Rect::new(x, y, width, height)
}

fn rect_center(rect: Rect) -> (i32, i32) {
    let cx = i32::from(rect.x) + i32::from(rect.width.saturating_sub(1)) / 2;
    let cy = i32::from(rect.y) + i32::from(rect.height.saturating_sub(1)) / 2;
    (cx, cy)
}

fn axis_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    end.saturating_sub(start)
}

fn orthogonal_overlap(source: Rect, candidate: Rect, direction: Direction) -> u16 {
    match direction {
        Direction::Up | Direction::Down => {
            axis_overlap(source.x, source.width, candidate.x, candidate.width)
        }
        Direction::Left | Direction::Right => {
            axis_overlap(source.y, source.height, candidate.y, candidate.height)
        }
    }
}

fn best_neighbor(rects: &[Rect], source_idx: usize, direction: Direction) -> Option<usize> {
    if source_idx >= rects.len() {
        return None;
    }
    let (sx, sy) = rect_center(rects[source_idx]);
    let source = rects[source_idx];
    let mut best: Option<((bool, u32, u32, u32), usize)> = None;

    for (idx, rect) in rects.iter().enumerate() {
        if idx == source_idx {
            continue;
        }
        let (tx, ty) = rect_center(*rect);
        let dx = tx - sx;
        let dy = ty - sy;

        let in_direction = match direction {
            Direction::Up => dy < 0,
            Direction::Down => dy > 0,
            Direction::Left => dx < 0,
            Direction::Right => dx > 0,
        };
        if !in_direction {
            continue;
        }

        let overlap = orthogonal_overlap(source, *rect, direction);
        let (primary, secondary) = match direction {
            Direction::Up | Direction::Down => (dy.unsigned_abs(), dx.unsigned_abs()),
            Direction::Left | Direction::Right => (dx.unsigned_abs(), dy.unsigned_abs()),
        };
        // Prefer nodes that overlap on the orthogonal axis, then shortest
        // primary distance, then shortest off-axis distance.
        let score = (
            overlap == 0,
            primary,
            secondary,
            u32::MAX - u32::from(overlap),
        );

        match best {
            None => best = Some((score, idx)),
            Some((best_score, _)) if score < best_score => best = Some((score, idx)),
            _ => {}
        }
    }

    best.map(|(_, idx)| idx)
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
    /// Index into `focus_ring` for current focus
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
    pub const fn new() -> Self {
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

    const fn default_target_for_context(context: FocusContext) -> Option<FocusTarget> {
        match context {
            FocusContext::Modal => Some(FocusTarget::ModalContent),
            FocusContext::CommandPalette => Some(FocusTarget::TextInput(0)),
            FocusContext::ActionMenu | FocusContext::ToastPanel => Some(FocusTarget::List(0)),
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
    pub const fn consumes_text_input(&self) -> bool {
        self.current.accepts_text_input()
    }

    /// Check if focus is trapped (modal, palette, etc.).
    #[must_use]
    pub const fn is_trapped(&self) -> bool {
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
        self.ring_index = self.ring_index_of(self.current).unwrap_or_else(|| len - 1);
        self.ring_index = (self.ring_index + 1) % len;
        let target = self.focus_ring[self.ring_index];
        self.focus(target)
    }

    /// Move focus to the previous element in the focus ring (`BackTab`).
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
    pub const fn show_indicator(&mut self) {
        self.indicator_visible = true;
    }

    /// Hide the focus indicator.
    pub const fn hide_indicator(&mut self) {
        self.indicator_visible = false;
    }

    /// Toggle focus indicator visibility.
    pub const fn toggle_indicator(&mut self) {
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

    /// Add the primary search bar (`TextInput` 0).
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

    /// Build and create a `FocusManager` with this ring.
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

    #[test]
    fn focus_graph_defined_for_all_screens() {
        for &screen in ALL_SCREEN_IDS {
            let graph = FocusGraph::for_screen(screen, Rect::new(0, 0, 120, 40));
            assert!(
                !graph.nodes().is_empty(),
                "screen {screen:?} should define at least one focus node"
            );
        }
    }

    #[test]
    fn focus_graph_node_ids_stable_across_resizes() {
        for &screen in ALL_SCREEN_IDS {
            let compact = FocusGraph::for_screen(screen, Rect::new(0, 0, 90, 28));
            let wide = FocusGraph::for_screen(screen, Rect::new(0, 0, 210, 64));

            let compact_ids: Vec<&str> = compact.nodes().iter().map(|node| node.id).collect();
            let wide_ids: Vec<&str> = wide.nodes().iter().map(|node| node.id).collect();
            assert_eq!(
                compact_ids, wide_ids,
                "screen {screen:?} should keep stable node IDs across layout changes"
            );

            let compact_targets: Vec<FocusTarget> =
                compact.nodes().iter().map(|node| node.target).collect();
            let wide_targets: Vec<FocusTarget> =
                wide.nodes().iter().map(|node| node.target).collect();
            assert_eq!(
                compact_targets, wide_targets,
                "screen {screen:?} should keep stable focus targets across layout changes"
            );
        }
    }

    #[test]
    fn focus_graph_messages_neighbors_match_spatial_layout() {
        let graph = FocusGraph::for_screen(MailScreenId::Messages, Rect::new(0, 0, 120, 40));
        let list_idx = graph
            .node_index(FocusTarget::List(0))
            .expect("messages list node should exist");
        let search_idx = graph
            .node_index(FocusTarget::TextInput(0))
            .expect("messages search node should exist");
        let detail_idx = graph
            .node_index(FocusTarget::DetailPanel)
            .expect("messages detail node should exist");

        assert_eq!(graph.nodes()[list_idx].neighbors.right, Some(detail_idx));
        assert_eq!(graph.nodes()[detail_idx].neighbors.left, Some(list_idx));
        assert_eq!(graph.nodes()[list_idx].neighbors.up, Some(search_idx));
        assert_eq!(graph.nodes()[detail_idx].neighbors.right, None);
        assert_eq!(graph.nodes()[list_idx].neighbors.left, None);
    }

    #[test]
    fn focus_graph_messages_compact_layout_omits_hidden_detail_panel() {
        let graph = FocusGraph::for_screen(MailScreenId::Messages, Rect::new(0, 0, 60, 20));
        assert_eq!(graph.nodes().len(), 2);
        assert!(graph.node(FocusTarget::DetailPanel).is_none());
        assert!(graph.node(FocusTarget::TextInput(0)).is_some());
        assert!(graph.node(FocusTarget::List(0)).is_some());
    }

    #[test]
    fn focus_graph_dashboard_node_count_matches_layout_templates() {
        let graph = FocusGraph::for_screen(MailScreenId::Dashboard, Rect::new(0, 0, 160, 48));
        assert_eq!(graph.nodes().len(), 5);
    }

    #[test]
    fn best_neighbor_prefers_orthogonally_overlapping_candidate() {
        let rects = vec![
            Rect::new(50, 10, 20, 20), // source
            Rect::new(20, 10, 20, 20), // overlap on Y axis (should win)
            Rect::new(30, 0, 10, 5),   // closer primary distance, but no overlap
        ];
        let winner = best_neighbor(&rects, 0, Direction::Left);
        assert_eq!(winner, Some(1));
    }

    #[test]
    fn best_neighbor_falls_back_to_distance_when_no_overlap_exists() {
        let rects = vec![
            Rect::new(50, 30, 20, 10), // source
            Rect::new(20, 0, 20, 5),   // left candidate A (farther)
            Rect::new(30, 0, 10, 5),   // left candidate B (nearer)
        ];
        let winner = best_neighbor(&rects, 0, Direction::Left);
        assert_eq!(winner, Some(2));
    }

    #[test]
    fn focus_graph_tab_indices_are_dense_and_unique() {
        for &screen in ALL_SCREEN_IDS {
            let graph = FocusGraph::for_screen(screen, Rect::new(0, 0, 100, 32));
            let mut tab_indices: Vec<usize> =
                graph.nodes().iter().map(|node| node.tab_index).collect();
            tab_indices.sort_unstable();
            let expected: Vec<usize> = (0..graph.nodes().len()).collect();
            assert_eq!(
                tab_indices, expected,
                "screen {screen:?} should expose contiguous tab order"
            );
        }
    }

    #[test]
    fn focus_graph_rects_are_inside_area() {
        let area = Rect::new(3, 7, 140, 44);
        for &screen in ALL_SCREEN_IDS {
            let graph = FocusGraph::for_screen(screen, area);
            for node in graph.nodes() {
                let node_right = u32::from(node.rect.x) + u32::from(node.rect.width);
                let node_bottom = u32::from(node.rect.y) + u32::from(node.rect.height);
                let area_right = u32::from(area.x) + u32::from(area.width.max(1));
                let area_bottom = u32::from(area.y) + u32::from(area.height.max(1));

                assert!(node.rect.x >= area.x);
                assert!(node.rect.y >= area.y);
                assert!(node.rect.width >= 1);
                assert!(node.rect.height >= 1);
                assert!(
                    node_right <= area_right,
                    "screen {screen:?} node {} exceeds right bound",
                    node.id
                );
                assert!(
                    node_bottom <= area_bottom,
                    "screen {screen:?} node {} exceeds bottom bound",
                    node.id
                );
            }
        }
    }
}
