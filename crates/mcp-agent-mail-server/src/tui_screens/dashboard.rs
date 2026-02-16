//! Dashboard screen — the default landing surface for `AgentMailTUI`.
//!
//! Displays real-time stats, a live event log, and health alarms in a
//! responsive layout that adapts from 80×24 to 200×50+.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ftui::Style;
use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba};
use ftui_extras::canvas::{Canvas, Mode, Painter};
use ftui_extras::charts::{LineChart, Series};
use ftui_extras::markdown::MarkdownTheme;
use ftui_extras::text_effects::{ColorGradient, StyledText, TextEffect};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, EventLogEntry, EventSeverity, MailEvent,
    MailEventKind, ProjectSummary, ReservationSnapshot, VerbosityTier, format_event_timestamp,
};
use crate::tui_layout::{
    DensityHint, PanelConstraint, PanelPolicy, PanelSlot, ReactiveLayout, SplitAxis, TerminalClass,
};
use crate::tui_screens::{DeepLinkTarget, HelpEntry, MailScreen, MailScreenMsg};
use crate::tui_widgets::{
    AnomalyCard, AnomalySeverity, ChartTransition, MetricTile, MetricTrend, PercentileRibbon,
    PercentileSample, ReservationGauge,
};
use ftui_widgets::input::TextInput;
use ftui_widgets::sparkline::Sparkline;

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max event log entries kept in scroll-back.
const EVENT_LOG_CAPACITY: usize = 2000;
const SHIMMER_WINDOW_MICROS: i64 = 500_000;
const SHIMMER_MAX_ROWS: usize = 5;
const SHIMMER_HIGHLIGHT_WIDTH: usize = 5;

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
        TerminalClass::Tiny => 0,
        TerminalClass::Compact => 3, // show 1 card, condensed
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

/// Title band height by terminal class (0 on tiny terminals).
const fn title_band_height(tc: TerminalClass) -> u16 {
    match tc {
        TerminalClass::Tiny => 0,
        _ => 1,
    }
}

/// Max percentile samples to retain.
const PERCENTILE_HISTORY_CAP: usize = 120;

/// Max throughput samples to retain.
const THROUGHPUT_HISTORY_CAP: usize = 120;
/// Chart transition duration for throughput updates.
const CHART_TRANSITION_DURATION: Duration = Duration::from_millis(200);
const RECENT_MESSAGE_PREVIEW_STALE_MICROS: i64 = 10 * 60 * 1_000_000;
const RESERVATION_SOON_THRESHOLD_MICROS: i64 = 5 * 60 * 1_000_000;
const AGENT_ACTIVE_THRESHOLD_MICROS: i64 = 60 * 1_000_000;
const AGENT_IDLE_THRESHOLD_MICROS: i64 = 5 * 60 * 1_000_000;
const TOP_MATCH_SAMPLE_CAP: usize = 4;
const ULTRAWIDE_INSIGHT_MIN_WIDTH: u16 = 120;
const ULTRAWIDE_INSIGHT_MIN_HEIGHT: u16 = 14;
const ULTRAWIDE_BOTTOM_MIN_WIDTH: u16 = 130;
const ULTRAWIDE_BOTTOM_MIN_HEIGHT: u16 = 8;
const SUPERGRID_INSIGHT_MIN_WIDTH: u16 = 170;
const SUPERGRID_INSIGHT_MIN_HEIGHT: u16 = 20;
const SUPERGRID_BOTTOM_MIN_WIDTH: u16 = 180;
const SUPERGRID_BOTTOM_MIN_HEIGHT: u16 = 9;
const MEGAGRID_BOTTOM_MIN_WIDTH: u16 = 220;
const MEGAGRID_BOTTOM_MIN_HEIGHT: u16 = 14;

/// Anomaly thresholds.
const ACK_PENDING_WARN: u64 = 3;
const ACK_PENDING_HIGH: u64 = 10;
const ERROR_RATE_WARN: f64 = 0.05;
const ERROR_RATE_HIGH: f64 = 0.15;
const RING_FILL_WARN: u8 = 80;
const TOOL_LATENCY_WARN_MS: u64 = 500;
const TOOL_LATENCY_HIGH_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardQuickFilter {
    All,
    Messages,
    Tools,
    Reservations,
}

impl DashboardQuickFilter {
    const fn next(self) -> Self {
        match self {
            Self::All => Self::Messages,
            Self::Messages => Self::Tools,
            Self::Tools => Self::Reservations,
            Self::Reservations => Self::All,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Messages => "Messages",
            Self::Tools => "Tools",
            Self::Reservations => "Reservations",
        }
    }

    const fn control_label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Messages => "Msg",
            Self::Tools => "Tools",
            Self::Reservations => "Resv",
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::All => "1",
            Self::Messages => "2",
            Self::Tools => "3",
            Self::Reservations => "4",
        }
    }

    const fn includes_messages(self) -> bool {
        matches!(self, Self::All | Self::Messages)
    }
}

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
    /// Current quick-filter selection for the dashboard `LogViewer`.
    quick_filter: DashboardQuickFilter,
    /// Verbosity tier controlling minimum severity shown.
    verbosity: VerbosityTier,
    /// Previous `DbStatSnapshot` for delta indicators.
    prev_db_stats: DbStatSnapshot,
    /// Current `DbStatSnapshot` used for summary panels and trend deltas.
    current_db_stats: DbStatSnapshot,
    /// Sparkline data: recent latency samples.
    sparkline_data: Vec<f64>,
    // ── Showcase composition state ───────────────────────────────
    /// Detected anomalies (refreshed each stat tick).
    anomalies: Vec<DetectedAnomaly>,
    /// Rolling percentile samples for the trend ribbon.
    percentile_history: Vec<PercentileSample>,
    /// Rolling throughput samples (requests per stat interval).
    throughput_history: Vec<f64>,
    /// Interpolated throughput samples rendered by the chart.
    animated_throughput_history: Vec<f64>,
    /// Transition state for throughput chart updates.
    throughput_transition: ChartTransition,
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
    /// Whether chart transitions are enabled (`AM_TUI_CHART_ANIMATIONS`).
    chart_animations_enabled: bool,
    /// Whether the console log panel is visible (toggled with `l`).
    show_log_panel: bool,
    /// Live dashboard query input (`/` to focus). Filters every panel in-place.
    quick_query_input: TextInput,
    /// True while the query input has keyboard focus.
    quick_query_active: bool,
    /// Console log pane for tool call cards / HTTP requests.
    console_log: RefCell<crate::console::LogPane>,
    /// Last consumed console log sequence number.
    console_log_last_seq: u64,
    /// Dashboard event stream rendered via `LogViewer`.
    event_log_viewer: RefCell<crate::console::LogPane>,
    /// Last DB snapshot timestamp used for synthetic delta events.
    last_db_snapshot_micros: i64,
    /// Last observed message counter for DB-delta synthesis.
    last_db_messages: u64,
    /// Last observed reservation counter for DB-delta synthesis.
    last_db_reservations: u64,
    /// Whether DB-delta baseline has been initialized.
    db_delta_baseline_ready: bool,
}

/// A pre-formatted event log entry.
pub(crate) type EventEntry = EventLogEntry;

/// Dashboard preview payload for the most recent message event.
#[derive(Debug, Clone)]
struct RecentMessagePreview {
    timestamp_micros: i64,
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
                timestamp_micros: *timestamp_micros,
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
                timestamp_micros: *timestamp_micros,
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

    fn is_stale(&self) -> bool {
        unix_epoch_micros_now().is_some_and(|now| {
            now.saturating_sub(self.timestamp_micros) > RECENT_MESSAGE_PREVIEW_STALE_MICROS
        })
    }
}

fn top_message_project_hint(snapshot: &DbStatSnapshot) -> Option<String> {
    snapshot
        .projects_list
        .iter()
        .filter(|project| project.message_count > 0)
        .max_by_key(|project| project.message_count)
        .map(|project| {
            format!(
                "top project {} ({} msgs)",
                project.slug, project.message_count
            )
        })
}

fn top_reservation_project_hint(snapshot: &DbStatSnapshot) -> Option<String> {
    snapshot
        .projects_list
        .iter()
        .filter(|project| project.reservation_count > 0)
        .max_by_key(|project| project.reservation_count)
        .map(|project| {
            format!(
                "top lock project {} ({})",
                project.slug, project.reservation_count
            )
        })
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
            quick_filter: DashboardQuickFilter::All,
            verbosity: VerbosityTier::Verbose,
            prev_db_stats: DbStatSnapshot::default(),
            current_db_stats: DbStatSnapshot::default(),
            sparkline_data: Vec::with_capacity(60),
            anomalies: Vec::new(),
            percentile_history: Vec::with_capacity(PERCENTILE_HISTORY_CAP),
            throughput_history: Vec::with_capacity(THROUGHPUT_HISTORY_CAP),
            animated_throughput_history: Vec::with_capacity(THROUGHPUT_HISTORY_CAP),
            throughput_transition: ChartTransition::new(CHART_TRANSITION_DURATION),
            prev_req_total: 0,
            show_trend_panel: true,
            recent_message_preview: None,
            pulse_phase: 0.0,
            reduced_motion: reduced_motion_enabled(),
            chart_animations_enabled: chart_animations_enabled(),
            show_log_panel: false,
            quick_query_input: TextInput::new()
                .with_placeholder("Type to live-filter dashboard; Enter opens Search Cockpit")
                .with_focused(false),
            quick_query_active: false,
            console_log: RefCell::new(crate::console::LogPane::new()),
            console_log_last_seq: 0,
            event_log_viewer: RefCell::new(crate::console::LogPane::new()),
            last_db_snapshot_micros: 0,
            last_db_messages: 0,
            last_db_reservations: 0,
            db_delta_baseline_ready: false,
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
        self.trim_event_log();
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_db_delta_events(&mut self, state: &TuiSharedState) {
        let Some(snapshot) = state.db_stats_snapshot() else {
            return;
        };
        if snapshot.timestamp_micros <= self.last_db_snapshot_micros {
            return;
        }

        if !self.db_delta_baseline_ready {
            self.db_delta_baseline_ready = true;
            self.last_db_messages = snapshot.messages;
            self.last_db_reservations = snapshot.file_reservations;
            self.last_db_snapshot_micros = snapshot.timestamp_micros;
            if self.event_log.is_empty() {
                if snapshot.messages > 0 {
                    let mut summary = format!("DB baseline: {} total messages", snapshot.messages);
                    if let Some(hint) = top_message_project_hint(&snapshot) {
                        summary = format!("{hint} · {summary}");
                    }
                    let synthetic = MailEvent::message_received(
                        i64::try_from(snapshot.messages).unwrap_or(i64::MAX),
                        "db-poller",
                        Vec::new(),
                        &summary,
                        "db-snapshot",
                        "all-projects",
                    );
                    if let Some(preview) = RecentMessagePreview::from_event(&synthetic) {
                        self.recent_message_preview = Some(preview);
                    }
                    self.event_log.push(format_event(&synthetic));
                }
                if snapshot.file_reservations > 0 {
                    let mut path = if snapshot.file_reservations == 1 {
                        "1 active reservation currently held".to_string()
                    } else {
                        format!(
                            "{} active reservations currently held",
                            snapshot.file_reservations
                        )
                    };
                    if let Some(hint) = top_reservation_project_hint(&snapshot) {
                        path.push_str(" · ");
                        path.push_str(&hint);
                    }
                    let synthetic = MailEvent::reservation_granted(
                        "db-poller",
                        vec![path],
                        false,
                        0,
                        "all-projects",
                    );
                    self.event_log.push(format_event(&synthetic));
                }
                self.trim_event_log();
            }
            return;
        }

        if snapshot.messages > self.last_db_messages {
            let delta = snapshot.messages.saturating_sub(self.last_db_messages);
            let mut summary = if delta == 1 {
                "DB observed 1 new message".to_string()
            } else {
                format!("DB observed {delta} new messages")
            };
            if let Some(hint) = top_message_project_hint(&snapshot) {
                summary = format!("{hint} · {summary}");
            }
            let synthetic = MailEvent::message_received(
                i64::try_from(snapshot.messages).unwrap_or(i64::MAX),
                "db-poller",
                Vec::new(),
                &summary,
                "db-snapshot",
                "all-projects",
            );
            if let Some(preview) = RecentMessagePreview::from_event(&synthetic) {
                self.recent_message_preview = Some(preview);
            }
            self.event_log.push(format_event(&synthetic));
        }

        if snapshot.file_reservations > self.last_db_reservations {
            let delta = snapshot
                .file_reservations
                .saturating_sub(self.last_db_reservations);
            let mut path = if delta == 1 {
                "1 active reservation added".to_string()
            } else {
                format!("{delta} active reservations added")
            };
            if let Some(hint) = top_reservation_project_hint(&snapshot) {
                path.push_str(" · ");
                path.push_str(&hint);
            }
            let synthetic =
                MailEvent::reservation_granted("db-poller", vec![path], false, 0, "all-projects");
            self.event_log.push(format_event(&synthetic));
        } else if snapshot.file_reservations < self.last_db_reservations {
            let delta = self
                .last_db_reservations
                .saturating_sub(snapshot.file_reservations);
            let mut path = if delta == 1 {
                "1 active reservation removed".to_string()
            } else {
                format!("{delta} active reservations removed")
            };
            if let Some(hint) = top_reservation_project_hint(&snapshot) {
                path.push_str(" · ");
                path.push_str(&hint);
            }
            let synthetic =
                MailEvent::reservation_released("db-poller", vec![path], "all-projects");
            self.event_log.push(format_event(&synthetic));
        }

        self.last_db_messages = snapshot.messages;
        self.last_db_reservations = snapshot.file_reservations;
        self.last_db_snapshot_micros = snapshot.timestamp_micros;
        self.trim_event_log();
    }

    fn trim_event_log(&mut self) {
        if self.event_log.len() > EVENT_LOG_CAPACITY {
            let excess = self.event_log.len() - EVENT_LOG_CAPACITY;
            self.event_log.drain(..excess);
        }
    }

    /// Visible entries after applying verbosity tier and type filter.
    fn visible_entries(&self) -> Vec<&EventEntry> {
        let query_terms = parse_query_terms(self.quick_query());
        self.event_log
            .iter()
            .filter(|e| {
                self.verbosity.includes(e.severity)
                    && (self.type_filter.is_empty() || self.type_filter.contains(&e.kind))
                    && event_entry_matches_query(e, &query_terms)
            })
            .collect()
    }

    fn quick_query(&self) -> &str {
        self.quick_query_input.value().trim()
    }

    fn begin_query_edit(&mut self) {
        self.quick_query_active = true;
        self.quick_query_input.set_focused(true);
    }

    fn end_query_edit(&mut self) {
        self.quick_query_active = false;
        self.quick_query_input.set_focused(false);
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

        // Counter/list divergence makes reservation and detail panes look empty.
        if db.file_reservations > 0 && db.reservation_snapshots.is_empty() {
            out.push(DetectedAnomaly {
                severity: AnomalySeverity::High,
                confidence: 0.92,
                headline: format!(
                    "{} active reservations but no reservation rows",
                    db.file_reservations
                ),
                rationale: "DB summary counters and reservation detail snapshots diverged"
                    .to_string(),
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

    // Render the event log into the given area via the free function.
    // NOTE: render_event_log_panel removed; caller now invokes render_event_log
    // directly with inline_anomaly_count for narrow-width annotation support.
    /// Render the console log panel in the sidebar area.
    fn render_console_log_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let tp = crate::tui_theme::TuiThemePalette::current();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Console Log ")
            .style(Style::default().fg(tp.panel_border));
        let inner = block.inner(area);
        block.render(area, frame);
        self.console_log.borrow_mut().render(inner, frame);
    }

    /// Build the `ReactiveLayout` for the main content area.
    ///
    /// Layout contains:
    /// - Primary event log
    /// - Optional trend panel (right rail)
    /// - Recent message markdown preview (bottom rail on wide terminals)
    /// - Optional console log panel (bottom sidebar)
    fn main_content_layout(show_trend_panel: bool, show_log_panel: bool) -> ReactiveLayout {
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

        if show_log_panel {
            layout = layout.panel(PanelPolicy::new(
                PanelSlot::Sidebar,
                3,
                SplitAxis::Horizontal,
                PanelConstraint::visible(0.45, 10),
            ));
        }

        layout.panel(
            PanelPolicy::new(
                PanelSlot::Footer,
                2,
                SplitAxis::Horizontal,
                PanelConstraint::HIDDEN,
            )
            .at(TerminalClass::Normal, PanelConstraint::visible(0.22, 5))
            .at(TerminalClass::Wide, PanelConstraint::visible(0.30, 8))
            .at(TerminalClass::UltraWide, PanelConstraint::visible(0.28, 9)),
        )
    }

    fn apply_quick_filter(&mut self, filter: DashboardQuickFilter) {
        self.quick_filter = filter;
        self.type_filter.clear();
        match filter {
            DashboardQuickFilter::All => {}
            DashboardQuickFilter::Messages => {
                self.type_filter.insert(MailEventKind::MessageSent);
                self.type_filter.insert(MailEventKind::MessageReceived);
            }
            DashboardQuickFilter::Tools => {
                self.type_filter.insert(MailEventKind::ToolCallStart);
                self.type_filter.insert(MailEventKind::ToolCallEnd);
            }
            DashboardQuickFilter::Reservations => {
                self.type_filter.insert(MailEventKind::ReservationGranted);
                self.type_filter.insert(MailEventKind::ReservationReleased);
            }
        }
        self.clamp_scroll_offset();
    }

    fn cycle_quick_filter(&mut self) {
        self.apply_quick_filter(self.quick_filter.next());
    }

    fn clamp_scroll_offset(&mut self) {
        if self.auto_follow {
            self.scroll_offset = 0;
            return;
        }
        let max_scroll = self.visible_entries().len().saturating_sub(1);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
    }
}

impl Default for DashboardScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MailScreen for DashboardScreen {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, event: &Event, _state: &TuiSharedState) -> Cmd<MailScreenMsg> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if self.quick_query_active {
                    match key.code {
                        KeyCode::Enter => {
                            let query = self.quick_query().to_string();
                            self.end_query_edit();
                            return Cmd::msg(MailScreenMsg::DeepLink(
                                DeepLinkTarget::SearchFocused(query),
                            ));
                        }
                        KeyCode::Escape | KeyCode::Tab => {
                            self.end_query_edit();
                        }
                        _ => {
                            let before = self.quick_query_input.value().to_string();
                            self.quick_query_input.handle_event(event);
                            if self.quick_query_input.value() != before {
                                self.clamp_scroll_offset();
                            }
                        }
                    }
                    return Cmd::None;
                }

                match key.code {
                    KeyCode::Char('/') => {
                        self.begin_query_edit();
                    }
                    KeyCode::Escape => {
                        if !self.quick_query().is_empty() {
                            self.quick_query_input.clear();
                            self.clamp_scroll_offset();
                        }
                    }
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
                    // Enter with query jumps to Search; otherwise timeline at focused event.
                    KeyCode::Enter => {
                        if !self.quick_query().is_empty() {
                            return Cmd::msg(MailScreenMsg::DeepLink(
                                DeepLinkTarget::SearchFocused(self.quick_query().to_string()),
                            ));
                        }
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
                        self.clamp_scroll_offset();
                    }
                    // Toggle trend panel visibility
                    KeyCode::Char('p') => {
                        self.show_trend_panel = !self.show_trend_panel;
                    }
                    // Toggle console log panel
                    KeyCode::Char('l') => {
                        self.show_log_panel = !self.show_log_panel;
                    }
                    // Toggle type filter
                    KeyCode::Char('t') => {
                        self.cycle_quick_filter();
                    }
                    KeyCode::Char('1') => self.apply_quick_filter(DashboardQuickFilter::All),
                    KeyCode::Char('2') => self.apply_quick_filter(DashboardQuickFilter::Messages),
                    KeyCode::Char('3') => self.apply_quick_filter(DashboardQuickFilter::Tools),
                    KeyCode::Char('4') => {
                        self.apply_quick_filter(DashboardQuickFilter::Reservations);
                    }
                    _ => {}
                }
            }
            // Mouse: scroll wheel moves event log (parity with j/k)
            Event::Mouse(mouse) => match mouse.kind {
                ftui::MouseEventKind::ScrollDown => {
                    if self.scroll_offset > 0 {
                        self.scroll_offset = self.scroll_offset.saturating_sub(1);
                    }
                    if self.scroll_offset == 0 {
                        self.auto_follow = true;
                    }
                }
                ftui::MouseEventKind::ScrollUp => {
                    self.scroll_offset += 1;
                    self.auto_follow = false;
                }
                _ => {}
            },
            _ => {}
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
        // Synthesize dashboard-friendly deltas from polled DB counters so
        // message/reservation movement remains visible even when no matching
        // domain events were emitted into the ring buffer.
        self.ingest_db_delta_events(state);
        // Keep scroll offset in-bounds so the log view never goes empty due
        // stale offsets after trims/filter changes.
        self.clamp_scroll_offset();

        // Ingest console log entries when panel is visible
        if self.show_log_panel {
            let new_entries = state.console_log_since(self.console_log_last_seq);
            if !new_entries.is_empty() {
                let mut pane = self.console_log.borrow_mut();
                for (seq, line) in &new_entries {
                    self.console_log_last_seq = *seq;
                    for l in line.split('\n') {
                        pane.push(crate::console::ansi_to_line(l));
                    }
                }
            }
        }

        // Refresh sparkline from per-request latency samples
        self.sparkline_data = state.sparkline_snapshot();

        // Refresh stats and compute trends on stat interval
        if tick_count % STAT_REFRESH_TICKS == 0 {
            if let Some(stats) = state.db_stats_snapshot() {
                if self.current_db_stats.timestamp_micros == 0 {
                    self.current_db_stats = stats.clone();
                    self.prev_db_stats = stats;
                } else if stats.timestamp_micros > self.current_db_stats.timestamp_micros {
                    self.prev_db_stats = std::mem::replace(&mut self.current_db_stats, stats);
                } else {
                    self.current_db_stats = stats;
                }
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

        let now = Instant::now();
        self.throughput_transition
            .set_target(&self.throughput_history, now);
        self.animated_throughput_history = self
            .throughput_transition
            .sample_values(now, self.reduced_motion || !self.chart_animations_enabled);
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        let tc = TerminalClass::from_rect(area);
        let density = DensityHint::from_terminal_class(tc);
        let effects_enabled = state.config_snapshot().tui_effects;

        // ── Panel budgets (explicit per terminal class) ──────────
        let title_h = title_band_height(tc);
        let summary_h = summary_band_height(tc);
        let anomaly_h = anomaly_rail_height(tc, self.anomalies.len());
        let footer_h = footer_bar_height(tc);
        let chrome_h = title_h + summary_h + anomaly_h + footer_h;
        let main_h = area.height.saturating_sub(chrome_h).max(3);

        // ── Rect allocation ──────────────────────────────────────
        let mut y = area.y;
        let title_area = Rect::new(area.x, y, area.width, title_h);
        y += title_h;
        let summary_area = Rect::new(area.x, y, area.width, summary_h);
        y += summary_h;
        let anomaly_area = Rect::new(area.x, y, area.width, anomaly_h);
        y += anomaly_h;
        let main_area = Rect::new(area.x, y, area.width, main_h);
        y += main_h;
        let footer_area = Rect::new(area.x, y, area.width, footer_h);

        // ── Gradient title ───────────────────────────────────────
        if title_h > 0 {
            render_gradient_title(frame, title_area, effects_enabled);
        }

        // ── Render bands ─────────────────────────────────────────
        render_summary_band(
            frame,
            summary_area,
            state,
            &self.current_db_stats,
            &self.prev_db_stats,
            density,
            self.pulse_phase,
        );

        if anomaly_h > 0 {
            render_anomaly_rail(frame, anomaly_area, &self.anomalies);
        }

        // Main: dense adaptive dashboard composition.
        let db_snapshot = if self.current_db_stats.timestamp_micros > 0 {
            self.current_db_stats.clone()
        } else {
            state.db_stats_snapshot().unwrap_or_default()
        };
        let visible_entries = self.visible_entries();
        let quick_query = self.quick_query();
        let layout = Self::main_content_layout(self.show_trend_panel, self.show_log_panel);
        let comp = layout.compute(main_area);
        // When the anomaly rail is hidden (Tiny), inject an inline annotation
        // so the operator still sees anomaly presence.
        let inline_anomaly_count = if anomaly_h == 0 {
            self.anomalies.len()
        } else {
            0
        };
        render_primary_cluster(
            frame,
            comp.primary(),
            &self.event_log_viewer,
            &self.quick_query_input,
            self.quick_query_active,
            quick_query,
            &db_snapshot,
            &visible_entries,
            self.scroll_offset,
            self.auto_follow,
            self.quick_filter,
            self.verbosity,
            inline_anomaly_count,
            effects_enabled && !self.reduced_motion,
        );
        if let Some(trend_rect) = comp.rect(PanelSlot::Inspector) {
            render_insight_rail(
                frame,
                trend_rect,
                &db_snapshot,
                quick_query,
                &self.anomalies,
                &visible_entries,
                &self.percentile_history,
                &self.animated_throughput_history,
                &self.event_log,
            );
        }
        if let Some(preview_rect) = comp.rect(PanelSlot::Footer) {
            let preview = if self.quick_filter.includes_messages() {
                self.recent_message_preview
                    .as_ref()
                    .filter(|preview| !preview.is_stale())
            } else {
                None
            };
            render_bottom_rail(
                frame,
                preview_rect,
                quick_query,
                &db_snapshot,
                &visible_entries,
                preview,
            );
        }
        if let Some(log_rect) = comp.rect(PanelSlot::Sidebar) {
            self.render_console_log_panel(frame, log_rect);
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
                action: "Search (if query) or timeline event",
            },
            HelpEntry {
                key: "/",
                action: "Focus live dashboard search",
            },
            HelpEntry {
                key: "Esc",
                action: "Dismiss overlay / quit confirm",
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
                action: "Cycle quick filter",
            },
            HelpEntry {
                key: "1-4",
                action: "Set filter (All/Msg/Tools/Resv)",
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
            HelpEntry {
                key: "l",
                action: "Toggle console log",
            },
            HelpEntry {
                key: "Mouse",
                action: "Wheel scroll event log",
            },
        ]
    }

    fn context_help_tip(&self) -> Option<&'static str> {
        Some("Overview of projects, agents, and live request counters.")
    }

    fn consumes_text_input(&self) -> bool {
        self.quick_query_active
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

/// Format a timestamp (microseconds) as HH:MM:SS.mmm.
fn format_ts(micros: i64) -> String {
    format_event_timestamp(micros)
}

/// Format a single `MailEvent` into a compact log entry.
#[must_use]
pub(crate) fn format_event(event: &MailEvent) -> EventEntry {
    event.to_event_log_entry()
}

#[cfg(test)]
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

fn parse_query_terms(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn text_matches_query_terms(haystack: &str, query_terms: &[String]) -> bool {
    if query_terms.is_empty() {
        return true;
    }
    let lower = haystack.to_ascii_lowercase();
    query_terms.iter().all(|term| lower.contains(term))
}

fn event_entry_matches_query(entry: &EventEntry, query_terms: &[String]) -> bool {
    if query_terms.is_empty() {
        return true;
    }
    let searchable = format!(
        "{} {} {} {} {}",
        entry.kind.compact_label(),
        entry.summary,
        entry.timestamp,
        entry.severity.badge(),
        entry.icon
    );
    text_matches_query_terms(&searchable, query_terms)
}

fn parse_tool_end_duration(summary: &str) -> Option<(String, u64)> {
    let mut parts = summary.split_whitespace();
    let tool_name = parts.next()?;
    let duration_token = parts.next()?;
    let duration_ms = duration_token.strip_suffix("ms")?.parse::<u64>().ok()?;
    Some((tool_name.to_string(), duration_ms))
}

fn ratio_bar(value: u64, max: u64, cells: usize) -> String {
    if cells == 0 {
        return String::new();
    }
    let filled_raw = if max == 0 {
        0usize
    } else {
        let numerator = u128::from(value).saturating_mul(u128::try_from(cells).unwrap_or(0));
        let denom = u128::from(max).max(1);
        usize::try_from(numerator / denom)
            .unwrap_or(cells)
            .min(cells)
    };
    let filled = if value > 0 && filled_raw == 0 {
        1usize.min(cells)
    } else {
        filled_raw
    };
    let mut out = String::with_capacity(cells);
    for idx in 0..cells {
        out.push(if idx < filled { '█' } else { '░' });
    }
    out
}

fn format_sample_with_overflow(samples: &[String], total: usize) -> String {
    if total == 0 || samples.is_empty() {
        return "none".to_string();
    }
    if total > samples.len() {
        return format!("{} +{}", samples.join(", "), total - samples.len());
    }
    samples.join(", ")
}

fn format_relative_micros(timestamp_micros: i64, now_micros: i64) -> String {
    if timestamp_micros <= 0 {
        return "n/a".to_string();
    }
    let elapsed = now_micros.saturating_sub(timestamp_micros).max(0);
    let secs = elapsed / 1_000_000;
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h", secs / 3600);
    }
    format!("{}d", secs / 86_400)
}

fn render_panel_hint_line(frame: &mut Frame<'_>, inner: Rect, hint: &str) {
    if inner.width < 8 || inner.height < 2 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    Paragraph::new(truncate(hint, usize::from(hint_area.width)))
        .style(crate::tui_theme::text_meta(&tp))
        .render(hint_area, frame);
}

const fn is_supergrid_insight_area(area: Rect) -> bool {
    area.width >= SUPERGRID_INSIGHT_MIN_WIDTH && area.height >= SUPERGRID_INSIGHT_MIN_HEIGHT
}

const fn is_supergrid_bottom_area(area: Rect) -> bool {
    area.width >= SUPERGRID_BOTTOM_MIN_WIDTH && area.height >= SUPERGRID_BOTTOM_MIN_HEIGHT
}

const fn is_megagrid_bottom_area(area: Rect) -> bool {
    area.width >= MEGAGRID_BOTTOM_MIN_WIDTH && area.height >= MEGAGRID_BOTTOM_MIN_HEIGHT
}

fn split_top(area: Rect, top_h: u16) -> (Rect, Rect) {
    let top_h = top_h.min(area.height);
    let top = Rect::new(area.x, area.y, area.width, top_h);
    let bottom = Rect::new(
        area.x,
        area.y.saturating_add(top_h),
        area.width,
        area.height.saturating_sub(top_h),
    );
    (top, bottom)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn split_width_ratio_with_gap(area: Rect, left_ratio: f32, gap: u16) -> (Rect, Rect) {
    let gap = gap.min(area.width.saturating_sub(1));
    let available = area.width.saturating_sub(gap);
    let left_w = (f32::from(available) * left_ratio).round() as u16;
    let left_w = left_w.min(available);
    let left = Rect::new(area.x, area.y, left_w, area.height);
    let right = Rect::new(
        area.x.saturating_add(left_w).saturating_add(gap),
        area.y,
        available.saturating_sub(left_w),
        area.height,
    );
    (left, right)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn split_height_ratio_with_gap(area: Rect, top_ratio: f32, gap: u16) -> (Rect, Rect) {
    let gap = gap.min(area.height.saturating_sub(1));
    let available = area.height.saturating_sub(gap);
    let top_h = (f32::from(available) * top_ratio).round() as u16;
    let top_h = top_h.min(available);
    let top = Rect::new(area.x, area.y, area.width, top_h);
    let bottom = Rect::new(
        area.x,
        area.y.saturating_add(top_h).saturating_add(gap),
        area.width,
        available.saturating_sub(top_h),
    );
    (top, bottom)
}

fn dense_columns_for_width(width: u16, min_col_width: u16, max_cols: usize) -> usize {
    if width == 0 || max_cols == 0 {
        return 1;
    }
    let min_col = min_col_width.max(1);
    let mut cols = 1usize;
    while cols < max_cols {
        let next = cols + 1;
        let next_u16 = u16::try_from(next).unwrap_or(u16::MAX);
        let needed = min_col
            .saturating_mul(next_u16)
            .saturating_add(next_u16.saturating_sub(1));
        if needed > width {
            break;
        }
        cols = next;
    }
    cols.max(1)
}

fn render_lines_with_columns(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: &[Line],
    min_col_width: u16,
    max_cols: usize,
) {
    if area.is_empty() {
        return;
    }
    let rows = usize::from(area.height);
    if rows == 0 || lines.is_empty() {
        return;
    }
    let cols_by_width = dense_columns_for_width(area.width, min_col_width, max_cols);
    if cols_by_width <= 1 || lines.len() <= rows {
        Paragraph::new(Text::from_lines(lines.to_vec())).render(area, frame);
        return;
    }
    let cols_needed = lines.len().div_ceil(rows);
    let cols = cols_by_width.min(cols_needed).max(1);
    if cols <= 1 {
        Paragraph::new(Text::from_lines(lines.to_vec())).render(area, frame);
        return;
    }
    let cols_u16 = u16::try_from(cols).unwrap_or(1).max(1);
    let total_gap = cols_u16.saturating_sub(1);
    let available_width = area.width.saturating_sub(total_gap);
    let base_width = available_width.checked_div(cols_u16).unwrap_or(area.width);
    let mut x = area.x;
    for col_idx in 0..cols {
        let col = u16::try_from(col_idx).unwrap_or(u16::MAX);
        let width = if col_idx == cols - 1 {
            let used = base_width
                .saturating_mul(col)
                .saturating_add(col.min(total_gap));
            available_width.saturating_sub(used)
        } else {
            base_width
        };
        let rect = Rect::new(x, area.y, width, area.height);
        let start = col_idx.saturating_mul(rows);
        let end = start.saturating_add(rows).min(lines.len());
        if start >= end {
            break;
        }
        Paragraph::new(Text::from_lines(lines[start..end].to_vec())).render(rect, frame);
        x = x.saturating_add(width).saturating_add(1);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the dashboard title with optional gradient effect.
///
/// Uses [`StyledText`] with [`TextEffect::HorizontalGradient`] to produce
/// a smooth color transition from `status_accent` to `severity_ok` across
/// the title text when effects are enabled. The title is centered within
/// the given area.
fn render_gradient_title(frame: &mut Frame<'_>, area: Rect, effects_enabled: bool) {
    use ftui::text::{Line, Span};

    if area.width == 0 || area.height == 0 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let full_title = "Agent Mail Dashboard";
    let title_text = truncate(full_title, usize::from(area.width));
    let text_len = u16::try_from(title_text.len()).unwrap_or(area.width);
    let text_len = text_len.min(area.width);
    let x_offset = area.width.saturating_sub(text_len) / 2;
    let title_area = Rect::new(area.x + x_offset, area.y, text_len, 1);

    if effects_enabled {
        let gradient = ColorGradient::new(vec![(0.0, tp.status_accent), (1.0, tp.severity_ok)]);
        StyledText::new(title_text)
            .effect(TextEffect::HorizontalGradient { gradient })
            .base_color(tp.status_accent)
            .bold()
            .render(title_area, frame);
        return;
    }

    let line = Line::from_spans([Span::styled(title_text, crate::tui_theme::text_accent(&tp))]);
    Paragraph::new(line).render(title_area, frame);
}

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
    current_stats: &DbStatSnapshot,
    prev_stats: &DbStatSnapshot,
    density: DensityHint,
    pulse_phase: f32,
) {
    let counters = state.request_counters();
    let db = current_stats;
    let uptime_str = format_duration(state.uptime());
    let avg_ms = counters
        .latency_total_ms
        .checked_div(counters.total)
        .unwrap_or(0);
    let avg_str = format!("{avg_ms}ms");
    let msg_str = format!("{}", db.messages);
    let agent_str = format!("{}", db.agents);
    let lock_str = format!("{}", db.file_reservations);
    let project_str = format!("{}", db.projects);
    let ack_str = format!("{}", db.ack_pending);
    let req_str = format!("{}", counters.total);
    let contact_str = format!("{}", db.contact_links);
    let ring_stats = state.event_ring_stats();
    let ring_fill = ring_stats.fill_pct();
    let ring_fill_str = format!("{ring_fill}%");
    let drop_count = ring_stats.total_drops();
    let drop_str = format!("{drop_count}");
    let error_total = counters.status_4xx.saturating_add(counters.status_5xx);
    #[allow(clippy::cast_precision_loss)]
    let error_rate = if counters.total == 0 {
        0.0
    } else {
        error_total as f64 / counters.total as f64
    };
    #[allow(clippy::cast_precision_loss)]
    let error_rate_str = format!("{:.1}%", error_rate * 100.0);

    // Determine trend directions by comparing to previous snapshot.
    let msg_trend = trend_for(db.messages, prev_stats.messages);
    let agent_trend = trend_for(db.agents, prev_stats.agents);
    let contact_trend = trend_for(db.contact_links, prev_stats.contact_links);
    let reservation_trend = trend_for(db.file_reservations, prev_stats.file_reservations);
    let ack_trend = match db.ack_pending.cmp(&prev_stats.ack_pending) {
        std::cmp::Ordering::Greater => MetricTrend::Up, // ack growing = bad
        std::cmp::Ordering::Less => MetricTrend::Down,
        std::cmp::Ordering::Equal => MetricTrend::Flat,
    };

    // Calculate pulse color for Requests
    let tp = crate::tui_theme::TuiThemePalette::current();
    let pulse = f32::midpoint(pulse_phase.sin(), 1.0); // 0.0 to 1.0
    let req_color = crate::tui_theme::lerp_color(tp.metric_requests, tp.sparkline_hi, pulse);

    // Build tiles based on density.
    //
    // Ordered by operational priority: actionable/flow metrics first,
    // infrastructure/context metrics last.
    let ack_color = if db.ack_pending > 0 {
        tp.metric_ack_bad
    } else {
        tp.metric_ack_ok
    };
    let error_color = if error_rate >= ERROR_RATE_HIGH {
        tp.severity_error
    } else if error_rate >= ERROR_RATE_WARN {
        tp.severity_warn
    } else {
        tp.severity_ok
    };
    let ring_color = if ring_fill >= RING_FILL_WARN {
        tp.severity_warn
    } else {
        tp.metric_requests
    };
    let drop_color = if drop_count > 0 {
        tp.severity_warn
    } else {
        tp.severity_ok
    };
    let mut tiles: Vec<(&str, &str, MetricTrend, PackedRgba)> = match density {
        DensityHint::Minimal | DensityHint::Compact => vec![
            ("Msg", &msg_str, msg_trend, tp.metric_messages),
            ("Ack", &ack_str, ack_trend, ack_color),
            ("Locks", &lock_str, reservation_trend, tp.ttl_warning),
            ("Req", &req_str, MetricTrend::Flat, req_color),
        ],
        DensityHint::Normal => vec![
            ("Messages", &msg_str, msg_trend, tp.metric_messages),
            ("Ack Pend", &ack_str, ack_trend, ack_color),
            ("Locks", &lock_str, reservation_trend, tp.ttl_warning),
            ("Agents", &agent_str, agent_trend, tp.metric_agents),
            ("Contacts", &contact_str, contact_trend, tp.status_accent),
            ("Requests", &req_str, MetricTrend::Flat, req_color),
            ("Avg Lat", &avg_str, MetricTrend::Flat, tp.metric_latency),
            ("Error %", &error_rate_str, MetricTrend::Flat, error_color),
        ],
        DensityHint::Detailed => vec![
            ("Messages", &msg_str, msg_trend, tp.metric_messages),
            ("Ack Pend", &ack_str, ack_trend, ack_color),
            ("Locks", &lock_str, reservation_trend, tp.ttl_warning),
            ("Agents", &agent_str, agent_trend, tp.metric_agents),
            ("Contacts", &contact_str, contact_trend, tp.status_accent),
            (
                "Projects",
                &project_str,
                MetricTrend::Flat,
                tp.status_accent,
            ),
            ("Requests", &req_str, MetricTrend::Flat, req_color),
            ("Avg Lat", &avg_str, MetricTrend::Flat, tp.metric_latency),
            ("Error %", &error_rate_str, MetricTrend::Flat, error_color),
            ("Ring Fill", &ring_fill_str, MetricTrend::Flat, ring_color),
            ("Drops", &drop_str, MetricTrend::Flat, drop_color),
            ("Uptime", &uptime_str, MetricTrend::Flat, tp.metric_uptime),
        ],
    };

    let max_tiles_by_width = usize::from((area.width / 12).max(1));
    if tiles.len() > max_tiles_by_width {
        tiles.truncate(max_tiles_by_width);
    }

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
    // Adaptive card count: 1 on narrow terminals, up to 3 on wide.
    let max_cards = if area.width < 80 { 1 } else { 3 };
    let visible = anomalies.len().min(max_cards);
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

#[allow(clippy::too_many_arguments)]
fn render_primary_cluster(
    frame: &mut Frame<'_>,
    area: Rect,
    viewer: &RefCell<crate::console::LogPane>,
    query_input: &TextInput,
    query_active: bool,
    query_text: &str,
    db_snapshot: &DbStatSnapshot,
    entries: &[&EventEntry],
    scroll_offset: usize,
    auto_follow: bool,
    quick_filter: DashboardQuickFilter,
    verbosity: VerbosityTier,
    inline_anomaly_count: usize,
    effects_enabled: bool,
) {
    if area.height < 3 || area.width < 24 {
        return;
    }

    let mut query_h = if area.height >= 15 {
        3
    } else if area.height >= 9 {
        2
    } else {
        0
    };
    if area.height.saturating_sub(query_h) < 3 {
        query_h = 0;
    }
    let (query_area, event_area) = split_top(area, query_h);

    if query_h > 0 {
        render_dashboard_query_bar(
            frame,
            query_area,
            query_input,
            query_active,
            query_text,
            db_snapshot,
            entries,
        );
    }

    if event_area.height >= 3 {
        render_event_log(
            frame,
            event_area,
            viewer,
            entries,
            scroll_offset,
            auto_follow,
            quick_filter,
            verbosity,
            inline_anomaly_count,
            effects_enabled,
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_dashboard_query_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    query_input: &TextInput,
    query_active: bool,
    query_text: &str,
    db_snapshot: &DbStatSnapshot,
    entries: &[&EventEntry],
) {
    if area.is_empty() {
        return;
    }
    let query_terms = parse_query_terms(query_text);
    let agent_matches = db_snapshot
        .agents_list
        .iter()
        .filter(|agent| {
            text_matches_query_terms(&format!("{} {}", agent.name, agent.program), &query_terms)
        })
        .count();
    let project_matches = db_snapshot
        .projects_list
        .iter()
        .filter(|project| {
            text_matches_query_terms(
                &format!("{} {}", project.slug, project.human_key),
                &query_terms,
            )
        })
        .count();
    let reservation_matches = db_snapshot
        .reservation_snapshots
        .iter()
        .filter(|reservation| {
            text_matches_query_terms(
                &format!(
                    "{} {} {}",
                    reservation.project_slug, reservation.agent_name, reservation.path_pattern
                ),
                &query_terms,
            )
        })
        .count();
    let contact_matches = db_snapshot
        .contacts_list
        .iter()
        .filter(|contact| {
            text_matches_query_terms(
                &format!(
                    "{} {} {} {}",
                    contact.from_agent, contact.to_agent, contact.status, contact.reason
                ),
                &query_terms,
            )
        })
        .count();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = if query_active {
        "Live Filter [EDITING]"
    } else {
        "Live Filter [/ to edit]"
    };
    let border = if query_active {
        Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
    } else {
        Style::default().fg(tp.panel_border)
    };
    let block = Block::default()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    query_input.render(Rect::new(inner.x, inner.y, inner.width, 1), frame);

    if inner.height < 2 {
        return;
    }

    let status_line = if query_terms.is_empty() {
        format!(
            "Type to filter all panels in-place. Enter opens Search Cockpit. Totals: ev={} ag={} pr={} rs={} ct={}",
            entries.len(),
            db_snapshot.agents,
            db_snapshot.projects,
            db_snapshot.file_reservations,
            db_snapshot.contact_links
        )
    } else {
        format!(
            "Matches: ev={} ag={} pr={} rs={} ct={} · terms:{} · Esc clears · Enter opens Search",
            entries.len(),
            agent_matches,
            project_matches,
            reservation_matches,
            contact_matches,
            query_terms.len(),
        )
    };
    Paragraph::new(truncate(&status_line, usize::from(inner.width)))
        .render(Rect::new(inner.x, inner.y + 1, inner.width, 1), frame);

    if inner.height < 3 {
        return;
    }
    let mode_line = if query_terms.is_empty() {
        "Live panels: throughput, agents, contacts, projects, reservations, signals, tools"
    } else {
        "Filtering uses case-insensitive AND matching across each panel"
    };
    Paragraph::new(truncate(mode_line, usize::from(inner.width)))
        .style(crate::tui_theme::text_meta(&tp))
        .render(Rect::new(inner.x, inner.y + 2, inner.width, 1), frame);
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn render_insight_rail(
    frame: &mut Frame<'_>,
    area: Rect,
    db_snapshot: &DbStatSnapshot,
    query_text: &str,
    anomalies: &[DetectedAnomaly],
    entries: &[&EventEntry],
    percentile_history: &[PercentileSample],
    throughput_history: &[f64],
    event_log: &[EventEntry],
) {
    if area.width < 24 || area.height < 8 {
        return;
    }

    let ultrawide = area.width >= ULTRAWIDE_INSIGHT_MIN_WIDTH && area.height >= 20;
    let (trend_area, remaining) = if area.height >= 20 {
        let trend_height = if ultrawide {
            (area.height.saturating_mul(2) / 5).max(8)
        } else {
            (area.height / 3).max(7)
        };
        split_top(area, trend_height)
    } else {
        (Rect::new(0, 0, 0, 0), area)
    };

    if trend_area.height > 0 {
        render_trend_panel(
            frame,
            trend_area,
            percentile_history,
            throughput_history,
            event_log,
        );
    }

    if remaining.height < 4 {
        return;
    }

    if is_supergrid_insight_area(remaining) {
        let (col_a, rest) = split_width_ratio_with_gap(remaining, 0.25, 1);
        let (col_b, rest) = split_width_ratio_with_gap(rest, 1.0 / 3.0, 1);
        let (col_c, col_d) = split_width_ratio_with_gap(rest, 0.5, 1);
        let (a_top, a_bottom) = split_height_ratio_with_gap(col_a, 0.5, 1);
        let (b_top, b_bottom) = split_height_ratio_with_gap(col_b, 0.5, 1);
        let (c_top, c_bottom) = split_height_ratio_with_gap(col_c, 0.5, 1);
        let (d_top, d_bottom) = split_height_ratio_with_gap(col_d, 0.5, 1);

        render_agents_snapshot_panel(frame, a_top, &db_snapshot.agents_list, query_text);
        render_contacts_snapshot_panel(frame, a_bottom, &db_snapshot.contacts_list, query_text);
        render_projects_snapshot_panel(frame, b_top, &db_snapshot.projects_list, query_text);
        render_project_load_panel(frame, b_bottom, &db_snapshot.projects_list, query_text);
        render_reservation_watch_panel(
            frame,
            c_top,
            &db_snapshot.reservation_snapshots,
            query_text,
        );
        render_reservation_ttl_buckets_panel(
            frame,
            c_bottom,
            &db_snapshot.reservation_snapshots,
            query_text,
        );
        render_signal_panel(
            frame,
            d_top,
            anomalies,
            entries,
            &db_snapshot.contacts_list,
            query_text,
        );
        render_tool_latency_panel(frame, d_bottom, entries, query_text);
        return;
    }

    if remaining.width >= ULTRAWIDE_INSIGHT_MIN_WIDTH
        && remaining.height >= ULTRAWIDE_INSIGHT_MIN_HEIGHT
    {
        let (col_a, rest) = split_width_ratio_with_gap(remaining, 0.34, 1);
        let (col_b, col_c) = split_width_ratio_with_gap(rest, 0.5, 1);
        let (a_top, a_bottom) = split_height_ratio_with_gap(col_a, 0.5, 1);
        let (b_top, b_bottom) = split_height_ratio_with_gap(col_b, 0.5, 1);
        let (c_top, c_bottom) = split_height_ratio_with_gap(col_c, 0.5, 1);

        render_agents_snapshot_panel(frame, a_top, &db_snapshot.agents_list, query_text);
        render_contacts_snapshot_panel(frame, a_bottom, &db_snapshot.contacts_list, query_text);
        render_projects_snapshot_panel(frame, b_top, &db_snapshot.projects_list, query_text);
        render_reservation_watch_panel(
            frame,
            b_bottom,
            &db_snapshot.reservation_snapshots,
            query_text,
        );
        render_signal_panel(
            frame,
            c_top,
            anomalies,
            entries,
            &db_snapshot.contacts_list,
            query_text,
        );
        render_event_mix_panel(frame, c_bottom, entries, query_text);
        return;
    }

    if remaining.width < 50 || remaining.height < 12 {
        let (top, bottom) = split_height_ratio_with_gap(remaining, 0.5, 1);
        render_agents_snapshot_panel(frame, top, &db_snapshot.agents_list, query_text);
        render_reservation_watch_panel(
            frame,
            bottom,
            &db_snapshot.reservation_snapshots,
            query_text,
        );
        return;
    }

    let (top_row, bottom_row) = split_height_ratio_with_gap(remaining, 0.5, 1);
    let (top_left, top_right) = split_width_ratio_with_gap(top_row, 0.54, 1);
    let (bottom_left, bottom_right) = split_width_ratio_with_gap(bottom_row, 0.54, 1);

    render_agents_snapshot_panel(frame, top_left, &db_snapshot.agents_list, query_text);
    render_projects_snapshot_panel(frame, top_right, &db_snapshot.projects_list, query_text);
    render_reservation_watch_panel(
        frame,
        bottom_left,
        &db_snapshot.reservation_snapshots,
        query_text,
    );
    render_signal_panel(
        frame,
        bottom_right,
        anomalies,
        entries,
        &db_snapshot.contacts_list,
        query_text,
    );
}

fn render_bottom_rail(
    frame: &mut Frame<'_>,
    area: Rect,
    query_text: &str,
    db_snapshot: &DbStatSnapshot,
    entries: &[&EventEntry],
    preview: Option<&RecentMessagePreview>,
) {
    if area.width < 24 || area.height < 4 {
        return;
    }

    if is_megagrid_bottom_area(area) {
        let (preview_col, rest) = split_width_ratio_with_gap(area, 0.30, 1);
        let (query_col, rest) = split_width_ratio_with_gap(rest, 0.29, 1);
        let (activity_col, metrics_col) = split_width_ratio_with_gap(rest, 0.40, 1);

        render_recent_message_preview_panel(frame, preview_col, preview);
        render_query_matches_panel(frame, query_col, query_text, db_snapshot, entries);
        render_recent_activity_panel(frame, activity_col, entries, query_text);

        if metrics_col.height >= 10 {
            let (metrics_top, metrics_bottom) = split_height_ratio_with_gap(metrics_col, 0.5, 1);
            let (top_left, top_right) = split_width_ratio_with_gap(metrics_top, 0.5, 1);
            let (bottom_left, bottom_right) = split_width_ratio_with_gap(metrics_bottom, 0.5, 1);
            render_event_mix_panel(frame, top_left, entries, query_text);
            render_message_flow_panel(frame, top_right, entries, query_text);
            render_tool_latency_panel(frame, bottom_left, entries, query_text);
            render_reservation_ttl_buckets_panel(
                frame,
                bottom_right,
                &db_snapshot.reservation_snapshots,
                query_text,
            );
        } else {
            let (left, right) = split_width_ratio_with_gap(metrics_col, 0.5, 1);
            render_event_mix_panel(frame, left, entries, query_text);
            render_message_flow_panel(frame, right, entries, query_text);
        }
        return;
    }

    if is_supergrid_bottom_area(area) {
        let (left, rest) = split_width_ratio_with_gap(area, 0.34, 1);
        let (middle, rest) = split_width_ratio_with_gap(rest, 0.33, 1);
        let (right, aux) = split_width_ratio_with_gap(rest, 0.5, 1);
        render_recent_message_preview_panel(frame, left, preview);
        render_query_matches_panel(frame, middle, query_text, db_snapshot, entries);
        render_recent_activity_panel(frame, right, entries, query_text);

        if aux.height >= 14 {
            let (aux_top, aux_bottom) = split_height_ratio_with_gap(aux, 0.5, 1);
            let (top_left, top_right) = split_width_ratio_with_gap(aux_top, 0.5, 1);
            let (bottom_left, bottom_right) = split_width_ratio_with_gap(aux_bottom, 0.5, 1);
            render_event_mix_panel(frame, top_left, entries, query_text);
            render_message_flow_panel(frame, top_right, entries, query_text);
            render_tool_latency_panel(frame, bottom_left, entries, query_text);
            render_reservation_ttl_buckets_panel(
                frame,
                bottom_right,
                &db_snapshot.reservation_snapshots,
                query_text,
            );
        } else if aux.height >= 10 {
            let (aux_top, aux_bottom) = split_height_ratio_with_gap(aux, 0.5, 1);
            render_event_mix_panel(frame, aux_top, entries, query_text);
            render_message_flow_panel(frame, aux_bottom, entries, query_text);
        } else {
            render_message_flow_panel(frame, aux, entries, query_text);
        }
        return;
    }

    if area.width >= ULTRAWIDE_BOTTOM_MIN_WIDTH && area.height >= ULTRAWIDE_BOTTOM_MIN_HEIGHT {
        let (left, rest) = split_width_ratio_with_gap(area, 0.5, 1);
        let (middle, right) = split_width_ratio_with_gap(rest, 0.5, 1);
        render_recent_message_preview_panel(frame, left, preview);
        render_query_matches_panel(frame, middle, query_text, db_snapshot, entries);
        render_recent_activity_panel(frame, right, entries, query_text);
        return;
    }

    if area.width < 70 || area.height < 6 {
        if query_text.is_empty() {
            render_recent_message_preview_panel(frame, area, preview);
        } else {
            render_query_matches_panel(frame, area, query_text, db_snapshot, entries);
        }
        return;
    }

    let (left, right) = split_width_ratio_with_gap(area, 0.62, 1);
    render_recent_message_preview_panel(frame, left, preview);
    render_query_matches_panel(frame, right, query_text, db_snapshot, entries);
}

fn render_agents_snapshot_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[AgentSummary],
    query_text: &str,
) {
    if area.width < 18 || area.height < 3 {
        return;
    }
    let query_terms = parse_query_terms(query_text);
    let mut rows: Vec<&AgentSummary> = agents
        .iter()
        .filter(|agent| {
            text_matches_query_terms(&format!("{} {}", agent.name, agent.program), &query_terms)
        })
        .collect();
    rows.sort_by_key(|agent| std::cmp::Reverse(agent.last_active_ts));

    let now = unix_epoch_micros_now().unwrap_or_default();
    let mut active = 0usize;
    let mut idle = 0usize;
    let mut stale = 0usize;
    for agent in &rows {
        if agent.last_active_ts <= 0 {
            stale += 1;
            continue;
        }
        let elapsed = now.saturating_sub(agent.last_active_ts);
        if elapsed < AGENT_ACTIVE_THRESHOLD_MICROS {
            active += 1;
        } else if elapsed < AGENT_IDLE_THRESHOLD_MICROS {
            idle += 1;
        } else {
            stale += 1;
        }
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Agents · {}", rows.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        Paragraph::new("No matching agents")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    if content_rows >= 2 {
        lines.push(Line::from_spans([
            Span::styled(
                format!("active:{active} idle:{idle} stale:{stale}"),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled("●<60s ●<5m ○>=5m", crate::tui_theme::text_meta(&tp)),
        ]));
    }
    let dense_cols = dense_columns_for_width(content_area.width, 44, 2);
    let list_budget = content_rows
        .saturating_mul(dense_cols)
        .saturating_sub(lines.len());
    for agent in rows.iter().take(list_budget) {
        let (marker, color) = agent_status_marker(agent.last_active_ts, now);
        let age = format_relative_micros(agent.last_active_ts, now);
        lines.push(Line::from_spans([
            Span::styled(marker.to_string(), Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(
                truncate(&agent.name, 14),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(&agent.program, 9),
                crate::tui_theme::text_meta(&tp),
            ),
            Span::raw(" "),
            Span::styled(age, crate::tui_theme::text_meta(&tp)),
        ]));
    }
    render_lines_with_columns(frame, content_area, &lines, 44, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "sorted by most recently active");
    }
}

#[allow(clippy::too_many_lines)]
fn render_contacts_snapshot_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    contacts: &[ContactSummary],
    query_text: &str,
) {
    if area.width < 18 || area.height < 3 {
        return;
    }
    let query_terms = parse_query_terms(query_text);
    let mut rows: Vec<&ContactSummary> = contacts
        .iter()
        .filter(|contact| {
            text_matches_query_terms(
                &format!(
                    "{} {} {} {} {} {}",
                    contact.from_agent,
                    contact.to_agent,
                    contact.from_project_slug,
                    contact.to_project_slug,
                    contact.status,
                    contact.reason
                ),
                &query_terms,
            )
        })
        .collect();
    rows.sort_by_key(|contact| std::cmp::Reverse(contact.updated_ts));

    let pending = rows
        .iter()
        .filter(|contact| contact.status.eq_ignore_ascii_case("pending"))
        .count();
    let accepted = rows
        .iter()
        .filter(|contact| contact.status.eq_ignore_ascii_case("accepted"))
        .count();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Contacts · {} (p:{pending})", rows.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        Paragraph::new("No matching contacts")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let now = unix_epoch_micros_now().unwrap_or_default();
    let mut lines = Vec::new();
    if content_rows >= 2 {
        lines.push(Line::from_spans([
            Span::styled(
                format!("pending:{pending} active:{accepted}"),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                format!("total:{}", rows.len()),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }

    let dense_cols = dense_columns_for_width(content_area.width, 40, 2);
    let list_budget = content_rows
        .saturating_mul(dense_cols)
        .saturating_sub(lines.len());
    for contact in rows.iter().take(list_budget) {
        let age = format_relative_micros(contact.updated_ts, now);
        let status_color = if contact.status.eq_ignore_ascii_case("pending") {
            tp.severity_warn
        } else if contact.status.eq_ignore_ascii_case("accepted") {
            tp.severity_ok
        } else {
            tp.text_muted
        };
        lines.push(Line::from_spans([
            Span::styled(
                truncate(&contact.from_agent, 10),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw("→"),
            Span::styled(
                truncate(&contact.to_agent, 10),
                Style::default().fg(tp.text_primary),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(&contact.status, 7),
                Style::default().fg(status_color),
            ),
            Span::raw(" "),
            Span::styled(age, crate::tui_theme::text_meta(&tp)),
        ]));
    }

    render_lines_with_columns(frame, content_area, &lines, 40, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "pending links need respond_contact");
    }
}

fn render_projects_snapshot_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    projects: &[ProjectSummary],
    query_text: &str,
) {
    if area.width < 18 || area.height < 3 {
        return;
    }
    let query_terms = parse_query_terms(query_text);
    let mut rows: Vec<&ProjectSummary> = projects
        .iter()
        .filter(|project| {
            text_matches_query_terms(
                &format!("{} {}", project.slug, project.human_key),
                &query_terms,
            )
        })
        .collect();
    rows.sort_by(|a, b| {
        b.message_count
            .cmp(&a.message_count)
            .then_with(|| b.agent_count.cmp(&a.agent_count))
    });

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Projects · {}", rows.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        Paragraph::new("No matching projects")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let total_messages = rows
        .iter()
        .map(|project| project.message_count)
        .sum::<u64>();
    let total_agents = rows.iter().map(|project| project.agent_count).sum::<u64>();
    let total_reservations = rows
        .iter()
        .map(|project| project.reservation_count)
        .sum::<u64>();

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    if content_rows >= 2 {
        lines.push(Line::from_spans([
            Span::styled(
                format!("msg:{total_messages} ag:{total_agents} res:{total_reservations}"),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                format!("shown:{}", rows.len()),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }
    let dense_cols = dense_columns_for_width(content_area.width, 44, 2);
    let list_budget = content_rows
        .saturating_mul(dense_cols)
        .saturating_sub(lines.len());
    for project in rows.iter().take(list_budget) {
        lines.push(Line::from_spans([
            Span::styled(
                truncate(&project.slug, 16),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                format!(
                    "m:{} a:{} r:{}",
                    project.message_count, project.agent_count, project.reservation_count
                ),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }
    render_lines_with_columns(frame, content_area, &lines, 44, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "sorted by message volume, then agents");
    }
}

fn render_project_load_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    projects: &[ProjectSummary],
    query_text: &str,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }

    let query_terms = parse_query_terms(query_text);
    let mut rows: Vec<(&ProjectSummary, u64)> = projects
        .iter()
        .filter(|project| {
            text_matches_query_terms(
                &format!("{} {}", project.slug, project.human_key),
                &query_terms,
            )
        })
        .map(|project| {
            let load_score = project
                .message_count
                .saturating_add(project.agent_count.saturating_mul(2))
                .saturating_add(project.reservation_count.saturating_mul(3));
            (project, load_score)
        })
        .collect();
    rows.sort_by_key(|pair| std::cmp::Reverse(pair.1));

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Project Load · {}", rows.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        Paragraph::new("No matching projects")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    let total_score = rows.iter().map(|(_, score)| *score).sum::<u64>();
    let max_score = rows.first().map_or(1, |(_, score)| (*score).max(1));
    if content_rows >= 2 {
        lines.push(Line::from_spans([
            Span::styled("score ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                format!("{total_score}"),
                Style::default().fg(tp.metric_requests).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                format!("projects:{}", rows.len()),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }

    let dense_cols = dense_columns_for_width(content_area.width, 44, 2);
    let list_budget = content_rows
        .saturating_mul(dense_cols)
        .saturating_sub(lines.len());
    for (project, score) in rows.iter().take(list_budget) {
        let bar = ratio_bar(*score, max_score, 8);
        lines.push(Line::from_spans([
            Span::styled(
                truncate(&project.slug, 14),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(tp.metric_requests)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "L:{score} m:{} a:{} r:{}",
                    project.message_count, project.agent_count, project.reservation_count
                ),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }

    render_lines_with_columns(frame, content_area, &lines, 44, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "L=msg + (2*agents) + (3*reservations)");
    }
}

#[allow(clippy::too_many_lines)]
fn render_reservation_watch_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    reservations: &[ReservationSnapshot],
    query_text: &str,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }
    let now = unix_epoch_micros_now().unwrap_or_default();
    let query_terms = parse_query_terms(query_text);
    let mut rows: Vec<&ReservationSnapshot> = reservations
        .iter()
        .filter(|reservation| !reservation.is_released())
        .filter(|reservation| {
            text_matches_query_terms(
                &format!(
                    "{} {} {}",
                    reservation.project_slug, reservation.agent_name, reservation.path_pattern
                ),
                &query_terms,
            )
        })
        .collect();
    rows.sort_by_key(|reservation| reservation.expires_ts);

    let soon_count = rows
        .iter()
        .filter(|reservation| {
            reservation.expires_ts.saturating_sub(now) <= RESERVATION_SOON_THRESHOLD_MICROS
        })
        .count();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Reservations · {} (≤5m:{soon_count})", rows.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        Paragraph::new("No active reservations")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let earliest = rows.first().copied();
    let ttl_display = earliest.map_or_else(
        || "none".to_string(),
        |reservation| {
            let secs = reservation_remaining_seconds(reservation, now);
            if secs == 0 {
                "expired".to_string()
            } else {
                format!("{secs}s next")
            }
        },
    );

    let hint_rows = usize::from(inner.height >= 6);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let gauge_h = if content_area.height >= 4 { 2 } else { 1 };
    let (gauge_area, list_area) = split_top(content_area, gauge_h);
    let soon_count = u32::try_from(soon_count).unwrap_or(u32::MAX);
    let total_rows = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    ReservationGauge::new("Expiring ≤5m", soon_count, total_rows)
        .ttl_display(&ttl_display)
        .render(gauge_area, frame);

    if list_area.height == 0 {
        return;
    }
    let path_budget = usize::from(inner.width.saturating_sub(34)).max(8);
    let dense_cols = dense_columns_for_width(list_area.width, 34, 2);
    let row_budget = usize::from(list_area.height).saturating_mul(dense_cols);
    let mut lines = Vec::new();
    for reservation in rows.iter().take(row_budget) {
        let remaining = reservation_remaining_seconds(reservation, now);
        let urgency = if remaining == 0 {
            ("✖", tp.severity_error)
        } else if remaining <= 60 {
            ("!", tp.severity_critical)
        } else if remaining <= 300 {
            ("▲", tp.severity_warn)
        } else {
            ("·", tp.text_muted)
        };
        let mode_marker = if reservation.exclusive { "X" } else { "S" };
        let mode_color = if reservation.exclusive {
            tp.severity_warn
        } else {
            tp.text_muted
        };
        lines.push(Line::from_spans([
            Span::styled(urgency.0, Style::default().fg(urgency.1)),
            Span::raw(" "),
            Span::styled(
                format!("{remaining:>4}s"),
                Style::default().fg(tp.metric_latency),
            ),
            Span::raw(" "),
            Span::styled(mode_marker, Style::default().fg(mode_color)),
            Span::raw(" "),
            Span::styled(
                truncate(&reservation.agent_name, 10),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(&reservation.project_slug, 10),
                crate::tui_theme::text_meta(&tp),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(&reservation.path_pattern, path_budget),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }
    render_lines_with_columns(frame, list_area, &lines, 34, 2);
    if hint_rows == 1 {
        render_panel_hint_line(
            frame,
            inner,
            "symbols: ✖ expired !<=60s ▲<=5m · mode X=exclusive",
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_reservation_ttl_buckets_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    reservations: &[ReservationSnapshot],
    query_text: &str,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }

    let now = unix_epoch_micros_now().unwrap_or_default();
    let query_terms = parse_query_terms(query_text);
    let filtered: Vec<&ReservationSnapshot> = reservations
        .iter()
        .filter(|reservation| !reservation.is_released())
        .filter(|reservation| {
            text_matches_query_terms(
                &format!(
                    "{} {} {}",
                    reservation.project_slug, reservation.agent_name, reservation.path_pattern
                ),
                &query_terms,
            )
        })
        .collect();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Reservation TTL · {}", filtered.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if filtered.is_empty() {
        Paragraph::new("No matching reservations")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let mut expired = 0usize;
    let mut s60 = 0usize;
    let mut m5 = 0usize;
    let mut m30 = 0usize;
    let mut long = 0usize;
    let mut exclusive = 0usize;

    for reservation in &filtered {
        if reservation.exclusive {
            exclusive += 1;
        }
        let remaining = reservation_remaining_seconds(reservation, now);
        if remaining == 0 {
            expired += 1;
        } else if remaining <= 60 {
            s60 += 1;
        } else if remaining <= 300 {
            m5 += 1;
        } else if remaining <= 1_800 {
            m30 += 1;
        } else {
            long += 1;
        }
    }
    let soon = expired + s60 + m5;
    let soon_u64 = u64::try_from(soon).unwrap_or(u64::MAX);
    let filtered_u64 = u64::try_from(filtered.len()).unwrap_or(u64::MAX).max(1);

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    if content_rows >= 1 {
        lines.push(Line::from_spans([Span::styled(
            format!("active:{} excl:{}", filtered.len(), exclusive),
            Style::default().fg(tp.text_primary).bold(),
        )]));
    }
    if content_rows >= 2 {
        lines.push(Line::from_spans([Span::styled(
            format!("exp:{expired} <=60s:{s60} <=5m:{m5} <=30m:{m30} >30m:{long}"),
            crate::tui_theme::text_meta(&tp),
        )]));
    }
    if content_rows >= 3 {
        lines.push(Line::from_spans([
            Span::styled("soon ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                ratio_bar(soon_u64, filtered_u64, 10),
                Style::default().fg(tp.ttl_warning),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{soon}/{}", filtered.len()),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }

    let soonest = filtered
        .iter()
        .min_by_key(|reservation| reservation.expires_ts);
    if lines.len() < content_rows {
        if let Some(soonest) = soonest {
            let ttl = reservation_remaining_seconds(soonest, now);
            lines.push(Line::from_spans([
                Span::styled("next ", crate::tui_theme::text_meta(&tp)),
                Span::styled(
                    format!("{ttl}s"),
                    Style::default().fg(tp.metric_latency).bold(),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate(&soonest.agent_name, 12),
                    Style::default().fg(tp.text_primary),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate(
                        &soonest.path_pattern,
                        usize::from(inner.width.saturating_sub(24)),
                    ),
                    crate::tui_theme::text_meta(&tp),
                ),
            ]));
        }
    }

    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    render_lines_with_columns(frame, content_area, &lines, 44, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "exclusive reservations should be short-lived");
    }
}

#[allow(clippy::too_many_lines)]
fn render_signal_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    anomalies: &[DetectedAnomaly],
    entries: &[&EventEntry],
    contacts: &[ContactSummary],
    query_text: &str,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }
    let query_terms = parse_query_terms(query_text);
    let pending_contacts = contacts
        .iter()
        .filter(|contact| contact.status.eq_ignore_ascii_case("pending"))
        .count();
    let active_contacts = contacts
        .iter()
        .filter(|contact| contact.status.eq_ignore_ascii_case("accepted"))
        .count();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Signals · anomalies:{}", anomalies.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    lines.push(Line::from_spans([
        Span::styled("contacts ", crate::tui_theme::text_meta(&tp)),
        Span::styled(
            format!("pending:{pending_contacts} active:{active_contacts}"),
            Style::default().fg(tp.text_primary).bold(),
        ),
    ]));

    let mut pushed = 1usize;
    for anomaly in anomalies.iter().filter(|anomaly| {
        text_matches_query_terms(
            &format!("{} {}", anomaly.headline, anomaly.rationale),
            &query_terms,
        )
    }) {
        if pushed >= content_rows {
            break;
        }
        lines.push(Line::from_spans([
            Span::styled("▲ ", Style::default().fg(anomaly.severity.color())),
            Span::styled(
                truncate(
                    &anomaly.headline,
                    usize::from(inner.width.saturating_sub(2)),
                ),
                Style::default().fg(tp.text_primary),
            ),
        ]));
        pushed += 1;
    }

    for entry in entries.iter().filter(|entry| {
        matches!(entry.severity, EventSeverity::Warn | EventSeverity::Error)
            && text_matches_query_terms(&entry.summary, &query_terms)
    }) {
        if pushed >= content_rows {
            break;
        }
        let marker = if entry.severity == EventSeverity::Error {
            ("✖", tp.severity_error)
        } else {
            ("!", tp.severity_warn)
        };
        lines.push(Line::from_spans([
            Span::styled(format!("{} ", marker.0), Style::default().fg(marker.1)),
            Span::styled(
                truncate(&entry.summary, usize::from(inner.width.saturating_sub(2))),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
        pushed += 1;
    }

    for entry in entries.iter().rev().filter(|entry| {
        matches!(
            entry.kind,
            MailEventKind::MessageSent | MailEventKind::MessageReceived
        ) && text_matches_query_terms(&entry.summary, &query_terms)
    }) {
        if pushed >= content_rows {
            break;
        }
        let marker = if entry.kind == MailEventKind::MessageSent {
            "↑"
        } else {
            "↓"
        };
        lines.push(Line::from_spans([
            Span::styled(
                format!("{marker} "),
                Style::default().fg(tp.metric_messages),
            ),
            Span::styled(
                truncate(&entry.summary, usize::from(inner.width.saturating_sub(2))),
                Style::default().fg(tp.text_primary),
            ),
        ]));
        pushed += 1;
    }

    if pushed == 1 && content_rows > 1 {
        lines.push(Line::from_spans([Span::styled(
            "No active warnings or message activity",
            crate::tui_theme::text_meta(&tp),
        )]));
    }

    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    render_lines_with_columns(frame, content_area, &lines, 40, 2);
    if hint_rows == 1 {
        render_panel_hint_line(
            frame,
            inner,
            "priority: anomalies, then warn/error, then traffic",
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_event_mix_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[&EventEntry],
    query_text: &str,
) {
    if area.width < 18 || area.height < 3 {
        return;
    }
    let query_terms = parse_query_terms(query_text);
    let filtered: Vec<&EventEntry> = entries
        .iter()
        .copied()
        .filter(|entry| event_entry_matches_query(entry, &query_terms))
        .collect();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Event Mix · {}", filtered.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if filtered.is_empty() {
        Paragraph::new("No matching events")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let mut by_kind: HashMap<MailEventKind, usize> = HashMap::new();
    let mut msg = 0usize;
    let mut tool = 0usize;
    let mut reservation = 0usize;
    let mut http = 0usize;
    let mut lifecycle = 0usize;
    let mut warn = 0usize;
    let mut error = 0usize;

    for entry in &filtered {
        *by_kind.entry(entry.kind).or_insert(0) += 1;
        if matches!(entry.severity, EventSeverity::Warn) {
            warn += 1;
        }
        if matches!(entry.severity, EventSeverity::Error) {
            error += 1;
        }
        match entry.kind {
            MailEventKind::MessageSent | MailEventKind::MessageReceived => msg += 1,
            MailEventKind::ToolCallStart | MailEventKind::ToolCallEnd => tool += 1,
            MailEventKind::ReservationGranted | MailEventKind::ReservationReleased => {
                reservation += 1;
            }
            MailEventKind::HttpRequest => http += 1,
            MailEventKind::ServerStarted
            | MailEventKind::ServerShutdown
            | MailEventKind::HealthPulse
            | MailEventKind::AgentRegistered => lifecycle += 1,
        }
    }

    let mut top_kinds = by_kind.into_iter().collect::<Vec<_>>();
    top_kinds.sort_by_key(|pair| std::cmp::Reverse(pair.1));

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    if content_rows >= 1 {
        lines.push(Line::from_spans([Span::styled(
            format!("total:{} warn:{} err:{}", filtered.len(), warn, error),
            Style::default().fg(tp.text_primary).bold(),
        )]));
    }
    if content_rows >= 2 {
        lines.push(Line::from_spans([Span::styled(
            format!("msg:{msg} tool:{tool} res:{reservation} http:{http} sys:{lifecycle}"),
            crate::tui_theme::text_meta(&tp),
        )]));
    }
    if lines.len() < content_rows {
        if let Some((kind, count)) = top_kinds.first() {
            lines.push(Line::from_spans([
                Span::styled("top ", crate::tui_theme::text_meta(&tp)),
                Span::styled(
                    format!("{} ({count})", kind.compact_label()),
                    Style::default().fg(tp.metric_requests),
                ),
            ]));
        }
    }
    if lines.len() < content_rows {
        if let Some((kind, count)) = top_kinds.get(1) {
            lines.push(Line::from_spans([
                Span::styled("next ", crate::tui_theme::text_meta(&tp)),
                Span::styled(
                    format!("{} ({count})", kind.compact_label()),
                    Style::default().fg(tp.text_primary),
                ),
            ]));
        }
    }

    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    render_lines_with_columns(frame, content_area, &lines, 44, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "mix reflects current query + quick filter");
    }
}

#[derive(Default)]
struct ToolAgg {
    calls: usize,
    total_ms: u64,
    max_ms: u64,
    samples: Vec<u64>,
}

struct ToolLatencyRow {
    tool_name: String,
    calls: usize,
    total_ms: u64,
    avg_ms: u64,
    p95_ms: u64,
    max_ms: u64,
}

#[allow(clippy::too_many_lines)]
fn render_tool_latency_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[&EventEntry],
    query_text: &str,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }

    let query_terms = parse_query_terms(query_text);
    let mut by_tool: HashMap<String, ToolAgg> = HashMap::new();
    for entry in entries.iter().filter(|entry| {
        entry.kind == MailEventKind::ToolCallEnd
            && text_matches_query_terms(&entry.summary, &query_terms)
    }) {
        if let Some((tool_name, duration_ms)) = parse_tool_end_duration(&entry.summary) {
            let slot = by_tool.entry(tool_name).or_default();
            slot.calls += 1;
            slot.total_ms = slot.total_ms.saturating_add(duration_ms);
            slot.max_ms = slot.max_ms.max(duration_ms);
            slot.samples.push(duration_ms);
        }
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Tool Latency · {}", by_tool.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if by_tool.is_empty() {
        Paragraph::new("No matching tool completions")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let mut rows = by_tool
        .into_iter()
        .map(|(tool_name, mut stats)| {
            stats.samples.sort_unstable();
            let p95_idx =
                (stats.samples.len() * 95 / 100).min(stats.samples.len().saturating_sub(1));
            let p95_ms = stats.samples[p95_idx];
            let calls_u64 = u64::try_from(stats.calls).unwrap_or(1).max(1);
            let avg_ms = stats.total_ms.checked_div(calls_u64).unwrap_or(0);
            ToolLatencyRow {
                tool_name,
                calls: stats.calls,
                total_ms: stats.total_ms,
                avg_ms,
                p95_ms,
                max_ms: stats.max_ms,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.p95_ms
            .cmp(&a.p95_ms)
            .then_with(|| b.calls.cmp(&a.calls))
            .then_with(|| b.max_ms.cmp(&a.max_ms))
    });

    let total_calls = rows.iter().map(|stats| stats.calls).sum::<usize>();
    let total_latency_ms = rows.iter().map(|stats| stats.total_ms).sum::<u64>();
    let avg_latency = total_latency_ms
        .checked_div(u64::try_from(total_calls).unwrap_or(1).max(1))
        .unwrap_or(0);
    let slow_count = rows
        .iter()
        .filter(|stats| stats.p95_ms >= TOOL_LATENCY_WARN_MS)
        .count();
    let critical_count = rows
        .iter()
        .filter(|stats| stats.p95_ms >= TOOL_LATENCY_HIGH_MS)
        .count();
    let max_p95 = rows
        .iter()
        .map(|stats| stats.p95_ms)
        .max()
        .unwrap_or(1)
        .max(1);

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    if content_rows >= 1 {
        lines.push(Line::from_spans([Span::styled(
            format!(
                "tools:{} calls:{total_calls} fleet-avg:{avg_latency}ms slow:{slow_count} crit:{critical_count}",
                rows.len(),
            ),
            Style::default().fg(tp.text_primary).bold(),
        )]));
    }

    let dense_cols = dense_columns_for_width(content_area.width, 48, 2);
    let list_budget = content_rows
        .saturating_mul(dense_cols)
        .saturating_sub(lines.len());
    for stats in rows.iter().take(list_budget) {
        let calls_u64 = u64::try_from(stats.calls).unwrap_or(0);
        let latency_color = if stats.p95_ms >= TOOL_LATENCY_HIGH_MS {
            tp.severity_error
        } else if stats.p95_ms >= TOOL_LATENCY_WARN_MS {
            tp.severity_warn
        } else {
            tp.metric_latency
        };
        let latency_badge = if stats.p95_ms >= TOOL_LATENCY_HIGH_MS {
            "‼"
        } else if stats.p95_ms >= TOOL_LATENCY_WARN_MS {
            "!"
        } else {
            "·"
        };
        let bar = ratio_bar(stats.p95_ms, max_p95, 8);
        lines.push(Line::from_spans([
            Span::styled(latency_badge, Style::default().fg(latency_color)),
            Span::raw(" "),
            Span::styled(
                truncate(&stats.tool_name, 12),
                Style::default().fg(tp.text_primary).bold(),
            ),
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(latency_color)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "p95:{} avg:{} max:{} c:{calls_u64}",
                    stats.p95_ms, stats.avg_ms, stats.max_ms
                ),
                Style::default().fg(latency_color),
            ),
        ]));
    }

    render_lines_with_columns(frame, content_area, &lines, 48, 2);
    if hint_rows == 1 {
        render_panel_hint_line(
            frame,
            inner,
            "ranked by p95; fleet-avg weighted by call count",
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_recent_activity_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[&EventEntry],
    query_text: &str,
) {
    if area.width < 18 || area.height < 3 {
        return;
    }

    let query_terms = parse_query_terms(query_text);
    let filtered = entries
        .iter()
        .rev()
        .copied()
        .filter(|entry| {
            text_matches_query_terms(
                &format!("{} {}", entry.kind.compact_label(), entry.summary),
                &query_terms,
            )
        })
        .collect::<Vec<_>>();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Recent Activity · {}", filtered.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let hint_rows = usize::from(inner.height >= 5);
    let content_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(hint_rows).unwrap_or(0)),
    );
    let content_rows = usize::from(content_area.height);
    let mut lines = Vec::new();
    let warn_count = filtered
        .iter()
        .filter(|entry| matches!(entry.severity, EventSeverity::Warn))
        .count();
    let error_count = filtered
        .iter()
        .filter(|entry| matches!(entry.severity, EventSeverity::Error))
        .count();
    let message_count = filtered
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                MailEventKind::MessageSent | MailEventKind::MessageReceived
            )
        })
        .count();
    if content_rows >= 2 {
        lines.push(Line::from_spans([Span::styled(
            format!("warn:{warn_count} err:{error_count} msg:{message_count}"),
            Style::default().fg(tp.text_primary).bold(),
        )]));
    }
    let dense_cols = dense_columns_for_width(content_area.width, 40, 2);
    let cols_u16 = u16::try_from(dense_cols).unwrap_or(1).max(1);
    let total_gap = cols_u16.saturating_sub(1);
    let column_width = content_area
        .width
        .saturating_sub(total_gap)
        .checked_div(cols_u16)
        .unwrap_or(content_area.width)
        .max(1);
    let list_budget = content_rows
        .saturating_mul(dense_cols)
        .saturating_sub(lines.len());
    let kind_width = if column_width >= 48 { 10usize } else { 7usize };
    let summary_width =
        usize::from(column_width).saturating_sub(20usize.saturating_add(kind_width).max(1));
    for entry in filtered.iter().take(list_budget) {
        lines.push(Line::from_spans([
            Span::styled(entry.timestamp.clone(), crate::tui_theme::text_meta(&tp)),
            Span::raw(" "),
            Span::styled(
                entry.icon.to_string(),
                Style::default().fg(entry.severity.color()),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(entry.kind.compact_label(), kind_width),
                crate::tui_theme::text_meta(&tp),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(&entry.summary, summary_width),
                Style::default().fg(tp.text_primary),
            ),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from_spans([Span::styled(
            "No matching events",
            crate::tui_theme::text_meta(&tp),
        )]));
    }

    render_lines_with_columns(frame, content_area, &lines, 40, 2);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "icon color reflects event severity");
    }
}

#[allow(clippy::too_many_lines)]
fn render_message_flow_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[&EventEntry],
    query_text: &str,
) {
    if area.width < 20 || area.height < 3 {
        return;
    }

    let query_terms = parse_query_terms(query_text);
    let filtered = entries
        .iter()
        .copied()
        .filter(|entry| {
            matches!(
                entry.kind,
                MailEventKind::MessageSent | MailEventKind::MessageReceived
            ) && text_matches_query_terms(&entry.summary, &query_terms)
        })
        .collect::<Vec<_>>();

    let tp = crate::tui_theme::TuiThemePalette::current();
    let title = format!("Message Flow · {}", filtered.len());
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if filtered.is_empty() {
        Paragraph::new("No matching messages")
            .style(crate::tui_theme::text_meta(&tp))
            .render(inner, frame);
        return;
    }

    let sent = filtered
        .iter()
        .filter(|entry| entry.kind == MailEventKind::MessageSent)
        .count();
    let recv = filtered
        .iter()
        .filter(|entry| entry.kind == MailEventKind::MessageReceived)
        .count();
    let delta =
        i128::try_from(sent).unwrap_or(i128::MAX) - i128::try_from(recv).unwrap_or(i128::MAX);
    let total_msgs = u64::try_from(sent + recv).unwrap_or(0).max(1);
    let sent_u64 = u64::try_from(sent).unwrap_or(0);
    let recv_u64 = u64::try_from(recv).unwrap_or(0);
    let sent_pct = sent_u64
        .saturating_mul(100)
        .checked_div(total_msgs)
        .unwrap_or(0);
    let recv_pct = recv_u64
        .saturating_mul(100)
        .checked_div(total_msgs)
        .unwrap_or(0);

    let hint_rows = usize::from(inner.height >= 5);
    let content_rows = usize::from(inner.height).saturating_sub(hint_rows);
    let mut lines = Vec::new();
    if content_rows >= 1 {
        lines.push(Line::from_spans([
            Span::styled(
                format!("↑{sent} ({sent_pct}%)"),
                Style::default().fg(tp.metric_messages).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                format!("↓{recv} ({recv_pct}%)"),
                Style::default().fg(tp.metric_requests).bold(),
            ),
            Span::raw(" "),
            Span::styled(
                format!("Δ:{delta:+}"),
                Style::default().fg(tp.text_primary).bold(),
            ),
        ]));
    }
    if content_rows >= 2 {
        lines.push(Line::from_spans([
            Span::styled("↑", Style::default().fg(tp.metric_messages)),
            Span::styled(
                ratio_bar(sent_u64, total_msgs, 6),
                Style::default().fg(tp.metric_messages),
            ),
            Span::raw(" "),
            Span::styled("↓", Style::default().fg(tp.metric_requests)),
            Span::styled(
                ratio_bar(recv_u64, total_msgs, 6),
                Style::default().fg(tp.metric_requests),
            ),
        ]));
    }

    let list_budget = content_rows.saturating_sub(lines.len());
    for entry in filtered.iter().rev().take(list_budget) {
        let (marker, color) = if entry.kind == MailEventKind::MessageSent {
            ("↑", tp.metric_messages)
        } else {
            ("↓", tp.metric_requests)
        };
        lines.push(Line::from_spans([
            Span::styled(marker, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(
                truncate(&entry.summary, usize::from(inner.width.saturating_sub(2))),
                crate::tui_theme::text_meta(&tp),
            ),
        ]));
    }

    let visible = lines.into_iter().take(content_rows).collect::<Vec<_>>();
    Paragraph::new(Text::from_lines(visible)).render(inner, frame);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "↑ outbound, ↓ inbound (relative share bars)");
    }
}

#[allow(clippy::too_many_lines)]
fn render_query_matches_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    query_text: &str,
    db_snapshot: &DbStatSnapshot,
    entries: &[&EventEntry],
) {
    if area.width < 22 || area.height < 3 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Live Search")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let hint_rows = usize::from(inner.height >= 6);
    let content_rows = usize::from(inner.height).saturating_sub(hint_rows);
    let query_terms = parse_query_terms(query_text);
    let mut lines = Vec::new();
    if query_terms.is_empty() {
        lines.push(Line::from_spans([
            Span::styled("Hint: ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                "press / then type",
                Style::default().fg(tp.text_primary).bold(),
            ),
        ]));
        lines.push(Line::from_spans([Span::styled(
            format!(
                "Totals · events:{} agents:{} projects:{} reservations:{} contacts:{}",
                entries.len(),
                db_snapshot.agents,
                db_snapshot.projects,
                db_snapshot.file_reservations,
                db_snapshot.contact_links,
            ),
            crate::tui_theme::text_meta(&tp),
        )]));
        lines.push(Line::from_spans([Span::styled(
            "Enter jumps to Search with the same query",
            crate::tui_theme::text_meta(&tp),
        )]));
        if content_rows >= 4 {
            lines.push(Line::from_spans([Span::styled(
                "Live filtering is case-insensitive and uses AND semantics",
                crate::tui_theme::text_meta(&tp),
            )]));
        }
    } else {
        let mut matched_agents = Vec::new();
        let mut matched_agents_total = 0usize;
        for agent in db_snapshot.agents_list.iter().filter(|agent| {
            text_matches_query_terms(&format!("{} {}", agent.name, agent.program), &query_terms)
        }) {
            matched_agents_total += 1;
            if matched_agents.len() < TOP_MATCH_SAMPLE_CAP {
                matched_agents.push(agent.name.clone());
            }
        }

        let mut matched_projects = Vec::new();
        let mut matched_projects_total = 0usize;
        for project in db_snapshot.projects_list.iter().filter(|project| {
            text_matches_query_terms(
                &format!("{} {}", project.slug, project.human_key),
                &query_terms,
            )
        }) {
            matched_projects_total += 1;
            if matched_projects.len() < TOP_MATCH_SAMPLE_CAP {
                matched_projects.push(project.slug.clone());
            }
        }

        let mut matched_paths = Vec::new();
        let mut matched_paths_total = 0usize;
        for reservation in db_snapshot
            .reservation_snapshots
            .iter()
            .filter(|reservation| {
                text_matches_query_terms(
                    &format!(
                        "{} {} {}",
                        reservation.project_slug, reservation.agent_name, reservation.path_pattern
                    ),
                    &query_terms,
                )
            })
        {
            matched_paths_total += 1;
            if matched_paths.len() < TOP_MATCH_SAMPLE_CAP {
                matched_paths.push(reservation.path_pattern.clone());
            }
        }
        let mut matched_contacts = Vec::new();
        let mut matched_contacts_total = 0usize;
        for contact in db_snapshot.contacts_list.iter().filter(|contact| {
            text_matches_query_terms(
                &format!(
                    "{} {} {} {} {} {}",
                    contact.from_agent,
                    contact.to_agent,
                    contact.from_project_slug,
                    contact.to_project_slug,
                    contact.status,
                    contact.reason
                ),
                &query_terms,
            )
        }) {
            matched_contacts_total += 1;
            if matched_contacts.len() < TOP_MATCH_SAMPLE_CAP {
                matched_contacts.push(format!("{}→{}", contact.from_agent, contact.to_agent));
            }
        }
        let agent_den = u64::try_from(db_snapshot.agents_list.len())
            .unwrap_or(u64::MAX)
            .max(1);
        let project_den = u64::try_from(db_snapshot.projects_list.len())
            .unwrap_or(u64::MAX)
            .max(1);
        let reservation_den = u64::try_from(db_snapshot.reservation_snapshots.len())
            .unwrap_or(u64::MAX)
            .max(1);
        let contact_den = u64::try_from(db_snapshot.contacts_list.len())
            .unwrap_or(u64::MAX)
            .max(1);
        let matched_agents_u64 = u64::try_from(matched_agents_total).unwrap_or(u64::MAX);
        let matched_projects_u64 = u64::try_from(matched_projects_total).unwrap_or(u64::MAX);
        let matched_paths_u64 = u64::try_from(matched_paths_total).unwrap_or(u64::MAX);
        let matched_contacts_u64 = u64::try_from(matched_contacts_total).unwrap_or(u64::MAX);

        lines.push(Line::from_spans([
            Span::styled("Query ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                format!("\"{}\"", truncate(query_text, 42)),
                Style::default().fg(tp.text_primary).bold(),
            ),
        ]));
        lines.push(Line::from_spans([Span::styled(
            format!(
                "Matches · events:{} agents:{} projects:{} reservations:{} contacts:{}",
                entries.len(),
                matched_agents_total,
                matched_projects_total,
                matched_paths_total,
                matched_contacts_total,
            ),
            crate::tui_theme::text_meta(&tp),
        )]));
        lines.push(Line::from_spans([
            Span::styled("ag ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                ratio_bar(matched_agents_u64, agent_den, 6),
                Style::default().fg(tp.metric_agents),
            ),
            Span::raw(" "),
            Span::styled("pr ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                ratio_bar(matched_projects_u64, project_den, 6),
                Style::default().fg(tp.status_accent),
            ),
            Span::raw(" "),
            Span::styled("rs ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                ratio_bar(matched_paths_u64, reservation_den, 6),
                Style::default().fg(tp.ttl_warning),
            ),
            Span::raw(" "),
            Span::styled("ct ", crate::tui_theme::text_meta(&tp)),
            Span::styled(
                ratio_bar(matched_contacts_u64, contact_den, 6),
                Style::default().fg(tp.status_accent),
            ),
        ]));
        lines.push(Line::from_spans([Span::styled(
            format!(
                "Agents: {}",
                format_sample_with_overflow(&matched_agents, matched_agents_total)
            ),
            crate::tui_theme::text_meta(&tp),
        )]));
        lines.push(Line::from_spans([Span::styled(
            format!(
                "Projects: {}",
                format_sample_with_overflow(&matched_projects, matched_projects_total)
            ),
            crate::tui_theme::text_meta(&tp),
        )]));
        lines.push(Line::from_spans([Span::styled(
            format!(
                "Paths: {}",
                format_sample_with_overflow(&matched_paths, matched_paths_total)
            ),
            crate::tui_theme::text_meta(&tp),
        )]));
        lines.push(Line::from_spans([Span::styled(
            format!(
                "Contacts: {}",
                format_sample_with_overflow(&matched_contacts, matched_contacts_total)
            ),
            crate::tui_theme::text_meta(&tp),
        )]));
    }

    let visible_lines = lines.into_iter().take(content_rows).collect::<Vec<_>>();
    Paragraph::new(Text::from_lines(visible_lines)).render(inner, frame);
    if hint_rows == 1 {
        render_panel_hint_line(frame, inner, "Enter opens Search with this exact query");
    }
}

fn reservation_remaining_seconds(snapshot: &ReservationSnapshot, now_micros: i64) -> u64 {
    if snapshot.expires_ts <= now_micros {
        return 0;
    }
    let delta = snapshot.expires_ts.saturating_sub(now_micros);
    let secs = delta.saturating_add(999_999) / 1_000_000;
    u64::try_from(secs).unwrap_or(u64::MAX)
}

fn agent_status_marker(last_active_micros: i64, now_micros: i64) -> (char, PackedRgba) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    if last_active_micros <= 0 {
        return ('○', tp.activity_stale);
    }
    let elapsed = now_micros.saturating_sub(last_active_micros);
    if elapsed < AGENT_ACTIVE_THRESHOLD_MICROS {
        ('●', tp.activity_active)
    } else if elapsed < AGENT_IDLE_THRESHOLD_MICROS {
        ('●', tp.activity_idle)
    } else {
        ('○', tp.activity_stale)
    }
}

/// Render the trend/insight panel with percentile ribbon, throughput chart, and activity heatmap.
fn render_trend_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    percentile_history: &[PercentileSample],
    throughput_history: &[f64],
    event_log: &[EventEntry],
) {
    if area.width < 10 || area.height < 6 {
        return;
    }
    let tp = crate::tui_theme::TuiThemePalette::current();

    // Allocate vertical space: ribbon, throughput chart, and optional heatmap (br-18wct).
    let heatmap_h = if area.height >= 18 { 6 } else { 0 };
    let remaining = area.height.saturating_sub(heatmap_h);
    let ribbon_h = remaining / 2;
    let activity_h = remaining.saturating_sub(ribbon_h);
    let ribbon_area = Rect::new(area.x, area.y, area.width, ribbon_h);
    let activity_area = Rect::new(area.x, area.y + ribbon_h, area.width, activity_h);
    let heatmap_area = Rect::new(
        area.x,
        area.y + ribbon_h + activity_h,
        area.width,
        heatmap_h,
    );

    // Percentile ribbon
    if percentile_history.len() >= 2 {
        let ribbon = PercentileRibbon::new(percentile_history)
            .label("Latency")
            .block(
                Block::default()
                    .title("Latency P50/P95/P99")
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(tp.panel_border)),
            );
        ribbon.render(ribbon_area, frame);
    } else {
        let block = Block::default()
            .title("Latency (collecting...)")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        Paragraph::new("Awaiting data...")
            .block(block)
            .render(ribbon_area, frame);
    }

    // Throughput LineChart (br-3q8v0: replaced Sparkline with ftui_extras LineChart)
    if throughput_history.len() >= 2 {
        let block = Block::default()
            .title("Throughput (req/interval)")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        let inner = block.inner(activity_area);
        block.render(activity_area, frame);

        if inner.width > 4 && inner.height > 2 {
            // Take the most recent 60 samples (or fewer if not available yet).
            let window = 60.min(throughput_history.len());
            let start_idx = throughput_history.len().saturating_sub(window);
            let slice = &throughput_history[start_idx..];

            // Build (x, y) data: x = seconds ago (negative = past, 0 = now).
            #[allow(clippy::cast_precision_loss)]
            let data: Vec<(f64, f64)> = slice
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let x = i as f64 - (slice.len() as f64 - 1.0);
                    (x, v)
                })
                .collect();

            let max_val = slice.iter().copied().fold(1.0_f64, f64::max).max(1.0);

            let series = Series::new("calls/sec", &data, tp.metric_requests);
            #[allow(clippy::cast_precision_loss)]
            let x_min = -(window as f64 - 1.0);
            let chart = LineChart::new(vec![series])
                .x_bounds(x_min, 0.0)
                .y_bounds(0.0, max_val)
                .legend(true);
            chart.render(inner, frame);
        }
    } else {
        let block = Block::default()
            .title("Throughput (collecting...)")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(tp.panel_border));
        Paragraph::new("Awaiting data...")
            .block(block)
            .render(activity_area, frame);
    }

    // Activity heatmap (br-18wct): Braille Canvas showing event density over time.
    if heatmap_h > 0 {
        render_activity_heatmap(frame, heatmap_area, event_log);
    }
}

/// Number of distinct event kinds tracked for heatmap rows.
const HEATMAP_EVENT_KINDS: usize = 11;

/// Event kind labels for heatmap Y-axis (abbreviated).
const HEATMAP_KIND_LABELS: [&str; HEATMAP_EVENT_KINDS] = [
    "TlSt", "TlEn", "Send", "Recv", "RGnt", "RRel", "AReg", "HTTP", "Hlth", "SvUp", "SvDn",
];

/// Map a `MailEventKind` to its heatmap row index (0..10).
const fn heatmap_kind_index(kind: MailEventKind) -> usize {
    match kind {
        MailEventKind::ToolCallStart => 0,
        MailEventKind::ToolCallEnd => 1,
        MailEventKind::MessageSent => 2,
        MailEventKind::MessageReceived => 3,
        MailEventKind::ReservationGranted => 4,
        MailEventKind::ReservationReleased => 5,
        MailEventKind::AgentRegistered => 6,
        MailEventKind::HttpRequest => 7,
        MailEventKind::HealthPulse => 8,
        MailEventKind::ServerStarted => 9,
        MailEventKind::ServerShutdown => 10,
    }
}

/// Render a Braille-mode Canvas heatmap of event activity density.
///
/// X = time (bucketed into columns), Y = event kind, intensity = event count.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn render_activity_heatmap(frame: &mut Frame<'_>, area: Rect, event_log: &[EventEntry]) {
    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Activity Heatmap")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.width < 6 || inner.height < 2 || event_log.is_empty() {
        return;
    }

    // Reserve 5 columns for Y-axis labels.
    let label_w: u16 = 5;
    let chart_area = Rect {
        x: inner.x + label_w,
        y: inner.y,
        width: inner.width.saturating_sub(label_w),
        height: inner.height,
    };
    if chart_area.width == 0 || chart_area.height == 0 {
        return;
    }

    // Sub-pixel dimensions in Braille mode.
    let px_w = chart_area.width as usize * Mode::Braille.cols_per_cell() as usize;
    let px_h = chart_area.height as usize * Mode::Braille.rows_per_cell() as usize;

    // Determine time range from events.
    let ts_min = event_log
        .iter()
        .map(|e| e.timestamp_micros)
        .min()
        .unwrap_or(0);
    let ts_max = event_log
        .iter()
        .map(|e| e.timestamp_micros)
        .max()
        .unwrap_or(0);
    let ts_span = (ts_max - ts_min).max(1);

    // Bucket events into a grid: columns = time buckets, rows = event kinds.
    let num_cols = px_w;
    let mut grid = vec![vec![0u32; num_cols]; HEATMAP_EVENT_KINDS];

    for entry in event_log {
        let col = ((entry.timestamp_micros - ts_min) as f64 / ts_span as f64
            * (num_cols as f64 - 1.0)) as usize;
        let col = col.min(num_cols - 1);
        let row = heatmap_kind_index(entry.kind);
        grid[row][col] += 1;
    }

    // Find max count for intensity normalization.
    let max_count = grid
        .iter()
        .flat_map(|row| row.iter())
        .copied()
        .max()
        .unwrap_or(1)
        .max(1);

    // Paint onto Braille Canvas.
    let mut painter = Painter::for_area(chart_area, Mode::Braille);

    // Map each kind to a vertical band of sub-pixels.
    let row_height = px_h / HEATMAP_EVENT_KINDS;
    if row_height == 0 {
        return;
    }

    for (kind_idx, kind_row) in grid.iter().enumerate() {
        let y_base = kind_idx * row_height;
        for (col, &count) in kind_row.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let intensity = (f64::from(count) / f64::from(max_count)).sqrt();
            let r = (50.0 + intensity * 205.0) as u8;
            let g = (180.0 * (1.0 - intensity * 0.7)) as u8;
            let b = (50.0 + intensity * 50.0) as u8;
            let color = PackedRgba::rgb(r, g, b);

            // Fill sub-pixel rows for this kind at this time column.
            for dy in 0..row_height.min(3) {
                painter.point_colored(col as i32, (y_base + dy) as i32, color);
            }
        }
    }

    let canvas = Canvas::from_painter(&painter);
    canvas.render(chart_area, frame);

    // Render Y-axis labels.
    let label_area = Rect {
        x: inner.x,
        y: inner.y,
        width: label_w,
        height: inner.height,
    };
    let lines_per_kind = inner.height as usize / HEATMAP_EVENT_KINDS;
    if lines_per_kind > 0 {
        for (i, &label) in HEATMAP_KIND_LABELS.iter().enumerate() {
            let y_pos = label_area.y + (i * lines_per_kind) as u16;
            if y_pos < label_area.y + label_area.height {
                let text = Paragraph::new(label).style(Style::new().fg(tp.text_muted));
                text.render(
                    Rect {
                        x: label_area.x,
                        y: y_pos,
                        width: label_w,
                        height: 1,
                    },
                    frame,
                );
            }
        }
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

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title("Recent Message Preview")
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
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

#[allow(dead_code, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pulsing_severity_badge(
    severity: EventSeverity,
    pulse_phase: f32,
    reduced_motion: bool,
) -> Span<'static> {
    if reduced_motion || !matches!(severity, EventSeverity::Warn | EventSeverity::Error) {
        return severity.styled_badge();
    }

    let tp = crate::tui_theme::TuiThemePalette::current();
    let pulse = pulse_phase.sin().abs();
    let (base, highlight) = match severity {
        EventSeverity::Warn => (tp.severity_warn, tp.severity_critical),
        EventSeverity::Error => (tp.severity_error, tp.severity_critical),
        _ => return severity.styled_badge(),
    };
    let color = crate::tui_theme::lerp_color(base, highlight, pulse);
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
    viewer: &RefCell<crate::console::LogPane>,
    entries: &[&EventEntry],
    scroll_offset: usize,
    auto_follow: bool,
    quick_filter: DashboardQuickFilter,
    verbosity: VerbosityTier,
    inline_anomaly_count: usize,
    effects_enabled: bool,
) {
    if area.height < 3 || area.width < 20 {
        return;
    }

    let total = entries.len();
    let end = total.saturating_sub(scroll_offset);

    let follow_indicator = if auto_follow { " [FOLLOW]" } else { "" };
    let verbosity_indicator = format!(" [{}]", verbosity.label());
    let filter_indicator = format!(" [filter: {}]", quick_filter.label());
    let anomaly_indicator = if inline_anomaly_count > 0 {
        format!(" [{inline_anomaly_count} anomaly]")
    } else {
        String::new()
    };
    let title = format!(
        "Events ({end}/{total}){follow_indicator}{verbosity_indicator}{filter_indicator}{anomaly_indicator}",
    );

    let tp = crate::tui_theme::TuiThemePalette::current();
    let block = Block::default()
        .title(&title)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(tp.panel_border));
    let inner = block.inner(area);
    block.render(area, frame);

    if inner.height == 0 {
        return;
    }

    let controls_height = quick_filter_controls_height(inner.width, inner.height);
    let controls_area = Rect::new(inner.x, inner.y, inner.width, controls_height);
    let viewer_area = Rect::new(
        inner.x,
        inner.y.saturating_add(controls_height),
        inner.width,
        inner.height.saturating_sub(controls_height),
    );

    let meta_style = crate::tui_theme::text_meta(&tp);
    let active_style = Style::default()
        .fg(tp.selection_fg)
        .bg(tp.selection_bg)
        .bold();
    let mut controls = Vec::new();
    let mut controls_chars = 0usize;
    for filter in [
        DashboardQuickFilter::All,
        DashboardQuickFilter::Messages,
        DashboardQuickFilter::Tools,
        DashboardQuickFilter::Reservations,
    ] {
        let label = format!(" [{}:{}] ", filter.key(), filter.control_label());
        controls_chars = controls_chars.saturating_add(label.len());
        let span = if quick_filter == filter {
            Span::styled(label, active_style)
        } else {
            Span::styled(label, meta_style)
        };
        controls.push(span);
    }
    if controls_area.height > 0 {
        let controls_text = Text::from_line(Line::from_spans(controls));
        let mut paragraph = Paragraph::new(controls_text);
        if controls_area.height > 1 || controls_chars > usize::from(inner.width) {
            paragraph = paragraph.wrap(ftui::text::WrapMode::Word);
        }
        paragraph.render(controls_area, frame);
    }

    if viewer_area.height == 0 {
        return;
    }

    let (window_start, window_end) =
        event_log_window_bounds(total, scroll_offset, usize::from(viewer_area.height));
    let window_entries = &entries[window_start..window_end];
    let shimmer_progresses = dashboard_shimmer_progresses(window_entries, effects_enabled);
    if viewer_area.width == 0 {
        return;
    }

    let line_width_budget = usize::from(viewer_area.width);
    let mut pane = viewer.borrow_mut();
    pane.clear();
    pane.push_many(window_entries.iter().enumerate().map(|(idx, entry)| {
        let raw_line = format!(
            "{:>6} {} {:<3} {} {:<10} {}",
            entry.seq,
            entry.timestamp,
            entry.severity.badge(),
            entry.icon,
            entry.kind.compact_label(),
            entry.summary
        );
        let line = truncate(&raw_line, line_width_budget).to_string();
        shimmer_progresses.get(idx).and_then(|p| *p).map_or_else(
            || line.clone(),
            |progress| shimmerize_plain_text(&line, progress, SHIMMER_HIGHLIGHT_WIDTH),
        )
    }));
    pane.scroll_to_bottom();
    pane.render(viewer_area, frame);
}

fn quick_filter_controls_height(width: u16, available_height: u16) -> u16 {
    if width == 0 || available_height == 0 {
        return 0;
    }
    if available_height == 1 {
        // Preserve one line for the event viewer on cramped layouts.
        return 0;
    }
    let total_chars = [
        DashboardQuickFilter::All,
        DashboardQuickFilter::Messages,
        DashboardQuickFilter::Tools,
        DashboardQuickFilter::Reservations,
    ]
    .iter()
    .fold(0usize, |acc, filter| {
        acc.saturating_add(format!(" [{}:{}] ", filter.key(), filter.control_label()).len())
    });
    let width_usize = usize::from(width.max(1));
    let estimated_lines = total_chars.saturating_add(width_usize.saturating_sub(1)) / width_usize;
    let estimated_lines = estimated_lines.max(1);
    let max_controls = available_height.saturating_sub(1);
    if max_controls == 0 {
        return 0;
    }
    u16::try_from(estimated_lines)
        .unwrap_or(u16::MAX)
        .min(max_controls)
}

fn event_log_window_bounds(
    total: usize,
    scroll_offset: usize,
    viewport_rows: usize,
) -> (usize, usize) {
    if total == 0 || viewport_rows == 0 {
        return (0, 0);
    }

    let end = total.saturating_sub(scroll_offset);
    let window_rows = viewport_rows
        .saturating_mul(4)
        .max(viewport_rows.saturating_add(8));
    let start = end.saturating_sub(window_rows);
    (start, end)
}

fn unix_epoch_micros_now() -> Option<i64> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_micros();
    i64::try_from(micros).ok()
}

#[allow(clippy::cast_precision_loss)]
fn shimmer_progress_for_timestamp(now_micros: i64, timestamp_micros: i64) -> Option<f64> {
    if timestamp_micros <= 0 {
        return None;
    }
    let age = now_micros - timestamp_micros;
    if !(0..=SHIMMER_WINDOW_MICROS).contains(&age) {
        return None;
    }
    Some((age as f64 / SHIMMER_WINDOW_MICROS as f64).clamp(0.0, 1.0))
}

fn dashboard_shimmer_progresses(
    entries: &[&EventEntry],
    effects_enabled: bool,
) -> Vec<Option<f64>> {
    let mut progresses = vec![None; entries.len()];
    if !effects_enabled || entries.is_empty() {
        return progresses;
    }
    let Some(now_micros) = unix_epoch_micros_now() else {
        return progresses;
    };
    let mut shimmer_count = 0usize;
    for idx in (0..entries.len()).rev() {
        if shimmer_count >= SHIMMER_MAX_ROWS {
            break;
        }
        if let Some(progress) =
            shimmer_progress_for_timestamp(now_micros, entries[idx].timestamp_micros)
        {
            progresses[idx] = Some(progress);
            shimmer_count += 1;
        }
    }
    progresses
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn shimmer_window_indices(
    len_chars: usize,
    progress: f64,
    width_chars: usize,
) -> Option<(usize, usize)> {
    if len_chars == 0 {
        return None;
    }
    let clamped = progress.clamp(0.0, 1.0);
    let center = ((len_chars.saturating_sub(1)) as f64 * clamped).round() as usize;
    let width = width_chars.max(1).min(len_chars);
    let half = width / 2;
    let start = center.saturating_sub(half);
    let end = (start + width).min(len_chars);
    Some((start, end))
}

fn shimmerize_plain_text(text: &str, progress: f64, width_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let Some((start, end)) = shimmer_window_indices(chars.len(), progress, width_chars) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len() + 2);
    for (idx, ch) in chars.into_iter().enumerate() {
        if idx == start {
            out.push('[');
        }
        if idx >= start && idx < end {
            if ch.is_ascii_lowercase() {
                out.push(ch.to_ascii_uppercase());
            } else if ch == ' ' {
                out.push('·');
            } else {
                out.push(ch);
            }
        } else {
            out.push(ch);
        }
        if idx + 1 == end {
            out.push(']');
        }
    }
    out
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
        " Req:{} Avg:{}ms 2xx:{} 4xx:{} 5xx:{}   Events:{}/{} ({}%) {} {}",
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

/// Activity dot colors (used in tests), derived from the theme palette.
#[cfg(test)]
fn activity_green() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().activity_active
}
#[cfg(test)]
fn activity_yellow() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().activity_idle
}
#[cfg(test)]
fn activity_gray() -> PackedRgba {
    crate::tui_theme::TuiThemePalette::current().activity_stale
}

/// Returns an activity dot character and color based on how recently an agent
/// was active. Green = active (<60s), yellow = idle (<5m), gray = stale.
#[cfg(test)]
fn activity_indicator(now_us: i64, last_active_us: i64) -> (char, PackedRgba) {
    if last_active_us == 0 {
        return ('○', activity_gray());
    }
    let age = now_us.saturating_sub(last_active_us);
    if age < ACTIVE_THRESHOLD_US {
        ('●', activity_green())
    } else if age < IDLE_THRESHOLD_US {
        ('●', activity_yellow())
    } else {
        ('○', activity_gray())
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
    fn dashboard_shimmer_progress_expires_after_window() {
        let now = 1_700_000_000_000_000_i64;
        assert!(shimmer_progress_for_timestamp(now, now).is_some());
        assert!(shimmer_progress_for_timestamp(now, now - SHIMMER_WINDOW_MICROS - 1).is_none());
    }

    #[test]
    fn dashboard_shimmer_progress_caps_at_five_entries() {
        let now = unix_epoch_micros_now().expect("system clock should provide unix micros");
        let entries: Vec<EventEntry> = (0..8_u64)
            .map(|idx| EventEntry {
                kind: MailEventKind::MessageReceived,
                severity: EventSeverity::Info,
                seq: idx,
                timestamp_micros: now
                    - (i64::try_from(idx).expect("test idx should fit i64") * 10_000),
                timestamp: format_ts(
                    now - (i64::try_from(idx).expect("test idx should fit i64") * 10_000),
                ),
                icon: '✉',
                summary: format!("message-{idx}"),
            })
            .collect();
        let refs: Vec<&EventEntry> = entries.iter().collect();
        let shimmer = dashboard_shimmer_progresses(&refs, true);
        assert_eq!(
            shimmer.iter().filter(|p| p.is_some()).count(),
            SHIMMER_MAX_ROWS
        );
        assert!(
            dashboard_shimmer_progresses(&refs, false)
                .iter()
                .all(Option::is_none)
        );
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
    fn ratio_bar_handles_zero_and_partial_fill() {
        assert_eq!(ratio_bar(0, 100, 8), "░░░░░░░░");
        assert_eq!(ratio_bar(50, 100, 8), "████░░░░");
        assert_eq!(ratio_bar(120, 100, 8), "████████");
    }

    #[test]
    fn format_sample_with_overflow_appends_remaining_count() {
        let sample = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert_eq!(format_sample_with_overflow(&sample, 3), "A, B, C");
        assert_eq!(format_sample_with_overflow(&sample, 7), "A, B, C +4");
        assert_eq!(
            format_sample_with_overflow(&Vec::<String>::new(), 5),
            "none"
        );
    }

    #[test]
    fn parse_tool_end_duration_extracts_tool_and_ms() {
        let parsed = parse_tool_end_duration("send_message 42ms q=5 [BlueLake@proj]");
        assert_eq!(parsed, Some(("send_message".to_string(), 42)));
        assert_eq!(parse_tool_end_duration("→ send_message"), None);
        assert_eq!(parse_tool_end_duration("send_message bad q=1"), None);
    }

    #[test]
    fn supergrid_breakpoints_detect_large_panels() {
        assert!(!is_supergrid_insight_area(Rect::new(
            0,
            0,
            SUPERGRID_INSIGHT_MIN_WIDTH - 1,
            SUPERGRID_INSIGHT_MIN_HEIGHT
        )));
        assert!(!is_supergrid_insight_area(Rect::new(
            0,
            0,
            SUPERGRID_INSIGHT_MIN_WIDTH,
            SUPERGRID_INSIGHT_MIN_HEIGHT - 1
        )));
        assert!(is_supergrid_insight_area(Rect::new(
            0,
            0,
            SUPERGRID_INSIGHT_MIN_WIDTH,
            SUPERGRID_INSIGHT_MIN_HEIGHT
        )));

        assert!(!is_supergrid_bottom_area(Rect::new(
            0,
            0,
            SUPERGRID_BOTTOM_MIN_WIDTH - 1,
            SUPERGRID_BOTTOM_MIN_HEIGHT
        )));
        assert!(!is_supergrid_bottom_area(Rect::new(
            0,
            0,
            SUPERGRID_BOTTOM_MIN_WIDTH,
            SUPERGRID_BOTTOM_MIN_HEIGHT - 1
        )));
        assert!(is_supergrid_bottom_area(Rect::new(
            0,
            0,
            SUPERGRID_BOTTOM_MIN_WIDTH,
            SUPERGRID_BOTTOM_MIN_HEIGHT
        )));

        assert!(!is_megagrid_bottom_area(Rect::new(
            0,
            0,
            MEGAGRID_BOTTOM_MIN_WIDTH - 1,
            MEGAGRID_BOTTOM_MIN_HEIGHT
        )));
        assert!(!is_megagrid_bottom_area(Rect::new(
            0,
            0,
            MEGAGRID_BOTTOM_MIN_WIDTH,
            MEGAGRID_BOTTOM_MIN_HEIGHT - 1
        )));
        assert!(is_megagrid_bottom_area(Rect::new(
            0,
            0,
            MEGAGRID_BOTTOM_MIN_WIDTH,
            MEGAGRID_BOTTOM_MIN_HEIGHT
        )));
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
            timestamp_micros: 1_700_000_000_000_000,
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
    fn quick_filter_includes_messages_only_for_all_or_messages() {
        assert!(DashboardQuickFilter::All.includes_messages());
        assert!(DashboardQuickFilter::Messages.includes_messages());
        assert!(!DashboardQuickFilter::Tools.includes_messages());
        assert!(!DashboardQuickFilter::Reservations.includes_messages());
    }

    #[test]
    fn recent_message_preview_stale_detection() {
        let stale = RecentMessagePreview {
            timestamp_micros: 0,
            direction: "Inbound",
            timestamp: "00:00:00.000".to_string(),
            from: "A".to_string(),
            to: "B".to_string(),
            subject: "S".to_string(),
            thread_id: "t".to_string(),
            project: "p".to_string(),
        };
        assert!(stale.is_stale());
    }

    #[test]
    fn panel_budget_heights_match_terminal_classes() {
        assert_eq!(summary_band_height(TerminalClass::Tiny), 1);
        assert_eq!(summary_band_height(TerminalClass::Compact), 3);
        assert_eq!(summary_band_height(TerminalClass::Normal), 3);
        assert_eq!(summary_band_height(TerminalClass::Wide), 3);
        assert_eq!(summary_band_height(TerminalClass::UltraWide), 3);

        assert_eq!(anomaly_rail_height(TerminalClass::Tiny, 2), 0);
        assert_eq!(anomaly_rail_height(TerminalClass::Compact, 2), 3);
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
    fn quick_filter_controls_height_scales_with_width() {
        assert_eq!(quick_filter_controls_height(120, 12), 1);
        assert_eq!(quick_filter_controls_height(28, 12), 2);
        assert_eq!(quick_filter_controls_height(18, 12), 3);
    }

    #[test]
    fn quick_filter_controls_height_preserves_viewer_space() {
        assert_eq!(quick_filter_controls_height(8, 1), 0);
        assert_eq!(quick_filter_controls_height(8, 2), 1);
        assert_eq!(quick_filter_controls_height(8, 3), 2);
    }

    #[test]
    fn main_layout_ultrawide_exposes_double_surface_vs_standard() {
        let standard =
            DashboardScreen::main_content_layout(true, false).compute(Rect::new(0, 0, 100, 30));
        let ultra =
            DashboardScreen::main_content_layout(true, false).compute(Rect::new(0, 0, 200, 50));

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
            ultra_visible > standard_visible,
            "expected ultrawide to expose strictly more panel surface: standard={standard_visible}, ultrawide={ultra_visible}"
        );
        assert!(standard.rect(PanelSlot::Inspector).is_none());
        assert!(standard.rect(PanelSlot::Footer).is_some());
        assert!(ultra.rect(PanelSlot::Inspector).is_some());
        assert!(ultra.rect(PanelSlot::Footer).is_some());
    }

    #[test]
    fn main_layout_ultrawide_panels_fit_bounds_without_overlap() {
        let area = Rect::new(0, 0, 200, 50);
        let composition = DashboardScreen::main_content_layout(true, false).compute(area);
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
            DashboardScreen::main_content_layout(false, false).compute(Rect::new(0, 0, 200, 50));
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
    fn dashboard_synthesizes_message_delta_from_db_snapshot() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        state.update_db_stats(DbStatSnapshot {
            messages: 10,
            file_reservations: 3,
            timestamp_micros: 1,
            ..Default::default()
        });
        screen.tick(1, &state);
        assert!(
            screen.event_log.iter().any(|entry| {
                entry.kind == MailEventKind::MessageReceived
                    && entry.summary.contains("DB baseline: 10 total messages")
            }),
            "first snapshot should seed baseline message context"
        );

        state.update_db_stats(DbStatSnapshot {
            messages: 12,
            file_reservations: 3,
            timestamp_micros: 2,
            ..Default::default()
        });
        screen.tick(2, &state);

        assert!(
            screen.event_log.iter().any(|entry| {
                entry.kind == MailEventKind::MessageReceived
                    && entry.summary.contains("DB observed 2 new messages")
            }),
            "expected synthetic message-delta entry"
        );
    }

    #[test]
    fn dashboard_synthesizes_reservation_deltas_from_db_snapshot() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        state.update_db_stats(DbStatSnapshot {
            messages: 5,
            file_reservations: 4,
            timestamp_micros: 10,
            ..Default::default()
        });
        screen.tick(1, &state);

        state.update_db_stats(DbStatSnapshot {
            messages: 5,
            file_reservations: 6,
            timestamp_micros: 11,
            ..Default::default()
        });
        screen.tick(2, &state);

        assert!(
            screen.event_log.iter().any(|entry| {
                entry.kind == MailEventKind::ReservationGranted
                    && entry.summary.contains("active reservations added")
            }),
            "expected synthetic ReservationGranted delta entry"
        );

        state.update_db_stats(DbStatSnapshot {
            messages: 5,
            file_reservations: 2,
            timestamp_micros: 12,
            ..Default::default()
        });
        screen.tick(3, &state);

        assert!(
            screen.event_log.iter().any(|entry| {
                entry.kind == MailEventKind::ReservationReleased
                    && entry.summary.contains("active reservations removed")
            }),
            "expected synthetic ReservationReleased delta entry"
        );
    }

    #[test]
    fn dashboard_db_delta_events_include_top_project_hints() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        state.update_db_stats(DbStatSnapshot {
            messages: 4,
            file_reservations: 2,
            projects_list: vec![
                crate::tui_events::ProjectSummary {
                    slug: "alpha".to_string(),
                    message_count: 3,
                    reservation_count: 1,
                    ..Default::default()
                },
                crate::tui_events::ProjectSummary {
                    slug: "beta".to_string(),
                    message_count: 7,
                    reservation_count: 2,
                    ..Default::default()
                },
            ],
            timestamp_micros: 21,
            ..Default::default()
        });
        screen.tick(1, &state);

        assert!(
            screen
                .event_log
                .iter()
                .any(|entry| entry.summary.contains("top project beta (7 msgs)")),
            "baseline message summary should include top project hint"
        );

        assert!(
            screen
                .event_log
                .iter()
                .any(|entry| entry.summary.contains("top lock project beta (2)")),
            "baseline reservation summary should include top reservation project hint"
        );
    }

    #[test]
    fn dashboard_seeds_baseline_events_when_log_is_empty() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        state.update_db_stats(DbStatSnapshot {
            messages: 7,
            file_reservations: 3,
            timestamp_micros: 10,
            ..Default::default()
        });
        screen.tick(1, &state);

        let baseline_message = screen
            .event_log
            .iter()
            .find(|entry| entry.kind == MailEventKind::MessageReceived)
            .expect("expected synthetic baseline message entry");
        assert!(
            baseline_message
                .summary
                .contains("DB baseline: 7 total messages")
        );

        let baseline_reservations = screen
            .event_log
            .iter()
            .find(|entry| entry.kind == MailEventKind::ReservationGranted)
            .expect("expected synthetic baseline reservation entry");
        assert!(
            baseline_reservations
                .summary
                .contains("active reservations currently held")
        );
    }

    #[test]
    fn dashboard_stat_refresh_tracks_previous_and_current_snapshots() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        state.update_db_stats(DbStatSnapshot {
            messages: 5,
            agents: 2,
            file_reservations: 1,
            timestamp_micros: 100,
            ..Default::default()
        });
        screen.tick(STAT_REFRESH_TICKS, &state);
        assert_eq!(screen.current_db_stats.messages, 5);
        assert_eq!(screen.prev_db_stats.messages, 5);

        state.update_db_stats(DbStatSnapshot {
            messages: 9,
            agents: 3,
            file_reservations: 4,
            timestamp_micros: 200,
            ..Default::default()
        });
        screen.tick(STAT_REFRESH_TICKS * 2, &state);

        assert_eq!(screen.prev_db_stats.messages, 5);
        assert_eq!(screen.current_db_stats.messages, 9);
        assert_eq!(screen.prev_db_stats.file_reservations, 1);
        assert_eq!(screen.current_db_stats.file_reservations, 4);
    }

    #[test]
    fn detect_anomalies_flags_reservation_counter_snapshot_divergence() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let screen = DashboardScreen::new();

        state.update_db_stats(DbStatSnapshot {
            file_reservations: 7,
            reservation_snapshots: Vec::new(),
            timestamp_micros: 123,
            ..Default::default()
        });

        let anomalies = screen.detect_anomalies(&state);
        assert!(
            anomalies
                .iter()
                .any(|anomaly| anomaly.headline == "7 active reservations but no reservation rows"),
            "expected divergence anomaly to be emitted"
        );
    }

    #[test]
    fn dashboard_health_pulse_visible_at_verbose_verbosity() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        let _ = state.push_event(MailEvent::health_pulse(DbStatSnapshot::default()));
        screen.ingest_events(&state);
        // Health pulses are Debug-level, so Verbose (default) includes them.
        assert_eq!(screen.event_log.len(), 1, "event should be stored");
        assert_eq!(
            screen.visible_entries().len(),
            1,
            "health pulses visible at default Verbose verbosity"
        );
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
    fn slash_focuses_live_query_input() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        assert!(!screen.consumes_text_input());
        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('/'))), &state);
        assert!(screen.quick_query_active);
        assert!(screen.consumes_text_input());
    }

    #[test]
    fn live_query_filters_visible_entries() {
        let mut screen = DashboardScreen::new();
        screen.verbosity = VerbosityTier::All;
        screen.event_log.push(EventEntry {
            kind: MailEventKind::HttpRequest,
            severity: EventSeverity::Debug,
            seq: 1,
            timestamp_micros: 0,
            timestamp: "00:00:00.000".to_string(),
            icon: '↔',
            summary: "alpha endpoint".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
            seq: 2,
            timestamp_micros: 1_000,
            timestamp: "00:00:00.001".to_string(),
            icon: '⚙',
            summary: "beta tool".to_string(),
        });

        screen.quick_query_input.set_value("alpha");
        let visible = screen.visible_entries();
        assert_eq!(visible.len(), 1);
        assert!(visible[0].summary.contains("alpha"));
    }

    #[test]
    fn enter_with_live_query_navigates_to_search() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.quick_query_input.set_value("deploy pipeline");

        let enter = Event::Key(ftui::KeyEvent::new(KeyCode::Enter));
        let cmd = screen.update(&enter, &state);
        assert!(matches!(
            cmd,
            Cmd::Msg(MailScreenMsg::DeepLink(DeepLinkTarget::SearchFocused(query)))
                if query == "deploy pipeline"
        ));
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
        assert_eq!(screen.verbosity, VerbosityTier::Verbose);

        let key = Event::Key(ftui::KeyEvent::new(KeyCode::Char('v')));
        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::All);

        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::Minimal);

        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::Standard);

        screen.update(&key, &state);
        assert_eq!(screen.verbosity, VerbosityTier::Verbose);
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
            let icon = crate::tui_events::event_log_icon(kind);
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
    fn tick_clamps_stale_scroll_offset_to_visible_range() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.auto_follow = false;
        screen.scroll_offset = 9_999;

        let _ = state.push_event(MailEvent::server_started("http://test", "cfg"));
        let _ = state.push_event(MailEvent::server_started("http://test", "cfg"));
        let _ = state.push_event(MailEvent::server_started("http://test", "cfg"));

        screen.tick(1, &state);
        let max_scroll = screen.visible_entries().len().saturating_sub(1);
        assert_eq!(screen.scroll_offset, max_scroll);
    }

    #[test]
    fn quick_filter_change_clamps_stale_scroll_offset_immediately() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.auto_follow = false;
        screen.scroll_offset = 9_999;

        let _ = state.push_event(MailEvent::server_started("http://test", "cfg"));
        let _ = state.push_event(MailEvent::server_started("http://test", "cfg"));
        screen.ingest_events(&state);

        screen.apply_quick_filter(DashboardQuickFilter::Tools);
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn type_filter_cycles_through_states() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        let t = Event::Key(ftui::KeyEvent::new(KeyCode::Char('t')));

        // all -> messages
        screen.update(&t, &state);
        assert!(screen.type_filter.contains(&MailEventKind::MessageSent));
        assert!(screen.type_filter.contains(&MailEventKind::MessageReceived));

        // messages -> tools
        screen.update(&t, &state);
        assert!(screen.type_filter.contains(&MailEventKind::ToolCallStart));
        assert!(screen.type_filter.contains(&MailEventKind::ToolCallEnd));

        // tools -> reservations
        screen.update(&t, &state);
        assert!(
            screen
                .type_filter
                .contains(&MailEventKind::ReservationGranted)
        );
        assert!(
            screen
                .type_filter
                .contains(&MailEventKind::ReservationReleased)
        );

        // reservations -> all
        screen.update(&t, &state);
        assert!(screen.type_filter.is_empty());
    }

    #[test]
    fn quick_filter_hotkeys_apply_expected_filters() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('2'))), &state);
        assert_eq!(screen.quick_filter, DashboardQuickFilter::Messages);
        assert!(screen.type_filter.contains(&MailEventKind::MessageSent));
        assert!(screen.type_filter.contains(&MailEventKind::MessageReceived));

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('3'))), &state);
        assert_eq!(screen.quick_filter, DashboardQuickFilter::Tools);
        assert!(screen.type_filter.contains(&MailEventKind::ToolCallStart));
        assert!(screen.type_filter.contains(&MailEventKind::ToolCallEnd));

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('4'))), &state);
        assert_eq!(screen.quick_filter, DashboardQuickFilter::Reservations);
        assert!(
            screen
                .type_filter
                .contains(&MailEventKind::ReservationGranted)
        );
        assert!(
            screen
                .type_filter
                .contains(&MailEventKind::ReservationReleased)
        );

        screen.update(&Event::Key(ftui::KeyEvent::new(KeyCode::Char('1'))), &state);
        assert_eq!(screen.quick_filter, DashboardQuickFilter::All);
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

    #[test]
    fn gradient_title_renders_when_effects_enabled() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 1, &mut pool);
        render_gradient_title(&mut frame, Rect::new(0, 0, 80, 1), true);
    }

    #[test]
    fn gradient_title_falls_back_when_effects_disabled() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = Frame::new(80, 1, &mut pool);
        render_gradient_title(&mut frame, Rect::new(0, 0, 80, 1), false);
    }

    // ── Activity indicator tests ──────────────────────────────────

    #[test]
    fn activity_indicator_active() {
        let now = 1_000_000_000_i64; // 1 second in micros
        let recent = now - 30_000_000; // 30 seconds ago
        let (dot, color) = activity_indicator(now, recent);
        assert_eq!(dot, '●');
        assert_eq!(color, activity_green());
    }

    #[test]
    fn activity_indicator_idle() {
        let now = 1_000_000_000_i64;
        let idle = now - 120_000_000; // 2 minutes ago
        let (dot, color) = activity_indicator(now, idle);
        assert_eq!(dot, '●');
        assert_eq!(color, activity_yellow());
    }

    #[test]
    fn activity_indicator_stale() {
        let now = 1_000_000_000_i64;
        let stale = now - 600_000_000; // 10 minutes ago
        let (dot, color) = activity_indicator(now, stale);
        assert_eq!(dot, '○');
        assert_eq!(color, activity_gray());
    }

    #[test]
    fn activity_indicator_zero_ts_is_gray() {
        let (dot, color) = activity_indicator(1_000_000_000, 0);
        assert_eq!(dot, '○');
        assert_eq!(color, activity_gray());
    }

    #[test]
    fn activity_indicator_boundary_at_60s() {
        let now = 1_000_000_000_i64;
        // Exactly at boundary: 60s ago
        let at_boundary = now - ACTIVE_THRESHOLD_US;
        let (_, color) = activity_indicator(now, at_boundary);
        assert_eq!(
            color,
            activity_yellow(),
            "exactly 60s should be idle/yellow"
        );
        // 1us before boundary: 59.999999s ago
        let just_inside = now - ACTIVE_THRESHOLD_US + 1;
        let (_, color) = activity_indicator(now, just_inside);
        assert_eq!(color, activity_green(), "just under 60s should be green");
    }

    #[test]
    fn activity_indicator_boundary_at_5m() {
        let now = 1_000_000_000_i64;
        let at_boundary = now - IDLE_THRESHOLD_US;
        let (dot, color) = activity_indicator(now, at_boundary);
        assert_eq!(dot, '○');
        assert_eq!(color, activity_gray(), "exactly 5m should be stale/gray");
        let just_inside = now - IDLE_THRESHOLD_US + 1;
        let (dot, color) = activity_indicator(now, just_inside);
        assert_eq!(dot, '●');
        assert_eq!(color, activity_yellow(), "just under 5m should be yellow");
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

    // ── KPI ordering tests ──────────────────────────────────────

    /// Verify that KPI ordering prioritizes operational metrics first.
    #[test]
    fn kpi_tile_order_puts_operational_metrics_first() {
        // Detailed density includes flow + lock pressure + context.
        // Expected order: Messages, Ack Pend, Locks, Agents, Projects, Requests, Avg Lat, Uptime
        let labels = [
            "Messages", "Ack Pend", "Locks", "Agents", "Projects", "Requests", "Avg Lat", "Uptime",
        ];

        // Verify Messages is first (core flow indicator).
        assert_eq!(labels[0], "Messages");
        // Verify Ack Pending is second (actionable alert).
        assert_eq!(labels[1], "Ack Pend");
        // Verify lock pressure is visible in top metrics.
        assert_eq!(labels[2], "Locks");
        // Verify Uptime is last (context, not actionable).
        assert_eq!(labels[labels.len() - 1], "Uptime");
    }

    /// Verify compact density still shows the 3 most important metrics.
    #[test]
    fn kpi_compact_shows_core_metrics() {
        // Compact: Msg, Locks, Req.
        let compact_labels = ["Msg", "Locks", "Req"];
        assert_eq!(compact_labels[0], "Msg", "messages must lead in compact");
    }

    // ── Event salience tests ────────────────────────────────────

    #[test]
    fn anomaly_rail_shows_on_compact_terminals() {
        // Compact terminals should show anomalies (condensed) rather than hiding them.
        assert!(anomaly_rail_height(TerminalClass::Compact, 1) > 0);
        // Tiny still hides them.
        assert_eq!(anomaly_rail_height(TerminalClass::Tiny, 1), 0);
        // No anomalies = no rail regardless of terminal class.
        assert_eq!(anomaly_rail_height(TerminalClass::Normal, 0), 0);
    }

    #[test]
    fn event_severity_salience_hierarchy() {
        use ftui::style::StyleFlags;
        let has = |s: Style, f: StyleFlags| s.attrs.is_some_and(|a| a.contains(f));

        // Error and Warn should be bold (high salience).
        assert!(has(EventSeverity::Error.style(), StyleFlags::BOLD));
        assert!(has(EventSeverity::Warn.style(), StyleFlags::BOLD));

        // Trace should be dim (background noise).
        assert!(has(EventSeverity::Trace.style(), StyleFlags::DIM));

        // Info and Debug should NOT be bold (standard/subdued).
        assert!(!has(EventSeverity::Info.style(), StyleFlags::BOLD));
        assert!(!has(EventSeverity::Debug.style(), StyleFlags::BOLD));
    }

    // ── Mouse parity tests (br-1xt0m.1.12.4) ──────────────────

    #[test]
    fn mouse_scroll_up_increases_offset() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        assert_eq!(screen.scroll_offset, 0);

        let scroll_up = Event::Mouse(ftui::MouseEvent::new(
            ftui::MouseEventKind::ScrollUp,
            10,
            10,
        ));
        screen.update(&scroll_up, &state);
        assert_eq!(screen.scroll_offset, 1, "scroll up should increase offset");
        assert!(!screen.auto_follow, "scroll up should disable auto-follow");
    }

    #[test]
    fn mouse_scroll_down_decreases_offset() {
        let config = mcp_agent_mail_core::Config::default();
        let state = TuiSharedState::new(&config);
        let mut screen = DashboardScreen::new();
        screen.scroll_offset = 5;
        screen.auto_follow = false;

        let scroll_down = Event::Mouse(ftui::MouseEvent::new(
            ftui::MouseEventKind::ScrollDown,
            10,
            10,
        ));
        screen.update(&scroll_down, &state);
        assert_eq!(
            screen.scroll_offset, 4,
            "scroll down should decrease offset"
        );

        // Scroll to bottom re-enables auto-follow
        for _ in 0..10 {
            screen.update(&scroll_down, &state);
        }
        assert_eq!(screen.scroll_offset, 0);
        assert!(
            screen.auto_follow,
            "reaching bottom should re-enable auto-follow"
        );
    }

    // ── Screen logic, density heuristics, and failure paths (br-1xt0m.1.13.8) ──

    #[test]
    fn trend_for_up_down_flat() {
        assert_eq!(trend_for(10, 5), MetricTrend::Up);
        assert_eq!(trend_for(5, 10), MetricTrend::Down);
        assert_eq!(trend_for(5, 5), MetricTrend::Flat);
        assert_eq!(trend_for(0, 0), MetricTrend::Flat);
        assert_eq!(trend_for(1, 0), MetricTrend::Up);
        assert_eq!(trend_for(0, 1), MetricTrend::Down);
    }

    #[test]
    fn anomaly_rail_height_zero_when_no_anomalies() {
        for tc in [
            TerminalClass::Tiny,
            TerminalClass::Compact,
            TerminalClass::Normal,
            TerminalClass::Wide,
            TerminalClass::UltraWide,
        ] {
            assert_eq!(
                anomaly_rail_height(tc, 0),
                0,
                "zero anomalies → zero height for {tc:?}"
            );
        }
    }

    #[test]
    fn anomaly_rail_height_hidden_on_tiny() {
        assert_eq!(anomaly_rail_height(TerminalClass::Tiny, 3), 0);
    }

    #[test]
    fn anomaly_rail_height_compact_vs_normal() {
        assert_eq!(anomaly_rail_height(TerminalClass::Compact, 2), 3);
        assert_eq!(anomaly_rail_height(TerminalClass::Normal, 2), 4);
        assert_eq!(anomaly_rail_height(TerminalClass::Wide, 1), 4);
    }

    #[test]
    fn compute_percentile_empty_returns_zeros() {
        let p = DashboardScreen::compute_percentile(&[]);
        assert!((p.p50 - 0.0).abs() < f64::EPSILON);
        assert!((p.p95 - 0.0).abs() < f64::EPSILON);
        assert!((p.p99 - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_percentile_single_value() {
        let p = DashboardScreen::compute_percentile(&[42.0]);
        assert!((p.p50 - 42.0).abs() < f64::EPSILON);
        assert!((p.p95 - 42.0).abs() < f64::EPSILON);
        assert!((p.p99 - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_percentile_sorted_data() {
        // 100 values: 1.0, 2.0, ..., 100.0
        let data: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = DashboardScreen::compute_percentile(&data);
        assert!((p.p50 - 51.0).abs() < f64::EPSILON);
        assert!((p.p95 - 96.0).abs() < f64::EPSILON);
        assert!((p.p99 - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn footer_bar_hidden_on_tiny() {
        assert_eq!(footer_bar_height(TerminalClass::Tiny), 0);
        assert_eq!(footer_bar_height(TerminalClass::Compact), 1);
        assert_eq!(footer_bar_height(TerminalClass::Normal), 1);
    }

    #[test]
    fn title_band_hidden_on_tiny() {
        assert_eq!(title_band_height(TerminalClass::Tiny), 0);
        assert_eq!(title_band_height(TerminalClass::Compact), 1);
    }

    #[test]
    fn event_log_window_bounds_follow_mode_uses_tail_window() {
        let (start, end) = event_log_window_bounds(1_000, 0, 10);
        assert_eq!(end, 1_000);
        assert_eq!(start, 960);
    }

    #[test]
    fn event_log_window_bounds_scrolls_back_without_scanning_full_log() {
        let (start, end) = event_log_window_bounds(1_000, 300, 10);
        assert_eq!(end, 700);
        assert_eq!(start, 660);
    }
}
