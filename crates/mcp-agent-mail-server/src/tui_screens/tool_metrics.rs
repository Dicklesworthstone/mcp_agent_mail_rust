//! Tool Metrics screen — per-tool call counts, latency, and error rates.
//!
//! Enhanced with advanced widget integration (br-3vwi.7.5):
//! - `MetricTile` summary KPIs (total calls, avg latency, error rate)
//! - `BarChart` (horizontal) for per-tool latency distribution (p50/p95/p99)
//! - `Leaderboard` for top tools by call count
//! - `WidgetState` for loading/empty/ready states
//! - View mode toggle: table view (default) vs widget dashboard view

use std::collections::{HashMap, VecDeque};

use ftui::layout::Constraint;
use ftui::layout::Rect;
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_extras::charts::{BarChart, BarDirection, BarGroup};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEvent;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::{
    LeaderboardEntry, MetricTile, MetricTrend, PercentileSample, RankChange, WidgetState,
};

const COL_NAME: usize = 0;
const COL_CALLS: usize = 1;
const COL_ERRORS: usize = 2;
const COL_ERR_PCT: usize = 3;
const COL_AVG_MS: usize = 4;

const SORT_LABELS: &[&str] = &["Name", "Calls", "Errors", "Err%", "Avg(ms)"];

/// Max latency samples kept per tool for sparkline rendering.
const LATENCY_HISTORY: usize = 30;

/// Max percentile samples kept for the global latency ribbon.
const PERCENTILE_HISTORY: usize = 60;

/// Unicode block characters for inline sparkline.
const SPARK_CHARS: &[char] = &[
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

/// View mode for the metrics screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Traditional table view with sorting.
    Table,
    /// Widget dashboard view with metric tiles, ribbon, and leaderboard.
    Dashboard,
}

/// Accumulated stats for a single tool.
#[derive(Debug, Clone)]
struct ToolStats {
    name: String,
    calls: u64,
    errors: u64,
    total_duration_ms: u64,
    recent_latencies: VecDeque<u64>,
    /// Previous call count for leaderboard rank-change tracking.
    prev_calls: u64,
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

    fn record(&mut self, duration_ms: u64, is_error: bool) {
        self.calls += 1;
        self.total_duration_ms += duration_ms;
        if is_error {
            self.errors += 1;
        }
        if self.recent_latencies.len() >= LATENCY_HISTORY {
            self.recent_latencies.pop_front();
        }
        self.recent_latencies.push_back(duration_ms);
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
                self.tool_map
                    .entry(tool_name.clone())
                    .or_insert_with(|| ToolStats::new(tool_name.clone()))
                    .record(*duration_ms, is_error);
            }
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
                _ => std::cmp::Ordering::Equal,
            };
            if self.sort_asc { cmp } else { cmp.reverse() }
        });
        self.sorted_tools = tools.iter().map(|t| t.name.clone()).collect();

        // Clamp selection
        if let Some(sel) = self.table_state.selected {
            if sel >= self.sorted_tools.len() {
                self.table_state.selected = if self.sorted_tools.is_empty() {
                    None
                } else {
                    Some(self.sorted_tools.len() - 1)
                };
            }
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

    /// Render the table view (original view).
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
        let summary = format!(
            " {} tools | {} calls | {} errors | avg {}ms | Sort: {}{} | v=dashboard",
            self.tool_map.len(),
            total_calls,
            total_errors,
            avg_ms,
            sort_label,
            sort_indicator,
        );
        let p = Paragraph::new(summary);
        p.render(header_area, frame);

        // Table
        let header = Row::new(["Tool Name", "Calls", "Errors", "Err%", "Avg(ms)", "Trend"])
            .style(Style::default().bold());

        let rows: Vec<Row> = self
            .sorted_tools
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                let stats = self.tool_map.get(name)?;
                let err_pct = format!("{:.1}%", stats.err_pct());
                let spark = stats.sparkline_str();
                let style = if Some(i) == self.table_state.selected {
                    Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
                } else if stats.err_pct() > 5.0 {
                    Style::default().fg(tp.severity_error)
                } else {
                    Style::default()
                };
                Some(
                    Row::new([
                        stats.name.clone(),
                        format!("{}", stats.calls),
                        format!("{}", stats.errors),
                        err_pct,
                        format!("{}", stats.avg_ms()),
                        spark,
                    ])
                    .style(style),
                )
            })
            .collect();

        let widths = [
            Constraint::Percentage(25.0),
            Constraint::Percentage(12.0),
            Constraint::Percentage(12.0),
            Constraint::Percentage(10.0),
            Constraint::Percentage(12.0),
            Constraint::Percentage(29.0),
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
        if self.tool_map.is_empty() {
            let widget: WidgetState<'_, Paragraph<'_>> = WidgetState::Empty {
                message: "No tool calls recorded yet",
            };
            widget.render(area, frame);
            return;
        }

        // Layout: metric tiles (3h) + ribbon (8h) + leaderboard (rest)
        let tiles_h = 3_u16.min(area.height);
        let remaining = area.height.saturating_sub(tiles_h);
        let ribbon_h = if remaining > 12 { 8_u16 } else { remaining / 2 };
        let leader_h = remaining.saturating_sub(ribbon_h);

        let tiles_area = Rect::new(area.x, area.y, area.width, tiles_h);
        let ribbon_area = Rect::new(area.x, area.y + tiles_h, area.width, ribbon_h);
        let leader_area = Rect::new(area.x, area.y + tiles_h + ribbon_h, area.width, leader_h);

        // --- Metric Tiles ---
        self.render_metric_tiles(frame, tiles_area, state);

        // --- Percentile Ribbon ---
        if ribbon_h >= 3 {
            self.render_latency_ribbon(frame, ribbon_area);
        }

        // --- Leaderboard ---
        if leader_h >= 3 {
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
        if self.tool_map.is_empty()
            || self
                .tool_map
                .values()
                .all(|ts| ts.recent_latencies.is_empty())
        {
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

        // Build bar groups sorted by P99 descending (worst latency first, br-333hh).
        let mut sorted: Vec<&ToolStats> = self
            .tool_map
            .values()
            .filter(|ts| !ts.recent_latencies.is_empty())
            .collect();
        sorted.sort_by(|a, b| {
            b.percentile(99.0)
                .partial_cmp(&a.percentile(99.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Cap at 15 tools or available height, whichever is smaller (br-333hh).
        let max_groups = ((inner.height as usize) + 1) / 4;
        let visible = sorted.len().min(max_groups.max(1)).min(15);

        let groups: Vec<BarGroup<'_>> = sorted[..visible]
            .iter()
            .map(|ts| {
                BarGroup::new(
                    &ts.name,
                    vec![
                        ts.percentile(50.0),
                        ts.percentile(95.0),
                        ts.percentile(99.0),
                    ],
                )
            })
            .collect();

        // Severity-based coloring (br-333hh): green < 100ms, yellow < 500ms, red >= 500ms.
        let max_p99 = sorted[..visible]
            .iter()
            .map(|ts| ts.percentile(99.0))
            .fold(0.0_f64, f64::max);
        let severity_color = if max_p99 < 100.0 {
            PackedRgba::rgb(0, 200, 80) // green
        } else if max_p99 < 500.0 {
            PackedRgba::rgb(240, 180, 0) // yellow
        } else {
            PackedRgba::rgb(240, 60, 60) // red
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
}

impl Default for ToolMetricsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for ToolMetricsScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
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
                    _ => {}
                }
            }
        }
        Cmd::None
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        self.ingest_events(state);
        if tick_count % 10 == 0 {
            self.rebuild_sorted();
            self.snapshot_percentiles();
            self.snapshot_tick += 1;
        }
        // Checkpoint ranks every ~50 ticks for change tracking.
        if tick_count % 50 == 0 {
            self.checkpoint_ranks();
        }
        self.sync_focused_event();
    }

    fn focused_event(&self) -> Option<&crate::tui_events::MailEvent> {
        self.focused_synthetic.as_ref()
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.height < 3 || area.width < 30 {
            return;
        }

        match self.view_mode {
            ViewMode::Table => self.render_table_view(frame, area),
            ViewMode::Dashboard => self.render_dashboard_view(frame, area, state),
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
        ]
    }

    fn receive_deep_link(&mut self, target: &DeepLinkTarget) -> bool {
        if let DeepLinkTarget::ToolByName(name) = target {
            if let Some(pos) = self.sorted_tools.iter().position(|t| t == name) {
                self.table_state.selected = Some(pos);
                return true;
            }
        }
        false
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

    fn test_state() -> std::sync::Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
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
}
