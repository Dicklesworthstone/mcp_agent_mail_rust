//! br-3vwi.9.1: TUI V2 performance baselines and CI budgets.
//!
//! Measures cold-start model creation, per-tick update latency, per-screen
//! render latency, screen switching cost, and full-app render cost.
//! Results are saved as structured JSON artifacts under
//! `tests/artifacts/tui/perf_baselines/`.
//!
//! Run with budget enforcement:
//! ```
//! MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
//!   cargo test -p mcp-agent-mail-server --test tui_perf_baselines --release
//! ```

#![forbid(unsafe_code)]
#![allow(
    clippy::print_literal,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ftui::{Event, Frame, GraphemePool, KeyCode, KeyEvent};
use ftui_harness::Rect;
use ftui_runtime::program::Model;

use mcp_agent_mail_core::Config;
use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_screens::{
    ALL_SCREEN_IDS, MailScreen, MailScreenId, agents::AgentsScreen, analytics::AnalyticsScreen,
    contacts::ContactsScreen, dashboard::DashboardScreen, explorer::MailExplorerScreen,
    messages::MessageBrowserScreen, projects::ProjectsScreen, reservations::ReservationsScreen,
    search::SearchCockpitScreen, system_health::SystemHealthScreen, threads::ThreadExplorerScreen,
    timeline::TimelineScreen, tool_metrics::ToolMetricsScreen,
};

// ── Budget constants (microseconds) ──────────────────────────────────

/// Model creation budget: p95 < 100ms.
/// This is a one-time startup cost; includes 13 screen allocations,
/// command palette construction, and keymap initialization.
/// Cold-cache effects dominate early iterations; warm p50 is sub-ms.
const BUDGET_MODEL_INIT_P95_US: u64 = 100_000;

/// Single tick update budget: p95 < 2ms.
const BUDGET_TICK_UPDATE_P95_US: u64 = 2_000;

/// Single screen render budget: p95 < 10ms.
/// Generous to accommodate complex screens (search, threads).
const BUDGET_SCREEN_RENDER_P95_US: u64 = 10_000;

/// Full-app render (chrome + screen + overlays) budget: p95 < 15ms.
const BUDGET_APP_RENDER_P95_US: u64 = 15_000;

/// Screen switch + re-render budget: p95 < 2ms.
const BUDGET_SCREEN_SWITCH_P95_US: u64 = 2_000;

/// Full tick cycle (update + render) budget: p95 < 20ms.
/// Must stay well under the 100ms tick interval for smooth 10fps.
const BUDGET_TICK_CYCLE_P95_US: u64 = 20_000;

// ── Artifact types ───────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
struct PerfSample {
    surface: String,
    detail: String,
    iterations: usize,
    warmup: usize,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    budget_p95_us: u64,
    within_budget: bool,
}

#[derive(Debug, serde::Serialize)]
struct TuiBaselineReport {
    generated_at: String,
    agent: &'static str,
    bead: &'static str,
    build_profile: &'static str,
    samples: Vec<PerfSample>,
    all_within_budget: bool,
}

// ── Helpers ──────────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn artifacts_dir() -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let dir = repo_root().join(format!(
        "tests/artifacts/tui/perf_baselines/{ts}_{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn save_report(report: &TuiBaselineReport) {
    let dir = artifacts_dir();
    let path = dir.join("summary.json");
    let json = serde_json::to_string_pretty(report).unwrap_or_default();
    let _ = std::fs::write(&path, &json);
    eprintln!("artifact: {}", path.display());
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn enforce_budgets() -> bool {
    std::env::var("MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn test_state() -> Arc<TuiSharedState> {
    let config = Config::default();
    TuiSharedState::new(&config)
}

/// Measure a closure over `iterations` runs (after `warmup` warm-up runs).
/// Returns sorted latencies in microseconds.
fn measure<F: FnMut()>(warmup: usize, iterations: usize, mut f: F) -> Vec<u64> {
    for _ in 0..warmup {
        f();
    }
    let mut latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        #[allow(clippy::cast_possible_truncation)]
        latencies.push(start.elapsed().as_micros() as u64);
    }
    latencies.sort_unstable();
    latencies
}

fn make_sample(
    surface: &str,
    detail: &str,
    warmup: usize,
    iterations: usize,
    sorted: &[u64],
    budget_p95_us: u64,
) -> PerfSample {
    let p95 = percentile(sorted, 95.0);
    PerfSample {
        surface: surface.to_string(),
        detail: detail.to_string(),
        iterations,
        warmup,
        p50_us: percentile(sorted, 50.0),
        p95_us: p95,
        p99_us: percentile(sorted, 99.0),
        max_us: sorted.last().copied().unwrap_or(0),
        budget_p95_us,
        within_budget: p95 < budget_p95_us,
    }
}

const fn screen_name(id: MailScreenId) -> &'static str {
    match id {
        MailScreenId::Dashboard => "Dashboard",
        MailScreenId::Messages => "Messages",
        MailScreenId::Threads => "Threads",
        MailScreenId::Search => "Search",
        MailScreenId::Agents => "Agents",
        MailScreenId::Reservations => "Reservations",
        MailScreenId::ToolMetrics => "ToolMetrics",
        MailScreenId::SystemHealth => "SystemHealth",
        MailScreenId::Timeline => "Timeline",
        MailScreenId::Projects => "Projects",
        MailScreenId::Contacts => "Contacts",
        MailScreenId::Explorer => "Explorer",
        MailScreenId::Analytics => "Analytics",
    }
}

fn new_screen(id: MailScreenId, state: &Arc<TuiSharedState>) -> Box<dyn MailScreen> {
    match id {
        MailScreenId::Dashboard => Box::new(DashboardScreen::new()),
        MailScreenId::Messages => Box::new(MessageBrowserScreen::new()),
        MailScreenId::Threads => Box::new(ThreadExplorerScreen::new()),
        MailScreenId::Search => Box::new(SearchCockpitScreen::new()),
        MailScreenId::Agents => Box::new(AgentsScreen::new()),
        MailScreenId::Reservations => Box::new(ReservationsScreen::new()),
        MailScreenId::ToolMetrics => Box::new(ToolMetricsScreen::new()),
        MailScreenId::SystemHealth => Box::new(SystemHealthScreen::new(Arc::clone(state))),
        MailScreenId::Timeline => Box::new(TimelineScreen::new()),
        MailScreenId::Projects => Box::new(ProjectsScreen::new()),
        MailScreenId::Contacts => Box::new(ContactsScreen::new()),
        MailScreenId::Explorer => Box::new(MailExplorerScreen::new()),
        MailScreenId::Analytics => Box::new(AnalyticsScreen::new()),
    }
}

// ── Tests ────────────────────────────────────────────────────────────

/// PERF-TUI-1: Model cold-start initialization.
/// Measures `MailAppModel::new()` which creates all 13 screens, palette, etc.
#[test]
fn perf_model_init() {
    let warmup = 3;
    let iterations = 50;
    let state = test_state();

    let sorted = measure(warmup, iterations, || {
        let _model = MailAppModel::new(Arc::clone(&state));
    });

    let sample = make_sample(
        "model_init",
        "MailAppModel::new() cold start",
        warmup,
        iterations,
        &sorted,
        BUDGET_MODEL_INIT_P95_US,
    );

    eprintln!(
        "model_init: p50={:.1}ms p95={:.1}ms p99={:.1}ms budget={:.1}ms {}",
        sample.p50_us as f64 / 1000.0,
        sample.p95_us as f64 / 1000.0,
        sample.p99_us as f64 / 1000.0,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "model_init p95 {:.1}ms exceeds budget {:.1}ms",
            sample.p95_us as f64 / 1000.0,
            sample.budget_p95_us as f64 / 1000.0,
        );
    }
}

/// PERF-TUI-2: Tick update latency (no rendering, just model.update(Tick)).
#[test]
fn perf_tick_update() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let warmup = 10;
    let iterations = 200;

    let sorted = measure(warmup, iterations, || {
        let _ = model.update(MailMsg::Terminal(Event::Tick));
    });

    let sample = make_sample(
        "tick_update",
        "model.update(Event::Tick)",
        warmup,
        iterations,
        &sorted,
        BUDGET_TICK_UPDATE_P95_US,
    );

    eprintln!(
        "tick_update: p50={:.1}ms p95={:.1}ms p99={:.1}ms budget={:.1}ms {}",
        sample.p50_us as f64 / 1000.0,
        sample.p95_us as f64 / 1000.0,
        sample.p99_us as f64 / 1000.0,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "tick_update p95 {:.1}ms exceeds budget {:.1}ms",
            sample.p95_us as f64 / 1000.0,
            sample.budget_p95_us as f64 / 1000.0,
        );
    }
}

/// PERF-TUI-3: Per-screen render latency at 80x24.
/// Renders each screen individually into a headless frame.
#[test]
fn perf_screen_render_80x24() {
    let state = test_state();
    let warmup = 5;
    let iterations = 100;
    let width: u16 = 80;
    let height: u16 = 24;

    let mut all_within = true;

    for &id in ALL_SCREEN_IDS {
        let screen = new_screen(id, &state);
        let area = Rect::new(0, 0, width, height);

        let sorted = measure(warmup, iterations, || {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            screen.view(&mut frame, area, &state);
        });

        let sample = make_sample(
            "screen_render_80x24",
            screen_name(id),
            warmup,
            iterations,
            &sorted,
            BUDGET_SCREEN_RENDER_P95_US,
        );

        eprintln!(
            "  {:<16} p50={:>6.1}µs p95={:>6.1}µs p99={:>6.1}µs {}",
            screen_name(id),
            sample.p50_us as f64,
            sample.p95_us as f64,
            sample.p99_us as f64,
            if sample.within_budget { "OK" } else { "OVER" },
        );

        if !sample.within_budget {
            all_within = false;
        }
    }

    if enforce_budgets() {
        assert!(
            all_within,
            "one or more screens exceeded render budget at 80x24"
        );
    }
}

/// PERF-TUI-4: Per-screen render latency at 120x40 (larger terminal).
#[test]
fn perf_screen_render_120x40() {
    let state = test_state();
    let warmup = 5;
    let iterations = 100;
    let width: u16 = 120;
    let height: u16 = 40;

    let mut all_within = true;

    for &id in ALL_SCREEN_IDS {
        let screen = new_screen(id, &state);
        let area = Rect::new(0, 0, width, height);

        let sorted = measure(warmup, iterations, || {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            screen.view(&mut frame, area, &state);
        });

        let sample = make_sample(
            "screen_render_120x40",
            screen_name(id),
            warmup,
            iterations,
            &sorted,
            BUDGET_SCREEN_RENDER_P95_US,
        );

        eprintln!(
            "  {:<16} p50={:>6.1}µs p95={:>6.1}µs p99={:>6.1}µs {}",
            screen_name(id),
            sample.p50_us as f64,
            sample.p95_us as f64,
            sample.p99_us as f64,
            if sample.within_budget { "OK" } else { "OVER" },
        );

        if !sample.within_budget {
            all_within = false;
        }
    }

    if enforce_budgets() {
        assert!(
            all_within,
            "one or more screens exceeded render budget at 120x40"
        );
    }
}

/// PERF-TUI-5: Full app render (tab bar + active screen + status + overlays).
#[test]
fn perf_app_render() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();
    // Process a few ticks to warm internal state
    for _ in 0..5 {
        let _ = model.update(MailMsg::Terminal(Event::Tick));
    }

    let warmup = 5;
    let iterations = 100;
    let width: u16 = 120;
    let height: u16 = 40;

    let sorted = measure(warmup, iterations, || {
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);
    });

    let sample = make_sample(
        "app_render",
        "full app render 120x40 (Dashboard)",
        warmup,
        iterations,
        &sorted,
        BUDGET_APP_RENDER_P95_US,
    );

    eprintln!(
        "app_render: p50={:.1}µs p95={:.1}µs p99={:.1}µs budget={:.1}ms {}",
        sample.p50_us as f64,
        sample.p95_us as f64,
        sample.p99_us as f64,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "app_render p95 {:.1}µs exceeds budget {:.1}ms",
            sample.p95_us as f64,
            sample.budget_p95_us as f64 / 1000.0,
        );
    }
}

/// PERF-TUI-6: Screen switch cost (Tab key → re-render).
#[test]
fn perf_screen_switch() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let warmup = 5;
    let iterations = 100;
    let width: u16 = 120;
    let height: u16 = 40;
    let tab_event = Event::Key(KeyEvent::new(KeyCode::Tab));

    let sorted = measure(warmup, iterations, || {
        let _ = model.update(MailMsg::Terminal(tab_event.clone()));
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);
    });

    let sample = make_sample(
        "screen_switch",
        "Tab + re-render cycle",
        warmup,
        iterations,
        &sorted,
        BUDGET_SCREEN_SWITCH_P95_US,
    );

    eprintln!(
        "screen_switch: p50={:.1}µs p95={:.1}µs p99={:.1}µs budget={:.1}ms {}",
        sample.p50_us as f64,
        sample.p95_us as f64,
        sample.p99_us as f64,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "screen_switch p95 {:.1}µs exceeds budget {:.1}ms",
            sample.p95_us as f64,
            sample.budget_p95_us as f64 / 1000.0,
        );
    }
}

/// PERF-TUI-7: Full tick cycle (update + render) — the critical path.
/// Must stay well under the 100ms tick interval for smooth rendering.
#[test]
fn perf_tick_cycle() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let warmup = 10;
    let iterations = 200;
    let width: u16 = 120;
    let height: u16 = 40;

    let sorted = measure(warmup, iterations, || {
        let _ = model.update(MailMsg::Terminal(Event::Tick));
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);
    });

    let sample = make_sample(
        "tick_cycle",
        "update(Tick) + view() @ 120x40",
        warmup,
        iterations,
        &sorted,
        BUDGET_TICK_CYCLE_P95_US,
    );

    eprintln!(
        "tick_cycle: p50={:.1}µs p95={:.1}µs p99={:.1}µs budget={:.1}ms {}",
        sample.p50_us as f64,
        sample.p95_us as f64,
        sample.p99_us as f64,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "tick_cycle p95 {:.1}µs exceeds budget {:.1}ms (100ms tick interval)",
            sample.p95_us as f64,
            sample.budget_p95_us as f64 / 1000.0,
        );
    }
}

/// PERF-TUI-8: Command palette open + type + execute cycle.
#[test]
fn perf_palette_cycle() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let warmup = 3;
    let iterations = 50;

    let ctrl_p =
        Event::Key(KeyEvent::new(KeyCode::Char('p')).with_modifiers(ftui::Modifiers::CTRL));
    let esc = Event::Key(KeyEvent::new(KeyCode::Escape));

    let sorted = measure(warmup, iterations, || {
        // Open palette
        let _ = model.update(MailMsg::Terminal(ctrl_p.clone()));
        // Type a character
        let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
            'm',
        )))));
        // Dismiss
        let _ = model.update(MailMsg::Terminal(esc.clone()));
    });

    let sample = make_sample(
        "palette_cycle",
        "Ctrl+P → type → Esc",
        warmup,
        iterations,
        &sorted,
        BUDGET_SCREEN_SWITCH_P95_US,
    );

    eprintln!(
        "palette_cycle: p50={:.1}µs p95={:.1}µs p99={:.1}µs budget={:.1}ms {}",
        sample.p50_us as f64,
        sample.p95_us as f64,
        sample.p99_us as f64,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "palette_cycle p95 {:.1}µs exceeds budget",
            sample.p95_us as f64,
        );
    }
}

/// PERF-TUI-9: Search screen interaction cycle.
/// Simulates switching to the Search screen, typing a query character,
/// pressing Enter, then rendering — the hot path for interactive search.
#[test]
fn perf_search_interaction() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    // Navigate to the Search screen via Tab cycling until we get there.
    let tab_event = Event::Key(KeyEvent::new(KeyCode::Tab));
    for _ in 0..ALL_SCREEN_IDS.len() {
        if model.active_screen() == MailScreenId::Search {
            break;
        }
        let _ = model.update(MailMsg::Terminal(tab_event.clone()));
    }

    let warmup = 5;
    let iterations = 100;
    let width: u16 = 120;
    let height: u16 = 40;

    let char_a = Event::Key(KeyEvent::new(KeyCode::Char('a')));
    let enter = Event::Key(KeyEvent::new(KeyCode::Enter));
    let slash = Event::Key(KeyEvent::new(KeyCode::Char('/')));

    let sorted = measure(warmup, iterations, || {
        // Open query bar (/ focuses it)
        let _ = model.update(MailMsg::Terminal(slash.clone()));
        // Type a character
        let _ = model.update(MailMsg::Terminal(char_a.clone()));
        // Submit search
        let _ = model.update(MailMsg::Terminal(enter.clone()));
        // Render
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);
    });

    let sample = make_sample(
        "search_interaction",
        "Search: / → type → Enter → render",
        warmup,
        iterations,
        &sorted,
        BUDGET_SCREEN_SWITCH_P95_US,
    );

    eprintln!(
        "search_interaction: p50={:.1}µs p95={:.1}µs p99={:.1}µs budget={:.1}ms {}",
        sample.p50_us as f64,
        sample.p95_us as f64,
        sample.p99_us as f64,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "search_interaction p95 {:.1}µs exceeds budget",
            sample.p95_us as f64,
        );
    }
}

/// PERF-TUI-10: Rapid key navigation (arrow keys scrolling through list).
/// Simulates Up/Down key presses on the Messages screen to measure
/// navigation responsiveness.
#[test]
fn perf_key_navigation() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let warmup = 5;
    let iterations = 200;
    let width: u16 = 120;
    let height: u16 = 40;

    let down = Event::Key(KeyEvent::new(KeyCode::Down));
    let up = Event::Key(KeyEvent::new(KeyCode::Up));

    let sorted = measure(warmup, iterations, || {
        let _ = model.update(MailMsg::Terminal(down.clone()));
        let _ = model.update(MailMsg::Terminal(up.clone()));
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);
    });

    let sample = make_sample(
        "key_navigation",
        "Down + Up + render",
        warmup,
        iterations,
        &sorted,
        BUDGET_SCREEN_SWITCH_P95_US,
    );

    eprintln!(
        "key_navigation: p50={:.1}µs p95={:.1}µs p99={:.1}µs budget={:.1}ms {}",
        sample.p50_us as f64,
        sample.p95_us as f64,
        sample.p99_us as f64,
        sample.budget_p95_us as f64 / 1000.0,
        if sample.within_budget { "OK" } else { "OVER" },
    );

    if enforce_budgets() {
        assert!(
            sample.within_budget,
            "key_navigation p95 {:.1}µs exceeds budget",
            sample.p95_us as f64,
        );
    }
}

/// PERF-TUI-REPORT: Aggregated baseline report (emits JSON artifact).
/// This test runs last and produces the summary artifact.
#[test]
fn z_perf_baseline_report() {
    let state = test_state();
    let width: u16 = 120;
    let height: u16 = 40;
    let warmup = 5;
    let iterations = 50;

    let mut samples = Vec::new();

    // Model init
    {
        let sorted = measure(warmup, iterations, || {
            let _model = MailAppModel::new(Arc::clone(&state));
        });
        samples.push(make_sample(
            "model_init",
            "MailAppModel::new()",
            warmup,
            iterations,
            &sorted,
            BUDGET_MODEL_INIT_P95_US,
        ));
    }

    // Tick update
    {
        let mut model = MailAppModel::new(Arc::clone(&state));
        let _ = model.init();
        let sorted = measure(warmup, iterations, || {
            let _ = model.update(MailMsg::Terminal(Event::Tick));
        });
        samples.push(make_sample(
            "tick_update",
            "update(Tick)",
            warmup,
            iterations,
            &sorted,
            BUDGET_TICK_UPDATE_P95_US,
        ));
    }

    // Per-screen renders
    for &id in ALL_SCREEN_IDS {
        let screen = new_screen(id, &state);
        let area = Rect::new(0, 0, width, height);
        let sorted = measure(warmup, iterations, || {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            screen.view(&mut frame, area, &state);
        });
        samples.push(make_sample(
            "screen_render",
            screen_name(id),
            warmup,
            iterations,
            &sorted,
            BUDGET_SCREEN_RENDER_P95_US,
        ));
    }

    // Full app render
    {
        let mut model = MailAppModel::new(Arc::clone(&state));
        let _ = model.init();
        for _ in 0..5 {
            let _ = model.update(MailMsg::Terminal(Event::Tick));
        }
        let sorted = measure(warmup, iterations, || {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            model.view(&mut frame);
        });
        samples.push(make_sample(
            "app_render",
            "full app 120x40",
            warmup,
            iterations,
            &sorted,
            BUDGET_APP_RENDER_P95_US,
        ));
    }

    // Tick cycle
    {
        let mut model = MailAppModel::new(Arc::clone(&state));
        let _ = model.init();
        let sorted = measure(warmup, iterations, || {
            let _ = model.update(MailMsg::Terminal(Event::Tick));
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            model.view(&mut frame);
        });
        samples.push(make_sample(
            "tick_cycle",
            "update+view 120x40",
            warmup,
            iterations,
            &sorted,
            BUDGET_TICK_CYCLE_P95_US,
        ));
    }

    // Search interaction
    {
        let mut model = MailAppModel::new(Arc::clone(&state));
        let _ = model.init();
        let tab_event = Event::Key(KeyEvent::new(KeyCode::Tab));
        for _ in 0..ALL_SCREEN_IDS.len() {
            if model.active_screen() == MailScreenId::Search {
                break;
            }
            let _ = model.update(MailMsg::Terminal(tab_event.clone()));
        }
        let slash = Event::Key(KeyEvent::new(KeyCode::Char('/')));
        let char_a = Event::Key(KeyEvent::new(KeyCode::Char('a')));
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter));
        let sorted = measure(warmup, iterations, || {
            let _ = model.update(MailMsg::Terminal(slash.clone()));
            let _ = model.update(MailMsg::Terminal(char_a.clone()));
            let _ = model.update(MailMsg::Terminal(enter.clone()));
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            model.view(&mut frame);
        });
        samples.push(make_sample(
            "search_interaction",
            "/ → type → Enter → render",
            warmup,
            iterations,
            &sorted,
            BUDGET_SCREEN_SWITCH_P95_US,
        ));
    }

    // Key navigation
    {
        let mut model = MailAppModel::new(Arc::clone(&state));
        let _ = model.init();
        let down = Event::Key(KeyEvent::new(KeyCode::Down));
        let up = Event::Key(KeyEvent::new(KeyCode::Up));
        let sorted = measure(warmup, iterations, || {
            let _ = model.update(MailMsg::Terminal(down.clone()));
            let _ = model.update(MailMsg::Terminal(up.clone()));
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(width, height, &mut pool);
            model.view(&mut frame);
        });
        samples.push(make_sample(
            "key_navigation",
            "Down + Up + render",
            warmup,
            iterations,
            &sorted,
            BUDGET_SCREEN_SWITCH_P95_US,
        ));
    }

    let all_within = samples.iter().all(|s| s.within_budget);

    let report = TuiBaselineReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        agent: "RubyPrairie",
        bead: "br-3vwi.9.1",
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        samples,
        all_within_budget: all_within,
    };

    // Print summary table
    eprintln!("\n═══ TUI Performance Baselines ═══");
    eprintln!(
        "{:<24} {:>8} {:>8} {:>8} {:>8} {}",
        "Surface", "p50µs", "p95µs", "p99µs", "Budget", "Status"
    );
    eprintln!("{}", "─".repeat(76));
    for s in &report.samples {
        let label = if s.surface == "screen_render" {
            format!("  {}", s.detail)
        } else {
            s.surface.clone()
        };
        eprintln!(
            "{:<24} {:>8} {:>8} {:>8} {:>7}ms {}",
            label,
            s.p50_us,
            s.p95_us,
            s.p99_us,
            s.budget_p95_us / 1000,
            if s.within_budget { "  OK" } else { "OVER" },
        );
    }
    eprintln!("{}", "─".repeat(76));
    eprintln!(
        "Overall: {}",
        if all_within {
            "ALL WITHIN BUDGET"
        } else {
            "BUDGET EXCEEDED"
        }
    );

    save_report(&report);

    if enforce_budgets() {
        assert!(all_within, "one or more TUI perf samples exceeded budget");
    }
}
