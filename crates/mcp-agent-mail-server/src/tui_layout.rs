//! Docking layout model for `AgentMailTUI` panes.
//!
//! Provides a [`DockLayout`] that splits a rectangular area into a
//! *primary* region and a *docked* panel (inspector, detail, etc.)
//! with configurable position and ratio.
//!
//! # Key features
//! - Four dock positions: bottom, top, left, right
//! - Adjustable split ratio (clamped 0.2 – 0.8)
//! - Toggle dock visibility without losing ratio/position
//! - Serializable for persistence (br-10wc.8.3)

use ftui::layout::Rect;
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// DockPosition
// ──────────────────────────────────────────────────────────────────────

/// Where the docked panel is placed relative to the primary content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockPosition {
    Bottom,
    Top,
    Left,
    Right,
}

impl DockPosition {
    /// Cycle to the next position: Bottom → Right → Top → Left → Bottom.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Bottom => Self::Right,
            Self::Right => Self::Top,
            Self::Top => Self::Left,
            Self::Left => Self::Bottom,
        }
    }

    /// Cycle to the previous position.
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::Bottom => Self::Left,
            Self::Left => Self::Top,
            Self::Top => Self::Right,
            Self::Right => Self::Bottom,
        }
    }

    /// Whether this is a horizontal split (top/bottom) or vertical (left/right).
    #[must_use]
    pub const fn is_horizontal(self) -> bool {
        matches!(self, Self::Top | Self::Bottom)
    }

    /// Short label for status line display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bottom => "Bottom",
            Self::Top => "Top",
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// DockLayout
// ──────────────────────────────────────────────────────────────────────

/// Minimum ratio for either the primary or dock pane (prevents collapse).
const MIN_RATIO: f32 = 0.2;
/// Maximum ratio for the dock pane.
const MAX_RATIO: f32 = 0.8;
/// Step size for ratio adjustment.
const RATIO_STEP: f32 = 0.05;
/// Border hit-test tolerance in cells.
const BORDER_HIT_TOLERANCE: u16 = 1;

/// Layout configuration for a docked panel.
///
/// The `ratio` controls how much of the available space the **dock pane**
/// occupies (0.2 = small dock, 0.8 = large dock).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DockLayout {
    /// Where the dock is placed.
    pub position: DockPosition,
    /// Fraction of the total area given to the dock (0.2 – 0.8).
    pub ratio: f32,
    /// Whether the dock pane is visible.
    pub visible: bool,
}

impl DockLayout {
    /// Create a new layout with the given position and ratio.
    #[must_use]
    pub const fn new(position: DockPosition, ratio: f32) -> Self {
        let r = if ratio < MIN_RATIO {
            MIN_RATIO
        } else if ratio > MAX_RATIO {
            MAX_RATIO
        } else {
            ratio
        };
        Self {
            position,
            ratio: r,
            visible: true,
        }
    }

    /// Builder: set visibility.
    #[must_use]
    pub const fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Default: dock on the right, 40% ratio.
    #[must_use]
    pub const fn right_40() -> Self {
        Self {
            position: DockPosition::Right,
            ratio: 0.4,
            visible: true,
        }
    }

    /// Default: dock on the bottom, 30% ratio.
    #[must_use]
    pub const fn bottom_30() -> Self {
        Self {
            position: DockPosition::Bottom,
            ratio: 0.3,
            visible: true,
        }
    }

    /// Toggle the dock visibility.
    pub const fn toggle_visible(&mut self) {
        self.visible = !self.visible;
    }

    /// Cycle the dock position to the next value.
    pub const fn cycle_position(&mut self) {
        self.position = self.position.next();
    }

    /// Cycle the dock position backwards.
    pub const fn cycle_position_prev(&mut self) {
        self.position = self.position.prev();
    }

    /// Increase the dock ratio by one step.
    pub fn grow_dock(&mut self) {
        self.ratio = (self.ratio + RATIO_STEP).min(MAX_RATIO);
    }

    /// Decrease the dock ratio by one step.
    pub fn shrink_dock(&mut self) {
        self.ratio = (self.ratio - RATIO_STEP).max(MIN_RATIO);
    }

    /// Set the ratio directly (clamped).
    pub const fn set_ratio(&mut self, ratio: f32) {
        if ratio < MIN_RATIO {
            self.ratio = MIN_RATIO;
        } else if ratio > MAX_RATIO {
            self.ratio = MAX_RATIO;
        } else {
            self.ratio = ratio;
        }
    }

    /// Return the current ratio as an integer percentage (e.g. 40 for 0.4).
    #[must_use]
    pub fn ratio_percent(&self) -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let p = (self.ratio * 100.0).round() as u8;
        p
    }

    /// Short label describing the current dock state, e.g. "Right 40%".
    #[must_use]
    pub fn state_label(&self) -> String {
        format!("{} {}%", self.position.label(), self.ratio_percent())
    }

    /// Test whether a mouse coordinate (x, y) is on the dock border
    /// for the given area. Returns `true` if the coordinate is within
    /// `BORDER_HIT_TOLERANCE` cells of the split boundary.
    #[must_use]
    pub fn hit_test_border(&self, area: Rect, x: u16, y: u16) -> bool {
        if !self.visible {
            return false;
        }
        let split = self.split(area);
        let Some(dock) = split.dock else {
            return false;
        };
        match self.position {
            DockPosition::Bottom => {
                // Border is the horizontal line between primary bottom and dock top.
                let border_y = dock.y;
                y.abs_diff(border_y) <= BORDER_HIT_TOLERANCE
                    && x >= area.x
                    && x < area.x + area.width
            }
            DockPosition::Top => {
                let border_y = dock.y + dock.height;
                y.abs_diff(border_y) <= BORDER_HIT_TOLERANCE
                    && x >= area.x
                    && x < area.x + area.width
            }
            DockPosition::Right => {
                let border_x = dock.x;
                x.abs_diff(border_x) <= BORDER_HIT_TOLERANCE
                    && y >= area.y
                    && y < area.y + area.height
            }
            DockPosition::Left => {
                let border_x = dock.x + dock.width;
                x.abs_diff(border_x) <= BORDER_HIT_TOLERANCE
                    && y >= area.y
                    && y < area.y + area.height
            }
        }
    }

    /// Adjust the ratio based on a mouse drag position within the given area.
    ///
    /// For horizontal splits (top/bottom) uses the y-coordinate; for
    /// vertical splits (left/right) uses the x-coordinate.
    pub fn drag_to(&mut self, area: Rect, x: u16, y: u16) {
        let new_ratio = match self.position {
            DockPosition::Bottom => {
                // Dock is at the bottom: ratio = dock_height / total_height
                let total = f32::from(area.height);
                if total < 1.0 {
                    return;
                }
                let dock_h = f32::from((area.y + area.height).saturating_sub(y));
                dock_h / total
            }
            DockPosition::Top => {
                // Dock is at the top: ratio = dock_height / total_height
                let total = f32::from(area.height);
                if total < 1.0 {
                    return;
                }
                let dock_h = f32::from(y.saturating_sub(area.y));
                dock_h / total
            }
            DockPosition::Right => {
                // Dock is on the right: ratio = dock_width / total_width
                let total = f32::from(area.width);
                if total < 1.0 {
                    return;
                }
                let dock_w = f32::from((area.x + area.width).saturating_sub(x));
                dock_w / total
            }
            DockPosition::Left => {
                // Dock is on the left: ratio = dock_width / total_width
                let total = f32::from(area.width);
                if total < 1.0 {
                    return;
                }
                let dock_w = f32::from(x.saturating_sub(area.x));
                dock_w / total
            }
        };
        self.set_ratio(new_ratio);
    }

    /// Set the ratio to a named preset.
    pub const fn apply_preset(&mut self, preset: DockPreset) {
        self.set_ratio(preset.ratio());
    }

    /// Split the given area into (primary, dock) rects.
    ///
    /// If the dock is not visible, returns `(area, None)`.
    /// If the area is too small for the split, returns the full area as primary.
    #[must_use]
    pub fn split(&self, area: Rect) -> DockSplit {
        if !self.visible {
            return DockSplit {
                primary: area,
                dock: None,
            };
        }

        // Minimum dimensions for a useful dock.
        let min_dim = 4_u16;

        match self.position {
            DockPosition::Bottom => {
                let dock_h = dock_size(area.height, self.ratio, min_dim);
                if dock_h == 0 {
                    return DockSplit::primary_only(area);
                }
                let primary_h = area.height - dock_h;
                DockSplit {
                    primary: Rect::new(area.x, area.y, area.width, primary_h),
                    dock: Some(Rect::new(area.x, area.y + primary_h, area.width, dock_h)),
                }
            }
            DockPosition::Top => {
                let dock_h = dock_size(area.height, self.ratio, min_dim);
                if dock_h == 0 {
                    return DockSplit::primary_only(area);
                }
                let primary_h = area.height - dock_h;
                DockSplit {
                    primary: Rect::new(area.x, area.y + dock_h, area.width, primary_h),
                    dock: Some(Rect::new(area.x, area.y, area.width, dock_h)),
                }
            }
            DockPosition::Right => {
                let dock_w = dock_size(area.width, self.ratio, min_dim);
                if dock_w == 0 {
                    return DockSplit::primary_only(area);
                }
                let primary_w = area.width - dock_w;
                DockSplit {
                    primary: Rect::new(area.x, area.y, primary_w, area.height),
                    dock: Some(Rect::new(area.x + primary_w, area.y, dock_w, area.height)),
                }
            }
            DockPosition::Left => {
                let dock_w = dock_size(area.width, self.ratio, min_dim);
                if dock_w == 0 {
                    return DockSplit::primary_only(area);
                }
                let primary_w = area.width - dock_w;
                DockSplit {
                    primary: Rect::new(area.x + dock_w, area.y, primary_w, area.height),
                    dock: Some(Rect::new(area.x, area.y, dock_w, area.height)),
                }
            }
        }
    }
}

impl Default for DockLayout {
    fn default() -> Self {
        Self::right_40()
    }
}

// ──────────────────────────────────────────────────────────────────────
// DockSplit — result of splitting
// ──────────────────────────────────────────────────────────────────────

/// The result of splitting an area with a dock layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockSplit {
    /// The primary content area.
    pub primary: Rect,
    /// The dock pane area (None if hidden or area too small).
    pub dock: Option<Rect>,
}

impl DockSplit {
    const fn primary_only(area: Rect) -> Self {
        Self {
            primary: area,
            dock: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// DockPreset — named ratio presets
// ──────────────────────────────────────────────────────────────────────

/// Named ratio presets for quick layout switching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockPreset {
    /// Small dock (20%).
    Compact,
    /// One-third dock (33%).
    Third,
    /// Balanced 40% (default).
    Balanced,
    /// Even 50/50 split.
    Half,
    /// Large dock (60%).
    Wide,
}

impl DockPreset {
    /// The ratio value for this preset.
    #[must_use]
    pub const fn ratio(self) -> f32 {
        match self {
            Self::Compact => 0.20,
            Self::Third => 0.33,
            Self::Balanced => 0.40,
            Self::Half => 0.50,
            Self::Wide => 0.60,
        }
    }

    /// Short display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "20%",
            Self::Third => "33%",
            Self::Balanced => "40%",
            Self::Half => "50%",
            Self::Wide => "60%",
        }
    }

    /// Cycle to the next preset.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Compact => Self::Third,
            Self::Third => Self::Balanced,
            Self::Balanced => Self::Half,
            Self::Half => Self::Wide,
            Self::Wide => Self::Compact,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────

/// Compute dock dimension in pixels, enforcing minimum for both sides.
fn dock_size(total: u16, ratio: f32, min_dim: u16) -> u16 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = (f32::from(total) * ratio).round() as u16;
    let dock = raw.max(min_dim);
    let primary = total.saturating_sub(dock);
    if primary < min_dim || dock > total {
        0 // Can't fit both panes.
    } else {
        dock
    }
}

// ══════════════════════════════════════════════════════════════════════
// Reactive Panel Layout Engine
// ══════════════════════════════════════════════════════════════════════
//
// Provides terminal-size-aware layout that adapts panel visibility,
// density, and proportions from tiny (≤40 cols) to ultrawide (180+).

// ──────────────────────────────────────────────────────────────────────
// TerminalClass — breakpoint classification
// ──────────────────────────────────────────────────────────────────────

/// Terminal size classification for breakpoint-based layout decisions.
///
/// Screens use this to decide how many panels to show, what detail
/// level to render, and whether to collapse sidebars or inspectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalClass {
    /// < 40 cols or < 10 rows. Single-panel, minimal chrome.
    Tiny,
    /// 40–79 cols. Abbreviated labels, no side panels.
    Compact,
    /// 80–119 cols. Standard single-column layout.
    Normal,
    /// 120–179 cols. Two-column layouts with inspector panels.
    Wide,
    /// 180+ cols. Three-column layouts, full detail everywhere.
    UltraWide,
}

/// Width breakpoints (cols) for terminal classification.
const BREAKPOINT_TINY: u16 = 40;
const BREAKPOINT_COMPACT: u16 = 80;
const BREAKPOINT_NORMAL: u16 = 120;
const BREAKPOINT_WIDE: u16 = 180;

/// Height threshold below which the terminal is classified as Tiny
/// regardless of width.
const BREAKPOINT_MIN_HEIGHT: u16 = 10;

impl TerminalClass {
    /// Classify a terminal area into a breakpoint.
    #[must_use]
    pub const fn classify(width: u16, height: u16) -> Self {
        if width < BREAKPOINT_TINY || height < BREAKPOINT_MIN_HEIGHT {
            Self::Tiny
        } else if width < BREAKPOINT_COMPACT {
            Self::Compact
        } else if width < BREAKPOINT_NORMAL {
            Self::Normal
        } else if width < BREAKPOINT_WIDE {
            Self::Wide
        } else {
            Self::UltraWide
        }
    }

    /// Classify from a `Rect`.
    #[must_use]
    pub const fn from_rect(area: Rect) -> Self {
        Self::classify(area.width, area.height)
    }

    /// Maximum number of simultaneous vertical splits recommended.
    #[must_use]
    pub const fn max_columns(self) -> u8 {
        match self {
            Self::Tiny | Self::Compact => 1,
            Self::Normal | Self::Wide => 2,
            Self::UltraWide => 3,
        }
    }

    /// Whether side panels (inspector, sidebar) should be visible.
    #[must_use]
    pub const fn supports_side_panel(self) -> bool {
        matches!(self, Self::Normal | Self::Wide | Self::UltraWide)
    }

    /// Whether an inspector/detail dock is recommended.
    #[must_use]
    pub const fn supports_inspector(self) -> bool {
        matches!(self, Self::Wide | Self::UltraWide)
    }

    /// Short label for display in status bar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Wide => "wide",
            Self::UltraWide => "ultrawide",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// DensityHint — rendering detail level
// ──────────────────────────────────────────────────────────────────────

/// Hint for screens about how much detail to render.
///
/// Screens should respect this when choosing between abbreviated and
/// full-form text, column counts, decoration density, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DensityHint {
    /// Show only critical information. Omit decorations, use single-char
    /// labels, skip optional columns.
    Minimal,
    /// Abbreviated labels, fewer columns, compact spacing.
    Compact,
    /// Standard full-detail layout.
    Normal,
    /// Maximum detail: multi-column where possible, full timestamps,
    /// sparklines, secondary metrics.
    Detailed,
}

impl DensityHint {
    /// Derive a density hint from terminal class.
    #[must_use]
    pub const fn from_terminal_class(tc: TerminalClass) -> Self {
        match tc {
            TerminalClass::Tiny => Self::Minimal,
            TerminalClass::Compact => Self::Compact,
            TerminalClass::Normal => Self::Normal,
            TerminalClass::Wide | TerminalClass::UltraWide => Self::Detailed,
        }
    }

    /// Maximum number of table columns to show.
    #[must_use]
    pub const fn max_table_columns(self) -> usize {
        match self {
            Self::Minimal => 2,
            Self::Compact => 4,
            Self::Normal => 6,
            Self::Detailed => 10,
        }
    }

    /// Whether timestamps should use full ISO-8601 format.
    #[must_use]
    pub const fn full_timestamps(self) -> bool {
        matches!(self, Self::Normal | Self::Detailed)
    }

    /// Whether to show sparklines and inline charts.
    #[must_use]
    pub const fn show_sparklines(self) -> bool {
        matches!(self, Self::Normal | Self::Detailed)
    }
}

// ──────────────────────────────────────────────────────────────────────
// PanelSlot — named panel positions
// ──────────────────────────────────────────────────────────────────────

/// Named panel slots within a screen layout.
///
/// Each slot has a semantic role. The reactive engine decides which
/// slots are visible at each breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelSlot {
    /// Primary content area (always visible).
    Primary,
    /// Detail/inspector panel (right or bottom dock).
    Inspector,
    /// Left sidebar (navigation, tree view).
    Sidebar,
    /// Bottom panel (logs, preview, supplementary info).
    Footer,
}

// ──────────────────────────────────────────────────────────────────────
// PanelVisibility — per-breakpoint panel state
// ──────────────────────────────────────────────────────────────────────

/// How a panel appears at a given breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelVisibility {
    /// Fully visible with its allocated space.
    Visible,
    /// Collapsed to a minimal indicator (e.g., 1-char gutter).
    Collapsed,
    /// Completely hidden; space reclaimed by neighbors.
    Hidden,
}

// ──────────────────────────────────────────────────────────────────────
// PanelConstraint — size constraints for a single panel
// ──────────────────────────────────────────────────────────────────────

/// Size constraints for a panel at a specific breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PanelConstraint {
    /// Visibility at this breakpoint.
    pub visibility: PanelVisibility,
    /// Fraction of available width (for vertical splits) or height
    /// (for horizontal splits). Range 0.0–1.0.
    pub ratio: f32,
    /// Minimum dimension in cells.
    pub min_cells: u16,
    /// Maximum dimension in cells (0 = unlimited).
    pub max_cells: u16,
}

impl PanelConstraint {
    /// A visible panel with the given ratio and min size.
    #[must_use]
    pub const fn visible(ratio: f32, min_cells: u16) -> Self {
        Self {
            visibility: PanelVisibility::Visible,
            ratio,
            min_cells,
            max_cells: 0,
        }
    }

    /// A hidden panel.
    pub const HIDDEN: Self = Self {
        visibility: PanelVisibility::Hidden,
        ratio: 0.0,
        min_cells: 0,
        max_cells: 0,
    };

    /// A collapsed panel (1-cell gutter).
    pub const COLLAPSED: Self = Self {
        visibility: PanelVisibility::Collapsed,
        ratio: 0.0,
        min_cells: 1,
        max_cells: 1,
    };

    /// Builder: set max cells.
    #[must_use]
    pub const fn with_max(mut self, max: u16) -> Self {
        self.max_cells = max;
        self
    }
}

// ──────────────────────────────────────────────────────────────────────
// PanelPolicy — per-breakpoint rules for a named panel
// ──────────────────────────────────────────────────────────────────────

/// Layout policy for a single panel across all terminal breakpoints.
///
/// Priority determines panel ordering: lower numbers = higher priority.
/// The primary panel should always be priority 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelPolicy {
    /// Which slot this policy controls.
    pub slot: PanelSlot,
    /// Priority for space allocation (0 = highest).
    pub priority: u8,
    /// Axis along which this panel splits from its parent.
    pub axis: SplitAxis,
    /// Constraint per terminal class. Index by `TerminalClass` ordinal.
    constraints: [PanelConstraint; 5],
}

/// Axis for panel splitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    /// Panel occupies a vertical slice (left-right split).
    Vertical,
    /// Panel occupies a horizontal slice (top-bottom split).
    Horizontal,
}

impl PanelPolicy {
    /// Create a new policy with uniform constraints across all breakpoints.
    #[must_use]
    pub const fn new(
        slot: PanelSlot,
        priority: u8,
        axis: SplitAxis,
        default: PanelConstraint,
    ) -> Self {
        Self {
            slot,
            priority,
            axis,
            constraints: [default, default, default, default, default],
        }
    }

    /// Builder: set constraint for a specific terminal class.
    #[must_use]
    pub const fn at(mut self, tc: TerminalClass, constraint: PanelConstraint) -> Self {
        self.constraints[tc as usize] = constraint;
        self
    }

    /// Get the constraint for a given terminal class.
    #[must_use]
    pub const fn constraint_for(&self, tc: TerminalClass) -> &PanelConstraint {
        &self.constraints[tc as usize]
    }
}

// ──────────────────────────────────────────────────────────────────────
// LayoutComposition — computed panel rects
// ──────────────────────────────────────────────────────────────────────

/// The computed layout result: a set of named panel rects.
#[derive(Debug, Clone)]
pub struct LayoutComposition {
    /// Terminal class at the time of computation.
    pub terminal_class: TerminalClass,
    /// Density hint derived from terminal class.
    pub density: DensityHint,
    /// Whether we fell back to single-panel mode.
    pub fallback_active: bool,
    /// Computed panels with their rects and visibility.
    pub panels: Vec<ComputedPanel>,
}

/// A single panel's computed position and size.
#[derive(Debug, Clone, Copy)]
pub struct ComputedPanel {
    /// Which slot this panel occupies.
    pub slot: PanelSlot,
    /// The rectangle assigned to this panel.
    pub rect: Rect,
    /// Whether the panel is visible, collapsed, or hidden.
    pub visibility: PanelVisibility,
}

impl LayoutComposition {
    /// Look up a panel by slot. Returns `None` if the slot is hidden.
    #[must_use]
    pub fn panel(&self, slot: PanelSlot) -> Option<&ComputedPanel> {
        self.panels
            .iter()
            .find(|p| p.slot == slot && p.visibility != PanelVisibility::Hidden)
    }

    /// Get the rect for a slot, if visible or collapsed.
    #[must_use]
    pub fn rect(&self, slot: PanelSlot) -> Option<Rect> {
        self.panel(slot).map(|p| p.rect)
    }

    /// Get the primary panel rect (always present).
    #[must_use]
    pub fn primary(&self) -> Rect {
        self.rect(PanelSlot::Primary)
            .expect("primary panel should always exist")
    }
}

// ──────────────────────────────────────────────────────────────────────
// ReactiveLayout — the layout engine
// ──────────────────────────────────────────────────────────────────────

/// Reactive panel layout engine.
///
/// Given an area and a set of panel policies, computes actual panel
/// positions that adapt to terminal size. Supports deterministic
/// fallback to single-panel mode for tiny terminals.
///
/// # Usage
/// ```ignore
/// let engine = ReactiveLayout::new()
///     .panel(PanelPolicy::new(PanelSlot::Primary, 0, SplitAxis::Vertical,
///         PanelConstraint::visible(1.0, 20)))
///     .panel(PanelPolicy::new(PanelSlot::Inspector, 1, SplitAxis::Vertical,
///         PanelConstraint::HIDDEN)
///         .at(TerminalClass::Wide, PanelConstraint::visible(0.35, 30))
///         .at(TerminalClass::UltraWide, PanelConstraint::visible(0.3, 40)));
///
/// let composition = engine.compute(content_area);
/// let primary_rect = composition.primary();
/// if let Some(inspector) = composition.rect(PanelSlot::Inspector) {
///     // render inspector into inspector rect
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ReactiveLayout {
    policies: Vec<PanelPolicy>,
}

impl ReactiveLayout {
    /// Create a new empty layout engine.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policies: Vec::new(),
        }
    }

    /// Add a panel policy. Returns self for chaining.
    #[must_use]
    pub fn panel(mut self, policy: PanelPolicy) -> Self {
        self.policies.push(policy);
        self
    }

    /// Compute the layout for the given area.
    ///
    /// The algorithm:
    /// 1. Classify terminal size.
    /// 2. Sort panels by priority.
    /// 3. For each panel, check its constraint at the current breakpoint.
    /// 4. Allocate space along the specified axis, respecting min/max.
    /// 5. If the area is too small, fall back to primary-only mode.
    #[must_use]
    pub fn compute(&self, area: Rect) -> LayoutComposition {
        let tc = TerminalClass::from_rect(area);
        let density = DensityHint::from_terminal_class(tc);

        // Fallback: if no policies, return the full area as primary.
        if self.policies.is_empty() {
            return LayoutComposition {
                terminal_class: tc,
                density,
                fallback_active: true,
                panels: vec![ComputedPanel {
                    slot: PanelSlot::Primary,
                    rect: area,
                    visibility: PanelVisibility::Visible,
                }],
            };
        }

        // Sort by priority (lower = higher priority = allocated first).
        let mut sorted: Vec<&PanelPolicy> = self.policies.iter().collect();
        sorted.sort_by_key(|p| p.priority);

        // Separate horizontal and vertical panels.
        let mut v_panels: Vec<(&PanelPolicy, &PanelConstraint)> = Vec::new();
        let mut h_panels: Vec<(&PanelPolicy, &PanelConstraint)> = Vec::new();

        for policy in &sorted {
            let constraint = policy.constraint_for(tc);
            if constraint.visibility == PanelVisibility::Hidden {
                continue;
            }
            match policy.axis {
                SplitAxis::Vertical => v_panels.push((policy, constraint)),
                SplitAxis::Horizontal => h_panels.push((policy, constraint)),
            }
        }

        // Phase 1: Allocate horizontal splits (top-bottom) from the area.
        let (main_area, h_computed) = allocate_axis(area, &h_panels, false);

        // Phase 2: Allocate vertical splits (left-right) within main_area.
        let (_, v_computed) = allocate_axis(main_area, &v_panels, true);

        // Merge results.
        let mut panels: Vec<ComputedPanel> = Vec::new();
        panels.extend(v_computed);
        panels.extend(h_computed);

        // Add hidden panels for completeness.
        for policy in &sorted {
            let constraint = policy.constraint_for(tc);
            if constraint.visibility == PanelVisibility::Hidden {
                panels.push(ComputedPanel {
                    slot: policy.slot,
                    rect: Rect::new(0, 0, 0, 0),
                    visibility: PanelVisibility::Hidden,
                });
            }
        }

        let fallback_active = tc == TerminalClass::Tiny
            || panels
                .iter()
                .filter(|p| p.visibility == PanelVisibility::Visible)
                .count()
                <= 1;

        LayoutComposition {
            terminal_class: tc,
            density,
            fallback_active,
            panels,
        }
    }

    /// Create a standard two-panel layout (primary + inspector dock).
    ///
    /// The inspector appears on wide/ultrawide terminals, hidden on
    /// smaller sizes.
    #[must_use]
    pub fn standard_with_inspector() -> Self {
        Self::new()
            .panel(
                PanelPolicy::new(
                    PanelSlot::Primary,
                    0,
                    SplitAxis::Vertical,
                    PanelConstraint::visible(1.0, 20),
                )
                .at(TerminalClass::Wide, PanelConstraint::visible(0.65, 40))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.6, 50)),
            )
            .panel(
                PanelPolicy::new(
                    PanelSlot::Inspector,
                    1,
                    SplitAxis::Vertical,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Wide, PanelConstraint::visible(0.35, 30))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.4, 40)),
            )
    }

    /// Create a three-panel layout (sidebar + primary + inspector).
    ///
    /// Sidebar appears on wide+; inspector on ultrawide only.
    #[must_use]
    pub fn standard_three_panel() -> Self {
        Self::new()
            .panel(
                PanelPolicy::new(
                    PanelSlot::Sidebar,
                    1,
                    SplitAxis::Vertical,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Normal, PanelConstraint::COLLAPSED)
                .at(
                    TerminalClass::Wide,
                    PanelConstraint::visible(0.2, 24).with_max(40),
                )
                .at(
                    TerminalClass::UltraWide,
                    PanelConstraint::visible(0.15, 28).with_max(50),
                ),
            )
            .panel(PanelPolicy::new(
                PanelSlot::Primary,
                0,
                SplitAxis::Vertical,
                PanelConstraint::visible(1.0, 20),
            ))
            .panel(
                PanelPolicy::new(
                    PanelSlot::Inspector,
                    2,
                    SplitAxis::Vertical,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.3, 40)),
            )
    }

    /// Create a layout with a primary area and a bottom footer panel.
    #[must_use]
    pub fn standard_with_footer() -> Self {
        Self::new()
            .panel(
                PanelPolicy::new(
                    PanelSlot::Primary,
                    0,
                    SplitAxis::Horizontal,
                    PanelConstraint::visible(1.0, 6),
                )
                .at(TerminalClass::Normal, PanelConstraint::visible(0.7, 10))
                .at(TerminalClass::Wide, PanelConstraint::visible(0.7, 12))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.7, 14)),
            )
            .panel(
                PanelPolicy::new(
                    PanelSlot::Footer,
                    1,
                    SplitAxis::Horizontal,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Normal, PanelConstraint::visible(0.3, 5))
                .at(TerminalClass::Wide, PanelConstraint::visible(0.3, 6))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.3, 8)),
            )
    }
}

impl Default for ReactiveLayout {
    fn default() -> Self {
        Self::standard_with_inspector()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Allocation algorithm
// ──────────────────────────────────────────────────────────────────────

/// Allocate panels along one axis within the given area.
///
/// `is_vertical` controls whether we split the width (vertical splits,
/// left-to-right) or the height (horizontal splits, top-to-bottom).
///
/// Returns `(remaining_area, computed_panels)` where `remaining_area`
/// is the rect given to the first/primary panel (largest allocation).
fn allocate_axis(
    area: Rect,
    panels: &[(&PanelPolicy, &PanelConstraint)],
    is_vertical: bool,
) -> (Rect, Vec<ComputedPanel>) {
    let total = if is_vertical { area.width } else { area.height };
    let mut computed = Vec::with_capacity(panels.len());

    if panels.is_empty() {
        return (area, computed);
    }

    // If only one panel, give it the entire area.
    if panels.len() == 1 {
        let (policy, constraint) = &panels[0];
        computed.push(ComputedPanel {
            slot: policy.slot,
            rect: area,
            visibility: constraint.visibility,
        });
        return (area, computed);
    }

    // Two-pass allocation: non-primary panels first, primary gets the remainder.
    let mut allocations: Vec<(usize, u16, PanelVisibility)> = Vec::new();
    let mut primary_idx: Option<usize> = None;

    // Pass 1: Compute non-primary panel sizes.
    let mut non_primary_used: u16 = 0;
    for (i, (policy, constraint)) in panels.iter().enumerate() {
        if policy.slot == PanelSlot::Primary {
            primary_idx = Some(i);
            allocations.push((i, 0, constraint.visibility)); // placeholder
            continue;
        }

        let cells = match constraint.visibility {
            PanelVisibility::Visible => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let raw = (f32::from(total) * constraint.ratio).round() as u16;
                let clamped = raw.max(constraint.min_cells);
                if constraint.max_cells > 0 {
                    clamped.min(constraint.max_cells)
                } else {
                    clamped
                }
            }
            PanelVisibility::Collapsed => constraint.min_cells.max(1),
            PanelVisibility::Hidden => 0,
        };

        non_primary_used = non_primary_used.saturating_add(cells);
        allocations.push((i, cells, constraint.visibility));
    }

    // Pass 2: Give primary the remainder (at least its min_cells).
    if let Some(pi) = primary_idx {
        let primary_constraint = &panels[pi].1;
        let remaining = total.saturating_sub(non_primary_used);
        let cells = match primary_constraint.visibility {
            PanelVisibility::Visible => remaining.max(primary_constraint.min_cells),
            PanelVisibility::Collapsed => primary_constraint.min_cells.max(1),
            PanelVisibility::Hidden => 0,
        };
        allocations[pi].1 = cells;
    }

    // If over-allocated (non-primary panels + primary min exceed total),
    // shrink non-primary panels proportionally.
    let total_allocated: u16 = allocations.iter().map(|(_, c, _)| *c).sum();
    if total_allocated > total {
        let primary = primary_idx.unwrap_or(0);
        let overflow = total_allocated - total;
        let non_primary_total: u16 = allocations
            .iter()
            .filter(|(i, _, _)| *i != primary)
            .map(|(_, c, _)| *c)
            .sum();

        if non_primary_total > 0 {
            let mut remaining_cut = overflow;
            for (i, cells, vis) in &mut allocations {
                if *i == primary || *vis == PanelVisibility::Hidden {
                    continue;
                }
                let panel_constraint = &panels[*i].1;
                let cut =
                    (u32::from(*cells) * u32::from(overflow) / u32::from(non_primary_total)) as u16;
                let cut = cut.min(remaining_cut);
                let new_cells = cells.saturating_sub(cut);
                *cells = new_cells.max(panel_constraint.min_cells);
                remaining_cut = remaining_cut.saturating_sub(cut);
            }
        }

        // If still over, hide lowest-priority non-primary panels.
        let mut running_total: u16 = allocations.iter().map(|(_, c, _)| *c).sum();
        if running_total > total {
            for (i, cells, vis) in allocations.iter_mut().rev() {
                if *i == primary_idx.unwrap_or(0) {
                    continue;
                }
                running_total = running_total.saturating_sub(*cells);
                *cells = 0;
                *vis = PanelVisibility::Hidden;
                if running_total <= total {
                    break;
                }
            }
        }

        // Recalculate primary after shrinking to fill any space freed.
        if let Some(pi) = primary_idx {
            let non_primary: u16 = allocations
                .iter()
                .filter(|(i, _, _)| *i != pi)
                .map(|(_, c, _)| *c)
                .sum();
            let primary_constraint = &panels[pi].1;
            allocations[pi].1 = total
                .saturating_sub(non_primary)
                .max(primary_constraint.min_cells);
        }
    }

    // Give any remaining space to the primary panel.
    let used: u16 = allocations.iter().map(|(_, c, _)| *c).sum();
    if used < total {
        if let Some(primary) = primary_idx {
            allocations[primary].1 += total - used;
        } else if !allocations.is_empty() {
            allocations[0].1 += total - used;
        }
    }

    // Convert allocations to rects.
    let mut offset = if is_vertical { area.x } else { area.y };
    let mut primary_rect = area;

    for (i, (_, cells, vis)) in allocations.iter().enumerate() {
        let rect = if *cells == 0 {
            Rect::new(0, 0, 0, 0)
        } else if is_vertical {
            let r = Rect::new(offset, area.y, *cells, area.height);
            offset += cells;
            r
        } else {
            let r = Rect::new(area.x, offset, area.width, *cells);
            offset += cells;
            r
        };

        if Some(i) == primary_idx || (primary_idx.is_none() && i == 0) {
            primary_rect = rect;
        }

        computed.push(ComputedPanel {
            slot: panels[i].0.slot,
            rect,
            visibility: *vis,
        });
    }

    (primary_rect, computed)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    // ── DockPosition ─────────────────────────────────────────────────

    #[test]
    fn dock_position_next_cycles() {
        assert_eq!(DockPosition::Bottom.next(), DockPosition::Right);
        assert_eq!(DockPosition::Right.next(), DockPosition::Top);
        assert_eq!(DockPosition::Top.next(), DockPosition::Left);
        assert_eq!(DockPosition::Left.next(), DockPosition::Bottom);
    }

    #[test]
    fn dock_position_prev_cycles() {
        assert_eq!(DockPosition::Bottom.prev(), DockPosition::Left);
        assert_eq!(DockPosition::Left.prev(), DockPosition::Top);
        assert_eq!(DockPosition::Top.prev(), DockPosition::Right);
        assert_eq!(DockPosition::Right.prev(), DockPosition::Bottom);
    }

    #[test]
    fn dock_position_next_prev_roundtrip() {
        for pos in [
            DockPosition::Bottom,
            DockPosition::Top,
            DockPosition::Left,
            DockPosition::Right,
        ] {
            assert_eq!(pos.next().prev(), pos);
            assert_eq!(pos.prev().next(), pos);
        }
    }

    #[test]
    fn dock_position_is_horizontal() {
        assert!(DockPosition::Top.is_horizontal());
        assert!(DockPosition::Bottom.is_horizontal());
        assert!(!DockPosition::Left.is_horizontal());
        assert!(!DockPosition::Right.is_horizontal());
    }

    #[test]
    fn dock_position_labels() {
        assert_eq!(DockPosition::Bottom.label(), "Bottom");
        assert_eq!(DockPosition::Top.label(), "Top");
        assert_eq!(DockPosition::Left.label(), "Left");
        assert_eq!(DockPosition::Right.label(), "Right");
    }

    #[test]
    fn dock_position_serde_roundtrip() {
        for pos in [
            DockPosition::Bottom,
            DockPosition::Top,
            DockPosition::Left,
            DockPosition::Right,
        ] {
            let json = serde_json::to_string(&pos).unwrap();
            let round: DockPosition = serde_json::from_str(&json).unwrap();
            assert_eq!(round, pos);
        }
    }

    // ── DockLayout ───────────────────────────────────────────────────

    #[test]
    fn default_is_right_40() {
        let layout = DockLayout::default();
        assert_eq!(layout.position, DockPosition::Right);
        assert!((layout.ratio - 0.4).abs() < f32::EPSILON);
        assert!(layout.visible);
    }

    #[test]
    fn ratio_is_clamped() {
        let layout = DockLayout::new(DockPosition::Bottom, 0.05);
        assert!((layout.ratio - MIN_RATIO).abs() < f32::EPSILON);

        let layout = DockLayout::new(DockPosition::Bottom, 0.95);
        assert!((layout.ratio - MAX_RATIO).abs() < f32::EPSILON);
    }

    #[test]
    fn toggle_visible() {
        let mut layout = DockLayout::default();
        assert!(layout.visible);
        layout.toggle_visible();
        assert!(!layout.visible);
        layout.toggle_visible();
        assert!(layout.visible);
    }

    #[test]
    fn grow_shrink_dock() {
        let mut layout = DockLayout::new(DockPosition::Right, 0.5);
        layout.grow_dock();
        assert!(layout.ratio > 0.5);
        layout.shrink_dock();
        layout.shrink_dock();
        assert!(layout.ratio < 0.5);
    }

    #[test]
    fn grow_clamps_at_max() {
        let mut layout = DockLayout::new(DockPosition::Right, MAX_RATIO);
        layout.grow_dock();
        assert!((layout.ratio - MAX_RATIO).abs() < f32::EPSILON);
    }

    #[test]
    fn shrink_clamps_at_min() {
        let mut layout = DockLayout::new(DockPosition::Right, MIN_RATIO);
        layout.shrink_dock();
        assert!((layout.ratio - MIN_RATIO).abs() < f32::EPSILON);
    }

    #[test]
    fn cycle_position() {
        let mut layout = DockLayout::default();
        assert_eq!(layout.position, DockPosition::Right);
        layout.cycle_position();
        assert_eq!(layout.position, DockPosition::Top);
        layout.cycle_position();
        assert_eq!(layout.position, DockPosition::Left);
        layout.cycle_position();
        assert_eq!(layout.position, DockPosition::Bottom);
        layout.cycle_position();
        assert_eq!(layout.position, DockPosition::Right);
    }

    // ── split() ──────────────────────────────────────────────────────

    #[test]
    fn split_right() {
        let layout = DockLayout::new(DockPosition::Right, 0.4);
        let split = layout.split(area(100, 40));
        assert!(split.dock.is_some());
        let dock = split.dock.unwrap();
        assert_eq!(split.primary.x, 0);
        assert_eq!(split.primary.width + dock.width, 100);
        assert_eq!(dock.x, split.primary.width);
        assert_eq!(dock.height, 40);
    }

    #[test]
    fn split_left() {
        let layout = DockLayout::new(DockPosition::Left, 0.3);
        let split = layout.split(area(100, 40));
        assert!(split.dock.is_some());
        let dock = split.dock.unwrap();
        assert_eq!(dock.x, 0);
        assert_eq!(split.primary.x, dock.width);
        assert_eq!(split.primary.width + dock.width, 100);
    }

    #[test]
    fn split_bottom() {
        let layout = DockLayout::new(DockPosition::Bottom, 0.4);
        let split = layout.split(area(100, 40));
        assert!(split.dock.is_some());
        let dock = split.dock.unwrap();
        assert_eq!(split.primary.y, 0);
        assert_eq!(dock.y, split.primary.height);
        assert_eq!(split.primary.height + dock.height, 40);
        assert_eq!(dock.width, 100);
    }

    #[test]
    fn split_top() {
        let layout = DockLayout::new(DockPosition::Top, 0.3);
        let split = layout.split(area(100, 40));
        assert!(split.dock.is_some());
        let dock = split.dock.unwrap();
        assert_eq!(dock.y, 0);
        assert_eq!(split.primary.y, dock.height);
        assert_eq!(split.primary.height + dock.height, 40);
    }

    #[test]
    fn split_hidden_returns_primary_only() {
        let mut layout = DockLayout::new(DockPosition::Right, 0.4);
        layout.visible = false;
        let split = layout.split(area(100, 40));
        assert!(split.dock.is_none());
        assert_eq!(split.primary, area(100, 40));
    }

    #[test]
    fn split_too_small_returns_primary_only() {
        let layout = DockLayout::new(DockPosition::Right, 0.5);
        // Area only 6 wide — can't fit 2 panes of min 4 each.
        let split = layout.split(area(6, 40));
        assert!(split.dock.is_none());
        assert_eq!(split.primary, area(6, 40));
    }

    #[test]
    fn split_covers_full_area_all_positions() {
        for pos in [
            DockPosition::Bottom,
            DockPosition::Top,
            DockPosition::Left,
            DockPosition::Right,
        ] {
            let layout = DockLayout::new(pos, 0.4);
            let split = layout.split(area(120, 40));
            if let Some(dock) = split.dock {
                if pos.is_horizontal() {
                    assert_eq!(
                        split.primary.height + dock.height,
                        40,
                        "height mismatch for {pos:?}"
                    );
                    assert_eq!(split.primary.width, 120);
                    assert_eq!(dock.width, 120);
                } else {
                    assert_eq!(
                        split.primary.width + dock.width,
                        120,
                        "width mismatch for {pos:?}"
                    );
                    assert_eq!(split.primary.height, 40);
                    assert_eq!(dock.height, 40);
                }
            }
        }
    }

    #[test]
    fn split_preserves_area_origin() {
        let layout = DockLayout::new(DockPosition::Right, 0.4);
        let offset_area = Rect::new(10, 5, 100, 40);
        let split = layout.split(offset_area);
        assert!(split.dock.is_some());
        let dock = split.dock.unwrap();
        assert_eq!(split.primary.x, 10);
        assert_eq!(split.primary.y, 5);
        assert_eq!(dock.y, 5);
        assert_eq!(dock.x, 10 + split.primary.width);
    }

    #[test]
    fn serde_roundtrip() {
        let layout = DockLayout::new(DockPosition::Left, 0.35);
        let json = serde_json::to_string(&layout).unwrap();
        let round: DockLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(round.position, DockPosition::Left);
        assert!((round.ratio - 0.35).abs() < f32::EPSILON);
        assert!(round.visible);
    }

    // ── dock_size helper ─────────────────────────────────────────────

    #[test]
    fn dock_size_normal() {
        assert_eq!(dock_size(100, 0.4, 4), 40);
        assert_eq!(dock_size(100, 0.3, 4), 30);
    }

    #[test]
    fn dock_size_enforces_minimum() {
        // Ratio would give 2, but min is 4.
        assert_eq!(dock_size(100, 0.02, 4), 4);
    }

    #[test]
    fn dock_size_returns_zero_when_too_small() {
        // Total 6, dock min 4 → primary would be 2 < 4, not enough.
        assert_eq!(dock_size(6, 0.5, 4), 0);
    }

    // ── ratio_percent ───────────────────────────────────────────────

    #[test]
    fn ratio_percent_values() {
        assert_eq!(
            DockLayout::new(DockPosition::Right, 0.4).ratio_percent(),
            40
        );
        assert_eq!(
            DockLayout::new(DockPosition::Right, 0.33).ratio_percent(),
            33
        );
        assert_eq!(
            DockLayout::new(DockPosition::Right, 0.2).ratio_percent(),
            20
        );
        assert_eq!(
            DockLayout::new(DockPosition::Right, 0.8).ratio_percent(),
            80
        );
    }

    #[test]
    fn state_label_format() {
        let layout = DockLayout::new(DockPosition::Right, 0.4);
        assert_eq!(layout.state_label(), "Right 40%");

        let layout = DockLayout::new(DockPosition::Bottom, 0.33);
        assert_eq!(layout.state_label(), "Bottom 33%");
    }

    // ── hit_test_border ─────────────────────────────────────────────

    #[test]
    fn hit_test_border_right() {
        let layout = DockLayout::new(DockPosition::Right, 0.4);
        let a = area(100, 40);
        let split = layout.split(a);
        let dock = split.dock.unwrap();
        // The border is at dock.x (60).
        assert!(layout.hit_test_border(a, dock.x, 20));
        assert!(layout.hit_test_border(a, dock.x - 1, 20)); // within tolerance
        // Far away from border should not hit.
        assert!(!layout.hit_test_border(a, 10, 20));
        assert!(!layout.hit_test_border(a, 90, 20));
    }

    #[test]
    fn hit_test_border_bottom() {
        let layout = DockLayout::new(DockPosition::Bottom, 0.3);
        let a = area(100, 40);
        let split = layout.split(a);
        let dock = split.dock.unwrap();
        // Border is at dock.y.
        assert!(layout.hit_test_border(a, 50, dock.y));
        assert!(layout.hit_test_border(a, 50, dock.y - 1));
        assert!(!layout.hit_test_border(a, 50, 5));
    }

    #[test]
    fn hit_test_hidden_returns_false() {
        let mut layout = DockLayout::new(DockPosition::Right, 0.4);
        layout.visible = false;
        assert!(!layout.hit_test_border(area(100, 40), 60, 20));
    }

    // ── drag_to ─────────────────────────────────────────────────────

    #[test]
    fn drag_to_right() {
        let mut layout = DockLayout::new(DockPosition::Right, 0.4);
        let a = area(100, 40);
        // Drag the border to x=50 → dock_width = 100-50 = 50 → ratio 0.5
        layout.drag_to(a, 50, 20);
        assert!((layout.ratio - 0.5).abs() < 0.02);
    }

    #[test]
    fn drag_to_bottom() {
        let mut layout = DockLayout::new(DockPosition::Bottom, 0.3);
        let a = area(100, 40);
        // Drag border to y=20 → dock_height = 40-20 = 20 → ratio 0.5
        layout.drag_to(a, 50, 20);
        assert!((layout.ratio - 0.5).abs() < 0.05);
    }

    #[test]
    fn drag_to_left() {
        let mut layout = DockLayout::new(DockPosition::Left, 0.3);
        let a = area(100, 40);
        // Drag border to x=40 → dock_width = 40-0 = 40 → ratio 0.4
        layout.drag_to(a, 40, 20);
        assert!((layout.ratio - 0.4).abs() < 0.02);
    }

    #[test]
    fn drag_to_top() {
        let mut layout = DockLayout::new(DockPosition::Top, 0.3);
        let a = area(100, 40);
        // Drag border to y=16 → dock_height = 16-0 = 16 → ratio 0.4
        layout.drag_to(a, 50, 16);
        assert!((layout.ratio - 0.4).abs() < 0.05);
    }

    #[test]
    fn drag_to_clamps() {
        let mut layout = DockLayout::new(DockPosition::Right, 0.4);
        let a = area(100, 40);
        // Drag far right → almost 0 dock → clamped to MIN_RATIO
        layout.drag_to(a, 95, 20);
        assert!((layout.ratio - MIN_RATIO).abs() < f32::EPSILON);
        // Drag far left → almost all dock → clamped to MAX_RATIO
        layout.drag_to(a, 5, 20);
        assert!((layout.ratio - MAX_RATIO).abs() < f32::EPSILON);
    }

    // ── DockPreset ──────────────────────────────────────────────────

    #[test]
    fn preset_ratios() {
        assert!((DockPreset::Compact.ratio() - 0.20).abs() < f32::EPSILON);
        assert!((DockPreset::Third.ratio() - 0.33).abs() < f32::EPSILON);
        assert!((DockPreset::Balanced.ratio() - 0.40).abs() < f32::EPSILON);
        assert!((DockPreset::Half.ratio() - 0.50).abs() < f32::EPSILON);
        assert!((DockPreset::Wide.ratio() - 0.60).abs() < f32::EPSILON);
    }

    #[test]
    fn preset_labels() {
        assert_eq!(DockPreset::Compact.label(), "20%");
        assert_eq!(DockPreset::Half.label(), "50%");
    }

    #[test]
    fn preset_cycle_round_trips() {
        let mut p = DockPreset::Compact;
        for _ in 0..5 {
            p = p.next();
        }
        assert_eq!(p, DockPreset::Compact);
    }

    #[test]
    fn apply_preset() {
        let mut layout = DockLayout::new(DockPosition::Right, 0.4);
        layout.apply_preset(DockPreset::Half);
        assert!((layout.ratio - 0.5).abs() < f32::EPSILON);
    }

    // ══════════════════════════════════════════════════════════════════
    // Reactive Layout Engine Tests
    // ══════════════════════════════════════════════════════════════════

    // ── TerminalClass ─────────────────────────────────────────────

    #[test]
    fn terminal_class_breakpoints() {
        assert_eq!(TerminalClass::classify(30, 24), TerminalClass::Tiny);
        assert_eq!(TerminalClass::classify(39, 24), TerminalClass::Tiny);
        assert_eq!(TerminalClass::classify(40, 24), TerminalClass::Compact);
        assert_eq!(TerminalClass::classify(79, 24), TerminalClass::Compact);
        assert_eq!(TerminalClass::classify(80, 24), TerminalClass::Normal);
        assert_eq!(TerminalClass::classify(119, 24), TerminalClass::Normal);
        assert_eq!(TerminalClass::classify(120, 24), TerminalClass::Wide);
        assert_eq!(TerminalClass::classify(179, 24), TerminalClass::Wide);
        assert_eq!(TerminalClass::classify(180, 24), TerminalClass::UltraWide);
        assert_eq!(TerminalClass::classify(300, 50), TerminalClass::UltraWide);
    }

    #[test]
    fn terminal_class_tiny_on_short_height() {
        // Even 200 cols wide, if height < 10, it's Tiny.
        assert_eq!(TerminalClass::classify(200, 9), TerminalClass::Tiny);
        assert_eq!(TerminalClass::classify(200, 5), TerminalClass::Tiny);
    }

    #[test]
    fn terminal_class_from_rect() {
        assert_eq!(
            TerminalClass::from_rect(area(100, 30)),
            TerminalClass::Normal
        );
        assert_eq!(TerminalClass::from_rect(area(150, 40)), TerminalClass::Wide);
    }

    #[test]
    fn terminal_class_max_columns() {
        assert_eq!(TerminalClass::Tiny.max_columns(), 1);
        assert_eq!(TerminalClass::Compact.max_columns(), 1);
        assert_eq!(TerminalClass::Normal.max_columns(), 2);
        assert_eq!(TerminalClass::Wide.max_columns(), 2);
        assert_eq!(TerminalClass::UltraWide.max_columns(), 3);
    }

    #[test]
    fn terminal_class_supports_side_panel() {
        assert!(!TerminalClass::Tiny.supports_side_panel());
        assert!(!TerminalClass::Compact.supports_side_panel());
        assert!(TerminalClass::Normal.supports_side_panel());
        assert!(TerminalClass::Wide.supports_side_panel());
        assert!(TerminalClass::UltraWide.supports_side_panel());
    }

    #[test]
    fn terminal_class_supports_inspector() {
        assert!(!TerminalClass::Tiny.supports_inspector());
        assert!(!TerminalClass::Compact.supports_inspector());
        assert!(!TerminalClass::Normal.supports_inspector());
        assert!(TerminalClass::Wide.supports_inspector());
        assert!(TerminalClass::UltraWide.supports_inspector());
    }

    #[test]
    fn terminal_class_labels() {
        assert_eq!(TerminalClass::Tiny.label(), "tiny");
        assert_eq!(TerminalClass::Compact.label(), "compact");
        assert_eq!(TerminalClass::Normal.label(), "normal");
        assert_eq!(TerminalClass::Wide.label(), "wide");
        assert_eq!(TerminalClass::UltraWide.label(), "ultrawide");
    }

    #[test]
    fn terminal_class_ordering() {
        assert!(TerminalClass::Tiny < TerminalClass::Compact);
        assert!(TerminalClass::Compact < TerminalClass::Normal);
        assert!(TerminalClass::Normal < TerminalClass::Wide);
        assert!(TerminalClass::Wide < TerminalClass::UltraWide);
    }

    #[test]
    fn terminal_class_serde_roundtrip() {
        for tc in [
            TerminalClass::Tiny,
            TerminalClass::Compact,
            TerminalClass::Normal,
            TerminalClass::Wide,
            TerminalClass::UltraWide,
        ] {
            let json = serde_json::to_string(&tc).unwrap();
            let round: TerminalClass = serde_json::from_str(&json).unwrap();
            assert_eq!(round, tc);
        }
    }

    // ── DensityHint ───────────────────────────────────────────────

    #[test]
    fn density_from_terminal_class() {
        assert_eq!(
            DensityHint::from_terminal_class(TerminalClass::Tiny),
            DensityHint::Minimal
        );
        assert_eq!(
            DensityHint::from_terminal_class(TerminalClass::Compact),
            DensityHint::Compact
        );
        assert_eq!(
            DensityHint::from_terminal_class(TerminalClass::Normal),
            DensityHint::Normal
        );
        assert_eq!(
            DensityHint::from_terminal_class(TerminalClass::Wide),
            DensityHint::Detailed
        );
        assert_eq!(
            DensityHint::from_terminal_class(TerminalClass::UltraWide),
            DensityHint::Detailed
        );
    }

    #[test]
    fn density_max_table_columns() {
        assert_eq!(DensityHint::Minimal.max_table_columns(), 2);
        assert_eq!(DensityHint::Compact.max_table_columns(), 4);
        assert_eq!(DensityHint::Normal.max_table_columns(), 6);
        assert_eq!(DensityHint::Detailed.max_table_columns(), 10);
    }

    #[test]
    fn density_full_timestamps() {
        assert!(!DensityHint::Minimal.full_timestamps());
        assert!(!DensityHint::Compact.full_timestamps());
        assert!(DensityHint::Normal.full_timestamps());
        assert!(DensityHint::Detailed.full_timestamps());
    }

    #[test]
    fn density_show_sparklines() {
        assert!(!DensityHint::Minimal.show_sparklines());
        assert!(!DensityHint::Compact.show_sparklines());
        assert!(DensityHint::Normal.show_sparklines());
        assert!(DensityHint::Detailed.show_sparklines());
    }

    #[test]
    fn density_ordering() {
        assert!(DensityHint::Minimal < DensityHint::Compact);
        assert!(DensityHint::Compact < DensityHint::Normal);
        assert!(DensityHint::Normal < DensityHint::Detailed);
    }

    // ── PanelConstraint ────────────────────────────────────────────

    #[test]
    fn panel_constraint_visible() {
        let c = PanelConstraint::visible(0.4, 20);
        assert_eq!(c.visibility, PanelVisibility::Visible);
        assert!((c.ratio - 0.4).abs() < f32::EPSILON);
        assert_eq!(c.min_cells, 20);
        assert_eq!(c.max_cells, 0);
    }

    #[test]
    fn panel_constraint_hidden() {
        assert_eq!(PanelConstraint::HIDDEN.visibility, PanelVisibility::Hidden);
        assert_eq!(PanelConstraint::HIDDEN.min_cells, 0);
    }

    #[test]
    fn panel_constraint_collapsed() {
        assert_eq!(
            PanelConstraint::COLLAPSED.visibility,
            PanelVisibility::Collapsed
        );
        assert_eq!(PanelConstraint::COLLAPSED.min_cells, 1);
        assert_eq!(PanelConstraint::COLLAPSED.max_cells, 1);
    }

    #[test]
    fn panel_constraint_with_max() {
        let c = PanelConstraint::visible(0.3, 20).with_max(60);
        assert_eq!(c.max_cells, 60);
    }

    // ── PanelPolicy ────────────────────────────────────────────────

    #[test]
    fn panel_policy_per_breakpoint() {
        let policy = PanelPolicy::new(
            PanelSlot::Inspector,
            1,
            SplitAxis::Vertical,
            PanelConstraint::HIDDEN,
        )
        .at(TerminalClass::Wide, PanelConstraint::visible(0.35, 30))
        .at(TerminalClass::UltraWide, PanelConstraint::visible(0.4, 40));

        assert_eq!(
            policy.constraint_for(TerminalClass::Tiny).visibility,
            PanelVisibility::Hidden
        );
        assert_eq!(
            policy.constraint_for(TerminalClass::Compact).visibility,
            PanelVisibility::Hidden
        );
        assert_eq!(
            policy.constraint_for(TerminalClass::Normal).visibility,
            PanelVisibility::Hidden
        );
        assert_eq!(
            policy.constraint_for(TerminalClass::Wide).visibility,
            PanelVisibility::Visible
        );
        assert_eq!(
            policy.constraint_for(TerminalClass::UltraWide).visibility,
            PanelVisibility::Visible
        );
    }

    // ── ReactiveLayout compute ────────────────────────────────────

    #[test]
    fn reactive_empty_layout_returns_full_area() {
        let engine = ReactiveLayout::new();
        let comp = engine.compute(area(100, 30));
        assert_eq!(comp.primary(), area(100, 30));
        assert!(comp.fallback_active);
    }

    #[test]
    fn reactive_single_panel_fills_area() {
        let engine = ReactiveLayout::new().panel(PanelPolicy::new(
            PanelSlot::Primary,
            0,
            SplitAxis::Vertical,
            PanelConstraint::visible(1.0, 20),
        ));
        let comp = engine.compute(area(100, 30));
        assert_eq!(comp.primary(), area(100, 30));
        assert_eq!(comp.terminal_class, TerminalClass::Normal);
        assert_eq!(comp.density, DensityHint::Normal);
    }

    #[test]
    fn reactive_two_panel_wide_shows_inspector() {
        let engine = ReactiveLayout::standard_with_inspector();
        // Wide terminal (140 cols)
        let comp = engine.compute(area(140, 30));
        assert_eq!(comp.terminal_class, TerminalClass::Wide);

        let primary = comp.rect(PanelSlot::Primary);
        let inspector = comp.rect(PanelSlot::Inspector);
        assert!(primary.is_some());
        assert!(inspector.is_some());

        let p = primary.unwrap();
        let i = inspector.unwrap();
        // Both should have some width
        assert!(p.width > 0);
        assert!(i.width > 0);
        // Together they should cover the full width
        assert_eq!(p.width + i.width, 140);
    }

    #[test]
    fn reactive_two_panel_compact_hides_inspector() {
        let engine = ReactiveLayout::standard_with_inspector();
        // Compact terminal (60 cols)
        let comp = engine.compute(area(60, 24));
        assert_eq!(comp.terminal_class, TerminalClass::Compact);

        let primary = comp.rect(PanelSlot::Primary);
        let inspector = comp.rect(PanelSlot::Inspector);
        assert!(primary.is_some());
        assert!(inspector.is_none()); // Hidden on compact
        assert_eq!(primary.unwrap().width, 60);
    }

    #[test]
    fn reactive_tiny_terminal_fallback() {
        let engine = ReactiveLayout::standard_with_inspector();
        let comp = engine.compute(area(30, 8));
        assert_eq!(comp.terminal_class, TerminalClass::Tiny);
        assert_eq!(comp.density, DensityHint::Minimal);
        assert!(comp.fallback_active);
        // Primary should get all the space
        assert_eq!(comp.primary(), area(30, 8));
    }

    #[test]
    fn reactive_ultrawide_shows_all_panels() {
        let engine = ReactiveLayout::standard_three_panel();
        let comp = engine.compute(area(200, 40));
        assert_eq!(comp.terminal_class, TerminalClass::UltraWide);

        let sidebar = comp.rect(PanelSlot::Sidebar);
        let primary = comp.rect(PanelSlot::Primary);
        let inspector = comp.rect(PanelSlot::Inspector);

        assert!(sidebar.is_some(), "sidebar should be visible on ultrawide");
        assert!(primary.is_some());
        assert!(
            inspector.is_some(),
            "inspector should be visible on ultrawide"
        );

        let s = sidebar.unwrap();
        let p = primary.unwrap();
        let i = inspector.unwrap();

        // All should have width
        assert!(s.width > 0);
        assert!(p.width > 0);
        assert!(i.width > 0);

        // Total width should cover the area
        assert_eq!(s.width + p.width + i.width, 200);
    }

    #[test]
    fn reactive_normal_hides_sidebar_in_three_panel() {
        let engine = ReactiveLayout::standard_three_panel();
        // Normal terminal (100 cols) — sidebar should be collapsed, inspector hidden
        let comp = engine.compute(area(100, 30));
        assert_eq!(comp.terminal_class, TerminalClass::Normal);

        let inspector = comp.rect(PanelSlot::Inspector);
        assert!(inspector.is_none(), "inspector hidden on normal");
    }

    #[test]
    fn reactive_footer_layout_shows_on_normal() {
        let engine = ReactiveLayout::standard_with_footer();
        let comp = engine.compute(area(100, 30));
        assert_eq!(comp.terminal_class, TerminalClass::Normal);

        let primary = comp.rect(PanelSlot::Primary);
        let footer = comp.rect(PanelSlot::Footer);

        assert!(primary.is_some());
        assert!(footer.is_some());

        let p = primary.unwrap();
        let f = footer.unwrap();
        // Horizontal split — heights should sum to total
        assert_eq!(p.height + f.height, 30);
        // Footer should be at the bottom
        assert_eq!(f.y, p.height);
    }

    #[test]
    fn reactive_footer_hidden_on_compact() {
        let engine = ReactiveLayout::standard_with_footer();
        let comp = engine.compute(area(60, 20));
        assert_eq!(comp.terminal_class, TerminalClass::Compact);

        let footer = comp.rect(PanelSlot::Footer);
        assert!(footer.is_none(), "footer hidden on compact");
    }

    #[test]
    fn reactive_density_propagated() {
        let engine = ReactiveLayout::standard_with_inspector();

        let comp_tiny = engine.compute(area(30, 8));
        assert_eq!(comp_tiny.density, DensityHint::Minimal);

        let comp_compact = engine.compute(area(60, 24));
        assert_eq!(comp_compact.density, DensityHint::Compact);

        let comp_normal = engine.compute(area(100, 30));
        assert_eq!(comp_normal.density, DensityHint::Normal);

        let comp_wide = engine.compute(area(150, 40));
        assert_eq!(comp_wide.density, DensityHint::Detailed);

        let comp_ultra = engine.compute(area(200, 50));
        assert_eq!(comp_ultra.density, DensityHint::Detailed);
    }

    #[test]
    fn reactive_composition_panel_lookup() {
        let engine = ReactiveLayout::standard_with_inspector();
        let comp = engine.compute(area(150, 40));

        // panel() should find visible panels
        assert!(comp.panel(PanelSlot::Primary).is_some());
        assert!(comp.panel(PanelSlot::Inspector).is_some());

        // Non-existent slots return None
        assert!(comp.panel(PanelSlot::Sidebar).is_none());
        assert!(comp.panel(PanelSlot::Footer).is_none());
    }

    #[test]
    fn reactive_default_is_standard_with_inspector() {
        let default_engine = ReactiveLayout::default();
        let standard_engine = ReactiveLayout::standard_with_inspector();

        // Both should produce the same result for same input.
        let comp_d = default_engine.compute(area(150, 40));
        let comp_s = standard_engine.compute(area(150, 40));

        assert_eq!(comp_d.terminal_class, comp_s.terminal_class);
        assert_eq!(comp_d.density, comp_s.density);
    }

    #[test]
    fn reactive_panel_max_cells_respected() {
        let engine = ReactiveLayout::new()
            .panel(PanelPolicy::new(
                PanelSlot::Primary,
                0,
                SplitAxis::Vertical,
                PanelConstraint::visible(1.0, 20),
            ))
            .panel(PanelPolicy::new(
                PanelSlot::Sidebar,
                1,
                SplitAxis::Vertical,
                PanelConstraint::visible(0.5, 10).with_max(30),
            ));

        let comp = engine.compute(area(200, 40));
        let sidebar = comp.rect(PanelSlot::Sidebar).unwrap();
        assert!(
            sidebar.width <= 30,
            "sidebar width {} should be <= 30",
            sidebar.width
        );
    }

    #[test]
    fn reactive_preserves_area_origin() {
        let engine = ReactiveLayout::standard_with_inspector();
        let offset = Rect::new(5, 3, 150, 40);
        let comp = engine.compute(offset);

        let p = comp.primary();
        assert_eq!(p.x, 5, "primary x should match area origin");
        assert_eq!(p.y, 3, "primary y should match area origin");
    }

    #[test]
    fn reactive_all_rects_within_bounds() {
        let engine = ReactiveLayout::standard_three_panel();
        for width in [30, 60, 100, 150, 200] {
            for height in [8, 20, 30, 50] {
                let a = area(width, height);
                let comp = engine.compute(a);
                for panel in &comp.panels {
                    if panel.visibility == PanelVisibility::Hidden {
                        continue;
                    }
                    assert!(
                        panel.rect.x + panel.rect.width <= a.width,
                        "panel {:?} exceeds width at {}x{}",
                        panel.slot,
                        width,
                        height
                    );
                    assert!(
                        panel.rect.y + panel.rect.height <= a.height,
                        "panel {:?} exceeds height at {}x{}",
                        panel.slot,
                        width,
                        height
                    );
                }
            }
        }
    }
}
