//! Query tracking and instrumentation for MCP Agent Mail.
//!
//! Provides lightweight counters for total queries, per-table breakdowns,
//! and a capped slow-query log. Mirrors the Python `QueryTracker`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Maximum number of slow queries retained in the log.
const SLOW_QUERY_LIMIT: usize = 50;

/// Compiled table extraction patterns (built once, reused).
static TABLE_PATTERNS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r#"(?i)\binsert\s+into\s+([\w.`"\[\]]+)"#).unwrap(),
        Regex::new(r#"(?i)\bupdate\s+([\w.`"\[\]]+)"#).unwrap(),
        Regex::new(r#"(?i)\bfrom\s+([\w.`"\[\]]+)"#).unwrap(),
    ]
});

/// A slow-query entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowQueryEntry {
    pub table: String,
    pub duration_ms: f64,
}

/// Lightweight query tracker matching the Python `QueryTracker`.
///
/// Thread-safe via atomics for counters and a mutex for per-table/slow maps.
#[derive(Debug)]
pub struct QueryTracker {
    enabled: AtomicBool,
    total: AtomicU64,
    total_time_us: AtomicU64,
    slow_threshold_us: AtomicU64,
    inner: Mutex<TrackerInner>,
}

#[derive(Debug, Default)]
struct TrackerInner {
    per_table: std::collections::HashMap<String, u64>,
    slow_queries: Vec<SlowQueryEntry>,
}

impl Default for QueryTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryTracker {
    /// Create a disabled tracker (no overhead until `enable()` is called).
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            total: AtomicU64::new(0),
            total_time_us: AtomicU64::new(0),
            slow_threshold_us: AtomicU64::new(250_000), // 250ms default
            inner: Mutex::new(TrackerInner::default()),
        }
    }

    /// Enable tracking with an optional slow-query threshold (in milliseconds).
    pub fn enable(&self, slow_threshold_ms: Option<u64>) {
        if let Some(ms) = slow_threshold_ms {
            self.slow_threshold_us
                .store(ms.saturating_mul(1000), Ordering::Relaxed);
        }
        self.enabled.store(true, Ordering::Release);
    }

    /// Disable tracking.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    /// Whether tracking is currently enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Record a completed query. Call this after each SQL execution.
    pub fn record(&self, sql: &str, duration_us: u64) {
        if !self.is_enabled() {
            return;
        }

        self.total.fetch_add(1, Ordering::Relaxed);
        self.total_time_us.fetch_add(duration_us, Ordering::Relaxed);

        let table = extract_table(sql);

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Per-table count
        *inner.per_table.entry(table.clone()).or_insert(0) += 1;

        // Slow query log
        let threshold = self.slow_threshold_us.load(Ordering::Relaxed);
        if duration_us >= threshold && inner.slow_queries.len() < SLOW_QUERY_LIMIT {
            inner.slow_queries.push(SlowQueryEntry {
                table,
                duration_ms: round_ms(duration_us),
            });
        }
    }

    /// Get a snapshot of current metrics.
    #[must_use]
    pub fn snapshot(&self) -> QueryTrackerSnapshot {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        QueryTrackerSnapshot {
            total: self.total.load(Ordering::Relaxed),
            total_time_ms: round_ms(self.total_time_us.load(Ordering::Relaxed)),
            per_table: inner.per_table.clone(),
            slow_queries: inner.slow_queries.clone(),
        }
    }

    /// Reset all counters and logs.
    pub fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.total_time_us.store(0, Ordering::Relaxed);
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.per_table.clear();
        inner.slow_queries.clear();
    }
}

/// Immutable snapshot of tracker state, suitable for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryTrackerSnapshot {
    pub total: u64,
    pub total_time_ms: f64,
    pub per_table: std::collections::HashMap<String, u64>,
    pub slow_queries: Vec<SlowQueryEntry>,
}

/// Start a timer for query instrumentation.
/// Returns an `Instant` that should be passed to [`elapsed_us`].
#[must_use]
pub fn query_timer() -> Instant {
    Instant::now()
}

/// Compute elapsed microseconds since the timer was started.
#[must_use]
pub fn elapsed_us(start: Instant) -> u64 {
    let micros = start.elapsed().as_micros().min(u128::from(u64::MAX));
    u64::try_from(micros).unwrap_or(u64::MAX)
}

/// Extract the primary table name from a SQL statement.
fn extract_table(sql: &str) -> String {
    for pattern in TABLE_PATTERNS.iter() {
        if let Some(captures) = pattern.captures(sql) {
            if let Some(m) = captures.get(1) {
                let raw = m.as_str();
                // Strip quoting characters
                return raw
                    .trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']')
                    .to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Round microseconds to milliseconds with 2 decimal places.
#[allow(clippy::cast_precision_loss)]
fn round_ms(us: u64) -> f64 {
    let ms = us as f64 / 1000.0;
    (ms * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_table_insert() {
        assert_eq!(
            extract_table("INSERT INTO messages (id) VALUES (1)"),
            "messages"
        );
    }

    #[test]
    fn extract_table_update() {
        assert_eq!(
            extract_table("UPDATE agents SET name = 'x' WHERE id = 1"),
            "agents"
        );
    }

    #[test]
    fn extract_table_select() {
        assert_eq!(
            extract_table("SELECT * FROM projects WHERE id = 1"),
            "projects"
        );
    }

    #[test]
    fn extract_table_quoted() {
        assert_eq!(
            extract_table(r#"SELECT * FROM "file_reservations" WHERE 1"#),
            "file_reservations"
        );
    }

    #[test]
    fn extract_table_unknown() {
        assert_eq!(extract_table("PRAGMA wal_checkpoint"), "unknown");
    }

    #[test]
    fn tracker_disabled_by_default() {
        let tracker = QueryTracker::new();
        assert!(!tracker.is_enabled());
        tracker.record("SELECT 1 FROM projects", 100);
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 0);
    }

    #[test]
    fn tracker_records_when_enabled() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(100)); // 100ms threshold
        tracker.record("SELECT * FROM messages WHERE id = 1", 50_000); // 50ms
        tracker.record("INSERT INTO agents (name) VALUES ('x')", 200_000); // 200ms (slow)
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 2);
        assert_eq!(snap.per_table.get("messages"), Some(&1));
        assert_eq!(snap.per_table.get("agents"), Some(&1));
        // 200ms >= 100ms threshold → slow
        assert_eq!(snap.slow_queries.len(), 1);
        assert_eq!(snap.slow_queries[0].table, "agents");
    }

    #[test]
    fn tracker_reset() {
        let tracker = QueryTracker::new();
        tracker.enable(None);
        tracker.record("SELECT 1 FROM projects", 100);
        tracker.reset();
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 0);
        assert!(snap.per_table.is_empty());
    }

    #[test]
    fn slow_query_cap() {
        let tracker = QueryTracker::new();
        tracker.enable(Some(0)); // 0ms threshold = everything is slow
        for i in 0..60 {
            tracker.record(&format!("SELECT {i} FROM messages"), 1000);
        }
        let snap = tracker.snapshot();
        assert_eq!(snap.total, 60);
        assert_eq!(snap.slow_queries.len(), SLOW_QUERY_LIMIT);
    }
}
