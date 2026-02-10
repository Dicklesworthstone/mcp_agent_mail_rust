//! Analytics screen — insight feed with anomaly explanation cards.
//!
//! Renders [`InsightCard`] items from [`quick_insight_feed()`] with severity
//! badges, confidence scores, rationale, and actionable next steps.

use ftui::layout::{Constraint, Flex, Rect};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table, TableState};
use ftui::widgets::StatefulWidget;
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_runtime::program::Cmd;
use mcp_agent_mail_core::{AnomalySeverity, InsightCard, InsightFeed, quick_insight_feed};

use crate::tui_bridge::TuiSharedState;
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenId, MailScreenMsg};

/// Refresh the insight feed every N ticks (~100ms each → ~5s).
const REFRESH_INTERVAL_TICKS: u64 = 50;

pub struct AnalyticsScreen {
    feed: InsightFeed,
    selected: usize,
    table_state: TableState,
    detail_scroll: u16,
    last_refresh_tick: u64,
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
            last_refresh_tick: 0,
        }
    }

    fn refresh_feed(&mut self) {
        self.feed = quick_insight_feed();
        if self.selected >= self.feed.cards.len() && !self.feed.cards.is_empty() {
            self.selected = self.feed.cards.len() - 1;
        }
    }

    fn selected_card(&self) -> Option<&InsightCard> {
        self.feed.cards.get(self.selected)
    }

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

    fn scroll_detail_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

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
}

// ── Rendering helpers ──────────────────────────────────────────────────

fn severity_style(severity: AnomalySeverity) -> Style {
    match severity {
        AnomalySeverity::Critical => Style::default().fg(PackedRgba::rgb(255, 60, 60)).bold(),
        AnomalySeverity::High => Style::default().fg(PackedRgba::rgb(255, 165, 0)).bold(),
        AnomalySeverity::Medium => Style::default().fg(PackedRgba::rgb(255, 255, 0)),
        AnomalySeverity::Low => Style::default().fg(PackedRgba::rgb(100, 200, 100)),
    }
}

fn severity_badge(severity: AnomalySeverity) -> &'static str {
    match severity {
        AnomalySeverity::Critical => "CRIT",
        AnomalySeverity::High => "HIGH",
        AnomalySeverity::Medium => " MED",
        AnomalySeverity::Low => " LOW",
    }
}

fn confidence_bar(confidence: f64) -> String {
    let filled = (confidence * 10.0).round() as usize;
    let empty = 10_usize.saturating_sub(filled);
    format!("[{}{}] {:3.0}%", "█".repeat(filled), "░".repeat(empty), confidence * 100.0)
}

fn render_card_list(
    frame: &mut Frame<'_>,
    area: Rect,
    feed: &InsightFeed,
    selected: usize,
    table_state: &mut TableState,
) {
    let header = Row::new(vec!["Sev", "Conf", "Headline"]).style(
        Style::default()
            .fg(PackedRgba::rgb(180, 180, 220))
            .bold(),
    );

    let rows: Vec<Row> = feed
        .cards
        .iter()
        .enumerate()
        .map(|(i, card)| {
            let sev_text = severity_badge(card.severity);
            let conf_text = format!("{:3.0}%", card.confidence * 100.0);
            let style = if i == selected {
                severity_style(card.severity).reverse()
            } else {
                severity_style(card.severity)
            };
            Row::new(vec![sev_text.to_string(), conf_text, card.headline.clone()]).style(style)
        })
        .collect();

    let widths = [
        Constraint::Fixed(5),
        Constraint::Fixed(5),
        Constraint::Percentage(100.0),
    ];

    let title = format!(
        " Insight Feed ({} cards from {} alerts) ",
        feed.cards_produced, feed.alerts_processed
    );

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::new()
                .title(title.as_str())
                .border_type(BorderType::Rounded),
        )
        .highlight_style(Style::default().reverse());

    table_state.select(Some(selected));
    StatefulWidget::render(&table, area, frame, table_state);
}

fn render_card_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    card: &InsightCard,
    scroll: u16,
) {
    use ftui::text::{Line, Span, Text};

    let mut lines = Vec::new();

    // Header: severity + confidence
    lines.push(Line::from_spans(vec![
        Span::styled(
            format!(" {} ", severity_badge(card.severity)),
            severity_style(card.severity),
        ),
        Span::raw("  "),
        Span::styled(
            confidence_bar(card.confidence),
            Style::default().fg(PackedRgba::rgb(120, 180, 255)),
        ),
    ]));
    lines.push(Line::raw(""));

    // Headline
    lines.push(Line::from_spans(vec![
        Span::styled("Headline: ", Style::default().bold()),
        Span::raw(&card.headline),
    ]));
    lines.push(Line::raw(""));

    // Rationale
    lines.push(Line::styled(
        "Rationale:",
        Style::default().bold(),
    ));
    for line in card.rationale.lines() {
        lines.push(Line::raw(format!("  {line}")));
    }
    lines.push(Line::raw(""));

    // Likely cause
    if let Some(ref cause) = card.likely_cause {
        lines.push(Line::from_spans(vec![
            Span::styled("Likely Cause: ", Style::default().fg(PackedRgba::rgb(255, 200, 100)).bold()),
            Span::raw(cause),
        ]));
        lines.push(Line::raw(""));
    }

    // Next steps
    if !card.next_steps.is_empty() {
        lines.push(Line::styled(
            "Next Steps:",
            Style::default().fg(PackedRgba::rgb(100, 220, 100)).bold(),
        ));
        for (i, step) in card.next_steps.iter().enumerate() {
            lines.push(Line::raw(format!("  {}. {step}", i + 1)));
        }
        lines.push(Line::raw(""));
    }

    // Deep links
    if !card.deep_links.is_empty() {
        lines.push(Line::styled(
            "Deep Links (Enter to navigate):",
            Style::default().fg(PackedRgba::rgb(120, 120, 220)),
        ));
        for link in &card.deep_links {
            lines.push(Line::raw(format!("  → {link}")));
        }
        lines.push(Line::raw(""));
    }

    // Supporting evidence summary
    if !card.supporting_trends.is_empty() {
        lines.push(Line::styled(
            format!("Supporting Trends ({})", card.supporting_trends.len()),
            Style::default().fg(PackedRgba::rgb(160, 160, 200)),
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
            format!("Supporting Correlations ({})", card.supporting_correlations.len()),
            Style::default().fg(PackedRgba::rgb(160, 160, 200)),
        ));
        for corr in &card.supporting_correlations {
            lines.push(Line::raw(format!(
                "  {} ↔ {} ({})",
                corr.metric_a, corr.metric_b, corr.explanation,
            )));
        }
    }

    let text = Text::from_lines(lines);
    let para = Paragraph::new(text)
        .scroll((scroll, 0))
        .block(
            Block::new()
                .title(" Card Detail ")
                .border_type(BorderType::Rounded),
        );
    para.render(area, frame);
}

fn render_empty_state(frame: &mut Frame<'_>, area: Rect) {
    let text = "No anomalies detected.\n\n\
                The insight feed monitors real-time KPI metrics and surfaces\n\
                anomaly explanation cards when deviations are detected.\n\n\
                Metrics are collected as tool calls flow through the server.";
    let para = Paragraph::new(text).block(
        Block::new()
            .title(" Insight Feed ")
            .border_type(BorderType::Rounded),
    );
    para.render(area, frame);
}

// ── MailScreen implementation ──────────────────────────────────────────

impl MailScreen for AnalyticsScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
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
                self.refresh_feed();
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

        // Split: top half for card list, bottom half for detail.
        let chunks = Flex::vertical()
            .constraints([
                Constraint::Percentage(40.0),
                Constraint::Percentage(60.0),
            ])
            .split(area);

        let mut table_state = self.table_state.clone();
        render_card_list(frame, chunks[0], &self.feed, self.selected, &mut table_state);

        if let Some(card) = self.selected_card() {
            render_card_detail(frame, chunks[1], card, self.detail_scroll);
        }
    }

    fn tick(&mut self, tick_count: u64, _state: &TuiSharedState) {
        if tick_count.wrapping_sub(self.last_refresh_tick) >= REFRESH_INTERVAL_TICKS {
            self.refresh_feed();
            self.last_refresh_tick = tick_count;
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
        assert!(matches!(msg, Some(MailScreenMsg::DeepLink(DeepLinkTarget::ThreadById(ref id))) if id == "abc-123"));

        let msg2 = AnalyticsScreen::parse_deep_link("tool:send_message");
        assert!(matches!(msg2, Some(MailScreenMsg::DeepLink(DeepLinkTarget::ToolByName(ref n))) if n == "send_message"));
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
    fn keybindings_returns_entries() {
        let screen = AnalyticsScreen::new();
        let bindings = screen.keybindings();
        assert!(!bindings.is_empty());
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "Enter"));
    }

    #[test]
    fn title_and_tab_label() {
        let screen = AnalyticsScreen::new();
        assert_eq!(screen.title(), "Analytics");
        assert_eq!(screen.tab_label(), "Insight");
    }
}
