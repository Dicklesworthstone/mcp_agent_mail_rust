#![allow(clippy::module_name_repetitions)]

use crate::console;
use crate::tui_events::{
    DbStatSnapshot, EventRingBuffer, EventRingStats, EventSeverity, MailEvent,
};
use asupersync::channel::mpsc::{SendError, Sender};
use asupersync::time::{sleep, wall_now};
use mcp_agent_mail_core::Config;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const REQUEST_SPARKLINE_CAPACITY: usize = 60;
const REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY: usize = 4096;
/// Max console log entries in the ring buffer.
const CONSOLE_LOG_CAPACITY: usize = 2000;
/// Max retained screen diagnostics snapshots.
const SCREEN_DIAGNOSTIC_CAPACITY: usize = 512;
const SERVER_CONTROL_SEND_RETRY_TIMEOUT: Duration = Duration::from_millis(250);
const SERVER_CONTROL_SEND_RETRY_SLEEP: Duration = Duration::from_millis(5);

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
            crate::connect_authority_host(&config.http_host),
            config.http_port,
            config.http_path
        );
        let web_ui_url = crate::build_web_ui_url(
            &config.http_host,
            config.http_port,
            config.http_bearer_token.as_deref(),
        );
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

/// Structured per-screen diagnostics used to trace DB/query/render mismatches.
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

impl ScreenDiagnosticSnapshot {
    #[must_use]
    pub fn to_log_line(&self) -> String {
        format!(
            "[screen_diag] screen={} scope={} raw={} rendered={} dropped={} params={} db={} storage={} transport={} auth={} ts={}",
            self.screen,
            self.scope,
            self.raw_count,
            self.rendered_count,
            self.dropped_count,
            self.query_params,
            self.db_url,
            self.storage_root,
            self.transport_mode,
            self.auth_enabled,
            self.timestamp_micros
        )
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let normalized = value.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn strict_truth_assertions_enabled(config: &ConfigSnapshot) -> bool {
    config.tui_debug || cfg!(debug_assertions) || env_truthy("AM_TUI_STRICT_TRUTH_ASSERTIONS")
}

pub(crate) fn query_params_explain_empty_state(query_params: &str) -> bool {
    query_params.split([';', ',']).any(|part| {
        let Some((raw_key, raw_value)) = part.split_once('=') else {
            return false;
        };
        let key = raw_key.trim().to_ascii_lowercase();
        let value = raw_value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
            .to_string();
        if value.is_empty() {
            return false;
        }
        let value_lc = value.to_ascii_lowercase();
        match key.as_str() {
            "filter" | "query" | "search" | "thread" | "thread_id" | "project" | "project_slug"
            | "agent" | "agent_name" | "sender" | "recipient" | "importance" | "ack" | "status" => {
                !matches!(value_lc.as_str(), "all" | "any" | "none" | "*" | "false")
            }
            "page" => value_lc.parse::<u64>().ok().is_some_and(|page| page > 1),
            "offset" => value_lc
                .parse::<u64>()
                .ok()
                .is_some_and(|offset| offset > 0),
            _ => false,
        }
    })
}

fn assert_screen_truth(snapshot: &ScreenDiagnosticSnapshot) {
    let has_raw_rows = snapshot.raw_count > 0;
    let rendered_none = snapshot.rendered_count == 0;
    let user_filter_active = query_params_explain_empty_state(&snapshot.query_params);
    assert!(
        !(has_raw_rows && rendered_none && !user_filter_active),
        "[truth_assertion] screen={} scope={} raw_count={} rendered_count={} query_params={} \
possible silent false-empty state",
        snapshot.screen,
        snapshot.scope,
        snapshot.raw_count,
        snapshot.rendered_count,
        snapshot.query_params
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTerminalEvent {
    Key { key: String, modifiers: u8 },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTerminalQueueStats {
    pub depth: usize,
    pub dropped_oldest_total: u64,
    pub resize_coalesced_total: u64,
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

#[derive(Debug, Clone)]
struct TickEventBatch {
    from_seq: u64,
    to_seq: u64,
    events: Arc<Vec<MailEvent>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DbWarmupState {
    #[default]
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StartupSignalState {
    first_paint: bool,
    db_warmup: DbWarmupState,
}

/// How far ahead (in microseconds) to warn about expiring reservations (5 min).
pub(crate) const RESERVATION_EXPIRY_WARN_MICROS: i64 = 5 * 60 * 1_000_000;

#[derive(Debug)]
struct DbStatsState {
    snapshot: DbStatSnapshot,
    generation: u64,
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
    db_stats: Mutex<DbStatsState>,
    sparkline_data: AtomicSparkline,
    remote_terminal_events: Mutex<VecDeque<RemoteTerminalEvent>>,
    remote_terminal_dropped_oldest: AtomicU64,
    remote_terminal_resize_coalesced: AtomicU64,
    message_drag: Mutex<Option<MessageDragSnapshot>>,
    keyboard_move: Mutex<Option<KeyboardMoveSnapshot>>,
    server_control_tx: Mutex<Option<Sender<ServerControlMsg>>>,
    /// Per-tick shared event batch prepared by the TUI event loop so screens
    /// can ingest recent events without each re-locking/re-scanning the ring.
    tick_event_batch: Mutex<Option<TickEventBatch>>,
    /// Console log ring buffer: `(seq, text)` pairs for tool call cards etc.
    console_log: Mutex<VecDeque<(u64, String)>>,
    console_log_seq: AtomicU64,
    /// Per-screen diagnostics snapshots, keyed by insertion sequence.
    screen_diagnostics: Mutex<VecDeque<(u64, ScreenDiagnosticSnapshot)>>,
    screen_diagnostic_seq: AtomicU64,
    /// Generation counter bumped when `update_db_stats` changes semantic DB content.
    db_stats_gen: AtomicU64,
    /// Whether the DB poller most recently observed a usable MCP Agent Mail DB context.
    db_context_available: AtomicBool,
    /// Fast-path latch for urgent inactive-screen cadence based on pending acks.
    urgent_ack_pending: AtomicBool,
    /// Earliest active reservation expiry timestamp used for urgent cadence checks.
    next_active_reservation_expiry_micros: AtomicI64,
    /// Generation counter bumped on each `record_request` call.
    request_gen: AtomicU64,
    /// Startup latches used to stage heavyweight background work behind first
    /// paint and behind the initial DB readiness result.
    startup_signals: (Mutex<StartupSignalState>, Condvar),
    /// Latest rendered TUI frame for the web dashboard mirror.
    web_dashboard_frame: crate::tui_web_dashboard::WebDashboardFrameStore,
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
            db_stats: Mutex::new(DbStatsState {
                snapshot: DbStatSnapshot::default(),
                generation: 0,
            }),
            sparkline_data: AtomicSparkline::new(),
            remote_terminal_events: Mutex::new(VecDeque::with_capacity(
                REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY,
            )),
            remote_terminal_dropped_oldest: AtomicU64::new(0),
            remote_terminal_resize_coalesced: AtomicU64::new(0),
            message_drag: Mutex::new(None),
            keyboard_move: Mutex::new(None),
            server_control_tx: Mutex::new(None),
            tick_event_batch: Mutex::new(None),
            console_log: Mutex::new(VecDeque::with_capacity(CONSOLE_LOG_CAPACITY)),
            console_log_seq: AtomicU64::new(0),
            screen_diagnostics: Mutex::new(VecDeque::with_capacity(SCREEN_DIAGNOSTIC_CAPACITY)),
            screen_diagnostic_seq: AtomicU64::new(0),
            db_stats_gen: AtomicU64::new(0),
            db_context_available: AtomicBool::new(false),
            urgent_ack_pending: AtomicBool::new(false),
            next_active_reservation_expiry_micros: AtomicI64::new(0),
            request_gen: AtomicU64::new(0),
            startup_signals: (Mutex::new(StartupSignalState::default()), Condvar::new()),
            web_dashboard_frame: crate::tui_web_dashboard::WebDashboardFrameStore::new(),
        })
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)] // 80+ call sites; by-value is clearer API intent
    pub fn push_event(&self, mut event: MailEvent) -> bool {
        // Keep event publication non-blocking so HTTP/tool handlers cannot stall
        // behind a contended TUI ring-buffer lock.
        match self.events.try_push(event) {
            Ok(_) => return true,
            Err(returned_event) => event = returned_event,
        }

        if event.severity() < EventSeverity::Info {
            return false;
        }

        // Give important events a few non-blocking retries, then drop instead of
        // risking transport stalls while the UI thread is rendering.
        let mut backoff = std::time::Duration::from_millis(2);
        for _ in 0..3 {
            std::thread::sleep(backoff);
            match self.events.try_push(event) {
                Ok(_) => return true,
                Err(returned_event) => event = returned_event,
            }
            backoff = backoff
                .checked_mul(2)
                .unwrap_or_else(|| std::time::Duration::from_millis(8))
                .min(std::time::Duration::from_millis(8));
        }

        false
    }

    /// Asynchronous version of `push_event` that uses `asupersync::time::sleep`.
    ///
    /// Use this in async contexts (like MCP tool handlers) to avoid blocking
    /// worker threads.
    pub async fn push_event_async(&self, mut event: MailEvent) -> bool {
        match self.events.try_push(event) {
            Ok(_) => return true,
            Err(returned_event) => event = returned_event,
        }

        if event.severity() < EventSeverity::Info {
            return false;
        }

        let mut backoff = std::time::Duration::from_millis(2);
        for _ in 0..3 {
            let () = sleep(wall_now(), backoff).await;
            match self.events.try_push(event) {
                Ok(_) => return true,
                Err(returned_event) => event = returned_event,
            }
            backoff = backoff
                .checked_mul(2)
                .unwrap_or_else(|| std::time::Duration::from_millis(8))
                .min(std::time::Duration::from_millis(8));
        }

        false
    }

    #[must_use]
    pub fn recent_events(&self, limit: usize) -> Vec<MailEvent> {
        self.events.try_iter_recent(limit).unwrap_or_default()
    }

    #[must_use]
    pub fn recent_event_signals(&self, limit: usize) -> Vec<(i64, EventSeverity)> {
        self.events.recent_signals(limit)
    }

    #[must_use]
    pub fn events_since(&self, seq: u64) -> Vec<MailEvent> {
        self.events.events_since_seq(seq)
    }

    #[must_use]
    pub fn events_since_limited(&self, seq: u64, limit: usize) -> Vec<MailEvent> {
        self.events.events_since_seq_limited(seq, limit)
    }

    /// Publish a per-tick shared event batch. Returns the highest sequence seen.
    ///
    /// Screens can consume this via [`Self::tick_events_since_limited`] to avoid
    /// repeated lock/scan work against the ring buffer during the same tick.
    pub fn publish_tick_event_batch(&self, from_seq: u64, events: Vec<MailEvent>) -> u64 {
        let to_seq = events.last().map_or(from_seq, MailEvent::seq);
        let batch = TickEventBatch {
            from_seq,
            to_seq,
            events: Arc::new(events),
        };
        let mut guard = self
            .tick_event_batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(batch);
        to_seq
    }

    /// Read events since `seq` using the shared per-tick batch when possible.
    ///
    /// Falls back to direct ring-buffer scan when the requested sequence is not
    /// covered by the published batch (for correctness under concurrent pushes).
    #[must_use]
    pub fn tick_events_since_limited(&self, seq: u64, limit: usize) -> Vec<MailEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let guard = self
            .tick_event_batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(batch) = guard.as_ref()
            && seq >= batch.from_seq
            && seq < batch.to_seq
        {
            return batch
                .events
                .iter()
                .filter(|event| event.seq() > seq)
                .take(limit)
                .cloned()
                .collect();
        }
        drop(guard);
        self.events.events_since_seq_limited(seq, limit)
    }

    #[must_use]
    pub fn event_ring_stats(&self) -> EventRingStats {
        self.events.stats()
    }

    pub fn record_request(&self, status: u16, duration_ms: u64) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.request_gen.fetch_add(1, Ordering::Relaxed);
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
        let semantic_change = db_stats_semantically_changed(&current.snapshot, &stats);
        let first_snapshot_delivery =
            current.snapshot.timestamp_micros == 0 && stats.timestamp_micros > 0;
        if semantic_change {
            self.urgent_ack_pending
                .store(stats.ack_pending > 0, Ordering::Relaxed);
            self.next_active_reservation_expiry_micros.store(
                next_active_reservation_expiry_micros(&stats),
                Ordering::Relaxed,
            );
        }
        current.snapshot = stats;
        if semantic_change || first_snapshot_delivery {
            current.generation = current.generation.saturating_add(1);
            self.db_stats_gen
                .store(current.generation, Ordering::Relaxed);
        }
    }

    pub fn refresh_db_stats_timestamp(&self, timestamp_micros: i64) {
        self.db_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .timestamp_micros = timestamp_micros;
    }

    pub fn mark_db_context_available(&self) {
        self.db_context_available.store(true, Ordering::Relaxed);
    }

    pub fn mark_db_context_unavailable(&self) {
        self.db_context_available.store(false, Ordering::Relaxed);
    }

    #[must_use]
    pub fn db_context_available(&self) -> bool {
        self.db_context_available.load(Ordering::Relaxed)
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.startup_signals.1.notify_all();
    }

    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub fn mark_first_paint(&self) {
        let (signals_lock, signals_cv) = &self.startup_signals;
        let should_notify = {
            let mut signals = signals_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if signals.first_paint {
                false
            } else {
                signals.first_paint = true;
                true
            }
        };
        if should_notify {
            signals_cv.notify_all();
        }
    }

    #[must_use]
    pub fn first_paint_seen(&self) -> bool {
        self.startup_signals
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_paint
    }

    #[must_use]
    pub fn wait_for_first_paint(&self, timeout: Duration) -> bool {
        self.wait_for_startup_signal(timeout, |signals| signals.first_paint)
    }

    pub fn mark_db_ready(&self) {
        let (signals_lock, signals_cv) = &self.startup_signals;
        let should_notify = {
            let mut signals = signals_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if signals.db_warmup == DbWarmupState::Ready {
                false
            } else {
                signals.db_warmup = DbWarmupState::Ready;
                true
            }
        };
        if should_notify {
            signals_cv.notify_all();
        }
    }

    #[must_use]
    pub fn db_ready(&self) -> bool {
        self.db_warmup_state() == DbWarmupState::Ready
    }

    pub fn mark_db_warmup_failed(&self) {
        let (signals_lock, signals_cv) = &self.startup_signals;
        let should_notify = {
            let mut signals = signals_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if signals.db_warmup == DbWarmupState::Pending {
                signals.db_warmup = DbWarmupState::Failed;
                true
            } else {
                false
            }
        };
        if should_notify {
            signals_cv.notify_all();
        }
    }

    #[must_use]
    pub fn db_warmup_state(&self) -> DbWarmupState {
        self.startup_signals
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .db_warmup
    }

    #[must_use]
    pub fn wait_for_db_warmup(&self, timeout: Duration) -> DbWarmupState {
        self.wait_for_startup_state(timeout, |signals| {
            signals.db_warmup != DbWarmupState::Pending
        })
        .db_warmup
    }

    #[must_use]
    pub fn wait_for_db_ready(&self, timeout: Duration) -> bool {
        self.wait_for_db_warmup(timeout) == DbWarmupState::Ready
    }

    pub fn request_headless_detach(&self) {
        self.detach_headless.store(true, Ordering::Relaxed);
        self.startup_signals.1.notify_all();
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

    #[must_use]
    pub fn tui_effects_enabled(&self) -> bool {
        self.config_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tui_effects
    }

    #[must_use]
    pub fn transport_mode_label(&self) -> &'static str {
        let guard = self
            .config_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.transport_mode()
    }

    #[must_use]
    fn wait_for_startup_signal<P>(&self, timeout: Duration, predicate: P) -> bool
    where
        P: Fn(StartupSignalState) -> bool + Copy,
    {
        predicate(self.wait_for_startup_state(timeout, predicate))
    }

    #[must_use]
    fn wait_for_startup_state<P>(&self, timeout: Duration, predicate: P) -> StartupSignalState
    where
        P: Fn(StartupSignalState) -> bool + Copy,
    {
        let (signals_lock, signals_cv) = &self.startup_signals;
        let mut signals = signals_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if predicate(*signals)
            || self.is_shutdown_requested()
            || self.is_headless_detach_requested()
        {
            return *signals;
        }
        if timeout.is_zero() {
            return *signals;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline
                || self.is_shutdown_requested()
                || self.is_headless_detach_requested()
            {
                return *signals;
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_signals, _) = signals_cv
                .wait_timeout(signals, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            signals = next_signals;
            if predicate(*signals) {
                return *signals;
            }
        }
    }

    pub fn update_config_snapshot(&self, snapshot: ConfigSnapshot) {
        let mut guard = self
            .config_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = snapshot;
    }

    /// Snapshot the active message drag state, if any.
    #[must_use]
    pub fn message_drag_snapshot(&self) -> Option<MessageDragSnapshot> {
        self.message_drag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the active message drag state.
    pub fn set_message_drag_snapshot(&self, drag: Option<MessageDragSnapshot>) {
        let mut guard = self
            .message_drag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = drag;
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the active keyboard move marker.
    pub fn set_keyboard_move_snapshot(&self, marker: Option<KeyboardMoveSnapshot>) {
        let mut guard = self
            .keyboard_move
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = marker;
    }

    /// Clear any active keyboard move marker.
    pub fn clear_keyboard_move_snapshot(&self) {
        self.set_keyboard_move_snapshot(None);
    }

    pub fn set_server_control_sender(&self, tx: Sender<ServerControlMsg>) {
        let mut guard = self
            .server_control_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(tx);
    }

    /// Access the web dashboard frame store for frame capture / serving.
    #[must_use]
    pub const fn web_dashboard_frame_store(
        &self,
    ) -> &crate::tui_web_dashboard::WebDashboardFrameStore {
        &self.web_dashboard_frame
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

        // Resize bursts are common during browser-driven tab traversal and only
        // the latest dimensions matter. Coalesce tail resize events in-place
        // to reduce queue pressure without dropping semantic input.
        if let RemoteTerminalEvent::Resize { cols, rows } = event {
            if let Some(RemoteTerminalEvent::Resize {
                cols: last_cols,
                rows: last_rows,
            }) = queue.back_mut()
            {
                *last_cols = cols;
                *last_rows = rows;
                self.remote_terminal_resize_coalesced
                    .fetch_add(1, Ordering::Relaxed);
                return false;
            }

            let dropped_oldest = if queue.len() >= REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY {
                let _ = queue.pop_front();
                self.remote_terminal_dropped_oldest
                    .fetch_add(1, Ordering::Relaxed);
                true
            } else {
                false
            };
            queue.push_back(RemoteTerminalEvent::Resize { cols, rows });
            return dropped_oldest;
        }

        let dropped_oldest = if queue.len() >= REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY {
            let _ = queue.pop_front();
            self.remote_terminal_dropped_oldest
                .fetch_add(1, Ordering::Relaxed);
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
    pub fn remote_terminal_queue_stats(&self) -> RemoteTerminalQueueStats {
        let depth = self
            .remote_terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        RemoteTerminalQueueStats {
            depth,
            dropped_oldest_total: self.remote_terminal_dropped_oldest.load(Ordering::Relaxed),
            resize_coalesced_total: self
                .remote_terminal_resize_coalesced
                .load(Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn try_send_server_control(&self, msg: ServerControlMsg) -> bool {
        let Some(tx) = self
            .server_control_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return false;
        };

        let deadline = Instant::now() + SERVER_CONTROL_SEND_RETRY_TIMEOUT;
        let mut pending = msg;
        loop {
            match tx.try_send(pending) {
                Ok(()) => return true,
                Err(SendError::Disconnected(_) | SendError::Cancelled(_)) => return false,
                Err(SendError::Full(msg)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    pending = msg;
                    std::thread::sleep(SERVER_CONTROL_SEND_RETRY_SLEEP);
                }
            }
        }
    }

    #[must_use]
    pub fn db_stats_snapshot(&self) -> Option<DbStatSnapshot> {
        Some(
            self.db_stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot
                .clone(),
        )
    }

    #[must_use]
    pub fn db_stats_snapshot_with_generation(&self) -> (DbStatSnapshot, u64) {
        let state = self
            .db_stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.snapshot.clone(), state.generation)
    }

    #[must_use]
    pub fn urgent_cadence_bypass_active(&self, now_micros: i64) -> bool {
        if self.urgent_ack_pending.load(Ordering::Relaxed) {
            return true;
        }
        let next_expiry = self
            .next_active_reservation_expiry_micros
            .load(Ordering::Relaxed);
        next_expiry > now_micros
            && next_expiry.saturating_sub(now_micros) <= RESERVATION_EXPIRY_WARN_MICROS
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

    /// Record a screen-level diagnostics snapshot.
    ///
    /// When TUI debug mode is enabled, a compact diagnostics line is also sent
    /// to the console log stream so operators can inspect query→render flow.
    pub fn push_screen_diagnostic(&self, snapshot: ScreenDiagnosticSnapshot) {
        let config = self.config_snapshot();
        if strict_truth_assertions_enabled(&config) {
            assert_screen_truth(&snapshot);
        }
        let seq = self
            .screen_diagnostic_seq
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let mut diagnostics = self
            .screen_diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if diagnostics.len() >= SCREEN_DIAGNOSTIC_CAPACITY {
            let _ = diagnostics.pop_front();
        }
        let log_line = if config.tui_debug {
            Some(snapshot.to_log_line())
        } else {
            None
        };
        diagnostics.push_back((seq, snapshot));
        drop(diagnostics);

        if let Some(line) = log_line {
            self.push_console_log(line);
        }
    }

    /// Snapshot all data generation counters for dirty-state tracking.
    ///
    /// Screens store the returned value and later compare it against a fresh
    /// snapshot via [`DataGeneration::dirty_since`] to determine which data
    /// channels have changed.
    #[must_use]
    pub fn data_generation(&self) -> crate::tui_screens::DataGeneration {
        crate::tui_screens::DataGeneration {
            event_total_pushed: self.events.stats().total_pushed,
            console_log_seq: self.console_log_seq.load(Ordering::Relaxed),
            db_stats_gen: self.db_stats_gen.load(Ordering::Relaxed),
            request_gen: self.request_gen.load(Ordering::Relaxed),
        }
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

    /// Return screen diagnostics snapshots with sequence > `since_seq`.
    #[must_use]
    pub fn screen_diagnostics_since(&self, since_seq: u64) -> Vec<(u64, ScreenDiagnosticSnapshot)> {
        let diagnostics = self
            .screen_diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        diagnostics
            .iter()
            .filter(|(seq, _)| *seq > since_seq)
            .cloned()
            .collect()
    }

    /// Return up to `limit` most recent diagnostics for a given screen.
    #[must_use]
    pub fn screen_diagnostics_recent(
        &self,
        screen: &str,
        limit: usize,
    ) -> Vec<(u64, ScreenDiagnosticSnapshot)> {
        if limit == 0 {
            return Vec::new();
        }
        let diagnostics = self
            .screen_diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut result = Vec::with_capacity(limit.min(diagnostics.len()));
        for (seq, diag) in diagnostics.iter().rev() {
            if diag.screen == screen {
                result.push((*seq, diag.clone()));
                if result.len() >= limit {
                    break;
                }
            }
        }
        drop(diagnostics);
        result
    }
}

fn db_stats_semantically_changed(previous: &DbStatSnapshot, next: &DbStatSnapshot) -> bool {
    previous.projects != next.projects
        || previous.agents != next.agents
        || previous.messages != next.messages
        || previous.file_reservations != next.file_reservations
        || previous.contact_links != next.contact_links
        || previous.ack_pending != next.ack_pending
        || previous.agents_list != next.agents_list
        || previous.projects_list != next.projects_list
        || previous.contacts_list != next.contacts_list
        || previous.reservation_snapshots != next.reservation_snapshots
}

fn next_active_reservation_expiry_micros(snapshot: &DbStatSnapshot) -> i64 {
    snapshot
        .reservation_snapshots
        .iter()
        .filter(|reservation| reservation.released_ts.is_none_or(|ts| ts <= 0))
        .map(|reservation| reservation.expires_ts)
        .min()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_events::MailEventKind;
    use asupersync::channel::mpsc;
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
        let limited = state.events_since_limited(0, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].kind(), MailEventKind::HttpRequest);
    }

    #[test]
    fn tick_event_batch_returns_shared_subset_when_seq_is_covered() {
        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 16);
        assert!(state.push_event(MailEvent::http_request("GET", "/a", 200, 1, "127.0.0.1")));
        assert!(state.push_event(MailEvent::http_request("GET", "/b", 200, 1, "127.0.0.1")));
        assert!(state.push_event(MailEvent::http_request("GET", "/c", 200, 1, "127.0.0.1")));

        let from_seq = 1;
        let batch = state.events_since_limited(from_seq, 8);
        let to_seq = state.publish_tick_event_batch(from_seq, batch);
        assert_eq!(to_seq, 3);

        let covered = state.tick_events_since_limited(1, 8);
        let seqs: Vec<u64> = covered.iter().map(MailEvent::seq).collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn tick_event_batch_falls_back_when_seq_not_covered() {
        let config = Config::default();
        let state = TuiSharedState::with_event_capacity(&config, 16);
        assert!(state.push_event(MailEvent::http_request("GET", "/a", 200, 1, "127.0.0.1")));
        assert!(state.push_event(MailEvent::http_request("GET", "/b", 200, 1, "127.0.0.1")));

        // Publish empty shared batch (from seq already at tail).
        let to_seq = state.publish_tick_event_batch(2, Vec::new());
        assert_eq!(to_seq, 2);

        // Request older history; must fall back to ring scan.
        let events = state.tick_events_since_limited(0, 8);
        let seqs: Vec<u64> = events.iter().map(MailEvent::seq).collect();
        assert_eq!(seqs, vec![1, 2]);
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
    fn startup_latches_mark_and_wait() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(!state.first_paint_seen());
        assert!(!state.db_ready());
        assert!(!state.db_context_available());
        assert_eq!(state.db_warmup_state(), DbWarmupState::Pending);
        assert!(!state.wait_for_first_paint(Duration::ZERO));
        assert!(!state.wait_for_db_ready(Duration::ZERO));
        assert_eq!(
            state.wait_for_db_warmup(Duration::ZERO),
            DbWarmupState::Pending
        );

        let state_clone = Arc::clone(&state);
        let waiter = thread::spawn(move || {
            let first_paint = state_clone.wait_for_first_paint(Duration::from_millis(250));
            let db_state = state_clone.wait_for_db_warmup(Duration::from_millis(250));
            (first_paint, db_state)
        });

        thread::sleep(Duration::from_millis(10));
        state.mark_first_paint();
        state.mark_db_ready();

        let (first_paint, db_state) = waiter.join().expect("join startup waiter");
        assert!(first_paint);
        assert_eq!(db_state, DbWarmupState::Ready);
        assert!(state.first_paint_seen());
        assert!(state.db_ready());
        assert_eq!(state.db_warmup_state(), DbWarmupState::Ready);
    }

    #[test]
    fn startup_db_warmup_failure_unblocks_waiters_and_ready_can_promote_later() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let state_clone = Arc::clone(&state);
        let waiter =
            thread::spawn(move || state_clone.wait_for_db_warmup(Duration::from_millis(250)));

        thread::sleep(Duration::from_millis(10));
        state.mark_db_warmup_failed();

        let db_state = waiter.join().expect("join warmup waiter");
        assert_eq!(db_state, DbWarmupState::Failed);
        assert_eq!(state.db_warmup_state(), DbWarmupState::Failed);
        assert!(!state.db_ready());
        assert!(!state.wait_for_db_ready(Duration::ZERO));

        state.mark_db_ready();

        assert_eq!(state.db_warmup_state(), DbWarmupState::Ready);
        assert!(state.db_ready());
        assert!(state.wait_for_db_ready(Duration::ZERO));
    }

    #[test]
    fn db_context_availability_tracks_latest_poll_result() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        assert!(!state.db_context_available());

        state.mark_db_context_available();
        assert!(state.db_context_available());

        state.mark_db_context_unavailable();
        assert!(!state.db_context_available());
    }

    #[test]
    fn headless_detach_unblocks_startup_waiters() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let state_clone = Arc::clone(&state);
        let waiter = thread::spawn(move || {
            let first_paint = state_clone.wait_for_first_paint(Duration::from_secs(5));
            let db_state = state_clone.wait_for_db_warmup(Duration::from_secs(5));
            (first_paint, db_state)
        });

        thread::sleep(Duration::from_millis(10));
        state.request_headless_detach();

        let (first_paint, db_state) = waiter.join().expect("join startup waiter");
        assert!(!first_paint);
        assert_eq!(db_state, DbWarmupState::Pending);
        assert!(state.is_headless_detach_requested());
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
        let (tx, mut rx) = mpsc::channel(1);
        state.set_server_control_sender(tx);
        rx.close();
        drop(rx); // Drop receiver
        assert!(!state.try_send_server_control(ServerControlMsg::Shutdown));
    }

    #[test]
    fn server_control_with_live_receiver_succeeds() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let (tx, mut rx) = mpsc::channel(1);
        state.set_server_control_sender(tx);

        assert!(state.try_send_server_control(ServerControlMsg::ToggleTransportBase));
        assert_eq!(
            rx.try_recv().ok(),
            Some(ServerControlMsg::ToggleTransportBase)
        );
    }

    #[test]
    fn server_control_retries_when_queue_is_temporarily_full() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let (tx, mut rx) = mpsc::channel(1);
        let fill_tx = tx.clone();
        state.set_server_control_sender(tx);
        fill_tx
            .try_send(ServerControlMsg::ToggleTransportBase)
            .expect("fill control queue");

        let drain = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let first = rx.try_recv().ok();
            let deadline = Instant::now() + Duration::from_millis(250);
            let second = loop {
                match rx.try_recv() {
                    Ok(msg) => break Some(msg),
                    Err(mpsc::RecvError::Empty) => {
                        if Instant::now() >= deadline {
                            break None;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(mpsc::RecvError::Disconnected | mpsc::RecvError::Cancelled) => break None,
                }
            };
            (first, second)
        });

        assert!(
            state.try_send_server_control(ServerControlMsg::SetTransportBase(TransportBase::Api,))
        );

        let (first, second) = drain.join().expect("join drain thread");
        assert_eq!(first, Some(ServerControlMsg::ToggleTransportBase));
        assert_eq!(
            second,
            Some(ServerControlMsg::SetTransportBase(TransportBase::Api))
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
        let queue_stats = state.remote_terminal_queue_stats();
        assert_eq!(queue_stats.depth, REMOTE_TERMINAL_EVENT_QUEUE_CAPACITY);
        assert_eq!(queue_stats.dropped_oldest_total, 32);
        assert_eq!(queue_stats.resize_coalesced_total, 0);
    }

    #[test]
    fn remote_terminal_resize_events_coalesce_tail() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        assert!(
            !state.push_remote_terminal_event(RemoteTerminalEvent::Resize { cols: 80, rows: 24 })
        );
        assert!(
            !state.push_remote_terminal_event(RemoteTerminalEvent::Resize {
                cols: 120,
                rows: 42,
            })
        );

        let queue_stats = state.remote_terminal_queue_stats();
        assert_eq!(queue_stats.depth, 1);
        assert_eq!(queue_stats.dropped_oldest_total, 0);
        assert_eq!(queue_stats.resize_coalesced_total, 1);

        let drained = state.drain_remote_terminal_events(8);
        assert_eq!(
            drained,
            vec![RemoteTerminalEvent::Resize {
                cols: 120,
                rows: 42
            }]
        );
    }

    #[test]
    fn remote_terminal_resize_coalescing_preserves_prior_key_event() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        assert!(!state.push_remote_terminal_event(RemoteTerminalEvent::Key {
            key: "j".to_string(),
            modifiers: 0,
        }));
        assert!(
            !state.push_remote_terminal_event(RemoteTerminalEvent::Resize { cols: 90, rows: 30 })
        );
        assert!(
            !state.push_remote_terminal_event(RemoteTerminalEvent::Resize {
                cols: 100,
                rows: 31,
            })
        );

        let drained = state.drain_remote_terminal_events(8);
        assert_eq!(
            drained,
            vec![
                RemoteTerminalEvent::Key {
                    key: "j".to_string(),
                    modifiers: 0,
                },
                RemoteTerminalEvent::Resize {
                    cols: 100,
                    rows: 31
                },
            ]
        );
        let queue_stats = state.remote_terminal_queue_stats();
        assert_eq!(queue_stats.depth, 0);
        assert_eq!(queue_stats.resize_coalesced_total, 1);
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

    fn sample_screen_diag(screen: &str) -> ScreenDiagnosticSnapshot {
        ScreenDiagnosticSnapshot {
            screen: screen.to_string(),
            scope: "db_stats.agents_list".to_string(),
            query_params: "filter=red;sort=Active".to_string(),
            raw_count: 100,
            rendered_count: 12,
            dropped_count: 88,
            timestamp_micros: 42,
            db_url: "sqlite:///tmp/am.db".to_string(),
            storage_root: "/tmp/am".to_string(),
            transport_mode: "mcp".to_string(),
            auth_enabled: true,
        }
    }

    #[test]
    fn screen_diagnostics_round_trip() {
        let state = TuiSharedState::new(&Config::default());
        state.push_screen_diagnostic(sample_screen_diag("agents"));
        state.push_screen_diagnostic(sample_screen_diag("projects"));

        let rows = state.screen_diagnostics_since(0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[0].1.screen, "agents");
        assert_eq!(rows[1].0, 2);
        assert_eq!(rows[1].1.screen, "projects");
    }

    #[test]
    fn screen_diagnostics_log_when_debug_enabled() {
        let config = Config {
            tui_debug: true,
            ..Config::default()
        };
        let state = TuiSharedState::new(&config);
        state.push_screen_diagnostic(sample_screen_diag("agents"));

        let logs = state.console_log_since(0);
        assert_eq!(logs.len(), 1);
        assert!(logs[0].1.contains("[screen_diag]"));
        assert!(logs[0].1.contains("screen=agents"));
    }

    #[test]
    fn screen_diagnostics_do_not_log_when_debug_disabled() {
        let state = TuiSharedState::new(&Config::default());
        state.push_screen_diagnostic(sample_screen_diag("agents"));
        assert!(state.console_log_since(0).is_empty());
    }

    #[test]
    fn screen_diagnostics_panic_on_silent_false_empty_without_filter() {
        let state = TuiSharedState::new(&Config::default());
        let mut snapshot = sample_screen_diag("agents");
        snapshot.query_params = r#"filter="";sort_col=name;sort_asc=true"#.to_string();
        snapshot.raw_count = 5;
        snapshot.rendered_count = 0;
        snapshot.dropped_count = 5;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.push_screen_diagnostic(snapshot);
        }));
        assert!(result.is_err(), "expected truth assertion to panic");
    }

    #[test]
    fn screen_diagnostics_allow_empty_when_filter_is_active() {
        let state = TuiSharedState::new(&Config::default());
        let mut snapshot = sample_screen_diag("agents");
        snapshot.query_params = r#"filter="urgent";sort_col=name;sort_asc=true"#.to_string();
        snapshot.raw_count = 5;
        snapshot.rendered_count = 0;
        snapshot.dropped_count = 5;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.push_screen_diagnostic(snapshot);
        }));
        assert!(result.is_ok(), "active user filter should suppress panic");
    }

    #[test]
    fn screen_diagnostics_allow_empty_with_structured_filter_context() {
        let state = TuiSharedState::new(&Config::default());
        let mut snapshot = sample_screen_diag("dashboard");
        snapshot.query_params =
            "filter=query:incident|verbosity:verbose|types:MessageReceived".to_string();
        snapshot.raw_count = 12;
        snapshot.rendered_count = 0;
        snapshot.dropped_count = 12;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.push_screen_diagnostic(snapshot);
        }));
        assert!(
            result.is_ok(),
            "structured non-empty filter context should suppress panic"
        );
    }

    #[test]
    fn screen_diagnostics_panic_when_filter_is_all() {
        let state = TuiSharedState::new(&Config::default());
        let mut snapshot = sample_screen_diag("agents");
        snapshot.query_params = "filter=all;sort_col=name;sort_asc=true".to_string();
        snapshot.raw_count = 5;
        snapshot.rendered_count = 0;
        snapshot.dropped_count = 5;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.push_screen_diagnostic(snapshot);
        }));
        assert!(
            result.is_err(),
            "filter=all should not suppress truth assertion"
        );
    }

    #[test]
    fn screen_diagnostics_panic_for_messages_global_all_signature() {
        let state = TuiSharedState::new(&Config::default());
        let mut snapshot = sample_screen_diag("messages");
        snapshot.query_params = "raw=4;rendered=0;filter=all;mode=global;project=all;method=LocalCache;live_added=0;total_results=4".to_string();
        snapshot.raw_count = 4;
        snapshot.rendered_count = 0;
        snapshot.dropped_count = 4;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.push_screen_diagnostic(snapshot);
        }));
        assert!(
            result.is_err(),
            "messages global/all signature should not suppress truth assertion"
        );
    }

    #[test]
    fn screen_diagnostics_allow_messages_local_project_filter_signature() {
        let state = TuiSharedState::new(&Config::default());
        let mut snapshot = sample_screen_diag("messages");
        snapshot.query_params = "raw=4;rendered=0;filter=project:alpha;mode=local;project=alpha;method=LocalCache;live_added=0;total_results=4".to_string();
        snapshot.raw_count = 4;
        snapshot.rendered_count = 0;
        snapshot.dropped_count = 4;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.push_screen_diagnostic(snapshot);
        }));
        assert!(
            result.is_ok(),
            "messages project filter signature should suppress truth assertion"
        );
    }

    // ── Data generation / dirty-state tests ──────────────────────

    #[test]
    fn data_generation_starts_at_zero() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let data_gen = state.data_generation();
        assert_eq!(data_gen.event_total_pushed, 0);
        assert_eq!(data_gen.console_log_seq, 0);
        assert_eq!(data_gen.db_stats_gen, 0);
        assert_eq!(data_gen.request_gen, 0);
    }

    #[test]
    fn data_generation_event_push_bumps_counter() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let gen_before = state.data_generation();
        let _ = state.push_event(crate::tui_events::MailEvent::ServerStarted {
            seq: 0,
            timestamp_micros: 1,
            source: crate::tui_events::EventSource::Lifecycle,
            redacted: false,
            endpoint: "http://127.0.0.1:8765/mcp/".to_string(),
            config_summary: "test".to_string(),
        });
        let gen_after = state.data_generation();
        assert!(gen_after.event_total_pushed > gen_before.event_total_pushed);
    }

    #[test]
    fn data_generation_console_log_bumps_counter() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let gen_before = state.data_generation();
        state.push_console_log("hello".into());
        let gen_after = state.data_generation();
        assert!(gen_after.console_log_seq > gen_before.console_log_seq);
    }

    #[test]
    fn data_generation_db_stats_bumps_counter_on_semantic_change() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let gen_before = state.data_generation();
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            projects: 1,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        let gen_after = state.data_generation();
        assert!(gen_after.db_stats_gen > gen_before.db_stats_gen);
    }

    #[test]
    fn data_generation_db_stats_ignores_timestamp_only_refresh() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            projects: 1,
            timestamp_micros: 10,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        let gen_before = state.data_generation();
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            projects: 1,
            timestamp_micros: 20,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        let gen_after = state.data_generation();
        assert_eq!(gen_after.db_stats_gen, gen_before.db_stats_gen);
    }

    #[test]
    fn data_generation_db_stats_bumps_counter_on_first_zero_snapshot_delivery() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let gen_before = state.data_generation();
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            timestamp_micros: 10,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        let gen_after = state.data_generation();
        assert!(gen_after.db_stats_gen > gen_before.db_stats_gen);
    }

    #[test]
    fn refresh_db_stats_timestamp_preserves_db_stats_generation() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            projects: 1,
            timestamp_micros: 10,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        let gen_before = state.data_generation();

        state.refresh_db_stats_timestamp(20);

        let gen_after = state.data_generation();
        let snapshot = state.db_stats_snapshot().expect("snapshot");
        assert_eq!(gen_after.db_stats_gen, gen_before.db_stats_gen);
        assert_eq!(snapshot.timestamp_micros, 20);
    }

    #[test]
    fn db_stats_snapshot_with_generation_returns_matching_first_delivery_generation() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            timestamp_micros: 10,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });

        let (snapshot, generation) = state.db_stats_snapshot_with_generation();
        assert_eq!(snapshot.timestamp_micros, 10);
        assert_eq!(generation, state.data_generation().db_stats_gen);
    }

    #[test]
    fn data_generation_record_request_bumps_counter() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let gen_before = state.data_generation();
        state.record_request(200, 5);
        let gen_after = state.data_generation();
        assert!(gen_after.request_gen > gen_before.request_gen);
    }

    #[test]
    fn urgent_cadence_bypass_uses_ack_pending_without_snapshot_clone() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            ack_pending: 1,
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        assert!(state.urgent_cadence_bypass_active(0));
    }

    #[test]
    fn urgent_cadence_bypass_uses_expiring_reservations_without_snapshot_clone() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        state.update_db_stats(crate::tui_bridge::DbStatSnapshot {
            reservation_snapshots: vec![crate::tui_events::ReservationSnapshot {
                id: 1,
                project_slug: "alpha".to_string(),
                agent_name: "BlueLake".to_string(),
                path_pattern: "src/lib.rs".to_string(),
                exclusive: true,
                granted_ts: 0,
                expires_ts: RESERVATION_EXPIRY_WARN_MICROS - 1,
                released_ts: None,
            }],
            ..crate::tui_bridge::DbStatSnapshot::default()
        });
        assert!(state.urgent_cadence_bypass_active(0));
    }

    #[test]
    fn dirty_since_no_change_returns_clean() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let data_gen = state.data_generation();
        let flags = crate::tui_screens::dirty_since(&data_gen, &state.data_generation());
        assert!(!flags.any());
    }

    #[test]
    fn dirty_since_selective_channel_detection() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);
        let data_gen = state.data_generation();

        // Only bump console log
        state.push_console_log("test".into());

        let flags = crate::tui_screens::dirty_since(&data_gen, &state.data_generation());
        assert!(!flags.events, "events should be clean");
        assert!(flags.console_log, "console_log should be dirty");
        assert!(!flags.db_stats, "db_stats should be clean");
        assert!(!flags.requests, "requests should be clean");
    }

    // ── D2: dirty-state invalidation tests (br-legjy.4.2) ──────────

    #[test]
    fn stale_sentinel_produces_all_dirty_flags() {
        let stale = crate::tui_screens::DataGeneration::stale();
        let fresh = crate::tui_screens::DataGeneration::default();
        let flags = crate::tui_screens::dirty_since(&stale, &fresh);
        assert!(flags.events, "stale→fresh: events should be dirty");
        assert!(
            flags.console_log,
            "stale→fresh: console_log should be dirty"
        );
        assert!(flags.db_stats, "stale→fresh: db_stats should be dirty");
        assert!(flags.requests, "stale→fresh: requests should be dirty");
        assert!(flags.any(), "stale→fresh: any() must be true");
    }

    #[test]
    fn dirty_flags_all_returns_all_dirty() {
        let flags = crate::tui_screens::DirtyFlags::all();
        assert!(flags.events);
        assert!(flags.console_log);
        assert!(flags.db_stats);
        assert!(flags.requests);
        assert!(flags.any());
    }

    #[test]
    fn dirty_flags_default_returns_all_clean() {
        let flags = crate::tui_screens::DirtyFlags::default();
        assert!(!flags.events);
        assert!(!flags.console_log);
        assert!(!flags.db_stats);
        assert!(!flags.requests);
        assert!(!flags.any());
    }

    #[test]
    fn dirty_since_events_only() {
        let prev = crate::tui_screens::DataGeneration::default();
        let curr = crate::tui_screens::DataGeneration {
            event_total_pushed: 1,
            ..Default::default()
        };
        let flags = crate::tui_screens::dirty_since(&prev, &curr);
        assert!(flags.events);
        assert!(!flags.console_log);
        assert!(!flags.db_stats);
        assert!(!flags.requests);
    }

    #[test]
    fn dirty_since_db_stats_only() {
        let prev = crate::tui_screens::DataGeneration::default();
        let curr = crate::tui_screens::DataGeneration {
            db_stats_gen: 42,
            ..Default::default()
        };
        let flags = crate::tui_screens::dirty_since(&prev, &curr);
        assert!(!flags.events);
        assert!(!flags.console_log);
        assert!(flags.db_stats);
        assert!(!flags.requests);
    }

    #[test]
    fn dirty_since_requests_only() {
        let prev = crate::tui_screens::DataGeneration::default();
        let curr = crate::tui_screens::DataGeneration {
            request_gen: 7,
            ..Default::default()
        };
        let flags = crate::tui_screens::dirty_since(&prev, &curr);
        assert!(!flags.events);
        assert!(!flags.console_log);
        assert!(!flags.db_stats);
        assert!(flags.requests);
    }

    #[test]
    fn dirty_since_multiple_channels_simultaneously() {
        let prev = crate::tui_screens::DataGeneration::default();
        let curr = crate::tui_screens::DataGeneration {
            event_total_pushed: 5,
            console_log_seq: 3,
            db_stats_gen: 0,
            request_gen: 0,
        };
        let flags = crate::tui_screens::dirty_since(&prev, &curr);
        assert!(flags.events);
        assert!(flags.console_log);
        assert!(!flags.db_stats);
        assert!(!flags.requests);
        assert!(flags.any());
    }

    #[test]
    fn dirty_since_identical_generations_are_clean() {
        let snapshot = crate::tui_screens::DataGeneration {
            event_total_pushed: 10,
            console_log_seq: 20,
            db_stats_gen: 30,
            request_gen: 40,
        };
        let flags = crate::tui_screens::dirty_since(&snapshot, &snapshot);
        assert!(!flags.any(), "identical generations must yield all-clean");
    }

    #[test]
    fn dirty_since_resets_after_snapshot_update() {
        let config = Config::default();
        let state = TuiSharedState::new(&config);

        // Take initial snapshot
        let gen1 = state.data_generation();
        // Mutate console log
        state.push_console_log("line-1".into());
        let gen2 = state.data_generation();

        // gen1→gen2: console_log dirty
        let flags = crate::tui_screens::dirty_since(&gen1, &gen2);
        assert!(flags.console_log);

        // gen2→gen2: nothing changed since we snapshotted
        let flags2 = crate::tui_screens::dirty_since(&gen2, &gen2);
        assert!(!flags2.any(), "after snapshot update, should be clean");
    }

    #[test]
    fn stale_sentinel_differs_from_default() {
        let stale = crate::tui_screens::DataGeneration::stale();
        let default = crate::tui_screens::DataGeneration::default();
        assert_ne!(stale, default, "stale must differ from default");
    }

    // ── E8: query_params_explain_empty_state invariants ──────────────

    #[test]
    fn e8_empty_query_params_not_user_filter() {
        assert!(
            !super::query_params_explain_empty_state(""),
            "empty query_params must not explain empty state"
        );
    }

    #[test]
    fn e8_filter_all_not_user_filter() {
        assert!(
            !super::query_params_explain_empty_state("filter=all"),
            "filter=all is not a user filter"
        );
        assert!(
            !super::query_params_explain_empty_state("filter=any"),
            "filter=any is not a user filter"
        );
        assert!(
            !super::query_params_explain_empty_state("filter=none"),
            "filter=none is not a user filter"
        );
        assert!(
            !super::query_params_explain_empty_state("filter=*"),
            "filter=* is not a user filter"
        );
    }

    #[test]
    fn e8_active_filter_is_user_filter() {
        assert!(
            super::query_params_explain_empty_state("filter=urgent"),
            "filter=urgent should be recognized as user filter"
        );
        assert!(
            super::query_params_explain_empty_state("query=something"),
            "query=something should be recognized"
        );
        assert!(
            super::query_params_explain_empty_state("project=my-proj"),
            "project=my-proj should be recognized"
        );
        assert!(
            super::query_params_explain_empty_state("agent=RedFox"),
            "agent=RedFox should be recognized"
        );
    }

    #[test]
    fn e8_page_offset_user_filter() {
        assert!(
            super::query_params_explain_empty_state("page=2"),
            "page=2 should explain empty state (pagination)"
        );
        assert!(
            !super::query_params_explain_empty_state("page=1"),
            "page=1 should not explain empty state"
        );
        assert!(
            super::query_params_explain_empty_state("offset=50"),
            "offset=50 should explain empty state"
        );
        assert!(
            !super::query_params_explain_empty_state("offset=0"),
            "offset=0 should not explain empty state"
        );
    }

    #[test]
    fn e8_case_insensitive_filter_keys() {
        assert!(
            super::query_params_explain_empty_state("Filter=urgent"),
            "key matching should be case-insensitive"
        );
        assert!(
            super::query_params_explain_empty_state("QUERY=test"),
            "key matching should be case-insensitive"
        );
    }

    #[test]
    fn e8_semicolon_separated_params() {
        assert!(
            super::query_params_explain_empty_state("raw=0;filter=urgent;rendered=0"),
            "semicolon-separated params should be parsed"
        );
        assert!(
            !super::query_params_explain_empty_state("raw=0;filter=all;rendered=0"),
            "filter=all among other params should not be a user filter"
        );
    }

    #[test]
    fn e8_empty_value_not_user_filter() {
        assert!(
            !super::query_params_explain_empty_state("filter="),
            "empty filter value should not be user filter"
        );
        assert!(
            !super::query_params_explain_empty_state("query="),
            "empty query value should not be user filter"
        );
    }

    #[test]
    fn e8_unknown_keys_ignored() {
        assert!(
            !super::query_params_explain_empty_state("raw=20;rendered=0;method=Recent"),
            "unknown keys should not explain empty state"
        );
    }

    // ── E8: ScreenDiagnosticSnapshot invariants ─────────────────────

    #[test]
    fn e8_diagnostic_snapshot_dropped_count_invariant() {
        let snap = ScreenDiagnosticSnapshot {
            screen: "test".into(),
            scope: "test.scope".into(),
            query_params: "filter=all".into(),
            raw_count: 20,
            rendered_count: 15,
            dropped_count: 5,
            timestamp_micros: 1_704_067_200_000_000,
            db_url: "sqlite:///test".into(),
            storage_root: "/tmp".into(),
            transport_mode: "stdio".into(),
            auth_enabled: false,
        };
        assert_eq!(
            snap.dropped_count,
            snap.raw_count.saturating_sub(snap.rendered_count),
            "dropped_count must equal raw_count - rendered_count"
        );
    }

    #[test]
    fn e8_diagnostic_to_log_line_contains_all_fields() {
        let snap = ScreenDiagnosticSnapshot {
            screen: "messages".into(),
            scope: "message_search.results".into(),
            query_params: "filter=urgent".into(),
            raw_count: 10,
            rendered_count: 8,
            dropped_count: 2,
            timestamp_micros: 1_704_067_200_000_000,
            db_url: "sqlite:///test.db".into(),
            storage_root: "/tmp/am".into(),
            transport_mode: "http".into(),
            auth_enabled: true,
        };
        let line = snap.to_log_line();
        assert!(
            line.contains("messages"),
            "log line should contain screen name"
        );
        assert!(
            line.contains("message_search.results"),
            "log line should contain scope"
        );
        assert!(line.contains("raw=10"), "log line should contain raw count");
        assert!(
            line.contains("rendered=8"),
            "log line should contain rendered count"
        );
    }

    // ── E8: ConfigSnapshot context binding invariants ────────────────

    #[test]
    fn e8_config_snapshot_auth_matches_token_presence() {
        let config_with_token = Config {
            http_bearer_token: Some("secret".into()),
            ..Default::default()
        };
        let snap = ConfigSnapshot::from_config(&config_with_token);
        assert!(
            snap.auth_enabled,
            "auth_enabled must be true when bearer token is set"
        );

        let config_without_token = Config {
            http_bearer_token: None,
            ..Default::default()
        };
        let snap = ConfigSnapshot::from_config(&config_without_token);
        assert!(
            !snap.auth_enabled,
            "auth_enabled must be false when bearer token is None"
        );
    }

    #[test]
    fn e8_config_snapshot_sanitizes_db_url() {
        let config = Config {
            database_url: "sqlite:///secret_path?token=abc".into(),
            ..Default::default()
        };
        let snap = ConfigSnapshot::from_config(&config);
        // raw_database_url should have the full URL
        assert_eq!(snap.raw_database_url, config.database_url);
        // sanitized database_url should not expose secrets in common cases
        assert!(!snap.database_url.is_empty());
    }

    #[test]
    fn e8_config_snapshot_transport_mode_derived() {
        let config = Config::default();
        let snap = ConfigSnapshot::from_config(&config);
        let mode = snap.transport_mode();
        // transport_mode is derived from http_path via TransportBase
        assert!(
            matches!(mode, "api" | "mcp" | "custom"),
            "transport_mode should be api, mcp, or custom, got: {mode}"
        );
    }

    #[test]
    fn e8_config_snapshot_normalizes_operator_urls_for_wildcard_and_ipv6_hosts() {
        let wildcard = Config {
            http_host: "0.0.0.0".into(),
            http_port: 8765,
            http_path: "/mcp/".into(),
            ..Default::default()
        };
        let snap = ConfigSnapshot::from_config(&wildcard);
        assert_eq!(snap.endpoint, "http://127.0.0.1:8765/mcp/");
        assert_eq!(snap.web_ui_url, "http://127.0.0.1:8765/mail");

        let ipv6 = Config {
            http_host: "2001:db8::42".into(),
            http_port: 8765,
            http_path: "/api/".into(),
            ..Default::default()
        };
        let snap = ConfigSnapshot::from_config(&ipv6);
        assert_eq!(snap.endpoint, "http://[2001:db8::42]:8765/api/");
        assert_eq!(snap.web_ui_url, "http://[2001:db8::42]:8765/mail");
    }

    // ── E8: assert_screen_truth invariants ───────────────────────────

    #[test]
    fn e8_truth_assertion_allows_both_zero() {
        // raw=0, rendered=0 → OK (genuinely empty data)
        let snap = ScreenDiagnosticSnapshot {
            screen: "test".into(),
            scope: "test".into(),
            query_params: String::new(),
            raw_count: 0,
            rendered_count: 0,
            dropped_count: 0,
            timestamp_micros: 0,
            db_url: String::new(),
            storage_root: String::new(),
            transport_mode: String::new(),
            auth_enabled: false,
        };
        // Should not panic
        super::assert_screen_truth(&snap);
    }

    #[test]
    fn e8_truth_assertion_allows_raw_gt_zero_with_rendered() {
        // raw=10, rendered=10 → OK (data shown)
        let snap = ScreenDiagnosticSnapshot {
            screen: "test".into(),
            scope: "test".into(),
            query_params: String::new(),
            raw_count: 10,
            rendered_count: 10,
            dropped_count: 0,
            timestamp_micros: 0,
            db_url: String::new(),
            storage_root: String::new(),
            transport_mode: String::new(),
            auth_enabled: false,
        };
        super::assert_screen_truth(&snap);
    }

    #[test]
    fn e8_truth_assertion_allows_empty_rendered_with_user_filter() {
        // raw=10, rendered=0, filter=urgent → OK (user filter explains empty)
        let snap = ScreenDiagnosticSnapshot {
            screen: "test".into(),
            scope: "test".into(),
            query_params: "filter=urgent".into(),
            raw_count: 10,
            rendered_count: 0,
            dropped_count: 10,
            timestamp_micros: 0,
            db_url: String::new(),
            storage_root: String::new(),
            transport_mode: String::new(),
            auth_enabled: false,
        };
        super::assert_screen_truth(&snap);
    }

    #[test]
    #[should_panic(expected = "truth_assertion")]
    fn e8_truth_assertion_catches_false_empty() {
        // raw=10, rendered=0, no filter → PANIC (false-empty state)
        let snap = ScreenDiagnosticSnapshot {
            screen: "test".into(),
            scope: "test".into(),
            query_params: "filter=all".into(),
            raw_count: 10,
            rendered_count: 0,
            dropped_count: 10,
            timestamp_micros: 0,
            db_url: String::new(),
            storage_root: String::new(),
            transport_mode: String::new(),
            auth_enabled: false,
        };
        super::assert_screen_truth(&snap);
    }
}
