//! Criterion benchmarks for frame-time budget enforcement (br-43g80).
//!
//! Includes:
//! - Bayesian diff-decision overhead
//! - Dashboard render with 10k events
//! - Timeline render with 10k events
//! - Messages render with 1000 searched rows (markdown bodies)
//! - Full-density heatmap rendering
//! - Ambient effect rendering
//! - Theme switch latency

use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use ftui::layout::Rect;
use ftui::widgets::Widget;
use ftui::{Event, Frame, GraphemePool, KeyCode, KeyEvent, Model};
use ftui_extras::theme::ThemeId;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_decision::{BayesianDiffStrategy, FrameState};
use mcp_agent_mail_server::tui_events::{DbStatSnapshot, MailEvent};
use mcp_agent_mail_server::tui_screens::MailScreenId;
use mcp_agent_mail_server::tui_theme;
use mcp_agent_mail_server::tui_widgets::{
    AmbientEffectRenderer, AmbientHealthInput, AmbientMode, HeatmapGrid,
};
use tempfile::TempDir;

const fn stable_frame() -> FrameState {
    FrameState {
        change_ratio: 0.05,
        is_resize: false,
        budget_remaining_ms: 14.0,
        error_count: 0,
    }
}

const fn bursty_frame() -> FrameState {
    FrameState {
        change_ratio: 0.6,
        is_resize: false,
        budget_remaining_ms: 12.0,
        error_count: 0,
    }
}

const fn resize_frame() -> FrameState {
    FrameState {
        change_ratio: 0.0,
        is_resize: true,
        budget_remaining_ms: 16.0,
        error_count: 0,
    }
}

const fn degraded_frame() -> FrameState {
    FrameState {
        change_ratio: 0.2,
        is_resize: false,
        budget_remaining_ms: 2.0,
        error_count: 5,
    }
}

fn render_model(model: &MailAppModel, width: u16, height: u16) {
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(width, height, &mut pool);
    model.view(&mut frame);
    black_box(frame.width());
}

fn make_model(state: Arc<TuiSharedState>, screen: MailScreenId) -> MailAppModel {
    let mut model = MailAppModel::new(state);
    let _ = model.update(MailMsg::SwitchScreen(screen));
    let _ = model.update(MailMsg::Terminal(Event::Tick));
    model
}

fn populate_dashboard_stats(state: &TuiSharedState) {
    state.update_db_stats(DbStatSnapshot {
        projects: 42,
        agents: 1_024,
        messages: 245_000,
        file_reservations: 540,
        contact_links: 3_400,
        ack_pending: 128,
        timestamp_micros: chrono::Utc::now().timestamp_micros(),
        ..DbStatSnapshot::default()
    });

    for idx in 0..240_u64 {
        let status = if idx % 17 == 0 {
            500
        } else if idx % 9 == 0 {
            404
        } else {
            200
        };
        state.record_request(status, 5 + (idx % 120));
    }
}

fn populate_event_ring(state: &TuiSharedState, count: usize) {
    for idx in 0..count {
        let seq_id = i64::try_from(idx.saturating_add(1)).unwrap_or(i64::MAX - 1);
        let event = match idx % 7 {
            0 => MailEvent::message_received(
                seq_id,
                "BenchSender",
                vec!["BenchReceiver".to_string()],
                format!("alert benchmark subject {seq_id}"),
                format!("bench-thread-{}", idx % 64),
                "bench-project",
            ),
            1 => MailEvent::tool_call_end(
                "send_message",
                u64::try_from((idx % 80) + 1).unwrap_or(1),
                Some("ok".to_string()),
                2,
                1.6,
                vec![("messages".to_string(), 1), ("agents".to_string(), 1)],
                Some("bench-project".to_string()),
                Some("BenchAgent".to_string()),
            ),
            2 => MailEvent::reservation_granted(
                "BenchAgent",
                vec![format!("src/module_{}/**", idx % 32)],
                idx % 3 == 0,
                3600,
                "bench-project",
            ),
            3 => MailEvent::agent_registered(
                format!("Agent{}", idx % 128),
                "codex-cli",
                "gpt-5-codex",
                "bench-project",
            ),
            4 => MailEvent::http_request(
                "POST",
                "/mcp",
                if idx % 19 == 0 { 500 } else { 200 },
                u64::try_from((idx % 120) + 2).unwrap_or(2),
                "127.0.0.1",
            ),
            5 => MailEvent::tool_call_start(
                "fetch_inbox",
                serde_json::json!({"limit": 20, "agent": "BenchAgent"}),
                Some("bench-project".to_string()),
                Some("BenchAgent".to_string()),
            ),
            _ => MailEvent::server_started("http://127.0.0.1:8765/mcp/", "benchmark"),
        };
        let _ = state.push_event(event);
    }
}

fn sql_escape(input: &str) -> String {
    input.replace('\'', "''")
}

fn init_messages_bench_db(path: &Path, message_count: usize) {
    let path_str = path.display().to_string();
    let conn = DbConn::open_file(&path_str).expect("open benchmark sqlite file");
    conn.execute_raw(
        "CREATE TABLE projects (\
           id INTEGER PRIMARY KEY, \
           slug TEXT NOT NULL, \
           human_key TEXT NOT NULL, \
           created_at INTEGER NOT NULL\
         )",
    )
    .expect("create projects");
    conn.execute_raw(
        "CREATE TABLE agents (\
           id INTEGER PRIMARY KEY, \
           project_id INTEGER NOT NULL, \
           name TEXT NOT NULL, \
           program TEXT NOT NULL, \
           model TEXT NOT NULL, \
           task_description TEXT NOT NULL DEFAULT '', \
           inception_ts INTEGER NOT NULL, \
           last_active_ts INTEGER NOT NULL, \
           attachments_policy TEXT NOT NULL DEFAULT 'auto', \
           contact_policy TEXT NOT NULL DEFAULT 'auto'\
         )",
    )
    .expect("create agents");
    conn.execute_raw(
        "CREATE TABLE messages (\
           id INTEGER PRIMARY KEY, \
           project_id INTEGER NOT NULL, \
           sender_id INTEGER NOT NULL, \
           thread_id TEXT, \
           subject TEXT NOT NULL, \
           body_md TEXT NOT NULL, \
           importance TEXT NOT NULL DEFAULT 'normal', \
           ack_required INTEGER NOT NULL DEFAULT 0, \
           created_ts INTEGER NOT NULL, \
           attachments TEXT NOT NULL DEFAULT '[]'\
         )",
    )
    .expect("create messages");
    conn.execute_raw(
        "CREATE TABLE message_recipients (\
           message_id INTEGER NOT NULL, \
           agent_id INTEGER NOT NULL, \
           kind TEXT NOT NULL DEFAULT 'to', \
           read_ts INTEGER, \
           ack_ts INTEGER\
         )",
    )
    .expect("create message_recipients");
    conn.execute_raw("CREATE INDEX idx_messages_created_ts ON messages(created_ts DESC)")
        .expect("create messages index");

    conn.execute_raw(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'bench-project', '/bench/project', 1700000000000000)",
    )
    .expect("insert project");
    conn.execute_raw(
        "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES \
         (1, 1, 'BenchSender', 'codex-cli', 'gpt-5-codex', 'bench sender', 1700000000000000, 1700000000000000, 'auto', 'auto'), \
         (2, 1, 'BenchReceiver', 'codex-cli', 'gpt-5-codex', 'bench receiver', 1700000000000000, 1700000000000000, 'auto', 'auto')",
    )
    .expect("insert agents");

    for idx in 0..message_count {
        let id = i64::try_from(idx.saturating_add(1)).unwrap_or(i64::MAX - 1);
        let created_ts = 1_730_000_000_000_000_i64 + id.saturating_mul(1_000_i64);
        let subject = format!("alert benchmark message {id}");
        let body_md = format!(
            "# Alert {id}\n\n- status: degraded\n- p95_ms: {}\n\n```json\n{{\"id\": {id}, \"kind\": \"benchmark\"}}\n```\n",
            id % 1_000
        );
        let thread_id = format!("bench-thread-{}", id % 64);
        let insert_message = format!(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
             VALUES ({id}, 1, 1, '{}', '{}', '{}', 'high', 0, {created_ts}, '[]')",
            sql_escape(&thread_id),
            sql_escape(&subject),
            sql_escape(&body_md),
        );
        conn.execute_raw(&insert_message)
            .expect("insert benchmark message");
        let insert_recipient = format!(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES ({id}, 2, 'to', NULL, NULL)"
        );
        conn.execute_raw(&insert_recipient)
            .expect("insert benchmark recipient");
    }
}

fn prepare_messages_model(message_count: usize) -> (MailAppModel, TempDir) {
    let tmp = TempDir::new().expect("create temp dir for message benchmark");
    let db_path = tmp.path().join("messages_bench.sqlite3");
    init_messages_bench_db(&db_path, message_count);

    let config = Config {
        database_url: format!("sqlite:///{}", db_path.display()),
        ..Config::default()
    };

    let state = TuiSharedState::new(&config);
    let mut model = MailAppModel::new(state);
    let _ = model.update(MailMsg::SwitchScreen(MailScreenId::Messages));
    let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
        '/',
    )))));
    for ch in "alert".chars() {
        let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
            ch,
        )))));
    }
    let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
    let _ = model.update(MailMsg::Terminal(Event::Tick));
    render_model(&model, 180, 48);

    (model, tmp)
}

fn build_heatmap_data(rows: usize, cols: usize) -> Vec<Vec<f64>> {
    (0..rows)
        .map(|r| {
            (0..cols)
                .map(|c| {
                    let mix = u32::try_from((r.saturating_mul(31) + c.saturating_mul(17)) % 100)
                        .unwrap_or(0);
                    f64::from(mix) / 100.0
                })
                .collect()
        })
        .collect()
}

/// Benchmark: 100 Bayesian strategy decisions on stable frames.
fn bench_frame_bayesian_stable(c: &mut Criterion) {
    c.bench_function("bayesian_100_stable_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            for _ in 0..100 {
                let action = strategy.observe_with_ledger(&stable_frame(), None);
                black_box(action);
            }
            black_box(strategy.posterior())
        });
    });
}

/// Benchmark: 100 frames with mixed conditions (cycling through all 4 states).
fn bench_frame_bayesian_mixed(c: &mut Criterion) {
    let frames = [
        stable_frame(),
        bursty_frame(),
        resize_frame(),
        degraded_frame(),
    ];
    c.bench_function("bayesian_100_mixed_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            for i in 0..100 {
                let action = strategy.observe_with_ledger(&frames[i % 4], None);
                black_box(action);
            }
            black_box(strategy.posterior())
        });
    });
}

/// Benchmark: baseline always-full (deterministic fallback) for comparison.
fn bench_frame_full_baseline(c: &mut Criterion) {
    c.bench_function("full_baseline_100_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            strategy.deterministic_fallback = true;
            for _ in 0..100 {
                let action = strategy.observe_with_ledger(&stable_frame(), None);
                black_box(action);
            }
        });
    });
}

/// Benchmark: 1000 frames to measure throughput at scale.
fn bench_frame_bayesian_1000(c: &mut Criterion) {
    let frames = [
        stable_frame(),
        bursty_frame(),
        resize_frame(),
        degraded_frame(),
    ];
    c.bench_function("bayesian_1000_mixed_frames", |b| {
        b.iter(|| {
            let mut strategy = BayesianDiffStrategy::new();
            for i in 0..1000 {
                let action = strategy.observe_with_ledger(&frames[i % 4], None);
                black_box(action);
            }
            black_box(strategy.posterior())
        });
    });
}

/// Benchmark: dashboard frame render with 10k events in the ring buffer.
fn bench_dashboard_frame_10k_events(c: &mut Criterion) {
    let config = Config::default();
    let state = TuiSharedState::with_event_capacity(&config, 12_000);
    populate_event_ring(&state, 10_000);
    populate_dashboard_stats(&state);
    let model = make_model(Arc::clone(&state), MailScreenId::Dashboard);
    render_model(&model, 180, 48);

    c.bench_function("dashboard_frame_10k_events_180x48", |b| {
        b.iter(|| render_model(&model, 180, 48));
    });
}

/// Benchmark: timeline frame render with 10k events in the ring buffer.
fn bench_timeline_frame_10k_events(c: &mut Criterion) {
    let config = Config::default();
    let state = TuiSharedState::with_event_capacity(&config, 12_000);
    populate_event_ring(&state, 10_000);
    let mut model = make_model(Arc::clone(&state), MailScreenId::Timeline);
    let _ = model.update(MailMsg::Terminal(Event::Tick));
    render_model(&model, 180, 48);

    c.bench_function("timeline_frame_10k_events_180x48", |b| {
        b.iter(|| render_model(&model, 180, 48));
    });
}

/// Benchmark: messages frame render with 1000 query results and markdown bodies.
fn bench_messages_frame_1000_results(c: &mut Criterion) {
    let (model, tmp) = prepare_messages_model(1_000);
    c.bench_function("messages_frame_1000_results_180x48", |b| {
        b.iter(|| render_model(&model, 180, 48));
    });
    black_box(tmp.path());
}

/// Benchmark: full-density heatmap render.
fn bench_heatmap_full_density(c: &mut Criterion) {
    let data = build_heatmap_data(40, 120);
    let area = Rect::new(0, 0, 160, 48);
    c.bench_function("heatmap_full_density_40x120_160x48", |b| {
        b.iter(|| {
            let widget = HeatmapGrid::new(&data).data_generation(1);
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(area.width, area.height, &mut pool);
            widget.render(area, &mut frame);
            black_box(widget.layout_cache().compute_count);
        });
    });
}

/// Benchmark: ambient effect renderer in subtle mode.
fn bench_ambient_renderer(c: &mut Criterion) {
    let area = Rect::new(0, 0, 160, 48);
    let mut renderer = AmbientEffectRenderer::new();
    let health = AmbientHealthInput {
        critical_alerts_active: false,
        failed_probe_count: 0,
        total_probe_count: 5,
        event_buffer_utilization: 0.35,
        seconds_since_last_event: 3,
    };
    let mut uptime_s = 0.0_f64;

    c.bench_function("ambient_renderer_subtle_160x48", |b| {
        b.iter(|| {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(area.width, area.height, &mut pool);
            let telemetry =
                renderer.render(area, &mut frame, AmbientMode::Subtle, health, uptime_s);
            uptime_s += 0.016;
            black_box(telemetry.render_duration);
        });
    });
}

/// Benchmark: theme switching between two canonical themes.
fn bench_theme_switch(c: &mut Criterion) {
    let _ = tui_theme::set_theme_and_get_name(ThemeId::CyberpunkAurora);
    c.bench_function("theme_switch_darcula_cyberpunk_pair", |b| {
        b.iter(|| {
            let first = tui_theme::set_theme_and_get_name(ThemeId::Darcula);
            let second = tui_theme::set_theme_and_get_name(ThemeId::CyberpunkAurora);
            black_box((first, second));
        });
    });
}

criterion_group!(
    benches,
    bench_frame_bayesian_stable,
    bench_frame_bayesian_mixed,
    bench_frame_full_baseline,
    bench_frame_bayesian_1000,
    bench_dashboard_frame_10k_events,
    bench_timeline_frame_10k_events,
    bench_messages_frame_1000_results,
    bench_heatmap_full_density,
    bench_ambient_renderer,
    bench_theme_switch,
);

criterion_main!(benches);
