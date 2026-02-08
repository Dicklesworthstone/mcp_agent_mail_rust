//! Dashboard screen — the default landing surface for `AgentMailTUI`.
//!
//! Displays real-time stats, a live event log, and health alarms in a
//! responsive layout that adapts from 80×24 to 200×50+.

use std::collections::HashSet;

use ftui::layout::Rect;
use ftui::text::{Line, Span, Text};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::progress::MiniBar;
use ftui::widgets::Sparkline;
use ftui::{Event, Frame, KeyCode, KeyEventKind, PackedRgba};
use ftui_runtime::program::Cmd;

use crate::tui_bridge::TuiSharedState;
use crate::tui_events::{DbStatSnapshot, EventSeverity, MailEvent, MailEventKind, VerbosityTier};
use crate::tui_screens::{HelpEntry, MailScreen, MailScreenMsg};

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Max event log entries kept in scroll-back.
const EVENT_LOG_CAPACITY: usize = 2000;

/// Stat tiles refresh every N ticks (100ms each → 1 s).
const STAT_REFRESH_TICKS: u64 = 10;

/// Unicode block characters for sparkline rendering (bottom-aligned).
const SPARK_CHARS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// ──────────────────────────────────────────────────────────────────────
// DashboardScreen
// ──────────────────────────────────────────────────────────────────────

/// The main dashboard screen.
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
}

/// A pre-formatted event log entry.
#[derive(Debug, Clone)]
pub(crate) struct EventEntry {
    pub(crate) kind: MailEventKind,
    pub(crate) severity: EventSeverity,
    pub(crate) timestamp: String,
    pub(crate) icon: char,
    pub(crate) summary: String,
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
        }
    }

    /// Ingest new events from the ring buffer.
    fn ingest_events(&mut self, state: &TuiSharedState) {
        let new_events = state.events_since(self.last_seq);
        for event in &new_events {
            self.last_seq = event.seq().max(self.last_seq);
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
                    // Cycle verbosity tier
                    KeyCode::Char('v') => {
                        self.verbosity = self.verbosity.next();
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

    fn tick(&mut self, tick_count: u64, state: &TuiSharedState) {
        // Ingest new events every tick
        self.ingest_events(state);

        // Refresh stat snapshot periodically
        if tick_count % STAT_REFRESH_TICKS == 0 {
            if let Some(stats) = state.db_stats_snapshot() {
                self.prev_db_stats = stats;
            }
        }

        // Sample sparkline data from request counters
        let counters = state.request_counters();
        if self.sparkline_data.len() >= 60 {
            self.sparkline_data.remove(0);
        }
        #[allow(clippy::cast_precision_loss)]
        let val = counters.latency_total_ms as f64;
        self.sparkline_data.push(val);
    }

    fn view(&self, frame: &mut Frame<'_>, area: Rect, state: &TuiSharedState) {
        // Main layout: [stat tiles row: 7 lines] | [event log: fill] | [footer: 1 line]
        let stat_height = 7_u16;
        let footer_height = 1_u16;
        let log_height = area
            .height
            .saturating_sub(stat_height + footer_height)
            .max(3);

        let stat_area = Rect::new(area.x, area.y, area.width, stat_height);
        let log_area = Rect::new(area.x, area.y + stat_height, area.width, log_height);
        let footer_area = Rect::new(
            area.x,
            area.y + stat_height + log_height,
            area.width,
            footer_height,
        );

        render_stat_tiles(frame, stat_area, state, &self.prev_db_stats, &self.sparkline_data);
        render_event_log(
            frame,
            log_area,
            &self.visible_entries(),
            self.scroll_offset,
            self.auto_follow,
            &self.type_filter,
            self.verbosity,
        );
        render_footer(frame, footer_area, state);
    }

    fn keybindings(&self) -> Vec<HelpEntry> {
        vec![
            HelpEntry {
                key: "j/k",
                action: "Scroll event log",
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
    let timestamp = format_ts(event.timestamp_micros());

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

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the stat tiles row with Sparkline and MiniBar widgets.
fn render_stat_tiles(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiSharedState,
    _prev_stats: &DbStatSnapshot,
    sparkline_data: &[f64],
) {
    // Split into 3 columns: server info | DB stats | agents
    let col_width = area.width / 3;
    let col1 = Rect::new(area.x, area.y, col_width, area.height);
    let col2 = Rect::new(area.x + col_width, area.y, col_width, area.height);
    let col3 = Rect::new(
        area.x + col_width * 2,
        area.y,
        area.width.saturating_sub(col_width * 2),
        area.height,
    );

    // ── Server tile: uptime + counters + latency sparkline ──────
    let counters = state.request_counters();
    let uptime = state.uptime();
    let uptime_str = format_duration(uptime);
    let avg_ms = counters
        .latency_total_ms
        .checked_div(counters.total)
        .unwrap_or(0);
    let info = format!(
        "Up: {uptime_str}  Avg: {avg_ms}ms\nReq: {} 2xx:{} 4xx:{} 5xx:{}",
        counters.total, counters.status_2xx, counters.status_4xx, counters.status_5xx,
    );
    let block = Block::default()
        .title("Server")
        .border_type(BorderType::Rounded);
    let p = Paragraph::new(info).block(block);
    p.render(col1, frame);

    // Render latency sparkline in the bottom of the server tile (inside the border)
    let spark_inner_y = col1.y + col1.height.saturating_sub(2);
    let spark_inner_w = col_width.saturating_sub(2);
    if spark_inner_w > 4 && col1.height >= 5 {
        let spark_area = Rect::new(col1.x + 1, spark_inner_y, spark_inner_w, 1);
        let sparkline = Sparkline::new(sparkline_data)
            .gradient(PackedRgba::GREEN, PackedRgba::RED);
        sparkline.render(spark_area, frame);
    }

    // ── Database tile: stats + MiniBar gauges ───────────────────
    let db = state.db_stats_snapshot().unwrap_or_default();
    let stats_text = format!(
        "Proj: {:>5}  Agents: {:>5}\nMsg:  {:>5}  Reserv: {:>5}\nLinks:{:>5}  AckPnd: {:>5}",
        db.projects, db.agents, db.messages, db.file_reservations, db.contact_links, db.ack_pending,
    );
    let block = Block::default()
        .title("Database")
        .border_type(BorderType::Rounded);
    let p = Paragraph::new(stats_text).block(block);
    p.render(col2, frame);

    // MiniBar for request success rate in the bottom of the DB tile
    let bar_inner_y = col2.y + col2.height.saturating_sub(2);
    let bar_inner_w = col_width.saturating_sub(2);
    if bar_inner_w > 6 && col2.height >= 5 {
        let bar_area = Rect::new(col2.x + 1, bar_inner_y, bar_inner_w, 1);
        #[allow(clippy::cast_precision_loss)]
        let success_rate = if counters.total > 0 {
            counters.status_2xx as f64 / counters.total as f64
        } else {
            1.0
        };
        let bar = MiniBar::new(success_rate, bar_inner_w).show_percent(true);
        bar.render(bar_area, frame);
    }

    // ── Agents tile: show top 5 agents ──────────────────────────
    let agents = &db.agents_list;
    let agent_text = if agents.is_empty() {
        "(no agents)".to_string()
    } else {
        agents
            .iter()
            .take(5)
            .map(|a| format!("{} ({})", a.name, a.program))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let title = format!("Agents ({})", db.agents);
    let block = Block::default()
        .title(title.as_str())
        .border_type(BorderType::Rounded);
    let p = Paragraph::new(agent_text).block(block);
    p.render(col3, frame);
}

/// Render the scrollable event log.
fn render_event_log(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[&EventEntry],
    scroll_offset: usize,
    auto_follow: bool,
    type_filter: &HashSet<MailEventKind>,
    verbosity: VerbosityTier,
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

    // Build styled text lines with colored severity badges
    let mut text_lines: Vec<Line> = Vec::with_capacity(viewport.len());
    for entry in viewport {
        let sev = entry.severity;
        let line = Line::from_spans([
            Span::raw(format!("{} ", entry.timestamp)),
            sev.styled_badge(),
            Span::raw(" "),
            Span::styled(format!("{}", entry.icon), sev.style()),
            Span::raw(format!(" {}", entry.summary)),
        ]);
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
#[must_use]
pub fn render_sparkline(data: &[f64], width: usize) -> String {
    if data.is_empty() || width == 0 {
        return String::new();
    }

    // Take the last `width` samples
    let start = data.len().saturating_sub(width);
    let slice = &data[start..];

    let max = slice.iter().copied().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return " ".repeat(slice.len());
    }

    slice
        .iter()
        .map(|&v| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let normalized = (v / max * 8.0).round() as usize;
            SPARK_CHARS[normalized.min(SPARK_CHARS.len() - 1)]
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        let data = vec![0.0, 0.0, 0.0];
        let spark = render_sparkline(&data, 3);
        assert_eq!(spark, "   ");
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
            timestamp: "00:00:00.000".to_string(),
            icon: '↔',
            summary: "GET /".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
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
        assert!(bindings.iter().any(|b| b.key == "f"));
        assert!(bindings.iter().any(|b| b.key == "v"));
        assert!(bindings.iter().any(|b| b.key == "t"));
    }

    #[test]
    fn verbosity_tiers_filter_correctly() {
        let mut screen = DashboardScreen::new();
        // Add events at different severities
        screen.event_log.push(EventEntry {
            kind: MailEventKind::HealthPulse,
            severity: EventSeverity::Trace,
            timestamp: "00:00:00.000".to_string(),
            icon: '♥',
            summary: "pulse".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
            timestamp: "00:00:00.001".to_string(),
            icon: '⚙',
            summary: "tool done".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::MessageSent,
            severity: EventSeverity::Info,
            timestamp: "00:00:00.002".to_string(),
            icon: '✉',
            summary: "msg sent".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ServerShutdown,
            severity: EventSeverity::Warn,
            timestamp: "00:00:00.003".to_string(),
            icon: '⏹',
            summary: "shutdown".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::HttpRequest,
            severity: EventSeverity::Error,
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
    fn verbosity_and_type_filter_combine() {
        let mut screen = DashboardScreen::new();
        // Add an Info-level message and a Debug-level tool end
        screen.event_log.push(EventEntry {
            kind: MailEventKind::MessageSent,
            severity: EventSeverity::Info,
            timestamp: "00:00:00.000".to_string(),
            icon: '✉',
            summary: "msg".to_string(),
        });
        screen.event_log.push(EventEntry {
            kind: MailEventKind::ToolCallEnd,
            severity: EventSeverity::Debug,
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
}
