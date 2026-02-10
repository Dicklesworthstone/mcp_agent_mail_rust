#![allow(clippy::module_name_repetitions)]

use ftui::{PackedRgba, Style};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub const DEFAULT_EVENT_RING_CAPACITY: usize = 10_000;

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

// ──────────────────────────────────────────────────────────────────────
// EventSeverity — derived importance level for filtering
// ──────────────────────────────────────────────────────────────────────

/// Severity level derived from event data, used for verbosity filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    /// High-frequency background noise (tool starts, health pulses).
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
    pub name: String,
    pub program: String,
    pub last_active_ts: i64,
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
    pub timestamp_micros: i64,
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
    /// HTTP severity depends on status code; tool starts and health pulses
    /// are trace-level; tool completions are debug; messages, reservations,
    /// and lifecycle events are info; server shutdown is warn.
    #[must_use]
    pub const fn severity(&self) -> EventSeverity {
        match self {
            Self::ToolCallStart { .. } | Self::HealthPulse { .. } => EventSeverity::Trace,
            Self::ToolCallEnd { .. } => EventSeverity::Debug,
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
    /// Returns `Some(seq)` on success, `None` if the lock is contended
    /// or the event was dropped by the sampling policy.  This is the
    /// preferred path for the server thread where blocking on the TUI
    /// reader is unacceptable.
    #[must_use]
    pub fn try_push(&self, event: MailEvent) -> Option<u64> {
        let Ok(mut inner) = self.inner.try_lock() else {
            self.contention_drops.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        // Apply sampling policy when buffer is filling up.
        if self.should_sample(&inner, &event) {
            self.sampled_drops.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        Some(Self::push_inner(&mut inner, event))
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
        counter % self.policy.sample_rate != 0
    }

    fn push_inner(inner: &mut EventRingBufferInner, mut event: MailEvent) -> u64 {
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.saturating_add(1);
        event.set_seq(seq);
        event.set_timestamp_if_unset(chrono::Utc::now().timestamp_micros());
        if inner.events.len() >= inner.capacity {
            let _ = inner.events.pop_front();
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
        inner
            .events
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    #[must_use]
    pub fn try_iter_recent(&self, limit: usize) -> Option<Vec<MailEvent>> {
        if limit == 0 {
            return Some(Vec::new());
        }
        let inner = self.inner.try_lock().ok()?;
        Some(
            inner
                .events
                .iter()
                .rev()
                .take(limit)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
        )
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
        inner
            .events
            .iter()
            .filter(|event| event.seq() > seq)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn try_events_since_seq(&self, seq: u64) -> Option<Vec<MailEvent>> {
        let inner = self.inner.try_lock().ok()?;
        Some(
            inner
                .events
                .iter()
                .filter(|event| event.seq() > seq)
                .cloned()
                .collect(),
        )
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
                        name: "TealMeadow".to_string(),
                        program: "codex-cli".to_string(),
                        last_active_ts: 99,
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
        let result = ring.try_push(sample_http("/ok", 200));
        assert_eq!(result, Some(1));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn try_push_returns_none_when_locked() {
        let ring = EventRingBuffer::with_capacity(8);
        let _guard = ring.inner.lock().expect("lock");
        let ring2 = ring.clone();
        assert!(ring2.try_push(sample_http("/blocked", 500)).is_none());
    }

    #[test]
    fn try_iter_recent_returns_none_when_locked() {
        let ring = EventRingBuffer::with_capacity(8);
        let _ = ring.push(sample_http("/ok", 200));
        let _guard = ring.inner.lock().expect("lock");
        let ring2 = ring.clone();
        assert!(ring2.try_iter_recent(1).is_none());
    }

    #[test]
    fn try_events_since_seq_returns_none_when_locked() {
        let ring = EventRingBuffer::with_capacity(8);
        let _ = ring.push(sample_http("/ok", 200));
        let _guard = ring.inner.lock().expect("lock");
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
            MailEvent::message_sent(1, "a", vec![], "s", "t", "p"),
            MailEvent::message_received(1, "a", vec![], "s", "t", "p"),
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
                    name: "GoldFox".into(),
                    program: "claude-code".into(),
                    last_active_ts: 123_456,
                },
                AgentSummary {
                    name: "SilverWolf".into(),
                    program: "codex-cli".into(),
                    last_active_ts: 789_012,
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
            MailEvent::message_sent(1, "a", vec![], "s", "t", "p").severity(),
            EventSeverity::Info
        );
        assert_eq!(
            MailEvent::message_received(1, "a", vec![], "s", "t", "p").severity(),
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
            EventSeverity::Trace
        );
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

    // ── VerbosityTier tests ────────────────────────────────────────

    #[test]
    fn verbosity_default_is_standard() {
        assert_eq!(VerbosityTier::default(), VerbosityTier::Standard);
    }

    #[test]
    fn verbosity_includes_logic() {
        // Minimal: only Warn + Error
        assert!(!VerbosityTier::Minimal.includes(EventSeverity::Trace));
        assert!(!VerbosityTier::Minimal.includes(EventSeverity::Debug));
        assert!(!VerbosityTier::Minimal.includes(EventSeverity::Info));
        assert!(VerbosityTier::Minimal.includes(EventSeverity::Warn));
        assert!(VerbosityTier::Minimal.includes(EventSeverity::Error));

        // Standard: Info + Warn + Error
        assert!(!VerbosityTier::Standard.includes(EventSeverity::Trace));
        assert!(!VerbosityTier::Standard.includes(EventSeverity::Debug));
        assert!(VerbosityTier::Standard.includes(EventSeverity::Info));
        assert!(VerbosityTier::Standard.includes(EventSeverity::Warn));
        assert!(VerbosityTier::Standard.includes(EventSeverity::Error));

        // Verbose: Debug + Info + Warn + Error
        assert!(!VerbosityTier::Verbose.includes(EventSeverity::Trace));
        assert!(VerbosityTier::Verbose.includes(EventSeverity::Debug));
        assert!(VerbosityTier::Verbose.includes(EventSeverity::Info));
        assert!(VerbosityTier::Verbose.includes(EventSeverity::Warn));
        assert!(VerbosityTier::Verbose.includes(EventSeverity::Error));

        // All: everything
        assert!(VerbosityTier::All.includes(EventSeverity::Trace));
        assert!(VerbosityTier::All.includes(EventSeverity::Debug));
        assert!(VerbosityTier::All.includes(EventSeverity::Info));
        assert!(VerbosityTier::All.includes(EventSeverity::Warn));
        assert!(VerbosityTier::All.includes(EventSeverity::Error));
    }

    #[test]
    fn verbosity_next_cycles() {
        assert_eq!(VerbosityTier::Minimal.next(), VerbosityTier::Standard);
        assert_eq!(VerbosityTier::Standard.next(), VerbosityTier::Verbose);
        assert_eq!(VerbosityTier::Verbose.next(), VerbosityTier::All);
        assert_eq!(VerbosityTier::All.next(), VerbosityTier::Minimal);
    }

    #[test]
    fn verbosity_label_values() {
        assert_eq!(VerbosityTier::Minimal.label(), "Minimal");
        assert_eq!(VerbosityTier::Standard.label(), "Standard");
        assert_eq!(VerbosityTier::Verbose.label(), "Verbose");
        assert_eq!(VerbosityTier::All.label(), "All");
    }

    #[test]
    fn verbosity_serde_roundtrip() {
        for tier in [
            VerbosityTier::Minimal,
            VerbosityTier::Standard,
            VerbosityTier::Verbose,
            VerbosityTier::All,
        ] {
            let json = serde_json::to_string(&tier).expect("serialize");
            let round: VerbosityTier = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round, tier);
        }
    }

    #[test]
    fn severity_serde_roundtrip() {
        for sev in [
            EventSeverity::Trace,
            EventSeverity::Debug,
            EventSeverity::Info,
            EventSeverity::Warn,
            EventSeverity::Error,
        ] {
            let json = serde_json::to_string(&sev).expect("serialize");
            let round: EventSeverity = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round, sev);
        }
    }

    // ── Severity design system tests ─────────────────────────────────

    #[test]
    fn severity_colors_are_distinct() {
        let colors: Vec<PackedRgba> = [
            EventSeverity::Trace,
            EventSeverity::Debug,
            EventSeverity::Info,
            EventSeverity::Warn,
            EventSeverity::Error,
        ]
        .iter()
        .map(|s| s.color())
        .collect();

        // All 5 must be different from each other.
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "colors at {i} and {j} must differ");
            }
        }
    }

    #[test]
    fn severity_colors_match_constants() {
        assert_eq!(EventSeverity::Trace.color(), SEV_TRACE_FG);
        assert_eq!(EventSeverity::Debug.color(), SEV_DEBUG_FG);
        assert_eq!(EventSeverity::Info.color(), SEV_INFO_FG);
        assert_eq!(EventSeverity::Warn.color(), SEV_WARN_FG);
        assert_eq!(EventSeverity::Error.color(), SEV_ERROR_FG);
    }

    #[test]
    fn severity_style_has_foreground() {
        for sev in [
            EventSeverity::Trace,
            EventSeverity::Debug,
            EventSeverity::Info,
            EventSeverity::Warn,
            EventSeverity::Error,
        ] {
            let style = sev.style();
            assert!(style.fg.is_some(), "{sev:?} style must have fg");
        }
    }

    #[test]
    fn severity_warn_and_error_are_bold() {
        use ftui::style::StyleFlags;
        let warn_style = EventSeverity::Warn.style();
        let err_style = EventSeverity::Error.style();
        assert!(
            warn_style
                .attrs
                .unwrap_or(StyleFlags(0))
                .contains(StyleFlags::BOLD),
            "Warn must be bold"
        );
        assert!(
            err_style
                .attrs
                .unwrap_or(StyleFlags(0))
                .contains(StyleFlags::BOLD),
            "Error must be bold"
        );
    }

    #[test]
    fn severity_trace_is_dim() {
        use ftui::style::StyleFlags;
        let trace_style = EventSeverity::Trace.style();
        assert!(
            trace_style
                .attrs
                .unwrap_or(StyleFlags(0))
                .contains(StyleFlags::DIM),
            "Trace must be dim"
        );
    }

    #[test]
    fn severity_styled_badge_contains_badge_text() {
        for sev in [
            EventSeverity::Trace,
            EventSeverity::Debug,
            EventSeverity::Info,
            EventSeverity::Warn,
            EventSeverity::Error,
        ] {
            let span = sev.styled_badge();
            assert_eq!(span.as_str(), sev.badge());
            assert!(span.style.is_some(), "{sev:?} styled_badge must have style");
        }
    }

    #[test]
    fn severity_symbols_are_distinct() {
        let symbols: Vec<char> = [
            EventSeverity::Trace,
            EventSeverity::Debug,
            EventSeverity::Info,
            EventSeverity::Warn,
            EventSeverity::Error,
        ]
        .iter()
        .map(|s| s.symbol())
        .collect();

        for i in 0..symbols.len() {
            for j in (i + 1)..symbols.len() {
                assert_ne!(symbols[i], symbols[j], "symbols at {i} and {j} must differ");
            }
        }
    }

    #[test]
    fn severity_styled_symbol_has_style() {
        for sev in [
            EventSeverity::Trace,
            EventSeverity::Debug,
            EventSeverity::Info,
            EventSeverity::Warn,
            EventSeverity::Error,
        ] {
            let span = sev.styled_symbol();
            assert!(!span.as_str().is_empty());
            assert!(
                span.style.is_some(),
                "{sev:?} styled_symbol must have style"
            );
        }
    }

    // ── Ring buffer edge-case tests ──────────────────────────────

    #[test]
    fn ring_buffer_capacity_one() {
        let ring = EventRingBuffer::with_capacity(1);
        assert_eq!(ring.push(sample_http("/a", 200)), 1);
        assert_eq!(ring.push(sample_http("/b", 200)), 2);
        assert_eq!(ring.len(), 1);
        let events = ring.iter_recent(10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq(), 2);

        let stats = ring.stats();
        assert_eq!(stats.capacity, 1);
        assert_eq!(stats.len, 1);
        assert_eq!(stats.total_pushed, 2);
        assert_eq!(stats.dropped_overflow, 1);
    }

    #[test]
    fn ring_buffer_capacity_zero_is_bounded_to_one() {
        let ring = EventRingBuffer::with_capacity(0);
        let stats = ring.stats();
        assert_eq!(stats.capacity, 1);
    }

    #[test]
    fn ring_buffer_seq_starts_at_one_and_is_monotonic() {
        let ring = EventRingBuffer::with_capacity(100);
        for i in 0_u64..50 {
            let seq = ring.push(sample_http(&format!("/{i}"), 200));
            assert_eq!(seq, i + 1);
        }
    }

    #[test]
    fn ring_buffer_set_timestamp_if_unset_preserves_existing() {
        let ring = EventRingBuffer::with_capacity(4);
        let event = MailEvent::ToolCallStart {
            seq: 0,
            timestamp_micros: 42_000_000,
            source: EventSource::Tooling,
            redacted: false,
            tool_name: "test".into(),
            params_json: Value::Null,
            project: None,
            agent: None,
        };
        // Push overwrites seq but should preserve non-zero timestamp
        let _ = ring.push(event);
        let events = ring.iter_recent(1);
        assert_eq!(events[0].timestamp_micros(), 42_000_000);
    }

    #[test]
    fn replay_range_single_element() {
        let ring = EventRingBuffer::with_capacity(10);
        let _ = ring.push(sample_http("/x", 200));
        let replay = ring.replay_range(1, 1);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq(), 1);
    }

    #[test]
    fn replay_range_after_overflow_misses_evicted() {
        let ring = EventRingBuffer::with_capacity(3);
        for i in 0..6 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }
        // Seqs 1,2,3 are evicted. Only 4,5,6 remain.
        let replay = ring.replay_range(1, 6);
        let seqs: Vec<u64> = replay.iter().map(MailEvent::seq).collect();
        assert_eq!(seqs, vec![4, 5, 6]);
    }

    #[test]
    fn filter_by_kind_empty_ring() {
        let ring = EventRingBuffer::with_capacity(10);
        let results = ring.filter_by_kind(MailEventKind::HttpRequest);
        assert!(results.is_empty());
    }

    #[test]
    fn since_timestamp_empty_ring() {
        let ring = EventRingBuffer::with_capacity(10);
        let results = ring.since_timestamp(0);
        assert!(results.is_empty());
    }

    #[test]
    fn iter_recent_more_than_available() {
        let ring = EventRingBuffer::with_capacity(10);
        let _ = ring.push(sample_http("/a", 200));
        let _ = ring.push(sample_http("/b", 200));
        let events = ring.iter_recent(100);
        assert_eq!(events.len(), 2);
        // Verify order is preserved (oldest first)
        assert!(events[0].seq() < events[1].seq());
    }

    #[test]
    fn concurrent_push_at_capacity_boundary() {
        let ring = EventRingBuffer::with_capacity(4);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let ring = ring.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let _ = ring.push(sample_http("/concurrent", 200));
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        // Buffer should be at capacity
        assert!(ring.len() <= 4);
        let stats = ring.stats();
        assert_eq!(stats.total_pushed, 400);
        assert!(stats.dropped_overflow >= 396); // 400 - 4
    }

    // ── Severity edge cases ─────────────────────────────────────

    #[test]
    fn http_status_boundary_values() {
        // 399 is Debug (< 400)
        assert_eq!(
            MailEvent::http_request("GET", "/", 399, 1, "127.0.0.1").severity(),
            EventSeverity::Debug
        );
        // 400 is Warn
        assert_eq!(
            MailEvent::http_request("GET", "/", 400, 1, "127.0.0.1").severity(),
            EventSeverity::Warn
        );
        // 499 is Warn
        assert_eq!(
            MailEvent::http_request("GET", "/", 499, 1, "127.0.0.1").severity(),
            EventSeverity::Warn
        );
        // 500 is Error
        assert_eq!(
            MailEvent::http_request("GET", "/", 500, 1, "127.0.0.1").severity(),
            EventSeverity::Error
        );
        // 100 is Debug
        assert_eq!(
            MailEvent::http_request("GET", "/", 100, 1, "127.0.0.1").severity(),
            EventSeverity::Debug
        );
    }

    #[test]
    fn event_source_serde_roundtrip() {
        for source in [
            EventSource::Tooling,
            EventSource::Http,
            EventSource::Mail,
            EventSource::Reservations,
            EventSource::Lifecycle,
            EventSource::Database,
            EventSource::Unknown,
        ] {
            let json = serde_json::to_string(&source).expect("serialize");
            let round: EventSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round, source);
        }
    }

    #[test]
    fn mail_event_kind_serde_roundtrip() {
        for kind in [
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
        ] {
            let json = serde_json::to_string(&kind).expect("serialize");
            let round: MailEventKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round, kind);
        }
    }

    #[test]
    fn reservation_released_event_severity() {
        assert_eq!(
            MailEvent::reservation_released("a", vec![], "p").severity(),
            EventSeverity::Info
        );
    }

    // ── Backpressure / drop accounting tests ──────────────────────

    #[test]
    fn contention_drops_tracked() {
        let ring = EventRingBuffer::with_capacity(8);
        // Hold the lock to force contention
        let guard = ring.inner.lock().expect("lock");
        let ring2 = ring.clone();
        assert!(ring2.try_push(sample_http("/blocked", 500)).is_none());
        assert!(ring2.try_push(sample_http("/blocked2", 500)).is_none());
        // Can't read stats with lock held, drop first
        drop(guard);
        let stats = ring.stats();
        assert_eq!(stats.contention_drops, 2);
    }

    #[test]
    fn sampling_policy_activates_at_threshold() {
        // Capacity 10, threshold 80% = activates at 8 events
        let policy = BackpressurePolicy {
            threshold_pct: 80,
            sample_rate: 2, // Keep 1 in 2
        };
        let ring = EventRingBuffer::with_capacity_and_policy(10, policy);

        // Fill to 7 (under threshold) — all events should be accepted
        for i in 0..7 {
            let seq = ring.try_push(sample_http(&format!("/{i}"), 200));
            assert!(
                seq.is_some(),
                "event {i} should be accepted under threshold"
            );
        }
        assert_eq!(ring.stats().sampled_drops, 0);

        // Fill to 8+ (at threshold) — Trace/Debug events get sampled
        // HTTP 200 events are Debug severity, so they'll be sampled
        let mut accepted = 0;
        let mut rejected = 0;
        for _ in 0..10 {
            match ring.try_push(sample_http("/sampled", 200)) {
                Some(_) => accepted += 1,
                None => rejected += 1,
            }
        }
        // With sample_rate=2, roughly half should be accepted
        assert!(accepted > 0, "some events should be accepted");
        assert!(rejected > 0, "some events should be sampled/dropped");
        assert!(ring.stats().sampled_drops > 0);
    }

    #[test]
    fn sampling_preserves_high_severity_events() {
        let policy = BackpressurePolicy {
            threshold_pct: 50,
            sample_rate: 100, // Very aggressive sampling
        };
        let ring = EventRingBuffer::with_capacity_and_policy(10, policy);

        // Fill to threshold
        for i in 0..6 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        // Info-level events should always be accepted
        // message_sent is Info severity
        for _ in 0..10 {
            let event = MailEvent::message_sent(1, "A", vec![], "s", "t", "p");
            let result = ring.try_push(event);
            assert!(result.is_some(), "Info events should never be sampled");
        }
        assert_eq!(ring.stats().sampled_drops, 0);
    }

    #[test]
    fn sampling_preserves_error_events() {
        let policy = BackpressurePolicy {
            threshold_pct: 50,
            sample_rate: 100,
        };
        let ring = EventRingBuffer::with_capacity_and_policy(10, policy);

        // Fill to threshold
        for i in 0..6 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        // Error events (HTTP 500) should always be accepted
        for _ in 0..10 {
            let event = MailEvent::http_request("GET", "/err", 500, 1, "127.0.0.1");
            let result = ring.try_push(event);
            assert!(result.is_some(), "Error events should never be sampled");
        }
    }

    #[test]
    fn stats_total_drops_aggregates() {
        let stats = EventRingStats {
            capacity: 100,
            len: 50,
            total_pushed: 60,
            dropped_overflow: 10,
            contention_drops: 5,
            sampled_drops: 3,
            next_seq: 61,
        };
        assert_eq!(stats.total_drops(), 18);
        assert!(stats.has_drops());
    }

    #[test]
    fn stats_no_drops() {
        let stats = EventRingStats {
            capacity: 100,
            len: 50,
            total_pushed: 50,
            dropped_overflow: 0,
            contention_drops: 0,
            sampled_drops: 0,
            next_seq: 51,
        };
        assert_eq!(stats.total_drops(), 0);
        assert!(!stats.has_drops());
    }

    #[test]
    fn stats_fill_pct() {
        let stats = EventRingStats {
            capacity: 100,
            len: 80,
            total_pushed: 80,
            dropped_overflow: 0,
            contention_drops: 0,
            sampled_drops: 0,
            next_seq: 81,
        };
        assert_eq!(stats.fill_pct(), 80);
    }

    #[test]
    fn stats_fill_pct_empty() {
        let stats = EventRingStats {
            capacity: 100,
            len: 0,
            total_pushed: 0,
            dropped_overflow: 0,
            contention_drops: 0,
            sampled_drops: 0,
            next_seq: 1,
        };
        assert_eq!(stats.fill_pct(), 0);
    }

    #[test]
    fn stats_fill_pct_full() {
        let stats = EventRingStats {
            capacity: 100,
            len: 100,
            total_pushed: 200,
            dropped_overflow: 100,
            contention_drops: 0,
            sampled_drops: 0,
            next_seq: 201,
        };
        assert_eq!(stats.fill_pct(), 100);
    }

    #[test]
    fn stats_fill_pct_zero_capacity() {
        let stats = EventRingStats {
            capacity: 0,
            len: 0,
            total_pushed: 0,
            dropped_overflow: 0,
            contention_drops: 0,
            sampled_drops: 0,
            next_seq: 1,
        };
        assert_eq!(stats.fill_pct(), 100);
    }

    #[test]
    fn backpressure_policy_default() {
        let policy = BackpressurePolicy::default();
        assert_eq!(policy.threshold_pct, 80);
        assert_eq!(policy.sample_rate, 4);
    }

    #[test]
    fn ring_exposes_policy() {
        let policy = BackpressurePolicy {
            threshold_pct: 50,
            sample_rate: 8,
        };
        let ring = EventRingBuffer::with_capacity_and_policy(10, policy);
        assert_eq!(ring.policy(), policy);
    }

    #[test]
    fn invalid_backpressure_policy_is_normalized() {
        let ring = EventRingBuffer::with_capacity_and_policy(
            10,
            BackpressurePolicy {
                threshold_pct: u8::MAX,
                sample_rate: 0,
            },
        );
        let policy = ring.policy();
        assert_eq!(policy.threshold_pct, 100);
        assert_eq!(policy.sample_rate, 1);
        assert_eq!(ring.try_push(sample_tool_start("normalize")), Some(1));
    }

    #[test]
    fn no_sampling_below_threshold() {
        let policy = BackpressurePolicy {
            threshold_pct: 90,
            sample_rate: 100,
        };
        let ring = EventRingBuffer::with_capacity_and_policy(100, policy);

        // Fill to 50% (well below threshold)
        for i in 0..50 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        // All try_push should succeed (below threshold)
        for _ in 0..20 {
            let result = ring.try_push(sample_http("/ok", 200));
            assert!(result.is_some());
        }
        assert_eq!(ring.stats().sampled_drops, 0);
    }

    // ── Performance regression harness (br-10wc.13.4) ─────────────

    /// Budget: 10k events pushed in under 50ms on typical hardware.
    #[test]
    fn perf_push_throughput_10k() {
        let ring = EventRingBuffer::with_capacity(10_000);
        let start = std::time::Instant::now();
        for i in 0..10_000u64 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }
        let elapsed = start.elapsed();
        let stats = ring.stats();
        assert_eq!(stats.total_pushed, 10_000);
        assert_eq!(stats.len, 10_000);
        // Budget: under 50ms (generous — usually < 5ms).
        assert!(
            elapsed.as_millis() < 50,
            "push throughput regression: 10k events took {elapsed:?}"
        );
    }

    /// Budget: 50k events with overflow in under 200ms.
    #[test]
    fn perf_push_throughput_50k_with_overflow() {
        let ring = EventRingBuffer::with_capacity(5_000);
        let start = std::time::Instant::now();
        for i in 0..50_000u64 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }
        let elapsed = start.elapsed();
        let stats = ring.stats();
        assert_eq!(stats.total_pushed, 50_000);
        assert_eq!(stats.len, 5_000); // bounded
        assert_eq!(stats.dropped_overflow, 45_000);
        assert!(
            elapsed.as_millis() < 200,
            "push throughput regression: 50k events took {elapsed:?}"
        );
    }

    /// Budget: `iter_recent(1000)` from a full 10k ring under 10ms.
    #[test]
    fn perf_iter_recent_from_full_ring() {
        let ring = EventRingBuffer::with_capacity(10_000);
        for i in 0..10_000u64 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let start = std::time::Instant::now();
        let events = ring.iter_recent(1_000);
        let elapsed = start.elapsed();

        assert_eq!(events.len(), 1_000);
        assert!(
            elapsed.as_millis() < 10,
            "iter_recent regression: 1000 from 10k took {elapsed:?}"
        );
    }

    /// Budget: `events_since_seq` scan of full ring under 10ms.
    #[test]
    fn perf_events_since_seq_scan() {
        let ring = EventRingBuffer::with_capacity(10_000);
        for i in 0..10_000u64 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let start = std::time::Instant::now();
        let events = ring.events_since_seq(9_500);
        let elapsed = start.elapsed();

        assert_eq!(events.len(), 500);
        assert!(
            elapsed.as_millis() < 10,
            "events_since_seq regression: scan took {elapsed:?}"
        );
    }

    /// Memory bound: ring never exceeds capacity even under sustained load.
    #[test]
    fn perf_memory_bound_sustained_load() {
        let cap = 1_000;
        let ring = EventRingBuffer::with_capacity(cap);

        // Push 100x capacity
        for i in 0..100_000u64 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let stats = ring.stats();
        assert_eq!(stats.len, cap);
        assert_eq!(stats.total_pushed, 100_000);
        assert_eq!(stats.dropped_overflow, 99_000);
    }

    /// Backpressure activates within a tight window at threshold.
    #[test]
    fn perf_backpressure_activation_timing() {
        let policy = BackpressurePolicy {
            threshold_pct: 80,
            sample_rate: 4,
        };
        let ring = EventRingBuffer::with_capacity_and_policy(100, policy);

        // Fill to 79% — no sampling
        for i in 0..79 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }
        assert_eq!(ring.stats().sampled_drops, 0);

        // Push 1 more to reach 80% (threshold) — Trace events may now be sampled
        let _ = ring.push(sample_http("/threshold", 200));

        // Push 20 Trace-level events via try_push (HttpRequest with 200 is Debug).
        // We need events that are Trace-level. HealthPulse is Trace.
        let mut sampled = 0u64;
        for _ in 0..20 {
            let event = MailEvent::health_pulse(DbStatSnapshot::default());
            if ring.try_push(event).is_none() {
                sampled += 1;
            }
        }
        // With sample_rate=4, roughly 3/4 should be dropped
        assert!(sampled > 0, "backpressure should have activated at 80%");
        assert_eq!(ring.stats().sampled_drops, sampled);
    }

    /// Concurrent push throughput: multiple threads pushing simultaneously.
    #[test]
    fn perf_concurrent_push_throughput() {
        let ring = Arc::new(EventRingBuffer::with_capacity(10_000));
        let thread_count = 4;
        let events_per_thread = 5_000;

        let start = std::time::Instant::now();
        let handles: Vec<_> = (0..thread_count)
            .map(|t| {
                let ring = Arc::clone(&ring);
                std::thread::spawn(move || {
                    for i in 0..events_per_thread {
                        let _ = ring.push(sample_http(&format!("/t{t}/{i}"), 200));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        let elapsed = start.elapsed();

        let stats = ring.stats();
        assert_eq!(stats.total_pushed, thread_count * events_per_thread);
        assert!(
            elapsed.as_millis() < 500,
            "concurrent push regression: {thread_count}x{events_per_thread} took {elapsed:?}"
        );
    }

    /// `try_push` contention tracking under concurrent access.
    #[test]
    fn perf_try_push_contention_tracking() {
        let ring = Arc::new(EventRingBuffer::with_capacity(10_000));
        let thread_count: usize = 4;
        let events_per_thread: usize = 2_000;

        // Spawn all threads first to preserve concurrent contention.
        let mut handles = Vec::with_capacity(thread_count);
        for t in 0..thread_count {
            let ring = Arc::clone(&ring);
            handles.push(std::thread::spawn(move || {
                let mut pushed = 0u64;
                for i in 0..events_per_thread {
                    if ring
                        .try_push(sample_http(&format!("/t{t}/{i}"), 200))
                        .is_some()
                    {
                        pushed += 1;
                    }
                }
                pushed
            }));
        }

        let total_pushed: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        let stats = ring.stats();

        // Some may have been dropped due to contention
        let total_attempted =
            u64::try_from(thread_count).unwrap() * u64::try_from(events_per_thread).unwrap();
        let contention_drops = stats.contention_drops;
        assert_eq!(total_pushed + contention_drops, total_attempted);
    }

    /// Filter-by-kind performance on a full ring.
    #[test]
    fn perf_filter_by_kind_full_ring() {
        let ring = EventRingBuffer::with_capacity(10_000);
        // Mix of events
        for i in 0..10_000u64 {
            if i % 3 == 0 {
                let _ = ring.push(MailEvent::tool_call_start(
                    "test",
                    serde_json::Value::Null,
                    Some("p".to_string()),
                    Some("a".to_string()),
                ));
            } else {
                let _ = ring.push(sample_http(&format!("/{i}"), 200));
            }
        }

        let start = std::time::Instant::now();
        let tool_events = ring.filter_by_kind(MailEventKind::ToolCallStart);
        let elapsed = start.elapsed();

        assert!(tool_events.len() > 3000); // ~1/3 of 10k
        assert!(
            elapsed.as_millis() < 10,
            "filter_by_kind regression: took {elapsed:?}"
        );
    }

    /// Stats computation is O(1) on a full ring.
    #[test]
    fn perf_stats_is_constant_time() {
        let ring = EventRingBuffer::with_capacity(10_000);
        for i in 0..10_000u64 {
            let _ = ring.push(sample_http(&format!("/{i}"), 200));
        }

        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            let _ = ring.stats();
        }
        let elapsed = start.elapsed();

        // 10k stats calls on a full ring should be under 10ms
        assert!(
            elapsed.as_millis() < 10,
            "stats() is not constant time: 10k calls took {elapsed:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Event factory normalization tests (br-10wc.12.2)
    // ────────────────────────────────────────────────────────────────

    /// All factory constructors produce seq=0, timestamp=0 (assigned on push).
    #[test]
    fn factory_events_have_zero_seq_and_timestamp() {
        let events = vec![
            MailEvent::tool_call_start("t", Value::Null, None, None),
            MailEvent::tool_call_end("t", 0, None, 0, 0.0, vec![], None, None),
            MailEvent::message_sent(1, "A", vec![], "sub", "tid", "proj"),
            MailEvent::message_received(1, "A", vec![], "sub", "tid", "proj"),
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
            MailEvent::message_sent(1, "A", vec![], "sub", "tid", "proj"),
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
            MailEvent::message_sent(1, "A", vec![], "s", "t", "p").source(),
            EventSource::Mail
        );
        assert_eq!(
            MailEvent::message_received(1, "A", vec![], "s", "t", "p").source(),
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
            MailEvent::message_sent(1, "A", vec![], "s", "t", "p").kind(),
            MailEventKind::MessageSent
        );
        assert_eq!(
            MailEvent::message_received(1, "A", vec![], "s", "t", "p").kind(),
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

    // ────────────────────────────────────────────────────────────────
    // Masking/redaction integration tests (br-10wc.12.2)
    // ────────────────────────────────────────────────────────────────

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
            MailEvent::message_sent(1, "A", vec![], "s", "t", "p").severity(),
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
}
