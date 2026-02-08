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
    /// [`BORDER_HIT_TOLERANCE`] cells of the split boundary.
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
}
