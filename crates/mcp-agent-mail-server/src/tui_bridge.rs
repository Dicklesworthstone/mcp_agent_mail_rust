#![allow(clippy::module_name_repetitions)]

use crate::console;
use crate::tui_events::{DbStatSnapshot, EventRingBuffer, EventRingStats, MailEvent};
use mcp_agent_mail_core::Config;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REQUEST_SPARKLINE_CAPACITY: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSnapshot {
    pub endpoint: String,
    pub web_ui_url: String,
    pub app_environment: String,
    pub auth_enabled: bool,
    pub database_url: String,
    pub storage_root: String,
    pub console_theme: String,
    pub tool_filter_profile: String,
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
            web_ui_url,
            app_environment: config.app_environment.to_string(),
            auth_enabled: config.http_bearer_token.is_some(),
            database_url,
            storage_root: config.storage_root.display().to_string(),
            console_theme: format!("{:?}", config.console_theme),
            tool_filter_profile: config.tool_filter.profile.clone(),
        }
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
    config_snapshot: ConfigSnapshot,
    db_stats: Mutex<DbStatSnapshot>,
    sparkline_data: Mutex<VecDeque<f64>>,
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
            config_snapshot: ConfigSnapshot::from_config(config),
            db_stats: Mutex::new(DbStatSnapshot::default()),
            sparkline_data: Mutex::new(VecDeque::with_capacity(REQUEST_SPARKLINE_CAPACITY)),
        })
    }

    #[must_use]
    pub fn push_event(&self, event: MailEvent) -> bool {
        self.events.try_push(event).is_some()
    }

    #[must_use]
    pub fn recent_events(&self, limit: usize) -> Vec<MailEvent> {
        self.events.try_iter_recent(limit).unwrap_or_default()
    }

    #[must_use]
    pub fn events_since(&self, seq: u64) -> Vec<MailEvent> {
        self.events.try_events_since_seq(seq).unwrap_or_default()
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

        if let Ok(mut sparkline) = self.sparkline_data.try_lock() {
            if sparkline.len() >= REQUEST_SPARKLINE_CAPACITY {
                let _ = sparkline.pop_front();
            }
            sparkline.push_back(duration_ms as f64);
        }
    }

    pub fn update_db_stats(&self, stats: DbStatSnapshot) {
        if let Ok(mut current) = self.db_stats.try_lock() {
            *current = stats;
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    #[must_use]
    pub fn config_snapshot(&self) -> ConfigSnapshot {
        self.config_snapshot.clone()
    }

    #[must_use]
    pub fn db_stats_snapshot(&self) -> Option<DbStatSnapshot> {
        self.db_stats.try_lock().ok().map(|stats| stats.clone())
    }

    #[must_use]
    pub fn sparkline_snapshot(&self) -> Vec<f64> {
        self.sparkline_data
            .try_lock()
            .ok()
            .map(|sparkline| sparkline.iter().copied().collect())
            .unwrap_or_default()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_events::MailEventKind;
    use std::thread;

    fn config_for_test() -> Config {
        let mut config = Config::default();
        config.database_url = "postgres://alice:supersecret@localhost:5432/mail".to_string();
        config.http_bearer_token = Some("token".to_string());
        config
    }

    #[test]
    fn config_snapshot_masks_database_url() {
        let config = config_for_test();
        let snapshot = ConfigSnapshot::from_config(&config);
        assert!(!snapshot.database_url.contains("supersecret"));
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
}
