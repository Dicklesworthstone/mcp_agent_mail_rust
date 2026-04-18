#![allow(clippy::module_name_repetitions)]

use ftui::layout::Rect;
use ftui::text::{Line, Span};
use ftui::{Frame, PackedRgba, Style};
use ftui_widgets::fenwick::FenwickTree;
use ftui_widgets::virtualized::RenderItem;
use mcp_agent_mail_core::AgentHealthScorecard;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub const DEFAULT_EVENT_RING_CAPACITY: usize = 10_000;
/// Bounded scan window for low-severity-preferred eviction when the ring is full.
///
/// A tight bound prevents O(capacity) scans under sustained load while still
/// biasing eviction toward low-severity entries near the oldest frontier.
const LOW_SEVERITY_EVICT_SCAN_LIMIT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Tooling,
    Http,
    Mail,
    Reservations,
    Lifecycle,
    Database,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailEventKind {
    ToolCallStart,
    ToolCallEnd,
    MessageSent,
    MessageReceived,
    ReservationGranted,
    ReservationReleased,
    AgentRegistered,
    HttpRequest,
    HealthPulse,
    ServerStarted,
    ServerShutdown,
}

impl MailEventKind {
    /// Compact display label for log/timeline renderers.
    #[must_use]
    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::ToolCallStart => "ToolStart",
            Self::ToolCallEnd => "ToolEnd",
            Self::MessageSent => "MsgSent",
            Self::MessageReceived => "MsgRecv",
            Self::ReservationGranted => "ResGrant",
            Self::ReservationReleased => "ResRelease",
            Self::AgentRegistered => "AgentReg",
            Self::HttpRequest => "HTTP",
            Self::HealthPulse => "Health",
            Self::ServerStarted => "SrvStart",
            Self::ServerShutdown => "SrvStop",
        }
    }
}

/// Pre-formatted event log entry used by Dashboard/Timeline log views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLogEntry {
    pub kind: MailEventKind,
    pub severity: EventSeverity,
    pub seq: u64,
    pub timestamp_micros: i64,
    pub timestamp: String,
    pub icon: char,
    pub summary: String,
}

// ──────────────────────────────────────────────────────────────────────
// EventSeverity — derived importance level for filtering
// ──────────────────────────────────────────────────────────────────────

/// Severity level derived from event data, used for verbosity filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    /// High-frequency background noise (tool starts).
    Trace,
    /// Routine operational detail (tool completions, successful HTTP).
    Debug,
    /// Noteworthy business events (messages, reservations, lifecycle).
    Info,
    /// Abnormal but non-critical (HTTP 4xx, server shutdown).
    Warn,
    /// Failures requiring attention (HTTP 5xx).
    Error,
}

// ── Severity visual design tokens ─────────────────────────────────
//
// Centralised color palette so every screen renders severity badges
// identically.  The palette is tuned for dark terminals (light-on-dark)
// and degrades gracefully on terminals without true-color support.

/// Trace — dim gray; background noise, nearly invisible.
pub const SEV_TRACE_FG: PackedRgba = PackedRgba::rgb(100, 105, 120);
/// Debug — cyan; routine detail, visible but subdued.
pub const SEV_DEBUG_FG: PackedRgba = PackedRgba::rgb(100, 200, 230);
/// Info — green; noteworthy business events, clearly visible.
pub const SEV_INFO_FG: PackedRgba = PackedRgba::rgb(120, 220, 150);
/// Warn — amber/yellow; abnormal conditions, draws attention.
pub const SEV_WARN_FG: PackedRgba = PackedRgba::rgb(255, 184, 108);
/// Error — red; failures requiring immediate triage.
pub const SEV_ERROR_FG: PackedRgba = PackedRgba::rgb(255, 100, 100);

impl EventSeverity {
    /// Short badge label for rendering.
    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Trace => "TRC",
            Self::Debug => "DBG",
            Self::Info => "INF",
            Self::Warn => "WRN",
            Self::Error => "ERR",
        }
    }

    /// Foreground color for this severity level.
    #[must_use]
    pub const fn color(self) -> PackedRgba {
        match self {
            Self::Trace => SEV_TRACE_FG,
            Self::Debug => SEV_DEBUG_FG,
            Self::Info => SEV_INFO_FG,
            Self::Warn => SEV_WARN_FG,
            Self::Error => SEV_ERROR_FG,
        }
    }

    /// Full style for the severity badge text.
    ///
    /// Warn and Error are bold for rapid triage; Trace is dim to
    /// push noise into the background.
    #[must_use]
    pub fn style(self) -> Style {
        match self {
            Self::Trace => Style::default().fg(SEV_TRACE_FG).dim(),
            Self::Debug => Style::default().fg(SEV_DEBUG_FG),
            Self::Info => Style::default().fg(SEV_INFO_FG),
            Self::Warn => Style::default().fg(SEV_WARN_FG).bold(),
            Self::Error => Style::default().fg(SEV_ERROR_FG).bold(),
        }
    }

    /// A styled [`ftui::text::Span`] rendering the severity badge
    /// (e.g. bold red `ERR`).
    ///
    /// This is the canonical way to render a severity indicator in
    /// any TUI pane.
    #[must_use]
    pub fn styled_badge(self) -> ftui::text::Span<'static> {
        ftui::text::Span::styled(self.badge(), self.style())
    }

    /// Unicode indicator symbol for this severity level.
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Trace => '·',
            Self::Debug => '○',
            Self::Info => '●',
            Self::Warn => '▲',
            Self::Error => '✖',
        }
    }

    /// A styled span with the severity symbol (e.g. bold red `✖`).
    #[must_use]
    pub fn styled_symbol(self) -> ftui::text::Span<'static> {
        let s: String = self.symbol().into();
        ftui::text::Span::styled(s, self.style())
    }
}

pub(crate) const fn event_log_icon(kind: MailEventKind) -> char {
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

/// Format microsecond timestamps as `HH:MM:SS.mmm`.
#[must_use]
pub fn format_event_timestamp(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let millis = micros.rem_euclid(1_000_000) as u64 / 1000;
    let h = secs.rem_euclid(86400) as u64 / 3600;
    let m = secs.rem_euclid(3600) as u64 / 60;
    let s = secs.rem_euclid(60) as u64;
    format!("{h:02}:{m:02}:{s:02}.{millis:03}")
}

fn format_event_context(project: Option<&str>, agent: Option<&str>) -> String {
    match (project, agent) {
        (Some(p), Some(a)) => format!(" [{a}@{p}]"),
        (None, Some(a)) => format!(" [{a}]"),
        (Some(p), None) => format!(" [@{p}]"),
        (None, None) => String::new(),
    }
}

fn truncate_utf8_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ──────────────────────────────────────────────────────────────────────
// VerbosityTier — preset filter levels
// ──────────────────────────────────────────────────────────────────────

/// Preset verbosity tiers controlling which severity levels are visible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerbosityTier {
    /// Only errors and warnings.
    Minimal,
    /// Errors, warnings, and info (default).
    #[default]
    Standard,
    /// Errors, warnings, info, and debug.
    Verbose,
    /// Everything including trace.
    All,
}

impl VerbosityTier {
    /// Whether a given severity passes this tier's filter.
    #[must_use]
    pub const fn includes(self, severity: EventSeverity) -> bool {
        match self {
            Self::All => true,
            Self::Verbose => !matches!(severity, EventSeverity::Trace),
            Self::Standard => matches!(
                severity,
                EventSeverity::Info | EventSeverity::Warn | EventSeverity::Error
            ),
            Self::Minimal => matches!(severity, EventSeverity::Warn | EventSeverity::Error),
        }
    }

    /// Cycle to the next tier.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Minimal => Self::Standard,
            Self::Standard => Self::Verbose,
            Self::Verbose => Self::All,
            Self::All => Self::Minimal,
        }
    }

    /// Short display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::Standard => "Standard",
            Self::Verbose => "Verbose",
            Self::All => "All",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentSummary {
    pub project: String,
    pub name: String,
    pub program: String,
    pub model: String,
    pub last_active_ts: i64,
    pub health: Option<AgentHealthScorecard>,
}

/// Per-project summary for the Projects screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProjectSummary {
    pub id: i64,
    pub slug: String,
    pub human_key: String,
    pub agent_count: u64,
    pub message_count: u64,
    pub reservation_count: u64,
    pub created_at: i64,
}

/// A contact link entry for the Contacts screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContactSummary {
    pub from_agent: String,
    pub to_agent: String,
    pub from_project_slug: String,
    pub to_project_slug: String,
    pub status: String,
    pub reason: String,
    pub updated_ts: i64,
    pub expires_ts: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DbStatSnapshot {
    pub projects: u64,
    pub agents: u64,
    pub messages: u64,
    pub file_reservations: u64,
    pub contact_links: u64,
    pub ack_pending: u64,
    pub agents_list: Vec<AgentSummary>,
    pub projects_list: Vec<ProjectSummary>,
    pub contacts_list: Vec<ContactSummary>,
    pub reservation_snapshots: Vec<ReservationSnapshot>,
    pub timestamp_micros: i64,
}

/// Cached reservation metadata derived from the `file_reservations` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationSnapshot {
    pub id: i64,
    pub project_slug: String,
    pub agent_name: String,
    pub path_pattern: String,
    pub exclusive: bool,
    pub granted_ts: i64,
    pub expires_ts: i64,
    pub released_ts: Option<i64>,
}

impl ReservationSnapshot {
    #[must_use]
    pub const fn is_released(&self) -> bool {
        self.released_ts.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailEvent {
    ToolCallStart {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        tool_name: String,
        params_json: Value,
        project: Option<String>,
        agent: Option<String>,
    },
    ToolCallEnd {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        tool_name: String,
        duration_ms: u64,
        result_preview: Option<String>,
        queries: u64,
        query_time_ms: f64,
        per_table: Vec<(String, u64)>,
        project: Option<String>,
        agent: Option<String>,
    },
    MessageSent {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        id: i64,
        from: String,
        to: Vec<String>,
        subject: String,
        thread_id: String,
        project: String,
        /// Full markdown body.
        body_md: String,
    },
    MessageReceived {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        id: i64,
        from: String,
        to: Vec<String>,
        subject: String,
        thread_id: String,
        project: String,
        /// Full markdown body.
        body_md: String,
    },
    ReservationGranted {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        agent: String,
        paths: Vec<String>,
        exclusive: bool,
        ttl_s: u64,
        project: String,
    },
    ReservationReleased {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        agent: String,
        paths: Vec<String>,
        project: String,
    },
    AgentRegistered {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        name: String,
        program: String,
        model_name: String,
        project: String,
    },
    HttpRequest {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        method: String,
        path: String,
        status: u16,
        duration_ms: u64,
        client_ip: String,
    },
    HealthPulse {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        db_stats: DbStatSnapshot,
    },
    ServerStarted {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
        endpoint: String,
        config_summary: String,
    },
    ServerShutdown {
        seq: u64,
        timestamp_micros: i64,
        source: EventSource,
        redacted: bool,
    },
}

impl MailEvent {
    #[must_use]
    pub fn tool_call_start(
        tool_name: impl Into<String>,
        params_json: Value,
        project: Option<String>,
        agent: Option<String>,
    ) -> Self {
        Self::ToolCallStart {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Tooling,
            redacted: false,
            tool_name: tool_name.into(),
            params_json,
            project,
            agent,
        }
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn tool_call_end(
        tool_name: impl Into<String>,
        duration_ms: u64,
        result_preview: Option<String>,
        queries: u64,
        query_time_ms: f64,
        per_table: Vec<(String, u64)>,
        project: Option<String>,
        agent: Option<String>,
    ) -> Self {
        Self::ToolCallEnd {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Tooling,
            redacted: false,
            tool_name: tool_name.into(),
            duration_ms,
            result_preview,
            queries,
            query_time_ms,
            per_table,
            project,
            agent,
        }
    }

    #[must_use]
    pub fn message_sent(
        id: i64,
        from: impl Into<String>,
        to: Vec<String>,
        subject: impl Into<String>,
        thread_id: impl Into<String>,
        project: impl Into<String>,
        body_md: impl Into<String>,
    ) -> Self {
        Self::MessageSent {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Mail,
            redacted: false,
            id,
            from: from.into(),
            to,
            subject: subject.into(),
            thread_id: thread_id.into(),
            project: project.into(),
            body_md: body_md.into(),
        }
    }

    #[must_use]
    pub fn message_received(
        id: i64,
        from: impl Into<String>,
        to: Vec<String>,
        subject: impl Into<String>,
        thread_id: impl Into<String>,
        project: impl Into<String>,
        body_md: impl Into<String>,
    ) -> Self {
        Self::MessageReceived {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Mail,
            redacted: false,
            id,
            from: from.into(),
            to,
            subject: subject.into(),
            thread_id: thread_id.into(),
            project: project.into(),
            body_md: body_md.into(),
        }
    }

    #[must_use]
    pub fn reservation_granted(
        agent: impl Into<String>,
        paths: Vec<String>,
        exclusive: bool,
        ttl_s: u64,
        project: impl Into<String>,
    ) -> Self {
        Self::ReservationGranted {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Reservations,
            redacted: false,
            agent: agent.into(),
            paths,
            exclusive,
            ttl_s,
            project: project.into(),
        }
    }

    #[must_use]
    pub fn reservation_released(
        agent: impl Into<String>,
        paths: Vec<String>,
        project: impl Into<String>,
    ) -> Self {
        Self::ReservationReleased {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Reservations,
            redacted: false,
            agent: agent.into(),
            paths,
            project: project.into(),
        }
    }

    #[must_use]
    pub fn agent_registered(
        name: impl Into<String>,
        program: impl Into<String>,
        model_name: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        Self::AgentRegistered {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Lifecycle,
            redacted: false,
            name: name.into(),
            program: program.into(),
            model_name: model_name.into(),
            project: project.into(),
        }
    }

    #[must_use]
    pub fn http_request(
        method: impl Into<String>,
        path: impl Into<String>,
        status: u16,
        duration_ms: u64,
        client_ip: impl Into<String>,
    ) -> Self {
        Self::HttpRequest {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Http,
            redacted: false,
            method: method.into(),
            path: path.into(),
            status,
            duration_ms,
            client_ip: client_ip.into(),
        }
    }

    #[must_use]
    pub const fn health_pulse(db_stats: DbStatSnapshot) -> Self {
        Self::HealthPulse {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Database,
            redacted: false,
            db_stats,
        }
    }

    #[must_use]
    pub fn server_started(endpoint: impl Into<String>, config_summary: impl Into<String>) -> Self {
        Self::ServerStarted {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Lifecycle,
            redacted: false,
            endpoint: endpoint.into(),
            config_summary: config_summary.into(),
        }
    }

    #[must_use]
    pub const fn server_shutdown() -> Self {
        Self::ServerShutdown {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Lifecycle,
            redacted: false,
        }
    }

    /// Derive severity from the event data.
    ///
    /// HTTP severity depends on status code; tool starts are trace-level;
    /// health pulses and tool completions are debug; messages, reservations,
    /// and lifecycle events are info; server shutdown is warn.
    #[must_use]
    pub const fn severity(&self) -> EventSeverity {
        match self {
            Self::ToolCallStart { .. } => EventSeverity::Trace,
            Self::HealthPulse { .. } | Self::ToolCallEnd { .. } => EventSeverity::Debug,
            Self::MessageSent { .. }
            | Self::MessageReceived { .. }
            | Self::ReservationGranted { .. }
            | Self::ReservationReleased { .. }
            | Self::AgentRegistered { .. }
            | Self::ServerStarted { .. } => EventSeverity::Info,
            Self::HttpRequest { status, .. } => {
                if *status >= 500 {
                    EventSeverity::Error
                } else if *status >= 400 {
                    EventSeverity::Warn
                } else {
                    EventSeverity::Debug
                }
            }
            Self::ServerShutdown { .. } => EventSeverity::Warn,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> MailEventKind {
        match self {
            Self::ToolCallStart { .. } => MailEventKind::ToolCallStart,
            Self::ToolCallEnd { .. } => MailEventKind::ToolCallEnd,
            Self::MessageSent { .. } => MailEventKind::MessageSent,
            Self::MessageReceived { .. } => MailEventKind::MessageReceived,
            Self::ReservationGranted { .. } => MailEventKind::ReservationGranted,
            Self::ReservationReleased { .. } => MailEventKind::ReservationReleased,
            Self::AgentRegistered { .. } => MailEventKind::AgentRegistered,
            Self::HttpRequest { .. } => MailEventKind::HttpRequest,
            Self::HealthPulse { .. } => MailEventKind::HealthPulse,
            Self::ServerStarted { .. } => MailEventKind::ServerStarted,
            Self::ServerShutdown { .. } => MailEventKind::ServerShutdown,
        }
    }

    #[must_use]
    pub const fn seq(&self) -> u64 {
        match self {
            Self::ToolCallStart { seq, .. }
            | Self::ToolCallEnd { seq, .. }
            | Self::MessageSent { seq, .. }
            | Self::MessageReceived { seq, .. }
            | Self::ReservationGranted { seq, .. }
            | Self::ReservationReleased { seq, .. }
            | Self::AgentRegistered { seq, .. }
            | Self::HttpRequest { seq, .. }
            | Self::HealthPulse { seq, .. }
            | Self::ServerStarted { seq, .. }
            | Self::ServerShutdown { seq, .. } => *seq,
        }
    }

    #[must_use]
    pub const fn timestamp_micros(&self) -> i64 {
        match self {
            Self::ToolCallStart {
                timestamp_micros, ..
            }
            | Self::ToolCallEnd {
                timestamp_micros, ..
            }
            | Self::MessageSent {
                timestamp_micros, ..
            }
            | Self::MessageReceived {
                timestamp_micros, ..
            }
            | Self::ReservationGranted {
                timestamp_micros, ..
            }
            | Self::ReservationReleased {
                timestamp_micros, ..
            }
            | Self::AgentRegistered {
                timestamp_micros, ..
            }
            | Self::HttpRequest {
                timestamp_micros, ..
            }
            | Self::HealthPulse {
                timestamp_micros, ..
            }
            | Self::ServerStarted {
                timestamp_micros, ..
            }
            | Self::ServerShutdown {
                timestamp_micros, ..
            } => *timestamp_micros,
        }
    }

    #[must_use]
    pub const fn source(&self) -> EventSource {
        match self {
            Self::ToolCallStart { source, .. }
            | Self::ToolCallEnd { source, .. }
            | Self::MessageSent { source, .. }
            | Self::MessageReceived { source, .. }
            | Self::ReservationGranted { source, .. }
            | Self::ReservationReleased { source, .. }
            | Self::AgentRegistered { source, .. }
            | Self::HttpRequest { source, .. }
            | Self::HealthPulse { source, .. }
            | Self::ServerStarted { source, .. }
            | Self::ServerShutdown { source, .. } => *source,
        }
    }

    #[must_use]
    pub const fn redacted(&self) -> bool {
        match self {
            Self::ToolCallStart { redacted, .. }
            | Self::ToolCallEnd { redacted, .. }
            | Self::MessageSent { redacted, .. }
            | Self::MessageReceived { redacted, .. }
            | Self::ReservationGranted { redacted, .. }
            | Self::ReservationReleased { redacted, .. }
            | Self::AgentRegistered { redacted, .. }
            | Self::HttpRequest { redacted, .. }
            | Self::HealthPulse { redacted, .. }
            | Self::ServerStarted { redacted, .. }
            | Self::ServerShutdown { redacted, .. } => *redacted,
        }
    }

    /// Build a compact, severity-aware log entry for dashboard/timeline views.
    #[must_use]
    pub fn to_event_log_entry(&self) -> EventLogEntry {
        let kind = self.kind();
        EventLogEntry {
            kind,
            severity: self.severity(),
            seq: self.seq(),
            timestamp_micros: self.timestamp_micros(),
            timestamp: format_event_timestamp(self.timestamp_micros()),
            icon: event_log_icon(kind),
            summary: self.to_event_log_summary(),
        }
    }

    /// Build the one-line summary text shown in log views.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn to_event_log_summary(&self) -> String {
        match self {
            Self::ToolCallStart {
                tool_name,
                project,
                agent,
                ..
            } => {
                let ctx = format_event_context(project.as_deref(), agent.as_deref());
                format!("→ {tool_name}{ctx}")
            }
            Self::ToolCallEnd {
                tool_name,
                duration_ms,
                queries,
                project,
                agent,
                ..
            } => {
                let ctx = format_event_context(project.as_deref(), agent.as_deref());
                format!("{tool_name} {duration_ms}ms q={queries}{ctx}")
            }
            Self::MessageSent {
                from,
                to,
                subject,
                id,
                ..
            } => {
                let recipients = match to.len().cmp(&2) {
                    std::cmp::Ordering::Greater => {
                        format!("{}, {} +{}", to[0], to[1], to.len() - 2)
                    }
                    std::cmp::Ordering::Equal => format!("{} +1", to[0]),
                    std::cmp::Ordering::Less => to.join(", "),
                };
                format!(
                    "#{id} {from} → {recipients}: {}",
                    truncate_utf8_boundary(subject, 40)
                )
            }
            Self::MessageReceived {
                from, subject, id, ..
            } => {
                format!("#{id} from {from}: {}", truncate_utf8_boundary(subject, 40))
            }
            Self::ReservationGranted {
                agent,
                paths,
                exclusive,
                ..
            } => {
                let ex = if *exclusive { " (excl)" } else { "" };
                let path_display = if paths.len() > 2 {
                    format!("{} +{}", paths[0], paths.len() - 1)
                } else {
                    paths.join(", ")
                };
                format!("{agent}: {path_display}{ex}")
            }
            Self::ReservationReleased { agent, paths, .. } => {
                let path_display = if paths.len() > 2 {
                    format!("{} +{}", paths[0], paths.len() - 1)
                } else {
                    paths.join(", ")
                };
                format!("{agent}: released {path_display}")
            }
            Self::AgentRegistered {
                name,
                program,
                model_name,
                project,
                ..
            } => format!("{name} ({program}/{model_name}) in {project}"),
            Self::HttpRequest {
                method,
                path,
                status,
                duration_ms,
                ..
            } => format!("{method} {path} {status} {duration_ms}ms"),
            Self::HealthPulse { db_stats, .. } => format!(
                "p={} a={} m={} r={} c={} ack={}",
                db_stats.projects,
                db_stats.agents,
                db_stats.messages,
                db_stats.file_reservations,
                db_stats.contact_links,
                db_stats.ack_pending
            ),
            Self::ServerStarted { endpoint, .. } => format!("Server started at {endpoint}"),
            Self::ServerShutdown { .. } => "Server shutting down".to_string(),
        }
    }

    const fn set_seq(&mut self, seq: u64) {
        match self {
            Self::ToolCallStart { seq: s, .. }
            | Self::ToolCallEnd { seq: s, .. }
            | Self::MessageSent { seq: s, .. }
            | Self::MessageReceived { seq: s, .. }
            | Self::ReservationGranted { seq: s, .. }
            | Self::ReservationReleased { seq: s, .. }
            | Self::AgentRegistered { seq: s, .. }
            | Self::HttpRequest { seq: s, .. }
            | Self::HealthPulse { seq: s, .. }
            | Self::ServerStarted { seq: s, .. }
            | Self::ServerShutdown { seq: s, .. } => *s = seq,
        }
    }

    const fn set_timestamp_if_unset(&mut self, timestamp_micros: i64) {
        if self.timestamp_micros() > 0 {
            return;
        }
        match self {
            Self::ToolCallStart {
                timestamp_micros: ts,
                ..
            }
            | Self::ToolCallEnd {
                timestamp_micros: ts,
                ..
            }
            | Self::MessageSent {
                timestamp_micros: ts,
                ..
            }
            | Self::MessageReceived {
                timestamp_micros: ts,
                ..
            }
            | Self::ReservationGranted {
                timestamp_micros: ts,
                ..
            }
            | Self::ReservationReleased {
                timestamp_micros: ts,
                ..
            }
            | Self::AgentRegistered {
                timestamp_micros: ts,
                ..
            }
            | Self::HttpRequest {
                timestamp_micros: ts,
                ..
            }
            | Self::HealthPulse {
                timestamp_micros: ts,
                ..
            }
            | Self::ServerStarted {
                timestamp_micros: ts,
                ..
            }
            | Self::ServerShutdown {
                timestamp_micros: ts,
                ..
            } => *ts = timestamp_micros,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRingStats {
    pub capacity: usize,
    pub len: usize,
    pub total_pushed: u64,
    pub dropped_overflow: u64,
    /// Events lost because `try_push` could not acquire the lock.
    pub contention_drops: u64,
    /// Events dropped by the severity-based sampling policy.
    pub sampled_drops: u64,
    pub next_seq: u64,
}

impl EventRingStats {
    /// Total events lost from all causes (overflow + contention + sampling).
    #[must_use]
    pub const fn total_drops(&self) -> u64 {
        self.dropped_overflow
            .saturating_add(self.contention_drops)
            .saturating_add(self.sampled_drops)
    }

    /// Whether any drops have occurred, indicating reduced fidelity.
    #[must_use]
    pub const fn has_drops(&self) -> bool {
        self.contention_drops > 0 || self.sampled_drops > 0 || self.dropped_overflow > 0
    }

    /// Fill ratio as a percentage (0..=100).
    #[must_use]
    pub fn fill_pct(&self) -> u8 {
        if self.capacity == 0 {
            return 100;
        }
        let pct = self
            .len
            .saturating_mul(100)
            .checked_div(self.capacity)
            .unwrap_or(100)
            .min(100);
        u8::try_from(pct).unwrap_or(100)
    }
}

/// Severity-based sampling policy for backpressure.
///
/// When the ring buffer fill ratio exceeds the threshold, low-severity
/// events (Trace, Debug) are sampled at `1:sample_rate` to reduce load
/// while preserving important events (Info and above) at full fidelity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackpressurePolicy {
    /// Fill ratio (0..=100) at which sampling activates.
    pub threshold_pct: u8,
    /// Keep 1 out of N low-severity events when sampling is active.
    pub sample_rate: u64,
}

impl Default for BackpressurePolicy {
    fn default() -> Self {
        Self {
            threshold_pct: 80,
            sample_rate: 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventRingBuffer {
    inner: Arc<Mutex<EventRingBufferInner>>,
    contention_drops: Arc<AtomicU64>,
    sampled_drops: Arc<AtomicU64>,
    sample_counter: Arc<AtomicU64>,
    policy: BackpressurePolicy,
}

#[derive(Debug)]
struct EventRingBufferInner {
    events: VecDeque<MailEvent>,
    capacity: usize,
    next_seq: u64,
    total_pushed: u64,
}

impl EventRingBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_RING_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_policy(capacity, BackpressurePolicy::default())
    }

    #[must_use]
    pub fn with_capacity_and_policy(capacity: usize, policy: BackpressurePolicy) -> Self {
        let bounded_capacity = capacity.max(1);
        let normalized_policy = BackpressurePolicy {
            threshold_pct: policy.threshold_pct.min(100),
            sample_rate: policy.sample_rate.max(1),
        };
        let inner = EventRingBufferInner {
            events: VecDeque::with_capacity(bounded_capacity),
            capacity: bounded_capacity,
            next_seq: 1,
            total_pushed: 0,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            contention_drops: Arc::new(AtomicU64::new(0)),
            sampled_drops: Arc::new(AtomicU64::new(0)),
            sample_counter: Arc::new(AtomicU64::new(0)),
            policy: normalized_policy,
        }
    }

    #[must_use]
    pub fn push(&self, event: MailEvent) -> u64 {
        let mut inner = self.lock_inner();
        Self::push_inner(&mut inner, event)
    }

    /// Non-blocking push with backpressure policy.
    ///
    /// Returns `Ok(seq)` on success, `Err(event)` if the lock is contended
    /// or the event was dropped by the sampling policy.  This is the
    /// preferred path for the server thread where blocking on the TUI
    /// reader is unacceptable.
    #[allow(clippy::result_large_err)]
    pub fn try_push(&self, event: MailEvent) -> Result<u64, MailEvent> {
        let Ok(mut inner) = self.inner.try_lock() else {
            self.contention_drops.fetch_add(1, Ordering::Relaxed);
            return Err(event);
        };

        // Apply sampling policy when buffer is filling up.
        if self.should_sample(&inner, &event) {
            self.sampled_drops.fetch_add(1, Ordering::Relaxed);
            return Err(event);
        }

        Ok(Self::push_inner(&mut inner, event))
    }

    /// Check whether the event should be dropped by the sampling policy.
    ///
    /// Low-severity events (Trace, Debug) are sampled at `1:sample_rate`
    /// when the fill ratio exceeds the policy threshold.
    fn should_sample(&self, inner: &EventRingBufferInner, event: &MailEvent) -> bool {
        let fill_pct = u8::try_from(
            inner
                .events
                .len()
                .saturating_mul(100)
                .checked_div(inner.capacity)
                .unwrap_or(100)
                .min(100),
        )
        .unwrap_or(100);

        if fill_pct < self.policy.threshold_pct {
            return false;
        }

        // Only downsample low-severity events.
        let severity = event.severity();
        if severity >= EventSeverity::Info {
            return false; // always keep Info, Warn, Error
        }

        // Sample: keep 1 out of every N.
        let counter = self.sample_counter.fetch_add(1, Ordering::Relaxed);
        !counter.is_multiple_of(self.policy.sample_rate)
    }

    fn push_inner(inner: &mut EventRingBufferInner, mut event: MailEvent) -> u64 {
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.saturating_add(1);
        event.set_seq(seq);
        event.set_timestamp_if_unset(chrono::Utc::now().timestamp_micros());
        if inner.events.len() >= inner.capacity {
            let scan_limit = inner.events.len().min(LOW_SEVERITY_EVICT_SCAN_LIMIT);
            let drop_idx = inner
                .events
                .iter()
                .take(scan_limit)
                .position(|existing| existing.severity() < EventSeverity::Info)
                .unwrap_or(0);
            if drop_idx == 0 {
                let _ = inner.events.pop_front();
            } else {
                let _ = inner.events.remove(drop_idx);
            }
        }
        inner.events.push_back(event);
        inner.total_pushed = inner.total_pushed.saturating_add(1);
        seq
    }

    #[must_use]
    pub fn iter_recent(&self, limit: usize) -> Vec<MailEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let inner = self.lock_inner();
        Self::collect_recent_from_back(&inner.events, limit)
    }

    #[must_use]
    pub fn try_iter_recent(&self, limit: usize) -> Option<Vec<MailEvent>> {
        if limit == 0 {
            return Some(Vec::new());
        }
        let inner = self.inner.try_lock().ok()?;
        Some(Self::collect_recent_from_back(&inner.events, limit))
    }

    /// Return `(timestamp_micros, severity)` tuples for recent events.
    ///
    /// This avoids cloning full event payloads when only timing/severity
    /// signals are needed for heuristics (ambient health, lightweight stats).
    #[must_use]
    pub fn recent_signals(&self, limit: usize) -> Vec<(i64, EventSeverity)> {
        if limit == 0 {
            return Vec::new();
        }
        let inner = self.lock_inner();
        Self::collect_recent_signals_from_back(&inner.events, limit)
    }

    #[must_use]
    pub fn filter_by_kind(&self, kind: MailEventKind) -> Vec<MailEvent> {
        let inner = self.lock_inner();
        inner
            .events
            .iter()
            .filter(|event| event.kind() == kind)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn since_timestamp(&self, timestamp_micros: i64) -> Vec<MailEvent> {
        let inner = self.lock_inner();
        inner
            .events
            .iter()
            .filter(|event| event.timestamp_micros() > timestamp_micros)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn replay_range(&self, seq_from: u64, seq_to: u64) -> Vec<MailEvent> {
        if seq_from > seq_to {
            return Vec::new();
        }
        let inner = self.lock_inner();
        inner
            .events
            .iter()
            .filter(|event| {
                let seq = event.seq();
                seq >= seq_from && seq <= seq_to
            })
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn events_since_seq(&self, seq: u64) -> Vec<MailEvent> {
        let inner = self.lock_inner();
        Self::collect_events_since_seq(&inner.events, seq)
    }

    /// Return up to `limit` events with sequence numbers greater than `seq`.
    ///
    /// Events are returned oldest-first so callers can advance cursors
    /// monotonically without skipping intermediate records under backlog.
    #[must_use]
    pub fn events_since_seq_limited(&self, seq: u64, limit: usize) -> Vec<MailEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let inner = self.lock_inner();
        Self::collect_events_since_seq_limited(&inner.events, seq, limit)
    }

    #[must_use]
    pub fn try_events_since_seq(&self, seq: u64) -> Option<Vec<MailEvent>> {
        let inner = self.inner.try_lock().ok()?;
        Some(Self::collect_events_since_seq(&inner.events, seq))
    }

    #[inline]
    fn collect_recent_from_back(events: &VecDeque<MailEvent>, limit: usize) -> Vec<MailEvent> {
        let mut out = Vec::with_capacity(limit.min(events.len()));
        out.extend(events.iter().rev().take(limit).cloned());
        out.reverse();
        out
    }

    #[inline]
    fn collect_recent_signals_from_back(
        events: &VecDeque<MailEvent>,
        limit: usize,
    ) -> Vec<(i64, EventSeverity)> {
        let mut out = Vec::with_capacity(limit.min(events.len()));
        for event in events.iter().rev().take(limit) {
            out.push((event.timestamp_micros(), event.severity()));
        }
        out.reverse();
        out
    }

    #[inline]
    fn collect_events_since_seq(events: &VecDeque<MailEvent>, seq: u64) -> Vec<MailEvent> {
        // Events are appended with monotonic sequence numbers and kept in order.
        // Scan from the newest event backwards so steady-state polling
        // (`seq` already at tail) becomes O(1) instead of O(capacity).
        let mut out = Vec::new();
        for event in events.iter().rev() {
            if event.seq() <= seq {
                break;
            }
            out.push(event.clone());
        }
        out.reverse();
        out
    }

    #[inline]
    fn collect_events_since_seq_limited(
        events: &VecDeque<MailEvent>,
        seq: u64,
        limit: usize,
    ) -> Vec<MailEvent> {
        // Steady-state pollers usually ask for events past the current tail.
        // Fast-path that case to avoid scanning the entire ring on every tick.
        if events.back().is_none_or(|event| event.seq() <= seq) {
            return Vec::new();
        }

        // Iterate newest -> oldest so the common case (cursor near tail)
        // exits quickly like `collect_events_since_seq`. Keep only the
        // oldest `limit` unseen events in a bounded deque.
        let mut selected_rev: VecDeque<MailEvent> =
            VecDeque::with_capacity(limit.min(events.len()));
        for event in events.iter().rev() {
            if event.seq() <= seq {
                break;
            }
            if selected_rev.len() == limit {
                let _ = selected_rev.pop_front();
            }
            selected_rev.push_back(event.clone());
        }
        selected_rev.into_iter().rev().collect()
    }

    #[must_use]
    pub fn stats(&self) -> EventRingStats {
        let inner = self.lock_inner();
        EventRingStats {
            capacity: inner.capacity,
            len: inner.events.len(),
            total_pushed: inner.total_pushed,
            dropped_overflow: inner.total_pushed.saturating_sub(inner.events.len() as u64),
            contention_drops: self.contention_drops.load(Ordering::Relaxed),
            sampled_drops: self.sampled_drops.load(Ordering::Relaxed),
            next_seq: inner.next_seq,
        }
    }

    /// Current backpressure policy.
    #[must_use]
    pub const fn policy(&self) -> BackpressurePolicy {
        self.policy
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_inner().events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock_inner().events.is_empty()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.lock_inner().capacity
    }

    fn lock_inner(&self) -> MutexGuard<'_, EventRingBufferInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for EventRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// DataProvider Trait
// ──────────────────────────────────────────────────────────────────────

/// Abstraction layer above `VirtualizedList` for paginated data sources.
///
/// `VirtualizedList` operates on a slice `&[T]`. For large datasets, the
/// `DataProvider` layer:
/// - Provides a windowed slice to `VirtualizedList` (only loaded pages)
/// - Handles pagination for DB-backed sources
/// - Handles the full dataset for in-memory sources (Timeline ring buffer)
pub trait DataProvider {
    /// The item type that implements `RenderItem`.
    type Item: RenderItem;

    /// Total number of items in the data source.
    fn total_count(&self) -> usize;

    /// Return a slice of items for the given window.
    ///
    /// `start` is the 0-based index into the full dataset.
    /// `count` is the maximum number of items to return.
    /// The returned slice may be shorter than `count` if there aren't enough items.
    fn window(&self, start: usize, count: usize) -> &[Self::Item];

    /// Prefetch data around the given index (no-op for in-memory sources).
    ///
    /// DB-backed providers use this to load pages when scrolling approaches
    /// the edge of the currently loaded window.
    fn prefetch(&mut self, around: usize);

    /// Invalidate cached data, forcing a reload on next access.
    fn invalidate(&mut self);
}

/// Fenwick-backed viewport index for virtualized row ranges.
///
/// `VirtualizedList` scroll state is item-index based, but large data sources
/// often need fast offset→index mapping for variable row heights. This index
/// keeps that mapping O(log n) via a Fenwick tree.
#[derive(Debug, Clone)]
struct FenwickViewportIndex {
    heights: Vec<u32>,
    tree: FenwickTree,
}

impl Default for FenwickViewportIndex {
    fn default() -> Self {
        Self {
            heights: Vec::new(),
            tree: FenwickTree::new(0),
        }
    }
}

impl FenwickViewportIndex {
    fn rebuild_from_rows<T: RenderItem>(&mut self, rows: &[T]) {
        self.heights.clear();
        self.heights.reserve(rows.len());
        for row in rows {
            self.heights.push(u32::from(row.height().max(1)));
        }
        self.tree = if self.heights.is_empty() {
            FenwickTree::new(0)
        } else {
            FenwickTree::from_values(&self.heights)
        };
    }

    #[cfg(test)]
    fn rebuild_from_heights(&mut self, heights: &[u16]) {
        self.heights.clear();
        self.heights.reserve(heights.len());
        for &height in heights {
            self.heights.push(u32::from(height.max(1)));
        }
        self.tree = if self.heights.is_empty() {
            FenwickTree::new(0)
        } else {
            FenwickTree::from_values(&self.heights)
        };
    }

    fn total_height(&self) -> u32 {
        self.tree.total()
    }

    fn index_for_offset(&self, offset: u32) -> usize {
        if self.heights.is_empty() {
            return 0;
        }
        if offset >= self.total_height() {
            return self.heights.len();
        }
        self.tree
            .find_prefix(offset)
            .map_or(0, |idx| idx.saturating_add(1))
            .min(self.heights.len())
    }

    fn visible_count_from_index(&self, start: usize, viewport_height: u16) -> usize {
        if start >= self.heights.len() || viewport_height == 0 {
            return 0;
        }
        let mut consumed = 0u32;
        let max_height = u32::from(viewport_height);
        let mut count = 0usize;
        for &height in &self.heights[start..] {
            let next = consumed.saturating_add(height.max(1));
            if next > max_height {
                break;
            }
            consumed = next;
            count += 1;
        }
        count
    }

    fn range_for_offset(&self, offset: u32, viewport_height: u16) -> Range<usize> {
        let start = self.index_for_offset(offset);
        if start >= self.heights.len() {
            return start..start;
        }
        let visible = self.visible_count_from_index(start, viewport_height);
        let end = start.saturating_add(visible).min(self.heights.len());
        start..end
    }
}

// ──────────────────────────────────────────────────────────────────────
// TimelineRow — RenderItem for timeline events
// ──────────────────────────────────────────────────────────────────────

/// A pre-formatted timeline row for virtualized rendering.
///
/// This struct contains all data needed to render a single timeline event
/// row without accessing the original `MailEvent`.
#[derive(Debug, Clone)]
pub struct TimelineRow {
    /// Sequence number for identification and navigation.
    pub seq: u64,
    /// Event kind for filtering and icon selection.
    pub kind: MailEventKind,
    /// Derived severity for styling.
    pub severity: EventSeverity,
    /// Event source for filtering.
    pub source: EventSource,
    /// Formatted timestamp string (HH:MM:SS.mmm).
    pub timestamp: String,
    /// Icon character for the event type.
    pub icon: char,
    /// One-line summary text.
    pub summary: String,
    /// Raw timestamp for sorting/jumping.
    pub timestamp_micros: i64,
}

impl TimelineRow {
    /// Create a `TimelineRow` from a `MailEvent`.
    #[must_use]
    pub fn from_event(event: &MailEvent) -> Self {
        let kind = event.kind();
        let severity = event.severity();
        let source = event.source();
        let ts_micros = event.timestamp_micros();

        // Format timestamp as HH:MM:SS.mmm (Euclidean for negative safety)
        let secs = ts_micros.div_euclid(1_000_000);
        let millis = ts_micros.rem_euclid(1_000_000) / 1000;
        let hours = secs.rem_euclid(86400) / 3600;
        let mins = secs.rem_euclid(3600) / 60;
        let s = secs.rem_euclid(60);
        let timestamp = format!("{hours:02}:{mins:02}:{s:02}.{millis:03}");

        // Icon based on event kind
        let icon = match kind {
            MailEventKind::ToolCallStart => '▶',
            MailEventKind::ToolCallEnd => '■',
            MailEventKind::MessageSent => '↑',
            MailEventKind::MessageReceived => '↓',
            MailEventKind::ReservationGranted => '🔒',
            MailEventKind::ReservationReleased => '🔓',
            MailEventKind::AgentRegistered => '⊕',
            MailEventKind::HttpRequest => '⇄',
            MailEventKind::HealthPulse => '♥',
            MailEventKind::ServerStarted => '🚀',
            MailEventKind::ServerShutdown => '⏹',
        };

        // One-line summary based on event type
        let summary = Self::format_summary(event);

        Self {
            seq: event.seq(),
            kind,
            severity,
            source,
            timestamp,
            icon,
            summary,
            timestamp_micros: ts_micros,
        }
    }

    fn format_summary(event: &MailEvent) -> String {
        match event {
            MailEvent::ToolCallStart { tool_name, .. } => {
                format!("→ {tool_name}")
            }
            MailEvent::ToolCallEnd {
                tool_name,
                duration_ms,
                ..
            } => {
                format!("← {tool_name} ({duration_ms}ms)")
            }
            MailEvent::MessageSent {
                from, to, subject, ..
            }
            | MailEvent::MessageReceived {
                from, to, subject, ..
            } => {
                let recipients = if to.len() > 1 {
                    format!("{} +{}", to[0], to.len() - 1)
                } else {
                    to.first().cloned().unwrap_or_default()
                };
                format!("{from} → {recipients}: {subject}")
            }
            MailEvent::ReservationGranted {
                agent,
                paths,
                exclusive,
                ..
            } => {
                let mode = if *exclusive { "excl" } else { "shared" };
                let path_count = paths.len();
                format!("{agent} locked {path_count} path(s) [{mode}]")
            }
            MailEvent::ReservationReleased { agent, paths, .. } => {
                format!("{agent} released {} path(s)", paths.len())
            }
            MailEvent::AgentRegistered { name, program, .. } => {
                format!("{name} ({program}) registered")
            }
            MailEvent::HttpRequest {
                method,
                path,
                status,
                duration_ms,
                ..
            } => {
                format!("{method} {path} → {status} ({duration_ms}ms)")
            }
            MailEvent::HealthPulse { db_stats, .. } => {
                format!(
                    "agents={} msgs={} rsv={}",
                    db_stats.agents, db_stats.messages, db_stats.file_reservations
                )
            }
            MailEvent::ServerStarted { endpoint, .. } => {
                format!("Server started on {endpoint}")
            }
            MailEvent::ServerShutdown { .. } => "Server shutdown".to_string(),
        }
    }
}

impl RenderItem for TimelineRow {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 10 {
            return;
        }

        // Build the line: [seq] [timestamp] [icon] [summary]
        let seq_str = format!("{:>6}", self.seq);
        let severity_style = self.severity.style();
        let tp = crate::tui_theme::TuiThemePalette::current();

        let base_style = if selected {
            Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
        } else {
            Style::default()
        };

        let line = Line::from_spans([
            Span::styled(seq_str, base_style.fg(tp.text_muted)),
            Span::raw(" "),
            Span::styled(self.timestamp.clone(), base_style.fg(tp.text_secondary)),
            Span::raw(" "),
            Span::styled(self.icon.to_string(), severity_style),
            Span::raw(" "),
            Span::styled(self.summary.clone(), base_style),
        ]);

        let paragraph = ftui::widgets::paragraph::Paragraph::new(line);
        paragraph.render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

// ──────────────────────────────────────────────────────────────────────
// TimelineDataProvider — wraps EventRingBuffer
// ──────────────────────────────────────────────────────────────────────

/// `DataProvider` for the Timeline screen, backed by `EventRingBuffer`.
///
/// This provider converts `MailEvents` to `TimelineRows` on demand and caches
/// the converted rows for efficient window access.
pub struct TimelineDataProvider {
    /// Reference to the shared event ring buffer.
    ring: Arc<EventRingBuffer>,
    /// Cached converted rows.
    rows: Vec<TimelineRow>,
    /// Last sequence number we processed.
    last_seq: u64,
    /// Whether the cache is invalidated.
    dirty: bool,
    /// Fenwick-backed index for viewport calculations.
    height_index: FenwickViewportIndex,
}

impl TimelineDataProvider {
    /// Create a new provider wrapping the given ring buffer.
    #[must_use]
    pub fn new(ring: Arc<EventRingBuffer>) -> Self {
        Self {
            ring,
            rows: Vec::new(),
            last_seq: 0,
            dirty: true,
            height_index: FenwickViewportIndex::default(),
        }
    }

    /// Refresh the cache by converting any new events.
    pub fn refresh(&mut self) {
        if self.dirty {
            // Full rebuild — only clear dirty if we actually got the lock.
            if let Some(events) = self.ring.try_iter_recent(self.ring.capacity()) {
                self.rows.clear();
                self.rows.reserve(events.len());
                for event in &events {
                    self.rows.push(TimelineRow::from_event(event));
                    self.last_seq = self.last_seq.max(event.seq());
                }
                self.dirty = false;
            }
        } else {
            // Incremental update
            let new_events = self.ring.events_since_seq(self.last_seq);
            for event in &new_events {
                if event.seq() > self.last_seq {
                    self.rows.push(TimelineRow::from_event(event));
                    self.last_seq = event.seq();
                }
            }
            // Trim to ring buffer capacity if needed
            let capacity = self.ring.capacity();
            if self.rows.len() > capacity {
                let excess = self.rows.len() - capacity;
                self.rows.drain(..excess);
            }
        }
        self.height_index.rebuild_from_rows(&self.rows);
    }

    /// Compute the item range visible at a row-offset and viewport height.
    #[must_use]
    pub fn viewport_range_for_offset(
        &self,
        row_offset: usize,
        viewport_height: u16,
    ) -> Range<usize> {
        let offset = u32::try_from(row_offset).unwrap_or(u32::MAX);
        self.height_index.range_for_offset(offset, viewport_height)
    }

    /// Return the visible window for a row-offset and viewport height.
    #[must_use]
    pub fn window_for_viewport_offset(
        &self,
        row_offset: usize,
        viewport_height: u16,
    ) -> &[TimelineRow] {
        let range = self.viewport_range_for_offset(row_offset, viewport_height);
        &self.rows[range]
    }
}

impl DataProvider for TimelineDataProvider {
    type Item = TimelineRow;

    fn total_count(&self) -> usize {
        self.rows.len()
    }

    fn window(&self, start: usize, count: usize) -> &[Self::Item] {
        let len = self.rows.len();
        if start >= len {
            return &[];
        }
        let end = (start + count).min(len);
        &self.rows[start..end]
    }

    fn prefetch(&mut self, _around: usize) {
        // No-op for in-memory source; data is already available.
        // Just refresh to pick up any new events.
        self.refresh();
    }

    fn invalidate(&mut self) {
        self.dirty = true;
    }
}

// ──────────────────────────────────────────────────────────────────────
// MessageRow — RenderItem for message search results
// ──────────────────────────────────────────────────────────────────────

/// A pre-formatted message row for virtualized rendering.
#[derive(Debug, Clone)]
pub struct MessageRow {
    /// Message ID.
    pub id: i64,
    /// Sender agent name.
    pub from_agent: String,
    /// Recipient(s) summary.
    pub to_agents: String,
    /// Message subject.
    pub subject: String,
    /// Project slug.
    pub project_slug: String,
    /// Thread ID.
    pub thread_id: String,
    /// Formatted timestamp.
    pub timestamp: String,
    /// Importance level.
    pub importance: String,
    /// Whether acknowledgement is required.
    pub ack_required: bool,
    /// Raw timestamp for sorting.
    pub timestamp_micros: i64,
}

impl RenderItem for MessageRow {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let base_style = if selected {
            Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
        } else {
            Style::default()
        };

        // Importance badge
        let importance_style = match self.importance.as_str() {
            "urgent" => Style::default().fg(tp.severity_critical).bold(),
            "high" => Style::default().fg(tp.severity_warn),
            _ => Style::default().fg(tp.text_muted),
        };

        let ack_indicator = if self.ack_required { "◉" } else { " " };

        let line = Line::from_spans([
            Span::styled(ack_indicator, Style::default().fg(tp.severity_warn)),
            Span::raw(" "),
            Span::styled(self.timestamp.clone(), base_style.fg(tp.text_secondary)),
            Span::raw(" "),
            Span::styled(self.from_agent.clone(), base_style.fg(tp.status_accent)),
            Span::raw(" → "),
            Span::styled(self.to_agents.clone(), base_style.fg(tp.status_good)),
            Span::raw(" "),
            Span::styled(self.importance.clone(), importance_style),
            Span::raw(" "),
            Span::styled(self.subject.clone(), base_style),
        ]);

        let paragraph = ftui::widgets::paragraph::Paragraph::new(line);
        paragraph.render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

// ──────────────────────────────────────────────────────────────────────
// SearchHitRow — RenderItem for FTS search results
// ──────────────────────────────────────────────────────────────────────

/// A pre-formatted search result row for virtualized rendering.
#[derive(Debug, Clone)]
pub struct SearchHitRow {
    /// Document ID (message or other entity).
    pub id: i64,
    /// Subject line.
    pub subject: String,
    /// Snippet with search term highlighted.
    pub snippet: String,
    /// Sender/source.
    pub from: String,
    /// Relevance score (higher = better match).
    pub score: f32,
    /// Timestamp for sorting.
    pub timestamp_micros: i64,
}

impl RenderItem for SearchHitRow {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let base_style = if selected {
            Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
        } else {
            Style::default()
        };

        let score_str = format!("{:.1}", self.score);

        let line = Line::from_spans([
            Span::styled(score_str, base_style.fg(tp.status_accent)),
            Span::raw(" "),
            Span::styled(self.from.clone(), base_style.fg(tp.text_secondary)),
            Span::raw(": "),
            Span::styled(self.subject.clone(), base_style.bold()),
            Span::raw(" — "),
            Span::styled(self.snippet.clone(), base_style.fg(tp.text_muted)),
        ]);

        let paragraph = ftui::widgets::paragraph::Paragraph::new(line);
        paragraph.render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

// ──────────────────────────────────────────────────────────────────────
// ExplorerRow — RenderItem for explorer/browser results
// ──────────────────────────────────────────────────────────────────────

/// A pre-formatted explorer row for virtualized rendering.
#[derive(Debug, Clone)]
pub struct ExplorerRow {
    /// Entity ID.
    pub id: i64,
    /// Direction indicator (↑ outbound, ↓ inbound).
    pub direction: char,
    /// Sender/recipient depending on direction.
    pub primary_agent: String,
    /// Other party.
    pub secondary_agent: String,
    /// Subject line.
    pub subject: String,
    /// Formatted date.
    pub date: String,
    /// Raw timestamp for sorting.
    pub timestamp_micros: i64,
}

impl RenderItem for ExplorerRow {
    fn render(&self, area: Rect, frame: &mut Frame, selected: bool, _skip_rows: u16) {
        use ftui::widgets::Widget;

        if area.height == 0 || area.width < 20 {
            return;
        }

        let tp = crate::tui_theme::TuiThemePalette::current();
        let base_style = if selected {
            Style::default().fg(tp.selection_fg).bg(tp.selection_bg)
        } else {
            Style::default()
        };

        let direction_style = if self.direction == '↑' {
            Style::default().fg(tp.status_good)
        } else {
            Style::default().fg(tp.status_accent)
        };

        let line = Line::from_spans([
            Span::styled(self.direction.to_string(), direction_style),
            Span::raw(" "),
            Span::styled(self.date.clone(), base_style.fg(tp.text_secondary)),
            Span::raw(" "),
            Span::styled(self.primary_agent.clone(), base_style.fg(tp.status_accent)),
            Span::raw(" → "),
            Span::styled(self.secondary_agent.clone(), base_style.fg(tp.status_good)),
            Span::raw(" "),
            Span::styled(self.subject.clone(), base_style),
        ]);

        let paragraph = ftui::widgets::paragraph::Paragraph::new(line);
        paragraph.render(area, frame);
    }

    fn height(&self) -> u16 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tool_start(name: &str) -> MailEvent {
        MailEvent::tool_call_start(name, Value::Null, None, None)
    }

    fn sample_http(path: &str, status: u16) -> MailEvent {
        MailEvent::http_request("GET", path, status, 5, "127.0.0.1")
    }

    #[test]
    fn ring_buffer_assigns_monotonic_sequences() {
        let ring = EventRingBuffer::with_capacity(8);
        assert_eq!(ring.push(sample_tool_start("fetch_inbox")), 1);
        assert_eq!(ring.push(sample_tool_start("send_message")), 2);
        assert_eq!(ring.push(sample_http("/mcp/", 200)), 3);

        let seqs: Vec<u64> = ring
            .iter_recent(10)
            .into_iter()
            .map(|event| event.seq())
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn ring_buffer_drops_oldest_when_capacity_exceeded() {
        let ring = EventRingBuffer::with_capacity(3);
        for idx in 0..5 {
            let _ = ring.push(sample_http(&format!("/req/{idx}"), 200));
        }

        let events = ring.iter_recent(10);
        let seqs: Vec<u64> = events.iter().map(MailEvent::seq).collect();
        assert_eq!(seqs, vec![3, 4, 5]);

        let stats = ring.stats();
        assert_eq!(stats.capacity, 3);
        assert_eq!(stats.len, 3);
        assert_eq!(stats.total_pushed, 5);
        assert_eq!(stats.dropped_overflow, 2);
    }

    #[test]
    fn ring_buffer_prefers_evicting_low_severity_first() {
        let ring = EventRingBuffer::with_capacity(3);
        let _ = ring.push(MailEvent::message_sent(
            1,
            "A",
            vec!["B".to_string()],
            "m1",
            "t1",
            "p",
            "",
        ));
        let _ = ring.push(MailEvent::message_sent(
            2,
            "A",
            vec!["B".to_string()],
            "m2",
            "t2",
            "p",
            "",
        ));
        let _ = ring.push(MailEvent::tool_call_start(
            "fetch_inbox",
            Value::Null,
            None,
            None,
        ));
        let _ = ring.push(MailEvent::message_received(
            4,
            "B",
            vec!["A".to_string()],
            "m3",
            "t3",
            "p",
            "",
        ));

        let kinds: Vec<MailEventKind> = ring.iter_recent(10).iter().map(MailEvent::kind).collect();
        assert_eq!(
            kinds,
            vec![
                MailEventKind::MessageSent,
                MailEventKind::MessageSent,
                MailEventKind::MessageReceived
            ]
        );
    }

    #[test]
    fn ring_buffer_evicts_oldest_when_only_high_severity_events_exist() {
        let ring = EventRingBuffer::with_capacity(3);
        let _ = ring.push(MailEvent::message_sent(1, "A", vec![], "m1", "t1", "p", ""));
        let _ = ring.push(MailEvent::message_sent(2, "A", vec![], "m2", "t2", "p", ""));
        let _ = ring.push(MailEvent::message_sent(3, "A", vec![], "m3", "t3", "p", ""));
        let _ = ring.push(MailEvent::message_received(
            4,
            "B",
            vec![],
            "m4",
            "t4",
            "p",
            "",
        ));

        let seqs: Vec<u64> = ring.iter_recent(10).iter().map(MailEvent::seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);
    }

    #[test]
    fn filter_by_kind_returns_only_requested_events() {
        let ring = EventRingBuffer::with_capacity(16);
        let _ = ring.push(sample_http("/ok", 200));
        let _ = ring.push(sample_tool_start("fetch_inbox"));
        let _ = ring.push(sample_http("/bad", 500));

        let tool_events = ring.filter_by_kind(MailEventKind::ToolCallStart);
        assert_eq!(tool_events.len(), 1);
        assert_eq!(tool_events[0].kind(), MailEventKind::ToolCallStart);
    }

    #[test]
    fn since_timestamp_returns_newer_events_only() {
        let ring = EventRingBuffer::with_capacity(8);
        // Use explicit timestamps to avoid sub-microsecond timing collisions.
        let mut ev_a = sample_http("/a", 200);
        ev_a.set_timestamp_if_unset(1_000_000);
        let _ = ring.push(ev_a);
        let mut ev_b = sample_http("/b", 200);
        ev_b.set_timestamp_if_unset(2_000_000);
        let _ = ring.push(ev_b);
        let cutoff = ring.iter_recent(2)[0].timestamp_micros();
        assert_eq!(cutoff, 1_000_000);
        let mut ev_c = sample_http("/c", 200);
        ev_c.set_timestamp_if_unset(3_000_000);
        let _ = ring.push(ev_c);

        let newer = ring.since_timestamp(cutoff);
        assert_eq!(newer.len(), 2);
        assert!(newer.iter().all(|event| event.timestamp_micros() > cutoff));
    }

    #[test]
    fn replay_range_and_events_since_seq_work() {
        let ring = EventRingBuffer::with_capacity(10);
        for idx in 0..6 {
            let _ = ring.push(sample_http(&format!("/r/{idx}"), 200));
        }

        let replay = ring.replay_range(2, 4);
        let replay_seqs: Vec<u64> = replay.iter().map(MailEvent::seq).collect();
        assert_eq!(replay_seqs, vec![2, 3, 4]);

        let since = ring.events_since_seq(4);
        let since_seqs: Vec<u64> = since.iter().map(MailEvent::seq).collect();
        assert_eq!(since_seqs, vec![5, 6]);
    }

    #[test]
    fn events_since_seq_limited_preserves_order_and_limit() {
        let ring = EventRingBuffer::with_capacity(10);
        for idx in 0..6 {
            let _ = ring.push(sample_http(&format!("/limited/{idx}"), 200));
        }

        let first_batch = ring.events_since_seq_limited(2, 2);
        let first_seqs: Vec<u64> = first_batch.iter().map(MailEvent::seq).collect();
        assert_eq!(first_seqs, vec![3, 4]);

        let second_batch = ring.events_since_seq_limited(4, 2);
        let second_seqs: Vec<u64> = second_batch.iter().map(MailEvent::seq).collect();
        assert_eq!(second_seqs, vec![5, 6]);

        assert!(ring.events_since_seq_limited(6, 2).is_empty());
        assert!(ring.events_since_seq_limited(10, 2).is_empty());
        assert!(ring.events_since_seq_limited(2, 0).is_empty());
    }

    #[test]
    fn iter_recent_preserves_order_of_selected_slice() {
        let ring = EventRingBuffer::with_capacity(10);
        for idx in 0..6 {
            let _ = ring.push(sample_http(&format!("/x/{idx}"), 200));
        }
        let recent = ring.iter_recent(3);
        let seqs: Vec<u64> = recent.iter().map(MailEvent::seq).collect();
        assert_eq!(seqs, vec![4, 5, 6]);
    }

    #[test]
    fn recent_signals_preserve_selected_order() {
        let ring = EventRingBuffer::with_capacity(4);
        let mut a = sample_http("/ok", 200);
        a.set_timestamp_if_unset(1_000_000);
        let _ = ring.push(a);
        let mut b = sample_http("/err", 500);
        b.set_timestamp_if_unset(2_000_000);
        let _ = ring.push(b);

        let signals = ring.recent_signals(2);
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0], (1_000_000, EventSeverity::Debug));
        assert_eq!(signals[1], (2_000_000, EventSeverity::Error));
    }

    #[test]
    fn serde_roundtrip_covers_all_event_variants() {
        let events = vec![
            MailEvent::ToolCallStart {
                seq: 1,
                timestamp_micros: 101,
                source: EventSource::Tooling,
                redacted: false,
                tool_name: "fetch_inbox".to_string(),
                params_json: serde_json::json!({"limit": 10}),
                project: Some("proj".to_string()),
                agent: Some("TealMeadow".to_string()),
            },
            MailEvent::ToolCallEnd {
                seq: 2,
                timestamp_micros: 102,
                source: EventSource::Tooling,
                redacted: false,
                tool_name: "fetch_inbox".to_string(),
                duration_ms: 3,
                result_preview: Some("{\"ok\":true}".to_string()),
                queries: 2,
                query_time_ms: 0.25,
                per_table: vec![("messages".to_string(), 1)],
                project: Some("proj".to_string()),
                agent: Some("TealMeadow".to_string()),
            },
            MailEvent::MessageSent {
                seq: 3,
                timestamp_micros: 103,
                source: EventSource::Mail,
                redacted: false,
                id: 11,
                from: "TealMeadow".to_string(),
                to: vec!["IndigoRidge".to_string()],
                subject: "start".to_string(),
                thread_id: "br-10wc.15".to_string(),
                project: "proj".to_string(),
                body_md: "Starting work on the task".to_string(),
            },
            MailEvent::MessageReceived {
                seq: 4,
                timestamp_micros: 104,
                source: EventSource::Mail,
                redacted: false,
                id: 12,
                from: "IndigoRidge".to_string(),
                to: vec!["TealMeadow".to_string()],
                subject: "ack".to_string(),
                thread_id: "br-10wc.15".to_string(),
                project: "proj".to_string(),
                body_md: "Acknowledged".to_string(),
            },
            MailEvent::ReservationGranted {
                seq: 5,
                timestamp_micros: 105,
                source: EventSource::Reservations,
                redacted: false,
                agent: "TealMeadow".to_string(),
                paths: vec!["src/**".to_string()],
                exclusive: true,
                ttl_s: 3600,
                project: "proj".to_string(),
            },
            MailEvent::ReservationReleased {
                seq: 6,
                timestamp_micros: 106,
                source: EventSource::Reservations,
                redacted: false,
                agent: "TealMeadow".to_string(),
                paths: vec!["src/**".to_string()],
                project: "proj".to_string(),
            },
            MailEvent::AgentRegistered {
                seq: 7,
                timestamp_micros: 107,
                source: EventSource::Lifecycle,
                redacted: false,
                name: "TealMeadow".to_string(),
                program: "codex-cli".to_string(),
                model_name: "gpt-5".to_string(),
                project: "proj".to_string(),
            },
            MailEvent::HttpRequest {
                seq: 8,
                timestamp_micros: 108,
                source: EventSource::Http,
                redacted: false,
                method: "POST".to_string(),
                path: "/mcp/".to_string(),
                status: 200,
                duration_ms: 2,
                client_ip: "127.0.0.1".to_string(),
            },
            MailEvent::HealthPulse {
                seq: 9,
                timestamp_micros: 109,
                source: EventSource::Database,
                redacted: false,
                db_stats: DbStatSnapshot {
                    projects: 1,
                    agents: 2,
                    messages: 3,
                    file_reservations: 4,
                    contact_links: 5,
                    ack_pending: 6,
                    agents_list: vec![AgentSummary {
                        project: String::new(),
                        name: "TealMeadow".to_string(),
                        program: "codex-cli".to_string(),
                        model: String::new(),
                        last_active_ts: 99,
                        health: None,
                    }],
                    timestamp_micros: 109,
                    ..Default::default()
                },
            },
            MailEvent::ServerStarted {
                seq: 10,
                timestamp_micros: 110,
                source: EventSource::Lifecycle,
                redacted: false,
                endpoint: "http://127.0.0.1:8765/mcp/".to_string(),
                config_summary: "auth=on".to_string(),
            },
            MailEvent::ServerShutdown {
                seq: 11,
                timestamp_micros: 111,
                source: EventSource::Lifecycle,
                redacted: false,
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).expect("serialize MailEvent");
            let parsed: MailEvent = serde_json::from_str(&json).expect("deserialize MailEvent");
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn try_push_succeeds_when_unlocked() {
        let ring = EventRingBuffer::with_capacity(8);
        let result = ring.try_push(sample_http("/ok", 200)).ok();
        assert_eq!(result, Some(1));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn try_push_returns_none_when_locked() {
        let ring = EventRingBuffer::with_capacity(8);
        let _guard = ring
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring2 = ring.clone();
        assert!(ring2.try_push(sample_http("/blocked", 500)).is_err());
    }

    #[test]
    fn try_iter_recent_returns_none_when_locked() {
        let ring = EventRingBuffer::with_capacity(8);
        let _ = ring.push(sample_http("/ok", 200));
        let _guard = ring
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring2 = ring.clone();
        assert!(ring2.try_iter_recent(1).is_none());
    }

    #[test]
    fn try_events_since_seq_returns_none_when_locked() {
        let ring = EventRingBuffer::with_capacity(8);
        let _ = ring.push(sample_http("/ok", 200));
        let _guard = ring
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring2 = ring.clone();
        assert!(ring2.try_events_since_seq(0).is_none());
    }

    #[test]
    fn events_since_seq_zero_returns_all() {
        let ring = EventRingBuffer::with_capacity(10);
        for i in 0..4 {
            let _ = ring.push(sample_http(&format!("/all/{i}"), 200));
        }
        assert_eq!(ring.events_since_seq(0).len(), 4);
    }

    #[test]
    fn default_ring_buffer_uses_default_capacity() {
        let ring = EventRingBuffer::default();
        let stats = ring.stats();
        assert_eq!(stats.capacity, DEFAULT_EVENT_RING_CAPACITY);
        assert!(ring.is_empty());
    }

    #[test]
    fn shared_clone_sees_same_data() {
        let ring = EventRingBuffer::with_capacity(10);
        let ring2 = ring.clone();
        let _ = ring.push(sample_http("/a", 200));
        assert_eq!(ring2.len(), 1);
        let _ = ring2.push(sample_tool_start("test"));
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn accessor_methods_return_correct_values() {
        let ring = EventRingBuffer::with_capacity(8);
        let _ = ring.push(MailEvent::ToolCallStart {
            seq: 0,
            timestamp_micros: 42_000,
            source: EventSource::Tooling,
            redacted: true,
            tool_name: "send_message".into(),
            params_json: serde_json::json!({"to": "test"}),
            project: Some("proj".into()),
            agent: Some("GoldFox".into()),
        });
        let events = ring.iter_recent(1);
        let e = &events[0];
        assert_eq!(e.seq(), 1);
        assert_eq!(e.timestamp_micros(), 42_000);
        assert_eq!(e.source(), EventSource::Tooling);
        assert_eq!(e.kind(), MailEventKind::ToolCallStart);
        assert!(e.redacted());
    }

    #[test]
    fn all_kinds_have_correct_discriminant() {
        let events: Vec<MailEvent> = vec![
            MailEvent::tool_call_start("t", Value::Null, None, None),
            MailEvent::tool_call_end("t", 1, None, 0, 0.0, vec![], None, None),
            MailEvent::message_sent(1, "a", vec![], "s", "t", "p", ""),
            MailEvent::message_received(1, "a", vec![], "s", "t", "p", ""),
            MailEvent::reservation_granted("a", vec![], true, 60, "p"),
            MailEvent::reservation_released("a", vec![], "p"),
            MailEvent::agent_registered("n", "prog", "model", "p"),
            MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1"),
            MailEvent::health_pulse(DbStatSnapshot::default()),
            MailEvent::server_started("http://localhost", "test"),
            MailEvent::server_shutdown(),
        ];
        let expected = [
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
        for (event, kind) in events.iter().zip(expected.iter()) {
            assert_eq!(event.kind(), *kind, "mismatch for {kind:?}");
        }
    }

    #[test]
    fn serde_roundtrip_db_stat_snapshot() {
        let snap = DbStatSnapshot {
            projects: 3,
            agents: 7,
            messages: 1000,
            file_reservations: 12,
            contact_links: 4,
            ack_pending: 2,
            agents_list: vec![
                AgentSummary {
                    project: String::new(),
                    name: "GoldFox".into(),
                    program: "claude-code".into(),
                    model: String::new(),
                    last_active_ts: 123_456,
                    health: None,
                },
                AgentSummary {
                    project: String::new(),
                    name: "SilverWolf".into(),
                    program: "codex-cli".into(),
                    model: String::new(),
                    last_active_ts: 789_012,
                    health: None,
                },
            ],
            timestamp_micros: 500_000,
            ..Default::default()
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        let round: DbStatSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.projects, 3);
        assert_eq!(round.agents_list.len(), 2);
        assert_eq!(round.agents_list[1].name, "SilverWolf");
    }

    #[test]
    fn replay_range_empty_on_invalid_range() {
        let ring = EventRingBuffer::with_capacity(10);
        let _ = ring.push(sample_http("/x", 200));
        assert!(ring.replay_range(5, 2).is_empty());
        assert!(ring.replay_range(100, 200).is_empty());
    }

    #[test]
    fn iter_recent_zero_returns_empty() {
        let ring = EventRingBuffer::with_capacity(10);
        let _ = ring.push(sample_http("/x", 200));
        assert!(ring.iter_recent(0).is_empty());
    }

    #[test]
    fn event_ring_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EventRingBuffer>();
        assert_send_sync::<MailEvent>();
        assert_send_sync::<DbStatSnapshot>();
        assert_send_sync::<AgentSummary>();
    }

    // ── EventSeverity tests ────────────────────────────────────────

    #[test]
    fn severity_badge_values() {
        assert_eq!(EventSeverity::Trace.badge(), "TRC");
        assert_eq!(EventSeverity::Debug.badge(), "DBG");
        assert_eq!(EventSeverity::Info.badge(), "INF");
        assert_eq!(EventSeverity::Warn.badge(), "WRN");
        assert_eq!(EventSeverity::Error.badge(), "ERR");
    }

    #[test]
    fn severity_ordering() {
        assert!(EventSeverity::Trace < EventSeverity::Debug);
        assert!(EventSeverity::Debug < EventSeverity::Info);
        assert!(EventSeverity::Info < EventSeverity::Warn);
        assert!(EventSeverity::Warn < EventSeverity::Error);
    }

    #[test]
    fn severity_derived_from_event_kind() {
        assert_eq!(
            MailEvent::tool_call_start("t", Value::Null, None, None).severity(),
            EventSeverity::Trace
        );
        assert_eq!(
            MailEvent::tool_call_end("t", 1, None, 0, 0.0, vec![], None, None).severity(),
            EventSeverity::Debug
        );
        assert_eq!(
            MailEvent::message_sent(1, "a", vec![], "s", "t", "p", "").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::message_received(1, "a", vec![], "s", "t", "p", "").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::reservation_granted("a", vec![], true, 60, "p").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::agent_registered("n", "p", "m", "proj").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::server_started("http://test", "cfg").severity(),
            EventSeverity::Info
        );
        assert_eq!(MailEvent::server_shutdown().severity(), EventSeverity::Warn);
        assert_eq!(
            MailEvent::health_pulse(DbStatSnapshot::default()).severity(),
            EventSeverity::Debug
        );
    }

    #[test]
    fn severity_mapping_covers_all_event_variants_explicitly() {
        let cases = vec![
            (
                MailEvent::tool_call_start("tool", Value::Null, None, None),
                EventSeverity::Trace,
            ),
            (
                MailEvent::tool_call_end("tool", 1, None, 0, 0.0, vec![], None, None),
                EventSeverity::Debug,
            ),
            (
                MailEvent::message_sent(1, "A", vec!["B".to_string()], "s", "t", "p", ""),
                EventSeverity::Info,
            ),
            (
                MailEvent::message_received(1, "A", vec!["B".to_string()], "s", "t", "p", ""),
                EventSeverity::Info,
            ),
            (
                MailEvent::reservation_granted("A", vec!["src/**".to_string()], true, 60, "p"),
                EventSeverity::Info,
            ),
            (
                MailEvent::reservation_released("A", vec!["src/**".to_string()], "p"),
                EventSeverity::Info,
            ),
            (
                MailEvent::agent_registered("A", "codex-cli", "gpt-5-codex", "p"),
                EventSeverity::Info,
            ),
            (
                MailEvent::http_request("GET", "/", 200, 2, "127.0.0.1"),
                EventSeverity::Debug,
            ),
            (
                MailEvent::health_pulse(DbStatSnapshot::default()),
                EventSeverity::Debug,
            ),
            (
                MailEvent::server_started("http://127.0.0.1:8080", "test"),
                EventSeverity::Info,
            ),
            (MailEvent::server_shutdown(), EventSeverity::Warn),
        ];

        for (event, expected) in cases {
            assert_eq!(
                event.severity(),
                expected,
                "unexpected severity for {:?}",
                event.kind()
            );
        }
    }

    #[test]
    fn compact_labels_are_stable_and_short() {
        let labels = [
            MailEventKind::ToolCallStart.compact_label(),
            MailEventKind::ToolCallEnd.compact_label(),
            MailEventKind::MessageSent.compact_label(),
            MailEventKind::MessageReceived.compact_label(),
            MailEventKind::ReservationGranted.compact_label(),
            MailEventKind::ReservationReleased.compact_label(),
            MailEventKind::AgentRegistered.compact_label(),
            MailEventKind::HttpRequest.compact_label(),
            MailEventKind::HealthPulse.compact_label(),
            MailEventKind::ServerStarted.compact_label(),
            MailEventKind::ServerShutdown.compact_label(),
        ];
        for label in labels {
            assert!(!label.is_empty());
            assert!(label.len() <= 10, "label too long: {label}");
        }
    }

    #[test]
    fn to_event_log_entry_formats_expected_fields() {
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
        let entry = event.to_event_log_entry();
        assert_eq!(entry.kind, MailEventKind::ToolCallEnd);
        assert_eq!(entry.severity, EventSeverity::Debug);
        assert_eq!(entry.icon, '⚙');
        assert!(entry.timestamp.contains(':'));
        assert!(entry.timestamp.contains('.'));
        assert!(entry.summary.contains("send_message"));
        assert!(entry.summary.contains("42ms"));
        assert!(entry.summary.contains("q=5"));
        assert!(entry.summary.contains("[RedFox@my-proj]"));
    }

    #[test]
    fn to_event_log_entry_timestamp_is_zero_padded_hh_mm_ss_mmm() {
        let event = MailEvent::ServerStarted {
            seq: 7,
            // 05:07:09.456 (in microseconds)
            timestamp_micros: (((5 * 3600) + (7 * 60) + 9) * 1_000_000) + 456_000,
            source: EventSource::Lifecycle,
            redacted: false,
            endpoint: "http://127.0.0.1:8765".to_string(),
            config_summary: "cfg".to_string(),
        };

        let entry = event.to_event_log_entry();
        assert_eq!(entry.timestamp, "05:07:09.456");
    }

    #[test]
    fn to_event_log_entry_message_summary_compacts_multiple_recipients() {
        let event = MailEvent::message_sent(
            9,
            "GoldFox",
            vec!["SilverWolf".to_string(), "BlueBear".to_string()],
            "Re: progress update",
            "thread-1",
            "proj",
            "",
        );
        let entry = event.to_event_log_entry();

        assert!(entry.summary.contains("GoldFox"));
        assert!(
            entry.summary.contains("SilverWolf +1"),
            "summary should compact multiple recipients: {}",
            entry.summary
        );
        assert!(entry.summary.contains("Re: progress update"));
    }

    #[test]
    fn severity_http_by_status_code() {
        assert_eq!(
            MailEvent::http_request("GET", "/", 200, 1, "127.0.0.1").severity(),
            EventSeverity::Debug
        );
        assert_eq!(
            MailEvent::http_request("GET", "/", 301, 1, "127.0.0.1").severity(),
            EventSeverity::Debug
        );
        assert_eq!(
            MailEvent::http_request("GET", "/", 404, 1, "127.0.0.1").severity(),
            EventSeverity::Warn
        );
        assert_eq!(
            MailEvent::http_request("GET", "/", 500, 1, "127.0.0.1").severity(),
            EventSeverity::Error
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // Event factory normalization tests (br-10wc.12.2)
    // ──────────────────────────────────────────────────────────────────────

    /// All factory constructors produce seq=0, timestamp=0 (assigned on push).
    #[test]
    fn factory_events_have_zero_seq_and_timestamp() {
        let events = vec![
            MailEvent::tool_call_start("t", Value::Null, None, None),
            MailEvent::tool_call_end("t", 0, None, 0, 0.0, vec![], None, None),
            MailEvent::message_sent(1, "A", vec![], "sub", "tid", "proj", ""),
            MailEvent::message_received(1, "A", vec![], "sub", "tid", "proj", ""),
            MailEvent::reservation_granted("A", vec![], true, 60, "proj"),
            MailEvent::reservation_released("A", vec![], "proj"),
            MailEvent::agent_registered("A", "cc", "opus", "proj"),
            MailEvent::http_request("GET", "/", 200, 5, "127.0.0.1"),
            MailEvent::server_started("http://127.0.0.1:8080", "test"),
        ];
        for event in &events {
            assert_eq!(event.seq(), 0, "seq should be 0 for {:?}", event.kind());
            assert_eq!(
                event.timestamp_micros(),
                0,
                "timestamp should be 0 for {:?}",
                event.kind()
            );
        }
    }

    /// All factory constructors produce `redacted: false`.
    #[test]
    fn factory_events_are_not_redacted() {
        let events = vec![
            MailEvent::tool_call_start("t", Value::Null, None, None),
            MailEvent::tool_call_end("t", 0, None, 0, 0.0, vec![], None, None),
            MailEvent::message_sent(1, "A", vec![], "sub", "tid", "proj", ""),
            MailEvent::agent_registered("A", "cc", "opus", "proj"),
            MailEvent::http_request("GET", "/", 200, 5, "127.0.0.1"),
        ];
        for event in &events {
            assert!(
                !event.redacted(),
                "factory event should not be redacted: {:?}",
                event.kind()
            );
        }
    }

    /// `EventSource` is correctly assigned for each factory.
    #[test]
    fn factory_events_have_correct_source() {
        assert_eq!(
            MailEvent::tool_call_start("t", Value::Null, None, None).source(),
            EventSource::Tooling
        );
        assert_eq!(
            MailEvent::tool_call_end("t", 0, None, 0, 0.0, vec![], None, None).source(),
            EventSource::Tooling
        );
        assert_eq!(
            MailEvent::message_sent(1, "A", vec![], "s", "t", "p", "").source(),
            EventSource::Mail
        );
        assert_eq!(
            MailEvent::message_received(1, "A", vec![], "s", "t", "p", "").source(),
            EventSource::Mail
        );
        assert_eq!(
            MailEvent::reservation_granted("A", vec![], true, 60, "p").source(),
            EventSource::Reservations
        );
        assert_eq!(
            MailEvent::reservation_released("A", vec![], "p").source(),
            EventSource::Reservations
        );
        assert_eq!(
            MailEvent::agent_registered("A", "cc", "opus", "p").source(),
            EventSource::Lifecycle
        );
        assert_eq!(
            MailEvent::http_request("GET", "/", 200, 5, "127.0.0.1").source(),
            EventSource::Http
        );
        assert_eq!(
            MailEvent::server_started("http://127.0.0.1", "test").source(),
            EventSource::Lifecycle
        );
    }

    /// Push assigns monotonically increasing seq numbers.
    #[test]
    fn push_assigns_monotonic_seq() {
        let ring = EventRingBuffer::with_capacity(100);
        let seq1 = ring.push(sample_tool_start("a"));
        let seq2 = ring.push(sample_tool_start("b"));
        let seq3 = ring.push(sample_tool_start("c"));
        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);
        assert_eq!(seq3, 3);
    }

    /// Push assigns non-zero timestamps to events with timestamp=0.
    #[test]
    fn push_fills_timestamp_when_zero() {
        let ring = EventRingBuffer::with_capacity(100);
        let _ = ring.push(sample_tool_start("t"));
        let events = ring.events_since_seq(0);
        assert!(!events.is_empty());
        let ts = events[0].timestamp_micros();
        // Should be a reasonable recent timestamp (after 2020)
        assert!(
            ts > 1_577_836_800_000_000,
            "timestamp {ts} should be after 2020"
        );
    }

    /// `MailEventKind` is correctly mapped for every variant.
    #[test]
    fn kind_maps_correctly() {
        assert_eq!(
            MailEvent::tool_call_start("t", Value::Null, None, None).kind(),
            MailEventKind::ToolCallStart
        );
        assert_eq!(
            MailEvent::tool_call_end("t", 0, None, 0, 0.0, vec![], None, None).kind(),
            MailEventKind::ToolCallEnd
        );
        assert_eq!(
            MailEvent::message_sent(1, "A", vec![], "s", "t", "p", "").kind(),
            MailEventKind::MessageSent
        );
        assert_eq!(
            MailEvent::message_received(1, "A", vec![], "s", "t", "p", "").kind(),
            MailEventKind::MessageReceived
        );
        assert_eq!(
            MailEvent::reservation_granted("A", vec![], true, 60, "p").kind(),
            MailEventKind::ReservationGranted
        );
        assert_eq!(
            MailEvent::reservation_released("A", vec![], "p").kind(),
            MailEventKind::ReservationReleased
        );
        assert_eq!(
            MailEvent::agent_registered("A", "cc", "opus", "p").kind(),
            MailEventKind::AgentRegistered
        );
        assert_eq!(
            MailEvent::http_request("GET", "/", 200, 5, "127.0.0.1").kind(),
            MailEventKind::HttpRequest
        );
        assert_eq!(
            MailEvent::server_started("http://127.0.0.1", "test").kind(),
            MailEventKind::ServerStarted
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // Masking/redaction integration tests (br-10wc.12.2)
    // ──────────────────────────────────────────────────────────────────────

    /// `mask_json` redacts sensitive keys in tool params.
    #[test]
    fn mask_json_redacts_token_in_params() {
        let params = serde_json::json!({
            "project_key": "/data/proj",
            "auth_token": "secret-12345",
            "agent_name": "GoldFox"
        });
        let masked = crate::console::mask_json(&params);
        let obj = masked.as_object().unwrap();
        // project_key is allowlisted
        assert_eq!(obj["project_key"], "/data/proj");
        // auth_token contains "token" => redacted
        assert_eq!(obj["auth_token"], "<redacted>");
        // agent_name is safe
        assert_eq!(obj["agent_name"], "GoldFox");
    }

    /// `mask_json` handles nested objects.
    #[test]
    fn mask_json_handles_nested_secrets() {
        let params = serde_json::json!({
            "config": {
                "api_key": "key-xxx",
                "host": "example.com"
            }
        });
        let masked = crate::console::mask_json(&params);
        let config = masked["config"].as_object().unwrap();
        assert_eq!(config["api_key"], "<redacted>");
        assert_eq!(config["host"], "example.com");
    }

    /// `mask_json` sanitizes database URLs.
    #[test]
    fn mask_json_sanitizes_database_url() {
        let params = serde_json::json!({
            "database_url": "postgres://admin:s3cret@db.example.com/mydb"
        });
        let masked = crate::console::mask_json(&params);
        let url = masked["database_url"].as_str().unwrap();
        assert!(url.contains("admin"), "username should be preserved");
        assert!(url.contains("<redacted>"), "password should be masked");
        assert!(!url.contains("s3cret"), "original password should be gone");
    }

    /// `mask_json` preserves arrays of non-sensitive values.
    #[test]
    fn mask_json_preserves_safe_arrays() {
        let params = serde_json::json!({
            "to": ["GoldFox", "SilverWolf"],
            "paths": ["src/**", "tests/**"]
        });
        let masked = crate::console::mask_json(&params);
        assert_eq!(masked["to"], serde_json::json!(["GoldFox", "SilverWolf"]));
        assert_eq!(masked["paths"], serde_json::json!(["src/**", "tests/**"]));
    }

    /// `mask_json` handles mixed arrays with nested objects.
    #[test]
    fn mask_json_handles_array_with_objects() {
        let params = serde_json::json!([
            {"name": "ok", "secret": "hide-me"},
            {"name": "also_ok"}
        ]);
        let masked = crate::console::mask_json(&params);
        let arr = masked.as_array().unwrap();
        assert_eq!(arr[0]["name"], "ok");
        assert_eq!(arr[0]["secret"], "<redacted>");
        assert_eq!(arr[1]["name"], "also_ok");
    }

    /// `is_sensitive_key` correctly identifies common secret patterns.
    #[test]
    fn sensitive_key_detection() {
        // Positive cases
        assert!(crate::console::is_sensitive_key("auth_token"));
        assert!(crate::console::is_sensitive_key("AUTH_TOKEN"));
        assert!(crate::console::is_sensitive_key("api_key"));
        assert!(crate::console::is_sensitive_key("my_secret"));
        assert!(crate::console::is_sensitive_key("password"));
        assert!(crate::console::is_sensitive_key("bearer"));
        assert!(crate::console::is_sensitive_key("jwt_token"));
        assert!(crate::console::is_sensitive_key("private_key"));
        assert!(crate::console::is_sensitive_key("credential"));
        assert!(crate::console::is_sensitive_key("authorization"));
        assert!(crate::console::is_sensitive_key("auth_header"));

        // Negative cases - safe keys
        assert!(!crate::console::is_sensitive_key("project_key"));
        assert!(!crate::console::is_sensitive_key("storage_root"));
        assert!(!crate::console::is_sensitive_key("agent_name"));
        assert!(!crate::console::is_sensitive_key("tool_name"));
        assert!(!crate::console::is_sensitive_key("subject"));
    }

    /// URL sanitization preserves scheme, user, and host but masks password.
    #[test]
    fn url_sanitization_variants() {
        let cases = vec![
            (
                "database_url",
                "postgres://user:pass@host/db",
                true,
                "user",
                "<redacted>",
            ),
            (
                "redis_url",
                "redis://admin:secret@redis.local:6379/0",
                true,
                "admin",
                "<redacted>",
            ),
            (
                "database_url",
                "sqlite:///path/to/db.sqlite3",
                false,
                "",
                "",
            ), // No userinfo
            ("other_key", "postgres://user:pass@host/db", false, "", ""), // Key not recognized
        ];
        for (key, url, should_sanitize, expected_user, expected_mask) in cases {
            let result = crate::console::sanitize_known_value(key, url);
            if should_sanitize {
                let sanitized = result.unwrap_or_else(|| panic!("should sanitize {key}={url}"));
                assert!(
                    sanitized.contains(expected_user),
                    "user should be preserved in {sanitized}"
                );
                assert!(
                    sanitized.contains(expected_mask),
                    "mask should appear in {sanitized}"
                );
            } else {
                assert!(
                    result.is_none(),
                    "should not sanitize {key}={url}, got: {result:?}"
                );
            }
        }
    }

    /// Events pushed through ring buffer preserve their fields intact.
    #[test]
    fn ring_buffer_preserves_event_fields() {
        let ring = EventRingBuffer::with_capacity(100);

        let msg_event = MailEvent::message_sent(
            42,
            "GoldFox",
            vec!["SilverWolf".to_string()],
            "Test Subject",
            "thread-1",
            "my-project",
            "",
        );
        let _ = ring.push(msg_event);

        let events = ring.events_since_seq(0);
        assert_eq!(events.len(), 1);
        let e = &events[0];

        assert_eq!(e.kind(), MailEventKind::MessageSent);
        assert_eq!(e.source(), EventSource::Mail);
        assert!(!e.redacted());
        assert_eq!(e.seq(), 1);
        assert!(e.timestamp_micros() > 0);

        // Verify inner fields via Debug representation
        let debug = format!("{e:?}");
        assert!(debug.contains("GoldFox"));
        assert!(debug.contains("SilverWolf"));
        assert!(debug.contains("Test Subject"));
        assert!(debug.contains("thread-1"));
    }

    /// Masked params should persist through the event creation pipeline.
    #[test]
    fn masked_params_in_tool_call_start() {
        let raw_params = serde_json::json!({
            "project_key": "/safe/path",
            "auth_token": "my-secret-token-123"
        });
        let masked = crate::console::mask_json(&raw_params);
        let event = MailEvent::tool_call_start("register_agent", masked, None, None);

        // Verify through Debug that secret is masked
        let debug = format!("{event:?}");
        assert!(
            !debug.contains("my-secret-token-123"),
            "raw secret should not appear"
        );
        assert!(
            debug.contains("<redacted>"),
            "redaction marker should appear"
        );
        assert!(
            debug.contains("/safe/path"),
            "safe value should be preserved"
        );
    }

    /// `HealthPulse` events carry `DbStatSnapshot` data.
    #[test]
    fn health_pulse_carries_db_stats() {
        let ring = EventRingBuffer::with_capacity(100);
        let stats = DbStatSnapshot::default();
        let event = MailEvent::HealthPulse {
            seq: 0,
            timestamp_micros: 0,
            source: EventSource::Database,
            redacted: false,
            db_stats: stats,
        };
        let _ = ring.push(event);

        let events = ring.events_since_seq(0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind(), MailEventKind::HealthPulse);
        assert_eq!(events[0].source(), EventSource::Database);
    }

    /// `HttpRequest` severity classification by status code.
    #[test]
    fn http_severity_by_status_code() {
        let ok = MailEvent::http_request("GET", "/", 200, 5, "127.0.0.1");
        let redirect = MailEvent::http_request("GET", "/", 301, 5, "127.0.0.1");
        let not_found = MailEvent::http_request("GET", "/", 404, 5, "127.0.0.1");
        let server_err = MailEvent::http_request("GET", "/", 500, 5, "127.0.0.1");

        assert_eq!(ok.severity(), EventSeverity::Debug);
        assert_eq!(redirect.severity(), EventSeverity::Debug);
        assert_eq!(not_found.severity(), EventSeverity::Warn);
        assert_eq!(server_err.severity(), EventSeverity::Error);
    }

    /// Event severity classifications for non-HTTP variants.
    #[test]
    fn event_severity_classification() {
        assert_eq!(
            MailEvent::tool_call_start("t", Value::Null, None, None).severity(),
            EventSeverity::Trace
        );
        assert_eq!(
            MailEvent::tool_call_end("t", 0, None, 0, 0.0, vec![], None, None).severity(),
            EventSeverity::Debug
        );
        assert_eq!(
            MailEvent::message_sent(1, "A", vec![], "s", "t", "p", "").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::agent_registered("A", "cc", "opus", "p").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::server_started("http://x", "y").severity(),
            EventSeverity::Info
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // DataProvider and TimelineRow tests
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn timeline_row_from_event_formats_correctly() {
        let event = MailEvent::message_sent(
            42,
            "GoldFox",
            vec!["SilverWolf".to_string()],
            "Test Subject",
            "thread-1",
            "my-project",
            "",
        );

        let row = TimelineRow::from_event(&event);

        assert_eq!(row.kind, MailEventKind::MessageSent);
        assert_eq!(row.severity, EventSeverity::Info);
        assert_eq!(row.source, EventSource::Mail);
        assert_eq!(row.icon, '↑');
        assert!(row.summary.contains("GoldFox"));
        assert!(row.summary.contains("SilverWolf"));
        assert!(row.summary.contains("Test Subject"));
    }

    #[test]
    fn timeline_row_formats_tool_call_summary() {
        let start = MailEvent::tool_call_start("fetch_inbox", Value::Null, None, None);
        let end = MailEvent::tool_call_end("send_message", 150, None, 3, 25.0, vec![], None, None);

        let start_row = TimelineRow::from_event(&start);
        let end_row = TimelineRow::from_event(&end);

        assert!(start_row.summary.contains("fetch_inbox"));
        assert!(start_row.summary.starts_with("→"));
        assert_eq!(start_row.icon, '▶');

        assert!(end_row.summary.contains("send_message"));
        assert!(end_row.summary.contains("150ms"));
        assert!(end_row.summary.starts_with("←"));
        assert_eq!(end_row.icon, '■');
    }

    #[test]
    fn timeline_data_provider_total_count_matches_ring() {
        let ring = Arc::new(EventRingBuffer::with_capacity(100));
        let _ = ring.push(sample_tool_start("a"));
        let _ = ring.push(sample_tool_start("b"));
        let _ = ring.push(sample_tool_start("c"));

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();

        assert_eq!(provider.total_count(), 3);
    }

    #[test]
    fn timeline_data_provider_window_returns_slice() {
        let ring = Arc::new(EventRingBuffer::with_capacity(100));
        for i in 0..10 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();

        let window = provider.window(2, 5);
        assert_eq!(window.len(), 5);

        // Verify the window starts at index 2
        let full = provider.window(0, 100);
        assert_eq!(full.len(), 10);
    }

    #[test]
    fn timeline_data_provider_window_clamps_to_bounds() {
        let ring = Arc::new(EventRingBuffer::with_capacity(100));
        for i in 0..5 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();

        // Window beyond end
        let window = provider.window(3, 10);
        assert_eq!(window.len(), 2); // Only 2 items left from index 3

        // Window starting beyond end
        let window = provider.window(100, 10);
        assert_eq!(window.len(), 0);
    }

    #[test]
    fn fenwick_viewport_index_handles_variable_heights() {
        let mut index = FenwickViewportIndex::default();
        index.rebuild_from_heights(&[2, 1, 3, 1]);

        assert_eq!(index.range_for_offset(0, 2), 0..1);
        assert_eq!(index.range_for_offset(2, 2), 1..2);
        assert_eq!(index.range_for_offset(3, 3), 2..3);
        assert_eq!(index.range_for_offset(6, 3), 3..4);
        assert_eq!(index.range_for_offset(7, 3), 4..4);
    }

    #[test]
    fn timeline_data_provider_window_for_viewport_offset_uses_index() {
        let ring = Arc::new(EventRingBuffer::with_capacity(100));
        for i in 0..10 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();

        // With 1-row heights, viewport range should map directly to row indices.
        assert_eq!(provider.viewport_range_for_offset(3, 4), 3..7);
        assert_eq!(provider.viewport_range_for_offset(9, 4), 9..10);
        assert_eq!(provider.viewport_range_for_offset(20, 4), 10..10);

        assert_eq!(provider.window_for_viewport_offset(3, 4).len(), 4);
        assert_eq!(provider.window_for_viewport_offset(9, 4).len(), 1);
        assert_eq!(provider.window_for_viewport_offset(20, 4).len(), 0);
    }

    #[test]
    fn timeline_data_provider_prefetch_refreshes() {
        let ring = Arc::new(EventRingBuffer::with_capacity(100));
        let _ = ring.push(sample_tool_start("initial"));

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();
        assert_eq!(provider.total_count(), 1);

        // Add more events
        let _ = ring.push(sample_tool_start("second"));
        let _ = ring.push(sample_tool_start("third"));

        // Prefetch should pick up new events
        provider.prefetch(0);
        assert_eq!(provider.total_count(), 3);
    }

    #[test]
    fn timeline_data_provider_invalidate_forces_rebuild() {
        let ring = Arc::new(EventRingBuffer::with_capacity(100));
        for i in 0..5 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();
        assert_eq!(provider.total_count(), 5);

        // Invalidate and refresh
        provider.invalidate();
        provider.refresh();
        assert_eq!(provider.total_count(), 5);
    }

    #[test]
    fn message_row_render_item_height() {
        let row = MessageRow {
            id: 1,
            from_agent: "Alice".to_string(),
            to_agents: "Bob".to_string(),
            subject: "Hello".to_string(),
            project_slug: "proj".to_string(),
            thread_id: "t1".to_string(),
            timestamp: "12:00:00".to_string(),
            importance: "normal".to_string(),
            ack_required: false,
            timestamp_micros: 0,
        };

        assert_eq!(row.height(), 1);
    }

    #[test]
    fn search_hit_row_render_item_height() {
        let row = SearchHitRow {
            id: 1,
            subject: "Test".to_string(),
            snippet: "...test...".to_string(),
            from: "Agent".to_string(),
            score: 1.5,
            timestamp_micros: 0,
        };

        assert_eq!(row.height(), 1);
    }

    #[test]
    fn explorer_row_render_item_height() {
        let row = ExplorerRow {
            id: 1,
            direction: '↑',
            primary_agent: "Alice".to_string(),
            secondary_agent: "Bob".to_string(),
            subject: "Hello".to_string(),
            date: "2026-02-12".to_string(),
            timestamp_micros: 0,
        };

        assert_eq!(row.height(), 1);
    }

    // ────────────────────────────────────────────────────────────────
    // Performance benchmarks for virtualized rendering (br-2bbt.3)
    // ────────────────────────────────────────────────────────────────

    /// Benchmark: `TimelineDataProvider` with 10,000 events.
    ///
    /// Acceptance criteria from br-2bbt.3:
    /// - Frame render at p95: <16ms with 10,000 items in data source
    /// - Scroll latency: <1 frame
    ///
    /// This test measures the `DataProvider.window()` operation which is the
    /// critical path for `VirtualizedList` frame rendering. The actual rendering
    /// is handled by `ftui_widgets::VirtualizedList` which has O(1) complexity
    /// for visible rows only.
    #[test]
    #[allow(clippy::similar_names)]
    fn perf_virtualized_timeline_10k_events() {
        use std::time::Instant;

        // Create ring buffer with 10,000 events (mixed types)
        let ring = Arc::new(EventRingBuffer::with_capacity(10_000));
        for i in 0..10_000u64 {
            match i % 5 {
                0 => {
                    let _ = ring.push(MailEvent::tool_call_start(
                        format!("tool_{}", i % 50),
                        serde_json::json!({"iter": i}),
                        Some("proj".to_string()),
                        Some(format!("Agent{}", i % 10)),
                    ));
                }
                1 => {
                    let _ = ring.push(MailEvent::tool_call_end(
                        format!("tool_{}", i % 50),
                        i,
                        None,
                        5,
                        0.95,
                        vec![],
                        Some("proj".to_string()),
                        Some(format!("Agent{}", i % 10)),
                    ));
                }
                2 => {
                    #[allow(clippy::cast_possible_wrap)]
                    let _ = ring.push(MailEvent::message_sent(
                        i as i64,
                        format!("Agent{}", i % 10),
                        vec![format!("Agent{}", (i + 1) % 10)],
                        format!("Subject {i}"),
                        format!("thread-{}", i % 100),
                        "proj",
                        "",
                    ));
                }
                3 => {
                    let _ = ring.push(MailEvent::http_request(
                        "GET",
                        format!("/api/messages/{i}"),
                        200,
                        5,
                        "127.0.0.1",
                    ));
                }
                _ => {
                    let _ = ring.push(MailEvent::health_pulse(DbStatSnapshot::default()));
                }
            }
        }

        // Create DataProvider and measure initial refresh
        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        let refresh_start = Instant::now();
        provider.refresh();
        let refresh_elapsed = refresh_start.elapsed();

        assert_eq!(provider.total_count(), 10_000);
        // Initial refresh converts all events. Keep strict release budget while
        // allowing headroom for debug runs under shared CPU contention.
        let refresh_budget_ms = if cfg!(debug_assertions) { 200 } else { 50 };
        assert!(
            refresh_elapsed.as_millis() < refresh_budget_ms,
            "Initial refresh took too long: {refresh_elapsed:?} (budget: {refresh_budget_ms}ms)",
        );

        // Benchmark: measure 100 window() calls at different positions
        // This simulates scrolling through the timeline
        let mut timings_ns: Vec<u128> = Vec::with_capacity(100);
        for offset in (0..10_000).step_by(100) {
            let start = Instant::now();
            let window = provider.window(offset, 50);
            let elapsed = start.elapsed();
            timings_ns.push(elapsed.as_nanos());

            // Window should return up to 50 items (or fewer at end)
            assert!(window.len() <= 50);
        }

        // Sort for percentile calculation
        timings_ns.sort_unstable();
        let p50_ns = timings_ns[timings_ns.len() / 2];
        let p95_ns = timings_ns[timings_ns.len() * 95 / 100];
        let p99_ns = timings_ns[timings_ns.len() * 99 / 100];

        // Convert to microseconds for readable output
        let p50_us = p50_ns / 1000;
        let p95_us = p95_ns / 1000;
        let p99_us = p99_ns / 1000;

        // Acceptance criteria: window() must be sub-millisecond
        // This ensures the total frame budget of 16ms is achievable
        // (window() is just one component of the render pipeline)
        assert!(
            p95_us < 1000,
            "window() p95 exceeds 1ms: p50={p50_us}µs, p95={p95_us}µs, p99={p99_us}µs",
        );

        // Log percentiles for CI trend analysis (ignored by default, shown with --nocapture)
        eprintln!(
            "[perf] TimelineDataProvider.window() (10K events, 50 visible): \
             p50={p50_us}µs p95={p95_us}µs p99={p99_us}µs",
        );
    }

    /// Benchmark: rapid scrolling through 10K events.
    ///
    /// Simulates user holding down Page-Down key, which triggers
    /// rapid `window()` calls at increasing offsets.
    #[test]
    fn perf_rapid_scroll_timeline_10k() {
        use std::time::Instant;

        // Setup ring buffer with 10,000 events
        let ring = Arc::new(EventRingBuffer::with_capacity(10_000));
        for i in 0..10_000u64 {
            let path = format!("/scroll/{i}");
            let _ = ring.push(sample_http(&path, 200));
        }

        let mut provider = TimelineDataProvider::new(Arc::clone(&ring));
        provider.refresh();

        // Simulate rapid scrolling: 200 consecutive window calls
        let total_start = Instant::now();
        for page in 0..200 {
            let offset = (page * 50) % 10_000;
            let _ = provider.window(offset, 50);
        }
        let total_elapsed = total_start.elapsed();

        // 200 scrolls should complete in under 20ms (100µs per scroll average)
        assert!(
            total_elapsed.as_millis() < 20,
            "Rapid scroll (200 page transitions) took {total_elapsed:?}, budget: 20ms",
        );

        eprintln!(
            "[perf] Rapid scroll (200 page transitions): {}ms",
            total_elapsed.as_millis()
        );
    }
}
