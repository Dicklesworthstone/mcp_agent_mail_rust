#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentSummary {
    pub name: String,
    pub program: String,
    pub last_active_ts: i64,
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
    pub next_seq: u64,
}

#[derive(Debug, Clone)]
pub struct EventRingBuffer {
    inner: Arc<Mutex<EventRingBufferInner>>,
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
        let bounded_capacity = capacity.max(1);
        let inner = EventRingBufferInner {
            events: VecDeque::with_capacity(bounded_capacity),
            capacity: bounded_capacity,
            next_seq: 1,
            total_pushed: 0,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    #[must_use]
    pub fn push(&self, mut event: MailEvent) -> u64 {
        let mut inner = self.lock_inner();
        Self::push_inner(&mut inner, &mut event)
    }

    /// Non-blocking push.  Returns `Some(seq)` on success, `None` if the
    /// lock is contended.  This is the preferred path for the server
    /// thread where blocking on the TUI reader is unacceptable.
    #[must_use]
    pub fn try_push(&self, mut event: MailEvent) -> Option<u64> {
        let mut inner = self.inner.try_lock().ok()?;
        Some(Self::push_inner(&mut inner, &mut event))
    }

    fn push_inner(inner: &mut EventRingBufferInner, event: &mut MailEvent) -> u64 {
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.saturating_add(1);
        event.set_seq(seq);
        event.set_timestamp_if_unset(chrono::Utc::now().timestamp_micros());
        if inner.events.len() >= inner.capacity {
            let _ = inner.events.pop_front();
        }
        inner.events.push_back(event.clone());
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
            next_seq: inner.next_seq,
        }
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
        let _ = ring.push(sample_http("/a", 200));
        let _ = ring.push(sample_http("/b", 200));
        let cutoff = ring.iter_recent(2)[0].timestamp_micros();
        let _ = ring.push(sample_http("/c", 200));

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
}
