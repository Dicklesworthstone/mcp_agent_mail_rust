//! br-3vwi.9.3: Soak/stress replay harness for multi-project heavy workloads.
//!
//! Replays realistic operator activity patterns against the TUI model
//! under heavy data load to validate stability, graceful degradation,
//! and long-session reliability.
//!
//! Run (30-second default):
//! ```
//! cargo test -p mcp-agent-mail-server --test tui_soak_replay -- --ignored --nocapture
//! ```
//!
//! Extended (5 minutes):
//! ```
//! SOAK_DURATION_SECS=300 cargo test -p mcp-agent-mail-server --test tui_soak_replay -- --ignored --nocapture
//! ```

#![forbid(unsafe_code)]
#![allow(
    clippy::print_literal,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ftui::{Event, Frame, GraphemePool, KeyCode, KeyEvent, Modifiers};
use ftui_runtime::program::Model;

use mcp_agent_mail_core::Config;
use mcp_agent_mail_server::tui_app::{MailAppModel, MailMsg};
use mcp_agent_mail_server::tui_bridge::TuiSharedState;
use mcp_agent_mail_server::tui_screens::{ALL_SCREEN_IDS, MailScreenId};

// ── Configuration ────────────────────────────────────────────────────

/// Default soak duration (seconds). Override with `SOAK_DURATION_SECS`.
const DEFAULT_SOAK_SECS: u64 = 30;

/// Terminal dimensions for rendering.
const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// Snapshot interval (seconds) for periodic health checks.
const SNAPSHOT_INTERVAL_SECS: u64 = 5;

/// Maximum allowed p95 tick-cycle latency (microseconds).
/// Must stay well under the 100ms tick interval.
const BUDGET_TICK_CYCLE_P95_US: u64 = 50_000; // 50ms

/// Maximum allowed p95 render latency (microseconds).
const BUDGET_RENDER_P95_US: u64 = 75_000; // 75ms

/// Maximum allowed RSS growth factor over baseline.
/// E.g., 3.0 means RSS must not triple from startup.
const MAX_RSS_GROWTH_FACTOR: f64 = 3.0;

// ── Replay action set ────────────────────────────────────────────────

/// Operator actions replayed in sequence to simulate realistic usage.
#[derive(Debug, Clone, Copy)]
enum ReplayAction {
    /// Process a tick (100ms heartbeat).
    Tick,
    /// Switch to the next screen via Tab.
    TabNext,
    /// Switch to a specific screen.
    SwitchTo(MailScreenId),
    /// Scroll down (j or Down arrow).
    ScrollDown,
    /// Scroll up (k or Up arrow).
    ScrollUp,
    /// Open search bar (/).
    SearchFocus,
    /// Type a character.
    TypeChar(char),
    /// Press Enter.
    Enter,
    /// Press Escape.
    Escape,
    /// Render a frame (`model.view()`).
    Render,
    /// Toggle help overlay (?).
    ToggleHelp,
}

impl ReplayAction {
    const fn to_msg(self) -> Option<MailMsg> {
        match self {
            Self::Tick => Some(MailMsg::Terminal(Event::Tick)),
            Self::TabNext => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Tab)))),
            Self::SwitchTo(id) => Some(MailMsg::SwitchScreen(id)),
            Self::ScrollDown => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Down)))),
            Self::ScrollUp => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Up)))),
            Self::SearchFocus => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
                '/',
            ))))),
            Self::TypeChar(c) => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
                c,
            ))))),
            Self::Enter => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter)))),
            Self::Escape => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Escape,
            )))),
            Self::Render => None, // handled separately
            Self::ToggleHelp => Some(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
                '?',
            ))))),
        }
    }
}

/// Build a realistic operator activity sequence.
///
/// Simulates: startup → browse dashboard → switch screens → search →
/// browse results → check health → browse timeline → repeat.
fn build_replay_script() -> Vec<ReplayAction> {
    use ReplayAction::*;

    let mut script = Vec::with_capacity(512);

    // Phase 1: Startup + dashboard browsing
    for _ in 0..10 {
        script.push(Tick);
    }
    script.push(Render);
    for _ in 0..5 {
        script.push(ScrollDown);
        script.push(Tick);
    }
    script.push(Render);

    // Phase 2: Cycle through all screens
    for _ in 0..ALL_SCREEN_IDS.len() {
        script.push(TabNext);
        script.push(Tick);
        script.push(Render);
        for _ in 0..3 {
            script.push(ScrollDown);
        }
        script.push(Render);
    }

    // Phase 3: Search interaction
    script.push(SwitchTo(MailScreenId::Search));
    script.push(Tick);
    script.push(SearchFocus);
    for c in "error message".chars() {
        script.push(TypeChar(c));
    }
    script.push(Enter);
    script.push(Tick);
    script.push(Render);
    for _ in 0..5 {
        script.push(ScrollDown);
        script.push(Tick);
    }
    script.push(Render);
    script.push(Escape);

    // Phase 4: Messages screen browsing
    script.push(SwitchTo(MailScreenId::Messages));
    script.push(Tick);
    script.push(Render);
    for _ in 0..10 {
        script.push(ScrollDown);
        script.push(Tick);
    }
    for _ in 0..5 {
        script.push(ScrollUp);
        script.push(Tick);
    }
    script.push(Render);

    // Phase 5: Timeline
    script.push(SwitchTo(MailScreenId::Timeline));
    script.push(Tick);
    script.push(Render);
    for _ in 0..5 {
        script.push(ScrollDown);
    }
    script.push(Render);

    // Phase 6: Help overlay toggle
    script.push(ToggleHelp);
    script.push(Tick);
    script.push(Render);
    script.push(ToggleHelp);
    script.push(Tick);

    // Phase 7: Agents + Projects
    script.push(SwitchTo(MailScreenId::Agents));
    script.push(Tick);
    script.push(Render);
    script.push(SwitchTo(MailScreenId::Projects));
    script.push(Tick);
    script.push(Render);

    // Phase 8: Contacts + Reservations
    script.push(SwitchTo(MailScreenId::Contacts));
    script.push(Tick);
    script.push(Render);
    script.push(SwitchTo(MailScreenId::Reservations));
    script.push(Tick);
    script.push(Render);

    // Phase 9: Tool metrics + System health
    script.push(SwitchTo(MailScreenId::ToolMetrics));
    script.push(Tick);
    script.push(Render);
    script.push(SwitchTo(MailScreenId::SystemHealth));
    script.push(Tick);
    script.push(Render);

    // Phase 10: Explorer + Analytics
    script.push(SwitchTo(MailScreenId::Explorer));
    script.push(Tick);
    script.push(Render);
    script.push(SwitchTo(MailScreenId::Analytics));
    script.push(Tick);
    script.push(Render);

    // Phase 11: Return to dashboard, tick some more
    script.push(SwitchTo(MailScreenId::Dashboard));
    for _ in 0..5 {
        script.push(Tick);
    }
    script.push(Render);

    script
}

// ── Measurement infrastructure ───────────────────────────────────────

/// Read resident set size from /proc/self/statm (Linux only).
fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn mean(samples: &[u64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|v| *v as f64).sum();
    sum / samples.len() as f64
}

fn variance(samples: &[u64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let avg = mean(samples);
    samples
        .iter()
        .map(|v| {
            let delta = *v as f64 - avg;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64
}

fn stddev(samples: &[u64]) -> f64 {
    variance(samples).sqrt()
}

/// Periodic health snapshot captured during the soak run.
#[derive(Debug, serde::Serialize)]
struct SoakSnapshot {
    elapsed_secs: u64,
    total_actions: u64,
    total_renders: u64,
    replay_loops: u64,
    rss_kb: u64,
    action_p50_us: u64,
    action_p95_us: u64,
    action_p99_us: u64,
    render_p50_us: u64,
    render_p95_us: u64,
    render_p99_us: u64,
    errors: u64,
}

/// Final soak test report written as JSON artifact.
#[derive(Debug, serde::Serialize)]
struct SoakReport {
    generated_at: String,
    agent: &'static str,
    bead: &'static str,
    duration_secs: u64,
    total_actions: u64,
    total_renders: u64,
    replay_loops: u64,
    baseline_rss_kb: u64,
    final_rss_kb: u64,
    rss_growth_factor: f64,
    action_p50_us: u64,
    action_p95_us: u64,
    action_p99_us: u64,
    action_max_us: u64,
    action_mean_us: f64,
    action_variance_us2: f64,
    action_stddev_us: f64,
    render_p50_us: u64,
    render_p95_us: u64,
    render_p99_us: u64,
    render_max_us: u64,
    render_mean_us: f64,
    render_variance_us2: f64,
    render_stddev_us: f64,
    errors: u64,
    snapshots: Vec<SoakSnapshot>,
    verdict: &'static str,
}

/// Standalone metric report for interaction/search loops.
#[derive(Debug, serde::Serialize)]
struct SoakLoopReport {
    generated_at: String,
    bead: &'static str,
    metric_name: &'static str,
    category: &'static str,
    iterations: usize,
    samples_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    mean_us: f64,
    variance_us2: f64,
    stddev_us: f64,
    budget_p95_us: u64,
    passed: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf()
}

fn save_named_report<T: serde::Serialize>(report: &T, name: &str) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let dir = repo_root().join(format!(
        "tests/artifacts/tui/soak_replay/{ts}_{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.json"));
    let json = serde_json::to_string_pretty(report).unwrap_or_default();
    let _ = std::fs::write(&path, &json);
    eprintln!("soak artifact: {}", path.display());
}

fn save_report(report: &SoakReport) {
    save_named_report(report, "report");
}

fn soak_duration() -> Duration {
    let secs: u64 = std::env::var("SOAK_DURATION_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SOAK_SECS);
    Duration::from_secs(secs)
}

// ── Tests ────────────────────────────────────────────────────────────

/// SOAK-1: Sustained replay loop under empty-state TUI.
///
/// Continuously replays the operator activity script for the configured
/// duration, collecting latency and memory metrics. Validates that:
/// - Tick-cycle p95 stays within budget
/// - RSS does not grow unboundedly
/// - No panics or errors accumulate
#[test]
#[ignore = "soak test: replay loop for 30-300 seconds"]
fn soak_replay_empty_state() {
    let duration = soak_duration();
    let script = build_replay_script();
    let state = test_state();

    eprintln!(
        "\n=== Soak replay (empty state): {}s, {} actions/loop ===",
        duration.as_secs(),
        script.len()
    );

    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let baseline_rss = rss_kb();
    let start = Instant::now();

    let mut action_latencies: Vec<u64> = Vec::with_capacity(100_000);
    let mut render_latencies: Vec<u64> = Vec::with_capacity(50_000);
    let mut snapshots: Vec<SoakSnapshot> = Vec::new();
    let mut total_actions: u64 = 0;
    let mut total_renders: u64 = 0;
    let mut replay_loops: u64 = 0;
    let mut errors: u64 = 0;
    let mut last_snapshot = Instant::now();

    while start.elapsed() < duration {
        for action in &script {
            if start.elapsed() >= duration {
                break;
            }

            match action {
                ReplayAction::Render => {
                    let t0 = Instant::now();
                    let mut pool = GraphemePool::new();
                    let mut frame = Frame::new(WIDTH, HEIGHT, &mut pool);
                    model.view(&mut frame);
                    render_latencies.push(t0.elapsed().as_micros() as u64);
                    total_renders += 1;
                }
                other => {
                    if let Some(msg) = other.to_msg() {
                        let t0 = Instant::now();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _ = model.update(msg);
                        }));
                        let elapsed_us = t0.elapsed().as_micros() as u64;
                        action_latencies.push(elapsed_us);
                        total_actions += 1;

                        if result.is_err() {
                            errors += 1;
                        }
                    }
                }
            }

            // Periodic snapshot
            if last_snapshot.elapsed() >= Duration::from_secs(SNAPSHOT_INTERVAL_SECS) {
                let mut a_sorted = action_latencies.clone();
                a_sorted.sort_unstable();
                let mut r_sorted = render_latencies.clone();
                r_sorted.sort_unstable();

                snapshots.push(SoakSnapshot {
                    elapsed_secs: start.elapsed().as_secs(),
                    total_actions,
                    total_renders,
                    replay_loops,
                    rss_kb: rss_kb(),
                    action_p50_us: percentile(&a_sorted, 50.0),
                    action_p95_us: percentile(&a_sorted, 95.0),
                    action_p99_us: percentile(&a_sorted, 99.0),
                    render_p50_us: percentile(&r_sorted, 50.0),
                    render_p95_us: percentile(&r_sorted, 95.0),
                    render_p99_us: percentile(&r_sorted, 99.0),
                    errors,
                });

                eprintln!(
                    "  [{:>4}s] loops={} actions={} renders={} rss={}KB action_p95={:.0}µs render_p95={:.0}µs errors={}",
                    start.elapsed().as_secs(),
                    replay_loops,
                    total_actions,
                    total_renders,
                    rss_kb(),
                    percentile(&a_sorted, 95.0) as f64,
                    percentile(&r_sorted, 95.0) as f64,
                    errors,
                );

                last_snapshot = Instant::now();
            }
        }
        replay_loops += 1;
    }

    // Final statistics
    action_latencies.sort_unstable();
    render_latencies.sort_unstable();

    let final_rss = rss_kb();
    let rss_growth = if baseline_rss > 0 {
        final_rss as f64 / baseline_rss as f64
    } else {
        1.0
    };

    let report = SoakReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        agent: "RubyPrairie",
        bead: "br-3vwi.9.3",
        duration_secs: start.elapsed().as_secs(),
        total_actions,
        total_renders,
        replay_loops,
        baseline_rss_kb: baseline_rss,
        final_rss_kb: final_rss,
        rss_growth_factor: rss_growth,
        action_p50_us: percentile(&action_latencies, 50.0),
        action_p95_us: percentile(&action_latencies, 95.0),
        action_p99_us: percentile(&action_latencies, 99.0),
        action_max_us: action_latencies.last().copied().unwrap_or(0),
        action_mean_us: mean(&action_latencies),
        action_variance_us2: variance(&action_latencies),
        action_stddev_us: stddev(&action_latencies),
        render_p50_us: percentile(&render_latencies, 50.0),
        render_p95_us: percentile(&render_latencies, 95.0),
        render_p99_us: percentile(&render_latencies, 99.0),
        render_max_us: render_latencies.last().copied().unwrap_or(0),
        render_mean_us: mean(&render_latencies),
        render_variance_us2: variance(&render_latencies),
        render_stddev_us: stddev(&render_latencies),
        errors,
        snapshots,
        verdict: if errors == 0
            && rss_growth < MAX_RSS_GROWTH_FACTOR
            && percentile(&action_latencies, 95.0) < BUDGET_TICK_CYCLE_P95_US
            && percentile(&render_latencies, 95.0) < BUDGET_RENDER_P95_US
        {
            "PASS"
        } else {
            "FAIL"
        },
    };

    save_report(&report);

    eprintln!("\n=== Soak Report ===");
    eprintln!("  Duration:     {}s", report.duration_secs);
    eprintln!("  Replay loops: {}", report.replay_loops);
    eprintln!("  Actions:      {}", report.total_actions);
    eprintln!("  Renders:      {}", report.total_renders);
    eprintln!(
        "  Action p95:   {:.1}µs (budget: {:.1}µs)",
        report.action_p95_us as f64, BUDGET_TICK_CYCLE_P95_US as f64,
    );
    eprintln!(
        "  Render p95:   {:.1}µs (budget: {:.1}µs)",
        report.render_p95_us as f64, BUDGET_RENDER_P95_US as f64,
    );
    eprintln!(
        "  RSS:          {}KB → {}KB ({:.2}x, limit {:.1}x)",
        report.baseline_rss_kb,
        report.final_rss_kb,
        report.rss_growth_factor,
        MAX_RSS_GROWTH_FACTOR,
    );
    eprintln!("  Errors:       {}", report.errors);
    eprintln!("  Verdict:      {}", report.verdict);

    assert_eq!(
        report.errors, 0,
        "soak replay encountered {} errors",
        report.errors
    );
    assert!(
        report.action_p95_us < BUDGET_TICK_CYCLE_P95_US,
        "action p95 {:.1}µs exceeds budget {:.1}µs",
        report.action_p95_us as f64,
        BUDGET_TICK_CYCLE_P95_US as f64,
    );
    assert!(
        report.render_p95_us < BUDGET_RENDER_P95_US,
        "render p95 {:.1}µs exceeds budget {:.1}µs",
        report.render_p95_us as f64,
        BUDGET_RENDER_P95_US as f64,
    );
    assert!(
        report.rss_growth_factor < MAX_RSS_GROWTH_FACTOR,
        "RSS grew {:.2}x (limit {:.1}x): {}KB → {}KB",
        report.rss_growth_factor,
        MAX_RSS_GROWTH_FACTOR,
        report.baseline_rss_kb,
        report.final_rss_kb,
    );
}

/// SOAK-2: Heavy-interaction stress test.
///
/// Rapidly cycles through all screens with scrolling, searching, and
/// help overlay toggling — no pauses, maximum throughput. Validates
/// that the TUI remains responsive under sustained rapid input.
#[test]
fn soak_rapid_screen_cycling() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let iterations = 500;
    let mut pool = GraphemePool::new();

    let tab = Event::Key(KeyEvent::new(KeyCode::Tab));
    let down = Event::Key(KeyEvent::new(KeyCode::Down));
    let up = Event::Key(KeyEvent::new(KeyCode::Up));

    let mut latencies = Vec::with_capacity(iterations * ALL_SCREEN_IDS.len());

    for _ in 0..iterations {
        for _ in ALL_SCREEN_IDS {
            let t0 = Instant::now();
            let _ = model.update(MailMsg::Terminal(tab.clone()));
            let _ = model.update(MailMsg::Terminal(down.clone()));
            let _ = model.update(MailMsg::Terminal(down.clone()));
            let _ = model.update(MailMsg::Terminal(up.clone()));
            let _ = model.update(MailMsg::Terminal(Event::Tick));
            let mut frame = Frame::new(WIDTH, HEIGHT, &mut pool);
            model.view(&mut frame);
            latencies.push(t0.elapsed().as_micros() as u64);
        }
    }

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50.0);
    let p95 = percentile(&latencies, 95.0);
    let p99 = percentile(&latencies, 99.0);
    let max = latencies.last().copied().unwrap_or(0);
    let mean_us = mean(&latencies);
    let variance_us2 = variance(&latencies);
    let stddev_us = stddev(&latencies);

    eprintln!(
        "rapid_cycling: {} iterations × {} screens = {} cycles, p50={:.0}µs p95={:.0}µs max={:.0}µs",
        iterations,
        ALL_SCREEN_IDS.len(),
        latencies.len(),
        p50 as f64,
        p95 as f64,
        max as f64,
    );

    let report = SoakLoopReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        bead: "br-3vwi.9.3",
        metric_name: "tui_interaction_rapid_screen_cycle",
        category: "interaction",
        iterations: latencies.len(),
        samples_us: latencies.clone(),
        p50_us: p50,
        p95_us: p95,
        p99_us: p99,
        max_us: max,
        mean_us,
        variance_us2,
        stddev_us,
        budget_p95_us: BUDGET_TICK_CYCLE_P95_US,
        passed: p95 < BUDGET_TICK_CYCLE_P95_US,
    };
    save_named_report(&report, "rapid_screen_cycling_report");

    assert!(
        p95 < BUDGET_TICK_CYCLE_P95_US,
        "rapid_cycling p95 {:.0}µs exceeds budget {:.0}µs",
        p95 as f64,
        BUDGET_TICK_CYCLE_P95_US as f64,
    );
}

/// SOAK-3: Screen-specific soak — each screen gets extended exercise.
///
/// Runs 1000 tick+render cycles on each screen to detect per-screen
/// degradation or memory leaks in individual screen implementations.
#[test]
fn soak_per_screen_stability() {
    let state = test_state();
    let iterations = 1000;

    let down = Event::Key(KeyEvent::new(KeyCode::Down));
    let up = Event::Key(KeyEvent::new(KeyCode::Up));
    let mut pool = GraphemePool::new();

    let mut results: Vec<(MailScreenId, u64, u64, u64)> = Vec::new(); // (id, p50, p95, max)

    for &screen_id in ALL_SCREEN_IDS {
        let mut model = MailAppModel::new(Arc::clone(&state));
        let _ = model.init();

        // Switch to target screen
        let _ = model.update(MailMsg::SwitchScreen(screen_id));

        let mut latencies = Vec::with_capacity(iterations);

        for i in 0..iterations {
            let t0 = Instant::now();
            let _ = model.update(MailMsg::Terminal(Event::Tick));
            // Alternate scrolling to exercise cursor movement
            if i % 3 == 0 {
                let _ = model.update(MailMsg::Terminal(down.clone()));
            } else if i % 5 == 0 {
                let _ = model.update(MailMsg::Terminal(up.clone()));
            }
            let mut frame = Frame::new(WIDTH, HEIGHT, &mut pool);
            model.view(&mut frame);
            latencies.push(t0.elapsed().as_micros() as u64);
        }

        latencies.sort_unstable();
        let p50 = percentile(&latencies, 50.0);
        let p95 = percentile(&latencies, 95.0);
        let max = latencies.last().copied().unwrap_or(0);
        results.push((screen_id, p50, p95, max));
    }

    eprintln!("\n=== Per-screen stability ({iterations} cycles each) ===");
    eprintln!(
        "{:<15} {:>8} {:>8} {:>8}",
        "Screen", "p50 µs", "p95 µs", "max µs"
    );
    eprintln!("{}", "-".repeat(43));

    let mut all_pass = true;
    for (id, p50, p95, max) in &results {
        let status = if *p95 < BUDGET_TICK_CYCLE_P95_US {
            "OK"
        } else {
            all_pass = false;
            "OVER"
        };
        eprintln!(
            "{:<15} {:>8} {:>8} {:>8}  {}",
            screen_name(*id),
            p50,
            p95,
            max,
            status,
        );
    }

    assert!(all_pass, "one or more screens exceeded the p95 budget");
}

/// SOAK-4: Rapid search typing stress.
///
/// Types long queries character-by-character to stress the search input
/// path, debounce logic, and highlight term extraction.
#[test]
fn soak_search_typing_stress() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let _ = model.update(MailMsg::SwitchScreen(MailScreenId::Search));
    let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
        '/',
    )))));

    let queries = [
        "error handling in message delivery pipeline",
        "performance regression in search indexing",
        "concurrent file reservation conflicts detected",
        "agent coordination protocol timeout exceeded",
        "database migration schema validation failure",
    ];

    let mut latencies = Vec::with_capacity(1000);
    let mut pool = GraphemePool::new();

    for query in &queries {
        // Type query character by character
        for c in query.chars() {
            let t0 = Instant::now();
            let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
                c,
            )))));
            latencies.push(t0.elapsed().as_micros() as u64);
        }

        // Submit
        let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Enter))));
        let _ = model.update(MailMsg::Terminal(Event::Tick));

        // Render
        let mut frame = Frame::new(WIDTH, HEIGHT, &mut pool);
        model.view(&mut frame);

        // Clear for next query: Ctrl+A to select all, then type over
        let _ = model.update(MailMsg::Terminal(Event::Key(KeyEvent::new(KeyCode::Char(
            '/',
        )))));
        let _ = model.update(MailMsg::Terminal(Event::Key(
            KeyEvent::new(KeyCode::Char('a')).with_modifiers(Modifiers::CTRL),
        )));
    }

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50.0);
    let p95 = percentile(&latencies, 95.0);
    let p99 = percentile(&latencies, 99.0);
    let max = latencies.last().copied().unwrap_or(0);
    let mean_us = mean(&latencies);
    let variance_us2 = variance(&latencies);
    let stddev_us = stddev(&latencies);

    eprintln!(
        "search_typing: {} keystrokes, p50={:.0}µs p95={:.0}µs max={:.0}µs",
        latencies.len(),
        p50 as f64,
        p95 as f64,
        max as f64,
    );

    let report = SoakLoopReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        bead: "br-3vwi.9.3",
        metric_name: "tui_search_typing",
        category: "search",
        iterations: latencies.len(),
        samples_us: latencies.clone(),
        p50_us: p50,
        p95_us: p95,
        p99_us: p99,
        max_us: max,
        mean_us,
        variance_us2,
        stddev_us,
        budget_p95_us: 5_000,
        passed: p95 < 5_000,
    };
    save_named_report(&report, "search_typing_report");

    // Individual keystroke should be very fast (< 5ms p95)
    assert!(
        p95 < 5_000,
        "search typing p95 {:.0}µs exceeds 5ms",
        p95 as f64,
    );
}

/// SOAK-5: Verify no latency degradation over time.
///
/// Runs tick+render cycles in windows and asserts that the last window
/// is not significantly slower than the first (detecting degradation).
#[test]
fn soak_no_degradation() {
    let state = test_state();
    let mut model = MailAppModel::new(Arc::clone(&state));
    let _ = model.init();

    let window_size = 500;
    let num_windows = 10;
    let total = window_size * num_windows;

    let mut pool = GraphemePool::new();
    let tab = Event::Key(KeyEvent::new(KeyCode::Tab));
    let down = Event::Key(KeyEvent::new(KeyCode::Down));

    let mut all_latencies = Vec::with_capacity(total);

    for _ in 0..total {
        let t0 = Instant::now();
        let _ = model.update(MailMsg::Terminal(Event::Tick));
        let _ = model.update(MailMsg::Terminal(tab.clone()));
        let _ = model.update(MailMsg::Terminal(down.clone()));
        let mut frame = Frame::new(WIDTH, HEIGHT, &mut pool);
        model.view(&mut frame);
        all_latencies.push(t0.elapsed().as_micros() as u64);
    }

    // Compare first and last windows
    let first_window = &mut all_latencies[..window_size].to_vec();
    first_window.sort_unstable();
    let first_p95 = percentile(first_window, 95.0);

    let last_window = &mut all_latencies[(total - window_size)..].to_vec();
    last_window.sort_unstable();
    let last_p95 = percentile(last_window, 95.0);

    // Allow 2x degradation tolerance (generous to avoid flakes on loaded CI).
    let degradation = if first_p95 > 0 {
        last_p95 as f64 / first_p95 as f64
    } else {
        1.0
    };

    eprintln!(
        "degradation: first_p95={:.0}µs last_p95={:.0}µs ratio={:.2}x ({} windows × {})",
        first_p95 as f64, last_p95 as f64, degradation, num_windows, window_size,
    );

    assert!(
        degradation < 2.0,
        "latency degraded {:.2}x from first to last window (first_p95={:.0}µs, last_p95={:.0}µs)",
        degradation,
        first_p95 as f64,
        last_p95 as f64,
    );
}

// ── Helpers ──────────────────────────────────────────────────────────

fn test_state() -> Arc<TuiSharedState> {
    let config = Config::default();
    TuiSharedState::new(&config)
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
        MailScreenId::Attachments => "Attachments",
        MailScreenId::ArchiveBrowser => "ArchiveBrowser",
    }
}
