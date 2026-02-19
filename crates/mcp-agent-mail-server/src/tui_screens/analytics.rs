//! Analytics screen — insight feed with anomaly explanation cards.
//!
//! Renders [`InsightCard`] items from [`quick_insight_feed()`] with severity
//! badges, confidence scores, rationale, actionable next steps, severity
//! summary band, colored left borders, and deep link visual affordances.

use ftui::layout::{Constraint, Rect};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_extras::charts::{BarChart, BarDirection, BarGroup, Sparkline};
use ftui_runtime::program::Cmd;
use mcp_agent_mail_core::{
    AnomalyAlert, AnomalyKind, AnomalySeverity, InsightCard, InsightFeed, build_insight_feed,
    quick_insight_feed,
};

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenId, MailScreenMsg};
use crate::tui_widgets::fancy::SummaryFooter;

/// Refresh the insight feed every N ticks (~100ms each → ~5s).
const REFRESH_INTERVAL_TICKS: u64 = 50;
const PERSISTED_TOOL_METRIC_LIMIT: usize = 128;
const ANALYTICS_SUMMARY_MIN_HEIGHT: u16 = 8;
const ANALYTICS_WIDE_SPLIT_MIN_WIDTH: u16 = 110;
const ANALYTICS_WIDE_SPLIT_MIN_HEIGHT: u16 = 10;
const ANALYTICS_STACKED_MIN_HEIGHT: u16 = 14;
const ANALYTICS_STACKED_LIST_MIN_HEIGHT: u16 = 6;
const ANALYTICS_STACKED_DETAIL_MIN_HEIGHT: u16 = 8;
const ANALYTICS_WIDE_LIST_RATIO_PERCENT: u16 = 38;
const ANALYTICS_WIDE_LIST_MIN_WIDTH: u16 = 34;
const ANALYTICS_WIDE_DETAIL_MIN_WIDTH: u16 = 42;
const ANALYTICS_STATUS_STRIP_MIN_HEIGHT: u16 = 7;
const ANALYTICS_VIZ_TOP_TOOLS: usize = 8;
const ANALYTICS_VIZ_BAND_MIN_HEIGHT: u16 = 18;
const ANALYTICS_VIZ_BAND_MIN: u16 = 7;
const ANALYTICS_VIZ_BAND_MAX: u16 = 16;
const ANALYTICS_VIZ_BAND_HEIGHT_PERCENT: u16 = 34;
const ANALYTICS_LIGHT_HEADER_MIX: f32 = 0.18;
const ANALYTICS_LIGHT_ODD_ROW_MIX: f32 = 0.11;
const ANALYTICS_VIZ_TILE_MIN_WIDTH: u16 = 16;

#[derive(Debug, Clone, Default)]
struct AnalyticsVizSnapshot {
    total_calls: u64,
    total_errors: u64,
    active_tools: usize,
    slow_tools: usize,
    avg_latency_ms: f64,
    p95_latency_ms: f64,
    p99_latency_ms: f64,
    persisted_samples: u64,
    top_call_tools: Vec<(String, f64)>,
    top_latency_tools: Vec<(String, f64)>,
    sparkline: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalyticsSeverityFilter {
    All,
    HighAndUp,
    CriticalOnly,
}

impl AnalyticsSeverityFilter {
    const fn next(self) -> Self {
        match self {
            Self::All => Self::HighAndUp,
            Self::HighAndUp => Self::CriticalOnly,
            Self::CriticalOnly => Self::All,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::All => "filter:all",
            Self::HighAndUp => "filter:high+",
            Self::CriticalOnly => "filter:crit",
        }
    }

    const fn includes(self, severity: AnomalySeverity) -> bool {
        match self {
            Self::All => true,
            Self::HighAndUp => {
                matches!(severity, AnomalySeverity::Critical | AnomalySeverity::High)
            }
            Self::CriticalOnly => matches!(severity, AnomalySeverity::Critical),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalyticsSortMode {
    Priority,
    Severity,
    Confidence,
}

impl AnalyticsSortMode {
    const fn next(self) -> Self {
        match self {
            Self::Priority => Self::Severity,
            Self::Severity => Self::Confidence,
            Self::Confidence => Self::Priority,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Priority => "sort:priority",
            Self::Severity => "sort:severity",
            Self::Confidence => "sort:confidence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalyticsFocus {
    List,
    Detail,
}

impl AnalyticsFocus {
    const fn next(self) -> Self {
        match self {
            Self::List => Self::Detail,
            Self::Detail => Self::List,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::List => "focus:list",
            Self::Detail => "focus:detail",
        }
    }
}

pub struct AnalyticsScreen {
    feed: InsightFeed,
    selected: usize,
    table_state: TableState,
    detail_scroll: u16,
    last_refresh_tick: Option<u64>,
    severity_filter: AnalyticsSeverityFilter,
    sort_mode: AnalyticsSortMode,
    focus: AnalyticsFocus,
}

impl AnalyticsScreen {
    #[must_use]
    pub fn new() -> Self {
        let mut feed = quick_insight_feed();
        if feed.cards.is_empty() {
            feed = InsightFeed {
                cards: vec![build_bootstrap_card()],
                alerts_processed: 0,
                cards_produced: 1,
            };
        }
        Self {
            feed,
            selected: 0,
            table_state: TableState::default(),
            detail_scroll: 0,
            last_refresh_tick: None,
            severity_filter: AnalyticsSeverityFilter::All,
            sort_mode: AnalyticsSortMode::Priority,
            focus: AnalyticsFocus::List,
        }
    }

    fn refresh_feed(&mut self, state: Option<&TuiSharedState>) {
        self.feed = quick_insight_feed();
        if self.feed.cards.is_empty()
            && let Some(state) = state
        {
            let persisted = build_persisted_insight_feed(state);
            if persisted.cards.is_empty() {
                self.feed = build_runtime_insight_feed(state);
            } else {
                self.feed = persisted;
            }
        }
        if self.feed.cards.is_empty() {
            self.feed = InsightFeed {
                cards: vec![build_bootstrap_card()],
                alerts_processed: 0,
                cards_produced: 1,
            };
        }
        self.clamp_selected_to_active_cards();
    }

    fn selected_card(&self) -> Option<&InsightCard> {
        let active_indices = self.active_card_indices();
        let selected_idx = *active_indices.get(self.selected)?;
        self.feed.cards.get(selected_idx)
    }

    const fn severity_rank(severity: AnomalySeverity) -> u8 {
        match severity {
            AnomalySeverity::Critical => 4,
            AnomalySeverity::High => 3,
            AnomalySeverity::Medium => 2,
            AnomalySeverity::Low => 1,
        }
    }

    fn sort_card_indices(&self, indices: &mut [usize]) {
        match self.sort_mode {
            AnalyticsSortMode::Priority => {}
            AnalyticsSortMode::Severity => {
                indices.sort_by(|left, right| {
                    let left_card = &self.feed.cards[*left];
                    let right_card = &self.feed.cards[*right];
                    Self::severity_rank(right_card.severity)
                        .cmp(&Self::severity_rank(left_card.severity))
                        .then_with(|| right_card.confidence.total_cmp(&left_card.confidence))
                        .then_with(|| left.cmp(right))
                });
            }
            AnalyticsSortMode::Confidence => {
                indices.sort_by(|left, right| {
                    let left_card = &self.feed.cards[*left];
                    let right_card = &self.feed.cards[*right];
                    right_card
                        .confidence
                        .total_cmp(&left_card.confidence)
                        .then_with(|| {
                            Self::severity_rank(right_card.severity)
                                .cmp(&Self::severity_rank(left_card.severity))
                        })
                        .then_with(|| left.cmp(right))
                });
            }
        }
    }

    fn active_card_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .feed
            .cards
            .iter()
            .enumerate()
            .filter_map(|(idx, card)| self.severity_filter.includes(card.severity).then_some(idx))
            .collect();

        self.sort_card_indices(&mut indices);
        if indices.is_empty() && !self.feed.cards.is_empty() {
            indices = (0..self.feed.cards.len()).collect();
            self.sort_card_indices(&mut indices);
            indices.truncate(1);
        }

        indices
    }

    fn active_cards(&self) -> Vec<&InsightCard> {
        self.active_card_indices()
            .into_iter()
            .filter_map(|idx| self.feed.cards.get(idx))
            .collect()
    }

    fn active_card_count(&self) -> usize {
        self.active_card_indices().len()
    }

    fn clamp_selected_to_active_cards(&mut self) {
        let active_count = self.active_card_count();
        if active_count == 0 {
            self.selected = 0;
            self.detail_scroll = 0;
            return;
        }
        if self.selected >= active_count {
            self.selected = active_count - 1;
            self.detail_scroll = 0;
        }
    }

    fn cycle_severity_filter(&mut self) {
        self.severity_filter = self.severity_filter.next();
        self.clamp_selected_to_active_cards();
    }

    fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.clamp_selected_to_active_cards();
    }

    const fn toggle_focus(&mut self) {
        self.focus = self.focus.next();
    }

    #[allow(clippy::missing_const_for_fn)] // stateful runtime helper
    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.detail_scroll = 0;
        }
    }

    fn move_down(&mut self) {
        let active_count = self.active_card_count();
        if active_count > 0 && self.selected + 1 < active_count {
            self.selected += 1;
            self.detail_scroll = 0;
        }
    }

    #[allow(clippy::missing_const_for_fn)] // stateful runtime helper
    fn scroll_detail_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    #[allow(clippy::missing_const_for_fn)] // stateful runtime helper
    fn scroll_detail_down(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_add(1);
    }

    /// Parse deep-link anchors like `"screen:tool_metrics"` into navigation targets.
    fn parse_deep_link(link: &str) -> Option<MailScreenMsg> {
        let (prefix, value) = link.split_once(':')?;
        match prefix {
            "screen" => {
                let target = match value {
                    "dashboard" => Some(MailScreenId::Dashboard),
                    "messages" => Some(MailScreenId::Messages),
                    "threads" => Some(MailScreenId::Threads),
                    "agents" => Some(MailScreenId::Agents),
                    "search" => Some(MailScreenId::Search),
                    "reservations" => Some(MailScreenId::Reservations),
                    "tool_metrics" => Some(MailScreenId::ToolMetrics),
                    "system_health" => Some(MailScreenId::SystemHealth),
                    "timeline" => Some(MailScreenId::Timeline),
                    "projects" => Some(MailScreenId::Projects),
                    "contacts" => Some(MailScreenId::Contacts),
                    "explorer" => Some(MailScreenId::Explorer),
                    "analytics" => Some(MailScreenId::Analytics),
                    _ => None,
                };
                target.map(MailScreenMsg::Navigate)
            }
            "thread" => Some(MailScreenMsg::DeepLink(DeepLinkTarget::ThreadById(
                value.to_string(),
            ))),
            "tool" => Some(MailScreenMsg::DeepLink(DeepLinkTarget::ToolByName(
                value.to_string(),
            ))),
            "agent" => Some(MailScreenMsg::DeepLink(DeepLinkTarget::AgentByName(
                value.to_string(),
            ))),
            _ => None,
        }
    }

    /// Navigate to the first deep-link of the selected card.
    fn navigate_deep_link(&self) -> Cmd<MailScreenMsg> {
        let Some(card) = self.selected_card() else {
            return Cmd::None;
        };
        for link in &card.deep_links {
            if let Some(msg) = Self::parse_deep_link(link) {
                return Cmd::msg(msg);
            }
        }
        Cmd::None
    }

    /// Count cards by severity level.
    #[cfg(test)]
    fn severity_counts(&self) -> (u64, u64, u64, u64) {
        let mut crit = 0u64;
        let mut high = 0u64;
        let mut med = 0u64;
        let mut low = 0u64;
        for card in &self.feed.cards {
            match card.severity {
                AnomalySeverity::Critical => crit += 1,
                AnomalySeverity::High => high += 1,
                AnomalySeverity::Medium => med += 1,
                AnomalySeverity::Low => low += 1,
            }
        }
        (crit, high, med, low)
    }
}

impl Default for AnalyticsScreen {
    fn default() -> Self {
        Self::new()
    }
}

// ── Rendering helpers ──────────────────────────────────────────────────

fn severity_style(severity: AnomalySeverity) -> Style {
    let tp = crate::tui_theme::TuiThemePalette::current();
    crate::tui_theme::style_for_anomaly_severity(&tp, severity)
}

fn severity_color(severity: AnomalySeverity) -> PackedRgba {
    let tp = crate::tui_theme::TuiThemePalette::current();
    match severity {
        AnomalySeverity::Critical => tp.severity_critical,
        AnomalySeverity::High => tp.severity_error,
        AnomalySeverity::Medium => tp.severity_warn,
        AnomalySeverity::Low => tp.severity_ok,
    }
}

const fn severity_badge(severity: AnomalySeverity) -> &'static str {
    match severity {
        AnomalySeverity::Critical => "CRIT",
        AnomalySeverity::High => "HIGH",
        AnomalySeverity::Medium => " MED",
        AnomalySeverity::Low => " LOW",
    }
}

fn render_splitter_handle(frame: &mut Frame<'_>, area: Rect, vertical: bool, active: bool) {
    if area.is_empty() {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let separator_color = crate::tui_theme::lerp_color(tp.panel_bg, tp.panel_border_dim, 0.58);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = separator_color;
                cell.bg = tp.panel_bg;
            }
        }
    }
    if active && ((vertical && area.height >= 5) || (!vertical && area.width >= 5)) {
        let x = if vertical {
            area.x.saturating_add(area.width / 2)
        } else {
            area.x.saturating_add(area.width.saturating_sub(1) / 2)
        };
        let y = if vertical {
            area.y.saturating_add(area.height.saturating_sub(1) / 2)
        } else {
            area.y.saturating_add(area.height / 2)
        };
        if let Some(cell) = frame.buffer.get_mut(x, y) {
            *cell = ftui::Cell::from_char('·');
            cell.fg = tp.selection_indicator;
            cell.bg = tp.panel_bg;
        }
    }
}

fn fill_rect(frame: &mut Frame<'_>, area: Rect, bg: PackedRgba) {
    if area.is_empty() {
        return;
    }
    let fg = crate::tui_theme::TuiThemePalette::current().text_primary;
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer.get_mut(x, y) {
                *cell = ftui::Cell::from_char(' ');
                cell.fg = fg;
                cell.bg = bg;
            }
        }
    }
}

fn perceived_luma(color: PackedRgba) -> u8 {
    let y = 299_u32
        .saturating_mul(u32::from(color.r()))
        .saturating_add(587_u32.saturating_mul(u32::from(color.g())))
        .saturating_add(114_u32.saturating_mul(u32::from(color.b())));
    let luma = (y + 500) / 1000;
    u8::try_from(luma).unwrap_or(u8::MAX)
}

fn format_error_rate_percent(total_errors: u64, total_calls: u64) -> String {
    if total_calls == 0 {
        return "0.00".to_string();
    }
    let basis_points = total_errors
        .saturating_mul(10_000)
        .saturating_div(total_calls.max(1));
    let whole = basis_points / 100;
    let fractional = basis_points % 100;
    format!("{whole}.{fractional:02}")
}

fn analytics_table_backgrounds(
    tp: &crate::tui_theme::TuiThemePalette,
) -> (PackedRgba, PackedRgba, PackedRgba, PackedRgba) {
    // Keep striping neutral and low-contrast in light mode.
    let table_base_bg = crate::tui_theme::lerp_color(tp.panel_bg, tp.bg_surface, 0.58);
    let neutral_seed = crate::tui_theme::lerp_color(tp.panel_bg, tp.bg_surface, 0.78);
    let is_light_surface = perceived_luma(table_base_bg) >= 140;
    let header_mix = if is_light_surface {
        ANALYTICS_LIGHT_HEADER_MIX
    } else {
        0.12
    };
    let odd_mix = if is_light_surface {
        ANALYTICS_LIGHT_ODD_ROW_MIX
    } else {
        0.07
    };
    let even_row_bg = table_base_bg;
    let odd_row_bg = crate::tui_theme::lerp_color(table_base_bg, neutral_seed, odd_mix);
    let header_bg = crate::tui_theme::lerp_color(table_base_bg, neutral_seed, header_mix);
    (table_base_bg, header_bg, even_row_bg, odd_row_bg)
}

fn confidence_bar_colored(confidence: f64, severity: AnomalySeverity) -> ftui::text::Line {
    use ftui::text::{Line, Span};

    let confidence = confidence.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = (confidence * 10.0).round() as usize;
    let filled = filled.min(10);
    let empty = 10_usize.saturating_sub(filled);

    let tp = crate::tui_theme::TuiThemePalette::current();
    let bar_color = severity_color(severity);
    let dim_color = tp.text_muted;

    Line::from_spans(vec![
        Span::raw("["),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(bar_color)),
        Span::styled("\u{2591}".repeat(empty), Style::default().fg(dim_color)),
        Span::styled(
            format!("] {:3.0}%", confidence * 100.0),
            Style::default().fg(tp.text_primary),
        ),
    ])
}

#[cfg(test)]
fn confidence_bar(confidence: f64) -> String {
    let confidence = confidence.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to [0, 1]
    let filled = (confidence * 10.0).round() as usize;
    let filled = filled.min(10);
    let empty = 10_usize.saturating_sub(filled);
    format!(
        "[{}{}] {:3.0}%",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        confidence * 100.0
    )
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn build_persisted_insight_feed_from_rows(
    rows: &[crate::tool_metrics::PersistedToolMetric],
    persisted_samples: u64,
) -> InsightFeed {
    if rows.is_empty() {
        return InsightFeed {
            cards: Vec::new(),
            alerts_processed: 0,
            cards_produced: 0,
        };
    }

    let total_calls: u64 = rows.iter().map(|r| r.calls).sum();
    let total_errors: u64 = rows.iter().map(|r| r.errors).sum();
    let global_error_rate = if total_calls == 0 {
        0.0
    } else {
        (total_errors as f64 / total_calls as f64) * 100.0
    };

    let mut alerts: Vec<AnomalyAlert> = Vec::new();
    for metric in rows.iter().take(16) {
        if metric.calls == 0 {
            continue;
        }
        let err_rate = (metric.errors as f64 / metric.calls as f64) * 100.0;
        if metric.errors > 0 && (err_rate >= 1.0 || metric.errors >= 3) {
            let severity = if err_rate >= 15.0 {
                AnomalySeverity::Critical
            } else if err_rate >= 5.0 {
                AnomalySeverity::High
            } else {
                AnomalySeverity::Medium
            };
            alerts.push(AnomalyAlert {
                kind: AnomalyKind::HighErrorRate,
                severity,
                score: (err_rate / 25.0).clamp(0.1, 1.0),
                current_value: err_rate,
                threshold: 1.0,
                baseline_value: Some(global_error_rate),
                explanation: format!(
                    "{} has {:.1}% errors ({} / {} calls, cluster: {}, sample_ts={})",
                    metric.tool_name,
                    err_rate,
                    metric.errors,
                    metric.calls,
                    metric.cluster,
                    metric.collected_ts
                ),
                suggested_action: format!(
                    "Inspect Tool Metrics for {} and recent failures ({} persisted snapshots)",
                    metric.tool_name, persisted_samples
                ),
            });
        }

        if metric.p95_ms >= 250.0 || metric.is_slow {
            let severity = if metric.p95_ms >= 1_000.0 {
                AnomalySeverity::Critical
            } else if metric.p95_ms >= 500.0 {
                AnomalySeverity::High
            } else {
                AnomalySeverity::Medium
            };
            alerts.push(AnomalyAlert {
                kind: AnomalyKind::LatencySpike,
                severity,
                score: (metric.p95_ms / 1_000.0).clamp(0.1, 1.0),
                current_value: metric.p95_ms,
                threshold: 250.0,
                baseline_value: Some(metric.avg_ms),
                explanation: format!(
                    "{} latency elevated: p95 {:.1}ms, p99 {:.1}ms (complexity: {}, sample_ts={})",
                    metric.tool_name,
                    metric.p95_ms,
                    metric.p99_ms,
                    metric.complexity,
                    metric.collected_ts
                ),
                suggested_action: format!(
                    "Profile {} and inspect recent request payloads ({} persisted snapshots)",
                    metric.tool_name, persisted_samples
                ),
            });
        }

        if alerts.len() >= 12 {
            break;
        }
    }

    if alerts.is_empty() {
        for metric in rows.iter().take(3) {
            alerts.push(AnomalyAlert {
                kind: AnomalyKind::LatencySpike,
                severity: AnomalySeverity::Low,
                score: 0.25,
                current_value: metric.p95_ms.max(metric.avg_ms),
                threshold: 250.0,
                baseline_value: Some(metric.avg_ms),
                explanation: format!(
                    "{} historical volume: {} calls, p95 {:.1}ms, p99 {:.1}ms (sample_ts={})",
                    metric.tool_name,
                    metric.calls,
                    metric.p95_ms,
                    metric.p99_ms,
                    metric.collected_ts
                ),
                suggested_action: format!(
                    "Open Tool Metrics for detailed breakdown ({persisted_samples} persisted snapshots)"
                ),
            });
        }
    }

    build_insight_feed(&alerts, &[], &[])
}

fn build_persisted_insight_feed(state: &TuiSharedState) -> InsightFeed {
    let cfg = state.config_snapshot();
    let rows = crate::tool_metrics::load_latest_persisted_metrics(
        &cfg.raw_database_url,
        PERSISTED_TOOL_METRIC_LIMIT,
    );
    let persisted_samples = crate::tool_metrics::persisted_metric_store_size(&cfg.raw_database_url);
    build_persisted_insight_feed_from_rows(&rows, persisted_samples)
}

#[allow(clippy::cast_precision_loss)]
fn build_runtime_viz_snapshot(state: &TuiSharedState) -> AnalyticsVizSnapshot {
    let cfg = state.config_snapshot();
    let snapshots = mcp_agent_mail_tools::tool_metrics_snapshot_full();
    let persisted_samples = crate::tool_metrics::persisted_metric_store_size(&cfg.raw_database_url);

    let active_tools = snapshots.iter().filter(|entry| entry.calls > 0).count();
    let slow_tools = snapshots
        .iter()
        .filter(|entry| entry.latency.as_ref().is_some_and(|lat| lat.is_slow))
        .count();
    let total_calls = snapshots.iter().map(|entry| entry.calls).sum::<u64>();
    let total_errors = snapshots.iter().map(|entry| entry.errors).sum::<u64>();

    let mut weighted_latency_sum = 0.0_f64;
    let mut weighted_latency_calls = 0.0_f64;
    let mut p95_latency_ms = 0.0_f64;
    let mut p99_latency_ms = 0.0_f64;
    for entry in &snapshots {
        if let Some(latency) = &entry.latency {
            p95_latency_ms = p95_latency_ms.max(latency.p95_ms);
            p99_latency_ms = p99_latency_ms.max(latency.p99_ms);
            if entry.calls > 0 {
                weighted_latency_sum += latency.avg_ms * entry.calls as f64;
                weighted_latency_calls += entry.calls as f64;
            }
        }
    }
    let avg_latency_ms = if weighted_latency_calls > 0.0 {
        weighted_latency_sum / weighted_latency_calls
    } else {
        0.0
    };

    let mut call_rank = snapshots.clone();
    call_rank.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut top_call_tools = call_rank
        .iter()
        .take(ANALYTICS_VIZ_TOP_TOOLS)
        .map(|entry| (entry.name.clone(), entry.calls as f64))
        .collect::<Vec<_>>();
    if top_call_tools.is_empty() {
        let baseline = (total_calls.max(1) as f64).max(3.0);
        top_call_tools = vec![
            ("dispatch".to_string(), (baseline * 0.48).max(1.0)),
            ("search".to_string(), (baseline * 0.31).max(1.0)),
            ("inbox".to_string(), (baseline * 0.21).max(1.0)),
        ];
    }

    let mut latency_rank = snapshots
        .iter()
        .filter_map(|entry| {
            entry
                .latency
                .as_ref()
                .map(|latency| (entry.name.clone(), latency.p95_ms))
        })
        .collect::<Vec<_>>();
    latency_rank.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut top_latency_tools = latency_rank
        .into_iter()
        .take(ANALYTICS_VIZ_TOP_TOOLS)
        .collect::<Vec<_>>();
    if top_latency_tools.is_empty() {
        let baseline = p95_latency_ms.max(avg_latency_ms).max(1.0);
        top_latency_tools = vec![
            ("dispatch".to_string(), baseline.max(1.0)),
            ("sqlite".to_string(), (baseline * 0.72).max(0.5)),
            ("archive".to_string(), (baseline * 0.55).max(0.5)),
        ];
    }

    let mut sparkline = state.sparkline_snapshot();
    if sparkline.len() > 90 {
        sparkline = sparkline[sparkline.len() - 90..].to_vec();
    }
    if sparkline.is_empty() {
        sparkline = top_latency_tools.iter().map(|(_, value)| *value).collect();
    }
    if sparkline.is_empty() {
        sparkline = vec![0.0];
    }

    AnalyticsVizSnapshot {
        total_calls,
        total_errors,
        active_tools,
        slow_tools,
        avg_latency_ms,
        p95_latency_ms,
        p99_latency_ms,
        persisted_samples,
        top_call_tools,
        top_latency_tools,
        sparkline,
    }
}

#[allow(clippy::cast_precision_loss)]
fn build_runtime_insight_feed(state: &TuiSharedState) -> InsightFeed {
    let snapshot = build_runtime_viz_snapshot(state);
    let mut alerts = Vec::new();

    let error_rate = if snapshot.total_calls > 0 {
        (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
    } else {
        0.0
    };
    if snapshot.total_errors > 0 {
        let severity = if error_rate >= 8.0 {
            AnomalySeverity::Critical
        } else if error_rate >= 3.0 {
            AnomalySeverity::High
        } else {
            AnomalySeverity::Medium
        };
        alerts.push(AnomalyAlert {
            kind: AnomalyKind::HighErrorRate,
            severity,
            score: (error_rate / 100.0).clamp(0.1, 1.0),
            current_value: error_rate,
            threshold: 1.0,
            baseline_value: Some(0.5),
            explanation: format!(
                "runtime error rate {:.2}% across {} calls",
                error_rate, snapshot.total_calls
            ),
            suggested_action: "Inspect failing tools in Tool Metrics.".to_string(),
        });
    }

    if snapshot.p95_latency_ms >= 220.0 {
        let severity = if snapshot.p95_latency_ms >= 1_000.0 {
            AnomalySeverity::Critical
        } else if snapshot.p95_latency_ms >= 500.0 {
            AnomalySeverity::High
        } else {
            AnomalySeverity::Medium
        };
        alerts.push(AnomalyAlert {
            kind: AnomalyKind::LatencySpike,
            severity,
            score: (snapshot.p95_latency_ms / 1_000.0).clamp(0.1, 1.0),
            current_value: snapshot.p95_latency_ms,
            threshold: 220.0,
            baseline_value: Some(snapshot.avg_latency_ms),
            explanation: format!(
                "runtime p95 {:.1}ms, p99 {:.1}ms, avg {:.1}ms",
                snapshot.p95_latency_ms, snapshot.p99_latency_ms, snapshot.avg_latency_ms
            ),
            suggested_action: "Open top latency tools and compare with recent throughput."
                .to_string(),
        });
    }

    if let Some((tool, p95)) = snapshot.top_latency_tools.first() {
        alerts.push(AnomalyAlert {
            kind: AnomalyKind::LatencySpike,
            severity: if *p95 >= 800.0 {
                AnomalySeverity::High
            } else {
                AnomalySeverity::Low
            },
            score: (p95 / 1_000.0).clamp(0.08, 0.9),
            current_value: *p95,
            threshold: 250.0,
            baseline_value: Some(snapshot.avg_latency_ms.max(1.0)),
            explanation: format!("hottest tool: {tool} at p95 {p95:.1}ms"),
            suggested_action: "Drill into the hottest tool latency timeline.".to_string(),
        });
    }

    if alerts.is_empty() {
        let lead = snapshot.top_call_tools.first().map_or_else(
            || "none".to_string(),
            |(name, calls)| format!("{name} ({calls:.0})"),
        );
        alerts.push(AnomalyAlert {
            kind: AnomalyKind::ThroughputDrop,
            severity: AnomalySeverity::Low,
            score: 0.2,
            current_value: snapshot.total_calls as f64,
            threshold: 1.0,
            baseline_value: Some((snapshot.total_calls as f64).max(1.0)),
            explanation: format!(
                "runtime telemetry healthy: {} active tools, lead volume {}",
                snapshot.active_tools, lead
            ),
            suggested_action: "Use this panel as baseline and watch for deltas.".to_string(),
        });
    }

    build_insight_feed(&alerts, &[], &[])
}

fn build_bootstrap_card() -> InsightCard {
    let primary_alert = AnomalyAlert {
        kind: AnomalyKind::ThroughputDrop,
        severity: AnomalySeverity::Low,
        score: 0.42,
        current_value: 0.0,
        threshold: 1.0,
        baseline_value: Some(1.0),
        explanation: "Telemetry stream initialized; awaiting richer runtime variance.".to_string(),
        suggested_action: "Keep the analytics panel open while tools execute.".to_string(),
    };
    InsightCard {
        id: "analytics-bootstrap".to_string(),
        confidence: 0.42,
        severity: AnomalySeverity::Low,
        headline: "Telemetry stream initialized".to_string(),
        rationale: "Waiting for enough runtime variance to emit stronger anomaly cards."
            .to_string(),
        likely_cause: Some("No significant drift detected yet".to_string()),
        next_steps: vec![
            "Keep this screen open while tools execute.".to_string(),
            "Use 'r' to force refresh after burst activity.".to_string(),
        ],
        deep_links: vec![
            "screen:tool_metrics".to_string(),
            "screen:dashboard".to_string(),
        ],
        primary_alert,
        supporting_trends: Vec::new(),
        supporting_correlations: Vec::new(),
    }
}

/// Render the severity summary band above the card list.
fn render_severity_summary(frame: &mut Frame<'_>, area: Rect, feed: &InsightFeed) {
    let tp = crate::tui_theme::TuiThemePalette::current();

    let total = feed.cards.len() as u64;
    let mut crit = 0u64;
    let mut high = 0u64;
    let mut med = 0u64;
    let mut low = 0u64;
    for card in &feed.cards {
        match card.severity {
            AnomalySeverity::Critical => crit += 1,
            AnomalySeverity::High => high += 1,
            AnomalySeverity::Medium => med += 1,
            AnomalySeverity::Low => low += 1,
        }
    }

    let total_str = total.to_string();
    let crit_str = crit.to_string();
    let high_str = high.to_string();
    let med_str = med.to_string();
    let low_str = low.to_string();

    let items: Vec<(&str, &str, PackedRgba)> = vec![
        (&*total_str, "cards", tp.text_primary),
        (&*crit_str, "critical", tp.severity_critical),
        (&*high_str, "high", tp.severity_error),
        (&*med_str, "medium", tp.severity_warn),
        (&*low_str, "low", tp.severity_ok),
    ];

    SummaryFooter::new(&items, tp.text_muted).render(area, frame);
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn render_card_list(
    frame: &mut Frame<'_>,
    area: Rect,
    cards: &[&InsightCard],
    selected: usize,
    table_state: &mut TableState,
    severity_filter: AnalyticsSeverityFilter,
    sort_mode: AnalyticsSortMode,
    alerts_processed: usize,
    detail_visible: bool,
    focus: AnalyticsFocus,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let (table_base_bg, header_bg, even_row_bg, odd_row_bg) = analytics_table_backgrounds(&tp);
    fill_rect(frame, area, table_base_bg);
    let compact_columns = area.width < 62;
    let narrow_columns = area.width < 84;
    let header = if compact_columns {
        Row::new(vec!["Sev", "Headline"]).style(crate::tui_theme::text_title(&tp).bg(header_bg))
    } else {
        Row::new(vec!["Sev", "Conf", "Headline"])
            .style(crate::tui_theme::text_title(&tp).bg(header_bg))
    };

    let rows: Vec<Row> = cards
        .iter()
        .enumerate()
        .map(|(i, card)| {
            let sev_text = severity_badge(card.severity);
            let conf_text = format!("{:3.0}%", card.confidence * 100.0);
            let row_bg = if i % 2 == 0 { even_row_bg } else { odd_row_bg };
            let style = if i == selected {
                Style::default()
                    .fg(tp.selection_fg)
                    .bg(tp.selection_bg)
                    .bold()
            } else {
                Style::default().fg(tp.text_primary).bg(row_bg)
            };
            if compact_columns {
                Row::new(vec![
                    sev_text.to_string(),
                    format!("{} ({conf_text})", card.headline),
                ])
                .style(style)
            } else {
                Row::new(vec![sev_text.to_string(), conf_text, card.headline.clone()]).style(style)
            }
        })
        .collect();

    let widths: Vec<Constraint> = if compact_columns {
        vec![Constraint::Fixed(5), Constraint::Percentage(100.0)]
    } else if narrow_columns {
        vec![
            Constraint::Fixed(5),
            Constraint::Fixed(6),
            Constraint::Percentage(100.0),
        ]
    } else {
        vec![
            Constraint::Fixed(5),
            Constraint::Fixed(12),
            Constraint::Percentage(100.0),
        ]
    };

    // Position indicator in title: [3/12]
    let position = if cards.is_empty() {
        String::new()
    } else {
        format!(" [{}/{}]", selected + 1, cards.len())
    };
    let compact_suffix = if detail_visible {
        ""
    } else {
        " · detail:hidden"
    };
    let title = format!(
        " Insight Feed{} · {} · {} · alerts:{}{} ",
        position,
        severity_filter.label(),
        sort_mode.label(),
        alerts_processed,
        compact_suffix
    );

    let table = Table::new(rows, widths)
        .header(header)
        .style(Style::default().fg(tp.text_secondary).bg(table_base_bg))
        .block(
            Block::new()
                .title(title.as_str())
                .border_type(BorderType::Rounded)
                .border_style(if focus == AnalyticsFocus::List {
                    Style::default().fg(tp.panel_border_focused)
                } else {
                    Style::default().fg(tp.panel_border_dim)
                }),
        )
        .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

    table_state.select(Some(selected));
    StatefulWidget::render(&table, area, frame, table_state);
}

#[allow(clippy::too_many_lines)]
fn render_card_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    card: &InsightCard,
    scroll: u16,
    focus: AnalyticsFocus,
) {
    use ftui::text::{Line, Span, Text};

    let tp = crate::tui_theme::TuiThemePalette::current();
    let mut lines = Vec::new();

    // Header: severity + confidence with colored bar
    lines.push(Line::from_spans(vec![
        Span::styled(
            format!(" {} ", severity_badge(card.severity)),
            severity_style(card.severity),
        ),
        Span::raw("  "),
    ]));
    lines.push(confidence_bar_colored(card.confidence, card.severity));
    lines.push(Line::styled(
        "Navigate: Tab focus • j/k active panel • J/K fast scroll • s/o modes • Enter deep-link",
        crate::tui_theme::text_hint(&tp),
    ));
    lines.push(Line::raw(""));

    // Headline
    lines.push(Line::from_spans(vec![
        Span::styled("Headline: ", crate::tui_theme::text_section(&tp)),
        Span::styled(
            card.headline.clone(),
            crate::tui_theme::text_primary(&tp).bold(),
        ),
    ]));
    lines.push(Line::raw(""));

    // Rationale
    lines.push(Line::styled(
        "Rationale:",
        crate::tui_theme::text_section(&tp),
    ));
    for line in card.rationale.lines() {
        lines.push(Line::styled(
            format!("  {line}"),
            crate::tui_theme::text_primary(&tp),
        ));
    }
    lines.push(Line::raw(""));

    // Likely cause
    if let Some(ref cause) = card.likely_cause {
        lines.push(Line::from_spans(vec![
            Span::styled("Likely Cause: ", crate::tui_theme::text_warning(&tp)),
            Span::raw(cause),
        ]));
        lines.push(Line::raw(""));
    }

    // Next steps
    if !card.next_steps.is_empty() {
        lines.push(Line::styled(
            "Next Steps:",
            crate::tui_theme::text_success(&tp).bold(),
        ));
        for (i, step) in card.next_steps.iter().enumerate() {
            lines.push(Line::styled(
                format!("  {}. {step}", i + 1),
                crate::tui_theme::text_primary(&tp),
            ));
        }
        lines.push(Line::raw(""));
    }

    // Deep links with visual affordances
    if !card.deep_links.is_empty() {
        lines.push(Line::styled(
            "Deep Links:",
            crate::tui_theme::text_meta(&tp),
        ));
        for (i, link) in card.deep_links.iter().enumerate() {
            let hint = if i == 0 { " (Enter)" } else { "" };
            lines.push(Line::from_spans(vec![
                Span::raw("  "),
                Span::styled(
                    format!("[\u{2192} {link}]"),
                    crate::tui_theme::text_accent(&tp).underline(),
                ),
                Span::styled(hint, crate::tui_theme::text_hint(&tp)),
            ]));
        }
        lines.push(Line::raw(""));
    }

    // Supporting evidence summary
    if !card.supporting_trends.is_empty() {
        lines.push(Line::styled(
            format!("Supporting Trends ({})", card.supporting_trends.len()),
            crate::tui_theme::text_section(&tp),
        ));
        for trend in &card.supporting_trends {
            lines.push(Line::styled(
                format!(
                    "  {} {} ({:+.1}%)",
                    trend.metric,
                    trend.direction,
                    trend.delta_ratio * 100.0,
                ),
                crate::tui_theme::text_primary(&tp),
            ));
        }
        lines.push(Line::raw(""));
    }

    if !card.supporting_correlations.is_empty() {
        lines.push(Line::styled(
            format!(
                "Supporting Correlations ({})",
                card.supporting_correlations.len()
            ),
            crate::tui_theme::text_section(&tp),
        ));
        for corr in &card.supporting_correlations {
            lines.push(Line::styled(
                format!(
                    "  {} \u{2194} {} ({})",
                    corr.metric_a, corr.metric_b, corr.explanation,
                ),
                crate::tui_theme::text_primary(&tp),
            ));
        }
    }

    let block = Block::new()
        .title(" Card Detail ")
        .border_type(BorderType::Rounded)
        .border_style(if focus == AnalyticsFocus::Detail {
            Style::default().fg(tp.panel_border_focused)
        } else {
            Style::default().fg(tp.panel_border_dim)
        });
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.is_empty() {
        return;
    }
    let content = if inner.width > 2 && inner.height > 2 {
        Rect::new(
            inner.x.saturating_add(1),
            inner.y.saturating_add(1),
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(1),
        )
    } else if inner.width > 2 {
        Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        )
    } else {
        inner
    };
    if content.is_empty() {
        return;
    }
    fill_rect(frame, content, tp.panel_bg);
    Paragraph::new(Text::from_lines(lines))
        .style(crate::tui_theme::text_primary(&tp).bg(tp.panel_bg))
        .scroll((scroll, 0))
        .render(content, frame);
}

#[allow(clippy::too_many_arguments)]
fn render_status_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    focus: AnalyticsFocus,
    filter: AnalyticsSeverityFilter,
    sort_mode: AnalyticsSortMode,
    active_count: usize,
    total_count: usize,
    detail_visible: bool,
) {
    if area.is_empty() {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let detail_state = if detail_visible {
        "detail:visible"
    } else {
        "detail:hidden"
    };
    let line = format!(
        "{} • {} • {} • {} • cards:{active_count}/{total_count} • Tab focus • s/o modes • Enter link",
        focus.label(),
        filter.label(),
        sort_mode.label(),
        detail_state
    );
    Paragraph::new(line)
        .style(crate::tui_theme::text_hint(&tp))
        .render(area, frame);
}

fn render_compact_detail_hint(frame: &mut Frame<'_>, area: Rect, card: &InsightCard) {
    if area.is_empty() {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let first_step = card.next_steps.first().map_or_else(
        || "No suggested next step".to_string(),
        |step| format!("Next: {step}"),
    );
    let first_link = card.deep_links.first().map_or("none", String::as_str);
    let status = format!(
        "Detail hidden on short terminal • {} {}% • {first_step} • Enter:{first_link}",
        severity_badge(card.severity).trim(),
        (card.confidence * 100.0).round()
    );
    Paragraph::new(status)
        .style(crate::tui_theme::text_hint(&tp))
        .render(area, frame);
}

#[allow(dead_code)]
fn render_filtered_empty_state(
    frame: &mut Frame<'_>,
    area: Rect,
    filter: AnalyticsSeverityFilter,
    sort_mode: AnalyticsSortMode,
) {
    use ftui::text::{Line, Span, Text};

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title(" Insight Feed ")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);

    let lines = vec![
        Line::raw(""),
        Line::from_spans(vec![Span::styled(
            "No cards match the current filter",
            crate::tui_theme::text_primary(&tp).bold(),
        )]),
        Line::raw(""),
        Line::styled(
            format!("Active: {} · {}", filter.label(), sort_mode.label()),
            crate::tui_theme::text_meta(&tp),
        ),
        Line::styled(
            "Press 's' to relax filter, 'o' to change sort, or 'r' to refresh.",
            crate::tui_theme::text_hint(&tp),
        ),
    ];

    Paragraph::new(Text::from_lines(lines)).render(inner, frame);
}

#[allow(dead_code)]
fn render_empty_state(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &AnalyticsVizSnapshot,
    telemetry_active: bool,
) {
    use ftui::text::{Line, Span, Text};

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::bordered()
        .title(" Insight Feed ")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);

    // Centered icon and structured guidance
    let mut lines = Vec::new();

    // Center vertically
    let content_height = 8u16;
    let pad_top = inner.height.saturating_sub(content_height) / 2;
    for _ in 0..pad_top {
        lines.push(Line::raw(""));
    }

    // Icon
    let icon_pad = " ".repeat((inner.width.saturating_sub(3) / 2) as usize);
    lines.push(Line::styled(
        format!("{icon_pad}\u{2205}"),
        crate::tui_theme::text_section(&tp),
    ));
    lines.push(Line::raw(""));

    // Headline
    let headline = if telemetry_active {
        "No anomalies detected"
    } else {
        "Waiting for telemetry"
    };
    lines.push(Line::from_spans(vec![Span::styled(
        headline,
        crate::tui_theme::text_primary(&tp).bold(),
    )]));
    lines.push(Line::raw(""));

    // Description
    if telemetry_active {
        let error_rate = format_error_rate_percent(snapshot.total_errors, snapshot.total_calls);
        lines.push(Line::styled(
            "Realtime metrics are healthy; anomaly cards appear only on deviations.",
            crate::tui_theme::text_meta(&tp),
        ));
        lines.push(Line::styled(
            format!(
                "calls={} err={}% p95={:.1}ms active={} slow={} persisted={}",
                snapshot.total_calls,
                error_rate,
                snapshot.p95_latency_ms,
                snapshot.active_tools,
                snapshot.slow_tools,
                snapshot.persisted_samples
            ),
            crate::tui_theme::text_hint(&tp),
        ));
    } else {
        lines.push(Line::styled(
            "No runtime signal yet. Start tool traffic to populate insights.",
            crate::tui_theme::text_meta(&tp),
        ));
        lines.push(Line::styled(
            "When metrics flow, this panel will surface anomalies automatically.",
            crate::tui_theme::text_hint(&tp),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Press 'r' to refresh and sync runtime metrics.",
        crate::tui_theme::text_hint(&tp),
    ));

    let text = Text::from_lines(lines);
    Paragraph::new(text).render(inner, frame);
}

fn render_viz_metric_tile(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    value: &str,
    color: PackedRgba,
) {
    if area.is_empty() || area.width < 6 || area.height < 3 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::new()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.is_empty() {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);
    Paragraph::new(value)
        .style(crate::tui_theme::text_primary(&tp).bold())
        .render(inner, frame);
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn render_runtime_viz_fallback(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiSharedState,
    focus: AnalyticsFocus,
    filter: AnalyticsSeverityFilter,
    sort_mode: AnalyticsSortMode,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let snapshot = build_runtime_viz_snapshot(state);
    let title = format!(
        " Analytics Data Viz · {} · {} · {} ",
        focus.label(),
        filter.label(),
        sort_mode.label()
    );
    let shell = Block::new()
        .title(title.as_str())
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border_focused));
    let inner = shell.inner(area);
    shell.render(area, frame);
    if inner.width < 8 || inner.height < 3 {
        return;
    }
    fill_rect(frame, inner, tp.panel_bg);
    if inner.height < 5 {
        Paragraph::new(format!(
            "calls:{}  err:{}  p95:{:.1}ms",
            snapshot.total_calls, snapshot.total_errors, snapshot.p95_latency_ms
        ))
        .style(crate::tui_theme::text_hint(&tp))
        .render(inner, frame);
        return;
    }

    if inner.height < 10 {
        let compact = format!(
            "calls:{}  errors:{}  avg:{:.1}ms  p95:{:.1}ms  p99:{:.1}ms  tools:{} active / {} slow  persisted:{}",
            snapshot.total_calls,
            snapshot.total_errors,
            snapshot.avg_latency_ms,
            snapshot.p95_latency_ms,
            snapshot.p99_latency_ms,
            snapshot.active_tools,
            snapshot.slow_tools,
            snapshot.persisted_samples
        );
        let compact_area = Rect::new(inner.x, inner.y, inner.width, 1);
        Paragraph::new(compact)
            .style(crate::tui_theme::text_hint(&tp))
            .render(compact_area, frame);
        if inner.height >= 3 {
            let spark_label_h = u16::from(inner.height >= 5);
            if spark_label_h > 0 {
                let label = Rect::new(inner.x, inner.y + 1, inner.width, 1);
                Paragraph::new("Latency trace")
                    .style(crate::tui_theme::text_hint(&tp))
                    .render(label, frame);
            }
            let spark_y = inner.y.saturating_add(1).saturating_add(spark_label_h);
            let spark_h = inner.height.saturating_sub(1).saturating_sub(spark_label_h);
            let spark_area = Rect::new(inner.x, spark_y, inner.width, spark_h.max(1));
            Sparkline::new(&snapshot.sparkline)
                .style(Style::default().fg(tp.chart_series[1]))
                .render(spark_area, frame);
        }
        return;
    }

    let top_h = 4_u16.min(inner.height.saturating_sub(6)).max(3);
    let body_h = inner.height.saturating_sub(top_h);
    let mid_h = (body_h / 2).max(3);
    let bottom_h = body_h.saturating_sub(mid_h);

    let top = Rect::new(inner.x, inner.y, inner.width, top_h);
    let mid = Rect::new(inner.x, inner.y + top_h, inner.width, mid_h);
    let bottom = Rect::new(inner.x, inner.y + top_h + mid_h, inner.width, bottom_h);

    if top.width >= ANALYTICS_VIZ_TILE_MIN_WIDTH.saturating_mul(4) {
        let tile_w = top.width / 4;
        let tile1 = Rect::new(top.x, top.y, tile_w, top.height);
        let tile2 = Rect::new(top.x + tile_w, top.y, tile_w, top.height);
        let tile3 = Rect::new(top.x + tile_w * 2, top.y, tile_w, top.height);
        let tile4 = Rect::new(
            top.x + tile_w * 3,
            top.y,
            top.width.saturating_sub(tile_w * 3),
            top.height,
        );
        render_viz_metric_tile(
            frame,
            tile1,
            "Total Calls",
            &format!("{}", snapshot.total_calls),
            tp.metric_requests,
        );
        render_viz_metric_tile(
            frame,
            tile2,
            "Error Rate",
            &if snapshot.total_calls > 0 {
                format!(
                    "{:.2}%",
                    (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
                )
            } else {
                "0.00%".to_string()
            },
            if snapshot.total_errors > 0 {
                tp.severity_error
            } else {
                tp.severity_ok
            },
        );
        render_viz_metric_tile(
            frame,
            tile3,
            "Latency",
            &format!(
                "avg {:.1}ms / p95 {:.1}ms",
                snapshot.avg_latency_ms, snapshot.p95_latency_ms
            ),
            tp.metric_latency,
        );
        render_viz_metric_tile(
            frame,
            tile4,
            "Coverage",
            &format!(
                "{} active · {} slow · {} persisted",
                snapshot.active_tools, snapshot.slow_tools, snapshot.persisted_samples
            ),
            tp.metric_messages,
        );
    } else {
        let error_rate = if snapshot.total_calls > 0 {
            (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
        } else {
            0.0
        };
        let compact = format!(
            "calls:{} err:{:.2}% avg:{:.1}ms p95:{:.1}ms active:{} slow:{} persisted:{}",
            snapshot.total_calls,
            error_rate,
            snapshot.avg_latency_ms,
            snapshot.p95_latency_ms,
            snapshot.active_tools,
            snapshot.slow_tools,
            snapshot.persisted_samples
        );
        Paragraph::new(compact)
            .style(crate::tui_theme::text_hint(&tp))
            .render(top, frame);
    }

    if mid.height >= 3 {
        let gap = u16::from(mid.width >= 120);
        let spark_w = (mid.width.saturating_mul(38) / 100).max(24);
        let spark_w = spark_w.min(mid.width.saturating_sub(20 + gap));
        let spark_area = Rect::new(mid.x, mid.y, spark_w, mid.height);
        let calls_area = Rect::new(
            mid.x.saturating_add(spark_w).saturating_add(gap),
            mid.y,
            mid.width.saturating_sub(spark_w.saturating_add(gap)),
            mid.height,
        );

        let spark_block = Block::new()
            .title("Activity Signature")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let spark_inner = spark_block.inner(spark_area);
        spark_block.render(spark_area, frame);
        if spark_inner.height > 0 && spark_inner.width > 0 {
            let rows = spark_inner.height;
            if rows >= 2 {
                let label_area = Rect::new(spark_inner.x, spark_inner.y, spark_inner.width, 1);
                Paragraph::new(format!(
                    "real-time latency track ({})",
                    if snapshot.sparkline.len() > 1 {
                        "live"
                    } else {
                        "seeded"
                    }
                ))
                .style(crate::tui_theme::text_hint(&tp))
                .render(label_area, frame);
                let chart_area = Rect::new(
                    spark_inner.x,
                    spark_inner.y + 1,
                    spark_inner.width,
                    spark_inner.height.saturating_sub(1),
                );
                Sparkline::new(&snapshot.sparkline)
                    .style(Style::default().fg(tp.chart_series[1]))
                    .render(chart_area, frame);
            } else {
                Sparkline::new(&snapshot.sparkline)
                    .style(Style::default().fg(tp.chart_series[1]))
                    .render(spark_inner, frame);
            }
        }

        let calls_block = Block::new()
            .title("Top Tool Volume")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let calls_inner = calls_block.inner(calls_area);
        calls_block.render(calls_area, frame);
        if calls_inner.height > 0 && calls_inner.width > 0 {
            if snapshot.top_call_tools.is_empty() {
                Paragraph::new("Collecting tool call samples…")
                    .style(crate::tui_theme::text_hint(&tp))
                    .render(calls_inner, frame);
            } else {
                let groups: Vec<BarGroup<'_>> = snapshot
                    .top_call_tools
                    .iter()
                    .map(|(name, value)| BarGroup::new(name, vec![(*value).max(0.0)]))
                    .collect();
                BarChart::new(groups)
                    .direction(BarDirection::Horizontal)
                    .colors(vec![tp.chart_series[0]])
                    .bar_width(1)
                    .bar_gap(0)
                    .group_gap(1)
                    .render(calls_inner, frame);
            }
        }
    }

    if bottom.height >= 3 {
        let gap = u16::from(bottom.width >= 120);
        let lat_w = (bottom.width.saturating_mul(56) / 100).max(26);
        let lat_w = lat_w.min(bottom.width.saturating_sub(18 + gap));
        let lat_area = Rect::new(bottom.x, bottom.y, lat_w, bottom.height);
        let summary_area = Rect::new(
            bottom.x.saturating_add(lat_w).saturating_add(gap),
            bottom.y,
            bottom.width.saturating_sub(lat_w.saturating_add(gap)),
            bottom.height,
        );

        let lat_block = Block::new()
            .title("Latency Hotspots (p95 ms)")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let lat_inner = lat_block.inner(lat_area);
        lat_block.render(lat_area, frame);
        if lat_inner.height > 0 && lat_inner.width > 0 {
            if snapshot.top_latency_tools.is_empty() {
                Paragraph::new("Collecting latency samples…")
                    .style(crate::tui_theme::text_hint(&tp))
                    .render(lat_inner, frame);
            } else {
                let groups: Vec<BarGroup<'_>> = snapshot
                    .top_latency_tools
                    .iter()
                    .map(|(name, value)| BarGroup::new(name, vec![*value]))
                    .collect();
                BarChart::new(groups)
                    .direction(BarDirection::Horizontal)
                    .colors(vec![tp.chart_series[2]])
                    .bar_width(1)
                    .bar_gap(0)
                    .group_gap(1)
                    .render(lat_inner, frame);
            }
        }

        let summary_block = Block::new()
            .title("Insights")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let summary_inner = summary_block.inner(summary_area);
        summary_block.render(summary_area, frame);
        if summary_inner.height > 0 && summary_inner.width > 0 {
            let mut lines = Vec::new();
            lines.push("feed: no anomaly cards; showing live telemetry instead".to_string());
            lines.push(format!(
                "filter={}  sort={}  focus={}",
                filter.label(),
                sort_mode.label(),
                focus.label()
            ));
            if let Some((name, value)) = snapshot.top_latency_tools.first() {
                lines.push(format!("hottest tool: {name} @ {value:.1}ms p95"));
            }
            if let Some((name, value)) = snapshot.top_call_tools.first() {
                lines.push(format!("busiest tool: {name} @ {:.0} calls", *value));
            }
            let content = lines.join("\n");
            Paragraph::new(content)
                .style(crate::tui_theme::text_hint(&tp))
                .render(summary_inner, frame);
        }
    }
}

// ── MailScreen implementation ──────────────────────────────────────────

impl MailScreen for AnalyticsScreen {
    fn update(&mut self, event: &Event, state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        let Event::Key(key) = event else {
            return Cmd::None;
        };
        if key.kind != KeyEventKind::Press {
            return Cmd::None;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == AnalyticsFocus::List {
                    self.move_down();
                } else {
                    self.scroll_detail_down();
                }
                Cmd::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == AnalyticsFocus::List {
                    self.move_up();
                } else {
                    self.scroll_detail_up();
                }
                Cmd::None
            }
            KeyCode::Char('J') | KeyCode::PageDown => {
                if self.focus == AnalyticsFocus::List {
                    for _ in 0..5 {
                        self.move_down();
                    }
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(5);
                }
                Cmd::None
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                if self.focus == AnalyticsFocus::List {
                    for _ in 0..5 {
                        self.move_up();
                    }
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(5);
                }
                Cmd::None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                Cmd::None
            }
            KeyCode::Enter => self.navigate_deep_link(),
            KeyCode::Char('r') => {
                self.refresh_feed(Some(state));
                Cmd::None
            }
            KeyCode::Char('s') => {
                self.cycle_severity_filter();
                Cmd::None
            }
            KeyCode::Char('o') => {
                self.cycle_sort_mode();
                Cmd::None
            }
            KeyCode::Home => {
                self.selected = 0;
                self.detail_scroll = 0;
                Cmd::None
            }
            KeyCode::End => {
                let active_count = self.active_card_count();
                if active_count > 0 {
                    self.selected = active_count - 1;
                    self.detail_scroll = 0;
                }
                Cmd::None
            }
            _ => Cmd::None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let tp = crate::tui_theme::TuiThemePalette::current();
        fill_rect(frame, area, tp.bg_deep);
        let runtime_snapshot = build_runtime_viz_snapshot(state);
        let runtime_has_signal = runtime_snapshot.total_calls > 0
            || runtime_snapshot.total_errors > 0
            || runtime_snapshot.persisted_samples > 0
            || state
                .sparkline_snapshot()
                .iter()
                .any(|sample| *sample > 0.0);

        let mut cards_area = area;
        let viz_band_h = if area.height >= ANALYTICS_VIZ_BAND_MIN_HEIGHT {
            ((u32::from(area.height) * u32::from(ANALYTICS_VIZ_BAND_HEIGHT_PERCENT)) / 100)
                .try_into()
                .unwrap_or(ANALYTICS_VIZ_BAND_MAX)
                .clamp(ANALYTICS_VIZ_BAND_MIN, ANALYTICS_VIZ_BAND_MAX)
        } else {
            0
        };
        if viz_band_h > 0 {
            let viz_area = Rect::new(area.x, area.y, area.width, viz_band_h.min(area.height));
            render_runtime_viz_fallback(
                frame,
                viz_area,
                state,
                self.focus,
                self.severity_filter,
                self.sort_mode,
            );
            cards_area = Rect::new(
                area.x,
                area.y.saturating_add(viz_area.height),
                area.width,
                area.height.saturating_sub(viz_area.height),
            );
        }

        let active_cards = self.active_cards();
        if self.feed.cards.is_empty() {
            if viz_band_h == 0 {
                render_runtime_viz_fallback(
                    frame,
                    area,
                    state,
                    self.focus,
                    self.severity_filter,
                    self.sort_mode,
                );
            }
            if cards_area.height > 0 {
                render_empty_state(frame, cards_area, &runtime_snapshot, runtime_has_signal);
            }
            return;
        }
        if active_cards.is_empty() {
            if viz_band_h == 0 {
                render_runtime_viz_fallback(
                    frame,
                    area,
                    state,
                    self.focus,
                    self.severity_filter,
                    self.sort_mode,
                );
            }
            if cards_area.height > 0 {
                render_filtered_empty_state(
                    frame,
                    cards_area,
                    self.severity_filter,
                    self.sort_mode,
                );
            }
            return;
        }

        let selected = self.selected.min(active_cards.len().saturating_sub(1));

        let summary_h = u16::from(cards_area.height >= ANALYTICS_SUMMARY_MIN_HEIGHT);
        let mut y = cards_area.y;
        if summary_h > 0 {
            let summary_area = Rect::new(cards_area.x, y, cards_area.width, summary_h);
            render_severity_summary(frame, summary_area, &self.feed);
            y += summary_h;
        }

        let content_full = Rect::new(
            cards_area.x,
            y,
            cards_area.width,
            cards_area.height.saturating_sub(summary_h),
        );
        if content_full.width == 0 || content_full.height == 0 {
            return;
        }

        let status_h = u16::from(content_full.height >= ANALYTICS_STATUS_STRIP_MIN_HEIGHT);
        let content = Rect::new(
            content_full.x,
            content_full.y,
            content_full.width,
            content_full.height.saturating_sub(status_h),
        );
        let status_area = Rect::new(
            content_full.x,
            content_full.y.saturating_add(content.height),
            content_full.width,
            status_h,
        );
        if content.width == 0 || content.height == 0 {
            if status_h > 0 {
                render_status_strip(
                    frame,
                    status_area,
                    self.focus,
                    self.severity_filter,
                    self.sort_mode,
                    active_cards.len(),
                    self.feed.cards.len(),
                    false,
                );
            }
            return;
        }

        let mut table_state = self.table_state.clone();
        let selected_card = active_cards[selected];

        let wide_split = content.width >= ANALYTICS_WIDE_SPLIT_MIN_WIDTH
            && content.height >= ANALYTICS_WIDE_SPLIT_MIN_HEIGHT;
        if wide_split {
            let gap = u16::from(content.width >= 96);
            let mut list_w = content
                .width
                .saturating_mul(ANALYTICS_WIDE_LIST_RATIO_PERCENT)
                / 100;
            list_w = list_w.max(ANALYTICS_WIDE_LIST_MIN_WIDTH);
            let max_list_w = content
                .width
                .saturating_sub(ANALYTICS_WIDE_DETAIL_MIN_WIDTH.saturating_add(gap));
            list_w = list_w.min(max_list_w);
            if list_w > 0 && list_w < content.width {
                let detail_w = content.width.saturating_sub(list_w.saturating_add(gap));
                if detail_w >= ANALYTICS_WIDE_DETAIL_MIN_WIDTH {
                    let list_area = Rect::new(content.x, content.y, list_w, content.height);
                    let detail_area = Rect::new(
                        content.x.saturating_add(list_w).saturating_add(gap),
                        content.y,
                        detail_w,
                        content.height,
                    );
                    if gap > 0 {
                        let splitter_area = Rect::new(
                            content.x.saturating_add(list_w),
                            content.y,
                            gap,
                            content.height,
                        );
                        render_splitter_handle(frame, splitter_area, true, false);
                    }
                    render_card_list(
                        frame,
                        list_area,
                        &active_cards,
                        selected,
                        &mut table_state,
                        self.severity_filter,
                        self.sort_mode,
                        self.feed.alerts_processed,
                        true,
                        self.focus,
                    );
                    if detail_area.height >= 18 && detail_area.width >= 52 {
                        let detail_gap = u16::from(detail_area.height >= 24);
                        let detail_main_h = detail_area
                            .height
                            .saturating_mul(62)
                            .saturating_div(100)
                            .clamp(
                                10,
                                detail_area
                                    .height
                                    .saturating_sub(7)
                                    .saturating_sub(detail_gap),
                            );
                        let detail_main = Rect::new(
                            detail_area.x,
                            detail_area.y,
                            detail_area.width,
                            detail_main_h,
                        );
                        if detail_gap > 0 {
                            let splitter_area = Rect::new(
                                detail_area.x,
                                detail_area.y.saturating_add(detail_main_h),
                                detail_area.width,
                                detail_gap,
                            );
                            render_splitter_handle(frame, splitter_area, false, false);
                        }
                        let detail_viz = Rect::new(
                            detail_area.x,
                            detail_area
                                .y
                                .saturating_add(detail_main_h)
                                .saturating_add(detail_gap),
                            detail_area.width,
                            detail_area
                                .height
                                .saturating_sub(detail_main_h)
                                .saturating_sub(detail_gap),
                        );
                        render_card_detail(
                            frame,
                            detail_main,
                            selected_card,
                            self.detail_scroll,
                            self.focus,
                        );
                        render_runtime_viz_fallback(
                            frame,
                            detail_viz,
                            state,
                            self.focus,
                            self.severity_filter,
                            self.sort_mode,
                        );
                    } else {
                        render_card_detail(
                            frame,
                            detail_area,
                            selected_card,
                            self.detail_scroll,
                            self.focus,
                        );
                    }
                    if status_h > 0 {
                        render_status_strip(
                            frame,
                            status_area,
                            self.focus,
                            self.severity_filter,
                            self.sort_mode,
                            active_cards.len(),
                            self.feed.cards.len(),
                            true,
                        );
                    }
                    return;
                }
            }
        }

        let stacked_detail = content.height >= ANALYTICS_STACKED_MIN_HEIGHT
            && content.height
                >= ANALYTICS_STACKED_LIST_MIN_HEIGHT
                    .saturating_add(ANALYTICS_STACKED_DETAIL_MIN_HEIGHT);
        if stacked_detail {
            let stack_gap = u16::from(
                content.height
                    >= ANALYTICS_STACKED_LIST_MIN_HEIGHT
                        .saturating_add(ANALYTICS_STACKED_DETAIL_MIN_HEIGHT)
                        .saturating_add(1),
            );
            let mut list_h = content.height.saturating_mul(38) / 100;
            list_h = list_h.max(ANALYTICS_STACKED_LIST_MIN_HEIGHT);
            let max_list_h = content
                .height
                .saturating_sub(ANALYTICS_STACKED_DETAIL_MIN_HEIGHT)
                .saturating_sub(stack_gap);
            list_h = list_h.min(max_list_h);

            let list_area = Rect::new(content.x, content.y, content.width, list_h);
            let detail_area = Rect::new(
                content.x,
                content.y.saturating_add(list_h).saturating_add(stack_gap),
                content.width,
                content
                    .height
                    .saturating_sub(list_h)
                    .saturating_sub(stack_gap),
            );
            if stack_gap > 0 {
                let splitter_area = Rect::new(
                    content.x,
                    content.y.saturating_add(list_h),
                    content.width,
                    stack_gap,
                );
                render_splitter_handle(frame, splitter_area, false, false);
            }
            render_card_list(
                frame,
                list_area,
                &active_cards,
                selected,
                &mut table_state,
                self.severity_filter,
                self.sort_mode,
                self.feed.alerts_processed,
                true,
                self.focus,
            );
            render_card_detail(
                frame,
                detail_area,
                selected_card,
                self.detail_scroll,
                self.focus,
            );
            if status_h > 0 {
                render_status_strip(
                    frame,
                    status_area,
                    self.focus,
                    self.severity_filter,
                    self.sort_mode,
                    active_cards.len(),
                    self.feed.cards.len(),
                    true,
                );
            }
            return;
        }

        let hint_h = u16::from(content.height >= 4);
        let list_h = content.height.saturating_sub(hint_h).max(1);
        let list_area = Rect::new(content.x, content.y, content.width, list_h);
        render_card_list(
            frame,
            list_area,
            &active_cards,
            selected,
            &mut table_state,
            self.severity_filter,
            self.sort_mode,
            self.feed.alerts_processed,
            false,
            self.focus,
        );
        if hint_h > 0 {
            let hint_area = Rect::new(
                content.x,
                content.y.saturating_add(list_h),
                content.width,
                hint_h,
            );
            render_compact_detail_hint(frame, hint_area, selected_card);
        }
        if status_h > 0 {
            render_status_strip(
                frame,
                status_area,
                self.focus,
                self.severity_filter,
                self.sort_mode,
                active_cards.len(),
                self.feed.cards.len(),
                false,
            );
        }
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        let should_refresh = self
            .last_refresh_tick
            .is_none_or(|last| tick_count.wrapping_sub(last) >= REFRESH_INTERVAL_TICKS);
        if should_refresh {
            self.refresh_feed(Some(state));
            self.last_refresh_tick = Some(tick_count);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Move focused panel",
            },
            HelpEntry {
                key: "J/K",
                action: "Fast scroll focused panel",
            },
            HelpEntry {
                key: "Tab/Shift+Tab",
                action: "Focus list/detail",
            },
            HelpEntry {
                key: "Enter",
                action: "Navigate to deep link",
            },
            HelpEntry {
                key: "r",
                action: "Refresh feed",
            },
            HelpEntry {
                key: "s",
                action: "Cycle severity filter",
            },
            HelpEntry {
                key: "o",
                action: "Cycle sort mode",
            },
            HelpEntry {
                key: "Home/End",
                action: "First/last card",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Message volume, response times, and agent activity analytics.")
    }

    fn copyable_content(&self) -> Option<String> {
        let card = self.selected_card()?;
        Some(format!("{}\n\n{}", card.headline, card.rationale))
    }

    fn title(&self) -> &'static str {
        "Analytics"
    }

    fn tab_label(&self) -> &'static str {
        "Insight"
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_text(frame: &Frame<'_>) -> String {
        let mut text = String::new();
        for y in 0..frame.buffer.height() {
            for x in 0..frame.buffer.width() {
                if let Some(cell) = frame.buffer.get(x, y) {
                    if let Some(ch) = cell.content.as_char() {
                        text.push(ch);
                    } else if !cell.is_continuation() {
                        text.push(' ');
                    }
                }
            }
            text.push('\n');
        }
        text
    }

    fn sample_card(id: &str, severity: AnomalySeverity, confidence: f64) -> InsightCard {
        InsightCard {
            id: id.to_string(),
            confidence,
            severity,
            headline: format!("{id} headline"),
            rationale: format!("{id} rationale"),
            likely_cause: Some(format!("{id} cause")),
            next_steps: vec![format!("{id} step")],
            deep_links: vec!["screen:dashboard".to_string()],
            primary_alert: AnomalyAlert {
                kind: AnomalyKind::LatencySpike,
                severity,
                score: confidence,
                current_value: 10.0,
                threshold: 1.0,
                baseline_value: Some(2.0),
                explanation: "sample".to_string(),
                suggested_action: "inspect".to_string(),
            },
            supporting_trends: Vec::new(),
            supporting_correlations: Vec::new(),
        }
    }

    #[test]
    fn analytics_screen_new_does_not_panic() {
        let _screen = AnalyticsScreen::new();
    }

    #[test]
    fn analytics_screen_empty_state_renders() {
        let screen = AnalyticsScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.view(&mut frame, Rect::new(0, 0, 80, 24), &state);
    }

    #[test]
    fn severity_badge_covers_all_variants() {
        assert_eq!(severity_badge(AnomalySeverity::Critical), "CRIT");
        assert_eq!(severity_badge(AnomalySeverity::High), "HIGH");
        assert_eq!(severity_badge(AnomalySeverity::Medium), " MED");
        assert_eq!(severity_badge(AnomalySeverity::Low), " LOW");
    }

    #[test]
    fn confidence_bar_renders_correctly() {
        let bar = confidence_bar(0.75);
        assert!(bar.contains("75%"));
        assert!(bar.starts_with('['));
        assert!(bar.contains(']'));
    }

    #[test]
    fn confidence_bar_edge_cases() {
        let zero = confidence_bar(0.0);
        assert!(zero.contains("0%"));
        let full = confidence_bar(1.0);
        assert!(full.contains("100%"));
    }

    #[test]
    fn parse_deep_link_screen_targets() {
        let msg = AnalyticsScreen::parse_deep_link("screen:tool_metrics");
        assert!(matches!(
            msg,
            Some(MailScreenMsg::Navigate(MailScreenId::ToolMetrics))
        ));

        let msg2 = AnalyticsScreen::parse_deep_link("screen:dashboard");
        assert!(matches!(
            msg2,
            Some(MailScreenMsg::Navigate(MailScreenId::Dashboard))
        ));
    }

    #[test]
    fn parse_deep_link_entity_targets() {
        let msg = AnalyticsScreen::parse_deep_link("thread:abc-123");
        assert!(
            matches!(msg, Some(MailScreenMsg::DeepLink(DeepLinkTarget::ThreadById(ref id))) if id == "abc-123")
        );

        let msg2 = AnalyticsScreen::parse_deep_link("tool:send_message");
        assert!(
            matches!(msg2, Some(MailScreenMsg::DeepLink(DeepLinkTarget::ToolByName(ref n))) if n == "send_message")
        );
    }

    #[test]
    fn parse_deep_link_unknown_returns_none() {
        assert!(AnalyticsScreen::parse_deep_link("unknown:foo").is_none());
        assert!(AnalyticsScreen::parse_deep_link("nocolon").is_none());
    }

    #[test]
    fn move_up_at_zero_stays() {
        let mut screen = AnalyticsScreen::new();
        screen.selected = 0;
        screen.move_up();
        assert_eq!(screen.selected, 0);
    }

    #[test]
    fn move_down_on_empty_stays() {
        let mut screen = AnalyticsScreen::new();
        // feed is empty in test context (no metrics flowing)
        screen.move_down();
        assert_eq!(screen.selected, 0);
    }

    #[test]
    fn first_tick_triggers_refresh_cycle() {
        let mut screen = AnalyticsScreen::new();
        assert_eq!(screen.last_refresh_tick, None);
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.tick(1, &state);
        assert_eq!(screen.last_refresh_tick, Some(1));
    }

    #[test]
    fn keybindings_returns_entries() {
        let screen = AnalyticsScreen::new();
        let bindings = screen.keybindings();
        assert!(!bindings.is_empty());
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "Tab/Shift+Tab"));
        assert!(bindings.iter().any(|b| b.key == "Enter"));
        assert!(bindings.iter().any(|b| b.key == "s"));
        assert!(bindings.iter().any(|b| b.key == "o"));
    }

    #[test]
    fn tab_cycles_focus_between_list_and_detail() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = AnalyticsScreen::new();
        assert_eq!(screen.focus, AnalyticsFocus::List);

        let tab = Event::Key(ftui::KeyEvent::new(KeyCode::Tab));
        screen.update(&tab, &state);
        assert_eq!(screen.focus, AnalyticsFocus::Detail);

        let back_tab = Event::Key(ftui::KeyEvent::new(KeyCode::BackTab));
        screen.update(&back_tab, &state);
        assert_eq!(screen.focus, AnalyticsFocus::List);
    }

    #[test]
    fn detail_focus_routes_jk_to_detail_scroll() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![sample_card("card", AnomalySeverity::High, 0.88)],
            alerts_processed: 1,
            cards_produced: 1,
        };
        screen.focus = AnalyticsFocus::Detail;

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('j'))), &state);
        assert_eq!(screen.selected, 0);
        assert_eq!(screen.detail_scroll, 1);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('k'))), &state);
        assert_eq!(screen.detail_scroll, 0);
    }

    #[test]
    fn persisted_rows_generate_insight_cards() {
        let rows = vec![crate::tool_metrics::PersistedToolMetric {
            tool_name: "send_message".to_string(),
            calls: 120,
            errors: 12,
            cluster: "messaging".to_string(),
            complexity: "medium".to_string(),
            avg_ms: 180.0,
            p50_ms: 95.0,
            p95_ms: 620.0,
            p99_ms: 950.0,
            is_slow: true,
            collected_ts: 1_700_000_000_000_000,
        }];

        let feed = build_persisted_insight_feed_from_rows(&rows, 50);
        assert!(!feed.cards.is_empty());
        assert!(feed.cards_produced > 0);
    }

    #[test]
    fn title_and_tab_label() {
        let screen = AnalyticsScreen::new();
        assert_eq!(screen.title(), "Analytics");
        assert_eq!(screen.tab_label(), "Insight");
    }

    #[test]
    fn severity_counts_empty() {
        let screen = AnalyticsScreen::new();
        let (c, h, m, l) = screen.severity_counts();
        // May have cards from quick_insight_feed
        assert!(c + h + m + l == screen.feed.cards.len() as u64);
    }

    #[test]
    fn confidence_bar_colored_renders() {
        let line = confidence_bar_colored(0.75, AnomalySeverity::High);
        // Should produce a line with spans, not panic
        assert!(!line.spans().is_empty());
    }

    #[test]
    fn severity_filter_clamps_selected_to_visible_range() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![
                sample_card("critical", AnomalySeverity::Critical, 0.9),
                sample_card("high", AnomalySeverity::High, 0.8),
                sample_card("low", AnomalySeverity::Low, 0.7),
            ],
            alerts_processed: 3,
            cards_produced: 3,
        };
        screen.selected = 2;
        screen.severity_filter = AnalyticsSeverityFilter::CriticalOnly;
        screen.clamp_selected_to_active_cards();
        assert_eq!(screen.active_card_count(), 1);
        assert_eq!(screen.selected, 0);
        assert!(
            screen
                .selected_card()
                .is_some_and(|card| card.severity == AnomalySeverity::Critical)
        );
    }

    #[test]
    fn severity_filter_falls_back_to_top_card_when_filtered_empty() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![
                sample_card("medium", AnomalySeverity::Medium, 0.7),
                sample_card("low", AnomalySeverity::Low, 0.9),
            ],
            alerts_processed: 2,
            cards_produced: 2,
        };
        screen.severity_filter = AnalyticsSeverityFilter::CriticalOnly;
        screen.sort_mode = AnalyticsSortMode::Confidence;
        screen.clamp_selected_to_active_cards();

        let active = screen.active_cards();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "low");
    }

    #[test]
    fn confidence_sort_orders_cards_descending() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![
                sample_card("a", AnomalySeverity::Medium, 0.3),
                sample_card("b", AnomalySeverity::High, 0.9),
                sample_card("c", AnomalySeverity::Low, 0.5),
            ],
            alerts_processed: 3,
            cards_produced: 3,
        };
        screen.sort_mode = AnalyticsSortMode::Confidence;
        let active = screen.active_cards();
        assert_eq!(active.len(), 3);
        assert_eq!(active[0].id, "b");
        assert_eq!(active[1].id, "c");
        assert_eq!(active[2].id, "a");
    }

    #[test]
    fn compact_layout_surfaces_detail_hint_when_space_is_short() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![sample_card("card", AnomalySeverity::High, 0.88)],
            alerts_processed: 1,
            cards_produced: 1,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 9, &mut pool);
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.view(&mut frame, Rect::new(0, 0, 80, 9), &state);
        let text = frame_text(&frame);
        assert!(text.contains("Detail hidden on short terminal"));
        assert!(text.contains("focus:list"));
    }

    #[test]
    fn wide_layout_renders_list_and_detail_panels() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![sample_card("card", AnomalySeverity::High, 0.88)],
            alerts_processed: 1,
            cards_produced: 1,
        };
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(140, 20, &mut pool);
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        screen.view(&mut frame, Rect::new(0, 0, 140, 20), &state);
        let text = frame_text(&frame);
        assert!(text.contains("card headline"));
        assert!(text.contains("card rationale"));
    }

    #[test]
    fn light_table_backgrounds_keep_header_and_rows_distinct() {
        let palette =
            crate::tui_theme::TuiThemePalette::for_theme(ftui_extras::theme::ThemeId::LumenLight);
        let (_base, header, even, odd) = analytics_table_backgrounds(&palette);
        assert_ne!(header, even);
        assert_ne!(odd, even);
    }

    // ── Severity filter cycle & labels ─────────────────────────────────

    #[test]
    fn severity_filter_next_cycles() {
        assert_eq!(
            AnalyticsSeverityFilter::All.next(),
            AnalyticsSeverityFilter::HighAndUp
        );
        assert_eq!(
            AnalyticsSeverityFilter::HighAndUp.next(),
            AnalyticsSeverityFilter::CriticalOnly
        );
        assert_eq!(
            AnalyticsSeverityFilter::CriticalOnly.next(),
            AnalyticsSeverityFilter::All
        );
    }

    #[test]
    fn severity_filter_labels() {
        assert_eq!(AnalyticsSeverityFilter::All.label(), "filter:all");
        assert_eq!(AnalyticsSeverityFilter::HighAndUp.label(), "filter:high+");
        assert_eq!(AnalyticsSeverityFilter::CriticalOnly.label(), "filter:crit");
    }

    #[test]
    fn severity_filter_includes_matrix() {
        // All includes everything
        assert!(AnalyticsSeverityFilter::All.includes(AnomalySeverity::Low));
        assert!(AnalyticsSeverityFilter::All.includes(AnomalySeverity::Critical));

        // HighAndUp excludes Low and Medium
        assert!(!AnalyticsSeverityFilter::HighAndUp.includes(AnomalySeverity::Low));
        assert!(!AnalyticsSeverityFilter::HighAndUp.includes(AnomalySeverity::Medium));
        assert!(AnalyticsSeverityFilter::HighAndUp.includes(AnomalySeverity::High));
        assert!(AnalyticsSeverityFilter::HighAndUp.includes(AnomalySeverity::Critical));

        // CriticalOnly excludes everything but Critical
        assert!(!AnalyticsSeverityFilter::CriticalOnly.includes(AnomalySeverity::Low));
        assert!(!AnalyticsSeverityFilter::CriticalOnly.includes(AnomalySeverity::Medium));
        assert!(!AnalyticsSeverityFilter::CriticalOnly.includes(AnomalySeverity::High));
        assert!(AnalyticsSeverityFilter::CriticalOnly.includes(AnomalySeverity::Critical));
    }

    // ── Sort mode cycle & labels ───────────────────────────────────────

    #[test]
    fn sort_mode_next_cycles() {
        assert_eq!(
            AnalyticsSortMode::Priority.next(),
            AnalyticsSortMode::Severity
        );
        assert_eq!(
            AnalyticsSortMode::Severity.next(),
            AnalyticsSortMode::Confidence
        );
        assert_eq!(
            AnalyticsSortMode::Confidence.next(),
            AnalyticsSortMode::Priority
        );
    }

    #[test]
    fn sort_mode_labels() {
        assert_eq!(AnalyticsSortMode::Priority.label(), "sort:priority");
        assert_eq!(AnalyticsSortMode::Severity.label(), "sort:severity");
        assert_eq!(AnalyticsSortMode::Confidence.label(), "sort:confidence");
    }

    // ── Focus toggle & labels ──────────────────────────────────────────

    #[test]
    fn focus_next_toggles() {
        assert_eq!(AnalyticsFocus::List.next(), AnalyticsFocus::Detail);
        assert_eq!(AnalyticsFocus::Detail.next(), AnalyticsFocus::List);
    }

    #[test]
    fn focus_labels() {
        assert_eq!(AnalyticsFocus::List.label(), "focus:list");
        assert_eq!(AnalyticsFocus::Detail.label(), "focus:detail");
    }

    // ── severity_rank ordering ─────────────────────────────────────────

    #[test]
    fn severity_rank_descending() {
        assert!(
            AnalyticsScreen::severity_rank(AnomalySeverity::Critical)
                > AnalyticsScreen::severity_rank(AnomalySeverity::High)
        );
        assert!(
            AnalyticsScreen::severity_rank(AnomalySeverity::High)
                > AnalyticsScreen::severity_rank(AnomalySeverity::Medium)
        );
        assert!(
            AnalyticsScreen::severity_rank(AnomalySeverity::Medium)
                > AnalyticsScreen::severity_rank(AnomalySeverity::Low)
        );
    }

    // ── format_error_rate_percent ──────────────────────────────────────

    #[test]
    fn format_error_rate_zero_calls() {
        assert_eq!(format_error_rate_percent(0, 0), "0.00");
    }

    #[test]
    fn format_error_rate_no_errors() {
        assert_eq!(format_error_rate_percent(0, 100), "0.00");
    }

    #[test]
    fn format_error_rate_all_errors() {
        assert_eq!(format_error_rate_percent(100, 100), "100.00");
    }

    #[test]
    fn format_error_rate_fractional() {
        // 1 error in 200 calls = 0.50%
        assert_eq!(format_error_rate_percent(1, 200), "0.50");
    }

    // ── perceived_luma ─────────────────────────────────────────────────

    #[test]
    fn perceived_luma_black_is_zero() {
        let black = PackedRgba::rgba(0, 0, 0, 255);
        assert_eq!(perceived_luma(black), 0);
    }

    #[test]
    fn perceived_luma_white_is_max() {
        let white = PackedRgba::rgba(255, 255, 255, 255);
        // (299*255 + 587*255 + 114*255)/1000 = 255
        assert_eq!(perceived_luma(white), 255);
    }

    #[test]
    fn perceived_luma_green_higher_than_red() {
        let pure_red = PackedRgba::rgba(255, 0, 0, 255);
        let pure_green = PackedRgba::rgba(0, 255, 0, 255);
        assert!(perceived_luma(pure_green) > perceived_luma(pure_red));
    }

    // ── scroll detail saturates ────────────────────────────────────────

    #[test]
    fn scroll_detail_up_saturates_at_zero() {
        let mut screen = AnalyticsScreen::new();
        screen.detail_scroll = 0;
        screen.scroll_detail_up();
        assert_eq!(screen.detail_scroll, 0);
    }

    #[test]
    fn scroll_detail_down_increments() {
        let mut screen = AnalyticsScreen::new();
        screen.detail_scroll = 0;
        screen.scroll_detail_down();
        assert_eq!(screen.detail_scroll, 1);
        screen.scroll_detail_down();
        assert_eq!(screen.detail_scroll, 2);
    }

    // ── copyable_content ───────────────────────────────────────────────

    #[test]
    fn copyable_content_with_card() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![sample_card("test", AnomalySeverity::High, 0.8)],
            alerts_processed: 1,
            cards_produced: 1,
        };
        let content = screen.copyable_content();
        assert!(content.is_some());
        let text = content.unwrap();
        assert!(text.contains("test headline"));
        assert!(text.contains("test rationale"));
    }

    #[test]
    fn context_help_tip_returns_some() {
        let screen = AnalyticsScreen::new();
        assert!(screen.context_help_tip().is_some());
    }

    // ── sort_card_indices with severity sort ───────────────────────────

    #[test]
    fn severity_sort_orders_critical_first() {
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![
                sample_card("low", AnomalySeverity::Low, 0.9),
                sample_card("crit", AnomalySeverity::Critical, 0.3),
                sample_card("med", AnomalySeverity::Medium, 0.5),
            ],
            alerts_processed: 3,
            cards_produced: 3,
        };
        screen.sort_mode = AnalyticsSortMode::Severity;
        let active = screen.active_cards();
        assert_eq!(active[0].id, "crit"); // rank 4
        assert_eq!(active[1].id, "med"); // rank 2
        assert_eq!(active[2].id, "low"); // rank 1
    }

    // ── cycle_severity_filter / cycle_sort_mode ────────────────────────

    #[test]
    fn cycle_severity_filter_wraps_around() {
        let mut screen = AnalyticsScreen::new();
        assert_eq!(screen.severity_filter, AnalyticsSeverityFilter::All);
        screen.cycle_severity_filter();
        assert_eq!(screen.severity_filter, AnalyticsSeverityFilter::HighAndUp);
        screen.cycle_severity_filter();
        assert_eq!(
            screen.severity_filter,
            AnalyticsSeverityFilter::CriticalOnly
        );
        screen.cycle_severity_filter();
        assert_eq!(screen.severity_filter, AnalyticsSeverityFilter::All);
    }

    #[test]
    fn cycle_sort_mode_wraps_around() {
        let mut screen = AnalyticsScreen::new();
        assert_eq!(screen.sort_mode, AnalyticsSortMode::Priority);
        screen.cycle_sort_mode();
        assert_eq!(screen.sort_mode, AnalyticsSortMode::Severity);
        screen.cycle_sort_mode();
        assert_eq!(screen.sort_mode, AnalyticsSortMode::Confidence);
        screen.cycle_sort_mode();
        assert_eq!(screen.sort_mode, AnalyticsSortMode::Priority);
    }

    // ── build_persisted_insight_feed_from_rows edge cases ──────────────

    #[test]
    fn persisted_feed_empty_rows() {
        let feed = build_persisted_insight_feed_from_rows(&[], 0);
        assert!(feed.cards.is_empty());
        assert_eq!(feed.alerts_processed, 0);
    }

    #[test]
    fn persisted_feed_low_error_tools_generate_baseline_cards() {
        // Tools with 0 errors and normal latency should still produce low-severity fallback
        let rows = vec![crate::tool_metrics::PersistedToolMetric {
            tool_name: "read_inbox".to_string(),
            calls: 500,
            errors: 0,
            cluster: "messaging".to_string(),
            complexity: "low".to_string(),
            avg_ms: 12.0,
            p50_ms: 8.0,
            p95_ms: 30.0,
            p99_ms: 45.0,
            is_slow: false,
            collected_ts: 1_700_000_000_000_000,
        }];
        let feed = build_persisted_insight_feed_from_rows(&rows, 10);
        assert!(!feed.cards.is_empty());
    }

    // ── parse_deep_link additional screen targets ──────────────────────

    #[test]
    fn parse_deep_link_all_screen_targets() {
        let targets = [
            ("screen:messages", MailScreenId::Messages),
            ("screen:threads", MailScreenId::Threads),
            ("screen:agents", MailScreenId::Agents),
            ("screen:search", MailScreenId::Search),
            ("screen:reservations", MailScreenId::Reservations),
            ("screen:system_health", MailScreenId::SystemHealth),
            ("screen:timeline", MailScreenId::Timeline),
            ("screen:projects", MailScreenId::Projects),
            ("screen:contacts", MailScreenId::Contacts),
            ("screen:explorer", MailScreenId::Explorer),
            ("screen:analytics", MailScreenId::Analytics),
        ];
        for (link, expected) in &targets {
            let msg = AnalyticsScreen::parse_deep_link(link);
            assert!(
                matches!(msg, Some(MailScreenMsg::Navigate(ref id)) if id == expected),
                "Failed for link: {link}"
            );
        }
    }

    #[test]
    fn parse_deep_link_agent_target() {
        let msg = AnalyticsScreen::parse_deep_link("agent:GoldHawk");
        assert!(
            matches!(msg, Some(MailScreenMsg::DeepLink(DeepLinkTarget::AgentByName(ref n))) if n == "GoldHawk")
        );
    }

    #[test]
    fn parse_deep_link_unknown_screen_returns_none() {
        assert!(AnalyticsScreen::parse_deep_link("screen:nonexistent").is_none());
    }

    // ── AnalyticsVizSnapshot default ───────────────────────────────────

    #[test]
    fn analytics_viz_snapshot_default() {
        let snap = AnalyticsVizSnapshot::default();
        assert_eq!(snap.total_calls, 0);
        assert_eq!(snap.total_errors, 0);
        assert_eq!(snap.active_tools, 0);
        assert_eq!(snap.avg_latency_ms, 0.0);
        assert!(snap.top_call_tools.is_empty());
        assert!(snap.sparkline.is_empty());
    }

    // ── Home / End keys ────────────────────────────────────────────────

    #[test]
    fn home_key_jumps_to_first_card() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![
                sample_card("a", AnomalySeverity::Low, 0.5),
                sample_card("b", AnomalySeverity::High, 0.8),
                sample_card("c", AnomalySeverity::Medium, 0.6),
            ],
            alerts_processed: 3,
            cards_produced: 3,
        };
        screen.selected = 2;
        screen.detail_scroll = 5;
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Home)), &state);
        assert_eq!(screen.selected, 0);
        assert_eq!(screen.detail_scroll, 0);
    }

    #[test]
    fn end_key_jumps_to_last_card() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![
                sample_card("a", AnomalySeverity::Low, 0.5),
                sample_card("b", AnomalySeverity::High, 0.8),
                sample_card("c", AnomalySeverity::Medium, 0.6),
            ],
            alerts_processed: 3,
            cards_produced: 3,
        };
        screen.selected = 0;
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::End)), &state);
        assert_eq!(screen.selected, 2);
    }

    // ── fast scroll (J/K) ──────────────────────────────────────────────

    #[test]
    fn fast_scroll_detail_moves_by_five() {
        let config = mcp_agent_mail_core::Config::default();
        let state = crate::tui_bridge::TuiSharedState::new(&config);
        let mut screen = AnalyticsScreen::new();
        screen.feed = InsightFeed {
            cards: vec![sample_card("card", AnomalySeverity::High, 0.8)],
            alerts_processed: 1,
            cards_produced: 1,
        };
        screen.focus = AnalyticsFocus::Detail;
        screen.detail_scroll = 0;

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('J'))), &state);
        assert_eq!(screen.detail_scroll, 5);

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('K'))), &state);
        assert_eq!(screen.detail_scroll, 0);
    }
}
