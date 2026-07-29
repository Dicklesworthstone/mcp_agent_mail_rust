mod common;

use mcp_agent_mail_server::console::{
    BannerParams, ConsoleEventBuffer, ConsoleEventKind, ConsoleEventSeverity,
    STARTUP_STATE_JSON_BEGIN, STARTUP_STATE_JSON_END, StartupJsonRenderOptions, StartupStateJson,
    StartupStateStats, TIMELINE_MAX_EVENTS, TimelinePane, render_http_request_panel,
    render_startup_banner, render_startup_banner_with_options, render_tool_call_end,
    render_tool_call_start,
};

#[test]
fn startup_banner_sections_present_after_normalization() {
    let params = BannerParams {
        app_environment: "development",
        endpoint: "http://localhost:8765/mcp",
        database_url: "postgres://user:pass@localhost/db",
        storage_root: "/tmp/storage",
        auth_enabled: true,
        tools_log_enabled: true,
        tool_calls_log_enabled: true,
        console_theme: "Cyberpunk Aurora",
        web_ui_url: "http://localhost:8765/mail",
        projects: 3,
        agents: 5,
        messages: 42,
        file_reservations: 2,
        contact_links: 1,
        remote_url: None,
        startup_state_json: None,
    };
    let lines = render_startup_banner(&params);
    let joined = common::normalize_console_text(&lines.join("\n"));

    assert!(joined.contains("MCP Agent Mail"));
    assert!(joined.contains("Endpoint:"));
    assert!(joined.contains("Web UI:"));
    assert!(joined.contains("Tool logs:"));
    assert!(joined.contains("Tool panels:"));
    assert!(joined.contains("Database Statistics"));

    // Ensure banner sanitization is applied (userinfo password redaction).
    assert!(joined.contains("postgres://user:<redacted>@localhost/db"));
    assert!(!joined.contains("postgres://user:pass@localhost/db"));
}

#[test]
fn startup_banner_json_block_parses_after_normalization() {
    let params = BannerParams {
        app_environment: "development",
        endpoint: "http://localhost:8765/mcp",
        database_url: "postgres://user:pass@localhost/db",
        storage_root: "/tmp/storage",
        auth_enabled: true,
        tools_log_enabled: true,
        tool_calls_log_enabled: true,
        console_theme: "Cyberpunk Aurora",
        web_ui_url: "http://localhost:8765/mail",
        projects: 3,
        agents: 5,
        messages: 42,
        file_reservations: 2,
        contact_links: 1,
        remote_url: None,
        startup_state_json: Some(StartupStateJson {
            endpoint: "http://localhost:8765/mcp",
            web_ui: Some("http://localhost:8765/mail?token=secret123"),
            uptime: "2026-05-09T09:00:00+00:00",
            environment: "development",
            auth_enabled: true,
            tools_log_enabled: true,
            log_tool_calls_enabled: true,
            stats: StartupStateStats {
                projects: Some(3),
                agents: Some(5),
                messages: Some(42),
                file_reservations: Some(2),
                contact_links: Some(1),
            },
            storage_root: "/tmp/storage",
            database_url: Some("postgres://user:pass@localhost/db"),
            http_bearer_token: Some("secret123"),
            am_git_binary: Some("/usr/local/bin/git-2.50.x"),
        }),
    };
    let lines = render_startup_banner_with_options(
        &params,
        StartupJsonRenderOptions {
            enabled: true,
            use_ansi: true,
        },
    );
    let normalized = common::strip_ansi_and_osc(&lines.join("\n"));
    let start = normalized
        .find(STARTUP_STATE_JSON_BEGIN)
        .expect("json begin marker");
    let after_start = start + STARTUP_STATE_JSON_BEGIN.len();
    let end = normalized[after_start..]
        .find(STARTUP_STATE_JSON_END)
        .map(|idx| after_start + idx)
        .expect("json end marker");
    let json = normalized[after_start..end].trim();
    let value: serde_json::Value = serde_json::from_str(json).expect("parse startup state json");

    assert_eq!(value["stats"]["projects"], 3);
    assert_eq!(value["stats"]["agents"], 5);
    assert_eq!(value["stats"]["messages"], 42);
    assert_eq!(
        value["runtime"]["database_url"],
        "postgres://user:<redacted>@localhost/db"
    );
    assert_eq!(value["runtime"]["http_bearer_token"], "<redacted>");
    assert_eq!(value["runtime"]["am_git_binary"], "git-2.50.x");
    assert!(!normalized.contains("secret123"));
}

#[test]
fn tool_call_start_masks_params_after_normalization() {
    let params = serde_json::json!({
        "project_key": "/data/backend",
        "agent_name": "BlueLake",
        "bearer_token": "secret123"
    });
    let lines = render_tool_call_start("health_check", &params, None, None);
    let joined = common::normalize_console_text(&lines.join("\n"));

    assert!(joined.contains("TOOL CALL"));
    assert!(joined.contains("health_check"));
    assert!(joined.contains("Parameters:"));

    // Sensitive values masked; identity signals preserved.
    assert!(!joined.contains("secret123"));
    assert!(joined.contains("<redacted>"));
    assert!(joined.contains("/data/backend"));
    assert!(joined.contains("BlueLake"));
}

#[test]
fn tool_call_end_masks_result_json_after_normalization() {
    let result = r#"{"bearer_token":"secret123","ok":true}"#;
    let lines = render_tool_call_end("test_tool", 10, Some(result), 0, 0.0, &[], 2000);
    let joined = common::normalize_console_text(&lines.join("\n"));

    assert!(joined.contains("test_tool"));
    assert!(joined.contains("completed in"));
    assert!(joined.contains("Result:"));
    assert!(!joined.contains("secret123"));
    assert!(joined.contains("<redacted>"));
}

#[test]
fn http_request_panel_contains_method_path_status() {
    let panel =
        render_http_request_panel(100, "GET", "/health/liveness", 200, 5, "127.0.0.1", true)
            .expect("expected panel");
    let joined = common::normalize_console_text(&panel);
    assert!(joined.contains("GET"));
    assert!(joined.contains("/health/liveness"));
    assert!(joined.contains("200"));
    assert!(joined.contains("5ms"));
    assert!(joined.contains("client:"));
    assert!(joined.contains("127.0.0.1"));
}

#[test]
fn http_request_panel_plain_has_no_ansi_escapes() {
    let panel = render_http_request_panel(100, "POST", "/mcp", 201, 42, "10.0.0.1", false)
        .expect("expected panel");
    // For plain panels, the raw output should already be escape-free.
    assert_eq!(panel, common::strip_ansi_and_osc(&panel));
    assert!(panel.contains("POST"));
    assert!(panel.contains("/mcp"));
    assert!(panel.contains("201"));
    assert!(panel.contains("42ms"));
}

#[test]
fn console_event_buffer_eviction_keeps_newest_and_monotonic_ids() {
    let extra = 10usize;
    assert!(extra < TIMELINE_MAX_EVENTS);

    let mut buf = ConsoleEventBuffer::new();
    for i in 0..(TIMELINE_MAX_EVENTS + extra) {
        buf.push(
            ConsoleEventKind::HttpRequest,
            ConsoleEventSeverity::Info,
            format!("ev {i}"),
            vec![],
            None,
        );
    }

    assert_eq!(buf.len(), TIMELINE_MAX_EVENTS);
    let snap = buf.snapshot();
    assert_eq!(snap.len(), TIMELINE_MAX_EVENTS);
    assert_eq!(snap.first().expect("first").id, (extra as u64) + 1);
    assert_eq!(
        snap.last().expect("last").id,
        (TIMELINE_MAX_EVENTS + extra) as u64
    );

    for w in snap.windows(2) {
        assert!(w[0].id < w[1].id);
    }
}

#[test]
fn timeline_pane_severity_filter_cycles() {
    let mut pane = TimelinePane::new();
    assert_eq!(pane.filter_severity(), None);

    pane.cycle_severity_filter();
    assert_eq!(pane.filter_severity(), Some(ConsoleEventSeverity::Info));
    pane.cycle_severity_filter();
    assert_eq!(pane.filter_severity(), Some(ConsoleEventSeverity::Warn));
    pane.cycle_severity_filter();
    assert_eq!(pane.filter_severity(), Some(ConsoleEventSeverity::Error));
    pane.cycle_severity_filter();
    assert_eq!(pane.filter_severity(), None);
}

#[test]
fn timeline_pane_render_smoke_empty_does_not_panic() {
    use ftui::layout::Rect;
    use ftui::{Frame, GraphemePool};

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);

    let mut pane = TimelinePane::new();
    pane.render(Rect::new(0, 0, 80, 24), &mut frame, &[]);
}

#[test]
fn timeline_pane_render_smoke_single_event_does_not_panic() {
    use ftui::layout::Rect;
    use ftui::{Frame, GraphemePool};

    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(80, 24, &mut pool);

    let mut buf = ConsoleEventBuffer::new();
    buf.push(
        ConsoleEventKind::ToolCallStart,
        ConsoleEventSeverity::Warn,
        "hello timeline",
        vec![("agent".to_string(), "AmberStream".to_string())],
        Some(serde_json::json!({"ok": true})),
    );
    let events = buf.snapshot();

    let mut pane = TimelinePane::new();
    pane.render(Rect::new(0, 0, 80, 24), &mut frame, &events);
}
