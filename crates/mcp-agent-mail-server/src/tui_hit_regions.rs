//! Canonical hit-region IDs and layer classification for mouse dispatch.
//!
//! Hit IDs are partitioned into non-overlapping ranges by UI layer.  The
//! [`classify_hit`] function maps any [`HitId`] to its [`HitLayer`],
//! enabling priority-based routing (overlay > status > tab > category > pane).
//!
//! Range allocations (100-slot ranges, expandable):
//!
//! | Layer      | Base   | End    | Usage                            |
//! |------------|--------|--------|----------------------------------|
//! | Tab bar    | 1 000  | 1 099  | One ID per screen tab            |
//! | Category   | 2 000  | 2 099  | One ID per [`ScreenCategory`]    |
//! | Pane       | 4 000  | 4 099  | Content area per screen          |
//! | Overlay    | 5 000  | 5 099  | Help, palette, perf HUD, etc.    |
//! | Status bar | 6 000  | 6 099  | Status line toggle buttons       |

use ftui_render::frame::HitId;

use crate::tui_screens::{ALL_SCREEN_IDS, MailScreenId, ScreenCategory};

// ──────────────────────────────────────────────────────────────────────
// Base constants
// ──────────────────────────────────────────────────────────────────────

/// Base hit ID for tab-bar entries.  Tab for screen at display index `i`
/// has `HitId(TAB_HIT_BASE + i)` (0-indexed).
pub const TAB_HIT_BASE: u32 = 1_000;

/// Base hit ID for category tabs (ordered by [`ScreenCategory::ALL`]).
pub const CATEGORY_HIT_BASE: u32 = 2_000;

/// Base hit ID for screen pane content areas.
pub const PANE_HIT_BASE: u32 = 4_000;

/// Base hit ID for overlay elements.
pub const OVERLAY_HIT_BASE: u32 = 5_000;

/// Base hit ID for status bar toggles.
pub const STATUS_HIT_BASE: u32 = 6_000;

// ── Overlay sub-ranges ───────────────────────────────────────────────

/// Close button / dismiss area of the help overlay.
pub const OVERLAY_HELP_CLOSE: u32 = OVERLAY_HIT_BASE;
/// Scrollable content of the help overlay.
pub const OVERLAY_HELP_CONTENT: u32 = OVERLAY_HIT_BASE + 1;
/// Command palette overlay.
pub const OVERLAY_PALETTE: u32 = OVERLAY_HIT_BASE + 10;
/// Performance HUD overlay.
pub const OVERLAY_PERF_HUD: u32 = OVERLAY_HIT_BASE + 20;
/// Action menu overlay.
pub const OVERLAY_ACTION_MENU: u32 = OVERLAY_HIT_BASE + 30;
/// Toast / notification overlay.
pub const OVERLAY_TOAST: u32 = OVERLAY_HIT_BASE + 40;

// ── Status bar sub-ranges ────────────────────────────────────────────

/// Help toggle in the status bar.
pub const STATUS_HELP_TOGGLE: u32 = STATUS_HIT_BASE;
/// Palette toggle in the status bar.
pub const STATUS_PALETTE_TOGGLE: u32 = STATUS_HIT_BASE + 1;
/// Perf HUD toggle in the status bar.
pub const STATUS_PERF_TOGGLE: u32 = STATUS_HIT_BASE + 2;

const STATUS_HELP_ZONE_WIDTH: u16 = 14;
const STATUS_PALETTE_ZONE_WIDTH: u16 = 18;

// ──────────────────────────────────────────────────────────────────────
// HitLayer — layer classification enum
// ──────────────────────────────────────────────────────────────────────

/// Dispatch layer for a hit region.
///
/// Variants are ordered from highest to lowest routing priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitLayer {
    /// Overlay element (help, palette, perf HUD, action menu, toast).
    Overlay(u32),
    /// Status bar toggle button.
    StatusToggle(u32),
    /// Tab bar entry — resolves to a screen.
    Tab(MailScreenId),
    /// Category tab — resolves to a screen category.
    Category(ScreenCategory),
    /// Screen pane content area.
    Pane(MailScreenId),
    /// Unknown hit ID — forward to screen for local handling.
    Unknown,
}

// ──────────────────────────────────────────────────────────────────────
// Classification
// ──────────────────────────────────────────────────────────────────────

/// Classify a hit ID into its dispatch layer.
///
/// Layers are checked in priority order (overlay first, pane last).
#[must_use]
pub fn classify_hit(id: HitId) -> HitLayer {
    let raw = id.id();

    // Overlay (highest priority)
    if (OVERLAY_HIT_BASE..STATUS_HIT_BASE).contains(&raw) {
        return HitLayer::Overlay(raw);
    }

    // Status bar toggles
    if (STATUS_HIT_BASE..STATUS_HIT_BASE + 100).contains(&raw) {
        return HitLayer::StatusToggle(raw);
    }

    // Tab bar
    if let Some(screen) = screen_from_tab_hit(id) {
        return HitLayer::Tab(screen);
    }

    // Category tabs
    if let Some(cat) = category_from_hit(id) {
        return HitLayer::Category(cat);
    }

    // Pane content
    if let Some(screen) = screen_from_pane_hit(id) {
        return HitLayer::Pane(screen);
    }

    HitLayer::Unknown
}

// ──────────────────────────────────────────────────────────────────────
// Conversion helpers: MailScreenId ↔ HitId
// ──────────────────────────────────────────────────────────────────────

/// Create a tab-bar hit ID for a screen.
#[inline]
#[must_use]
pub fn tab_hit_id(screen: MailScreenId) -> HitId {
    #[allow(clippy::cast_possible_truncation)]
    HitId::new(TAB_HIT_BASE + screen.index() as u32)
}

/// Create a pane hit ID for a screen.
#[inline]
#[must_use]
pub fn pane_hit_id(screen: MailScreenId) -> HitId {
    #[allow(clippy::cast_possible_truncation)]
    HitId::new(PANE_HIT_BASE + screen.index() as u32)
}

/// Create a category-tab hit ID.
#[inline]
#[must_use]
pub fn category_hit_id(cat: ScreenCategory) -> HitId {
    let idx = ScreenCategory::ALL
        .iter()
        .position(|&c| c == cat)
        .unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    HitId::new(CATEGORY_HIT_BASE + idx as u32)
}

/// Resolve a tab-bar hit ID back to the screen it belongs to.
#[must_use]
pub fn screen_from_tab_hit(id: HitId) -> Option<MailScreenId> {
    let raw = id.id();
    if raw < TAB_HIT_BASE {
        return None;
    }
    let idx = (raw - TAB_HIT_BASE) as usize;
    ALL_SCREEN_IDS.get(idx).copied()
}

/// Resolve a pane hit ID back to the screen it belongs to.
#[must_use]
pub fn screen_from_pane_hit(id: HitId) -> Option<MailScreenId> {
    let raw = id.id();
    if !(PANE_HIT_BASE..OVERLAY_HIT_BASE).contains(&raw) {
        return None;
    }
    let idx = (raw - PANE_HIT_BASE) as usize;
    ALL_SCREEN_IDS.get(idx).copied()
}

/// Resolve a category-tab hit ID back to its category.
#[must_use]
pub fn category_from_hit(id: HitId) -> Option<ScreenCategory> {
    let raw = id.id();
    if !(CATEGORY_HIT_BASE..PANE_HIT_BASE).contains(&raw) {
        return None;
    }
    let idx = (raw - CATEGORY_HIT_BASE) as usize;
    ScreenCategory::ALL.get(idx).copied()
}

// ──────────────────────────────────────────────────────────────────────
// MouseDispatcher — central mouse event routing
// ──────────────────────────────────────────────────────────────────────

use std::cell::Cell;

use ftui::layout::Rect;
use ftui::{MouseButton, MouseEvent, MouseEventKind};

/// Result of dispatching a mouse event through the shell layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseAction {
    /// Switch to a screen (tab click).
    SwitchScreen(MailScreenId),
    /// Toggle the help overlay.
    ToggleHelp,
    /// Open the command palette.
    OpenPalette,
    /// Mouse event was not consumed by the shell — forward to active screen.
    Forward,
}

/// Per-tab hit region cached from the last `view()` call.
#[derive(Debug, Clone, Copy, Default)]
struct TabHitSlot {
    screen: Option<MailScreenId>,
    x_start: u16,
    x_end: u16,
    y: u16,
}

/// Central mouse dispatcher for shell-level interactions.
///
/// Caches chrome area positions from the last `view()` call and maps
/// mouse coordinates to shell actions (tab switch, help toggle, etc.).
/// Events that don't hit any shell region return [`MouseAction::Forward`]
/// so the active screen can handle them.
pub struct MouseDispatcher {
    /// Tab bar row cached from last `view()`.
    tab_bar_area: Cell<Rect>,
    /// Status line row cached from last `view()`.
    status_line_area: Cell<Rect>,
    /// Per-tab hit regions. Up to `ALL_SCREEN_IDS.len()` entries.
    tab_slots: Vec<Cell<TabHitSlot>>,
    /// Last hover target for coalesced hover events (future use).
    #[allow(dead_code)]
    last_hover_screen: Cell<Option<MailScreenId>>,
}

impl MouseDispatcher {
    /// Create a new dispatcher with empty cached regions.
    #[must_use]
    pub fn new() -> Self {
        let tab_slots = ALL_SCREEN_IDS
            .iter()
            .map(|_| Cell::new(TabHitSlot::default()))
            .collect();
        Self {
            tab_bar_area: Cell::new(Rect::new(0, 0, 0, 0)),
            status_line_area: Cell::new(Rect::new(0, 0, 0, 0)),
            tab_slots,
            last_hover_screen: Cell::new(None),
        }
    }

    /// Update cached chrome areas after a `view()` call.
    pub fn update_chrome_areas(&self, tab_bar: Rect, status_line: Rect) {
        self.tab_bar_area.set(tab_bar);
        self.status_line_area.set(status_line);
    }

    /// Record a tab's hit region during tab-bar rendering.
    ///
    /// `index` is the 0-based position in `ALL_SCREEN_IDS`.
    pub fn record_tab_slot(
        &self,
        index: usize,
        screen: MailScreenId,
        x_start: u16,
        x_end: u16,
        y: u16,
    ) {
        if let Some(slot) = self.tab_slots.get(index) {
            slot.set(TabHitSlot {
                screen: Some(screen),
                x_start,
                x_end,
                y,
            });
        }
    }

    /// Return a tab slot's position as `(x_start, x_end, y)`, if registered.
    pub fn tab_slot(&self, index: usize) -> Option<(u16, u16, u16)> {
        self.tab_slots.get(index).and_then(|cell| {
            let slot = cell.get();
            slot.screen.map(|_| (slot.x_start, slot.x_end, slot.y))
        })
    }

    /// Dispatch a mouse event through the shell priority chain.
    ///
    /// Priority order (highest first):
    /// 1. Tab bar clicks → `SwitchScreen`
    /// 2. Status line clicks:
    ///    - Right palette zone → `OpenPalette`
    ///    - Right-adjacent help zone → `ToggleHelp`
    ///    - Center region → `Forward`
    /// 3. Everything else → `Forward` to active screen
    ///
    /// Only `MouseDown(Left)` triggers actions to prevent double-fire.
    pub fn dispatch(&self, mouse: &MouseEvent) -> MouseAction {
        // Only respond to left-button press (not release/drag/scroll).
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return MouseAction::Forward;
        }

        let (mx, my) = (mouse.x, mouse.y);

        // Check tab bar first.
        let tab_area = self.tab_bar_area.get();
        if point_in_rect(tab_area, mx, my) {
            for slot_cell in &self.tab_slots {
                let slot = slot_cell.get();
                if let Some(screen) = slot.screen {
                    if my == slot.y && mx >= slot.x_start && mx < slot.x_end {
                        return MouseAction::SwitchScreen(screen);
                    }
                }
            }
        }

        // Check status line.
        let status_area = self.status_line_area.get();
        if point_in_rect(status_area, mx, my) {
            let rel_x = mx.saturating_sub(status_area.x);
            let mut palette_zone = STATUS_PALETTE_ZONE_WIDTH.min(status_area.width);
            let mut help_zone =
                STATUS_HELP_ZONE_WIDTH.min(status_area.width.saturating_sub(palette_zone));
            if help_zone == 0 && status_area.width > 1 {
                // On ultra-narrow status bars, split the line so both actions
                // remain reachable.
                help_zone = status_area.width / 2;
                palette_zone = status_area.width.saturating_sub(help_zone);
            }
            let palette_start = status_area.width.saturating_sub(palette_zone);
            if rel_x >= palette_start {
                return MouseAction::OpenPalette;
            }
            let help_start = palette_start.saturating_sub(help_zone);
            if rel_x >= help_start {
                return MouseAction::ToggleHelp;
            }

            return MouseAction::Forward;
        }

        MouseAction::Forward
    }
}

impl Default for MouseDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Check whether a point `(x, y)` falls inside a rectangle.
///
/// Returns `false` for empty (zero-area) rectangles.
#[inline]
#[must_use]
pub const fn point_in_rect(rect: Rect, x: u16, y: u16) -> bool {
    !rect.is_empty()
        && x >= rect.x
        && x < rect.x + rect.width
        && y >= rect.y
        && y < rect.y + rect.height
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn ranges_do_not_overlap() {
        // Verify non-overlapping: tab < category < pane < overlay < status
        assert!(TAB_HIT_BASE + 100 <= CATEGORY_HIT_BASE);
        assert!(CATEGORY_HIT_BASE + 100 <= PANE_HIT_BASE);
        assert!(PANE_HIT_BASE + 100 <= OVERLAY_HIT_BASE);
        assert!(OVERLAY_HIT_BASE + 100 <= STATUS_HIT_BASE);
    }

    #[test]
    fn tab_round_trip_all_screens() {
        for &id in ALL_SCREEN_IDS {
            let hit = tab_hit_id(id);
            assert_eq!(
                screen_from_tab_hit(hit),
                Some(id),
                "tab round-trip for {id:?}"
            );
        }
    }

    #[test]
    fn pane_round_trip_all_screens() {
        for &id in ALL_SCREEN_IDS {
            let hit = pane_hit_id(id);
            assert_eq!(
                screen_from_pane_hit(hit),
                Some(id),
                "pane round-trip for {id:?}"
            );
        }
    }

    #[test]
    fn category_round_trip() {
        for &cat in ScreenCategory::ALL {
            let hit = category_hit_id(cat);
            assert_eq!(
                category_from_hit(hit),
                Some(cat),
                "category round-trip for {cat:?}"
            );
        }
    }

    #[test]
    fn classify_tab_hit() {
        let hit = tab_hit_id(MailScreenId::Dashboard);
        assert_eq!(classify_hit(hit), HitLayer::Tab(MailScreenId::Dashboard));
    }

    #[test]
    fn classify_pane_hit() {
        let hit = pane_hit_id(MailScreenId::Messages);
        assert_eq!(classify_hit(hit), HitLayer::Pane(MailScreenId::Messages));
    }

    #[test]
    fn classify_category_hit() {
        let hit = category_hit_id(ScreenCategory::Communication);
        assert_eq!(
            classify_hit(hit),
            HitLayer::Category(ScreenCategory::Communication)
        );
    }

    #[test]
    fn classify_overlay_hit() {
        let hit = HitId::new(OVERLAY_HELP_CLOSE);
        assert_eq!(classify_hit(hit), HitLayer::Overlay(OVERLAY_HELP_CLOSE));

        let hit = HitId::new(OVERLAY_PALETTE);
        assert_eq!(classify_hit(hit), HitLayer::Overlay(OVERLAY_PALETTE));
    }

    #[test]
    fn classify_status_hit() {
        let hit = HitId::new(STATUS_HELP_TOGGLE);
        assert_eq!(
            classify_hit(hit),
            HitLayer::StatusToggle(STATUS_HELP_TOGGLE)
        );
    }

    #[test]
    fn classify_unknown_hit() {
        assert_eq!(classify_hit(HitId::new(0)), HitLayer::Unknown);
        assert_eq!(classify_hit(HitId::new(999)), HitLayer::Unknown);
        assert_eq!(classify_hit(HitId::new(7000)), HitLayer::Unknown);
    }

    #[test]
    fn overlay_takes_priority_over_pane_range() {
        // Overlay range (5000+) is checked before pane range (4000+),
        // so overlay IDs never get misclassified as panes.
        let hit = HitId::new(OVERLAY_HIT_BASE);
        assert!(matches!(classify_hit(hit), HitLayer::Overlay(_)));
    }

    #[test]
    fn out_of_range_tab_returns_none() {
        // Index beyond the screen count
        let hit = HitId::new(TAB_HIT_BASE + 99);
        assert_eq!(screen_from_tab_hit(hit), None);
    }

    #[test]
    fn out_of_range_pane_returns_none() {
        let hit = HitId::new(PANE_HIT_BASE + 99);
        assert_eq!(screen_from_pane_hit(hit), None);
    }

    #[test]
    fn out_of_range_category_returns_none() {
        let hit = HitId::new(CATEGORY_HIT_BASE + 99);
        assert_eq!(category_from_hit(hit), None);
    }

    #[test]
    fn every_screen_has_distinct_tab_and_pane_ids() {
        let mut tab_ids: Vec<u32> = ALL_SCREEN_IDS.iter().map(|&s| tab_hit_id(s).id()).collect();
        let mut pane_ids: Vec<u32> = ALL_SCREEN_IDS
            .iter()
            .map(|&s| pane_hit_id(s).id())
            .collect();
        tab_ids.sort_unstable();
        tab_ids.dedup();
        pane_ids.sort_unstable();
        pane_ids.dedup();
        assert_eq!(tab_ids.len(), ALL_SCREEN_IDS.len(), "tab IDs not unique");
        assert_eq!(pane_ids.len(), ALL_SCREEN_IDS.len(), "pane IDs not unique");
        // No overlap between tab and pane ranges
        for t in &tab_ids {
            assert!(!pane_ids.contains(t), "tab/pane ID collision at {t}");
        }
    }

    #[test]
    fn classify_covers_all_allocated_sub_ranges() {
        // Verify that each named overlay constant classifies correctly.
        let overlay_consts = [
            OVERLAY_HELP_CLOSE,
            OVERLAY_HELP_CONTENT,
            OVERLAY_PALETTE,
            OVERLAY_PERF_HUD,
            OVERLAY_ACTION_MENU,
            OVERLAY_TOAST,
        ];
        for &c in &overlay_consts {
            assert!(
                matches!(classify_hit(HitId::new(c)), HitLayer::Overlay(_)),
                "constant {c} not classified as Overlay"
            );
        }

        let status_consts = [
            STATUS_HELP_TOGGLE,
            STATUS_PALETTE_TOGGLE,
            STATUS_PERF_TOGGLE,
        ];
        for &c in &status_consts {
            assert!(
                matches!(classify_hit(HitId::new(c)), HitLayer::StatusToggle(_)),
                "constant {c} not classified as StatusToggle"
            );
        }
    }

    // ── MouseDispatcher tests ────────────────────────────────────────

    fn make_mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            x,
            y,
            modifiers: ftui::Modifiers::empty(),
        }
    }

    #[test]
    fn dispatcher_tab_click_routes_to_screen() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        // Register first tab: columns 0..8
        d.record_tab_slot(0, MailScreenId::Dashboard, 0, 8, 0);
        // Register second tab: columns 9..18
        d.record_tab_slot(1, MailScreenId::Messages, 9, 18, 0);

        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 3, 0);
        assert_eq!(
            d.dispatch(&ev),
            MouseAction::SwitchScreen(MailScreenId::Dashboard)
        );

        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 12, 0);
        assert_eq!(
            d.dispatch(&ev),
            MouseAction::SwitchScreen(MailScreenId::Messages)
        );
    }

    #[test]
    fn dispatcher_tab_gap_forwards() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        d.record_tab_slot(0, MailScreenId::Dashboard, 0, 8, 0);
        // Column 8 is the separator — no tab owns it.
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 8, 0);
        // Still in tab bar area but no slot matches — Forward.
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn dispatcher_status_line_click_toggles_help() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 52, 24);
        assert_eq!(d.dispatch(&ev), MouseAction::ToggleHelp);
    }

    #[test]
    fn dispatcher_status_right_side_opens_palette() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 78, 24);
        assert_eq!(d.dispatch(&ev), MouseAction::OpenPalette);
    }

    #[test]
    fn dispatcher_status_center_forwards() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 24, 24);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn dispatcher_status_narrow_width_keeps_help_and_palette_accessible() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 10, 1), Rect::new(0, 24, 10, 1));

        let help = make_mouse(MouseEventKind::Down(MouseButton::Left), 1, 24);
        assert_eq!(d.dispatch(&help), MouseAction::ToggleHelp);

        let palette = make_mouse(MouseEventKind::Down(MouseButton::Left), 9, 24);
        assert_eq!(d.dispatch(&palette), MouseAction::OpenPalette);
    }

    #[test]
    fn dispatcher_content_area_forwards() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 40, 12);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn dispatcher_ignores_non_left_clicks() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        d.record_tab_slot(0, MailScreenId::Dashboard, 0, 8, 0);
        // Right click on tab area — should forward, not switch.
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Right), 3, 0);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn dispatcher_ignores_mouse_up() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        d.record_tab_slot(0, MailScreenId::Dashboard, 0, 8, 0);
        let ev = make_mouse(MouseEventKind::Up(MouseButton::Left), 3, 0);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn dispatcher_ignores_scroll_on_status() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        let ev = make_mouse(MouseEventKind::ScrollDown, 5, 24);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn point_in_rect_boundary() {
        let r = Rect::new(10, 5, 20, 10);
        assert!(point_in_rect(r, 10, 5)); // top-left
        assert!(point_in_rect(r, 29, 14)); // bottom-right (inclusive)
        assert!(!point_in_rect(r, 30, 5)); // just right of right edge
        assert!(!point_in_rect(r, 10, 15)); // just below bottom
        assert!(!point_in_rect(r, 9, 5)); // just left
    }

    #[test]
    fn point_in_rect_empty() {
        let r = Rect::new(0, 0, 0, 0);
        assert!(!point_in_rect(r, 0, 0));
    }

    // ── Hit region edge case tests (br-1xt0m.1.13.7) ───────────

    #[test]
    fn point_in_rect_zero_width() {
        let r = Rect::new(5, 5, 0, 10);
        assert!(!point_in_rect(r, 5, 5));
    }

    #[test]
    fn point_in_rect_zero_height() {
        let r = Rect::new(5, 5, 10, 0);
        assert!(!point_in_rect(r, 5, 5));
    }

    #[test]
    fn point_in_rect_single_cell() {
        let r = Rect::new(5, 5, 1, 1);
        assert!(point_in_rect(r, 5, 5));
        assert!(!point_in_rect(r, 6, 5));
        assert!(!point_in_rect(r, 5, 6));
    }

    #[test]
    fn tab_slot_overwrite_updates_region() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        d.record_tab_slot(0, MailScreenId::Dashboard, 0, 8, 0);

        // Overwrite slot 0 with a different screen and region.
        d.record_tab_slot(0, MailScreenId::Messages, 10, 20, 0);
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 15, 0);
        assert_eq!(
            d.dispatch(&ev),
            MouseAction::SwitchScreen(MailScreenId::Messages)
        );

        // Old region should no longer hit.
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 3, 0);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn tab_slot_accessor_returns_none_for_unregistered() {
        let d = MouseDispatcher::new();
        // Before any recording, slot 0 should return None.
        assert!(d.tab_slot(0).is_none());

        d.record_tab_slot(0, MailScreenId::Dashboard, 5, 15, 0);
        let (x0, x1, y) = d.tab_slot(0).expect("slot 0 registered");
        assert_eq!((x0, x1, y), (5, 15, 0));

        // Out-of-range index.
        assert!(d.tab_slot(999).is_none());
    }

    #[test]
    fn dispatch_with_zero_area_chrome_forwards_all() {
        let d = MouseDispatcher::new();
        // Both chrome areas are zero-area → everything forwards.
        d.update_chrome_areas(Rect::new(0, 0, 0, 0), Rect::new(0, 0, 0, 0));
        let ev = make_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn dispatcher_drag_on_tab_forwards() {
        let d = MouseDispatcher::new();
        d.update_chrome_areas(Rect::new(0, 0, 80, 1), Rect::new(0, 24, 80, 1));
        d.record_tab_slot(0, MailScreenId::Dashboard, 0, 8, 0);
        let ev = make_mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0);
        assert_eq!(d.dispatch(&ev), MouseAction::Forward);
    }

    #[test]
    fn classify_boundary_ids_between_ranges() {
        // Last valid tab slot (just within range).
        let hit = HitId::new(TAB_HIT_BASE + 99);
        // Beyond ALL_SCREEN_IDS count, so should be Unknown (not tab).
        assert_eq!(classify_hit(hit), HitLayer::Unknown);

        // First overlay ID.
        assert!(matches!(
            classify_hit(HitId::new(OVERLAY_HIT_BASE)),
            HitLayer::Overlay(_)
        ));

        // Last ID before overlay (still in pane range, but beyond screen count).
        let hit = HitId::new(OVERLAY_HIT_BASE - 1);
        // This is in pane range (4000..5000) but index > screen count → Unknown.
        assert_eq!(classify_hit(hit), HitLayer::Unknown);
    }

    #[test]
    fn status_toggles_have_distinct_hit_ids() {
        let ids = [
            STATUS_HELP_TOGGLE,
            STATUS_PALETTE_TOGGLE,
            STATUS_PERF_TOGGLE,
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "status toggle IDs must be distinct");
            }
        }
    }

    #[test]
    fn overlay_sub_ranges_have_distinct_ids() {
        let ids = [
            OVERLAY_HELP_CLOSE,
            OVERLAY_HELP_CONTENT,
            OVERLAY_PALETTE,
            OVERLAY_PERF_HUD,
            OVERLAY_ACTION_MENU,
            OVERLAY_TOAST,
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "overlay sub-range IDs must be distinct");
            }
        }
    }
}
