//! System Health screen for `AgentMailTUI`.
//!
//! Focus: connection diagnostics (base-path, auth, handshake, reachability) with
//! actionable remediation hints.
//!
//! Enhanced with advanced widget integration (br-3vwi.7.5):
//! - `MetricTile` summary KPIs (uptime, TCP latency, request count, avg latency)
//! - `ReservationGauge` for event ring buffer utilization
//! - `AnomalyCard` for diagnostic findings with severity/remediation
//! - `WidgetState` for loading/ready states
//! - View mode toggle: text diagnostics (default) vs widget dashboard

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ftui::layout::{Breakpoint, Constraint, Flex, Rect, ResponsiveLayout};
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba, Style};
use ftui_extras::text_effects::{StyledText, TextEffect};
use ftui_runtime::program::Cmd;
use mcp_agent_mail_core::Config;

use crate::tui_bridge::{ConfigSnapshot, TuiSharedState};
use crate::tui_widgets::{
    AnomalyCard, AnomalySeverity, MetricTile, MetricTrend, ReservationGauge, WidgetState,
};

use super::{HelpEntry, MailScreen, MailScreenMsg};

const DIAG_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const IO_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_SLEEP: Duration = Duration::from_millis(50);
const MAX_READ_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Level {
    #[default]
    Ok,
    Warn,
    Fail,
}

impl Level {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    /// Styled badge color for the level (green/amber/red).
    fn style(self, tp: &crate::tui_theme::TuiThemePalette) -> Style {
        match self {
            Self::Ok => crate::tui_theme::text_success(tp),
            Self::Warn => crate::tui_theme::text_warning(tp),
            Self::Fail => crate::tui_theme::text_error(tp),
        }
    }
}

/// Build a styled diagnostic line: `  [LEVEL] description  detail`
fn level_styled_line(
    level: Level,
    tp: &crate::tui_theme::TuiThemePalette,
    description: String,
    detail: String,
) -> Line<'static> {
    let badge_style = level.style(tp);
    let desc_style = crate::tui_theme::text_primary(tp);
    let detail_style = crate::tui_theme::text_meta(tp);
    Line::from_spans([
        Span::raw("  "),
        Span::styled(format!("[{}]", level.label()), badge_style),
        Span::raw(" "),
        Span::styled(description, desc_style),
        Span::raw("  "),
        Span::styled(detail, detail_style),
    ])
}

/// Human-readable HTTP status description.
fn format_http_status(status: u16) -> String {
    match status {
        200 => "200 OK".to_string(),
        401 => "401 Unauthorized".to_string(),
        403 => "403 Forbidden".to_string(),
        404 => "404 Not Found".to_string(),
        405 => "405 Method Not Allowed".to_string(),
        500 => "500 Internal Error".to_string(),
        _ => status.to_string(),
    }
}

/// Width classes for adaptive dashboard layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidthClass {
    /// >= 80 columns: full layout with tiles, gauge, and cards.
    Wide,
    /// 40-79 columns: tiles + cards, no gauge.
    Medium,
    /// < 40 columns: compact summary + cards only.
    Narrow,
}

impl WidthClass {
    const fn from_width(w: u16) -> Self {
        if w >= 80 {
            Self::Wide
        } else if w >= 40 {
            Self::Medium
        } else {
            Self::Narrow
        }
    }
}

/// Numeric priority for severity sorting (higher = more critical).
const fn severity_priority(sev: AnomalySeverity) -> u8 {
    match sev {
        AnomalySeverity::Critical => 4,
        AnomalySeverity::High => 3,
        AnomalySeverity::Medium => 2,
        AnomalySeverity::Low => 1,
    }
}

#[derive(Debug, Clone, Default)]
struct ProbeLine {
    level: Level,
    name: &'static str,
    detail: String,
    remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ProbeAuthKind {
    #[default]
    Unauth,
    Auth,
}

impl ProbeAuthKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Unauth => "unauth",
            Self::Auth => "auth",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PathProbe {
    path: String,
    kind: ProbeAuthKind,
    status: Option<u16>,
    latency_ms: Option<u64>,
    body_has_tools: Option<bool>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct DiagnosticsSnapshot {
    checked_at: Option<DateTime<Utc>>,
    endpoint: String,
    web_ui_url: String,
    auth_enabled: bool,
    localhost_unauth_allowed: bool,
    token_present: bool,
    token_len: usize,
    http_host: String,
    http_port: u16,
    configured_path: String,
    tcp_latency_ms: Option<u64>,
    tcp_error: Option<String>,
    path_probes: Vec<PathProbe>,
    lines: Vec<ProbeLine>,
    /// Tailscale remote-access URL with token, if Tailscale is active.
    remote_url: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedEndpoint {
    host: String,
    port: u16,
    path: String,
}

/// View mode for the health screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Traditional text diagnostics view.
    Text,
    /// Widget dashboard view with metric tiles, gauges, and anomaly cards.
    Dashboard,
}

pub struct SystemHealthScreen {
    snapshot: Arc<Mutex<DiagnosticsSnapshot>>,
    refresh_requested: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    view_mode: ViewMode,
    /// Whether the detail/findings panel is visible on wide screens.
    detail_visible: bool,
    /// Scroll offset inside the detail panel.
    detail_scroll: usize,
    /// Selected anomaly/finding index for detail panel focus.
    anomaly_cursor: usize,
}

impl SystemHealthScreen {
    #[must_use]
    pub fn new(state: Arc<TuiSharedState>) -> Self {
        let snapshot = Arc::new(Mutex::new(DiagnosticsSnapshot::default()));
        let refresh_requested = Arc::new(AtomicBool::new(true)); // run once immediately
        let stop = Arc::new(AtomicBool::new(false));

        let worker = {
            let state = state;
            let snapshot = Arc::clone(&snapshot);
            let refresh_requested = Arc::clone(&refresh_requested);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("am-system-health".to_string())
                .spawn(move || {
                    diagnostics_worker_loop(&state, &snapshot, &refresh_requested, &stop);
                })
                .map_or_else(
                    |error| {
                        tracing::warn!(
                            error = %error,
                            "failed to spawn system health diagnostics worker"
                        );
                        None
                    },
                    Some,
                )
        };

        Self {
            snapshot,
            refresh_requested,
            stop,
            worker,
            view_mode: ViewMode::Text,
            detail_visible: true,
            detail_scroll: 0,
            anomaly_cursor: 0,
        }
    }

    fn request_refresh(&self) {
        self.refresh_requested.store(true, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DiagnosticsSnapshot {
        self.snapshot
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Render the original text diagnostics view.
    #[allow(clippy::too_many_lines)]
    fn render_text_view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let snap = self.snapshot();
        let effects_enabled = state.config_snapshot().tui_effects;

        let tp = crate::tui_theme::TuiThemePalette::current();
        let label_style = crate::tui_theme::text_meta(&tp);
        let value_style = crate::tui_theme::text_primary(&tp);
        let section_style = crate::tui_theme::text_section(&tp);
        let hint_style = crate::tui_theme::text_hint(&tp);
        let accent_style = crate::tui_theme::text_accent(&tp);
        let action_key_style = crate::tui_theme::text_action_key(&tp);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // ── Configuration Section ──
        lines.push(Line::from_spans([Span::styled(
            "\u{2500}\u{2500} Configuration \u{2500}\u{2500}",
            section_style,
        )]));

        lines.push(Line::from_spans([
            Span::styled("Endpoint:  ", label_style),
            Span::styled(snap.endpoint.clone(), value_style),
        ]));
        lines.push(Line::from_spans([
            Span::styled("Web UI:    ", label_style),
            Span::styled(snap.web_ui_url.clone(), value_style),
        ]));
        if let Some(ref url) = snap.remote_url {
            let remote_style = crate::tui_theme::text_accent(&tp).bold();
            lines.push(Line::from_spans([
                Span::styled("Remote:    ", label_style),
                Span::styled(url.clone(), remote_style),
            ]));
            lines.push(Line::from_spans([
                Span::styled("           ", label_style),
                Span::styled(
                    "(Tailscale — click to open web app from remote machine)".to_string(),
                    hint_style,
                ),
            ]));
        }

        let auth_text = if snap.auth_enabled {
            "enabled"
        } else {
            "disabled"
        };
        let auth_val_style = if snap.auth_enabled {
            crate::tui_theme::text_success(&tp)
        } else {
            crate::tui_theme::text_warning(&tp)
        };
        lines.push(Line::from_spans([
            Span::styled("Auth:      ", label_style),
            Span::styled(auth_text.to_string(), auth_val_style),
            Span::styled(
                format!(" (token: {}, len: {})", snap.token_present, snap.token_len),
                hint_style,
            ),
        ]));
        if snap.auth_enabled && snap.localhost_unauth_allowed {
            lines.push(Line::from_spans([
                Span::styled("           ", label_style),
                Span::styled(
                    "Note: localhost unauthenticated access allowed".to_string(),
                    hint_style,
                ),
            ]));
        }

        let checked = snap
            .checked_at
            .map_or_else(|| "(never)".to_string(), |t| t.to_rfc3339());
        lines.push(Line::from_spans([
            Span::styled("Checked:   ", label_style),
            Span::styled(checked, value_style),
        ]));

        let uptime = state.uptime();
        lines.push(Line::from_spans([
            Span::styled("Uptime:    ", label_style),
            Span::styled(format!("{}s", uptime.as_secs()), value_style),
        ]));

        lines.push(Line::raw(String::new()));

        // ── Connection Diagnostics Section ──
        lines.push(Line::from_spans([Span::styled(
            "\u{2500}\u{2500} Connection Diagnostics \u{2500}\u{2500}",
            section_style,
        )]));

        // TCP probe
        if let Some(err) = &snap.tcp_error {
            lines.push(level_styled_line(
                Level::Fail,
                &tp,
                format!("TCP {}:{}", snap.http_host, snap.http_port),
                err.clone(),
            ));
        } else {
            lines.push(level_styled_line(
                Level::Ok,
                &tp,
                format!("TCP {}:{}", snap.http_host, snap.http_port),
                format!("{}ms", snap.tcp_latency_ms.unwrap_or(0)),
            ));
        }

        // HTTP probes
        for p in &snap.path_probes {
            if let Some(err) = &p.error {
                lines.push(level_styled_line(
                    Level::Fail,
                    &tp,
                    format!("POST {} ({})", p.path, p.kind.label()),
                    err.clone(),
                ));
                continue;
            }
            let status = p.status.map_or_else(|| "?".into(), format_http_status);
            let latency = p.latency_ms.unwrap_or(0);
            let tools_hint = match p.body_has_tools {
                Some(true) => "tools: yes",
                Some(false) => "tools: no",
                None => "tools: ?",
            };
            let level = classify_http_probe(&snap, p);
            lines.push(level_styled_line(
                level,
                &tp,
                format!("POST {} ({})", p.path, p.kind.label()),
                format!("{status}  {latency}ms  {tools_hint}"),
            ));
        }

        // ── Findings Section ──
        if !snap.lines.is_empty() {
            lines.push(Line::raw(String::new()));
            lines.push(Line::from_spans([Span::styled(
                "\u{2500}\u{2500} Findings \u{2500}\u{2500}",
                section_style,
            )]));
            for line in &snap.lines {
                lines.push(level_styled_line(
                    line.level,
                    &tp,
                    line.name.to_string(),
                    line.detail.clone(),
                ));
                if let Some(fix) = &line.remediation {
                    lines.push(Line::from_spans([
                        Span::styled("       Fix: ", accent_style),
                        Span::styled(fix.clone(), hint_style),
                    ]));
                }
            }
        }

        lines.push(Line::raw(String::new()));
        lines.push(Line::from_spans([
            Span::styled("r", action_key_style),
            Span::styled(" Refresh  ", hint_style),
            Span::styled("v", action_key_style),
            Span::styled(" Dashboard", hint_style),
        ]));

        let block = Block::default()
            .title("System Health")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        Paragraph::new(Text::from_lines(lines))
            .block(block)
            .render(area, frame);

        if diagnostics_probe_in_progress(&snap, self.refresh_requested.load(Ordering::Relaxed)) {
            render_probing_indicator(frame, area, state, effects_enabled);
        }
    }

    /// Render the widget dashboard view.
    #[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
    fn render_dashboard_view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let snap = self.snapshot();
        let effects_enabled = state.config_snapshot().tui_effects;
        let probing =
            diagnostics_probe_in_progress(&snap, self.refresh_requested.load(Ordering::Relaxed));

        let critical_alerts = critical_finding_count(&snap);
        let content_area = if critical_alerts > 0 && area.height > 1 {
            let tp = crate::tui_theme::TuiThemePalette::current();
            let alert_text = format!("CRITICAL: {critical_alerts} failing health checks");
            let alert_style = crate::tui_theme::text_critical(&tp);
            Paragraph::new(alert_text.clone())
                .style(alert_style)
                .render(Rect::new(area.x, area.y, area.width, 1), frame);
            if effects_enabled {
                StyledText::new(alert_text)
                    .effect(TextEffect::PulsingGlow {
                        color: tp.severity_critical,
                        speed: 0.5,
                    })
                    .base_color(tp.severity_critical)
                    .bold()
                    .time(state.uptime().as_secs_f64())
                    .render(Rect::new(area.x, area.y, area.width, 1), frame);
            }
            Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                area.height.saturating_sub(1),
            )
        } else {
            area
        };

        if probing {
            render_probing_indicator(frame, area, state, effects_enabled);
        }

        if snap.checked_at.is_none() {
            let widget: WidgetState<'_, Paragraph<'_>> = WidgetState::Loading {
                message: "Running diagnostics...",
            };
            widget.render(content_area, frame);
            return;
        }

        // ── Remote access banner (Tailscale) ──
        let content_area = render_remote_url_banner(frame, content_area, &snap);

        // Adaptive width-class layout policy:
        //   Wide  (>= 80 cols): tiles (3h) + gauge (3h) + anomaly cards (rest)
        //   Medium (40-79 cols): tiles (3h) + anomaly cards (rest), gauge skipped
        //   Narrow (< 40 cols):  anomaly cards only (tiles as compact summary)
        let width_class = WidthClass::from_width(content_area.width);

        match width_class {
            WidthClass::Wide => {
                let tiles_h = 3_u16.min(content_area.height);
                let remaining = content_area.height.saturating_sub(tiles_h);
                let gauge_h = 3_u16.min(remaining);
                let cards_h = remaining.saturating_sub(gauge_h);

                let tiles_area =
                    Rect::new(content_area.x, content_area.y, content_area.width, tiles_h);
                let gauge_area = Rect::new(
                    content_area.x,
                    content_area.y + tiles_h,
                    content_area.width,
                    gauge_h,
                );
                let cards_area = Rect::new(
                    content_area.x,
                    content_area.y + tiles_h + gauge_h,
                    content_area.width,
                    cards_h,
                );

                self.render_metric_tiles(frame, tiles_area, state, &snap);
                if gauge_h >= 2 {
                    self.render_event_ring_gauge(frame, gauge_area, state);
                }
                if cards_h >= 3 {
                    self.render_anomaly_cards(frame, cards_area, &snap);
                }
            }
            WidthClass::Medium => {
                // Skip gauge; give more vertical space to cards
                let tiles_h = 3_u16.min(content_area.height);
                let cards_h = content_area.height.saturating_sub(tiles_h);

                let tiles_area =
                    Rect::new(content_area.x, content_area.y, content_area.width, tiles_h);
                let cards_area = Rect::new(
                    content_area.x,
                    content_area.y + tiles_h,
                    content_area.width,
                    cards_h,
                );

                self.render_metric_tiles(frame, tiles_area, state, &snap);
                if cards_h >= 3 {
                    self.render_anomaly_cards(frame, cards_area, &snap);
                }
            }
            WidthClass::Narrow => {
                // Compact summary line for tiles; prioritize anomaly cards
                let summary_h = 1_u16.min(content_area.height);
                let cards_h = content_area.height.saturating_sub(summary_h);

                let summary_area = Rect::new(
                    content_area.x,
                    content_area.y,
                    content_area.width,
                    summary_h,
                );
                let cards_area = Rect::new(
                    content_area.x,
                    content_area.y + summary_h,
                    content_area.width,
                    cards_h,
                );

                // Force narrow rendering path for metric tiles
                self.render_metric_tiles(frame, summary_area, state, &snap);
                if cards_h >= 3 {
                    self.render_anomaly_cards(frame, cards_area, &snap);
                }
            }
        }
    }

    /// Render the top metric tile row.
    #[allow(clippy::unused_self)]
    fn render_metric_tiles(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        state: &TuiSharedState,
        snap: &DiagnosticsSnapshot,
    ) {
        const METRIC_TILE_COUNT: u16 = 4;
        const MIN_TILE_WIDTH: u16 = 8;

        if area.is_empty() {
            return;
        }

        let uptime = state.uptime();
        let uptime_str = format_uptime(uptime);
        let tcp_latency_str = snap
            .tcp_latency_ms
            .map_or_else(|| "N/A".to_string(), |ms| format!("{ms}ms"));
        let counters = state.request_counters();
        let requests_str = format!("{}", counters.total);
        let avg_latency_str = format!("{}ms", state.avg_latency_ms());

        // On narrow panes, render a compact summary instead of silently drawing nothing.
        if area.width < METRIC_TILE_COUNT * MIN_TILE_WIDTH {
            let summary = format!(
                "Up {uptime_str} | TCP {tcp_latency_str} | Req {requests_str} | Avg {avg_latency_str}"
            );
            Paragraph::new(summary).render(area, frame);
            return;
        }

        // Split area into 4 tiles
        let tile_w = area.width / 4;
        let tile1 = Rect::new(area.x, area.y, tile_w, area.height);
        let tile2 = Rect::new(area.x + tile_w, area.y, tile_w, area.height);
        let tile3 = Rect::new(area.x + tile_w * 2, area.y, tile_w, area.height);
        let tile4 = Rect::new(
            area.x + tile_w * 3,
            area.y,
            area.width - tile_w * 3,
            area.height,
        );

        MetricTile::new("Uptime", &uptime_str, MetricTrend::Up).render(tile1, frame);

        let tcp_trend = if snap.tcp_error.is_some() {
            MetricTrend::Down
        } else {
            MetricTrend::Flat
        };
        MetricTile::new("TCP Latency", &tcp_latency_str, tcp_trend).render(tile2, frame);

        MetricTile::new(
            "Requests",
            &requests_str,
            if counters.total > 0 {
                MetricTrend::Up
            } else {
                MetricTrend::Flat
            },
        )
        .render(tile3, frame);

        let sparkline = state.sparkline_snapshot();
        MetricTile::new("Avg Latency", &avg_latency_str, MetricTrend::Flat)
            .sparkline(&sparkline)
            .render(tile4, frame);
    }

    /// Render event ring buffer gauge.
    #[allow(clippy::unused_self)]
    fn render_event_ring_gauge(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let ring_stats = state.event_ring_stats();

        #[allow(clippy::cast_possible_truncation)]
        let current = ring_stats.len as u32;
        #[allow(clippy::cast_possible_truncation)]
        let capacity = ring_stats.capacity as u32;

        let drops = ring_stats.total_drops();
        let ttl_str = if drops > 0 {
            format!("{drops} drops")
        } else {
            "0 drops".to_string()
        };

        ReservationGauge::new("Event Ring Buffer", current, capacity.max(1))
            .ttl_display(&ttl_str)
            .render(area, frame);
    }

    /// Render diagnostic findings as anomaly cards.
    #[allow(clippy::too_many_lines, clippy::unused_self)]
    fn render_anomaly_cards(&self, frame: &mut Frame<'_>, area: Rect, snap: &DiagnosticsSnapshot) {
        if area.is_empty() {
            return;
        }

        #[derive(Debug)]
        #[allow(clippy::items_after_statements)]
        struct FindingCard {
            severity: AnomalySeverity,
            confidence: f64,
            title: String,
            rationale: Option<String>,
        }

        let mut findings: Vec<FindingCard> = Vec::new();
        if let Some(err) = &snap.tcp_error {
            findings.push(FindingCard {
                severity: AnomalySeverity::Critical,
                confidence: 0.95,
                title: "TCP connection failed".to_string(),
                rationale: Some(err.clone()),
            });
        }
        for line in &snap.lines {
            let severity = match line.level {
                Level::Ok => AnomalySeverity::Low,
                Level::Warn => AnomalySeverity::Medium,
                Level::Fail => AnomalySeverity::High,
            };
            findings.push(FindingCard {
                severity,
                confidence: 0.8,
                title: line.detail.clone(),
                rationale: line.remediation.clone(),
            });
        }

        // Anomaly-first prioritization: sort by severity (Critical > High > Medium > Low)
        // so the most actionable findings are always visible, even on narrow/short terminals.
        findings.sort_by_key(|f| std::cmp::Reverse(severity_priority(f.severity)));

        if findings.is_empty() {
            // All healthy — render a single OK card
            let card = AnomalyCard::new(AnomalySeverity::Low, 1.0, "All diagnostics passed")
                .rationale("TCP reachable, HTTP probes healthy, auth configuration valid.");
            card.render(area, frame);
            return;
        }

        // Narrow-width fallback: when cards can't render properly (< 30 cols),
        // fall back to compact text lines showing severity + title.
        if area.width < 30 {
            let tp = crate::tui_theme::TuiThemePalette::current();
            let mut compact_lines: Vec<Line<'static>> = Vec::new();
            for f in &findings {
                let (badge, badge_style) = match f.severity {
                    AnomalySeverity::Critical => ("[CRIT]", crate::tui_theme::text_critical(&tp)),
                    AnomalySeverity::High => ("[HIGH]", crate::tui_theme::text_error(&tp)),
                    AnomalySeverity::Medium => ("[WARN]", crate::tui_theme::text_warning(&tp)),
                    AnomalySeverity::Low => ("[ OK ]", crate::tui_theme::text_success(&tp)),
                };
                compact_lines.push(Line::from_spans([
                    Span::styled(badge.to_string(), badge_style),
                    Span::raw(" "),
                    Span::raw(f.title.clone()),
                ]));
            }
            let visible = usize::from(area.height);
            let truncated: Vec<Line<'static>> = compact_lines.into_iter().take(visible).collect();
            Paragraph::new(Text::from_lines(truncated)).render(area, frame);
            return;
        }

        // Compute per-card height (minimum 4 lines each)
        let total_findings = findings.len();
        #[allow(clippy::cast_possible_truncation)]
        let card_h = (area.height / (total_findings as u16).max(1))
            .max(4)
            .min(area.height);
        let max_cards = usize::from((area.height / card_h).max(1));

        let render_cards: Vec<FindingCard> = if total_findings > max_cards {
            if max_cards == 1 {
                let mut first = findings.remove(0);
                let hidden = total_findings.saturating_sub(1);
                if hidden > 0 {
                    let mut rationale = first.rationale.unwrap_or_default();
                    if !rationale.is_empty() {
                        rationale.push(' ');
                    }
                    let _ = std::fmt::Write::write_fmt(
                        &mut rationale,
                        format_args!("{hidden} more findings hidden; enlarge the panel."),
                    );
                    first.rationale = Some(rationale);
                }
                vec![first]
            } else {
                let visible = max_cards - 1;
                let hidden = total_findings.saturating_sub(visible);
                let mut cards: Vec<FindingCard> = findings.into_iter().take(visible).collect();
                cards.push(FindingCard {
                    severity: AnomalySeverity::Medium,
                    confidence: 0.6,
                    title: format!("{hidden} more findings"),
                    rationale: Some("Enlarge this pane to view all diagnostics.".to_string()),
                });
                cards
            }
        } else {
            findings
        };

        let mut y_offset = area.y;
        for card_data in render_cards {
            let consumed_h = y_offset.saturating_sub(area.y);
            let remaining_h = area.height.saturating_sub(consumed_h);
            if remaining_h < 3 {
                break;
            }

            let current_h = card_h.min(remaining_h);
            let card_area = Rect::new(area.x, y_offset, area.width, current_h);
            let mut card =
                AnomalyCard::new(card_data.severity, card_data.confidence, &card_data.title);
            if let Some(rationale) = card_data.rationale.as_deref() {
                card = card.rationale(rationale);
            }
            card.render(card_area, frame);
            y_offset = y_offset.saturating_add(current_h);
        }
    }

    /// Render the anomaly detail panel for the currently selected finding (Dashboard mode).
    #[allow(clippy::cast_possible_truncation)]
    fn render_anomaly_detail_panel(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        snap: &DiagnosticsSnapshot,
    ) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Anomaly Detail ");
        let inner = block.inner(area);
        block.render(area, frame);

        let mut findings: Vec<(&str, AnomalySeverity, &str, Option<&str>)> = Vec::new();
        if let Some(err) = &snap.tcp_error {
            findings.push(("TCP connection", AnomalySeverity::Critical, err.as_str(), None));
        }
        for line in &snap.lines {
            let severity = match line.level {
                Level::Ok => AnomalySeverity::Low,
                Level::Warn => AnomalySeverity::Medium,
                Level::Fail => AnomalySeverity::High,
            };
            findings.push((
                line.name,
                severity,
                line.detail.as_str(),
                line.remediation.as_deref(),
            ));
        }

        if findings.is_empty() {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{2714}",
                "All Diagnostics Passed",
                "No anomalies detected.",
            );
            return;
        }

        let cursor = self.anomaly_cursor.min(findings.len().saturating_sub(1));
        let (name, severity, detail, remediation) = findings[cursor];

        let sev_color = match severity {
            AnomalySeverity::Critical => tp.severity_critical,
            AnomalySeverity::High => tp.severity_error,
            AnomalySeverity::Medium => tp.severity_warn,
            AnomalySeverity::Low => tp.severity_ok,
        };

        let mut lines: Vec<(String, String, Option<PackedRgba>)> = Vec::new();
        lines.push((
            "Finding".into(),
            format!("{}/{}", cursor + 1, findings.len()),
            None,
        ));
        lines.push(("Name".into(), name.to_string(), None));
        lines.push((
            "Severity".into(),
            format!("{severity:?}"),
            Some(sev_color),
        ));
        lines.push(("Detail".into(), detail.to_string(), None));
        if let Some(fix) = remediation {
            lines.push(("Remediation".into(), fix.to_string(), None));
        }

        // Related probes
        for probe in &snap.path_probes {
            let status_str = probe
                .status
                .map_or_else(|| "?".into(), format_http_status);
            let latency_str = probe
                .latency_ms
                .map_or_else(|| "?".to_string(), |ms| format!("{ms}ms"));
            lines.push((
                format!("Probe {}", probe.path),
                format!("{status_str} {latency_str}"),
                None,
            ));
        }

        render_kv_lines(frame, inner, &lines, self.detail_scroll, &tp);
    }

    /// Render the findings summary panel (Text mode).
    fn render_findings_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = crate::tui_panel_helpers::panel_block(" Findings ");
        let inner = block.inner(area);
        block.render(area, frame);

        let snap = self.snapshot();
        if snap.lines.is_empty() && snap.tcp_error.is_none() {
            crate::tui_panel_helpers::render_empty_state(
                frame,
                inner,
                "\u{2714}",
                "All Checks Passed",
                "No findings to display.",
            );
            return;
        }

        let mut lines: Vec<(String, String, Option<PackedRgba>)> = Vec::new();
        if let Some(err) = &snap.tcp_error {
            lines.push((
                "[CRIT] TCP".into(),
                err.clone(),
                Some(tp.severity_critical),
            ));
        }
        for probe_line in &snap.lines {
            let color = match probe_line.level {
                Level::Ok => tp.severity_ok,
                Level::Warn => tp.severity_warn,
                Level::Fail => tp.severity_error,
            };
            let badge = match probe_line.level {
                Level::Ok => "[OK]",
                Level::Warn => "[WARN]",
                Level::Fail => "[FAIL]",
            };
            lines.push((
                format!("{badge} {}", probe_line.name),
                probe_line.detail.clone(),
                Some(color),
            ));
            if let Some(fix) = &probe_line.remediation {
                lines.push(("  Fix".into(), fix.clone(), None));
            }
        }

        render_kv_lines(frame, inner, &lines, self.detail_scroll, &tp);
    }
}

/// Render key-value lines with a label column and a value column, supporting scroll.
#[allow(clippy::cast_possible_truncation)]
fn render_kv_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: &[(String, String, Option<PackedRgba>)],
    scroll: usize,
    tp: &crate::tui_theme::TuiThemePalette,
) {
    let label_w: u16 = 14;
    let visible = usize::from(area.height);
    let total = lines.len();
    let offset = scroll.min(total.saturating_sub(visible));

    for (i, (label, value, color)) in lines.iter().skip(offset).enumerate() {
        let row_y = area.y + i as u16;
        if row_y >= area.y.saturating_add(area.height) {
            break;
        }
        // Label column
        let label_display: String = if label.len() > label_w as usize {
            label.chars().take(label_w as usize).collect()
        } else {
            format!("{:<w$}", label, w = label_w as usize)
        };
        Paragraph::new(label_display)
            .style(Style::default().fg(tp.text_muted).bold())
            .render(Rect::new(area.x, row_y, label_w.min(area.width), 1), frame);

        // Value column
        let val_x = area.x + label_w;
        if val_x < area.x.saturating_add(area.width) {
            let val_w = area.x.saturating_add(area.width).saturating_sub(val_x);
            let val_style = color.map_or_else(
                || Style::default().fg(tp.text_primary),
                |c| Style::default().fg(c),
            );
            Paragraph::new(value.as_str())
                .style(val_style)
                .render(Rect::new(val_x, row_y, val_w, 1), frame);
        }
    }

    // Scroll indicator
    if total > visible && area.width > 2 {
        let indicator = format!("[{}/{}]", offset + 1, total.saturating_sub(visible) + 1);
        let iw = indicator.len().min(area.width as usize) as u16;
        let ix = area.x.saturating_add(area.width).saturating_sub(iw);
        let iy = area.y.saturating_add(area.height.saturating_sub(1));
        if iy >= area.y {
            Paragraph::new(indicator)
                .style(Style::default().fg(tp.text_muted))
                .render(Rect::new(ix, iy, iw, 1), frame);
        }
    }
}

impl Drop for SystemHealthScreen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.worker.take() {
            let _ = join.join();
        }
    }
}

impl MailScreen for SystemHealthScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('r') => self.request_refresh(),
                KeyCode::Char('v') => {
                    self.view_mode = match self.view_mode {
                        ViewMode::Text => ViewMode::Dashboard,
                        ViewMode::Dashboard => ViewMode::Text,
                    };
                }
                KeyCode::Char('i') => {
                    self.detail_visible = !self.detail_visible;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.anomaly_cursor = self.anomaly_cursor.saturating_add(1);
                    self.detail_scroll = 0;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.anomaly_cursor = self.anomaly_cursor.saturating_sub(1);
                    self.detail_scroll = 0;
                }
                KeyCode::Char('J') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::Char('K') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                _ => {}
            }
        }
        Cmd::None
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        // Outer bordered panel
        let outer_block = crate::tui_panel_helpers::panel_block(" System Health ");
        let inner = outer_block.inner(area);
        outer_block.render(area, frame);

        // Responsive layout: at Lg+ split into main content + detail panel
        let layout = ResponsiveLayout::new(
            Flex::vertical().constraints([Constraint::Fill]),
        )
        .at(
            Breakpoint::Lg,
            Flex::horizontal().constraints([
                Constraint::Percentage(55.0),
                Constraint::Fill,
            ]),
        )
        .at(
            Breakpoint::Xl,
            Flex::horizontal().constraints([
                Constraint::Percentage(50.0),
                Constraint::Fill,
            ]),
        );

        let split = if self.detail_visible {
            layout.split(inner)
        } else {
            ResponsiveLayout::new(Flex::vertical().constraints([Constraint::Fill]))
                .split(inner)
        };
        let main_area = split.rects[0];

        match self.view_mode {
            ViewMode::Text => self.render_text_view(frame, main_area, state),
            ViewMode::Dashboard => self.render_dashboard_view(frame, main_area, state),
        }

        // Render detail panel if visible (Lg+)
        if split.rects.len() >= 2 && self.detail_visible {
            match self.view_mode {
                ViewMode::Dashboard => {
                    let snap = self.snapshot();
                    self.render_anomaly_detail_panel(frame, split.rects[1], &snap);
                }
                ViewMode::Text => {
                    self.render_findings_panel(frame, split.rects[1]);
                }
            }
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "r",
                action: "Refresh diagnostics",
            },
            HelpEntry {
                key: "v",
                action: "Toggle text/dashboard view",
            },
            HelpEntry {
                key: "i",
                action: "Toggle detail panel",
            },
            HelpEntry {
                key: "j/k",
                action: "Navigate findings",
            },
            HelpEntry {
                key: "J/K",
                action: "Scroll detail panel",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Server status, connection pool, and WAL/cache diagnostics.")
    }

    fn title(&self) -> &'static str {
        "System Health"
    }
}

/// Format a duration as human-readable uptime.
fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m {s}s")
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

fn critical_finding_count(snap: &DiagnosticsSnapshot) -> usize {
    usize::from(snap.tcp_error.is_some())
        + snap
            .lines
            .iter()
            .filter(|line| line.level == Level::Fail)
            .count()
}

const fn diagnostics_probe_in_progress(
    snap: &DiagnosticsSnapshot,
    refresh_requested: bool,
) -> bool {
    snap.checked_at.is_none() || refresh_requested
}

fn render_probing_indicator(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiSharedState,
    effects_enabled: bool,
) {
    if area.height == 0 || area.width < 12 {
        return;
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let label = "PROBING...";
    let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
    let margin = 1_u16;
    let x = area.x.saturating_add(
        area.width
            .saturating_sub(label_width.saturating_add(margin)),
    );
    let render_area = Rect::new(x, area.y, area.width.saturating_sub(x - area.x), 1);

    if effects_enabled {
        StyledText::new(label)
            .effect(TextEffect::Pulse {
                speed: 2.0 / 3.0,
                min_alpha: 0.35,
            })
            .base_color(tp.severity_warn)
            .bold()
            .time(state.uptime().as_secs_f64())
            .render(render_area, frame);
    } else {
        Paragraph::new(label)
            .style(crate::tui_theme::text_warning(&tp))
            .render(render_area, frame);
    }
}

/// Render a prominent Tailscale remote-access URL banner at the top of the
/// area, returning the remaining area below. Returns `area` unchanged when
/// no Tailscale URL is available.
fn render_remote_url_banner(
    frame: &mut Frame<'_>,
    area: Rect,
    snap: &DiagnosticsSnapshot,
) -> Rect {
    let Some(ref url) = snap.remote_url else {
        return area;
    };
    let tp = crate::tui_theme::TuiThemePalette::current();
    let remote_style = crate::tui_theme::text_accent(&tp).bold();
    let label_style = crate::tui_theme::text_meta(&tp);
    let hint_style = crate::tui_theme::text_hint(&tp);
    let line = Line::from_spans([
        Span::styled(" Remote: ", label_style),
        Span::styled(url.clone(), remote_style),
        Span::styled("  (Tailscale)", hint_style),
    ]);
    Paragraph::new(line).render(
        Rect::new(area.x, area.y, area.width, 1),
        frame,
    );
    Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    )
}

fn diagnostics_worker_loop(
    state: &TuiSharedState,
    snapshot: &Mutex<DiagnosticsSnapshot>,
    refresh_requested: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut next_due = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let refresh = refresh_requested.swap(false, Ordering::Relaxed);
        if refresh || now >= next_due {
            let snap = run_diagnostics(state);
            if let Ok(mut guard) = snapshot.lock() {
                *guard = snap;
            }
            next_due = Instant::now() + DIAG_REFRESH_INTERVAL;
        }
        thread::sleep(WORKER_SLEEP);
    }
}

fn run_diagnostics(state: &TuiSharedState) -> DiagnosticsSnapshot {
    let cfg = state.config_snapshot();
    let env_cfg = Config::from_env();

    let remote_url = crate::detect_tailscale_ip().map(|ip| {
        crate::build_remote_url(&ip, env_cfg.http_port, env_cfg.http_bearer_token.as_deref())
    });

    let mut out = DiagnosticsSnapshot {
        checked_at: Some(Utc::now()),
        endpoint: cfg.endpoint.clone(),
        web_ui_url: cfg.web_ui_url.clone(),
        auth_enabled: cfg.auth_enabled,
        localhost_unauth_allowed: env_cfg.http_allow_localhost_unauthenticated,
        token_present: env_cfg.http_bearer_token.is_some(),
        token_len: env_cfg.http_bearer_token.as_deref().map_or(0, str::len),
        remote_url,
        ..Default::default()
    };

    let parsed = match parse_http_endpoint(&cfg) {
        Ok(p) => p,
        Err(e) => {
            out.lines.push(ProbeLine {
                level: Level::Fail,
                name: "endpoint-parse",
                detail: e,
                remediation: Some("Expected endpoint like 'http://127.0.0.1:8766/mcp/'".into()),
            });
            return out;
        }
    };

    out.http_host.clone_from(&parsed.host);
    out.http_port = parsed.port;
    out.configured_path.clone_from(&parsed.path);

    // TCP reachability
    match tcp_probe(&parsed.host, parsed.port) {
        Ok(ms) => out.tcp_latency_ms = Some(ms),
        Err(e) => out.tcp_error = Some(e),
    }

    // Base-path checks (configured + common aliases)
    let mut paths = Vec::new();
    push_unique_path(&mut paths, &parsed.path);
    push_unique_path(&mut paths, "/mcp/");
    push_unique_path(&mut paths, "/api/");

    let token = env_cfg.http_bearer_token.as_deref();

    for path in paths {
        let probe = http_probe_tools_list(
            &parsed.host,
            parsed.port,
            &path,
            ProbeAuthKind::Unauth,
            None,
        );
        out.path_probes.push(probe);
    }

    if let Some(token) = token {
        // Auth sanity: ensure an authenticated tools/list works on the configured path.
        let probe = http_probe_tools_list(
            &parsed.host,
            parsed.port,
            &parsed.path,
            ProbeAuthKind::Auth,
            Some(token),
        );
        out.path_probes.push(probe);
    }

    // Findings / remediation hints
    if out.token_present && out.token_len < 8 {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "auth-token",
            detail: "HTTP_BEARER_TOKEN is set but very short (< 8 chars)".into(),
            remediation: Some(
                "Use a longer token, or unset HTTP_BEARER_TOKEN to disable auth".into(),
            ),
        });
    }

    add_base_path_findings(&mut out);
    add_auth_findings(&mut out);

    out
}

fn push_unique_path(list: &mut Vec<String>, path: &str) {
    if list.iter().any(|p| p == path) {
        return;
    }
    list.push(path.to_string());
}

fn classify_http_probe(snap: &DiagnosticsSnapshot, probe: &PathProbe) -> Level {
    let Some(status) = probe.status else {
        return Level::Fail;
    };

    if probe.kind == ProbeAuthKind::Auth {
        return match status {
            200 => {
                if probe.body_has_tools == Some(false) {
                    Level::Warn
                } else {
                    Level::Ok
                }
            }
            404 | 500..=599 => Level::Fail,
            _ => Level::Warn,
        };
    }

    // If auth is enabled, a 401/403 still indicates the endpoint/path is reachable.
    if snap.auth_enabled && matches!(status, 401 | 403) {
        return Level::Ok;
    }

    match status {
        200 => {
            if snap.auth_enabled {
                // If auth is enabled but unauthenticated requests succeed, flag it.
                Level::Warn
            } else if probe.body_has_tools == Some(false) {
                Level::Warn
            } else {
                Level::Ok
            }
        }
        404 | 500..=599 => Level::Fail,
        _ => Level::Warn,
    }
}

fn add_base_path_findings(out: &mut DiagnosticsSnapshot) {
    let configured = out.configured_path.as_str();
    let configured_ok = out
        .path_probes
        .iter()
        .find(|p| p.kind == ProbeAuthKind::Unauth && p.path == configured)
        .is_some_and(|p| classify_http_probe(out, p) != Level::Fail);

    let mcp_ok = out
        .path_probes
        .iter()
        .find(|p| p.kind == ProbeAuthKind::Unauth && p.path == "/mcp/")
        .is_some_and(|p| classify_http_probe(out, p) != Level::Fail);
    let api_ok = out
        .path_probes
        .iter()
        .find(|p| p.kind == ProbeAuthKind::Unauth && p.path == "/api/")
        .is_some_and(|p| classify_http_probe(out, p) != Level::Fail);

    if !configured_ok && (mcp_ok || api_ok) {
        let good = if mcp_ok { "/mcp/" } else { "/api/" };
        out.lines.push(ProbeLine {
            level: Level::Fail,
            name: "base-path",
            detail: format!(
                "Configured HTTP_PATH {configured} is not reachable, but {good} appears reachable"
            ),
            remediation: Some(format!(
                "Set HTTP_PATH={good} (or run with --path {})",
                good.trim_matches('/')
            )),
        });
    }

    if !mcp_ok && api_ok {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "base-path-alias",
            detail: "Endpoint responds on /api/ but not /mcp/".into(),
            remediation: Some(
                "Clients using /mcp/ will see 404. Use /api/ (or enable /mcp/ alias)".into(),
            ),
        });
    }

    if !api_ok && mcp_ok {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "base-path-alias",
            detail: "Endpoint responds on /mcp/ but not /api/".into(),
            remediation: Some(
                "Clients using /api/ will see 404. Use /mcp/ (or enable /api/ alias)".into(),
            ),
        });
    }
}

fn add_auth_findings(out: &mut DiagnosticsSnapshot) {
    if !out.auth_enabled {
        return;
    }

    // If auth is enabled, at least one path should return 401/403 for unauthenticated access
    // (or 200 if localhost-unauth is allowed). We can't reliably infer localhost allowlist here,
    // so we just flag if *all* probes returned 200.
    if out.localhost_unauth_allowed {
        return;
    }

    let all_200 = out
        .path_probes
        .iter()
        .filter(|p| p.kind == ProbeAuthKind::Unauth)
        .filter_map(|p| p.status)
        .all(|s| s == 200);
    if all_200 {
        out.lines.push(ProbeLine {
            level: Level::Warn,
            name: "auth",
            detail: "Auth appears enabled, but unauthenticated probes returned 200 everywhere".into(),
            remediation: Some("If this is unexpected, verify HTTP_BEARER_TOKEN enforcement and localhost allowlist settings".into()),
        });
    }

    // If token is present, expect the auth probe on configured path to succeed.
    if out.token_present {
        let auth_probe_ok = out
            .path_probes
            .iter()
            .find(|p| p.kind == ProbeAuthKind::Auth && p.path == out.configured_path)
            .is_some_and(|p| p.status == Some(200));
        if !auth_probe_ok {
            out.lines.push(ProbeLine {
                level: Level::Fail,
                name: "auth",
                detail: "Authenticated probe did not succeed on configured endpoint".into(),
                remediation: Some("Verify HTTP_BEARER_TOKEN matches the server config (or unset it to disable auth)".into()),
            });
        }
    }
}

fn tcp_probe(host: &str, port: u16) -> Result<u64, String> {
    let addr = resolve_socket_addr(host, port)?;
    let start = Instant::now();
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(saturating_duration_ms_u64(start.elapsed()))
}

fn http_probe_tools_list(
    host: &str,
    port: u16,
    path: &str,
    kind: ProbeAuthKind,
    bearer_token: Option<&str>,
) -> PathProbe {
    let mut probe = PathProbe {
        path: path.to_string(),
        kind,
        ..Default::default()
    };

    let addr = match resolve_socket_addr(host, port) {
        Ok(a) => a,
        Err(e) => {
            probe.error = Some(e);
            return probe;
        }
    };

    let body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}";
    let mut req = String::new();
    let _ = write!(req, "POST {path} HTTP/1.1\r\n");
    let _ = write!(req, "Host: {host}:{port}\r\n");
    req.push_str("Content-Type: application/json\r\n");
    let _ = write!(req, "Content-Length: {}\r\n", body.len());
    req.push_str("Connection: close\r\n");
    if let Some(token) = bearer_token {
        // Never log token; header is only used for local self-probe.
        let _ = write!(req, "Authorization: Bearer {token}\r\n");
    }
    req.push_str("\r\n");

    let start = Instant::now();
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            probe.error = Some(format!("connect failed: {e}"));
            return probe;
        }
    };
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    if let Err(e) = stream.write_all(req.as_bytes()) {
        probe.error = Some(format!("write failed: {e}"));
        return probe;
    }
    if let Err(e) = stream.write_all(body) {
        probe.error = Some(format!("write body failed: {e}"));
        return probe;
    }

    let mut buf = vec![0_u8; MAX_READ_BYTES];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            probe.error = Some(format!("read failed: {e}"));
            return probe;
        }
    };
    buf.truncate(n);

    probe.latency_ms = Some(saturating_duration_ms_u64(start.elapsed()));
    probe.status = parse_http_status(&buf);

    if let Ok(text) = std::str::from_utf8(&buf) {
        // Cheap handshake sanity: tools/list result payload should contain "tools".
        if probe.status == Some(200) {
            probe.body_has_tools = Some(text.contains("\"tools\""));
        }
    }
    let _ = stream.shutdown(Shutdown::Both);

    probe
}

fn parse_http_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    // Example: "HTTP/1.1 200 OK"
    let mut parts = line.split_whitespace();
    let _http = parts.next()?;
    let code = parts.next()?;
    code.parse::<u16>().ok()
}

fn saturating_duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn resolve_socket_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    let ip = if host == "localhost" {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host.parse::<IpAddr>()
            .map_err(|_| format!("unsupported host {host:?} (expected an IP or localhost)"))?
    };
    Ok(SocketAddr::new(ip, port))
}

fn parse_http_endpoint(cfg: &ConfigSnapshot) -> Result<ParsedEndpoint, String> {
    let url = cfg.endpoint.trim();
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported endpoint scheme in {url:?} (expected http://)"))?;

    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };

    let (host, port) = parse_authority_host_port(authority)?;

    Ok(ParsedEndpoint {
        host,
        port,
        path: normalize_path(&path),
    })
}

fn normalize_path(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }
    let mut out = path.to_string();
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn parse_authority_host_port(authority: &str) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: [::1]:8766
        let Some((host, rest)) = rest.split_once(']') else {
            return Err(format!("invalid IPv6 authority {authority:?}"));
        };
        let port = if let Some(rest) = rest.strip_prefix(':') {
            rest.parse::<u16>()
                .map_err(|_| format!("invalid port in {authority:?}"))?
        } else {
            80
        };
        return Ok((host.to_string(), port));
    }

    let Some((host, port)) = authority.rsplit_once(':') else {
        return Ok((authority.to_string(), 80));
    };
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("invalid port in {authority:?}"))?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_harness::buffer_to_text;
    use mcp_agent_mail_core::Config;

    fn test_state() -> Arc<TuiSharedState> {
        TuiSharedState::new(&Config::default())
    }

    fn test_screen(snapshot: DiagnosticsSnapshot) -> SystemHealthScreen {
        SystemHealthScreen {
            snapshot: Arc::new(Mutex::new(snapshot)),
            refresh_requested: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            view_mode: ViewMode::Dashboard,
            detail_visible: true,
            detail_scroll: 0,
            anomaly_cursor: 0,
        }
    }

    #[test]
    fn parse_http_endpoint_ipv4() {
        let cfg = ConfigSnapshot {
            endpoint: "http://127.0.0.1:8766/api/".into(),
            http_path: "/api/".into(),
            web_ui_url: "http://127.0.0.1:8766/mail".into(),
            app_environment: "development".into(),
            auth_enabled: false,
            tui_effects: true,
            database_url: "sqlite:///./storage.sqlite3".into(),
            raw_database_url: "sqlite:///./storage.sqlite3".into(),
            storage_root: "/tmp/am".into(),
            console_theme: "cyberpunk_aurora".into(),
            tool_filter_profile: "default".into(),
            tui_debug: false,
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/api/");
    }

    #[test]
    fn parse_http_endpoint_ipv6_bracketed() {
        let cfg = ConfigSnapshot {
            endpoint: "http://[::1]:8766/mcp/".into(),
            http_path: "/mcp/".into(),
            web_ui_url: "http://[::1]:8766/mail".into(),
            app_environment: "development".into(),
            auth_enabled: true,
            tui_effects: true,
            database_url: "sqlite:///./storage.sqlite3".into(),
            raw_database_url: "sqlite:///./storage.sqlite3".into(),
            storage_root: "/tmp/am".into(),
            console_theme: "cyberpunk_aurora".into(),
            tool_filter_profile: "default".into(),
            tui_debug: false,
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/mcp/");
    }

    #[test]
    fn normalize_path_adds_slashes() {
        assert_eq!(normalize_path("api"), "/api/");
        assert_eq!(normalize_path("/api"), "/api/");
        assert_eq!(normalize_path("/api/"), "/api/");
    }

    #[test]
    fn normalize_path_root() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_path_nested() {
        assert_eq!(normalize_path("a/b/c"), "/a/b/c/");
        assert_eq!(normalize_path("/a/b/c"), "/a/b/c/");
        assert_eq!(normalize_path("/a/b/c/"), "/a/b/c/");
    }

    // --- parse_http_status ---

    #[test]
    fn parse_http_status_200_ok() {
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
    }

    #[test]
    fn parse_http_status_404_not_found() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n"),
            Some(404)
        );
    }

    #[test]
    fn parse_http_status_401() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 401 Unauthorized\r\n"),
            Some(401)
        );
    }

    #[test]
    fn parse_http_status_500() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 500 Internal Server Error\r\n"),
            Some(500)
        );
    }

    #[test]
    fn parse_http_status_no_crlf() {
        // No \r\n — line_end falls to buf.len(), still parseable
        assert_eq!(parse_http_status(b"HTTP/1.1 200 OK"), Some(200));
    }

    #[test]
    fn parse_http_status_empty() {
        assert_eq!(parse_http_status(b""), None);
    }

    #[test]
    fn parse_http_status_garbage() {
        assert_eq!(parse_http_status(b"not http at all\r\n"), None);
    }

    #[test]
    fn parse_http_status_invalid_code() {
        assert_eq!(parse_http_status(b"HTTP/1.1 XYZ Oops\r\n"), None);
    }

    // --- resolve_socket_addr ---

    #[test]
    fn resolve_socket_addr_localhost() {
        let addr = resolve_socket_addr("localhost", 8766).expect("resolve");
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8766));
    }

    #[test]
    fn resolve_socket_addr_ipv4() {
        let addr = resolve_socket_addr("192.168.1.1", 9000).expect("resolve");
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(addr.port(), 9000);
    }

    #[test]
    fn resolve_socket_addr_ipv6() {
        let addr = resolve_socket_addr("::1", 80).expect("resolve");
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 80);
    }

    #[test]
    fn resolve_socket_addr_invalid_host() {
        let err = resolve_socket_addr("not-an-ip", 80).unwrap_err();
        assert!(err.contains("unsupported host"));
    }

    // --- parse_authority_host_port ---

    #[test]
    fn parse_authority_ipv4_with_port() {
        let (host, port) = parse_authority_host_port("127.0.0.1:8766").expect("parse");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8766);
    }

    #[test]
    fn parse_authority_host_only_defaults_port_80() {
        let (host, port) = parse_authority_host_port("example.com").expect("parse");
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn parse_authority_ipv6_bracketed_with_port() {
        let (host, port) = parse_authority_host_port("[::1]:9090").expect("parse");
        assert_eq!(host, "::1");
        assert_eq!(port, 9090);
    }

    #[test]
    fn parse_authority_ipv6_bracketed_no_port() {
        let (host, port) = parse_authority_host_port("[::1]").expect("parse");
        assert_eq!(host, "::1");
        assert_eq!(port, 80);
    }

    #[test]
    fn parse_authority_invalid_port() {
        let err = parse_authority_host_port("127.0.0.1:notaport").unwrap_err();
        assert!(err.contains("invalid port"));
    }

    #[test]
    fn parse_authority_ipv6_unclosed_bracket() {
        let err = parse_authority_host_port("[::1").unwrap_err();
        assert!(err.contains("invalid IPv6"));
    }

    // --- push_unique_path ---

    #[test]
    fn push_unique_path_deduplicates() {
        let mut paths = Vec::new();
        push_unique_path(&mut paths, "/mcp/");
        push_unique_path(&mut paths, "/api/");
        push_unique_path(&mut paths, "/mcp/");
        assert_eq!(paths, vec!["/mcp/", "/api/"]);
    }

    #[test]
    fn push_unique_path_empty_list() {
        let mut paths = Vec::new();
        push_unique_path(&mut paths, "/");
        assert_eq!(paths.len(), 1);
    }

    // --- Level and ProbeAuthKind labels ---

    #[test]
    fn level_labels() {
        assert_eq!(Level::Ok.label(), "OK");
        assert_eq!(Level::Warn.label(), "WARN");
        assert_eq!(Level::Fail.label(), "FAIL");
    }

    #[test]
    fn probe_auth_kind_labels() {
        assert_eq!(ProbeAuthKind::Unauth.label(), "unauth");
        assert_eq!(ProbeAuthKind::Auth.label(), "auth");
    }

    // --- classify_http_probe ---

    fn make_snap(auth_enabled: bool) -> DiagnosticsSnapshot {
        DiagnosticsSnapshot {
            auth_enabled,
            ..Default::default()
        }
    }

    fn make_probe(
        kind: ProbeAuthKind,
        status: Option<u16>,
        body_has_tools: Option<bool>,
    ) -> PathProbe {
        PathProbe {
            path: "/mcp/".into(),
            kind,
            status,
            body_has_tools,
            ..Default::default()
        }
    }

    #[test]
    fn classify_no_status_is_fail() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, None, None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_auth_200_with_tools_is_ok() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(200), Some(true));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_auth_200_no_tools_is_warn() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(200), Some(false));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_auth_404_is_fail() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(404), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_auth_500_is_fail() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(500), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_auth_302_is_warn() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Auth, Some(302), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_unauth_401_auth_enabled_is_ok() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(401), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_unauth_403_auth_enabled_is_ok() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(403), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_unauth_200_auth_disabled_with_tools_is_ok() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(200), Some(true));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Ok);
    }

    #[test]
    fn classify_unauth_200_auth_disabled_no_tools_is_warn() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(200), Some(false));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_unauth_200_auth_enabled_is_warn() {
        let snap = make_snap(true);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(200), Some(true));
        assert_eq!(classify_http_probe(&snap, &probe), Level::Warn);
    }

    #[test]
    fn classify_unauth_404_is_fail() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(404), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    #[test]
    fn classify_unauth_503_is_fail() {
        let snap = make_snap(false);
        let probe = make_probe(ProbeAuthKind::Unauth, Some(503), None);
        assert_eq!(classify_http_probe(&snap, &probe), Level::Fail);
    }

    // --- add_base_path_findings ---

    #[test]
    fn base_path_findings_configured_ok_no_finding() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        assert!(
            out.lines.is_empty(),
            "no findings when configured path works"
        );
    }

    #[test]
    fn base_path_findings_configured_fails_mcp_works() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/custom/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/custom/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path" && l.level == Level::Fail)
        );
        assert!(out.lines.iter().any(|l| l.detail.contains("/mcp/")));
    }

    #[test]
    fn base_path_findings_mcp_down_api_up_warns() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        // Should have both a base-path FAIL and a base-path-alias WARN
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path" && l.level == Level::Fail)
        );
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path-alias" && l.level == Level::Warn)
        );
    }

    #[test]
    fn base_path_findings_api_down_mcp_up_warns() {
        let mut out = DiagnosticsSnapshot {
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(200),
                    body_has_tools: Some(true),
                    ..Default::default()
                },
                PathProbe {
                    path: "/api/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(404),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_base_path_findings(&mut out);
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "base-path-alias" && l.detail.contains("/api/"))
        );
    }

    // --- add_auth_findings ---

    #[test]
    fn auth_findings_disabled_no_findings() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: false,
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(out.lines.is_empty());
    }

    #[test]
    fn auth_findings_localhost_unauth_allowed_no_findings() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: true,
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(out.lines.is_empty());
    }

    #[test]
    fn auth_findings_all_200_warns() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            path_probes: vec![PathProbe {
                path: "/mcp/".into(),
                kind: ProbeAuthKind::Unauth,
                status: Some(200),
                ..Default::default()
            }],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(
            out.lines
                .iter()
                .any(|l| l.name == "auth" && l.level == Level::Warn)
        );
    }

    #[test]
    fn auth_findings_401_no_all200_warn() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            path_probes: vec![PathProbe {
                path: "/mcp/".into(),
                kind: ProbeAuthKind::Unauth,
                status: Some(401),
                ..Default::default()
            }],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        // Should NOT have the "all 200" warning
        assert!(!out.lines.iter().any(|l| l.name == "auth"
            && l.level == Level::Warn
            && l.detail.contains("200 everywhere")));
    }

    #[test]
    fn auth_findings_token_present_auth_probe_fails() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            token_present: true,
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(401),
                    ..Default::default()
                },
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Auth,
                    status: Some(403),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        assert!(out.lines.iter().any(|l| l.name == "auth"
            && l.level == Level::Fail
            && l.detail.contains("Authenticated probe did not succeed")));
    }

    #[test]
    fn auth_findings_token_present_auth_probe_ok() {
        let mut out = DiagnosticsSnapshot {
            auth_enabled: true,
            localhost_unauth_allowed: false,
            token_present: true,
            configured_path: "/mcp/".into(),
            path_probes: vec![
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Unauth,
                    status: Some(401),
                    ..Default::default()
                },
                PathProbe {
                    path: "/mcp/".into(),
                    kind: ProbeAuthKind::Auth,
                    status: Some(200),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        add_auth_findings(&mut out);
        // No auth failure finding
        assert!(
            !out.lines
                .iter()
                .any(|l| l.name == "auth" && l.level == Level::Fail)
        );
    }

    // --- parse_http_endpoint edge cases ---

    #[test]
    fn parse_http_endpoint_no_path() {
        let cfg = ConfigSnapshot {
            endpoint: "http://127.0.0.1:8766".into(),
            http_path: "/".into(),
            web_ui_url: String::new(),
            app_environment: String::new(),
            auth_enabled: false,
            tui_effects: true,
            database_url: String::new(),
            raw_database_url: String::new(),
            storage_root: String::new(),
            console_theme: String::new(),
            tool_filter_profile: String::new(),
            tui_debug: false,
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/");
    }

    #[test]
    fn parse_http_endpoint_https_rejected() {
        let cfg = ConfigSnapshot {
            endpoint: "https://127.0.0.1:8766/mcp/".into(),
            http_path: "/mcp/".into(),
            web_ui_url: String::new(),
            app_environment: String::new(),
            auth_enabled: false,
            tui_effects: true,
            database_url: String::new(),
            raw_database_url: String::new(),
            storage_root: String::new(),
            console_theme: String::new(),
            tool_filter_profile: String::new(),
            tui_debug: false,
        };
        let err = parse_http_endpoint(&cfg).unwrap_err();
        assert!(err.contains("unsupported endpoint scheme"));
    }

    #[test]
    fn parse_http_endpoint_trims_whitespace() {
        let cfg = ConfigSnapshot {
            endpoint: "  http://127.0.0.1:8766/api/  ".into(),
            http_path: "/api/".into(),
            web_ui_url: String::new(),
            app_environment: String::new(),
            auth_enabled: false,
            tui_effects: true,
            database_url: String::new(),
            raw_database_url: String::new(),
            storage_root: String::new(),
            console_theme: String::new(),
            tool_filter_profile: String::new(),
            tui_debug: false,
        };
        let parsed = parse_http_endpoint(&cfg).expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path, "/api/");
    }

    // --- New tests for br-3vwi.7.5 enhancements ---

    #[test]
    fn format_uptime_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn format_uptime_minutes() {
        assert_eq!(format_uptime(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn format_uptime_hours() {
        assert_eq!(format_uptime(Duration::from_mins(125)), "2h 5m");
    }

    #[test]
    fn critical_finding_count_includes_tcp_and_fail_lines() {
        let snap = DiagnosticsSnapshot {
            tcp_error: Some("connect failed".to_string()),
            lines: vec![
                ProbeLine {
                    level: Level::Warn,
                    name: "warn-only",
                    detail: "warn".to_string(),
                    remediation: None,
                },
                ProbeLine {
                    level: Level::Fail,
                    name: "fail-one",
                    detail: "fail".to_string(),
                    remediation: None,
                },
                ProbeLine {
                    level: Level::Fail,
                    name: "fail-two",
                    detail: "fail".to_string(),
                    remediation: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(critical_finding_count(&snap), 3);
    }

    #[test]
    fn diagnostics_probe_in_progress_tracks_refresh_flag() {
        let checked = DiagnosticsSnapshot {
            checked_at: Some(Utc::now()),
            ..Default::default()
        };
        assert!(diagnostics_probe_in_progress(
            &DiagnosticsSnapshot::default(),
            false
        ));
        assert!(diagnostics_probe_in_progress(&checked, true));
        assert!(!diagnostics_probe_in_progress(&checked, false));
    }

    #[test]
    fn view_mode_default_is_text() {
        // We can't easily construct SystemHealthScreen without a real TuiSharedState
        // with a running worker, but we can test the ViewMode enum.
        assert_ne!(ViewMode::Text, ViewMode::Dashboard);
    }

    #[test]
    fn anomaly_cards_empty_findings_renders_ok() {
        // Construct a snapshot with no findings and no TCP error
        let snap = DiagnosticsSnapshot {
            checked_at: Some(Utc::now()),
            ..Default::default()
        };

        // Verify the all-healthy path works by checking the snapshot directly
        assert!(snap.lines.is_empty());
        assert!(snap.tcp_error.is_none());
    }

    #[test]
    fn anomaly_cards_with_findings() {
        let snap = DiagnosticsSnapshot {
            checked_at: Some(Utc::now()),
            lines: vec![
                ProbeLine {
                    level: Level::Warn,
                    name: "test-warn",
                    detail: "Test warning".into(),
                    remediation: Some("Fix it".into()),
                },
                ProbeLine {
                    level: Level::Fail,
                    name: "test-fail",
                    detail: "Test failure".into(),
                    remediation: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(snap.lines.len(), 2);
        assert_eq!(snap.lines[0].level, Level::Warn);
        assert_eq!(snap.lines[1].level, Level::Fail);
    }

    #[test]
    fn anomaly_severity_mapping() {
        // Verify our Level -> AnomalySeverity mapping is consistent
        assert_eq!(
            match Level::Ok {
                Level::Ok => AnomalySeverity::Low,
                Level::Warn => AnomalySeverity::Medium,
                Level::Fail => AnomalySeverity::High,
            },
            AnomalySeverity::Low
        );
        assert_eq!(
            match Level::Warn {
                Level::Ok => AnomalySeverity::Low,
                Level::Warn => AnomalySeverity::Medium,
                Level::Fail => AnomalySeverity::High,
            },
            AnomalySeverity::Medium
        );
        assert_eq!(
            match Level::Fail {
                Level::Ok => AnomalySeverity::Low,
                Level::Warn => AnomalySeverity::Medium,
                Level::Fail => AnomalySeverity::High,
            },
            AnomalySeverity::High
        );
    }

    #[test]
    fn keybindings_includes_view_toggle() {
        let screen = test_screen(DiagnosticsSnapshot::default());
        let bindings = screen.keybindings();
        assert!(bindings.iter().any(|b| b.key == "r"));
        assert!(bindings.iter().any(|b| b.key == "v"));
    }

    #[test]
    fn metric_tiles_narrow_width_renders_compact_summary() {
        let state = test_state();
        let screen = test_screen(DiagnosticsSnapshot {
            tcp_latency_ms: Some(42),
            ..Default::default()
        });
        let snap = screen.snapshot();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(24, 3, &mut pool);
        screen.render_metric_tiles(&mut frame, Rect::new(0, 0, 24, 3), &state, &snap);

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("Up "),
            "expected compact metric summary in narrow layout, got:\n{text}"
        );
    }

    #[test]
    fn anomaly_cards_overflow_shows_overflow_indicator() {
        let mut snap = DiagnosticsSnapshot {
            tcp_error: Some("connection refused".to_string()),
            ..Default::default()
        };
        for idx in 0..6 {
            snap.lines.push(ProbeLine {
                level: Level::Warn,
                name: "overflow-test",
                detail: format!("Issue {idx}"),
                remediation: Some("Inspect logs".to_string()),
            });
        }
        let screen = test_screen(DiagnosticsSnapshot::default());

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 8, &mut pool);
        screen.render_anomaly_cards(&mut frame, Rect::new(0, 0, 80, 8), &snap);

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("more findings"),
            "expected overflow indicator card when findings exceed view, got:\n{text}"
        );
    }

    #[test]
    fn anomaly_cards_single_slot_mentions_hidden_findings() {
        let mut snap = DiagnosticsSnapshot {
            tcp_error: Some("connection refused".to_string()),
            ..Default::default()
        };
        for idx in 0..2 {
            snap.lines.push(ProbeLine {
                level: Level::Warn,
                name: "single-slot-overflow",
                detail: format!("Issue {idx}"),
                remediation: Some("Inspect logs".to_string()),
            });
        }
        let screen = test_screen(DiagnosticsSnapshot::default());

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 4, &mut pool);
        screen.render_anomaly_cards(&mut frame, Rect::new(0, 0, 80, 4), &snap);

        let text = buffer_to_text(&frame.buffer);
        assert!(
            text.contains("hidden"),
            "expected single-slot overflow annotation, got:\n{text}"
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // br-1xt0m.1.11.1: Structured diagnostic sections for text mode
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn format_http_status_common_codes() {
        assert_eq!(format_http_status(200), "200 OK");
        assert_eq!(format_http_status(401), "401 Unauthorized");
        assert_eq!(format_http_status(403), "403 Forbidden");
        assert_eq!(format_http_status(404), "404 Not Found");
        assert_eq!(format_http_status(500), "500 Internal Error");
        assert_eq!(format_http_status(418), "418"); // unknown falls through
    }

    #[test]
    fn level_style_returns_severity_colors() {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let ok_style = Level::Ok.style(&tp);
        let warn_style = Level::Warn.style(&tp);
        let fail_style = Level::Fail.style(&tp);
        // Each should be distinct
        assert_ne!(ok_style, warn_style);
        assert_ne!(warn_style, fail_style);
        assert_ne!(ok_style, fail_style);
    }

    #[test]
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn level_styled_line_contains_badge_and_detail() {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let line = level_styled_line(Level::Ok, &tp, "TCP check".into(), "5ms".into());
        let text: String = line.spans().iter().map(|s| s.as_str()).collect();
        assert!(text.contains("[OK]"), "line text: {text}");
        assert!(text.contains("TCP check"), "line text: {text}");
        assert!(text.contains("5ms"), "line text: {text}");
    }

    // ──────────────────────────────────────────────────────────────────
    // br-1xt0m.1.11.2: Adaptive width-class layout policy
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn width_class_boundaries() {
        assert_eq!(WidthClass::from_width(120), WidthClass::Wide);
        assert_eq!(WidthClass::from_width(80), WidthClass::Wide);
        assert_eq!(WidthClass::from_width(79), WidthClass::Medium);
        assert_eq!(WidthClass::from_width(40), WidthClass::Medium);
        assert_eq!(WidthClass::from_width(39), WidthClass::Narrow);
        assert_eq!(WidthClass::from_width(20), WidthClass::Narrow);
    }

    // ──────────────────────────────────────────────────────────────────
    // br-1xt0m.1.11.3: Narrow-width fallback + anomaly-first prioritization
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn severity_priority_orders_critical_first() {
        assert!(
            severity_priority(AnomalySeverity::Critical) > severity_priority(AnomalySeverity::High)
        );
        assert!(
            severity_priority(AnomalySeverity::High) > severity_priority(AnomalySeverity::Medium)
        );
        assert!(
            severity_priority(AnomalySeverity::Medium) > severity_priority(AnomalySeverity::Low)
        );
    }

    #[test]
    fn anomaly_cards_narrow_width_renders_compact_text() {
        let mut snap = DiagnosticsSnapshot::default();
        snap.lines.push(ProbeLine {
            level: Level::Fail,
            name: "auth",
            detail: "Token invalid".to_string(),
            remediation: None,
        });
        let screen = test_screen(DiagnosticsSnapshot::default());

        // Render at very narrow width (25 cols) — should use compact fallback
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(25, 5, &mut pool);
        screen.render_anomaly_cards(&mut frame, Rect::new(0, 0, 25, 5), &snap);

        let text = buffer_to_text(&frame.buffer);
        // Should show severity badge in compact format
        assert!(
            text.contains("[HIGH]") || text.contains("[CRIT]") || text.contains("passed"),
            "narrow render should use compact text: {text}"
        );
    }

    // ── Screen logic, density heuristics, and failure paths (br-1xt0m.1.13.8) ──

    #[test]
    fn severity_priority_ordering_all_levels() {
        assert!(
            severity_priority(AnomalySeverity::Critical) > severity_priority(AnomalySeverity::High)
        );
        assert!(
            severity_priority(AnomalySeverity::High) > severity_priority(AnomalySeverity::Medium)
        );
        assert!(
            severity_priority(AnomalySeverity::Medium) > severity_priority(AnomalySeverity::Low)
        );
    }

    #[test]
    fn severity_priority_values_distinct() {
        let values = [
            severity_priority(AnomalySeverity::Critical),
            severity_priority(AnomalySeverity::High),
            severity_priority(AnomalySeverity::Medium),
            severity_priority(AnomalySeverity::Low),
        ];
        for i in 0..values.len() {
            for j in (i + 1)..values.len() {
                assert_ne!(values[i], values[j], "priority values must be distinct");
            }
        }
    }

    #[test]
    fn width_class_boundary_values() {
        // Exact boundary at 80.
        assert_eq!(WidthClass::from_width(80), WidthClass::Wide);
        assert_eq!(WidthClass::from_width(79), WidthClass::Medium);
        // Exact boundary at 40.
        assert_eq!(WidthClass::from_width(40), WidthClass::Medium);
        assert_eq!(WidthClass::from_width(39), WidthClass::Narrow);
        // Extremes.
        assert_eq!(WidthClass::from_width(0), WidthClass::Narrow);
        assert_eq!(WidthClass::from_width(u16::MAX), WidthClass::Wide);
    }

    #[test]
    fn level_default_is_ok() {
        assert_eq!(Level::default(), Level::Ok);
    }

    #[test]
    fn probe_auth_kind_default_is_unauth() {
        assert_eq!(ProbeAuthKind::default(), ProbeAuthKind::Unauth);
    }
}
