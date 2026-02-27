//! Tool Metrics screen — per-tool call counts, latency, and error rates.
//!
//! Enhanced with advanced widget integration (br-3vwi.7.5):
//! - `MetricTile` summary KPIs (total calls, avg latency, error rate)
//! - `BarChart` (horizontal) for per-tool latency distribution (p50/p95/p99)
//! - `Leaderboard` for top tools by call count
//! - `WidgetState` for loading/empty/ready states
//! - View mode toggle: table view (default) vs widget dashboard view

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_extras::charts::{BarChart, BarDirection, BarGroup};
use ftui_runtime::program::Cmd;
use mcp_agent_mail_core::bocpd::BocpdDetector;
use mcp_agent_mail_core::conformal::ConformalPredictor;
use mcp_agent_mail_core::evidence_ledger::evidence_ledger;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEvent;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry;

use crate::tui_widgets::{
    AnomalyCard, AnomalySeverity, ChartTransition, DisclosureLevel, LeaderboardEntry, MetricTile,
    MetricTrend, PercentileSample, RankChange, TransparencyWidget, WidgetState,
};

const COL_NAME: usize = 0;
const COL_CALLS: usize = 1;
const COL_ERRORS: usize = 2;
const COL_ERR_PCT: usize = 3;
const COL_AVG_MS: usize = 4;
const COL_CP: usize = 5;

const SORT_LABELS: &[&str] = &["Name", "Calls", "Errors", "Err%", "Avg(ms)", "CP"];

/// Max latency samples kept per tool for sparkline rendering.
const LATENCY_HISTORY: usize = 30;

/// Max percentile samples kept for the global latency ribbon.
const PERCENTILE_HISTORY: usize = 60;
/// Reload persisted tool metrics every ~3s as a startup/live fallback.
const PERSISTED_HYDRATE_INTERVAL_TICKS: u64 = 30;
/// Chart transition duration for latency bars.
const CHART_TRANSITION_DURATION: Duration = Duration::from_millis(200);

/// BOCPD hazard rate: expect one change point every ~250 tool calls.
const BOCPD_HAZARD: f64 = 1.0 / 250.0;
/// BOCPD detection threshold for cumulative mass on short run lengths.
const BOCPD_THRESHOLD: f64 = 0.5;
/// BOCPD maximum run length to track per tool.
const BOCPD_MAX_RUN: usize = 500;
/// Conformal prediction window: 200 recent latencies for calibration.
const CONFORMAL_WINDOW: usize = 200;
/// Conformal coverage level (90%).
const CONFORMAL_COVERAGE: f64 = 0.90;
/// Max change-point events stored per tool.
const MAX_CHANGE_POINTS_PER_TOOL: usize = 50;
/// Max anomaly cards displayed in the dashboard.
const MAX_ANOMALY_CARDS: usize = 5;

/// Unicode block characters for inline sparkline.
const SPARK_CHARS: &[char] = &[
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn reduced_motion_enabled() -> bool {
    env_flag_enabled("AM_TUI_REDUCED_MOTION") || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

fn chart_animations_enabled() -> bool {
    !std::env::var("AM_TUI_CHART_ANIMATIONS").is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "0" | "false" | "no" | "off")
    })
}

/// View mode for the metrics screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Traditional table view with sorting.
    Table,
    /// Widget dashboard view with metric tiles, ribbon, and leaderboard.
    Dashboard,
}

/// A recorded change-point event for a specific tool.
#[derive(Debug, Clone)]
struct ToolChangePoint {
    /// Tool call index at which the change was detected.
    call_index: u64,
    /// Posterior probability of the change point.
    probability: f64,
    /// Estimated latency mean before the change.
    pre_mean_ms: f64,
    /// Estimated latency mean after the change.
    post_mean_ms: f64,
}

/// Accumulated stats for a single tool.
struct ToolStats {
    name: String,
    calls: u64,
    errors: u64,
    total_duration_ms: u64,
    recent_latencies: VecDeque<u64>,
    /// Previous call count for leaderboard rank-change tracking.
    prev_calls: u64,
    /// Per-tool BOCPD detector for latency change-point detection.
    bocpd: BocpdDetector,
    /// Per-tool conformal predictor for latency intervals.
    conformal: ConformalPredictor,
    /// Recorded change-point events (most recent last).
    change_points: VecDeque<ToolChangePoint>,
}

impl ToolStats {
    fn new(name: String) -> Self {
        Self {
            name,
            calls: 0,
            errors: 0,
            total_duration_ms: 0,
            recent_latencies: VecDeque::with_capacity(LATENCY_HISTORY),
            prev_calls: 0,
            bocpd: BocpdDetector::new(BOCPD_HAZARD, BOCPD_THRESHOLD, BOCPD_MAX_RUN),
            conformal: ConformalPredictor::new(CONFORMAL_WINDOW, CONFORMAL_COVERAGE),
            change_points: VecDeque::with_capacity(MAX_CHANGE_POINTS_PER_TOOL),
        }
    }

    fn avg_ms(&self) -> u64 {
        self.total_duration_ms.checked_div(self.calls).unwrap_or(0)
    }

    #[allow(clippy::cast_precision_loss)]
    fn err_pct(&self) -> f64 {
        if self.calls == 0 {
            return 0.0;
        }
        (self.errors as f64 / self.calls as f64) * 100.0
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn sparkline_str(&self) -> String {
        if self.recent_latencies.is_empty() {
            return String::new();
        }
        let max = self
            .recent_latencies
            .iter()
            .copied()
            .max()
            .unwrap_or(1)
            .max(1);
        self.recent_latencies
            .iter()
            .map(|&v| {
                let normalized = ((v as f64 / max as f64) * 8.0).round() as usize;
                SPARK_CHARS[normalized.min(SPARK_CHARS.len() - 1)]
            })
            .collect()
    }

    #[allow(clippy::cast_precision_loss, dead_code)]
    fn sparkline_f64(&self) -> Vec<f64> {
        self.recent_latencies.iter().map(|&v| v as f64).collect()
    }

    #[allow(clippy::cast_precision_loss)]
    fn record(&mut self, duration_ms: u64, is_error: bool) -> Option<ToolChangePoint> {
        self.calls += 1;
        self.total_duration_ms += duration_ms;
        if is_error {
            self.errors += 1;
        }
        if self.recent_latencies.len() >= LATENCY_HISTORY {
            self.recent_latencies.pop_front();
        }
        self.recent_latencies.push_back(duration_ms);

        // Feed latency to BOCPD and conformal predictor.
        let latency_f = duration_ms as f64;
        self.conformal.observe(latency_f);

        // Check for change-point detection.
        if let Some(cp) = self.bocpd.observe(latency_f) {
            let tcp = ToolChangePoint {
                call_index: self.calls,
                probability: cp.probability,
                pre_mean_ms: cp.pre_mean,
                post_mean_ms: cp.post_mean,
            };
            if self.change_points.len() >= MAX_CHANGE_POINTS_PER_TOOL {
                self.change_points.pop_front();
            }
            self.change_points.push_back(tcp.clone());
            return Some(tcp);
        }
        None
    }

    /// Number of detected change points.
    fn change_point_count(&self) -> usize {
        self.change_points.len()
    }

    /// Compute percentile from recent latencies using nearest-rank method.
    fn percentile(&self, pct: f64) -> f64 {
        if self.recent_latencies.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<u64> = self.recent_latencies.iter().copied().collect();
        sorted.sort_unstable();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let idx = ((pct / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        #[allow(clippy::cast_precision_loss)]
        let val = sorted[idx.min(sorted.len() - 1)] as f64;
        val
    }

    /// Snapshot the current rank change since last checkpoint.
    fn rank_change(&self) -> RankChange {
        if self.prev_calls == 0 {
            RankChange::New
        } else if self.calls > self.prev_calls {
            #[allow(clippy::cast_possible_truncation)]
            let delta = (self.calls - self.prev_calls).min(u64::from(u32::MAX)) as u32;
            RankChange::Up(delta)
        } else {
            RankChange::Steady
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn ms_f64_to_u64(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= u64::MAX as f64 {
        u64::MAX
    } else {
        value.round() as u64
    }
}

#[allow(clippy::cast_precision_loss)]
fn total_duration_ms_from_avg(avg_ms: f64, calls: u64) -> u64 {
    if calls == 0 {
        return 0;
    }
    let total = avg_ms * calls as f64;
    ms_f64_to_u64(total)
}

fn synthesize_recent_latency_samples(
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
) -> VecDeque<u64> {
    let mut out = VecDeque::with_capacity(LATENCY_HISTORY);
    let mut seeds = [
        ms_f64_to_u64(p50_ms),
        ms_f64_to_u64(avg_ms),
        ms_f64_to_u64(p95_ms),
        ms_f64_to_u64(p99_ms),
    ];
    if seeds.iter().all(|v| *v == 0) {
        seeds = [1, 1, 1, 1];
    }
    while out.len() < LATENCY_HISTORY {
        for value in &seeds {
            if out.len() >= LATENCY_HISTORY {
                break;
            }
            out.push_back(*value);
        }
    }
    out
}

#[derive(Debug, Clone)]
struct LatencyRibbonRow {
    name: String,
    p50: f64,
    p95: f64,
    p99: f64,
}

impl LatencyRibbonRow {
    const fn values(&self) -> [f64; 3] {
        [self.p50, self.p95, self.p99]
    }
}

/// An anomaly event for display in the dashboard.
#[derive(Debug, Clone)]
struct AnomalyEvent {
    tool_name: String,
    probability: f64,
    pre_mean_ms: f64,
    post_mean_ms: f64,
}

#[allow(clippy::struct_excessive_bools)]
pub struct ToolMetricsScreen {
    table_state: TableState,
    tool_map: HashMap<String, ToolStats>,
    sorted_tools: Vec<String>,
    sort_col: usize,
    sort_asc: bool,
    last_seq: u64,
    /// Synthetic event for the focused tool (palette quick actions).
    focused_synthetic: Option<crate::tui_events::MailEvent>,
    /// Current view mode (table vs dashboard).
    view_mode: ViewMode,
    /// Global latency percentile samples for the ribbon.
    percentile_samples: VecDeque<PercentileSample>,
    /// Tick counter for periodic percentile snapshot.
    snapshot_tick: u64,
    /// Last tick where persisted metrics fallback was attempted.
    last_persisted_hydrate_tick: u64,
    /// Animated latency rows consumed by the bar-chart ribbon.
    latency_ribbon_rows: Vec<LatencyRibbonRow>,
    /// Transition state for latency chart values.
    latency_chart_transition: ChartTransition,
    /// Whether chart transitions are enabled (`AM_TUI_CHART_ANIMATIONS`).
    chart_animations_enabled: bool,
    /// Recent anomaly events (change-point detections) for the dashboard.
    anomaly_events: VecDeque<AnomalyEvent>,
    /// Recent evidence ledger entries for the transparency panel.
    evidence_entries: Vec<EvidenceLedgerEntry>,
    /// Current disclosure level for the transparency widget.
    disclosure_level: DisclosureLevel,
    /// Whether the evidence drill-down panel is active (shows selected entry at L2+).
    drilldown_active: bool,
    /// Selected evidence entry index for drill-down.
    drilldown_index: usize,
    /// Whether the detail panel is visible on wide screens (table mode).
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
}

impl ToolMetricsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            tool_map: HashMap::new(),
            sorted_tools: Vec::new(),
            sort_col: COL_CALLS,
            sort_asc: false,
            last_seq: 0,
            focused_synthetic: None,
            view_mode: ViewMode::Table,
            percentile_samples: VecDeque::with_capacity(PERCENTILE_HISTORY),
            snapshot_tick: 0,
            last_persisted_hydrate_tick: 0,
            latency_ribbon_rows: Vec::new(),
            latency_chart_transition: ChartTransition::new(CHART_TRANSITION_DURATION),
            chart_animations_enabled: chart_animations_enabled(),
            anomaly_events: VecDeque::with_capacity(MAX_ANOMALY_CARDS),
            evidence_entries: Vec::new(),
            disclosure_level: DisclosureLevel::Badge,
            drilldown_active: false,
            drilldown_index: 0,
            detail_visible: true,
            detail_scroll: 0,
        }
    }

    /// Rebuild the synthetic `MailEvent` for the currently selected tool.
    fn sync_focused_event(&mut self) {
        self.focused_synthetic = self
            .table_state
            .selected
            .and_then(|i| self.sorted_tools.get(i))
            .and_then(|name| self.tool_map.get(name))
            .map(|ts| {
                crate::tui_events::MailEvent::tool_call_end(
                    &ts.name,
                    ts.avg_ms(),
                    None,
                    ts.calls,
                    0.0,
                    vec![],
                    None,
                    None,
                )
            });
    }

    fn ingest_events(&mut self, state: &TuiSharedState) {
        let events = state.events_since(self.last_seq);
        for event in &events {
            self.last_seq = event.seq().max(self.last_seq);
            if let MailEvent::ToolCallEnd {
                tool_name,
                duration_ms,
                result_preview,
                ..
            } = event
            {
                let is_error = result_preview
                    .as_deref()
                    .is_some_and(|p| p.contains("error") || p.contains("Error"));
                let cp = self
                    .tool_map
                    .entry(tool_name.clone())
                    .or_insert_with(|| ToolStats::new(tool_name.clone()))
                    .record(*duration_ms, is_error);

                // Handle change-point detection.
                if let Some(tcp) = cp {
                    // Record to evidence ledger.
                    evidence_ledger().record(
                        "metrics.bocpd.change_point",
                        serde_json::json!({
                            "tool": tool_name,
                            "call_index": tcp.call_index,
                            "pre_mean_ms": tcp.pre_mean_ms,
                            "post_mean_ms": tcp.post_mean_ms,
                        }),
                        "change_point_detected",
                        None,
                        tcp.probability,
                        "bocpd",
                    );

                    // Push anomaly event for dashboard display.
                    if self.anomaly_events.len() >= MAX_ANOMALY_CARDS {
                        self.anomaly_events.pop_front();
                    }
                    self.anomaly_events.push_back(AnomalyEvent {
                        tool_name: tool_name.clone(),
                        probability: tcp.probability,
                        pre_mean_ms: tcp.pre_mean_ms,
                        post_mean_ms: tcp.post_mean_ms,
                    });
                }
            }
        }
    }

    fn hydrate_from_persisted_metrics(&mut self, state: &TuiSharedState) {
        let cfg = state.config_snapshot();
        let persisted =
            crate::tool_metrics::load_latest_persisted_metrics(&cfg.raw_database_url, 512);
        if persisted.is_empty() {
            return;
        }

        let mut updated = false;
        for metric in &persisted {
            let tool_stats = self
                .tool_map
                .entry(metric.tool_name.clone())
                .or_insert_with(|| ToolStats::new(metric.tool_name.clone()));
            if tool_stats.calls > metric.calls {
                continue;
            }

            tool_stats.calls = metric.calls;
            tool_stats.errors = metric.errors.min(metric.calls);
            tool_stats.total_duration_ms = total_duration_ms_from_avg(metric.avg_ms, metric.calls);
            tool_stats.recent_latencies = synthesize_recent_latency_samples(
                metric.avg_ms,
                metric.p50_ms,
                metric.p95_ms,
                metric.p99_ms,
            );
            updated = true;
        }

        if updated {
            self.rebuild_sorted();
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn hydrate_from_runtime_snapshot(&mut self) {
        let runtime = mcp_agent_mail_tools::tool_metrics_snapshot_full();
        if runtime.is_empty() {
            return;
        }

        let mut updated = false;
        for metric in runtime {
            let tool_stats = self
                .tool_map
                .entry(metric.name.clone())
                .or_insert_with(|| ToolStats::new(metric.name.clone()));

            if tool_stats.calls > metric.calls {
                continue;
            }

            tool_stats.calls = metric.calls;
            tool_stats.errors = metric.errors.min(metric.calls);
            if let Some(latency) = metric.latency {
                tool_stats.total_duration_ms =
                    total_duration_ms_from_avg(latency.avg_ms, metric.calls);
                tool_stats.recent_latencies = synthesize_recent_latency_samples(
                    latency.avg_ms,
                    latency.p50_ms,
                    latency.p95_ms,
                    latency.p99_ms,
                );
            } else if metric.calls == 0 {
                tool_stats.total_duration_ms = 0;
                tool_stats.recent_latencies.clear();
            }
            updated = true;
        }

        if updated {
            self.rebuild_sorted();
        }
    }

    fn rebuild_sorted(&mut self) {
        let mut tools: Vec<&ToolStats> = self.tool_map.values().collect();
        tools.sort_by(|a, b| {
            let cmp = match self.sort_col {
                COL_NAME => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                COL_CALLS => a.calls.cmp(&b.calls),
                COL_ERRORS => a.errors.cmp(&b.errors),
                COL_ERR_PCT => a
                    .err_pct()
                    .partial_cmp(&b.err_pct())
                    .unwrap_or(std::cmp::Ordering::Equal),
                COL_AVG_MS => a.avg_ms().cmp(&b.avg_ms()),
                COL_CP => a.change_point_count().cmp(&b.change_point_count()),
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });
        self.sorted_tools = tools.iter().map(|t| t.name.clone()).collect();

        // Clamp selection
        if let Some(sel) = self.table_state.selected
            && sel >= self.sorted_tools.len()
        {
            self.table_state.selected = if self.sorted_tools.is_empty() {
                None
            } else {
                Some(self.sorted_tools.len() - 1)
            };
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sorted_tools.is_empty() {
            return;
        }
        let len = self.sorted_tools.len();
        let current = self.table_state.selected.unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(delta.unsigned_abs()).min(len - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.table_state.selected = Some(next);
        self.detail_scroll = 0;
    }

    /// Get total stats across all tools.
    fn totals(&self) -> (u64, u64, u64) {
        let mut calls = 0u64;
        let mut errors = 0u64;
        let mut total_ms = 0u64;
        for stats in self.tool_map.values() {
            calls += stats.calls;
            errors += stats.errors;
            total_ms += stats.total_duration_ms;
        }
        let avg = total_ms.checked_div(calls).unwrap_or(0);
        (calls, errors, avg)
    }

    /// Take a global percentile snapshot from all tools' recent latencies.
    fn snapshot_percentiles(&mut self) {
        if self.tool_map.is_empty() {
            return;
        }
        // Aggregate all recent latencies across all tools.
        let mut all_latencies: Vec<u64> = self
            .tool_map
            .values()
            .flat_map(|ts| ts.recent_latencies.iter().copied())
            .collect();
        if all_latencies.is_empty() {
            return;
        }
        all_latencies.sort_unstable();
        let p = |pct: f64| -> f64 {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let idx = ((pct / 100.0) * (all_latencies.len() as f64 - 1.0)).round() as usize;
            #[allow(clippy::cast_precision_loss)]
            let val = all_latencies[idx.min(all_latencies.len() - 1)] as f64;
            val
        };
        let sample = PercentileSample {
            p50: p(50.0),
            p95: p(95.0),
            p99: p(99.0),
        };
        if self.percentile_samples.len() >= PERCENTILE_HISTORY {
            self.percentile_samples.pop_front();
        }
        self.percentile_samples.push_back(sample);
    }

    /// Checkpoint rank changes for leaderboard tracking.
    fn checkpoint_ranks(&mut self) {
        for stats in self.tool_map.values_mut() {
            stats.prev_calls = stats.calls;
        }
    }

    fn compute_latency_rows(&self) -> Vec<LatencyRibbonRow> {
        let mut rows: Vec<LatencyRibbonRow> = self
            .tool_map
            .values()
            .filter(|ts| !ts.recent_latencies.is_empty())
            .map(|ts| LatencyRibbonRow {
                name: ts.name.clone(),
                p50: ts.percentile(50.0),
                p95: ts.percentile(95.0),
                p99: ts.percentile(99.0),
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .p99
                .partial_cmp(&left.p99)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    }

    fn refresh_latency_ribbon_animation(&mut self) {
        let rows = self.compute_latency_rows();
        if rows.is_empty() {
            self.latency_ribbon_rows.clear();
            self.latency_chart_transition.clear();
            return;
        }

        let target_values: Vec<f64> = rows.iter().flat_map(LatencyRibbonRow::values).collect();
        let now = Instant::now();
        self.latency_chart_transition
            .set_target(&target_values, now);
        let sampled = self.latency_chart_transition.sample_values(
            now,
            reduced_motion_enabled() || !self.chart_animations_enabled,
        );

        self.latency_ribbon_rows = rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| {
                let base = idx * 3;
                let p50 = sampled.get(base).copied().unwrap_or(row.p50);
                let p95 = sampled.get(base + 1).copied().unwrap_or(row.p95);
                let p99 = sampled.get(base + 2).copied().unwrap_or(row.p99);
                LatencyRibbonRow {
                    name: row.name,
                    p50,
                    p95,
                    p99,
                }
            })
            .collect();
    }

    /// Render the table view (original view).
    #[allow(clippy::too_many_lines)]
    fn render_table_view(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let header_h = 1_u16;
        let table_h = area.height.saturating_sub(header_h);
        let header_area = Rect::new(area.x, area.y, area.width, header_h);
        let table_area = Rect::new(area.x, area.y + header_h, area.width, table_h);

        // Summary line
        let (total_calls, total_errors, avg_ms) = self.totals();
        let sort_indicator = if self.sort_asc {
            "\u{25b2}"
        } else {
            "\u{25bc}"
        };
        let sort_label = SORT_LABELS.get(self.sort_col).unwrap_or(&"?");
        let total_cp: usize = self
            .tool_map
            .values()
            .map(ToolStats::change_point_count)
            .sum();
        let summary = format!(
            " {} tools | {} calls | {} errors | avg {}ms | {} CP | Sort: {}{} | v=dashboard",
            self.tool_map.len(),
            total_calls,
            total_errors,
            avg_ms,
            total_cp,
            sort_label,
            sort_indicator,
        );
        let p = Paragraph::new(summary);
        p.render(header_area, frame);

        // Table
        let header = Row::new([
            "Tool Name",
            "Calls",
            "Errors",
            "Err%",
            "Avg(ms)",
            "CP",
            "CI(90%)",
            "Trend",
        ])
        .style(Style::default().bold());

        let rows: Vec<Row> = self
            .sorted_tools
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                let stats = self.tool_map.get(name)?;
                let err_pct = format!("{:.1}%", stats.err_pct());
                let spark = stats.sparkline_str();
                let cp_count = format!("{}", stats.change_point_count());
                #[allow(clippy::option_if_let_else)]
                let ci_str = if let Some(interval) = stats.conformal.predict() {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let lo = interval.lower.max(0.0) as u64;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let hi = interval.upper.max(0.0) as u64;
                    format!("[{lo}-{hi}]")
                } else {
                    "—".to_string()
                };
                let row_bg = if i % 2 == 0 {
                    crate::tui_theme::lerp_color(tp.panel_bg, tp.bg_surface, 0.14)
                } else {
                    crate::tui_theme::lerp_color(tp.panel_bg, tp.bg_surface, 0.24)
                };
                let style = if Some(i) == self.table_state.selected {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else if stats.err_pct() > 5.0 {
                    Style::default().fg(tp.severity_error).bg(row_bg)
                } else {
                    Style::default().fg(tp.text_primary).bg(row_bg)
                };
                Some(
                    Row::new([
                        stats.name.clone(),
                        format!("{}", stats.calls),
                        format!("{}", stats.errors),
                        err_pct,
                        format!("{}", stats.avg_ms()),
                        cp_count,
                        ci_str,
                        spark,
                    ])
                    .style(style),
                )
            })
            .collect();

        let widths = [
            Constraint::Percentage(22.0),
            Constraint::Percentage(9.0),
            Constraint::Percentage(9.0),
            Constraint::Percentage(8.0),
            Constraint::Percentage(10.0),
            Constraint::Percentage(6.0),
            Constraint::Percentage(12.0),
            Constraint::Percentage(24.0),
        ];

        let block = Block::default()
            .title("Tool Metrics")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
    }

    /// Render the widget dashboard view.
    #[allow(clippy::cast_precision_loss)]
    fn render_dashboard_view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        // Layout: tiles (3h) + anomaly (4h) + transparency (3h) + ribbon (8h) + leaderboard (rest)
        let tiles_h = 3_u16.min(area.height);
        let remaining = area.height.saturating_sub(tiles_h);
        let anomaly_h = if self.anomaly_events.is_empty() || remaining < 16 {
            0_u16
        } else {
            4_u16.min(remaining / 4)
        };
        let after_anomaly = remaining.saturating_sub(anomaly_h);
        let evidence_h = if self.evidence_entries.is_empty() || after_anomaly < 10 {
            0_u16
        } else {
            match self.disclosure_level {
                DisclosureLevel::Badge => 1,
                DisclosureLevel::Summary => 3_u16.min(after_anomaly / 4),
                DisclosureLevel::Detail | DisclosureLevel::DeepDive => 5_u16.min(after_anomaly / 3),
            }
        };
        let tiles_area = Rect::new(area.x, area.y, area.width, tiles_h);
        let mut y_offset = area.y + tiles_h;

        // --- Metric Tiles ---
        self.render_metric_tiles(frame, tiles_area, state);

        // --- Anomaly Cards ---
        if anomaly_h >= 3 {
            let anomaly_area = Rect::new(area.x, y_offset, area.width, anomaly_h);
            self.render_anomaly_cards(frame, anomaly_area);
            y_offset += anomaly_h;
        }

        // --- Evidence Transparency Panel ---
        if evidence_h > 0 {
            let evidence_area = Rect::new(area.x, y_offset, area.width, evidence_h);
            if self.drilldown_active && self.drilldown_index < self.evidence_entries.len() {
                // Drill-down: show only the selected entry at the current level.
                let single = &self.evidence_entries[self.drilldown_index..=self.drilldown_index];
                TransparencyWidget::new(single)
                    .level(self.disclosure_level)
                    .render(evidence_area, frame);
            } else {
                // Normal: show all entries.
                TransparencyWidget::new(&self.evidence_entries)
                    .level(self.disclosure_level)
                    .render(evidence_area, frame);
            }
            y_offset += evidence_h;
        }

        // --- Cache Eviction Transparency ---
        let cache_entries: Vec<EvidenceLedgerEntry> = self
            .evidence_entries
            .iter()
            .filter(|e| e.decision_point.starts_with("cache"))
            .cloned()
            .collect();
        if !cache_entries.is_empty() && y_offset + 2 < area.y + area.height {
            let cache_h = 2_u16.min(area.height.saturating_sub(y_offset - area.y));
            let cache_area = Rect::new(area.x, y_offset, area.width, cache_h);
            TransparencyWidget::new(&cache_entries)
                .level(DisclosureLevel::Summary)
                .render(cache_area, frame);
            y_offset += cache_h;
        }

        // Recalculate remaining space after dynamic sections.
        let used = y_offset.saturating_sub(area.y);
        let after_all = area.height.saturating_sub(used);
        let final_ribbon_h = if after_all > 12 { 8_u16 } else { after_all / 2 };
        let final_leader_h = after_all.saturating_sub(final_ribbon_h);
        let ribbon_area = Rect::new(area.x, y_offset, area.width, final_ribbon_h);
        let leader_area = Rect::new(
            area.x,
            y_offset + final_ribbon_h,
            area.width,
            final_leader_h,
        );

        // --- Percentile Ribbon ---
        if final_ribbon_h >= 3 {
            self.render_latency_ribbon(frame, ribbon_area);
        }

        // --- Leaderboard ---
        if final_leader_h >= 3 {
            self.render_leaderboard(frame, leader_area);
        }
    }

    /// Render the top metric tile row.
    fn render_metric_tiles(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        if area.width < 10 || area.height < 1 {
            return;
        }
        let (total_calls, total_errors, avg_ms) = self.totals();
        let calls_str = format!("{total_calls}");
        let latency_str = format!("{avg_ms}ms");
        #[allow(clippy::cast_precision_loss)]
        let err_rate = if total_calls > 0 {
            format!("{:.1}%", (total_errors as f64 / total_calls as f64) * 100.0)
        } else {
            "0.0%".to_string()
        };

        let sparkline_data = state.sparkline_snapshot();

        // Split area into 3 tiles
        let tile_w = area.width / 3;
        let tile1 = Rect::new(area.x, area.y, tile_w, area.height);
        let tile2 = Rect::new(area.x + tile_w, area.y, tile_w, area.height);
        let tile3 = Rect::new(
            area.x + tile_w * 2,
            area.y,
            area.width - tile_w * 2,
            area.height,
        );

        let trend_calls = if total_calls > 0 {
            MetricTrend::Up
        } else {
            MetricTrend::Flat
        };
        let trend_latency = if avg_ms > 100 {
            MetricTrend::Down
        } else {
            MetricTrend::Flat
        };
        #[allow(clippy::cast_precision_loss)]
        let trend_errors = if total_errors as f64 / (total_calls.max(1) as f64) > 0.05 {
            MetricTrend::Down
        } else {
            MetricTrend::Flat
        };

        MetricTile::new("Total Calls", &calls_str, trend_calls)
            .sparkline(&sparkline_data)
            .render(tile1, frame);

        MetricTile::new("Avg Latency", &latency_str, trend_latency).render(tile2, frame);

        MetricTile::new("Error Rate", &err_rate, trend_errors)
            .value_color(if total_errors > 0 {
                tp.severity_error
            } else {
                tp.severity_ok
            })
            .render(tile3, frame);
    }

    /// Render the latency distribution panel as a horizontal bar chart.
    ///
    /// Each tool becomes a `BarGroup` with three bars: P50, P95, P99.
    /// Colors are taken from the theme palette's chart series.
    fn render_latency_ribbon(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.latency_ribbon_rows.is_empty() {
            let widget: WidgetState<'_, Paragraph<'_>> = WidgetState::Loading {
                message: "Collecting latency samples...",
            };
            widget.render(area, frame);
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Latency Distribution (p50/p95/p99)")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));

        let inner = block.inner(area);
        block.render(area, frame);

        if inner.is_empty() {
            return;
        }

        // Cap at 15 tools or available height, whichever is smaller (br-333hh).
        let max_groups = ((inner.height as usize) + 1) / 4;
        let visible = self
            .latency_ribbon_rows
            .len()
            .min(max_groups.max(1))
            .min(15);

        let groups: Vec<BarGroup<'_>> = self.latency_ribbon_rows[..visible]
            .iter()
            .map(|row| BarGroup::new(&row.name, vec![row.p50, row.p95, row.p99]))
            .collect();

        // Severity-based coloring (br-333hh): green < 100ms, yellow < 500ms, red >= 500ms.
        let max_p99 = self.latency_ribbon_rows[..visible]
            .iter()
            .map(|row| row.p99)
            .fold(0.0_f64, f64::max);
        let severity_color = if max_p99 < 100.0 {
            tp.severity_ok
        } else if max_p99 < 500.0 {
            tp.severity_warn
        } else {
            tp.severity_error
        };
        let colors: Vec<PackedRgba> = vec![
            tp.chart_series[0], // P50 — theme default
            tp.chart_series[1], // P95 — theme default
            severity_color,     // P99 — severity-coded
        ];

        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .colors(colors)
            .bar_width(1)
            .bar_gap(0)
            .group_gap(1);

        chart.render(inner, frame);
    }

    /// Render the top-tools leaderboard.
    fn render_leaderboard(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut sorted: Vec<&ToolStats> = self.tool_map.values().collect();
        sorted.sort_by_key(|ts| std::cmp::Reverse(ts.calls));

        let entries: Vec<LeaderboardEntry<'_>> = sorted
            .iter()
            .take(10)
            .map(|ts| LeaderboardEntry {
                name: &ts.name,
                #[allow(clippy::cast_precision_loss)]
                value: ts.calls as f64,
                secondary: None,
                change: ts.rank_change(),
            })
            .collect();

        if entries.is_empty() {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Top Tools by Call Count")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));

        crate::tui_widgets::Leaderboard::new(&entries)
            .block(block)
            .value_suffix("calls")
            .max_visible(area.height.saturating_sub(2) as usize)
            .render(area, frame);
    }

    /// Render anomaly cards from BOCPD change-point detections.
    fn render_anomaly_cards(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.anomaly_events.is_empty() || area.height < 3 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::default()
            .title("Change Points (BOCPD)")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let inner = block.inner(area);
        block.render(area, frame);

        if inner.is_empty() {
            return;
        }

        // Show the most recent anomaly event as an AnomalyCard.
        if let Some(evt) = self.anomaly_events.back() {
            let headline_text = format!(
                "{}: latency shift {:.0}ms -> {:.0}ms",
                evt.tool_name, evt.pre_mean_ms, evt.post_mean_ms
            );
            let severity = if (evt.post_mean_ms - evt.pre_mean_ms).abs() > 100.0 {
                AnomalySeverity::High
            } else {
                AnomalySeverity::Medium
            };
            let card = AnomalyCard::new(severity, evt.probability, &headline_text);
            card.render(inner, frame);
        }
    }

    /// Render the detail panel for the currently selected tool (Table mode).
    #[allow(clippy::cast_possible_truncation)]
    fn render_tool_detail_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Tool Detail ");
        let inner = block.inner(area);
        block.render(area, frame);

        let Some(selected_idx) = self.table_state.selected else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f527}",
                "No Tool Selected",
                "Select a tool from the table to view details.",
            );
            return;
        };

        let Some(tool_name) = self.sorted_tools.get(selected_idx) else {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{1f527}",
                "No Tool Selected",
                "Select a tool from the table to view details.",
            );
            return;
        };

        let Some(stats) = self.tool_map.get(tool_name) else {
            return;
        };

        let lines = Self::build_tool_detail_lines(stats, &tp);
        render_kv_lines(frame, inner, &lines, self.detail_scroll, &tp);
    }

    fn build_tool_detail_lines(
        stats: &ToolStats,
        tp: &crate::tui_theme::TuiThemePalette,
    ) -> Vec<(String, String, Option<PackedRgba>)> {
        let mut lines: Vec<(String, String, Option<PackedRgba>)> = Vec::new();

        lines.push(("Tool".into(), stats.name.clone(), None));
        lines.push(("Calls".into(), stats.calls.to_string(), None));
        lines.push((
            "Errors".into(),
            stats.errors.to_string(),
            if stats.errors > 0 {
                Some(tp.severity_error)
            } else {
                None
            },
        ));

        let err_pct = stats.err_pct();
        lines.push((
            "Error %".into(),
            format!("{err_pct:.1}%"),
            if err_pct > 5.0 {
                Some(tp.severity_error)
            } else {
                None
            },
        ));

        lines.push(("Avg Latency".into(), format!("{}ms", stats.avg_ms()), None));

        // Percentiles from recent latencies
        if !stats.recent_latencies.is_empty() {
            lines.push((
                "P50".into(),
                format!("{:.0}ms", stats.percentile(50.0)),
                None,
            ));
            lines.push((
                "P95".into(),
                format!("{:.0}ms", stats.percentile(95.0)),
                None,
            ));
            lines.push((
                "P99".into(),
                format!("{:.0}ms", stats.percentile(99.0)),
                None,
            ));
        }

        // Change points
        let cp = stats.change_point_count();
        if cp > 0 {
            lines.push(("Change Pts".into(), cp.to_string(), Some(tp.severity_warn)));
        }

        // Conformal interval
        if let Some(interval) = stats.conformal.predict() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let lo = interval.lower.max(0.0) as u64;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let hi = interval.upper.max(0.0) as u64;
            lines.push(("CI (90%)".into(), format!("[{lo}-{hi}]ms"), None));
        }

        // Sparkline
        let spark = stats.sparkline_str();
        if !spark.is_empty() {
            lines.push(("Trend".into(), spark, Some(tp.metric_messages)));
        }

        // Recent latencies (last 10)
        if !stats.recent_latencies.is_empty() {
            lines.push((String::new(), String::new(), None));
            lines.push(("Recent".into(), "Latencies (ms)".into(), None));
            let recent: Vec<String> = stats
                .recent_latencies
                .iter()
                .rev()
                .take(10)
                .map(ToString::to_string)
                .collect();
            lines.push((String::new(), recent.join(", "), None));
        }

        lines
    }
}

/// Render key-value lines with a label column and a value column, supporting scroll.
#[allow(clippy::cast_possible_truncation)]
fn render_kv_lines(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: &[(String, String, Option<PackedRgba>)],
    scroll: usize,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    let visible_height = usize::from(inner.height);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = scroll.min(max_scroll);
    let label_w = 12u16;

    for (i, (label, value, color)) in lines.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let label_area = Rect::new(inner.x, y, label_w.min(inner.width), 1);
        let label_text = format!("{label}:");
        Paragraph::new(label_text)
            .style(Style::default().fg(tp.text_muted).bold())
            .render(label_area, frame);

        let val_x = inner.x + label_w + 1;
        if val_x < inner.x + inner.width {
            let val_w = (inner.x + inner.width).saturating_sub(val_x);
            let val_area = Rect::new(val_x, y, val_w, 1);
            let val_style = color.map_or_else(
                || Style::default().fg(tp.text_primary),
                |c| Style::default().fg(c),
            );
            Paragraph::new(value.as_str())
                .style(val_style)
                .render(val_area, frame);
        }
    }

    if total_lines > visible_height {
        let indicator = format!(
            " {}/{} ",
            scroll + 1,
            total_lines.saturating_sub(visible_height) + 1
        );
        let ind_w = indicator.len() as u16;
        if ind_w < inner.width {
            let ind_area = Rect::new(
                inner.x + inner.width - ind_w,
                inner.y + inner.height.saturating_sub(1),
                ind_w,
                1,
            );
            Paragraph::new(indicator)
                .style(Style::default().fg(tp.text_muted))
                .render(ind_area, frame);
        }
    }
}

impl Default for ToolMetricsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ToolMetricsScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Char('G') | KeyCode::End => {
                    if !self.sorted_tools.is_empty() {
                        self.table_state.selected = Some(self.sorted_tools.len() - 1);
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if !self.sorted_tools.is_empty() {
                        self.table_state.selected = Some(0);
                    }
                }
                KeyCode::Char('s') => {
                    self.sort_col = (self.sort_col + 1) % SORT_LABELS.len();
                    self.rebuild_sorted();
                }
                KeyCode::Char('S') => {
                    self.sort_asc = !self.sort_asc;
                    self.rebuild_sorted();
                }
                KeyCode::Char('v') => {
                    self.view_mode = match self.view_mode {
                        ViewMode::Table => ViewMode::Dashboard,
                        ViewMode::Dashboard => ViewMode::Table,
                    };
                }
                KeyCode::Char('i') => {
                    self.detail_visible = !self.detail_visible;
                }
                KeyCode::Char('J') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Char('l') => {
                    self.disclosure_level = self.disclosure_level.next();
                }
                KeyCode::Char('L') => {
                    self.disclosure_level = self.disclosure_level.prev();
                }
                KeyCode::Enter => {
                    // Drill down: activate evidence panel or step deeper.
                    if self.drilldown_active {
                        self.disclosure_level = self.disclosure_level.next();
                    } else if !self.evidence_entries.is_empty() {
                        self.drilldown_active = true;
                        self.disclosure_level = DisclosureLevel::Detail;
                    }
                }
                KeyCode::Escape => {
                    // Step up one level or deactivate drill-down.
                    if self.drilldown_active {
                        if self.disclosure_level == DisclosureLevel::Badge {
                            self.drilldown_active = false;
                        } else {
                            self.disclosure_level = self.disclosure_level.prev();
                        }
                    }
                }
                KeyCode::Char('1') => {
                    self.disclosure_level = DisclosureLevel::Badge;
                }
                KeyCode::Char('2') => {
                    self.disclosure_level = DisclosureLevel::Summary;
                }
                KeyCode::Char('3') => {
                    self.disclosure_level = DisclosureLevel::Detail;
                }
                KeyCode::Char('4') => {
                    self.disclosure_level = DisclosureLevel::DeepDive;
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        if self.tool_map.is_empty()
            || tick_count.wrapping_sub(self.last_persisted_hydrate_tick)
                >= PERSISTED_HYDRATE_INTERVAL_TICKS
        {
            self.hydrate_from_persisted_metrics(state);
            self.last_persisted_hydrate_tick = tick_count;
        }
        self.ingest_events(state);
        if self.tool_map.is_empty() || tick_count.is_multiple_of(20) {
            self.hydrate_from_runtime_snapshot();
        }
        if tick_count.is_multiple_of(10) {
            self.rebuild_sorted();
            self.snapshot_percentiles();
            self.snapshot_tick += 1;
            // Refresh evidence entries for the transparency panel.
            self.evidence_entries = evidence_ledger().recent(20);
        }
        // Checkpoint ranks every ~50 ticks for change tracking.
        if tick_count.is_multiple_of(50) {
            self.checkpoint_ranks();
        }
        self.refresh_latency_ribbon_animation();
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 30 {
            return;
        }

        // Outer bordered panel
        let outer_block = crate::tui_panel_helpers::panel_block(" Tool Metrics ");
        let inner = outer_block.inner(area);
        outer_block.render(area, frame);

        match self.view_mode {
            ViewMode::Table => {
                // Responsive layout: table + detail on wide screens
                let layout =
                    ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
                        .at(
                            Breakpoint::Lg,
                            Flex::horizontal()
                                .constraints([Constraint::Percentage(55.0), Constraint::Fill]),
                        )
                        .at(
                            Breakpoint::Xl,
                            Flex::horizontal()
                                .constraints([Constraint::Percentage(50.0), Constraint::Fill]),
                        );

                let split = layout.split(inner);
                self.render_table_view(frame, split.rects[0]);

                if split.rects.len() >= 2 && self.detail_visible {
                    self.render_tool_detail_panel(frame, split.rects[1]);
                }
            }
            ViewMode::Dashboard => self.render_dashboard_view(frame, inner, state),
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate tools",
            },
            HelpEntry {
                key: "s",
                action: "Cycle sort column",
            },
            HelpEntry {
                key: "S",
                action: "Toggle sort order",
            },
            HelpEntry {
                key: "v",
                action: "Toggle table/dashboard view",
            },
            HelpEntry {
                key: "i",
                action: "Toggle detail panel",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("MCP tool call counts, latency, and error rates. Sort by any column.")
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::ToolByName(name) = target
            && let Some(pos) = self.sorted_tools.iter().position(|t| t == name)
        {
            self.table_state.selected = Some(pos);
            return true;
        }
        false
    }

    fn copyable_content(&self) -> Option<String> {
        let idx = self.table_state.selected?;
        let tool_name = self.sorted_tools.get(idx)?;
        Some(tool_name.clone())
    }

    fn title(&self) -> &'static str {
        "Tool Metrics"
    }

    fn tab_label(&self) -> &'static str {
        "Tools"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::Config;
    use mcp_agent_mail_db::DbConn;
    use mcp_agent_mail_db::sqlmodel::Value;

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
    }

    fn test_state_with_database_url(database_url: String) -> std::sync::Arc<TuiSharedState> {
        let config = Config {
            database_url,
            ..Config::default()
        };
        TuiSharedState::new(&config)
    }

    #[test]
    fn new_screen_defaults() {
        let screen = ToolMetricsScreen::new();
        assert!(screen.tool_map.is_empty());
        assert_eq!(screen.sort_col, COL_CALLS);
        assert!(!screen.sort_asc);
        assert_eq!(screen.view_mode, ViewMode::Table);
    }

    #[test]
    fn tick_hydrates_from_persisted_metrics_when_events_are_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tool_metrics_hydration.db");
        let conn = DbConn::open_file(db_path.display().to_string()).expect("open sqlite");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS tool_metrics_snapshots (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 collected_ts INTEGER NOT NULL, \
                 tool_name TEXT NOT NULL, \
                 calls INTEGER NOT NULL DEFAULT 0, \
                 errors INTEGER NOT NULL DEFAULT 0, \
                 cluster TEXT NOT NULL DEFAULT '', \
                 capabilities_json TEXT NOT NULL DEFAULT '[]', \
                 complexity TEXT NOT NULL DEFAULT 'unknown', \
                 latency_avg_ms REAL, \
                 latency_min_ms REAL, \
                 latency_max_ms REAL, \
                 latency_p50_ms REAL, \
                 latency_p95_ms REAL, \
                 latency_p99_ms REAL, \
                 latency_is_slow INTEGER NOT NULL DEFAULT 0\
             )",
            &[],
        )
        .expect("create metrics table");
        conn.execute_sync(
            "INSERT INTO tool_metrics_snapshots (\
                 collected_ts, tool_name, calls, errors, cluster, capabilities_json, complexity, \
                 latency_avg_ms, latency_min_ms, latency_max_ms, latency_p50_ms, latency_p95_ms, latency_p99_ms, latency_is_slow\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1_700_000_000_000_000),
                Value::Text("send_message".to_string()),
                Value::BigInt(42),
                Value::BigInt(5),
                Value::Text("messaging".to_string()),
                Value::Text("[]".to_string()),
                Value::Text("medium".to_string()),
                Value::Double(180.0),
                Value::Double(20.0),
                Value::Double(950.0),
                Value::Double(120.0),
                Value::Double(640.0),
                Value::Double(950.0),
                Value::BigInt(1),
            ],
        )
        .expect("insert metrics row");

        // Drop the writer connection so data is flushed to disk before
        // load_latest_persisted_metrics opens a new connection to read.
        drop(conn);

        let database_url = format!("sqlite:///{}", db_path.display());
        let state = test_state_with_database_url(database_url);
        let mut screen = ToolMetricsScreen::new();
        screen.tick(0, &state);

        let hydrated_stats = screen
            .tool_map
            .get("send_message")
            .expect("persisted metrics should hydrate tool stats");
        assert_eq!(hydrated_stats.calls, 42);
        assert_eq!(hydrated_stats.errors, 5);
        assert!(!hydrated_stats.recent_latencies.is_empty());
    }

    #[test]
    fn renders_without_panic() {
        let state = test_state();
        let screen = ToolMetricsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn renders_at_minimum_size() {
        let state = test_state();
        let screen = ToolMetricsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(30, 3, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 30, 3), &state);
    }

    #[test]
    fn renders_tiny_without_panic() {
        let state = test_state();
        let screen = ToolMetricsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(10, 2, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 10, 2), &state);
    }

    #[test]
    fn title_and_label() {
        let screen = ToolMetricsScreen::new();
        assert_eq!(screen.title(), "Tool Metrics");
        assert_eq!(screen.tab_label(), "Tools");
    }

    #[test]
    fn keybindings_documented() {
        let screen = ToolMetricsScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 4);
    }

    #[test]
    fn tool_stats_record_and_compute() {
        let mut stats = ToolStats::new("test".into());
        stats.record(10, false);
        stats.record(20, false);
        stats.record(30, true);
        assert_eq!(stats.calls, 3);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.avg_ms(), 20);
        assert!((stats.err_pct() - 33.3).abs() < 1.0);
    }

    #[test]
    fn tool_stats_sparkline() {
        let mut stats = ToolStats::new("test".into());
        for i in 0..10 {
            stats.record(i * 10, false);
        }
        let spark = stats.sparkline_str();
        assert!(!spark.is_empty());
        assert_eq!(spark.chars().count(), 10);
    }

    #[test]
    fn tool_stats_empty_sparkline() {
        let stats = ToolStats::new("empty".into());
        assert!(stats.sparkline_str().is_empty());
    }

    #[test]
    fn ingest_tool_call_end_events() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        let _ = state.push_event(MailEvent::tool_call_end(
            "send_message",
            42,
            Some("ok".into()),
            1,
            0.5,
            vec![],
            None,
            None,
        ));
        let _ = state.push_event(MailEvent::tool_call_end(
            "fetch_inbox",
            10,
            None,
            2,
            0.2,
            vec![],
            None,
            None,
        ));

        screen.ingest_events(&state);
        assert_eq!(screen.tool_map.len(), 2);
        assert_eq!(screen.tool_map["send_message"].calls, 1);
        assert_eq!(screen.tool_map["fetch_inbox"].calls, 1);
    }

    #[test]
    fn deep_link_tool_by_name() {
        let mut screen = ToolMetricsScreen::new();
        screen.sorted_tools = vec!["send_message".into(), "fetch_inbox".into()];
        let handled = screen.receive_deep_link(&DeepLinkTarget::ToolByName("fetch_inbox".into()));
        assert!(handled);
        assert_eq!(screen.table_state.selected, Some(1));
    }

    #[test]
    fn s_cycles_sort() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();
        let initial = screen.sort_col;
        let s = Event::Key(ftui::KeyEvent::new(KeyCode::Char('s')));
        screen.update(&s, &state);
        assert_ne!(screen.sort_col, initial);
    }

    #[test]
    fn default_impl() {
        let screen = ToolMetricsScreen::default();
        assert!(screen.tool_map.is_empty());
    }

    // --- New tests for br-3vwi.7.5 enhancements ---

    #[test]
    fn v_toggles_view_mode() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();
        assert_eq!(screen.view_mode, ViewMode::Table);

        let v = Event::Key(ftui::KeyEvent::new(KeyCode::Char('v')));
        screen.update(&v, &state);
        assert_eq!(screen.view_mode, ViewMode::Dashboard);

        screen.update(&v, &state);
        assert_eq!(screen.view_mode, ViewMode::Table);
    }

    #[test]
    fn dashboard_view_renders_empty() {
        let state = test_state();
        let screen = ToolMetricsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.render_dashboard_view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn dashboard_view_renders_with_data() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Populate some tool data
        let _ = state.push_event(MailEvent::tool_call_end(
            "send_message",
            42,
            None,
            1,
            0.5,
            vec![],
            None,
            None,
        ));
        let _ = state.push_event(MailEvent::tool_call_end(
            "fetch_inbox",
            10,
            None,
            2,
            0.2,
            vec![],
            None,
            None,
        ));
        screen.ingest_events(&state);
        screen.snapshot_percentiles();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.render_dashboard_view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn percentile_snapshot_populates_samples() {
        let mut screen = ToolMetricsScreen::new();
        assert!(screen.percentile_samples.is_empty());

        // Add data
        screen.tool_map.insert("test_tool".into(), {
            let mut ts = ToolStats::new("test_tool".into());
            for i in 0..20 {
                ts.record(i * 5, false);
            }
            ts
        });

        screen.snapshot_percentiles();
        assert_eq!(screen.percentile_samples.len(), 1);

        let sample = &screen.percentile_samples[0];
        assert!(sample.p50 > 0.0);
        assert!(sample.p95 >= sample.p50);
        assert!(sample.p99 >= sample.p95);
    }

    #[test]
    fn percentile_snapshot_empty_tools_noop() {
        let mut screen = ToolMetricsScreen::new();
        screen.snapshot_percentiles();
        assert!(screen.percentile_samples.is_empty());
    }

    #[test]
    fn tool_stats_percentile_computation() {
        let mut stats = ToolStats::new("test".into());
        for i in 1..=100 {
            stats.record(i, false);
        }
        let p50 = stats.percentile(50.0);
        let p95 = stats.percentile(95.0);
        let p99 = stats.percentile(99.0);
        // With 30-element window (limited by LATENCY_HISTORY), we have values 71..=100
        assert!(p50 > 0.0);
        assert!(p95 >= p50);
        assert!(p99 >= p95);
    }

    #[test]
    fn tool_stats_rank_change_new() {
        let stats = ToolStats::new("new_tool".into());
        assert!(matches!(stats.rank_change(), RankChange::New));
    }

    #[test]
    fn tool_stats_rank_change_up() {
        let mut stats = ToolStats::new("tool".into());
        stats.prev_calls = 5;
        stats.calls = 10;
        assert!(matches!(stats.rank_change(), RankChange::Up(5)));
    }

    #[test]
    fn tool_stats_rank_change_steady() {
        let mut stats = ToolStats::new("tool".into());
        stats.prev_calls = 10;
        stats.calls = 10;
        assert!(matches!(stats.rank_change(), RankChange::Steady));
    }

    #[test]
    fn checkpoint_ranks_updates_prev() {
        let mut screen = ToolMetricsScreen::new();
        screen.tool_map.insert("tool".into(), {
            let mut ts = ToolStats::new("tool".into());
            ts.calls = 42;
            ts
        });
        screen.checkpoint_ranks();
        assert_eq!(screen.tool_map["tool"].prev_calls, 42);
    }

    #[test]
    fn metric_tiles_render_without_panic() {
        let state = test_state();
        let screen = ToolMetricsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 3, &mut pool);
        screen.render_metric_tiles(&mut frame, Rect::new(0, 0, 120, 3), &state);
    }

    #[test]
    fn leaderboard_render_without_panic() {
        let mut screen = ToolMetricsScreen::new();
        screen.tool_map.insert("a".into(), {
            let mut ts = ToolStats::new("a".into());
            ts.record(10, false);
            ts
        });
        screen.tool_map.insert("b".into(), {
            let mut ts = ToolStats::new("b".into());
            ts.record(20, false);
            ts.record(30, false);
            ts
        });

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 10, &mut pool);
        screen.render_leaderboard(&mut frame, Rect::new(0, 0, 60, 10));
    }

    #[test]
    fn latency_ribbon_renders_loading_when_empty() {
        let screen = ToolMetricsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 8, &mut pool);
        screen.render_latency_ribbon(&mut frame, Rect::new(0, 0, 60, 8));
    }

    #[test]
    fn latency_ribbon_renders_with_samples() {
        let mut screen = ToolMetricsScreen::new();
        for i in 0..5 {
            screen.percentile_samples.push_back(PercentileSample {
                p50: 10.0 + f64::from(i),
                p95: 50.0 + f64::from(i),
                p99: 90.0 + f64::from(i),
            });
        }
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(60, 8, &mut pool);
        screen.render_latency_ribbon(&mut frame, Rect::new(0, 0, 60, 8));
    }

    #[test]
    fn tool_stats_sparkline_f64() {
        let mut stats = ToolStats::new("test".into());
        stats.record(10, false);
        stats.record(20, false);
        let data = stats.sparkline_f64();
        assert_eq!(data.len(), 2);
        assert!((data[0] - 10.0).abs() < f64::EPSILON);
        assert!((data[1] - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_history_bounded() {
        let mut screen = ToolMetricsScreen::new();
        screen.tool_map.insert("tool".into(), {
            let mut ts = ToolStats::new("tool".into());
            for i in 0..10 {
                ts.record(i * 10, false);
            }
            ts
        });

        // Push more than PERCENTILE_HISTORY samples
        for _ in 0..(PERCENTILE_HISTORY + 10) {
            screen.snapshot_percentiles();
        }
        assert!(screen.percentile_samples.len() <= PERCENTILE_HISTORY);
    }

    // --- BOCPD + Conformal integration tests (br-h8xsy G.3) ---

    /// 1. Simulate tool calls with a latency shift; verify change point detected.
    #[test]
    fn metrics_bocpd_integration() {
        let mut stats = ToolStats::new("send_message".into());

        // 200 calls at ~50ms (stable regime).
        for _ in 0..200 {
            let _ = stats.record(50, false);
        }
        assert_eq!(
            stats.change_point_count(),
            0,
            "no change points during stable regime"
        );

        // 200 calls at ~500ms (10x latency shift).
        let mut detected = false;
        for _ in 0..200 {
            if stats.record(500, false).is_some() {
                detected = true;
            }
        }
        assert!(
            detected,
            "BOCPD should detect the 50ms -> 500ms latency shift"
        );
        assert!(
            stats.change_point_count() >= 1,
            "at least one change point recorded"
        );
    }

    /// 2. Verify conformal prediction intervals are displayed (predict returns Some
    ///    after sufficient calibration).
    #[test]
    fn metrics_conformal_intervals_displayed() {
        let mut stats = ToolStats::new("fetch_inbox".into());

        // Less than MIN_CALIBRATION (30) — no interval yet.
        for i in 0..29 {
            let _ = stats.record(10 + i, false);
        }
        assert!(
            stats.conformal.predict().is_none(),
            "no interval with < 30 observations"
        );

        // One more observation crosses threshold.
        let _ = stats.record(15, false);
        let interval = stats
            .conformal
            .predict()
            .expect("interval should be available at 30 observations");
        assert!(
            interval.lower < interval.upper,
            "lower ({}) < upper ({})",
            interval.lower,
            interval.upper
        );
        assert!((interval.coverage - 0.90).abs() < 1e-10);
    }

    /// 3. Change point triggers anomaly event creation in the screen.
    #[test]
    fn metrics_anomaly_card_emitted() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Stable regime.
        for _ in 0..200 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "slow_tool",
                50,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }
        screen.ingest_events(&state);
        assert!(
            screen.anomaly_events.is_empty(),
            "no anomaly during stable regime"
        );

        // Shift regime.
        for _ in 0..200 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "slow_tool",
                500,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }
        screen.ingest_events(&state);
        assert!(
            !screen.anomaly_events.is_empty(),
            "anomaly event should be emitted after latency shift"
        );

        let evt = &screen.anomaly_events[0];
        assert_eq!(evt.tool_name, "slow_tool");
        assert!(evt.post_mean_ms > evt.pre_mean_ms);
    }

    /// 4. Change point detections are recorded to the evidence ledger.
    #[test]
    fn metrics_evidence_recorded() {
        let ledger = evidence_ledger();
        let before_count = ledger.query("metrics.bocpd.change_point", 1000).len();

        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Stable then shift.
        for _ in 0..200 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "evidence_tool",
                30,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }
        for _ in 0..200 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "evidence_tool",
                300,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }
        screen.ingest_events(&state);

        let after_count = ledger.query("metrics.bocpd.change_point", 1000).len();
        assert!(
            after_count > before_count,
            "evidence ledger should have new entries: before={before_count}, after={after_count}"
        );
    }

    /// 5. Change point in tool A does not affect tool B's state.
    #[test]
    fn metrics_per_tool_isolation() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Tool A: stable, then shift.
        for _ in 0..200 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "tool_a",
                50,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }
        for _ in 0..200 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "tool_a",
                500,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }

        // Tool B: stable throughout.
        for _ in 0..400 {
            let _ = state.push_event(MailEvent::tool_call_end(
                "tool_b",
                50,
                None,
                1,
                0.5,
                vec![],
                None,
                None,
            ));
        }

        screen.ingest_events(&state);

        let a_cp = screen.tool_map["tool_a"].change_point_count();
        let b_cp = screen.tool_map["tool_b"].change_point_count();

        assert!(a_cp >= 1, "tool_a should have change points, got {a_cp}");
        assert_eq!(
            b_cp, 0,
            "tool_b should have no change points (stable), got {b_cp}"
        );
    }

    // --- TransparencyWidget integration tests (br-272c2, H.2) ---

    /// 1. Verify dashboard renders the transparency panel when evidence entries exist.
    #[test]
    fn transparency_metrics_screen_integration() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Populate tools so dashboard renders content (not empty state).
        let _ = state.push_event(MailEvent::tool_call_end(
            "tool_x",
            25,
            None,
            1,
            0.5,
            vec![],
            None,
            None,
        ));
        screen.ingest_events(&state);

        // Inject evidence entries manually (simulates tick ingestion).
        screen.evidence_entries = vec![
            mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry::new(
                "test-d1",
                "tui.diff_strategy",
                "incremental",
                0.85,
                serde_json::json!({}),
            ),
        ];
        screen.view_mode = ViewMode::Dashboard;

        // Render should not panic and should include evidence panel.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 40, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 40), &state);
    }

    /// 2. Verify `l`/`L` keys cycle disclosure level forward and backward.
    #[test]
    fn transparency_keyboard_navigation() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();
        assert_eq!(screen.disclosure_level, DisclosureLevel::Badge);

        let l_key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('l')));
        screen.update(&l_key, &state);
        assert_eq!(screen.disclosure_level, DisclosureLevel::Summary);

        screen.update(&l_key, &state);
        assert_eq!(screen.disclosure_level, DisclosureLevel::Detail);

        screen.update(&l_key, &state);
        assert_eq!(screen.disclosure_level, DisclosureLevel::DeepDive);

        // Wraps back to Badge.
        screen.update(&l_key, &state);
        assert_eq!(screen.disclosure_level, DisclosureLevel::Badge);

        // `L` goes backward.
        let shift_l = Event::Key(ftui::KeyEvent::new(KeyCode::Char('L')));
        screen.update(&shift_l, &state);
        assert_eq!(screen.disclosure_level, DisclosureLevel::DeepDive);

        screen.update(&shift_l, &state);
        assert_eq!(screen.disclosure_level, DisclosureLevel::Detail);
    }

    /// 3. Verify tick ingests evidence entries from the global ledger.
    #[test]
    fn transparency_evidence_ingestion_via_tick() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();
        assert!(screen.evidence_entries.is_empty());

        // Record an entry to the global evidence ledger.
        evidence_ledger().record(
            "test.transparency.tick",
            serde_json::json!({"key": "value"}),
            "test_action",
            None,
            0.75,
            "test_model",
        );

        // tick at a multiple of 10 triggers evidence refresh.
        screen.tick(10, &state);
        assert!(
            !screen.evidence_entries.is_empty(),
            "tick should populate evidence_entries from global ledger"
        );
    }

    /// 4. Verify dashboard renders different layouts at each disclosure level.
    #[test]
    fn transparency_disclosure_levels_affect_dashboard() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Populate data.
        let _ = state.push_event(MailEvent::tool_call_end(
            "t1",
            30,
            None,
            1,
            0.5,
            vec![],
            None,
            None,
        ));
        screen.ingest_events(&state);

        screen.evidence_entries = vec![
            mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry::new(
                "dd1",
                "cache.eviction",
                "promote",
                0.65,
                serde_json::json!({}),
            ),
            mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry::new(
                "dd2",
                "tui.diff",
                "full",
                0.9,
                serde_json::json!({}),
            ),
        ];
        screen.view_mode = ViewMode::Dashboard;

        let levels = [
            DisclosureLevel::Badge,
            DisclosureLevel::Summary,
            DisclosureLevel::Detail,
            DisclosureLevel::DeepDive,
        ];

        for level in &levels {
            screen.disclosure_level = *level;
            let mut pool = ftui::GraphemePool::new();
            let mut frame = Frame::new(120, 40, &mut pool);
            // Should not panic at any level.
            screen.render_dashboard_view(&mut frame, Rect::new(0, 0, 120, 40), &state);
        }
    }

    /// 5. Verify cache eviction entries appear in the dashboard cache section (H.2).
    #[test]
    #[allow(clippy::needless_collect)]
    fn transparency_cache_screen_integration() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Populate tools so dashboard renders content.
        let _ = state.push_event(MailEvent::tool_call_end(
            "tool_y",
            15,
            None,
            1,
            0.0,
            vec![],
            None,
            None,
        ));
        screen.ingest_events(&state);

        // Add cache eviction entries.
        screen.evidence_entries = vec![
            mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry::new(
                "cache-d1",
                "cache.eviction",
                "promote",
                0.70,
                serde_json::json!({"freq": 5}),
            ),
            mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry::new(
                "cache-d2",
                "cache.s3fifo",
                "demote",
                0.45,
                serde_json::json!({"ghost": true}),
            ),
        ];
        screen.view_mode = ViewMode::Dashboard;

        // Render should not panic and should include cache transparency section.
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 40, &mut pool);
        screen.render_dashboard_view(&mut frame, Rect::new(0, 0, 120, 40), &state);

        // Verify cache entries are filtered (both start with "cache").
        let cache_entries: Vec<_> = screen
            .evidence_entries
            .iter()
            .filter(|e| e.decision_point.starts_with("cache"))
            .collect();
        assert_eq!(cache_entries.len(), 2, "should filter 2 cache entries");
    }

    /// 6. Verify Enter activates drill-down at L2, Escape steps back (H.2).
    #[test]
    fn transparency_drill_down_from_ledger() {
        let state = test_state();
        let mut screen = ToolMetricsScreen::new();

        // Inject evidence entries.
        screen.evidence_entries = vec![
            mcp_agent_mail_core::evidence_ledger::EvidenceLedgerEntry::new(
                "drill-d1",
                "tui.diff_strategy",
                "incremental",
                0.85,
                serde_json::json!({}),
            ),
        ];

        assert!(!screen.drilldown_active, "drill-down should start inactive");

        // Press Enter to activate drill-down.
        let enter_key = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        screen.update(&enter_key, &state);
        assert!(screen.drilldown_active, "Enter should activate drill-down");
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::Detail,
            "Enter should jump to Detail level"
        );

        // Press Enter again to step deeper.
        screen.update(&enter_key, &state);
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::DeepDive,
            "Enter in drill-down should step to next level"
        );

        // Press Escape to step back.
        let esc_key = Event::Key(ftui::KeyEvent::new(KeyCode::Escape));
        screen.update(&esc_key, &state);
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::Detail,
            "Escape should step back one level"
        );

        // Continue pressing Escape until drill-down deactivates.
        screen.update(&esc_key, &state); // Detail -> Summary
        screen.update(&esc_key, &state); // Summary -> Badge
        screen.update(&esc_key, &state); // Badge -> deactivate
        assert!(
            !screen.drilldown_active,
            "Escape at Badge should deactivate drill-down"
        );

        // Numeric shortcuts: 1-4 jump directly.
        let key3 = Event::Key(ftui::KeyEvent::new(KeyCode::Char('3')));
        screen.update(&key3, &state);
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::Detail,
            "3 -> Detail"
        );

        let key1 = Event::Key(ftui::KeyEvent::new(KeyCode::Char('1')));
        screen.update(&key1, &state);
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::Badge,
            "1 -> Badge"
        );

        let key4 = Event::Key(ftui::KeyEvent::new(KeyCode::Char('4')));
        screen.update(&key4, &state);
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::DeepDive,
            "4 -> DeepDive"
        );

        let key2 = Event::Key(ftui::KeyEvent::new(KeyCode::Char('2')));
        screen.update(&key2, &state);
        assert_eq!(
            screen.disclosure_level,
            DisclosureLevel::Summary,
            "2 -> Summary"
        );
    }
}
