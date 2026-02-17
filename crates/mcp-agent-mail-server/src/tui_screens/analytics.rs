//! Analytics screen — insight feed with anomaly explanation cards.
//!
//! Renders [`InsightCard`] items from [`quick_insight_feed()`] with severity
//! badges, confidence scores, rationale, actionable next steps, severity
//! summary band, colored left borders, and deep link visual affordances.

use ftui::layout::{Constraint, Flex, Rect};
use ftui::widgets::StatefulWidget;
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
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

pub struct AnalyticsScreen {
    feed: InsightFeed,
    selected: usize,
    table_state: TableState,
    detail_scroll: u16,
    last_refresh_tick: Option<u64>,
}

impl AnalyticsScreen {
    #[must_use]
    pub fn new() -> Self {
        let feed = quick_insight_feed();
        Self {
            feed,
            selected: 0,
            table_state: TableState::default(),
            detail_scroll: 0,
            last_refresh_tick: None,
        }
    }

    fn refresh_feed(&mut self, state: Option<&TuiSharedState>) {
        self.feed = quick_insight_feed();
        if self.feed.cards.is_empty() {
            if let Some(state) = state {
                let persisted = build_persisted_insight_feed(state);
                if !persisted.cards.is_empty() {
                    self.feed = persisted;
                }
            }
        }
        if self.selected >= self.feed.cards.len() && !self.feed.cards.is_empty() {
            self.selected = self.feed.cards.len() - 1;
        }
    }

    fn selected_card(&self) -> Option<&InsightCard> {
        self.feed.cards.get(self.selected)
    }

    #[allow(clippy::missing_const_for_fn)] // stateful runtime helper
    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.detail_scroll = 0;
        }
    }

    fn move_down(&mut self) {
        if !self.feed.cards.is_empty() && self.selected < self.feed.cards.len() - 1 {
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
        Span::styled(format!("] {:3.0}%", confidence * 100.0), Style::default()),
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

fn render_card_list(
    frame: &mut Frame<'_>,
    area: Rect,
    feed: &InsightFeed,
    selected: usize,
    table_state: &mut TableState,
) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let header =
        Row::new(vec![" ", "Sev", "Conf", "Headline"]).style(crate::tui_theme::text_title(&tp));

    let rows: Vec<Row> = feed
        .cards
        .iter()
        .enumerate()
        .map(|(i, card)| {
            let sev_text = severity_badge(card.severity);
            let conf_text = format!("{:3.0}%", card.confidence * 100.0);
            let border_char = "\u{2590}"; // ▐ colored left border
            let style = if i == selected {
                severity_style(card.severity).bg(tp.selection_bg)
            } else {
                severity_style(card.severity)
            };
            Row::new(vec![
                border_char.to_string(),
                sev_text.to_string(),
                conf_text,
                card.headline.clone(),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Fixed(1),
        Constraint::Fixed(5),
        Constraint::Fixed(5),
        Constraint::Percentage(100.0),
    ];

    // Position indicator in title: [3/12]
    let position = if feed.cards.is_empty() {
        String::new()
    } else {
        format!(" [{}/{}]", selected + 1, feed.cards.len())
    };
    let title = format!(
        " Insight Feed{} (from {} alerts) ",
        position, feed.alerts_processed
    );

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::new()
                .title(title.as_str())
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(tp.panel_border)),
        )
        .highlight_style(Style::default().fg(tp.selection_fg).bg(tp.selection_bg));

    table_state.select(Some(selected));
    StatefulWidget::render(&table, area, frame, table_state);
}

fn render_card_detail(frame: &mut Frame<'_>, area: Rect, card: &InsightCard, scroll: u16) {
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
    lines.push(Line::raw(""));

    // Headline
    lines.push(Line::from_spans(vec![
        Span::styled("Headline: ", Style::default().bold()),
        Span::raw(&card.headline),
    ]));
    lines.push(Line::raw(""));

    // Rationale
    lines.push(Line::styled("Rationale:", Style::default().bold()));
    for line in card.rationale.lines() {
        lines.push(Line::raw(format!("  {line}")));
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
            lines.push(Line::raw(format!("  {}. {step}", i + 1)));
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
            lines.push(Line::raw(format!(
                "  {} {} ({:+.1}%)",
                trend.metric,
                trend.direction,
                trend.delta_ratio * 100.0,
            )));
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
            lines.push(Line::raw(format!(
                "  {} \u{2194} {} ({})",
                corr.metric_a, corr.metric_b, corr.explanation,
            )));
        }
    }

    let text = Text::from_lines(lines);
    let para = Paragraph::new(text).scroll((scroll, 0)).block(
        Block::new()
            .title(" Card Detail ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border)),
    );
    para.render(area, frame);
}

fn render_empty_state(frame: &mut Frame<'_>, area: Rect) {
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
    lines.push(Line::from_spans(vec![Span::styled(
        "No anomalies detected",
        crate::tui_theme::text_primary(&tp).bold(),
    )]));
    lines.push(Line::raw(""));

    // Description
    lines.push(Line::styled(
        "The insight feed monitors real-time KPI metrics",
        crate::tui_theme::text_meta(&tp),
    ));
    lines.push(Line::styled(
        "and surfaces anomaly cards when deviations occur.",
        crate::tui_theme::text_meta(&tp),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Metrics are collected as tool calls flow through the server.",
        crate::tui_theme::text_hint(&tp),
    ));

    let text = Text::from_lines(lines);
    Paragraph::new(text).render(inner, frame);
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
                self.move_down();
                Cmd::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_up();
                Cmd::None
            }
            KeyCode::Char('J') | KeyCode::PageDown => {
                self.scroll_detail_down();
                Cmd::None
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.scroll_detail_up();
                Cmd::None
            }
            KeyCode::Enter => self.navigate_deep_link(),
            KeyCode::Char('r') => {
                self.refresh_feed(Some(state));
                Cmd::None
            }
            KeyCode::Home => {
                self.selected = 0;
                self.detail_scroll = 0;
                Cmd::None
            }
            KeyCode::End => {
                if !self.feed.cards.is_empty() {
                    self.selected = self.feed.cards.len() - 1;
                    self.detail_scroll = 0;
                }
                Cmd::None
            }
            _ => Cmd::None,
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, _state: &TuiSharedState) {
        if self.feed.cards.is_empty() {
            render_empty_state(frame, area);
            return;
        }
        let selected = self.selected.min(self.feed.cards.len().saturating_sub(1));

        // Layout: severity_summary(1) + card_list(40%) + detail(rest)
        let summary_h: u16 = if area.height >= 8 { 1 } else { 0 };
        let remaining_h = area.height.saturating_sub(summary_h);

        let mut y = area.y;

        // ── Severity summary band ──────────────────────────────────────
        if summary_h > 0 {
            let summary_area = Rect::new(area.x, y, area.width, summary_h);
            render_severity_summary(frame, summary_area, &self.feed);
            y += summary_h;
        }

        // Split remaining: top 40% for card list, bottom 60% for detail.
        let list_area_h = remaining_h * 40 / 100;
        let detail_area_h = remaining_h.saturating_sub(list_area_h);

        let list_area = Rect::new(area.x, y, area.width, list_area_h);
        let detail_area = Rect::new(area.x, y + list_area_h, area.width, detail_area_h);

        let mut table_state = self.table_state.clone();
        render_card_list(frame, list_area, &self.feed, selected, &mut table_state);

        if let Some(card) = self.feed.cards.get(selected) {
            render_card_detail(frame, detail_area, card, self.detail_scroll);
        }
    }

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        let should_refresh = self.last_refresh_tick.map_or(true, |last| {
            tick_count.wrapping_sub(last) >= REFRESH_INTERVAL_TICKS
        });
        if should_refresh {
            self.refresh_feed(Some(state));
            self.last_refresh_tick = Some(tick_count);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Navigate cards",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail",
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
        assert!(bindings.iter().any(|b| b.key == "Enter"));
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
}
