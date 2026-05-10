#![forbid(unsafe_code)]

use std::sync::Arc;

use ftui::{Event, Frame, GraphemePool, KeyCode, KeyEvent, Modifiers};
use ftui_runtime::program::Model;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg, OverlayLayer};
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_events::MailEvent;
use mcp_agent_mail_server::tui_screens::system_health::SystemHealthScreen;
use mcp_agent_mail_server::tui_screens::{MailScreen, MailScreenId};

fn test_state() -> Arc<TuiSharedState> {
    TuiSharedState::new(&Config::default())
}

fn retry_event(repo_slug: &str, attempt_n: u32, exhausted: bool) -> MailEvent {
    MailEvent::git_segfault_retry(
        if exhausted {
            "git_segfault_retry_exhausted"
        } else {
            "git_segfault_retry_attempt"
        },
        repo_slug,
        attempt_n,
        Some(11),
        exhausted,
    )
}

fn render_app_text(model: &MailAppModel) -> String {
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(160, 48, &mut pool);
    model.view(&mut frame);
    ftui_harness::buffer_to_text(&frame.buffer)
}

#[test]
fn system_health_badge_renders_git_segfault_retry_totals() {
    let state = test_state();
    let mut screen = SystemHealthScreen::new(Arc::clone(&state));
    assert!(state.push_event(retry_event("data-projects-foo", 1, false)));
    assert!(state.push_event(retry_event("data-projects-foo", 2, false)));
    assert!(state.push_event(retry_event("data-projects-foo", 3, true)));

    screen.tick(1, &state);

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(160, 48, &mut pool);
    screen.view(&mut frame, ftui::layout::Rect::new(0, 0, 160, 48), &state);
    let text = ftui_harness::buffer_to_text(&frame.buffer);

    assert!(
        text.contains("Git retry guard"),
        "System Health should render the git retry guard row: {text}"
    );
    assert!(
        text.contains("git segfault retries: 3 (EXHAUSTED 1)"),
        "badge should include live retry/exhaustion totals: {text}"
    );
}

#[test]
fn mail_app_toast_pipeline_warns_once_then_ctrl_y_dismisses() {
    let config = Config {
        tui_coach_hints_enabled: false,
        ..Default::default()
    };
    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::with_config(Arc::clone(&state), &config);
    model.update(MailMsg::SwitchScreen(MailScreenId::SystemHealth));

    assert!(state.push_event(retry_event("data-projects-foo", 1, false)));
    model.update(MailMsg::Terminal(Event::Tick));

    let text = render_app_text(&model);
    let warning_prefix = "git 2.51.0 segfault retried on data-proje";
    assert!(
        text.contains(warning_prefix),
        "first retry should surface a warning toast: {text}"
    );
    assert_eq!(model.topmost_overlay(), OverlayLayer::Toasts);

    assert!(state.push_event(retry_event("data-projects-foo", 2, false)));
    model.update(MailMsg::Terminal(Event::Tick));
    let text = render_app_text(&model);
    assert_eq!(
        text.matches(warning_prefix).count(),
        1,
        "subsequent retry should not enqueue another first-warning toast: {text}"
    );

    let ctrl_y = Event::Key(KeyEvent::new(KeyCode::Char('y')).with_modifiers(Modifiers::CTRL));
    model.update(MailMsg::Terminal(ctrl_y));
    assert_eq!(model.topmost_overlay(), OverlayLayer::ToastFocus);

    let mut text = String::new();
    for _ in 0..5 {
        model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
        model.update(MailMsg::Terminal(Event::Tick));
        text = render_app_text(&model);
        if !text.contains(warning_prefix) {
            break;
        }
    }
    assert!(
        !text.contains(warning_prefix),
        "Enter in toast focus should make the git warning dismissible: {text}"
    );

    model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(
        KeyCode::Escape,
    ))));
    assert_ne!(model.topmost_overlay(), OverlayLayer::ToastFocus);
}
