//! Tool Metrics screen — per-tool call counts, latency, and error rates.

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
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::MailEvent;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};

const COL_NAME: usize = 0;
const COL_CALLS: usize = 1;
const COL_ERRORS: usize = 2;
const COL_ERR_PCT: usize = 3;
const COL_AVG_MS: usize = 4;

const SORT_LABELS: &[&str] = &["Name", "Calls", "Errors", "Err%", "Avg(ms)"];

/// Max latency samples kept per tool for sparkline rendering.
const LATENCY_HISTORY: usize = 30;

/// Unicode block characters for inline sparkline.
const SPARK_CHARS: &[char] = &[
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

/// Accumulated stats for a single tool.
#[derive(Debug, Clone)]
struct ToolStats {
    name: String,
    calls: u64,
    errors: u64,
    total_duration_ms: u64,
    recent_latencies: VecDeque<u64>,
}

impl ToolStats {
    fn new(name: String) -> Self {
        Self {
            name,
            calls: 0,
            errors: 0,
            total_duration_ms: 0,
            recent_latencies: VecDeque::with_capacity(LATENCY_HISTORY),
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
}

pub struct ToolMetricsScreen {
    table_state: TableState,
    tool_map: HashMap<String, ToolStats>,
    sorted_tools: Vec<String>,
    sort_col: usize,
    sort_asc: bool,
    last_seq: u64,
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
        }
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
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if area.height < 3 || area.width < 30 {
            return;
        }

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
            " {} tools | {} calls | {} errors | avg {}ms | Sort: {}{}",
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
                    Style::default()
                        .fg(PackedRgba::rgb(0, 0, 0))
                        .bg(PackedRgba::rgb(100, 200, 230))
                } else if stats.err_pct() > 5.0 {
                    Style::default().fg(PackedRgba::rgb(255, 100, 100))
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
            .border_type(BorderType::Rounded);

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(
                Style::default()
                    .fg(PackedRgba::rgb(0, 0, 0))
                    .bg(PackedRgba::rgb(100, 200, 230)),
            );

        let mut ts = self.table_state.clone();
        StatefulWidget::render(&table, table_area, frame, &mut ts);
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
        assert!(bindings.len() >= 3);
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
}
