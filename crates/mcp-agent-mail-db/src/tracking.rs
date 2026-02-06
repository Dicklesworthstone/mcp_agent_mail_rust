//! Query tracking and instrumentation for MCP Agent Mail.
//!
//! Provides lightweight counters for total queries, per-table breakdowns,
//! and a capped slow-query log. Mirrors the Python `QueryTracker`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Maximum number of slow queries retained in the log.
const SLOW_QUERY_LIMIT: usize = 50;

/// Compiled table extraction patterns (built once, reused).
static TABLE_PATTERNS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r#"(?i)\binsert\s+(?:or\s+\w+\s+)?into\s+([\w.`"\[\]]+)"#).unwrap(),
        Regex::new(r#"(?i)\bupdate\s+([\w.`"\[\]]+)"#).unwrap(),
        Regex::new(r#"(?i)\bfrom\s+([\w.`"\[\]]+)"#).unwrap(),
    ]
});

/// A slow-query entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowQueryEntry {
    pub table: Option<String>,
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
    slow_enabled: AtomicBool,
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
            slow_enabled: AtomicBool::new(true),
            slow_threshold_us: AtomicU64::new(250_000), // 250ms default
            inner: Mutex::new(TrackerInner::default()),
        }
    }

    /// Enable tracking with an optional slow-query threshold (in milliseconds).
    pub fn enable(&self, slow_threshold_ms: Option<u64>) {
        match slow_threshold_ms {
            Some(ms) => {
                self.slow_threshold_us
                    .store(ms.saturating_mul(1000), Ordering::Relaxed);
                self.slow_enabled.store(true, Ordering::Release);
            }
            None => {
                self.slow_enabled.store(false, Ordering::Release);
            }
        }
        self.enabled.store(true, Ordering::Release);
    }

    /// Disable tracking.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
        self.slow_enabled.store(false, Ordering::Release);
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
        if let Some(ref table_name) = table {
            *inner.per_table.entry(table_name.clone()).or_insert(0) += 1;
        }

        // Slow query log
        if self.slow_enabled.load(Ordering::Acquire) {
            let threshold = self.slow_threshold_us.load(Ordering::Relaxed);
            if duration_us >= threshold && inner.slow_queries.len() < SLOW_QUERY_LIMIT {
                inner.slow_queries.push(SlowQueryEntry {
                    table,
                    duration_ms: round_ms(duration_us),
                });
            }
        }
    }

    /// Get a snapshot of current metrics.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn snapshot(&self) -> QueryTrackerSnapshot {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slow_query_ms = if self.slow_enabled.load(Ordering::Acquire) {
            Some(self.slow_threshold_us.load(Ordering::Relaxed) as f64 / 1000.0)
        } else {
            None
        };
        QueryTrackerSnapshot {
            total: self.total.load(Ordering::Relaxed),
            total_time_ms: round_ms(self.total_time_us.load(Ordering::Relaxed)),
            per_table: inner.per_table.clone(),
            slow_query_ms,
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
    pub slow_query_ms: Option<f64>,
    pub slow_queries: Vec<SlowQueryEntry>,
}

impl QueryTrackerSnapshot {
    /// Convert the snapshot into a JSON-friendly dictionary matching legacy output.
    #[must_use]
    pub fn to_dict(&self) -> serde_json::Value {
        let mut pairs: Vec<(&String, &u64)> = self.per_table.iter().collect();
        pairs.sort_by(|(a_name, a_count), (b_name, b_count)| {
            b_count.cmp(a_count).then_with(|| a_name.cmp(b_name))
        });

        let mut per_table = serde_json::Map::new();
        for (name, count) in pairs {
            per_table.insert(name.clone(), serde_json::Value::Number((*count).into()));
        }

        let slow_queries = self
            .slow_queries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "table": entry.table,
                    "duration_ms": entry.duration_ms,
                })
            })
            .collect::<Vec<_>>();

        serde_json::json!({
            "total": self.total,
            "total_time_ms": self.total_time_ms,
            "per_table": per_table,
            "slow_query_ms": self.slow_query_ms,
            "slow_queries": slow_queries,
        })
    }
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

thread_local! {
    static ACTIVE_TRACKER: RefCell<Option<Arc<QueryTracker>>> = const { RefCell::new(None) };
}

/// Guard that restores the previous active tracker on drop.
pub struct ActiveTrackerGuard {
    previous: Option<Arc<QueryTracker>>,
}

impl Drop for ActiveTrackerGuard {
    fn drop(&mut self) {
        ACTIVE_TRACKER.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Set the active query tracker for the current thread.
pub fn set_active_tracker(tracker: Arc<QueryTracker>) -> ActiveTrackerGuard {
    let previous = ACTIVE_TRACKER.with(|slot| slot.borrow_mut().replace(tracker));
    ActiveTrackerGuard { previous }
}

/// Return the active tracker for the current thread, if any.
#[must_use]
pub fn active_tracker() -> Option<Arc<QueryTracker>> {
    ACTIVE_TRACKER.with(|slot| slot.borrow().clone())
}

/// Access the global tracker for enabling/disabling and snapshots.
#[must_use]
pub fn global_tracker() -> &'static QueryTracker {
    &crate::QUERY_TRACKER
}

/// Record a query against the active tracker (or the global fallback).
///
/// Called by `TrackedConnection` / `TrackedTransaction` after each SQL execution.
/// No-op when tracking is disabled.
pub fn record_query(sql: &str, duration_us: u64) {
    if let Some(tracker) = active_tracker() {
        tracker.record(sql, duration_us);
    } else {
        crate::QUERY_TRACKER.record(sql, duration_us);
    }
}

/// Extract the primary table name from a SQL statement.
///
/// Handles schema-qualified names (`public.agents` → `agents`) and
/// various quoting styles (backticks, double-quotes, brackets).
fn extract_table(sql: &str) -> Option<String> {
    /// Compiled pattern to split on schema dots, capturing optional schema segments.
    static SCHEMA_DOT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"[`"\[\]]*\.[`"\[\]]*"#).unwrap());

    for pattern in TABLE_PATTERNS.iter() {
        if let Some(captures) = pattern.captures(sql) {
            if let Some(m) = captures.get(1) {
                let raw = m.as_str();
                // Take last segment after schema dots, then strip quote chars
                let last_segment = SCHEMA_DOT.split(raw).last().unwrap_or(raw);
                let table =
                    last_segment.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']');
                if table.is_empty() {
                    return None;
                }
                return Some(table.to_string());
            }
        }
    }
    None
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

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn round_f64_to_u64(value: f64) -> u64 {
        if value.is_sign_negative() {
            0
        } else {
            value.round() as u64
        }
    }

    #[test]
    fn extract_table_insert() {
        assert_eq!(
            extract_table("INSERT INTO messages (id) VALUES (1)"),
            Some("messages".to_string())
        );
    }

    #[test]
    fn extract_table_update() {
        assert_eq!(
            extract_table("UPDATE agents SET name = 'x' WHERE id = 1"),
            Some("agents".to_string())
        );
    }

    #[test]
    fn extract_table_select() {
        assert_eq!(
            extract_table("SELECT * FROM projects WHERE id = 1"),
            Some("projects".to_string())
        );
    }

    #[test]
    fn extract_table_quoted() {
        assert_eq!(
            extract_table(r#"SELECT * FROM "file_reservations" WHERE 1"#),
            Some("file_reservations".to_string())
        );
    }

    #[test]
    fn extract_table_unknown() {
        assert_eq!(extract_table("PRAGMA wal_checkpoint"), None);
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
        assert_eq!(snap.slow_queries[0].table.as_deref(), Some("agents"));
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

    // ── Fixture-driven table extraction tests ──────────────────────────
    #[test]
    fn fixture_table_extraction() {
        let raw = include_str!(
            "../../mcp-agent-mail-db/tests/fixtures/instrumentation/table_extraction.json"
        );
        let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
        let vectors = doc["vectors"].as_array().unwrap();
        for (i, v) in vectors.iter().enumerate() {
            let sql = v["sql"].as_str().unwrap();
            let expected = v["expected"].as_str().map(String::from);
            let actual = extract_table(sql);
            assert_eq!(
                actual,
                expected,
                "table_extraction vector {i}: {desc}",
                desc = v["desc"].as_str().unwrap_or("?")
            );
        }
    }

    // ── Fixture-driven tracker aggregation tests ───────────────────────
    #[test]
    fn fixture_tracker_aggregation() {
        let raw = include_str!(
            "../../mcp-agent-mail-db/tests/fixtures/instrumentation/tracker_aggregation.json"
        );
        let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
        let vectors = doc["vectors"].as_array().unwrap();
        for (i, v) in vectors.iter().enumerate() {
            let desc = v["desc"].as_str().unwrap_or("?");
            let slow_threshold_ms = if v["slow_query_ms"].is_null() {
                None
            } else {
                Some(round_f64_to_u64(v["slow_query_ms"].as_f64().unwrap()))
            };

            let tracker = QueryTracker::new();
            tracker.enable(slow_threshold_ms);

            let queries = v["queries"].as_array().unwrap();
            for q in queries {
                let sql = q["sql"].as_str().unwrap();
                let duration_ms = q["duration_ms"].as_f64().unwrap();
                // Convert ms to us for the tracker
                let duration_micros = round_f64_to_u64(duration_ms * 1000.0);
                tracker.record(sql, duration_micros);
            }

            let snap = tracker.snapshot();
            let expected = &v["expected"];

            // total
            assert_eq!(
                snap.total,
                expected["total"].as_u64().unwrap(),
                "aggregation vector {i} ({desc}): total mismatch"
            );

            // total_time_ms (compare with tolerance for floating point)
            let expected_time = expected["total_time_ms"].as_f64().unwrap();
            assert!(
                (snap.total_time_ms - expected_time).abs() < 0.02,
                "aggregation vector {i} ({desc}): total_time_ms mismatch: got {}, expected {}",
                snap.total_time_ms,
                expected_time
            );

            // per_table
            let expected_table = expected["per_table"].as_object().unwrap();
            assert_eq!(
                snap.per_table.len(),
                expected_table.len(),
                "aggregation vector {i} ({desc}): per_table length mismatch"
            );
            for (table, count) in expected_table {
                assert_eq!(
                    snap.per_table.get(table),
                    Some(&(count.as_u64().unwrap())),
                    "aggregation vector {i} ({desc}): table {table} count mismatch"
                );
            }

            // slow_query_ms
            if expected["slow_query_ms"].is_null() {
                assert_eq!(
                    snap.slow_query_ms, None,
                    "aggregation vector {i} ({desc}): slow_query_ms should be None"
                );
            } else {
                let expected_sq = expected["slow_query_ms"].as_f64().unwrap();
                assert!(
                    snap.slow_query_ms.is_some(),
                    "aggregation vector {i} ({desc}): slow_query_ms should be Some"
                );
                assert!(
                    (snap.slow_query_ms.unwrap() - expected_sq).abs() < 0.01,
                    "aggregation vector {i} ({desc}): slow_query_ms mismatch"
                );
            }

            // slow_queries
            let expected_slow = expected["slow_queries"].as_array().unwrap();
            assert_eq!(
                snap.slow_queries.len(),
                expected_slow.len(),
                "aggregation vector {i} ({desc}): slow_queries count mismatch"
            );
            for (j, (actual_sq, expected_sq)) in snap
                .slow_queries
                .iter()
                .zip(expected_slow.iter())
                .enumerate()
            {
                let exp_table = expected_sq["table"].as_str().map(String::from);
                assert_eq!(
                    actual_sq.table, exp_table,
                    "aggregation vector {i}.slow[{j}] ({desc}): table mismatch"
                );
                let exp_dur = expected_sq["duration_ms"].as_f64().unwrap();
                assert!(
                    (actual_sq.duration_ms - exp_dur).abs() < 0.02,
                    "aggregation vector {i}.slow[{j}] ({desc}): duration_ms mismatch: got {}, expected {}",
                    actual_sq.duration_ms,
                    exp_dur
                );
            }
        }
    }
}
