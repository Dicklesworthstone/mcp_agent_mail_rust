//! Browser-safe in-memory state adapter for the real dashboard screen.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::tui_events::{DbStatSnapshot, EventRingBuffer, EventRingStats, MailEvent};
use crate::tui_screens::DataGeneration;

const CONSOLE_LOG_CAPACITY: usize = 2_000;
const SCREEN_DIAGNOSTIC_CAPACITY: usize = 256;

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
        Self {
            events: Mutex::new(EventRingBuffer::with_capacity(10_000)),
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

    pub fn reset(&self) {
        *self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            EventRingBuffer::with_capacity(10_000);
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
        if counters.total == 0 {
            0
        } else {
            counters.latency_total_ms / counters.total
        }
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
        DataGeneration {
            event_total_pushed: self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stats()
                .total_pushed,
            console_log_seq: self.console_log_seq.load(Ordering::Relaxed),
            db_stats_gen: self.db_stats_gen.load(Ordering::Relaxed),
            request_gen: self.request_gen.load(Ordering::Relaxed),
        }
    }
}
