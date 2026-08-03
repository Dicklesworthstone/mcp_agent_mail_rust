//! Browser-safe in-memory state adapter for the real dashboard screen.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::tui_events::{
    BackpressurePolicy, DbStatSnapshot, EventRingBuffer, EventRingStats, MailEvent,
};
use crate::tui_screens::DataGeneration;

const CONSOLE_LOG_CAPACITY: usize = 2_000;
const SCREEN_DIAGNOSTIC_CAPACITY: usize = 256;
const REPLAY_EVENT_CAPACITY: usize = 10_000;

fn replay_event_ring(capacity: usize) -> EventRingBuffer {
    EventRingBuffer::with_capacity_and_policy(
        capacity,
        BackpressurePolicy {
            threshold_pct: 100,
            sample_rate: 1,
        },
    )
}

/// Proof cursor for an exact replay-ring projection.
///
/// A cache may append events only when the epoch, capacity, and overflow count
/// still match and the sequence tail advances exactly by the retained-length
/// delta. Any reset, eviction, or inconsistent tail forces a full rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplayEventCursor {
    pub(crate) epoch: u64,
    pub(crate) capacity: usize,
    pub(crate) len: usize,
    pub(crate) total_pushed: u64,
    pub(crate) dropped_overflow: u64,
    pub(crate) next_seq: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ReplayEventBatch {
    pub(crate) cursor: ReplayEventCursor,
    pub(crate) events: Vec<MailEvent>,
    pub(crate) incremental: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    pub database_url: String,
    pub storage_root: String,
    pub auth_enabled: bool,
    pub tui_effects: bool,
}

impl ConfigSnapshot {
    #[must_use]
    pub const fn transport_mode(&self) -> &'static str {
        "browser-replay"
    }
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            database_url: "sanitized demo pack".to_string(),
            storage_root: "public replay; no filesystem access".to_string(),
            auth_enabled: false,
            tui_effects: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCounters {
    pub total: u64,
    pub status_2xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub latency_total_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenDiagnosticSnapshot {
    pub screen: String,
    pub scope: String,
    pub query_params: String,
    pub raw_count: u64,
    pub rendered_count: u64,
    pub dropped_count: u64,
    pub timestamp_micros: i64,
    pub db_url: String,
    pub storage_root: String,
    pub transport_mode: String,
    pub auth_enabled: bool,
}

#[derive(Debug)]
pub struct TuiSharedState {
    events: Mutex<EventRingBuffer>,
    replay_event_capacity: usize,
    event_epoch: AtomicU64,
    config: Mutex<ConfigSnapshot>,
    db_stats: Mutex<DbStatSnapshot>,
    requests: Mutex<RequestCounters>,
    sparkline: Mutex<Vec<f64>>,
    console_log: Mutex<VecDeque<(u64, String)>>,
    diagnostics: Mutex<VecDeque<(u64, ScreenDiagnosticSnapshot)>>,
    console_log_seq: AtomicU64,
    diagnostic_seq: AtomicU64,
    db_stats_gen: AtomicU64,
    request_gen: AtomicU64,
    logical_elapsed_micros: AtomicU64,
}

impl Default for TuiSharedState {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiSharedState {
    #[must_use]
    pub fn new() -> Self {
        Self::with_replay_event_capacity(REPLAY_EVENT_CAPACITY)
    }

    fn with_replay_event_capacity(replay_event_capacity: usize) -> Self {
        let replay_event_capacity = replay_event_capacity.max(1);
        Self {
            events: Mutex::new(replay_event_ring(replay_event_capacity)),
            replay_event_capacity,
            event_epoch: AtomicU64::new(0),
            config: Mutex::new(ConfigSnapshot::default()),
            db_stats: Mutex::new(DbStatSnapshot::default()),
            requests: Mutex::new(RequestCounters::default()),
            sparkline: Mutex::new(Vec::new()),
            console_log: Mutex::new(VecDeque::with_capacity(CONSOLE_LOG_CAPACITY)),
            diagnostics: Mutex::new(VecDeque::with_capacity(SCREEN_DIAGNOSTIC_CAPACITY)),
            console_log_seq: AtomicU64::new(0),
            diagnostic_seq: AtomicU64::new(0),
            db_stats_gen: AtomicU64::new(0),
            request_gen: AtomicU64::new(0),
            logical_elapsed_micros: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_replay_event_capacity(replay_event_capacity: usize) -> Self {
        Self::with_replay_event_capacity(replay_event_capacity)
    }

    pub fn reset(&self) {
        {
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *events = replay_event_ring(self.replay_event_capacity);
            // Increment while the outer event lock is held. Snapshot readers
            // therefore cannot pair the new ring with the old epoch.
            self.event_epoch.fetch_add(1, Ordering::Relaxed);
        }
        *self
            .db_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = DbStatSnapshot::default();
        *self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RequestCounters::default();
        self.sparkline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.console_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.console_log_seq.store(0, Ordering::Relaxed);
        self.diagnostic_seq.store(0, Ordering::Relaxed);
        self.db_stats_gen.store(0, Ordering::Relaxed);
        self.request_gen.store(0, Ordering::Relaxed);
        self.logical_elapsed_micros.store(0, Ordering::Relaxed);
    }

    pub fn set_config(&self, config: ConfigSnapshot) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    pub fn set_elapsed_ms(&self, elapsed_ms: u64) {
        self.logical_elapsed_micros
            .store(elapsed_ms.saturating_mul(1_000), Ordering::Relaxed);
    }

    pub fn push_event(&self, event: MailEvent) -> bool {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_push(event)
            .is_ok()
    }

    #[must_use]
    pub fn tick_events_since_limited(&self, seq: u64, limit: usize) -> Vec<MailEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events_since_seq_limited(seq, limit)
    }

    /// Return every event currently retained by the browser replay ring,
    /// oldest first. Public projection screens cache the derived rows, so the
    /// full 10,000-event accepted-pack boundary is read only when replay data
    /// changes instead of silently freezing the UI at the oldest 2,000 rows.
    #[must_use]
    pub fn replay_events(&self) -> Vec<MailEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_recent(self.replay_event_capacity)
    }

    /// Return either a proof-safe tail delta after `previous` or a complete
    /// replacement snapshot. The decision and event clone happen while the
    /// outer replay lock is held, so a concurrent reset/push cannot split the
    /// cursor from the returned rows.
    #[must_use]
    pub(crate) fn replay_event_batch(
        &self,
        previous: Option<ReplayEventCursor>,
    ) -> ReplayEventBatch {
        let events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stats = events.stats();
        let cursor = ReplayEventCursor {
            epoch: self.event_epoch.load(Ordering::Relaxed),
            capacity: stats.capacity,
            len: stats.len,
            total_pushed: stats.total_pushed,
            dropped_overflow: stats.dropped_overflow,
            next_seq: stats.next_seq,
        };

        if let Some(previous) = previous {
            let expected_delta = cursor.len.saturating_sub(previous.len);
            let expected_next_seq = previous
                .next_seq
                .saturating_add(u64::try_from(expected_delta).unwrap_or(u64::MAX));
            let cursor_allows_append = cursor.epoch == previous.epoch
                && cursor.capacity == previous.capacity
                && cursor.len >= previous.len
                && cursor.total_pushed >= previous.total_pushed
                && cursor.dropped_overflow == previous.dropped_overflow
                && cursor.next_seq == expected_next_seq;
            if cursor_allows_append {
                let tail_seq = previous.next_seq.saturating_sub(1);
                let delta = events.events_since_seq_limited(tail_seq, self.replay_event_capacity);
                let tail_matches = delta.len() == expected_delta
                    && delta
                        .first()
                        .is_none_or(|event| event.seq() == previous.next_seq)
                    && delta
                        .last()
                        .is_none_or(|event| event.seq().saturating_add(1) == cursor.next_seq);
                if tail_matches {
                    return ReplayEventBatch {
                        cursor,
                        events: delta,
                        incremental: true,
                    };
                }
            }
        }

        ReplayEventBatch {
            cursor,
            events: events.iter_recent(self.replay_event_capacity),
            incremental: false,
        }
    }

    #[must_use]
    pub fn event_ring_stats(&self) -> EventRingStats {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stats()
    }

    pub fn record_request(&self, status: u16, duration_ms: u64) {
        let mut counters = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counters.total = counters.total.saturating_add(1);
        counters.latency_total_ms = counters.latency_total_ms.saturating_add(duration_ms);
        match status {
            200..=299 => counters.status_2xx = counters.status_2xx.saturating_add(1),
            400..=499 => counters.status_4xx = counters.status_4xx.saturating_add(1),
            500..=599 => counters.status_5xx = counters.status_5xx.saturating_add(1),
            _ => {}
        }
        drop(counters);
        self.request_gen.fetch_add(1, Ordering::Relaxed);
        let mut samples = self
            .sparkline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if samples.len() >= 60 {
            let _ = samples.remove(0);
        }
        samples.push(duration_ms as f64);
    }

    pub fn set_request_counters(&self, counters: RequestCounters) {
        *self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = counters;
        self.request_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_sparkline(&self, samples: Vec<f64>) {
        *self
            .sparkline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = samples;
    }

    pub fn update_db_stats(&self, stats: DbStatSnapshot) {
        let mut current = self
            .db_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current != stats {
            *current = stats;
            self.db_stats_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn db_stats_snapshot(&self) -> Option<DbStatSnapshot> {
        Some(
            self.db_stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

    #[must_use]
    pub fn sparkline_snapshot(&self) -> Vec<f64> {
        self.sparkline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn request_counters(&self) -> RequestCounters {
        *self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Request-counter generation for the current replay epoch, used by
    /// incremental public projections. Reading the atomic directly avoids
    /// taking the replay-ring lock a second time after an exact event batch has
    /// already been cloned; a replay reset also changes the event epoch.
    #[must_use]
    pub(crate) fn request_generation(&self) -> u64 {
        self.request_gen.load(Ordering::Relaxed)
    }

    /// Human-readable transport label used by the shared production chrome.
    #[must_use]
    pub fn transport_mode_label(&self) -> &'static str {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .transport_mode()
    }

    /// Mean request latency used by the shared production status line.
    #[must_use]
    pub fn avg_latency_ms(&self) -> u64 {
        let counters = self.request_counters();
        counters
            .latency_total_ms
            .checked_div(counters.total)
            .unwrap_or(0)
    }

    #[must_use]
    pub fn uptime(&self) -> Duration {
        Duration::from_micros(self.logical_elapsed_micros.load(Ordering::Relaxed))
    }

    #[must_use]
    pub fn config_snapshot(&self) -> ConfigSnapshot {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn tui_effects_enabled(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tui_effects
    }

    pub fn push_console_log(&self, text: String) {
        let seq = self.console_log_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut log = self
            .console_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.len() >= CONSOLE_LOG_CAPACITY {
            let _ = log.pop_front();
        }
        log.push_back((seq, text));
    }

    #[must_use]
    pub fn console_log_since(&self, since_seq: u64) -> Vec<(u64, String)> {
        self.console_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(seq, _)| *seq > since_seq)
            .cloned()
            .collect()
    }

    pub fn push_screen_diagnostic(&self, snapshot: ScreenDiagnosticSnapshot) {
        let seq = self.diagnostic_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut diagnostics = self
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if diagnostics.len() >= SCREEN_DIAGNOSTIC_CAPACITY {
            let _ = diagnostics.pop_front();
        }
        diagnostics.push_back((seq, snapshot));
    }

    #[must_use]
    pub fn screen_diagnostics_since(&self, since_seq: u64) -> Vec<(u64, ScreenDiagnosticSnapshot)> {
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(seq, _)| *seq > since_seq)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn data_generation(&self) -> DataGeneration {
        let events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        DataGeneration {
            // Read the epoch while holding the same outer lock used by reset.
            // Otherwise a reset could pair its replacement ring with the old
            // epoch when the retained event count happens to be unchanged.
            event_epoch: self.event_epoch.load(Ordering::Relaxed),
            event_total_pushed: events.stats().total_pushed,
            console_log_seq: self.console_log_seq.load(Ordering::Relaxed),
            db_stats_gen: self.db_stats_gen.load(Ordering::Relaxed),
            request_gen: self.request_gen.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_events::MailEventKind;

    #[test]
    fn replay_ring_retains_the_full_mixed_low_severity_pack_boundary() {
        let state = TuiSharedState::new();

        for index in 0..REPLAY_EVENT_CAPACITY {
            let event = if index.is_multiple_of(2) {
                MailEvent::tool_call_start(
                    "synthetic_tool",
                    serde_json::json!({ "index": index }),
                    Some("synthetic-project".to_string()),
                    Some("SyntheticAgent".to_string()),
                )
            } else {
                MailEvent::http_request("GET", "/api/health", 200, 1, "synthetic-client")
            };
            assert!(state.push_event(event), "replay event {index} was dropped");
        }

        let events = state.replay_events();
        let stats = state.event_ring_stats();
        assert_eq!(events.len(), REPLAY_EVENT_CAPACITY);
        assert_eq!(stats.len, REPLAY_EVENT_CAPACITY);
        assert_eq!(stats.total_pushed, REPLAY_EVENT_CAPACITY as u64);
        assert_eq!(stats.sampled_drops, 0);
        assert_eq!(stats.total_drops(), 0);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind() == MailEventKind::ToolCallStart)
                .count(),
            REPLAY_EVENT_CAPACITY / 2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind() == MailEventKind::HttpRequest)
                .count(),
            REPLAY_EVENT_CAPACITY / 2
        );
    }
}
