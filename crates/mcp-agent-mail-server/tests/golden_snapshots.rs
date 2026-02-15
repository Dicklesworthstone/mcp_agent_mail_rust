#![forbid(unsafe_code)]

//! Golden snapshot tests for key TUI screens and states.
//!
//! Renders each screen at standard terminal sizes (80x24, 120x40) and
//! compares against stored `.snap` baselines under `tests/snapshots/`.
//!
//! Run `BLESS=1 cargo test -p mcp-agent-mail-server --test golden_snapshots`
//! to create or update snapshot files.

use std::sync::Arc;

use ftui::{Frame, GraphemePool};
use ftui_harness::{Rect, assert_snapshot, buffer_to_text};
use mcp_agent_mail_core::Config;
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_screens::{
    MailScreen, MailScreenId, agents::AgentsScreen, dashboard::DashboardScreen,
    messages::MessageBrowserScreen, reservations::ReservationsScreen, search::SearchCockpitScreen,
    system_health::SystemHealthScreen, threads::ThreadExplorerScreen,
    tool_metrics::ToolMetricsScreen,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_state() -> Arc<TuiSharedState> {
    let config = Config::default();
    TuiSharedState::new(&config)
}

/// Render a single screen into a buffer and snapshot it.
fn snapshot_screen(
    screen: &dyn MailScreen,
    state: &TuiSharedState,
    width: u16,
    height: u16,
    name: &str,
) {
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    let area = Rect::new(0, 0, width, height);
    screen.view(&mut frame, area, state);
    assert_snapshot!(name, &frame.buffer);
}

/// Render the full app model (tab bar + screen + status line) and snapshot it.
fn snapshot_app(width: u16, height: u16, screen_id: MailScreenId, name: &str) {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));

    // Navigate to the target screen
    if screen_id != MailScreenId::Dashboard {
        model.update(MailMsg::SwitchScreen(screen_id));
    }

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    model.view(&mut frame);
    assert_snapshot!(name, &frame.buffer);
}

// ---------------------------------------------------------------------------
// Individual screen snapshots (80x24)
// ---------------------------------------------------------------------------

#[test]
fn dashboard_empty_80x24() {
    let state = test_state();
    let screen = DashboardScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "dashboard_empty_80x24");
}

#[test]
fn messages_empty_80x24() {
    let state = test_state();
    let screen = MessageBrowserScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "messages_empty_80x24");
}

#[test]
fn threads_empty_80x24() {
    let state = test_state();
    let screen = ThreadExplorerScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "threads_empty_80x24");
}

#[test]
fn agents_empty_80x24() {
    let state = test_state();
    let screen = AgentsScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "agents_empty_80x24");
}

#[test]
fn search_empty_80x24() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "search_empty_80x24");
}

#[test]
fn reservations_empty_80x24() {
    let state = test_state();
    let screen = ReservationsScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "reservations_empty_80x24");
}

#[test]
fn tool_metrics_empty_80x24() {
    let state = test_state();
    let screen = ToolMetricsScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "tool_metrics_empty_80x24");
}

#[test]
fn system_health_empty_80x24() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 80, 24, "system_health_empty_80x24");
}

// ---------------------------------------------------------------------------
// Wide terminal snapshots (120x40)
// ---------------------------------------------------------------------------

#[test]
fn dashboard_empty_120x40() {
    let state = test_state();
    let screen = DashboardScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "dashboard_empty_120x40");
}

#[test]
fn dashboard_ultrawide_200x50() {
    let state = test_state();
    let screen = DashboardScreen::new();
    snapshot_screen(&screen, &state, 200, 50, "dashboard_ultrawide_200x50");
}

#[test]
fn agents_empty_120x40() {
    let state = test_state();
    let screen = AgentsScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "agents_empty_120x40");
}

#[test]
fn search_empty_120x40() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "search_empty_120x40");
}

#[test]
fn system_health_empty_120x40() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 120, 40, "system_health_empty_120x40");
}

// ---------------------------------------------------------------------------
// Full-app snapshots (tab bar + screen + status line)
// ---------------------------------------------------------------------------

#[test]
fn app_dashboard_80x24() {
    snapshot_app(80, 24, MailScreenId::Dashboard, "app_dashboard_80x24");
}

#[test]
fn app_messages_80x24() {
    snapshot_app(80, 24, MailScreenId::Messages, "app_messages_80x24");
}

#[test]
fn app_threads_80x24() {
    snapshot_app(80, 24, MailScreenId::Threads, "app_threads_80x24");
}

#[test]
fn app_agents_80x24() {
    snapshot_app(80, 24, MailScreenId::Agents, "app_agents_80x24");
}

#[test]
fn app_search_80x24() {
    snapshot_app(80, 24, MailScreenId::Search, "app_search_80x24");
}

#[test]
fn app_system_health_80x24() {
    snapshot_app(
        80,
        24,
        MailScreenId::SystemHealth,
        "app_system_health_80x24",
    );
}

// ---------------------------------------------------------------------------
// Compact terminal (minimal viable) snapshots
// ---------------------------------------------------------------------------

#[test]
fn dashboard_compact_40x12() {
    let state = test_state();
    let screen = DashboardScreen::new();
    snapshot_screen(&screen, &state, 40, 12, "dashboard_compact_40x12");
}

#[test]
fn messages_compact_40x12() {
    let state = test_state();
    let screen = MessageBrowserScreen::new();
    snapshot_screen(&screen, &state, 40, 12, "messages_compact_40x12");
}

// ---------------------------------------------------------------------------
// Sanity: buffer_to_text produces non-empty output
// ---------------------------------------------------------------------------

#[test]
fn buffer_to_text_is_not_blank() {
    let state = test_state();
    let screen = DashboardScreen::new();
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);
    let area = Rect::new(0, 0, 80, 24);
    screen.view(&mut frame, area, &state);
    let text = buffer_to_text(&frame.buffer);
    let non_space_count = text.chars().filter(|c| !c.is_whitespace()).count();
    assert!(
        non_space_count > 10,
        "Dashboard should render visible content, got {non_space_count} non-space chars"
    );
}

// ===========================================================================
// TUI V2 Widget Snapshot Tests (br-2bbt.11.1)
// ===========================================================================
//
// These tests cover the new TUI V2 widgets introduced in the
// TUI V2 Showcase-Grade Upgrade epic (br-2bbt) via full-app rendering:
// - Command Palette (br-2bbt.1)
// - Toast Notifications (br-2bbt.2)
// - Modal Dialogs (br-2bbt.5)
// - Native Charts (br-2bbt.4)

use ftui::{Event, KeyCode, Modifiers};

// ---------------------------------------------------------------------------
// Command Palette - Full App Snapshots (br-2bbt.1)
// ---------------------------------------------------------------------------

#[test]
fn app_with_palette_open_80x24() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));

    // Open command palette via Ctrl+P key event
    let ctrl_p =
        Event::Key(ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL));
    model.update(MailMsg::Terminal(ctrl_p));

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);
    model.view(&mut frame);
    assert_snapshot!("app_with_palette_open_80x24", &frame.buffer);
}

#[test]
fn app_with_palette_open_160x48() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));

    // Open command palette via Ctrl+P key event
    let ctrl_p =
        Event::Key(ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL));
    model.update(MailMsg::Terminal(ctrl_p));

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(160, 48, &mut pool);
    model.view(&mut frame);
    assert_snapshot!("app_with_palette_open_160x48", &frame.buffer);
}

// ---------------------------------------------------------------------------
// More terminal size variants for comprehensive coverage
// ---------------------------------------------------------------------------

#[test]
fn app_reservations_80x24() {
    snapshot_app(80, 24, MailScreenId::Reservations, "app_reservations_80x24");
}

#[test]
fn app_reservations_120x40() {
    snapshot_app(
        120,
        40,
        MailScreenId::Reservations,
        "app_reservations_120x40",
    );
}

#[test]
fn app_tool_metrics_80x24() {
    snapshot_app(80, 24, MailScreenId::ToolMetrics, "app_tool_metrics_80x24");
}

#[test]
fn app_tool_metrics_120x40() {
    snapshot_app(
        120,
        40,
        MailScreenId::ToolMetrics,
        "app_tool_metrics_120x40",
    );
}

// ---------------------------------------------------------------------------
// Large terminal variants for dashboard layouts
// ---------------------------------------------------------------------------

#[test]
fn app_dashboard_160x48() {
    snapshot_app(160, 48, MailScreenId::Dashboard, "app_dashboard_160x48");
}

#[test]
fn app_messages_160x48() {
    snapshot_app(160, 48, MailScreenId::Messages, "app_messages_160x48");
}

#[test]
fn app_search_160x48() {
    snapshot_app(160, 48, MailScreenId::Search, "app_search_160x48");
}

// ===========================================================================
// Snapshot Matrix — Width, Overlay, and Semantic Hierarchy (br-1xt0m.1.13.9)
// ===========================================================================
//
// These tests cover the 4-width matrix (80/100/120/160) for chrome/status,
// help overlay sizing, overlay stack precedence, high-density screen states,
// and semantic color token validation.

use mcp_agent_mail_server::tui_screens::timeline::TimelineScreen;

// ---------------------------------------------------------------------------
// Chrome/Status/Help at 80/100/120/160 columns
// ---------------------------------------------------------------------------

/// Status bar at 80 columns: Critical + High + Medium segments visible.
#[test]
fn app_dashboard_80x24_status_segments() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::MailAppModel;

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let model = MailAppModel::new(Arc::clone(&state));

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);
    model.view(&mut frame);
    let text = buffer_to_text(&frame.buffer);

    // At 80 cols: Critical segments (screen title + help hint) + High + Medium
    // Screen title "Dashboard" must be present (Critical)
    assert!(
        text.contains("Dashboard"),
        "80-col: screen title must be present (Critical): {text}"
    );
    // Help hint '?' must be present (Critical)
    assert!(
        text.contains('?'),
        "80-col: help hint must be present (Critical): {text}"
    );
}

/// Status bar at 100 columns: All segments including Low (counters, theme).
#[test]
fn app_dashboard_100x30_full_segments() {
    snapshot_app(100, 30, MailScreenId::Dashboard, "app_dashboard_100x30");
}

/// Status bar at 120 columns: Wide terminal shows everything.
#[test]
fn app_dashboard_120x40_full() {
    snapshot_app(120, 40, MailScreenId::Dashboard, "app_dashboard_120x40");
}

/// Status bar at 160 columns: Ultra-wide, all segments with room to spare.
#[test]
fn app_messages_100x30() {
    snapshot_app(100, 30, MailScreenId::Messages, "app_messages_100x30");
}

#[test]
fn app_search_100x30() {
    snapshot_app(100, 30, MailScreenId::Search, "app_search_100x30");
}

// ---------------------------------------------------------------------------
// Help overlay sizing matrix
// ---------------------------------------------------------------------------

/// Help overlay at 80x24: 60% → 48w clamped to 36-72, 14h clamped to 10-28.
#[test]
fn app_with_help_overlay_80x24() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));
    model.update(MailMsg::ToggleHelp);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);
    model.view(&mut frame);
    assert_snapshot!("app_with_help_overlay_80x24", &frame.buffer);
}

/// Help overlay at 120x40: larger overlay with more scroll room.
#[test]
fn app_with_help_overlay_120x40() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));
    model.update(MailMsg::ToggleHelp);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(120, 40, &mut pool);
    model.view(&mut frame);
    assert_snapshot!("app_with_help_overlay_120x40", &frame.buffer);
}

/// Help overlay at 160x48: wide but height-clamped at 28 rows.
#[test]
fn app_with_help_overlay_160x48() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));
    model.update(MailMsg::ToggleHelp);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(160, 48, &mut pool);
    model.view(&mut frame);
    assert_snapshot!("app_with_help_overlay_160x48", &frame.buffer);
}

// ---------------------------------------------------------------------------
// Overlay stack precedence: Help renders above palette above base
// ---------------------------------------------------------------------------

/// Both palette and help open: help (z=7) should render on top.
#[test]
fn app_help_over_palette_precedence_80x24() {
    use ftui_runtime::Model;
    use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};

    let config = Config::default();
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(Arc::clone(&state));

    // Open palette first (Ctrl+P), then help (?).
    let ctrl_p =
        Event::Key(ftui::KeyEvent::new(KeyCode::Char('p')).with_modifiers(Modifiers::CTRL));
    model.update(MailMsg::Terminal(ctrl_p));

    // Open help overlay on top.
    model.update(MailMsg::ToggleHelp);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);
    model.view(&mut frame);

    let text = buffer_to_text(&frame.buffer);
    // Help overlay title should be visible (topmost overlay)
    assert!(
        text.contains("Keyboard Shortcuts") || text.contains("Esc to close"),
        "help overlay should be visible on top of palette: {text}"
    );

    assert_snapshot!("app_help_over_palette_80x24", &frame.buffer);
}

// ---------------------------------------------------------------------------
// High-density screen states — 4-width matrix for key screens
// ---------------------------------------------------------------------------

// Messages screen at all 4 widths
#[test]
fn messages_width_matrix_80() {
    let state = test_state();
    let screen = MessageBrowserScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "messages_width_80x24");
}

#[test]
fn messages_width_matrix_100() {
    let state = test_state();
    let screen = MessageBrowserScreen::new();
    snapshot_screen(&screen, &state, 100, 30, "messages_width_100x30");
}

#[test]
fn messages_width_matrix_120() {
    let state = test_state();
    let screen = MessageBrowserScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "messages_width_120x40");
}

#[test]
fn messages_width_matrix_160() {
    let state = test_state();
    let screen = MessageBrowserScreen::new();
    snapshot_screen(&screen, &state, 160, 48, "messages_width_160x48");
}

// Threads screen at all 4 widths
#[test]
fn threads_width_matrix_80() {
    let state = test_state();
    let screen = ThreadExplorerScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "threads_width_80x24");
}

#[test]
fn threads_width_matrix_100() {
    let state = test_state();
    let screen = ThreadExplorerScreen::new();
    snapshot_screen(&screen, &state, 100, 30, "threads_width_100x30");
}

#[test]
fn threads_width_matrix_120() {
    let state = test_state();
    let screen = ThreadExplorerScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "threads_width_120x40");
}

#[test]
fn threads_width_matrix_160() {
    let state = test_state();
    let screen = ThreadExplorerScreen::new();
    snapshot_screen(&screen, &state, 160, 48, "threads_width_160x48");
}

// Search screen at all 4 widths
#[test]
fn search_width_matrix_80() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "search_width_80x24");
}

#[test]
fn search_width_matrix_100() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 100, 30, "search_width_100x30");
}

#[test]
fn search_width_matrix_120() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "search_width_120x40");
}

#[test]
fn search_width_matrix_160() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 160, 48, "search_width_160x48");
}

// Timeline screen at all 4 widths
#[test]
fn timeline_width_matrix_80() {
    let state = test_state();
    let screen = TimelineScreen::new();
    snapshot_screen(&screen, &state, 80, 24, "timeline_width_80x24");
}

#[test]
fn timeline_width_matrix_100() {
    let state = test_state();
    let screen = TimelineScreen::new();
    snapshot_screen(&screen, &state, 100, 30, "timeline_width_100x30");
}

#[test]
fn timeline_width_matrix_120() {
    let state = test_state();
    let screen = TimelineScreen::new();
    snapshot_screen(&screen, &state, 120, 40, "timeline_width_120x40");
}

#[test]
fn timeline_width_matrix_160() {
    let state = test_state();
    let screen = TimelineScreen::new();
    snapshot_screen(&screen, &state, 160, 48, "timeline_width_160x48");
}

// SystemHealth screen at all 4 widths
#[test]
fn system_health_width_matrix_80() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 80, 24, "system_health_width_80x24");
}

#[test]
fn system_health_width_matrix_100() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 100, 30, "system_health_width_100x30");
}

#[test]
fn system_health_width_matrix_120() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 120, 40, "system_health_width_120x40");
}

#[test]
fn system_health_width_matrix_160() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 160, 48, "system_health_width_160x48");
}

// ---------------------------------------------------------------------------
// Narrow terminal: minimum viable rendering (no panics)
// ---------------------------------------------------------------------------

#[test]
fn search_narrow_30x10() {
    let state = test_state();
    let screen = SearchCockpitScreen::new();
    snapshot_screen(&screen, &state, 30, 10, "search_narrow_30x10");
}

#[test]
fn system_health_narrow_30x10() {
    let state = test_state();
    let screen = SystemHealthScreen::new(Arc::clone(&state));
    snapshot_screen(&screen, &state, 30, 10, "system_health_narrow_30x10");
}

#[test]
fn timeline_narrow_30x10() {
    let state = test_state();
    let screen = TimelineScreen::new();
    snapshot_screen(&screen, &state, 30, 10, "timeline_narrow_30x10");
}

#[test]
fn threads_narrow_30x10() {
    let state = test_state();
    let screen = ThreadExplorerScreen::new();
    snapshot_screen(&screen, &state, 30, 10, "threads_narrow_30x10");
}
