#![allow(clippy::module_name_repetitions)]

use crate::console;
use crate::tui_events::{
    DbStatSnapshot, EventRingBuffer, EventRingStats, EventSeverity, MailEvent,
};
use mcp_agent_mail_core::Config;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REQUEST_SPARKLINE_CAPACITY: usize = 60;
const REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY: usize = 4096;
/// Max console log entries in the ring buffer.
const CONSOLE_LOG_CAPACITY: usize = 2000;

#[derive(Debug)]
struct AtomicSparkline {
    data: [AtomicU64; REQUEST_SPARKLINE_CAPACITY],
    head: AtomicUsize,
}

impl AtomicSparkline {
    fn new() -> Self {
        Self {
            data: std::array::from_fn(|_| AtomicU64::new(0)),
            head: AtomicUsize::new(0),
        }
    }

    fn push(&self, value: f64) {
        let idx = self.head.fetch_add(1, Ordering::Relaxed) % REQUEST_SPARKLINE_CAPACITY;
        self.data[idx].store(value.to_bits(), Ordering::Relaxed);
    }

    fn snapshot(&self) -> Vec<f64> {
        let head = self.head.load(Ordering::Relaxed);
        let mut result = Vec::with_capacity(REQUEST_SPARKLINE_CAPACITY);
        let count = head.min(REQUEST_SPARKLINE_CAPACITY);
        let start_idx = if head > REQUEST_SPARKLINE_CAPACITY {
            head % REQUEST_SPARKLINE_CAPACITY
        } else {
            0
        };

        for i in 0..count {
            let idx = (start_idx + i) % REQUEST_SPARKLINE_CAPACITY;
            let bits = self.data[idx].load(Ordering::Relaxed);
            result.push(f64::from_bits(bits));
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportBase {
    Mcp,
    Api,
}

impl TransportBase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Api => "api",
        }
    }

    #[must_use]
    pub const fn http_path(self) -> &'static str {
        match self {
            Self::Mcp => "/mcp/",
            Self::Api => "/api/",
        }
    }

    #[must_use]
    pub const fn toggle(self) -> Self {
        match self {
            Self::Mcp => Self::Api,
            Self::Api => Self::Mcp,
        }
    }

    #[must_use]
    pub fn from_http_path(path: &str) -> Option<Self> {
        let trimmed = path.trim().trim_end_matches('/');
        if trimmed.eq_ignore_ascii_case("mcp") || trimmed.eq_ignore_ascii_case("/mcp") {
            return Some(Self::Mcp);
        }
        if trimmed.eq_ignore_ascii_case("api") || trimmed.eq_ignore_ascii_case("/api") {
            return Some(Self::Api);
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerControlMsg {
    Shutdown,
    ToggleTransportBase,
    SetTransportBase(TransportBase),
    /// Send a composed message from the TUI compose panel.
    ComposeEnvelope(crate::tui_compose::ComposeEnvelope),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSnapshot {
    pub endpoint: String,
    pub http_path: String,
    pub web_ui_url: String,
    pub app_environment: String,
    pub auth_enabled: bool,
    pub tui_effects: bool,
    /// Database URL sanitized for UI rendering/logging.
    pub database_url: String,
    /// Raw database URL for internal DB connectivity.
    pub raw_database_url: String,
    pub storage_root: String,
    pub console_theme: String,
    pub tool_filter_profile: String,
    pub tui_debug: bool,
}

impl ConfigSnapshot {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let endpoint = format!(
            "http://{}:{}{}",
            config.http_host, config.http_port, config.http_path
        );
        let web_ui_url = format!("http://{}:{}/mail", config.http_host, config.http_port);
        let database_url = console::sanitize_known_value("database_url", &config.database_url)
            .unwrap_or_else(|| config.database_url.clone());

        Self {
            endpoint,
            http_path: config.http_path.clone(),
            web_ui_url,
            app_environment: config.app_environment.to_string(),
            auth_enabled: config.http_bearer_token.is_some(),
            tui_effects: config.tui_effects,
            database_url,
            raw_database_url: config.database_url.clone(),
            storage_root: config.storage_root.display().to_string(),
            console_theme: format!("{:?}", config.console_theme),
            tool_filter_profile: config.tool_filter.profile.clone(),
            tui_debug: config.tui_debug,
        }
    }

    #[must_use]
    pub fn transport_mode(&self) -> &'static str {
        TransportBase::from_http_path(&self.http_path).map_or("custom", TransportBase::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestCounters {
    pub total: u64,
    pub status_2xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub latency_total_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTerminalEvent {
    Key { key: String, modifiers: u8 },
    Resize { cols: u16, rows: u16 },
}

/// Shared snapshot for mouse drag-and-drop of messages across TUI screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDragSnapshot {
    pub message_id: i64,
    pub subject: String,
    pub source_thread_id: String,
    pub source_project_slug: String,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub hovered_thread_id: Option<String>,
    pub hovered_is_valid: bool,
    pub invalid_hover: bool,
}

/// Shared snapshot for keyboard-driven message move operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardMoveSnapshot {
    pub message_id: i64,
    pub subject: String,
    pub source_thread_id: String,
    pub source_project_slug: String,
}

#[derive(Debug)]
pub struct TuiSharedState {
    events: EventRingBuffer,
    requests_total: AtomicU64,
    requests_2xx: AtomicU64,
    requests_4xx: AtomicU64,
    requests_5xx: AtomicU64,
    latency_total_ms: AtomicU64,
    started_at: Instant,
    shutdown: AtomicBool,
    detach_headless: AtomicBool,
    config_snapshot: Mutex<ConfigSnapshot>,
    db_stats: Mutex<DbStatSnapshot>,
    sparkline_data: AtomicSparkline,
    remote_terminal_events: Mutex<VecDeque<RemoteTerminalEvent>>,
    message_drag: Mutex<Option<MessageDragSnapshot>>,
    keyboard_move: Mutex<Option<KeyboardMoveSnapshot>>,
    server_control_tx: Mutex<Option<Sender<ServerControlMsg>>>,
    /// Console log ring buffer: `(seq, text)` pairs for tool call cards etc.
    console_log: Mutex<VecDeque<(u64, String)>>,
    console_log_seq: AtomicU64,
}

impl TuiSharedState {
    #[must_use]
    pub fn new(config: &Config) -> Arc<Self> {
        Self::with_event_capacity(config, crate::tui_events::DEFAULT_EVENT_RING_CAPACITY)
    }

    #[must_use]
    pub fn with_event_capacity(config: &Config, event_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            events: EventRingBuffer::with_capacity(event_capacity),
            requests_total: AtomicU64::new(0),
            requests_2xx: AtomicU64::new(0),
            requests_4xx: AtomicU64::new(0),
            requests_5xx: AtomicU64::new(0),
            latency_total_ms: AtomicU64::new(0),
            started_at: Instant::now(),
            shutdown: AtomicBool::new(false),
            detach_headless: AtomicBool::new(false),
            config_snapshot: Mutex::new(ConfigSnapshot::from_config(config)),
            db_stats: Mutex::new(DbStatSnapshot::default()),
            sparkline_data: AtomicSparkline::new(),
            remote_terminal_events: Mutex::new(VecDeque::with_capacity(
                REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY,
            )),
            message_drag: Mutex::new(None),
            keyboard_move: Mutex::new(None),
            server_control_tx: Mutex::new(None),
            console_log: Mutex::new(VecDeque::with_capacity(CONSOLE_LOG_CAPACITY)),
            console_log_seq: AtomicU64::new(0),
        })
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)] // 80+ call sites; by-value is clearer API intent
    pub fn push_event(&self, event: MailEvent) -> bool {
        // Keep event publication non-blocking so HTTP/tool handlers cannot stall
        // behind a contended TUI ring-buffer lock.
        if self.events.try_push(event.clone()).is_some() {
            return true;
        }

        if event.severity() < EventSeverity::Info {
            return false;
        }

        // Give important events a few non-blocking retries, then drop instead of
        // risking transport stalls while the UI thread is rendering.
        for _ in 0..3 {
            std::thread::yield_now();
            if self.events.try_push(event.clone()).is_some() {
                return true;
            }
        }

        false
    }

    #[must_use]
    pub fn recent_events(&self, limit: usize) -> Vec<MailEvent> {
        self.events.try_iter_recent(limit).unwrap_or_default()
    }

    #[must_use]
    pub fn events_since(&self, seq: u64) -> Vec<MailEvent> {
        self.events.events_since_seq(seq)
    }

    #[must_use]
    pub fn event_ring_stats(&self) -> EventRingStats {
        self.events.stats()
    }

    pub fn record_request(&self, status: u16, duration_ms: u64) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.latency_total_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
        match status {
            200..=299 => {
                self.requests_2xx.fetch_add(1, Ordering::Relaxed);
            }
            400..=499 => {
                self.requests_4xx.fetch_add(1, Ordering::Relaxed);
            }
            500..=599 => {
                self.requests_5xx.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }

        let duration_ms_f64 = f64::from(u32::try_from(duration_ms).unwrap_or(u32::MAX));
        self.sparkline_data.push(duration_ms_f64);
    }

    pub fn update_db_stats(&self, stats: DbStatSnapshot) {
        let mut current = self
            .db_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = stats;
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub fn request_headless_detach(&self) {
        self.detach_headless.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_headless_detach_requested(&self) -> bool {
        self.detach_headless.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn take_headless_detach_requested(&self) -> bool {
        self.detach_headless.swap(false, Ordering::Relaxed)
    }

    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    #[must_use]
    pub fn config_snapshot(&self) -> ConfigSnapshot {
        self.config_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn update_config_snapshot(&self, snapshot: ConfigSnapshot) {
        if let Ok(mut guard) = self.config_snapshot.lock() {
            *guard = snapshot;
        }
    }

    /// Snapshot the active message drag state, if any.
    #[must_use]
    pub fn message_drag_snapshot(&self) -> Option<MessageDragSnapshot> {
        self.message_drag
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Replace the active message drag state.
    pub fn set_message_drag_snapshot(&self, drag: Option<MessageDragSnapshot>) {
        if let Ok(mut guard) = self.message_drag.lock() {
            *guard = drag;
        }
    }

    /// Clear any active message drag state.
    pub fn clear_message_drag_snapshot(&self) {
        self.set_message_drag_snapshot(None);
    }

    /// Snapshot the active keyboard move marker, if any.
    #[must_use]
    pub fn keyboard_move_snapshot(&self) -> Option<KeyboardMoveSnapshot> {
        self.keyboard_move
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Replace the active keyboard move marker.
    pub fn set_keyboard_move_snapshot(&self, marker: Option<KeyboardMoveSnapshot>) {
        if let Ok(mut guard) = self.keyboard_move.lock() {
            *guard = marker;
        }
    }

    /// Clear any active keyboard move marker.
    pub fn clear_keyboard_move_snapshot(&self) {
        self.set_keyboard_move_snapshot(None);
    }

    pub fn set_server_control_sender(&self, tx: Sender<ServerControlMsg>) {
        if let Ok(mut guard) = self.server_control_tx.lock() {
            *guard = Some(tx);
        }
    }

    /// Queue a remote terminal event from browser ingress.
    ///
    /// Returns `true` when an older event had to be dropped to keep the queue bounded.
    #[must_use]
    pub fn push_remote_terminal_event(&self, event: RemoteTerminalEvent) -> bool {
        let mut queue = self
            .remote_terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dropped_oldest = if queue.len() >= REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY {
            let _ = queue.pop_front();
            true
        } else {
            false
        };
        queue.push_back(event);
        dropped_oldest
    }

    #[must_use]
    pub fn drain_remote_terminal_events(&self, max_events: usize) -> Vec<RemoteTerminalEvent> {
        if max_events == 0 {
            return Vec::new();
        }
        let mut queue = self
            .remote_terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let drain_count = max_events.min(queue.len());
        queue.drain(..drain_count).collect()
    }

    #[must_use]
    pub fn remote_terminal_queue_len(&self) -> usize {
        self.remote_terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[must_use]
    pub fn try_send_server_control(&self, msg: ServerControlMsg) -> bool {
        self.server_control_tx
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .is_some_and(|tx| tx.send(msg).is_ok())
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
        self.sparkline_data.snapshot()
    }

    #[must_use]
    pub fn request_counters(&self) -> RequestCounters {
        RequestCounters {
            total: self.requests_total.load(Ordering::Relaxed),
            status_2xx: self.requests_2xx.load(Ordering::Relaxed),
            status_4xx: self.requests_4xx.load(Ordering::Relaxed),
            status_5xx: self.requests_5xx.load(Ordering::Relaxed),
            latency_total_ms: self.latency_total_ms.load(Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn avg_latency_ms(&self) -> u64 {
        let counters = self.request_counters();
        counters
            .latency_total_ms
            .checked_div(counters.total)
            .unwrap_or(0)
    }

    /// Push a console log line (tool call card, HTTP request, etc.).
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

    /// Return console log entries with sequence > `since_seq`.
    #[must_use]
    pub fn console_log_since(&self, since_seq: u64) -> Vec<(u64, String)> {
        let log = self
            .console_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.iter()
            .filter(|(seq, _)| *seq > since_seq)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_events::MailEventKind;
    use std::thread;

    fn config_for_test() -> Config {
        Config {
            database_url: "postgres://alice:supersecret@localhost:5432/mail".to_string(),
            http_bearer_token: Some("token".to_string()),
            ..Config::default()
        }
    }

    #[test]
    fn config_snapshot_masks_database_url() {
        let config = config_for_test();
        let snapshot = ConfigSnapshot::from_config(&config);
        assert!(!snapshot.database_url.contains("supersecret"));
        assert!(snapshot.raw_database_url.contains("supersecret"));
        assert!(snapshot.auth_enabled);
        assert!(snapshot.endpoint.contains("http://"));
    }

    #[test]
    fn record_request_updates_counters_and_latency() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.record_request(200, 10);
        state.record_request(404, 30);
        state.record_request(500, 20);

        let counters = state.request_counters();
        assert_eq!(counters.total, 3);
        assert_eq!(counters.status_2xx, 1);
        assert_eq!(counters.status_4xx, 1);
        assert_eq!(counters.status_5xx, 1);
        assert_eq!(state.avg_latency_ms(), 20);
    }

    #[test]
    fn record_request_large_duration_clamped_for_sparkline() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.record_request(200, u64::MAX);

        let counters = state.request_counters();
        assert_eq!(counters.total, 1);
        assert_eq!(counters.latency_total_ms, u64::MAX);

        let sparkline = state.sparkline_snapshot();
        assert_eq!(sparkline.len(), 1);
        assert!((sparkline[0] - f64::from(u32::MAX)).abs() < f64::EPSILON);
    }

    #[test]
    fn sparkline_is_bounded() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        for _ in 0..(REQUEST_SPARKLINE_CAPACITY + 20) {
            state.record_request(200, 5);
        }
        let sparkline = state.sparkline_snapshot();
        assert_eq!(sparkline.len(), REQUEST_SPARKLINE_CAPACITY);
    }

    #[test]
    fn push_event_and_retrieve_events() {
        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 4);

        assert!(state.push_event(MailEvent::http_request("GET", "/a", 200, 1, "127.0.0.1")));
        assert!(state.push_event(MailEvent::tool_call_start(
            "fetch_inbox",
            serde_json::Value::Null,
            Some("proj".to_string()),
            Some("TealMeadow".to_string()),
        )));

        let recent = state.recent_events(8);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].kind(), MailEventKind::HttpRequest);
        assert_eq!(recent[1].kind(), MailEventKind::ToolCallStart);
        assert_eq!(state.events_since(1).len(), 1);
    }

    #[test]
    fn shutdown_signal_propagates() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(!state.is_shutdown_requested());
        state.request_shutdown();
        assert!(state.is_shutdown_requested());
    }

    #[test]
    fn headless_detach_signal_propagates() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(!state.is_headless_detach_requested());
        state.request_headless_detach();
        assert!(state.is_headless_detach_requested());
        assert!(state.take_headless_detach_requested());
        assert!(!state.is_headless_detach_requested());
    }

    #[test]
    fn concurrent_push_and_reads_are_safe() {
        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 2048);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let state_clone = Arc::clone(&state);
            handles.push(thread::spawn(move || {
                for _ in 0..250 {
                    let _ = state_clone.push_event(MailEvent::http_request(
                        "GET",
                        "/concurrent",
                        200,
                        1,
                        "127.0.0.1",
                    ));
                }
            }));
        }
        for handle in handles {
            handle.join().expect("join writer");
        }

        let counters = state.event_ring_stats();
        assert!(counters.total_pushed > 0);
        assert!(state.recent_events(10).len() <= 10);
    }

    #[test]
    fn shared_state_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TuiSharedState>();
    }

    // ── Bridge edge-case tests ──────────────────────────────────

    #[test]
    fn avg_latency_zero_when_no_requests() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert_eq!(state.avg_latency_ms(), 0);
    }

    #[test]
    fn request_counter_status_ranges() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        // 1xx - no specific counter
        state.record_request(100, 1);
        // 3xx - no specific counter
        state.record_request(301, 1);
        let counters = state.request_counters();
        assert_eq!(counters.total, 2);
        assert_eq!(counters.status_2xx, 0);
        assert_eq!(counters.status_4xx, 0);
        assert_eq!(counters.status_5xx, 0);
    }

    #[test]
    fn sparkline_starts_empty() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(state.sparkline_snapshot().is_empty());
    }

    #[test]
    fn sparkline_single_data_point() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.record_request(200, 42);
        let sparkline = state.sparkline_snapshot();
        assert_eq!(sparkline.len(), 1);
        assert!((sparkline[0] - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_snapshot_transport_mode_custom_path() {
        let snap = ConfigSnapshot {
            endpoint: "http://127.0.0.1:8765/custom/v1/".into(),
            http_path: "/custom/v1/".into(),
            web_ui_url: "http://127.0.0.1:8765/mail".into(),
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
        assert_eq!(snap.transport_mode(), "custom");
    }

    #[test]
    fn config_snapshot_transport_mode_mcp() {
        let snap = ConfigSnapshot {
            http_path: "/mcp/".into(),
            ..ConfigSnapshot {
                endpoint: String::new(),
                http_path: String::new(),
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
            }
        };
        assert_eq!(snap.transport_mode(), "mcp");
    }

    #[test]
    fn config_snapshot_transport_mode_api() {
        let snap = ConfigSnapshot {
            http_path: "/api/".into(),
            ..ConfigSnapshot {
                endpoint: String::new(),
                http_path: String::new(),
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
            }
        };
        assert_eq!(snap.transport_mode(), "api");
    }

    #[test]
    fn transport_base_toggle() {
        assert_eq!(TransportBase::Mcp.toggle(), TransportBase::Api);
        assert_eq!(TransportBase::Api.toggle(), TransportBase::Mcp);
    }

    #[test]
    fn transport_base_from_http_path_variants() {
        assert_eq!(
            TransportBase::from_http_path("/mcp/"),
            Some(TransportBase::Mcp)
        );
        assert_eq!(
            TransportBase::from_http_path("/MCP/"),
            Some(TransportBase::Mcp)
        );
        assert_eq!(
            TransportBase::from_http_path("mcp"),
            Some(TransportBase::Mcp)
        );
        assert_eq!(
            TransportBase::from_http_path("/api/"),
            Some(TransportBase::Api)
        );
        assert_eq!(
            TransportBase::from_http_path("/API"),
            Some(TransportBase::Api)
        );
        assert_eq!(
            TransportBase::from_http_path("api"),
            Some(TransportBase::Api)
        );
        assert_eq!(TransportBase::from_http_path("/custom/"), None);
        assert_eq!(TransportBase::from_http_path(""), None);
    }

    #[test]
    fn transport_base_from_http_path_trims_whitespace() {
        assert_eq!(
            TransportBase::from_http_path("  /mcp/  "),
            Some(TransportBase::Mcp)
        );
        assert_eq!(
            TransportBase::from_http_path("  api  "),
            Some(TransportBase::Api)
        );
    }

    #[test]
    fn transport_base_str_and_path() {
        assert_eq!(TransportBase::Mcp.as_str(), "mcp");
        assert_eq!(TransportBase::Api.as_str(), "api");
        assert_eq!(TransportBase::Mcp.http_path(), "/mcp/");
        assert_eq!(TransportBase::Api.http_path(), "/api/");
    }

    #[test]
    fn update_config_snapshot_replaces_previous() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let snap1 = state.config_snapshot();

        let new_snap = ConfigSnapshot {
            endpoint: "http://127.0.0.1:9999/api/".into(),
            http_path: "/api/".into(),
            web_ui_url: "http://127.0.0.1:9999/mail".into(),
            app_environment: "production".into(),
            auth_enabled: true,
            tui_effects: false,
            database_url: "sqlite:///./new.sqlite3".into(),
            raw_database_url: "sqlite:///./new.sqlite3".into(),
            storage_root: "/tmp/new".into(),
            console_theme: "default".into(),
            tool_filter_profile: "minimal".into(),
            tui_debug: false,
        };
        state.update_config_snapshot(new_snap);
        let snap2 = state.config_snapshot();
        assert_eq!(snap2.endpoint, "http://127.0.0.1:9999/api/");
        assert!(snap2.auth_enabled);
        assert_ne!(snap1.endpoint, snap2.endpoint);
    }

    #[test]
    fn update_db_stats_and_snapshot() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        let snap = state.db_stats_snapshot().unwrap();
        assert_eq!(snap.projects, 0);

        state.update_db_stats(crate::tui_events::DbStatSnapshot {
            projects: 5,
            agents: 10,
            messages: 100,
            ..Default::default()
        });

        let snap = state.db_stats_snapshot().unwrap();
        assert_eq!(snap.projects, 5);
        assert_eq!(snap.agents, 10);
        assert_eq!(snap.messages, 100);
    }

    #[test]
    fn server_control_without_sender_returns_false() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        // No sender set, should return false
        assert!(!state.try_send_server_control(ServerControlMsg::Shutdown));
    }

    #[test]
    fn server_control_with_dropped_receiver_returns_false() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let (tx, rx) = std::sync::mpsc::channel();
        state.set_server_control_sender(tx);
        drop(rx); // Drop receiver
        assert!(!state.try_send_server_control(ServerControlMsg::Shutdown));
    }

    #[test]
    fn server_control_with_live_receiver_succeeds() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let (tx, rx) = std::sync::mpsc::channel();
        state.set_server_control_sender(tx);

        assert!(state.try_send_server_control(ServerControlMsg::ToggleTransportBase));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(100)).ok(),
            Some(ServerControlMsg::ToggleTransportBase)
        );
    }

    #[test]
    fn uptime_is_positive() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(state.uptime().as_nanos() > 0);
    }

    #[test]
    fn with_event_capacity_customizes_ring() {
        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 5);
        for i in 0..10 {
            let _ = state.push_event(crate::tui_events::MailEvent::http_request(
                "GET",
                format!("/{i}"),
                200,
                1,
                "127.0.0.1",
            ));
        }
        let ring_stats = state.event_ring_stats();
        assert_eq!(ring_stats.capacity, 5);
        assert_eq!(ring_stats.len, 5);
    }

    #[test]
    fn remote_terminal_event_queue_roundtrip() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert_eq!(state.remote_terminal_queue_len(), 0);

        assert!(!state.push_remote_terminal_event(RemoteTerminalEvent::Key {
            key: "j".to_string(),
            modifiers: 1,
        }));
        assert!(
            !state.push_remote_terminal_event(RemoteTerminalEvent::Resize {
                cols: 120,
                rows: 40,
            })
        );
        assert_eq!(state.remote_terminal_queue_len(), 2);

        let drained = state.drain_remote_terminal_events(8);
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            drained[0],
            RemoteTerminalEvent::Key {
                ref key,
                modifiers: 1
            } if key == "j"
        ));
        assert!(matches!(
            drained[1],
            RemoteTerminalEvent::Resize {
                cols: 120,
                rows: 40
            }
        ));
        assert_eq!(state.remote_terminal_queue_len(), 0);
    }

    #[test]
    fn remote_terminal_event_queue_is_bounded() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        let mut dropped = 0_usize;
        for i in 0..(REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY + 32) {
            if state.push_remote_terminal_event(RemoteTerminalEvent::Key {
                key: format!("k{i}"),
                modifiers: 0,
            }) {
                dropped += 1;
            }
        }

        assert_eq!(
            state.remote_terminal_queue_len(),
            REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY
        );
        assert_eq!(dropped, 32);
    }

    #[test]
    fn remote_terminal_event_drain_respects_limit_and_order() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        assert!(!state.push_remote_terminal_event(RemoteTerminalEvent::Key {
            key: "a".to_string(),
            modifiers: 0,
        }));
        assert!(!state.push_remote_terminal_event(RemoteTerminalEvent::Key {
            key: "b".to_string(),
            modifiers: 0,
        }));
        assert!(!state.push_remote_terminal_event(RemoteTerminalEvent::Key {
            key: "c".to_string(),
            modifiers: 0,
        }));

        let drained = state.drain_remote_terminal_events(2);
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            &drained[0],
            RemoteTerminalEvent::Key { key, modifiers: 0 } if key == "a"
        ));
        assert!(matches!(
            &drained[1],
            RemoteTerminalEvent::Key { key, modifiers: 0 } if key == "b"
        ));

        assert_eq!(state.remote_terminal_queue_len(), 1);
        let remaining = state.drain_remote_terminal_events(8);
        assert_eq!(remaining.len(), 1);
        assert!(matches!(
            &remaining[0],
            RemoteTerminalEvent::Key { key, modifiers: 0 } if key == "c"
        ));
    }

    #[test]
    fn remote_terminal_event_drain_zero_limit_noop() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(
            !state.push_remote_terminal_event(RemoteTerminalEvent::Resize {
                cols: 100,
                rows: 30,
            })
        );
        let drained = state.drain_remote_terminal_events(0);
        assert!(drained.is_empty());
        assert_eq!(state.remote_terminal_queue_len(), 1);
    }

    #[test]
    fn message_drag_snapshot_round_trip() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(state.message_drag_snapshot().is_none());

        let snapshot = MessageDragSnapshot {
            message_id: 42,
            subject: "Move me".to_string(),
            source_thread_id: "thread-a".to_string(),
            source_project_slug: "proj".to_string(),
            cursor_x: 12,
            cursor_y: 8,
            hovered_thread_id: Some("thread-b".to_string()),
            hovered_is_valid: true,
            invalid_hover: false,
        };
        state.set_message_drag_snapshot(Some(snapshot.clone()));
        assert_eq!(state.message_drag_snapshot(), Some(snapshot));

        state.clear_message_drag_snapshot();
        assert!(state.message_drag_snapshot().is_none());
    }

    #[test]
    fn keyboard_move_snapshot_round_trip() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(state.keyboard_move_snapshot().is_none());

        let snapshot = KeyboardMoveSnapshot {
            message_id: 7,
            subject: "Re-thread".to_string(),
            source_thread_id: "thread-a".to_string(),
            source_project_slug: "proj".to_string(),
        };
        state.set_keyboard_move_snapshot(Some(snapshot.clone()));
        assert_eq!(state.keyboard_move_snapshot(), Some(snapshot));

        state.clear_keyboard_move_snapshot();
        assert!(state.keyboard_move_snapshot().is_none());
    }

    #[test]
    fn console_log_since_filters_monotonically() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        state.push_console_log("first".to_string());
        state.push_console_log("second".to_string());
        state.push_console_log("third".to_string());

        let all = state.console_log_since(0);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, 1);
        assert_eq!(all[1].0, 2);
        assert_eq!(all[2].0, 3);
        assert_eq!(all[2].1, "third");

        let tail = state.console_log_since(2);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].0, 3);
        assert_eq!(tail[0].1, "third");
    }

    #[test]
    fn console_log_ring_is_bounded_to_capacity() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        for i in 0..(CONSOLE_LOG_CAPACITY + 5) {
            state.push_console_log(format!("line-{i}"));
        }

        let entries = state.console_log_since(0);
        assert_eq!(entries.len(), CONSOLE_LOG_CAPACITY);
        assert_eq!(entries[0].0, 6);
        assert_eq!(entries.last().map(|(seq, _)| *seq), Some(2005));
    }

    #[test]
    fn console_log_since_future_seq_returns_empty() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.push_console_log("alpha".to_string());
        state.push_console_log("beta".to_string());

        let future = state.console_log_since(999);
        assert!(future.is_empty());
    }
}
