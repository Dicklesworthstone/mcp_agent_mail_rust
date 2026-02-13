//! Dashboard screen — the default landing surface for `AgentMailTUI`.
//!
//! Displays real-time stats, a live event log, and health alarms in a
//! responsive layout that adapts from 80×24 to 200×50+.

use std::collections::HashSet;

use ftui::Style;
use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba};
use ftui_extras::markdown::MarkdownTheme;
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{DbStatSnapshot, EventSeverity, MailEvent, MailEventKind, VerbosityTier};
use crate::tui_layout::{
    DensityHint, PanelConstraint, PanelPolicy, PanelSlot, ReactiveLayout, SplitAxis, TerminalClass,
};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::{
    AnomalyCard, AnomalySeverity, MetricTile, MetricTrend, PercentileRibbon, PercentileSample,
};
use ftui_widgets::sparkline::Sparkline;

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max event log entries kept in scroll-back.
const EVENT_LOG_CAPACITY: usize = 2000;

/// Stat tiles refresh every N ticks (100ms each → 1 s).
const STAT_REFRESH_TICKS: u64 = 10;

// NOTE: SPARK_CHARS removed in br-2bbt.4.1 — now using ftui_widgets::Sparkline

// ── Panel budgets ────────────────────────────────────────────────────

/// Summary band height (`MetricTile` row) by terminal class.
const fn summary_band_height(tc: TerminalClass) -> u16 {
    match tc {
        TerminalClass::Tiny => 1,
        _ => 3,
    }
}

/// Anomaly rail height (0 when no anomalies or terminal too small).
const fn anomaly_rail_height(tc: TerminalClass, anomaly_count: usize) -> u16 {
    if anomaly_count == 0 {
        return 0;
    }
    match tc {
        TerminalClass::Tiny | TerminalClass::Compact => 0,
        _ => 4,
    }
}

/// Footer height by terminal class.
const fn footer_bar_height(tc: TerminalClass) -> u16 {
    match tc {
        TerminalClass::Tiny => 0,
        _ => 1,
    }
}

/// Max percentile samples to retain.
const PERCENTILE_HISTORY_CAP: usize = 120;

/// Max throughput samples to retain.
const THROUGHPUT_HISTORY_CAP: usize = 120;

/// Anomaly thresholds.
const ACK_PENDING_WARN: u64 = 3;
const ACK_PENDING_HIGH: u64 = 10;
const ERROR_RATE_WARN: f64 = 0.05;
const ERROR_RATE_HIGH: f64 = 0.15;
const RING_FILL_WARN: u8 = 80;

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn reduced_motion_enabled() -> bool {
    env_flag_enabled("AM_TUI_REDUCED_MOTION") || env_flag_enabled("AM_TUI_A11Y_REDUCED_MOTION")
}

// ── Detected anomaly ─────────────────────────────────────────────────

/// A runtime-detected anomaly for the anomaly/action rail.
#[derive(Debug, Clone)]
pub(crate) struct DetectedAnomaly {
    pub(crate) severity: AnomalySeverity,
    pub(crate) confidence: f64,
    pub(crate) headline: String,
    pub(crate) rationale: String,
}

// ──────────────────────────────────────────────────────────────────────
// DashboardScreen
// ──────────────────────────────────────────────────────────────────────

/// The main dashboard screen.
#[allow(clippy::struct_excessive_bools)]
pub struct DashboardScreen {
    /// Cached event log lines (rendered from `MailEvent`s).
    event_log: Vec<EventEntry>,
    /// Last sequence number consumed from the ring buffer.
    last_seq: u64,
    /// Scroll offset from the bottom (0 = auto-follow).
    scroll_offset: usize,
    /// Whether auto-follow is enabled.
    auto_follow: bool,
    /// Active event kind filters (empty = show all).
    type_filter: HashSet<MailEventKind>,
    /// Verbosity tier controlling minimum severity shown.
    verbosity: VerbosityTier,
    /// Previous `DbStatSnapshot` for delta indicators.
    prev_db_stats: DbStatSnapshot,
    /// Sparkline data: recent latency samples.
    sparkline_data: Vec<f64>,
    // ── Showcase composition state ───────────────────────────────
    /// Detected anomalies (refreshed each stat tick).
    anomalies: Vec<DetectedAnomaly>,
    /// Rolling percentile samples for the trend ribbon.
    percentile_history: Vec<PercentileSample>,
    /// Rolling throughput samples (requests per stat interval).
    throughput_history: Vec<f64>,
    /// Previous request total for delta/trend computation.
    prev_req_total: u64,
    /// Whether the trend panel is visible (toggled by user).
    show_trend_panel: bool,
    /// Metadata for the most recent message event, rendered as markdown.
    recent_message_preview: Option<RecentMessagePreview>,
    /// Animation phase for pulse effects.
    pulse_phase: f32,
    /// Reduced-motion mode disables pulse animation.
    reduced_motion: bool,
}

/// A pre-formatted event log entry.
#[derive(Debug, Clone)]
pub(crate) struct EventEntry {
    pub(crate) kind: MailEventKind,
    pub(crate) severity: EventSeverity,
    pub(crate) seq: u64,
    pub(crate) timestamp_micros: i64,
    pub(crate) timestamp: String,
    pub(crate) icon: char,
    pub(crate) summary: String,
}

/// Dashboard preview payload for the most recent message event.
#[derive(Debug, Clone)]
struct RecentMessagePreview {
    direction: &'static str,
    timestamp: String,
    from: String,
    to: String,
    subject: String,
    thread_id: String,
    project: String,
}

impl RecentMessagePreview {
    fn from_event(event: &MailEvent) -> Option<Self> {
        match event {
            MailEvent::MessageSent {
                timestamp_micros,
                from,
                to,
                subject,
                thread_id,
                project,
                ..
            } => Some(Self {
                direction: "Outbound",
                timestamp: format_ts(*timestamp_micros),
                from: from.clone(),
                to: summarize_recipients(to),
                subject: subject.clone(),
                thread_id: thread_id.clone(),
                project: project.clone(),
            }),
            MailEvent::MessageReceived {
                timestamp_micros,
                from,
                to,
                subject,
                thread_id,
                project,
                ..
            } => Some(Self {
                direction: "Inbound",
                timestamp: format_ts(*timestamp_micros),
                from: from.clone(),
                to: summarize_recipients(to),
                subject: subject.clone(),
                thread_id: thread_id.clone(),
                project: project.clone(),
            }),
            _ => None,
        }
    }

    fn to_markdown(&self) -> String {
        let subject = if self.subject.trim().is_empty() {
            "(no subject)"
        } else {
            truncate(&self.subject, 160)
        };
        let thread = if self.thread_id.trim().is_empty() {
            "(none)"
        } else {
            self.thread_id.as_str()
        };
        let project = if self.project.trim().is_empty() {
            "(unknown)"
        } else {
            self.project.as_str()
        };

        format!(
            "### {} Message · {}\n\n**{}**\n\n- **From:** `{}`\n- **To:** `{}`\n- **Thread:** `{}`\n- **Project:** `{}`\n\n_Preview is derived from event metadata; open Messages/Threads for full body._",
            self.direction, self.timestamp, subject, self.from, self.to, thread, project
        )
    }
}

impl DashboardScreen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            event_log: Vec::with_capacity(EVENT_LOG_CAPACITY),
            last_seq: 0,
            scroll_offset: 0,
            auto_follow: true,
            type_filter: HashSet::new(),
            verbosity: VerbosityTier::default(),
            prev_db_stats: DbStatSnapshot::default(),
            sparkline_data: Vec::with_capacity(60),
            anomalies: Vec::new(),
            percentile_history: Vec::with_capacity(PERCENTILE_HISTORY_CAP),
            throughput_history: Vec::with_capacity(THROUGHPUT_HISTORY_CAP),
            prev_req_total: 0,
            show_trend_panel: true,
            recent_message_preview: None,
            pulse_phase: 0.0,
            reduced_motion: reduced_motion_enabled(),
        }
    }

    /// Ingest new events from the ring buffer.
    fn ingest_events(&mut self, state: &TuiSharedState) {
        let new_events = state.events_since(self.last_seq);
        for event in &new_events {
            self.last_seq = event.seq().max(self.last_seq);
            if let Some(preview) = RecentMessagePreview::from_event(event) {
                self.recent_message_preview = Some(preview);
            }
            self.event_log.push(format_event(event));
        }
        // Trim to capacity
        if self.event_log.len() > EVENT_LOG_CAPACITY {
            let excess = self.event_log.len() - EVENT_LOG_CAPACITY;
            self.event_log.drain(..excess);
        }
    }

    /// Visible entries after applying verbosity tier and type filter.
    fn visible_entries(&self) -> Vec<&EventEntry> {
        self.event_log
            .iter()
            .filter(|e| {
                self.verbosity.includes(e.severity)
                    && (self.type_filter.is_empty() || self.type_filter.contains(&e.kind))
            })
            .collect()
    }

    /// Detect anomalies from current state.
    #[allow(clippy::cast_precision_loss, clippy::unused_self)]
    fn detect_anomalies(&self, state: &TuiSharedState) -> Vec<DetectedAnomaly> {
        let mut out = Vec::new();
        let counters = state.request_counters();
        let db = state.db_stats_snapshot().unwrap_or_default();
        let ring = state.event_ring_stats();

        // Ack pending anomaly.
        if db.ack_pending >= ACK_PENDING_HIGH {
            out.push(DetectedAnomaly {
                severity: AnomalySeverity::High,
                confidence: 0.95,
                headline: format!("{} messages awaiting acknowledgement", db.ack_pending),
                rationale: "High ack backlog may indicate stalled agents".into(),
            });
        } else if db.ack_pending >= ACK_PENDING_WARN {
            out.push(DetectedAnomaly {
                severity: AnomalySeverity::Medium,
                confidence: 0.7,
                headline: format!("{} ack-pending messages", db.ack_pending),
                rationale: "Monitor for growing backlog".into(),
            });
        }

        // Error rate anomaly.
        if counters.total > 20 {
            let err_rate = counters.status_5xx as f64 / counters.total as f64;
            if err_rate >= ERROR_RATE_HIGH {
                out.push(DetectedAnomaly {
                    severity: AnomalySeverity::Critical,
                    confidence: 0.9,
                    headline: format!("5xx error rate {:.0}%", err_rate * 100.0),
                    rationale: format!(
                        "{} of {} requests failed",
                        counters.status_5xx, counters.total
                    ),
                });
            } else if err_rate >= ERROR_RATE_WARN {
                out.push(DetectedAnomaly {
                    severity: AnomalySeverity::High,
                    confidence: 0.8,
                    headline: format!("Elevated 5xx rate {:.1}%", err_rate * 100.0),
                    rationale: "Server errors above threshold".into(),
                });
            }
        }

        // Ring buffer backpressure.
        if ring.fill_pct() >= RING_FILL_WARN {
            out.push(DetectedAnomaly {
                severity: AnomalySeverity::Medium,
                confidence: 0.85,
                headline: format!("Event ring {}% full", ring.fill_pct()),
                rationale: format!("{} events dropped", ring.total_drops()),
            });
        }

        out
    }

    /// Compute approximate percentiles from sparkline data.
    fn compute_percentile(data: &[f64]) -> PercentileSample {
        if data.is_empty() {
            return PercentileSample {
                p50: 0.0,
                p95: 0.0,
                p99: 0.0,
            };
        }
        let mut sorted: Vec<f64> = data.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let len = sorted.len();
        PercentileSample {
            p50: sorted[len / 2],
            p95: sorted[(len * 95 / 100).min(len - 1)],
            p99: sorted[(len * 99 / 100).min(len - 1)],
        }
    }

    /// Render the event log into the given area (delegates to the free function).
    fn render_event_log_panel(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let _ = state; // used for future deep-link context
        render_event_log(
            frame,
            area,
            &self.visible_entries(),
            self.scroll_offset,
            self.auto_follow,
            &self.type_filter,
            self.verbosity,
            self.pulse_phase,
            self.reduced_motion,
        );
    }

    /// Build the `ReactiveLayout` for the main content area.
    ///
    /// Layout contains:
    /// - Primary event log
    /// - Optional trend panel (right rail)
    /// - Recent message markdown preview (bottom rail on wide terminals)
    fn main_content_layout(show_trend_panel: bool) -> ReactiveLayout {
        let mut layout = ReactiveLayout::new()
            // Primary anchor for horizontal splitting (footer rail).
            .panel(PanelPolicy::new(
                PanelSlot::Primary,
                0,
                SplitAxis::Horizontal,
                PanelConstraint::visible(1.0, 20),
            ))
            // Primary anchor for vertical splitting (trend inspector).
            .panel(PanelPolicy::new(
                PanelSlot::Primary,
                0,
                SplitAxis::Vertical,
                PanelConstraint::visible(1.0, 20),
            ));

        if show_trend_panel {
            layout = layout.panel(
                PanelPolicy::new(
                    PanelSlot::Inspector,
                    1,
                    SplitAxis::Vertical,
                    PanelConstraint::HIDDEN,
                )
                .at(TerminalClass::Wide, PanelConstraint::visible(0.35, 30))
                .at(TerminalClass::UltraWide, PanelConstraint::visible(0.40, 40)),
            );
        }

        layout.panel(
            PanelPolicy::new(
                PanelSlot::Footer,
                2,
                SplitAxis::Horizontal,
                PanelConstraint::HIDDEN,
            )
            .at(TerminalClass::Wide, PanelConstraint::visible(0.30, 8))
            .at(TerminalClass::UltraWide, PanelConstraint::visible(0.28, 9)),
        )
    }
}

impl Default for DashboardScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for DashboardScreen {
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    // Scroll
                    KeyCode::Char('j') | KeyCode::Down => {
                        if self.scroll_offset > 0 {
                            self.scroll_offset = self.scroll_offset.saturating_sub(1);
                        }
                        if self.scroll_offset == 0 {
                            self.auto_follow = true;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.scroll_offset += 1;
                        self.auto_follow = false;
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        self.scroll_offset = 0;
                        self.auto_follow = true;
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        let visible = self.visible_entries();
                        self.scroll_offset = visible.len().saturating_sub(1);
                        self.auto_follow = false;
                    }
                    // Toggle follow mode
                    KeyCode::Char('f') => {
                        self.auto_follow = !self.auto_follow;
                        if self.auto_follow {
                            self.scroll_offset = 0;
                        }
                    }
                    // Deep-link: jump to Timeline at focused event timestamp.
                    KeyCode::Enter => {
                        let visible = self.visible_entries();
                        let idx = visible.len().saturating_sub(1 + self.scroll_offset);
                        if let Some(entry) = visible.get(idx) {
                            return Cmd::msg(MailScreenMsg::DeepLink(
                                DeepLinkTarget::TimelineAtTime(entry.timestamp_micros),
                            ));
                        }
                    }
                    // Cycle verbosity tier
                    KeyCode::Char('v') => {
                        self.verbosity = self.verbosity.next();
                    }
                    // Toggle trend panel visibility
                    KeyCode::Char('p') => {
                        self.show_trend_panel = !self.show_trend_panel;
                    }
                    // Toggle type filter
                    KeyCode::Char('t') => {
                        // Cycle through filter states:
                        // empty -> ToolCallEnd only -> MessageSent only -> HttpRequest only -> clear
                        if self.type_filter.is_empty() {
                            self.type_filter.insert(MailEventKind::ToolCallEnd);
                        } else if self.type_filter.contains(&MailEventKind::ToolCallEnd) {
                            self.type_filter.clear();
                            self.type_filter.insert(MailEventKind::MessageSent);
                        } else if self.type_filter.contains(&MailEventKind::MessageSent) {
                            self.type_filter.clear();
                            self.type_filter.insert(MailEventKind::HttpRequest);
                        } else {
                            self.type_filter.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
        Cmd::None
    }

    #[allow(clippy::cast_precision_loss)]
    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        // Update animation phase
        if self.reduced_motion {
            self.pulse_phase = 0.0;
        } else {
            self.pulse_phase += 0.2;
            if self.pulse_phase > std::f32::consts::PI * 2.0 {
                self.pulse_phase -= std::f32::consts::PI * 2.0;
            }
        }

        // Ingest new events every tick
        self.ingest_events(state);

        // Refresh sparkline from per-request latency samples
        self.sparkline_data = state.sparkline_snapshot();

        // Refresh stats and compute trends on stat interval
        if tick_count % STAT_REFRESH_TICKS == 0 {
            if let Some(stats) = state.db_stats_snapshot() {
                self.prev_db_stats = stats;
            }

            // Compute anomalies
            self.anomalies = self.detect_anomalies(state);

            // Track latency percentiles
            if !self.sparkline_data.is_empty() {
                let sample = Self::compute_percentile(&self.sparkline_data);
                self.percentile_history.push(sample);
                if self.percentile_history.len() > PERCENTILE_HISTORY_CAP {
                    self.percentile_history
                        .drain(..self.percentile_history.len() - PERCENTILE_HISTORY_CAP);
                }
            }

            // Track throughput (delta requests since last stat tick)
            let counters = state.request_counters();
            let delta = counters.total.saturating_sub(self.prev_req_total);
            self.throughput_history.push(delta as f64);
            if self.throughput_history.len() > THROUGHPUT_HISTORY_CAP {
                self.throughput_history
                    .drain(..self.throughput_history.len() - THROUGHPUT_HISTORY_CAP);
            }
            self.prev_req_total = counters.total;
        }
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let tc = TerminalClass::from_rect(area);
        let density = DensityHint::from_terminal_class(tc);

        // ── Panel budgets (explicit per terminal class) ──────────
        let summary_h = summary_band_height(tc);
        let anomaly_h = anomaly_rail_height(tc, self.anomalies.len());
        let footer_h = footer_bar_height(tc);
        let chrome_h = summary_h + anomaly_h + footer_h;
        let main_h = area.height.saturating_sub(chrome_h).max(3);

        // ── Rect allocation ──────────────────────────────────────
        let mut y = area.y;
        let summary_area = Rect::new(area.x, y, area.width, summary_h);
        y += summary_h;
        let anomaly_area = Rect::new(area.x, y, area.width, anomaly_h);
        y += anomaly_h;
        let main_area = Rect::new(area.x, y, area.width, main_h);
        y += main_h;
        let footer_area = Rect::new(area.x, y, area.width, footer_h);

        // ── Render bands ─────────────────────────────────────────
        render_summary_band(
            frame,
            summary_area,
            state,
            &self.prev_db_stats,
            density,
            self.pulse_phase,
        );

        if anomaly_h > 0 {
            render_anomaly_rail(frame, anomaly_area, &self.anomalies);
        }

        // Main: event log + optional trend panel + recent message markdown preview.
        let layout = Self::main_content_layout(self.show_trend_panel);
        let comp = layout.compute(main_area);
        self.render_event_log_panel(frame, comp.primary(), state);
        if let Some(trend_rect) = comp.rect(PanelSlot::Inspector) {
            render_trend_panel(
                frame,
                trend_rect,
                &self.percentile_history,
                &self.throughput_history,
            );
        }
        if let Some(preview_rect) = comp.rect(PanelSlot::Footer) {
            render_recent_message_preview_panel(
                frame,
                preview_rect,
                self.recent_message_preview.as_ref(),
            );
        }

        if footer_h > 0 {
            render_footer(frame, footer_area, state);
        }
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Scroll event log",
            },
            HelpEntry {
                key: "Enter",
                action: "Timeline at event",
            },
            HelpEntry {
                key: "f",
                action: "Toggle auto-follow",
            },
            HelpEntry {
                key: "v",
                action: "Cycle verbosity tier",
            },
            HelpEntry {
                key: "t",
                action: "Cycle type filter",
            },
            HelpEntry {
                key: "G",
                action: "Jump to bottom",
            },
            HelpEntry {
                key: "g",
                action: "Jump to top",
            },
            HelpEntry {
                key: "p",
                action: "Toggle trend panel",
            },
        ]
    }

    fn title(&self) -> &'static str {
        "Dashboard"
    }

    fn tab_label(&self) -> &'static str {
        "Dash"
    }
}

// ──────────────────────────────────────────────────────────────────────
// Event formatting
// ──────────────────────────────────────────────────────────────────────

/// Icons for each event kind.
const fn event_icon(kind: MailEventKind) -> char {
    match kind {
        MailEventKind::ToolCallStart | MailEventKind::ToolCallEnd => '⚙',
        MailEventKind::MessageSent => '✉',
        MailEventKind::MessageReceived => '📨',
        MailEventKind::ReservationGranted => '🔒',
        MailEventKind::ReservationReleased => '🔓',
        MailEventKind::AgentRegistered => '👤',
        MailEventKind::HttpRequest => '↔',
        MailEventKind::HealthPulse => '♥',
        MailEventKind::ServerStarted => '▶',
        MailEventKind::ServerShutdown => '⏹',
    }
}

/// Format a timestamp (microseconds) as HH:MM:SS.mmm.
fn format_ts(micros: i64) -> String {
    let secs = micros / 1_000_000;
    let millis = (micros % 1_000_000).unsigned_abs() / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        h.unsigned_abs(),
        m.unsigned_abs(),
        s.unsigned_abs(),
        millis,
    )
}

/// Format a single `MailEvent` into a compact log entry.
#[must_use]
#[allow(clippy::too_many_lines)]
pub(crate) fn format_event(event: &MailEvent) -> EventEntry {
    let kind = event.kind();
    let icon = event_icon(kind);
    let seq = event.seq();
    let timestamp_micros = event.timestamp_micros();
    let timestamp = format_ts(timestamp_micros);

    let summary = match event {
        MailEvent::ToolCallStart {
            tool_name,
            project,
            agent,
            ..
        } => {
            let ctx = format_ctx(project.as_deref(), agent.as_deref());
            format!("→ {tool_name}{ctx}")
        }
        MailEvent::ToolCallEnd {
            tool_name,
            duration_ms,
            queries,
            project,
            agent,
            ..
        } => {
            let ctx = format_ctx(project.as_deref(), agent.as_deref());
            format!("{tool_name} {duration_ms}ms q={queries}{ctx}")
        }
        MailEvent::MessageSent {
            from,
            to,
            subject,
            id,
            ..
        } => {
            let recipients = if to.len() > 2 {
                format!("{}, {} +{}", to[0], to[1], to.len() - 2)
            } else {
                to.join(", ")
            };
            format!("#{id} {from} → {recipients}: {}", truncate(subject, 40))
        }
        MailEvent::MessageReceived {
            from, subject, id, ..
        } => {
            format!("#{id} from {from}: {}", truncate(subject, 40))
        }
        MailEvent::ReservationGranted {
            agent,
            paths,
            exclusive,
            ..
        } => {
            let ex = if *exclusive { " (excl)" } else { "" };
            let p = if paths.len() > 2 {
                format!("{} +{}", paths[0], paths.len() - 1)
            } else {
                paths.join(", ")
            };
            format!("{agent}: {p}{ex}")
        }
        MailEvent::ReservationReleased { agent, paths, .. } => {
            let p = if paths.len() > 2 {
                format!("{} +{}", paths[0], paths.len() - 1)
            } else {
                paths.join(", ")
            };
            format!("{agent}: released {p}")
        }
        MailEvent::AgentRegistered {
            name,
            program,
            model_name,
            project,
            ..
        } => {
            format!("{name} ({program}/{model_name}) in {project}")
        }
        MailEvent::HttpRequest {
            method,
            path,
            status,
            duration_ms,
            ..
        } => {
            format!("{method} {path} {status} {duration_ms}ms")
        }
        MailEvent::HealthPulse { db_stats, .. } => {
            format!(
                "p={} a={} m={}",
                db_stats.projects, db_stats.agents, db_stats.messages
            )
        }
        MailEvent::ServerStarted { endpoint, .. } => {
            format!("Server started at {endpoint}")
        }
        MailEvent::ServerShutdown { .. } => "Server shutting down".to_string(),
    };

    EventEntry {
        kind,
        severity: event.severity(),
        seq,
        timestamp_micros,
        timestamp,
        icon,
        summary,
    }
}

fn format_ctx(project: Option<&str>, agent: Option<&str>) -> String {
    match (project, agent) {
        (Some(p), Some(a)) => format!(" [{a}@{p}]"),
        (None, Some(a)) => format!(" [{a}]"),
        (Some(p), None) => format!(" [@{p}]"),
        (None, None) => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Find a valid UTF-8 char boundary at or before `max`.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn summarize_recipients(recipients: &[String]) -> String {
    match recipients {
        [] => "(none)".to_string(),
        [one] => one.clone(),
        [one, two] => format!("{one}, {two}"),
        [one, two, three] => format!("{one}, {two}, {three}"),
        [one, two, three, rest @ ..] => {
            format!("{one}, {two}, {three} +{}", rest.len())
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the summary band using `MetricTile` widgets.
///
/// Adapts tile count to terminal density: 3 tiles at Minimal/Compact, up to 6 at Detailed.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]
fn render_summary_band(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiSharedState,
    prev_stats: &DbStatSnapshot,
    density: DensityHint,
    pulse_phase: f32,
) {
    let counters = state.request_counters();
    let db = state.db_stats_snapshot().unwrap_or_default();
    let uptime_str = format_duration(state.uptime());
    let avg_ms = counters
        .latency_total_ms
        .checked_div(counters.total)
        .unwrap_or(0);
    let avg_str = format!("{avg_ms}ms");
    let msg_str = format!("{}", db.messages);
    let agent_str = format!("{}", db.agents);
    let ack_str = format!("{}", db.ack_pending);
    let req_str = format!("{}", counters.total);

    // Determine trend directions by comparing to previous snapshot.
    let msg_trend = trend_for(db.messages, prev_stats.messages);
    let agent_trend = trend_for(db.agents, prev_stats.agents);
    let ack_trend = match db.ack_pending.cmp(&prev_stats.ack_pending) {
        std::cmp::Ordering::Greater => MetricTrend::Up, // ack growing = bad
        std::cmp::Ordering::Less => MetricTrend::Down,
        std::cmp::Ordering::Equal => MetricTrend::Flat,
    };

    // Calculate pulse color for Requests
    let pulse = f32::midpoint(pulse_phase.sin(), 1.0); // 0.0 to 1.0
    let base_req = PackedRgba::rgb(120, 200, 255);
    let glow_req = PackedRgba::rgb(200, 240, 255);
    // Simple lerp
    let r = f32::from(base_req.r()).mul_add(1.0 - pulse, f32::from(glow_req.r()) * pulse) as u8;
    let g = f32::from(base_req.g()).mul_add(1.0 - pulse, f32::from(glow_req.g()) * pulse) as u8;
    let b = f32::from(base_req.b()).mul_add(1.0 - pulse, f32::from(glow_req.b()) * pulse) as u8;
    let req_color = PackedRgba::rgb(r, g, b);

    // Build tiles based on density.
    let tiles: Vec<(&str, &str, MetricTrend, PackedRgba)> = match density {
        DensityHint::Minimal | DensityHint::Compact => vec![
            (
                "Up",
                &uptime_str,
                MetricTrend::Flat,
                PackedRgba::rgb(200, 200, 200),
            ),
            ("Req", &req_str, MetricTrend::Flat, req_color),
            ("Msg", &msg_str, msg_trend, PackedRgba::rgb(200, 200, 120)),
        ],
        DensityHint::Normal => vec![
            (
                "Uptime",
                &uptime_str,
                MetricTrend::Flat,
                PackedRgba::rgb(200, 200, 200),
            ),
            ("Requests", &req_str, MetricTrend::Flat, req_color),
            (
                "Avg Lat",
                &avg_str,
                MetricTrend::Flat,
                PackedRgba::rgb(180, 140, 255),
            ),
            (
                "Messages",
                &msg_str,
                msg_trend,
                PackedRgba::rgb(200, 200, 120),
            ),
            (
                "Agents",
                &agent_str,
                agent_trend,
                PackedRgba::rgb(120, 220, 160),
            ),
        ],
        DensityHint::Detailed => vec![
            (
                "Uptime",
                &uptime_str,
                MetricTrend::Flat,
                PackedRgba::rgb(200, 200, 200),
            ),
            ("Requests", &req_str, MetricTrend::Flat, req_color),
            (
                "Avg Lat",
                &avg_str,
                MetricTrend::Flat,
                PackedRgba::rgb(180, 140, 255),
            ),
            (
                "Messages",
                &msg_str,
                msg_trend,
                PackedRgba::rgb(200, 200, 120),
            ),
            (
                "Agents",
                &agent_str,
                agent_trend,
                PackedRgba::rgb(120, 220, 160),
            ),
            (
                "Ack Pend",
                &ack_str,
                ack_trend,
                if db.ack_pending > 0 {
                    PackedRgba::rgb(255, 100, 100)
                } else {
                    PackedRgba::rgb(120, 200, 120)
                },
            ),
        ],
    };

    let tile_count = tiles.len();
    if tile_count == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let tile_w = area.width / tile_count as u16;

    for (i, (label, value, trend, color)) in tiles.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let x = area.x + (i as u16) * tile_w;
        let w = if i == tile_count - 1 {
            area.width.saturating_sub(x - area.x)
        } else {
            tile_w
        };
        let tile_area = Rect::new(x, area.y, w, area.height);
        let tile = MetricTile::new(label, value, *trend).value_color(*color);
        tile.render(tile_area, frame);
    }
}

/// Render the anomaly/action rail using `AnomalyCard` widgets.
fn render_anomaly_rail(frame: &mut Frame<'_>, area: Rect, anomalies: &[DetectedAnomaly]) {
    if anomalies.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }
    // Show up to 3 anomalies side by side.
    let visible = anomalies.len().min(3);
    #[allow(clippy::cast_possible_truncation)]
    let card_w = area.width / visible as u16;
    for (i, anomaly) in anomalies.iter().take(visible).enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let x = area.x + (i as u16) * card_w;
        let w = if i == visible - 1 {
            area.width.saturating_sub(x - area.x)
        } else {
            card_w
        };
        let card_area = Rect::new(x, area.y, w, area.height);
        let card = AnomalyCard::new(anomaly.severity, anomaly.confidence, &anomaly.headline)
            .rationale(&anomaly.rationale);
        card.render(card_area, frame);
    }
}

/// Render the trend/insight panel with percentile ribbon and throughput activity.
fn render_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    percentile_history: &[PercentileSample],
    throughput_history: &[f64],
) {
    if area.width < 10 || area.height < 6 {
        return;
    }
    // Split vertically: top half = percentile ribbon, bottom half = throughput
    let ribbon_h = area.height / 2;
    let activity_h = area.height.saturating_sub(ribbon_h);
    let ribbon_area = Rect::new(area.x, area.y, area.width, ribbon_h);
    let activity_area = Rect::new(area.x, area.y + ribbon_h, area.width, activity_h);

    // Percentile ribbon
    if percentile_history.len() >= 2 {
        let ribbon = PercentileRibbon::new(percentile_history)
            .label("Latency")
            .block(
                Block::default()
                    .title("Latency P50/P95/P99")
                    .border_type(BorderType::Rounded),
            );
        ribbon.render(ribbon_area, frame);
    } else {
        let block = Block::default()
            .title("Latency (collecting...)")
            .border_type(BorderType::Rounded);
        Paragraph::new("Awaiting data...")
            .block(block)
            .render(ribbon_area, frame);
    }

    // Throughput activity sparkline (br-2bbt.4.1: now using ftui_widgets::Sparkline)
    if throughput_history.len() >= 2 {
        let block = Block::default()
            .title("Req/interval")
            .border_type(BorderType::Rounded);
        let inner = block.inner(activity_area);
        block.render(activity_area, frame);

        // Label row.
        if inner.height > 1 && inner.width > 0 {
            let label =
                Paragraph::new("Throughput").style(Style::new().fg(PackedRgba::rgb(180, 180, 180)));
            label.render(
                Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: 1,
                },
                frame,
            );

            // Sparkline below label — take most recent values that fit.
            let spark_area = Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: inner.height.saturating_sub(1),
            };
            if spark_area.width > 0 && spark_area.height > 0 {
                let start_idx = throughput_history
                    .len()
                    .saturating_sub(spark_area.width as usize);
                let slice = &throughput_history[start_idx..];
                let sparkline = Sparkline::new(slice)
                    .min(0.0)
                    .style(Style::new().fg(PackedRgba::rgb(80, 200, 255)));
                sparkline.render(spark_area, frame);
            }
        }
    } else {
        let block = Block::default()
            .title("Throughput (collecting...)")
            .border_type(BorderType::Rounded);
        Paragraph::new("Awaiting data...")
            .block(block)
            .render(activity_area, frame);
    }
}

/// Render the dashboard's recent-message markdown preview rail.
fn render_recent_message_preview_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    preview: Option<&RecentMessagePreview>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let block = Block::default()
        .title("Recent Message Preview")
        .border_type(BorderType::Rounded);
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let text = preview.map_or_else(
        || Text::from("No message traffic yet. Recent sent/received metadata appears here."),
        |preview| {
            let theme = MarkdownTheme::default();
            crate::tui_markdown::render_body(&preview.to_markdown(), &theme)
        },
    );

    Paragraph::new(text).render(inner, frame);
}

/// Derive a `MetricTrend` from two consecutive values.
const fn trend_for(current: u64, previous: u64) -> MetricTrend {
    if current > previous {
        MetricTrend::Up
    } else if current < previous {
        MetricTrend::Down
    } else {
        MetricTrend::Flat
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pulsing_severity_badge(
    severity: EventSeverity,
    pulse_phase: f32,
    reduced_motion: bool,
) -> Span<'static> {
    if reduced_motion || !matches!(severity, EventSeverity::Warn | EventSeverity::Error) {
        return severity.styled_badge();
    }

    let pulse = pulse_phase.sin().abs();
    let (base, highlight) = match severity {
        EventSeverity::Warn => ((176_u8, 122_u8, 32_u8), (255_u8, 198_u8, 90_u8)),
        EventSeverity::Error => ((148_u8, 54_u8, 54_u8), (255_u8, 96_u8, 96_u8)),
        _ => return severity.styled_badge(),
    };
    let lerp = |start: u8, end: u8| -> u8 {
        let start = f32::from(start);
        let end = f32::from(end);
        ((end - start).mul_add(pulse, start)).round() as u8
    };
    let color = PackedRgba::rgb(
        lerp(base.0, highlight.0),
        lerp(base.1, highlight.1),
        lerp(base.2, highlight.2),
    );
    Span::styled(
        severity.badge().to_string(),
        Style::default().fg(color).bold(),
    )
}

/// Render the scrollable event log.
#[allow(clippy::too_many_arguments)]
fn render_event_log(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[&EventEntry],
    scroll_offset: usize,
    auto_follow: bool,
    type_filter: &HashSet<MailEventKind>,
    verbosity: VerbosityTier,
    pulse_phase: f32,
    reduced_motion: bool,
) {
    let visible_height = area.height.saturating_sub(2) as usize; // -2 for border
    if visible_height == 0 {
        return;
    }

    // Compute viewport slice
    let total = entries.len();
    let start = if total <= visible_height {
        0
    } else if auto_follow || scroll_offset == 0 {
        total - visible_height
    } else {
        total.saturating_sub(visible_height + scroll_offset)
    };
    let end = (start + visible_height).min(total);
    let viewport = &entries[start..end];

    // Focused entry is the one at the bottom of the viewport (most recent in view).
    let focused_abs_idx = if total == 0 {
        None
    } else {
        Some(total.saturating_sub(1 + scroll_offset))
    };
    let focus_style = Style::default().reverse().bold();

    // Build styled text lines with colored severity badges
    let mut text_lines: Vec<Line> = Vec::with_capacity(viewport.len());
    for (view_idx, entry) in viewport.iter().enumerate() {
        let abs_idx = start + view_idx;
        let sev = entry.severity;
        let mut line = Line::from_spans([
            Span::raw(format!("{:>6} {} ", entry.seq, entry.timestamp)),
            pulsing_severity_badge(sev, pulse_phase, reduced_motion),
            Span::raw(" "),
            Span::styled(format!("{}", entry.icon), sev.style()),
            Span::raw(format!(" {}", entry.summary)),
        ]);
        if Some(abs_idx) == focused_abs_idx {
            line.apply_base_style(focus_style);
        }
        text_lines.push(line);
    }
    let text = Text::from_lines(text_lines);

    let follow_indicator = if auto_follow { " [FOLLOW]" } else { "" };
    let verbosity_indicator = format!(" [{}]", verbosity.label());
    let filter_indicator = if type_filter.is_empty() {
        String::new()
    } else {
        format!(
            " [filter: {}]",
            type_filter
                .iter()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let title =
        format!("Events ({end}/{total}){follow_indicator}{verbosity_indicator}{filter_indicator}",);

    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded);
    let p = Paragraph::new(text).block(block);
    p.render(area, frame);
}

/// Render the footer stats bar.
fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
    let counters = state.request_counters();
    let ring_stats = state.event_ring_stats();

    let avg_ms = counters
        .latency_total_ms
        .checked_div(counters.total)
        .unwrap_or(0);

    let total_drops = ring_stats.total_drops();
    let drop_detail = if total_drops == 0 {
        "Drops:0".to_string()
    } else {
        format!(
            "Drops:{} (ovf:{} ctn:{} smp:{})",
            total_drops,
            ring_stats.dropped_overflow,
            ring_stats.contention_drops,
            ring_stats.sampled_drops,
        )
    };
    let fill = ring_stats.fill_pct();
    let bp_indicator = if fill >= 80 { " [BP]" } else { "" };
    let footer = format!(
        " Req:{} Avg:{}ms 2xx:{} 4xx:{} 5xx:{} | Events:{}/{} ({}%) {} {}",
        counters.total,
        avg_ms,
        counters.status_2xx,
        counters.status_4xx,
        counters.status_5xx,
        ring_stats.len,
        ring_stats.capacity,
        fill,
        drop_detail,
        bp_indicator,
    );

    let p = Paragraph::new(footer);
    p.render(area, frame);
}

/// Format a Duration as human-readable (e.g. "2h 15m" or "45s").
fn format_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs >= 3600 {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        format!("{h}h {m}m")
    } else if total_secs >= 60 {
        let m = total_secs / 60;
        let s = total_secs % 60;
        format!("{m}m {s}s")
    } else {
        format!("{total_secs}s")
    }
}

/// Render a sparkline from data points using Unicode block chars.
///
/// (br-2bbt.4.1: Now delegates to `ftui_widgets::Sparkline::render_to_string()`.)
#[must_use]
pub fn render_sparkline(data: &[f64], width: usize) -> String {
    if data.is_empty() || width == 0 {
        return String::new();
    }

    // Take the last `width` samples
    let start = data.len().saturating_sub(width);
    let slice = &data[start..];

    // Use Sparkline widget's render_to_string for consistent block-char mapping.
    Sparkline::new(slice).min(0.0).render_to_string()
}

// ──────────────────────────────────────────────────────────────────────
// Activity indicators
// ──────────────────────────────────────────────────────────────────────

/// Thresholds for agent activity status (in microseconds, used in tests).
#[cfg(test)]
const ACTIVE_THRESHOLD_US: i64 = 60 * 1_000_000; // 60 seconds
#[cfg(test)]
const IDLE_THRESHOLD_US: i64 = 5 * 60 * 1_000_000; // 5 minutes

/// Activity dot colors (used in tests).
#[cfg(test)]
const ACTIVITY_GREEN: PackedRgba = PackedRgba::rgb(80, 220, 100);
#[cfg(test)]
const ACTIVITY_YELLOW: PackedRgba = PackedRgba::rgb(220, 200, 60);
#[cfg(test)]
const ACTIVITY_GRAY: PackedRgba = PackedRgba::rgb(120, 120, 120);

/// Returns an activity dot character and color based on how recently an agent
/// was active. Green = active (<60s), yellow = idle (<5m), gray = stale.
#[cfg(test)]
const fn activity_indicator(now_us: i64, last_active_us: i64) -> (char, PackedRgba) {
    if last_active_us == 0 {
        return ('○', ACTIVITY_GRAY);
    }
    let age = now_us.saturating_sub(last_active_us);
    if age < ACTIVE_THRESHOLD_US {
        ('●', ACTIVITY_GREEN)
    } else if age < IDLE_THRESHOLD_US {
        ('●', ACTIVITY_YELLOW)
    } else {
        ('○', ACTIVITY_GRAY)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rects_overlap(left: Rect, right: Rect) -> bool {
        let left_right = left.x.saturating_add(left.width);
        let right_right = right.x.saturating_add(right.width);
        let left_bottom = left.y.saturating_add(left.height);
        let right_bottom = right.y.saturating_add(right.height);
        left.x < right_right
            && right.x < left_right
            && left.y < right_bottom
            && right.y < left_bottom
    }

    #[test]
    fn format_ts_renders_hms_millis() {
        // 13:45:23.456
        let micros: i64 = (13 * 3600 + 45 * 60 + 23) * 1_000_000 + 456_000;
        assert_eq!(format_ts(micros), "13:45:23.456");
    }

    #[test]
    fn format_ts_wraps_at_24h() {
        let micros: i64 = 25 * 3600 * 1_000_000; // 25 hours
        assert_eq!(format_ts(micros), "01:00:00.000");
    }

    #[test]
    fn format_event_tool_call_end() {
        let event = MailEvent::tool_call_end(
            "send_message",
            42,
            Some("ok".to_string()),
            5,
            1.2,
            vec![("messages".to_string(), 3)],
            Some("my-proj".to_string()),
            Some("RedFox".to_string()),
        );
        let entry = format_event(&event);
        assert_eq!(entry.kind, MailEventKind::ToolCallEnd);
        assert!(entry.summary.contains("send_message"));
        assert!(entry.summary.contains("42ms"));
        assert!(entry.summary.contains("q=5"));
        assert!(entry.summary.contains("[RedFox@my-proj]"));
    }

    #[test]
    fn format_event_message_sent() {
        let event = MailEvent::message_sent(
            1,
            "GoldFox",
            vec!["SilverWolf".to_string()],
            "Hello world",
            "thread-1",
            "test-project",
        );
        let entry = format_event(&event);
        assert!(entry.summary.contains("GoldFox"));
        assert!(entry.summary.contains("SilverWolf"));
        assert!(entry.summary.contains("Hello world"));
    }

    #[test]
    fn format_event_http_request() {
        let event = MailEvent::http_request("POST", "/mcp/", 200, 5, "127.0.0.1");
        let entry = format_event(&event);
        assert!(entry.summary.contains("POST"));
        assert!(entry.summary.contains("/mcp/"));
        assert!(entry.summary.contains("200"));
        assert!(entry.summary.contains("5ms"));
    }

    #[test]
    fn format_event_server_started() {
        let event = MailEvent::server_started("http://localhost:8765", "tui=on");
        let entry = format_event(&event);
        assert!(entry.summary.contains("localhost:8765"));
    }

    #[test]
    fn format_event_server_shutdown() {
        let event = MailEvent::server_shutdown();
        let entry = format_event(&event);
        assert!(entry.summary.contains("shutting down"));
    }

    #[test]
    fn format_event_reservation_granted() {
        let event = MailEvent::reservation_granted(
            "BlueFox",
            vec!["src/**".to_string(), "tests/**".to_string()],
            true,
            3600,
            "proj",
        );
        let entry = format_event(&event);
        assert!(entry.summary.contains("BlueFox"));
        assert!(entry.summary.contains("src/**"));
        assert!(entry.summary.contains("(excl)"));
    }

    #[test]
    fn format_event_agent_registered() {
        let event = MailEvent::agent_registered("RedFox", "claude-code", "opus-4.6", "my-proj");
        let entry = format_event(&event);
        assert!(entry.summary.contains("RedFox"));
        assert!(entry.summary.contains("claude-code"));
        assert!(entry.summary.contains("opus-4.6"));
    }

    #[test]
    fn format_ctx_combinations() {
        assert_eq!(format_ctx(Some("p"), Some("a")), " [a@p]");
        assert_eq!(format_ctx(None, Some("a")), " [a]");
        assert_eq!(format_ctx(Some("p"), None), " [@p]");
        assert_eq!(format_ctx(None, None), "");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world!", 5), "hello");
    }

    #[test]
    fn truncate_multibyte_utf8() {
        // "café" — 'é' is 2 bytes (0xC3 0xA9); byte offsets: c=0, a=1, f=2, é=3..4
        assert_eq!(truncate("café", 4), "caf"); // byte 4 is mid-'é', backs up to 3
        assert_eq!(truncate("café", 5), "café"); // all 5 bytes fit
        // Emoji: '🎉' is 4 bytes; "hi🎉bye" = h(0) i(1) 🎉(2..5) b(6) y(7) e(8)
        assert_eq!(truncate("hi🎉bye", 3), "hi"); // byte 3 mid-emoji, backs up to 2
        assert_eq!(truncate("hi🎉bye", 6), "hi🎉"); // byte 6 = start of 'b'
    }

    #[test]
    fn summarize_recipients_formats_by_count() {
        assert_eq!(summarize_recipients(&[]), "(none)");
        assert_eq!(summarize_recipients(&["A".to_string()]), "A");
        assert_eq!(
            summarize_recipients(&["A".to_string(), "B".to_string()]),
            "A, B"
        );
        assert_eq!(
            summarize_recipients(&["A".to_string(), "B".to_string(), "C".to_string()]),
            "A, B, C"
        );
        assert_eq!(
            summarize_recipients(&[
                "A".to_string(),
                "B".to_string(),
                "C".to_string(),
                "D".to_string(),
            ]),
            "A, B, C +1"
        );
    }

    #[test]
    fn ingest_events_tracks_most_recent_message_preview() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        let _ = state.push_event(MailEvent::message_sent(
            1,
            "GoldFox",
            vec!["SilverWolf".to_string(), "RedPine".to_string()],
            "Initial update",
            "br-3vwi.6.5",
            "test-project",
        ));
        screen.ingest_events(&state);
        let first = screen
            .recent_message_preview
            .as_ref()
            .expect("expected outbound preview after message_sent");
        assert_eq!(first.direction, "Outbound");
        assert_eq!(first.from, "GoldFox");
        assert_eq!(first.to, "SilverWolf, RedPine");
        assert_eq!(first.thread_id, "br-3vwi.6.5");

        let _ = state.push_event(MailEvent::message_received(
            2,
            "TealBasin",
            vec!["GoldFox".to_string()],
            "Ack received",
            "br-3vwi.6.5",
            "test-project",
        ));
        screen.ingest_events(&state);
        let second = screen
            .recent_message_preview
            .as_ref()
            .expect("expected inbound preview after message_received");
        assert_eq!(second.direction, "Inbound");
        assert_eq!(second.from, "TealBasin");
        assert_eq!(second.to, "GoldFox");
        assert_eq!(second.subject, "Ack received");
    }

    #[test]
    fn recent_message_preview_markdown_contains_key_metadata() {
        let preview = RecentMessagePreview {
            direction: "Outbound",
            timestamp: "12:34:56.789".to_string(),
            from: "FrostyLantern".to_string(),
            to: "TealBasin, CalmCrane".to_string(),
            subject: "Status update".to_string(),
            thread_id: "br-3vwi.6.5".to_string(),
            project: "data-projects-mcp-agent-mail-rust".to_string(),
        };

        let md = preview.to_markdown();
        assert!(md.contains("Outbound Message"));
        assert!(md.contains("Status update"));
        assert!(md.contains("FrostyLantern"));
        assert!(md.contains("TealBasin, CalmCrane"));
        assert!(md.contains("br-3vwi.6.5"));
        assert!(md.contains("data-projects-mcp-agent-mail-rust"));
    }

    #[test]
    fn panel_budget_heights_match_terminal_classes() {
        assert_eq!(summary_band_height(TerminalClass::Tiny), 1);
        assert_eq!(summary_band_height(TerminalClass::Compact), 3);
        assert_eq!(summary_band_height(TerminalClass::Normal), 3);
        assert_eq!(summary_band_height(TerminalClass::Wide), 3);
        assert_eq!(summary_band_height(TerminalClass::UltraWide), 3);

        assert_eq!(anomaly_rail_height(TerminalClass::Tiny, 2), 0);
        assert_eq!(anomaly_rail_height(TerminalClass::Compact, 2), 0);
        assert_eq!(anomaly_rail_height(TerminalClass::Normal, 2), 4);
        assert_eq!(anomaly_rail_height(TerminalClass::Wide, 2), 4);
        assert_eq!(anomaly_rail_height(TerminalClass::UltraWide, 2), 4);

        assert_eq!(footer_bar_height(TerminalClass::Tiny), 0);
        assert_eq!(footer_bar_height(TerminalClass::Compact), 1);
        assert_eq!(footer_bar_height(TerminalClass::Normal), 1);
        assert_eq!(footer_bar_height(TerminalClass::Wide), 1);
        assert_eq!(footer_bar_height(TerminalClass::UltraWide), 1);
    }

    #[test]
    fn main_layout_ultrawide_exposes_double_surface_vs_standard() {
        let standard = DashboardScreen::main_content_layout(true).compute(Rect::new(0, 0, 100, 30));
        let ultra = DashboardScreen::main_content_layout(true).compute(Rect::new(0, 0, 200, 50));

        let standard_visible = standard
            .panels
            .iter()
            .filter(|p| p.visibility != crate::tui_layout::PanelVisibility::Hidden)
            .count();
        let ultra_visible = ultra
            .panels
            .iter()
            .filter(|p| p.visibility != crate::tui_layout::PanelVisibility::Hidden)
            .count();

        assert!(
            ultra_visible >= standard_visible.saturating_mul(2),
            "expected ultrawide to expose at least 2x panel surface: standard={standard_visible}, ultrawide={ultra_visible}"
        );
        assert!(standard.rect(PanelSlot::Inspector).is_none());
        assert!(standard.rect(PanelSlot::Footer).is_none());
        assert!(ultra.rect(PanelSlot::Inspector).is_some());
        assert!(ultra.rect(PanelSlot::Footer).is_some());
    }

    #[test]
    fn main_layout_ultrawide_panels_fit_bounds_without_overlap() {
        let area = Rect::new(0, 0, 200, 50);
        let composition = DashboardScreen::main_content_layout(true).compute(area);
        let visible_rects: Vec<Rect> = [
            composition.rect(PanelSlot::Primary),
            composition.rect(PanelSlot::Inspector),
            composition.rect(PanelSlot::Footer),
        ]
        .into_iter()
        .flatten()
        .collect();

        assert!(
            visible_rects.len() >= 3,
            "expected primary + trend + preview panels in ultrawide layout"
        );

        for rect in &visible_rects {
            let right = rect.x.saturating_add(rect.width);
            let bottom = rect.y.saturating_add(rect.height);
            assert!(rect.x >= area.x);
            assert!(rect.y >= area.y);
            assert!(right <= area.x.saturating_add(area.width));
            assert!(bottom <= area.y.saturating_add(area.height));
        }

        for (index, left) in visible_rects.iter().enumerate() {
            for right in visible_rects.iter().skip(index + 1) {
                assert!(
                    !rects_overlap(*left, *right),
                    "panel rects overlap in ultrawide layout: left={left:?} right={right:?}"
                );
            }
        }
    }

    #[test]
    fn main_layout_hides_trend_panel_when_disabled() {
        let composition =
            DashboardScreen::main_content_layout(false).compute(Rect::new(0, 0, 200, 50));
        assert!(composition.rect(PanelSlot::Inspector).is_none());
        assert!(composition.rect(PanelSlot::Footer).is_some());
    }

    #[test]
    fn render_sparkline_basic() {
        let data = vec![1.0, 2.0, 3.0, 4.0];
        let spark = render_sparkline(&data, 4);
        assert_eq!(spark.chars().count(), 4);
        // Last value (4.0) should be the tallest
        assert_eq!(spark.chars().last(), Some('█'));
    }

    #[test]
    fn render_sparkline_empty() {
        assert_eq!(render_sparkline(&[], 10), "");
        assert_eq!(render_sparkline(&[1.0], 0), "");
    }

    #[test]
    fn render_sparkline_all_zeros() {
        // ftui_widgets::Sparkline renders constant values as middle-height bars (▄)
        // since there's no variation to show relative height differences.
        let data = vec![0.0, 0.0, 0.0];
        let spark = render_sparkline(&data, 3);
        assert_eq!(spark, "▄▄▄");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(7380)),
            "2h 3m"
        );
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(125)),
            "2m 5s"
        );
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(std::time::Duration::from_secs(45)), "45s");
    }

    #[test]
    fn dashboard_screen_renders_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let screen = DashboardScreen::new();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(120, 30, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 120, 30), &state);
    }

    #[test]
    fn dashboard_screen_renders_at_minimum_size() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let screen = DashboardScreen::new();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 24, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 80, 24), &state);
    }

    #[test]
    fn dashboard_screen_renders_at_large_size() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let screen = DashboardScreen::new();

        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(200, 50, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 200, 50), &state);
    }

    #[test]
    fn dashboard_ingest_events() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        // Push some events
        let _ = state.push_event(MailEvent::server_started("http://test", "test"));
        let _ = state.push_event(MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1"));

        screen.ingest_events(&state);
        assert_eq!(screen.event_log.len(), 2);
    }

    #[test]
    fn dashboard_health_pulse_hidden_by_default_verbosity() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        let _ = state.push_event(MailEvent::health_pulse(DbStatSnapshot::default()));
        screen.ingest_events(&state);
        // Health pulses are ingested but hidden by Standard verbosity (Trace level)
        assert_eq!(screen.event_log.len(), 1, "event should be stored");
        assert_eq!(
            screen.visible_entries().len(),
            0,
            "health pulses hidden at Standard verbosity"
        );

        // Switching to All makes them visible
        screen.verbosity = VerbosityTier::All;
        assert_eq!(
            screen.visible_entries().len(),
            1,
            "health pulses visible at All verbosity"
        );
    }

    #[test]
    fn dashboard_type_filter_works() {
        let mut screen = DashboardScreen::new();
        // Set verbosity to All so type filter is the only variable
        screen.verbosity = VerbosityTier::All;
        screen.event_log.push(EventEntry {
            kind: MailEventKind::HttpRequest,
            severity: EventSeverity::Debug,
            seq: 1,
            timestamp_micros: 0,
            timestamp: "00:00:00.000".to_string(),
            icon: '↔',
            summary: "GET /".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
            seq: 2,
            timestamp_micros: 1_000,
            timestamp: "00:00:00.001".to_string(),
            icon: '⚙',
            summary: "send_message 5ms".to_string(),
        });

        // No filter: both visible
        assert_eq!(screen.visible_entries().len(), 2);

        // Filter to ToolCallEnd only
        screen.type_filter.insert(MailEventKind::ToolCallEnd);
        assert_eq!(screen.visible_entries().len(), 1);
        assert_eq!(screen.visible_entries()[0].kind, MailEventKind::ToolCallEnd);
    }

    #[test]
    fn dashboard_keybindings_are_documented() {
        let screen = DashboardScreen::new();
        let bindings = screen.keybindings();
        assert!(bindings.len() >= 4);
        assert!(bindings.iter().any(|b| b.key == "j/k"));
        assert!(bindings.iter().any(|b| b.key == "Enter"));
        assert!(bindings.iter().any(|b| b.key == "f"));
        assert!(bindings.iter().any(|b| b.key == "v"));
        assert!(bindings.iter().any(|b| b.key == "t"));
    }

    #[test]
    fn enter_deep_links_to_timeline_at_focused_event() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.verbosity = VerbosityTier::All;

        screen.event_log.push(EventEntry {
            kind: MailEventKind::HttpRequest,
            severity: EventSeverity::Debug,
            seq: 1,
            timestamp_micros: 111,
            timestamp: "00:00:00.000".to_string(),
            icon: '↔',
            summary: "GET /".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
            seq: 2,
            timestamp_micros: 222,
            timestamp: "00:00:00.001".to_string(),
            icon: '⚙',
            summary: "tool".to_string(),
        });

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let cmd = screen.update(&enter, &state);
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::TimelineAtTime(222)))
        ));

        // Scroll up one row (focus moves to older entry).
        screen.auto_follow = false;
        screen.scroll_offset = 1;
        let cmd2 = screen.update(&enter, &state);
        assert!(matches!(
            cmd2,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::TimelineAtTime(111)))
        ));
    }

    #[test]
    fn enter_on_empty_dashboard_is_noop() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let cmd = screen.update(&enter, &state);
        assert!(matches!(cmd, Cmd::None));
    }

    #[test]
    fn verbosity_tiers_filter_correctly() {
        let mut screen = DashboardScreen::new();
        // Add events at different severities
        screen.event_log.push(EventEntry {
            kind: MailEventKind::HealthPulse,
            severity: EventSeverity::Trace,
            seq: 1,
            timestamp_micros: 0,
            timestamp: "00:00:00.000".to_string(),
            icon: '♥',
            summary: "pulse".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
            seq: 2,
            timestamp_micros: 1_000,
            timestamp: "00:00:00.001".to_string(),
            icon: '⚙',
            summary: "tool done".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::MessageSent,
            severity: EventSeverity::Info,
            seq: 3,
            timestamp_micros: 2_000,
            timestamp: "00:00:00.002".to_string(),
            icon: '✉',
            summary: "msg sent".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ServerShutdown,
            severity: EventSeverity::Warn,
            seq: 4,
            timestamp_micros: 3_000,
            timestamp: "00:00:00.003".to_string(),
            icon: '⏹',
            summary: "shutdown".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::HttpRequest,
            severity: EventSeverity::Error,
            seq: 5,
            timestamp_micros: 4_000,
            timestamp: "00:00:00.004".to_string(),
            icon: '↔',
            summary: "500 error".to_string(),
        });

        // Minimal: Warn + Error only
        screen.verbosity = VerbosityTier::Minimal;
        assert_eq!(screen.visible_entries().len(), 2);

        // Standard: Info + Warn + Error
        screen.verbosity = VerbosityTier::Standard;
        assert_eq!(screen.visible_entries().len(), 3);

        // Verbose: Debug + Info + Warn + Error
        screen.verbosity = VerbosityTier::Verbose;
        assert_eq!(screen.visible_entries().len(), 4);

        // All: everything
        screen.verbosity = VerbosityTier::All;
        assert_eq!(screen.visible_entries().len(), 5);
    }

    #[test]
    fn verbosity_cycles_on_v_key() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        assert_eq!(screen.verbosity, VerbosityTier::Standard);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('v')));
        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::Verbose);

        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::All);

        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::Minimal);

        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::Standard);
    }

    #[test]
    fn severity_badge_in_format_output() {
        let event = MailEvent::server_started("http://test", "test");
        let entry = format_event(&event);
        assert_eq!(entry.severity, EventSeverity::Info);
        assert_eq!(entry.severity.badge(), "INF");
    }

    #[test]
    fn pulsing_badge_falls_back_when_reduced_motion() {
        let static_badge =
            pulsing_severity_badge(EventSeverity::Error, std::f32::consts::FRAC_PI_2, true);
        assert_eq!(static_badge, EventSeverity::Error.styled_badge());
    }

    #[test]
    fn pulsing_badge_differs_for_urgent_severity_when_enabled() {
        let pulsed =
            pulsing_severity_badge(EventSeverity::Warn, std::f32::consts::FRAC_PI_2, false);
        assert_ne!(pulsed, EventSeverity::Warn.styled_badge());
    }

    #[test]
    fn verbosity_and_type_filter_combine() {
        let mut screen = DashboardScreen::new();
        // Add an Info-level message and a Debug-level tool end
        screen.event_log.push(EventEntry {
            kind: MailEventKind::MessageSent,
            severity: EventSeverity::Info,
            seq: 1,
            timestamp_micros: 0,
            timestamp: "00:00:00.000".to_string(),
            icon: '✉',
            summary: "msg".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
            seq: 2,
            timestamp_micros: 1_000,
            timestamp: "00:00:00.001".to_string(),
            icon: '⚙',
            summary: "tool".to_string(),
        });

        // Standard verbosity hides Debug, so only Info visible
        screen.verbosity = VerbosityTier::Standard;
        assert_eq!(screen.visible_entries().len(), 1);

        // Now add type filter for ToolCallEnd only + Verbose verbosity
        screen.verbosity = VerbosityTier::Verbose;
        screen.type_filter.insert(MailEventKind::ToolCallEnd);
        assert_eq!(screen.visible_entries().len(), 1);
        assert_eq!(screen.visible_entries()[0].kind, MailEventKind::ToolCallEnd);
    }

    #[test]
    fn event_icon_coverage() {
        // Ensure all event kinds have icons
        let kinds = [
            MailEventKind::ToolCallStart,
            MailEventKind::ToolCallEnd,
            MailEventKind::MessageSent,
            MailEventKind::MessageReceived,
            MailEventKind::ReservationGranted,
            MailEventKind::ReservationReleased,
            MailEventKind::AgentRegistered,
            MailEventKind::HttpRequest,
            MailEventKind::HealthPulse,
            MailEventKind::ServerStarted,
            MailEventKind::ServerShutdown,
        ];
        for kind in kinds {
            let icon = event_icon(kind);
            assert_ne!(icon, '\0');
        }
    }

    // ── Dashboard state-machine edge cases ───────────────────────

    #[test]
    fn scroll_up_disables_auto_follow() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        assert!(screen.auto_follow);

        let up = Event::Key(ftui::KeyEvent::new(KeyCode::Char('k')));
        screen.update(&up, &state);
        assert!(!screen.auto_follow);
        assert_eq!(screen.scroll_offset, 1);
    }

    #[test]
    fn scroll_down_to_bottom_re_enables_follow() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.auto_follow = false;
        screen.scroll_offset = 1;

        let down = Event::Key(ftui::KeyEvent::new(KeyCode::Char('j')));
        screen.update(&down, &state);
        assert_eq!(screen.scroll_offset, 0);
        assert!(screen.auto_follow);
    }

    #[test]
    fn g_jumps_to_top() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.verbosity = VerbosityTier::All;

        // Add some events
        for _ in 0..20 {
            screen.event_log.push(EventEntry {
                kind: MailEventKind::HttpRequest,
                severity: EventSeverity::Debug,
                seq: 0,
                timestamp_micros: 0,
                timestamp: "00:00:00.000".to_string(),
                icon: '↔',
                summary: "GET /".to_string(),
            });
        }

        let g = Event::Key(ftui::KeyEvent::new(KeyCode::Char('g')));
        screen.update(&g, &state);
        assert!(!screen.auto_follow);
        assert!(screen.scroll_offset > 0);
    }

    #[test]
    fn g_upper_jumps_to_bottom() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.auto_follow = false;
        screen.scroll_offset = 10;

        let g = Event::Key(ftui::KeyEvent::new(KeyCode::Char('G')));
        screen.update(&g, &state);
        assert!(screen.auto_follow);
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn f_key_toggles_follow() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        assert!(screen.auto_follow);

        let f = Event::Key(ftui::KeyEvent::new(KeyCode::Char('f')));
        screen.update(&f, &state);
        assert!(!screen.auto_follow);

        screen.update(&f, &state);
        assert!(screen.auto_follow);
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn type_filter_cycles_through_states() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        let t = Event::Key(ftui::KeyEvent::new(KeyCode::Char('t')));

        // empty -> ToolCallEnd
        screen.update(&t, &state);
        assert!(screen.type_filter.contains(&MailEventKind::ToolCallEnd));

        // ToolCallEnd -> MessageSent
        screen.update(&t, &state);
        assert!(screen.type_filter.contains(&MailEventKind::MessageSent));

        // MessageSent -> HttpRequest
        screen.update(&t, &state);
        assert!(screen.type_filter.contains(&MailEventKind::HttpRequest));

        // HttpRequest -> clear
        screen.update(&t, &state);
        assert!(screen.type_filter.is_empty());
    }

    #[test]
    fn ingest_events_trims_to_capacity() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        // Push more than EVENT_LOG_CAPACITY events
        for i in 0..(EVENT_LOG_CAPACITY + 500) {
            let _ = state.push_event(MailEvent::http_request(
                "GET",
                format!("/{i}"),
                200,
                1,
                "127.0.0.1",
            ));
        }
        screen.ingest_events(&state);
        assert!(screen.event_log.len() <= EVENT_LOG_CAPACITY);
    }

    #[test]
    fn format_event_message_with_many_recipients() {
        let event = MailEvent::message_sent(
            1,
            "GoldFox",
            vec![
                "SilverWolf".to_string(),
                "BluePeak".to_string(),
                "RedLake".to_string(),
            ],
            "Hello",
            "t",
            "p",
        );
        let entry = format_event(&event);
        // 3 recipients -> should use "+N" format
        assert!(entry.summary.contains("+1"));
    }

    #[test]
    fn format_event_reservation_with_many_paths() {
        let event = MailEvent::reservation_granted(
            "BlueFox",
            vec![
                "src/**".to_string(),
                "tests/**".to_string(),
                "docs/**".to_string(),
            ],
            false,
            3600,
            "proj",
        );
        let entry = format_event(&event);
        assert!(entry.summary.contains("+2"));
        assert!(!entry.summary.contains("(excl)"));
    }

    #[test]
    fn format_event_reservation_released_with_many_paths() {
        let event = MailEvent::reservation_released(
            "BlueFox",
            vec!["a/**".to_string(), "b/**".to_string(), "c/**".to_string()],
            "proj",
        );
        let entry = format_event(&event);
        assert!(entry.summary.contains("released"));
        assert!(entry.summary.contains("+2"));
    }

    #[test]
    fn format_event_health_pulse() {
        let event = MailEvent::health_pulse(DbStatSnapshot {
            projects: 3,
            agents: 7,
            messages: 42,
            ..Default::default()
        });
        let entry = format_event(&event);
        assert!(entry.summary.contains("p=3"));
        assert!(entry.summary.contains("a=7"));
        assert!(entry.summary.contains("m=42"));
    }

    #[test]
    fn format_event_message_received() {
        let event = MailEvent::message_received(
            99,
            "SilverWolf",
            vec!["GoldFox".to_string()],
            "Status update",
            "thread-1",
            "proj",
        );
        let entry = format_event(&event);
        assert!(entry.summary.contains("#99"));
        assert!(entry.summary.contains("SilverWolf"));
        assert!(entry.summary.contains("Status update"));
    }

    #[test]
    fn format_event_tool_call_start() {
        let event = MailEvent::tool_call_start(
            "fetch_inbox",
            serde_json::Value::Null,
            Some("p".into()),
            Some("A".into()),
        );
        let entry = format_event(&event);
        assert!(entry.summary.contains("→ fetch_inbox"));
        assert!(entry.summary.contains("[A@p]"));
    }

    #[test]
    fn render_sparkline_width_larger_than_data() {
        let data = vec![1.0, 4.0];
        let spark = render_sparkline(&data, 10);
        // Should only produce chars for available data points (2)
        assert_eq!(spark.chars().count(), 2);
    }

    #[test]
    fn render_sparkline_single_value() {
        let data = vec![5.0];
        let spark = render_sparkline(&data, 5);
        assert_eq!(spark.chars().count(), 1);
        assert_eq!(spark.chars().next(), Some('█'));
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(std::time::Duration::from_secs(0)), "0s");
    }

    #[test]
    fn dashboard_title_and_label() {
        let screen = DashboardScreen::new();
        assert_eq!(screen.title(), "Dashboard");
        assert_eq!(screen.tab_label(), "Dash");
    }

    #[test]
    fn dashboard_default_impl() {
        let screen = DashboardScreen::default();
        assert!(screen.event_log.is_empty());
        assert!(screen.auto_follow);
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn dashboard_renders_at_zero_height_without_panic() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let screen = DashboardScreen::new();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 1, &mut pool);
        screen.view(&mut frame, Rect::new(0, 0, 80, 1), &state);
    }

    // ── Activity indicator tests ──────────────────────────────────

    #[test]
    fn activity_indicator_active() {
        let now = 1_000_000_000_i64; // 1 second in micros
        let recent = now - 30_000_000; // 30 seconds ago
        let (dot, color) = activity_indicator(now, recent);
        assert_eq!(dot, '●');
        assert_eq!(color, ACTIVITY_GREEN);
    }

    #[test]
    fn activity_indicator_idle() {
        let now = 1_000_000_000_i64;
        let idle = now - 120_000_000; // 2 minutes ago
        let (dot, color) = activity_indicator(now, idle);
        assert_eq!(dot, '●');
        assert_eq!(color, ACTIVITY_YELLOW);
    }

    #[test]
    fn activity_indicator_stale() {
        let now = 1_000_000_000_i64;
        let stale = now - 600_000_000; // 10 minutes ago
        let (dot, color) = activity_indicator(now, stale);
        assert_eq!(dot, '○');
        assert_eq!(color, ACTIVITY_GRAY);
    }

    #[test]
    fn activity_indicator_zero_ts_is_gray() {
        let (dot, color) = activity_indicator(1_000_000_000, 0);
        assert_eq!(dot, '○');
        assert_eq!(color, ACTIVITY_GRAY);
    }

    #[test]
    fn activity_indicator_boundary_at_60s() {
        let now = 1_000_000_000_i64;
        // Exactly at boundary: 60s ago
        let at_boundary = now - ACTIVE_THRESHOLD_US;
        let (_, color) = activity_indicator(now, at_boundary);
        assert_eq!(color, ACTIVITY_YELLOW, "exactly 60s should be idle/yellow");
        // 1us before boundary: 59.999999s ago
        let just_inside = now - ACTIVE_THRESHOLD_US + 1;
        let (_, color) = activity_indicator(now, just_inside);
        assert_eq!(color, ACTIVITY_GREEN, "just under 60s should be green");
    }

    #[test]
    fn activity_indicator_boundary_at_5m() {
        let now = 1_000_000_000_i64;
        let at_boundary = now - IDLE_THRESHOLD_US;
        let (dot, color) = activity_indicator(now, at_boundary);
        assert_eq!(dot, '○');
        assert_eq!(color, ACTIVITY_GRAY, "exactly 5m should be stale/gray");
        let just_inside = now - IDLE_THRESHOLD_US + 1;
        let (dot, color) = activity_indicator(now, just_inside);
        assert_eq!(dot, '●');
        assert_eq!(color, ACTIVITY_YELLOW, "just under 5m should be yellow");
    }

    /// Test that `render_sparkline` uses `Sparkline` widget correctly (br-2bbt.4.1).
    #[test]
    fn render_sparkline_uses_sparkline_widget() {
        // Verify that the sparkline produces block characters from ftui_widgets::Sparkline.
        let data = [0.0, 25.0, 50.0, 75.0, 100.0];
        let out = render_sparkline(&data, 10);
        // Should produce 5 characters (data length, limited by width).
        assert_eq!(out.chars().count(), 5);
        // First char should be lowest (space or ▁), last should be highest (█ or similar).
        let chars: Vec<char> = out.chars().collect();
        // Verify it contains block chars from Sparkline (▁▂▃▄▅▆▇█ or space for 0).
        let has_block_chars = chars
            .iter()
            .any(|&c| matches!(c, ' ' | '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
        assert!(
            has_block_chars,
            "render_sparkline should use Sparkline block characters"
        );
    }

    #[test]
    fn render_sparkline_empty_data() {
        let out = render_sparkline(&[], 10);
        assert!(out.is_empty());
    }

    #[test]
    fn render_sparkline_zero_width() {
        let data = [1.0, 2.0, 3.0];
        let out = render_sparkline(&data, 0);
        assert!(out.is_empty());
    }
}
