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
