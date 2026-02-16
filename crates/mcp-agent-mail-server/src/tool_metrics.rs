//! Background worker for tool metrics emission.
//!
//! Mirrors legacy Python `_worker_tool_metrics` in `http.py`:
//! - Periodically snapshots tool call/error counters and latency histograms
//! - Logs via structlog `tool.metrics` logger with `tool_metrics_snapshot` event
//! - Resets per-tool latency histograms each cycle for rolling-window view
//! - Logs slow tool warnings when any tool's p95 exceeds 500ms
//!
//! The worker runs on a dedicated OS thread with `std::thread::sleep` between
//! iterations, matching the pattern in `cleanup.rs` and `ack_ttl.rs`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::{Config, kpi_record_sample};
use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::pool::DbPoolConfig;
use mcp_agent_mail_db::sqlmodel::Value;
use mcp_agent_mail_db::timestamps::now_micros;
use mcp_agent_mail_tools::{
    MetricsSnapshotEntry, reset_tool_latencies, slow_tools, tool_metrics_snapshot,
};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Global shutdown flag for the tool metrics worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

const TOOL_METRICS_SNAPSHOTS_TABLE: &str = "tool_metrics_snapshots";
const METRICS_RETENTION_DAYS: i64 = 30;
const PRUNE_INTERVAL_TICKS: u64 = 60;

#[derive(Debug, Clone)]
pub struct PersistedToolMetric {
    pub tool_name: String,
    pub calls: u64,
    pub errors: u64,
    pub cluster: String,
    pub complexity: String,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub is_slow: bool,
    pub collected_ts: i64,
}

/// Start the tool metrics emit worker (if enabled).
///
/// Must be called at most once. Subsequent calls are no-ops.
pub fn start(config: &Config) {
    if !config.tool_metrics_emit_enabled {
        return;
    }

    let config = config.clone();
    let _ = WORKER.get_or_init(|| {
        SHUTDOWN.store(false, Ordering::Release);
        std::thread::Builder::new()
            .name("tool-metrics-emit".into())
            .spawn(move || metrics_loop(&config))
            .expect("failed to spawn tool metrics emit worker")
    });
}

/// Signal the worker to stop.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

fn metrics_loop(config: &Config) {
    let interval = std::time::Duration::from_secs(config.tool_metrics_emit_interval_seconds.max(5));
    let mut conn = open_metrics_connection(&config.database_url);
    if let Some(db) = conn.as_ref() {
        ensure_metrics_schema(db);
    }
    let mut tick_index: u64 = 0;

    info!(
        interval_secs = interval.as_secs(),
        "tool metrics emit worker started"
    );

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("tool metrics emit worker shutting down");
            return;
        }

        // Record KPI samples continuously so analytics has baseline data,
        // even during low/no tool-call periods.
        kpi_record_sample();

        // Take a snapshot and emit if non-empty (legacy: only log if snapshot is truthy).
        let snapshot = tool_metrics_snapshot();
        if !snapshot.is_empty() {
            let collected_ts = now_micros();
            // Serialize snapshot to JSON for structured logging.
            // Matches legacy: structlog.get_logger("tool.metrics").info("tool_metrics_snapshot", tools=snapshot)
            match serde_json::to_value(&snapshot) {
                Ok(tools_json) => {
                    info!(
                        target: "tool.metrics",
                        event = "tool_metrics_snapshot",
                        tools = %tools_json,
                        tool_count = snapshot.len(),
                        "tool metrics snapshot"
                    );
                }
                Err(_) => {
                    // Best-effort; never crash.
                    info!(
                        target: "tool.metrics",
                        event = "tool_metrics_snapshot",
                        tool_count = snapshot.len(),
                        "tool metrics snapshot (serialization failed)"
                    );
                }
            }

            if conn.is_none() && tick_index % 12 == 0 {
                conn = open_metrics_connection(&config.database_url);
                if let Some(db) = conn.as_ref() {
                    ensure_metrics_schema(db);
                }
            }
            if let Some(db) = conn.as_ref() {
                if let Err(err) = persist_snapshot_rows(db, collected_ts, &snapshot) {
                    warn!(
                        target: "tool.metrics",
                        error = %err,
                        "failed persisting tool metrics snapshot"
                    );
                } else if tick_index % PRUNE_INTERVAL_TICKS == 0 {
                    prune_old_snapshot_rows(db, collected_ts);
                }
            }

            // Emit slow tool warnings (p95 > 500ms).
            let slow = slow_tools();
            for entry in &slow {
                if let Some(lat) = &entry.latency {
                    warn!(
                        target: "tool.metrics",
                        event = "slow_tool_detected",
                        tool = entry.name.as_str(),
                        p95_ms = lat.p95_ms,
                        p99_ms = lat.p99_ms,
                        avg_ms = lat.avg_ms,
                        calls = entry.calls,
                        "slow tool detected: {} (p95={:.1}ms)",
                        entry.name,
                        lat.p95_ms,
                    );
                }
            }
        }

        // Rolling window: reset per-tool latency histograms so the next
        // snapshot reflects only the most recent interval.
        reset_tool_latencies();

        // Sleep in small increments to allow quick shutdown.
        let mut remaining = interval;
        while !remaining.is_zero() {
            if SHUTDOWN.load(Ordering::Acquire) {
                return;
            }
            let chunk = remaining.min(std::time::Duration::from_secs(1));
            std::thread::sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
        tick_index = tick_index.saturating_add(1);
    }
}

fn open_metrics_connection(database_url: &str) -> Option<DbConn> {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return None;
    }
    let cfg = DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let path = cfg.sqlite_path().ok()?;
    DbConn::open_file(&path).ok()
}

fn ensure_metrics_schema(conn: &DbConn) {
    let _ = conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS tool_metrics_snapshots (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             collected_ts INTEGER NOT NULL, \
             tool_name TEXT NOT NULL, \
             calls INTEGER NOT NULL DEFAULT 0, \
             errors INTEGER NOT NULL DEFAULT 0, \
             cluster TEXT NOT NULL DEFAULT '', \
             capabilities_json TEXT NOT NULL DEFAULT '[]', \
             complexity TEXT NOT NULL DEFAULT 'unknown', \
             latency_avg_ms REAL, \
             latency_min_ms REAL, \
             latency_max_ms REAL, \
             latency_p50_ms REAL, \
             latency_p95_ms REAL, \
             latency_p99_ms REAL, \
             latency_is_slow INTEGER NOT NULL DEFAULT 0\
         )",
        &[],
    );
    let _ = conn.execute_sync(
        "CREATE INDEX IF NOT EXISTS idx_tool_metrics_snapshots_tool_ts \
         ON tool_metrics_snapshots(tool_name, collected_ts DESC)",
        &[],
    );
    let _ = conn.execute_sync(
        "CREATE INDEX IF NOT EXISTS idx_tool_metrics_snapshots_collected_ts \
         ON tool_metrics_snapshots(collected_ts)",
        &[],
    );
}

#[allow(clippy::cast_possible_wrap)]
const fn i64_from_u64_saturating(value: u64) -> i64 {
    if value > i64::MAX as u64 {
        i64::MAX
    } else {
        value as i64
    }
}

fn persist_snapshot_rows(
    conn: &DbConn,
    collected_ts: i64,
    snapshot: &[MetricsSnapshotEntry],
) -> Result<(), String> {
    if snapshot.is_empty() {
        return Ok(());
    }
    let sql = "INSERT INTO tool_metrics_snapshots (\
                 collected_ts, tool_name, calls, errors, cluster, capabilities_json, complexity, \
                 latency_avg_ms, latency_min_ms, latency_max_ms, latency_p50_ms, latency_p95_ms, latency_p99_ms, latency_is_slow\
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

    for entry in snapshot {
        let capabilities_json =
            serde_json::to_string(&entry.capabilities).unwrap_or_else(|_| "[]".to_string());
        let latency = entry.latency.as_ref();
        let params = vec![
            Value::BigInt(collected_ts),
            Value::Text(entry.name.clone()),
            Value::BigInt(i64_from_u64_saturating(entry.calls)),
            Value::BigInt(i64_from_u64_saturating(entry.errors)),
            Value::Text(entry.cluster.clone()),
            Value::Text(capabilities_json),
            Value::Text(entry.complexity.clone()),
            latency.map_or(Value::Null, |lat| Value::Double(lat.avg_ms)),
            latency.map_or(Value::Null, |lat| Value::Double(lat.min_ms)),
            latency.map_or(Value::Null, |lat| Value::Double(lat.max_ms)),
            latency.map_or(Value::Null, |lat| Value::Double(lat.p50_ms)),
            latency.map_or(Value::Null, |lat| Value::Double(lat.p95_ms)),
            latency.map_or(Value::Null, |lat| Value::Double(lat.p99_ms)),
            Value::BigInt(i64::from(latency.is_some_and(|lat| lat.is_slow))),
        ];
        conn.execute_sync(sql, &params).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn prune_old_snapshot_rows(conn: &DbConn, collected_ts: i64) {
    let cutoff = collected_ts.saturating_sub(METRICS_RETENTION_DAYS * 86_400_000_000);
    let _ = conn.execute_sync(
        "DELETE FROM tool_metrics_snapshots WHERE collected_ts < ?",
        &[Value::BigInt(cutoff)],
    );
}

fn decode_metric_row(row: &mcp_agent_mail_db::sqlmodel_core::Row) -> Option<PersistedToolMetric> {
    let tool_name = row.get_named::<String>("tool_name").ok()?;
    let calls = row
        .get_named::<i64>("calls")
        .ok()
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0);
    let errors = row
        .get_named::<i64>("errors")
        .ok()
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0);
    let cluster = row
        .get_named::<String>("cluster")
        .ok()
        .unwrap_or_else(|| "unknown".to_string());
    let complexity = row
        .get_named::<String>("complexity")
        .ok()
        .unwrap_or_else(|| "unknown".to_string());
    let avg_ms = row.get_named::<f64>("latency_avg_ms").ok().unwrap_or(0.0);
    let p50_ms = row.get_named::<f64>("latency_p50_ms").ok().unwrap_or(0.0);
    let p95_ms = row.get_named::<f64>("latency_p95_ms").ok().unwrap_or(0.0);
    let p99_ms = row.get_named::<f64>("latency_p99_ms").ok().unwrap_or(0.0);
    let is_slow = row
        .get_named::<i64>("latency_is_slow")
        .ok()
        .is_some_and(|v| v != 0);
    let collected_ts = row.get_named::<i64>("collected_ts").ok().unwrap_or(0);

    Some(PersistedToolMetric {
        tool_name,
        calls,
        errors,
        cluster,
        complexity,
        avg_ms,
        p50_ms,
        p95_ms,
        p99_ms,
        is_slow,
        collected_ts,
    })
}

#[must_use]
pub fn load_latest_persisted_metrics(database_url: &str, limit: usize) -> Vec<PersistedToolMetric> {
    let Some(conn) = open_metrics_connection(database_url) else {
        return Vec::new();
    };
    ensure_metrics_schema(&conn);

    let limit_i64 = i64::try_from(limit.clamp(1, 2_000)).unwrap_or(200);
    let sql = "SELECT s.tool_name, s.calls, s.errors, s.cluster, s.complexity, \
                      s.latency_avg_ms, s.latency_p50_ms, s.latency_p95_ms, s.latency_p99_ms, \
                      s.latency_is_slow, s.collected_ts \
               FROM tool_metrics_snapshots s \
               JOIN ( \
                    SELECT tool_name, MAX(collected_ts) AS max_ts \
                    FROM tool_metrics_snapshots \
                    GROUP BY tool_name \
               ) latest \
                 ON latest.tool_name = s.tool_name AND latest.max_ts = s.collected_ts \
               ORDER BY s.calls DESC, s.tool_name ASC \
               LIMIT ?";
    conn.query_sync(sql, &[Value::BigInt(limit_i64)])
        .ok()
        .map(|rows| rows.iter().filter_map(decode_metric_row).collect())
        .unwrap_or_default()
}

#[must_use]
pub fn persisted_metric_store_size(database_url: &str) -> u64 {
    let Some(conn) = open_metrics_connection(database_url) else {
        return 0;
    };
    ensure_metrics_schema(&conn);
    conn.query_sync(
        &format!("SELECT COUNT(*) AS c FROM {TOOL_METRICS_SNAPSHOTS_TABLE}"),
        &[],
    )
    .ok()
    .and_then(|rows| rows.into_iter().next())
    .and_then(|row| row.get_named::<i64>("c").ok())
    .and_then(|v| u64::try_from(v).ok())
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_tools::{
        record_call, record_error, record_latency, reset_tool_latencies, reset_tool_metrics,
        slow_tools,
    };

    static METRICS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the test lock, recovering from poison if a previous test panicked.
    fn lock_metrics_test() -> std::sync::MutexGuard<'static, ()> {
        METRICS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn snapshot_structure_and_ordering() {
        // Record some activity.
        record_call("whois");
        record_call("send_message");
        record_call("send_message");
        record_error("send_message");
        record_call("health_check");

        let snapshot = tool_metrics_snapshot();
        assert!(!snapshot.is_empty());

        // Verify alphabetical ordering.
        for window in snapshot.windows(2) {
            assert!(
                window[0].name <= window[1].name,
                "snapshot not sorted: {} > {}",
                window[0].name,
                window[1].name
            );
        }

        // Verify required fields are present.
        for entry in &snapshot {
            assert!(!entry.name.is_empty());
            assert!(!entry.cluster.is_empty());
            assert!(!entry.complexity.is_empty());
        }
    }

    #[test]
    fn snapshot_json_serializable() {
        record_call("fetch_inbox");
        let snapshot = tool_metrics_snapshot();
        let json = serde_json::to_value(&snapshot);
        assert!(json.is_ok(), "snapshot should be JSON-serializable");

        let arr = json.unwrap();
        assert!(arr.is_array());
        if let Some(first) = arr.as_array().and_then(|a| a.first()) {
            assert!(first.get("name").is_some());
            assert!(first.get("calls").is_some());
            assert!(first.get("errors").is_some());
            assert!(first.get("cluster").is_some());
            assert!(first.get("capabilities").is_some());
            assert!(first.get("complexity").is_some());
        }
    }

    #[test]
    fn worker_disabled_by_default() {
        let config = Config::from_env();
        // Default config has tool_metrics_emit_enabled = false.
        // start() should be a no-op.
        assert!(!config.tool_metrics_emit_enabled);
    }

    #[test]
    fn snapshot_includes_latest_latency_bucket() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        record_call("send_message");
        record_latency("send_message", 800_000); // 800ms

        let snapshot = tool_metrics_snapshot();
        let entry = snapshot
            .iter()
            .find(|e| e.name == "send_message")
            .expect("send_message present in snapshot");
        let latency = entry.latency.as_ref().expect("latency should be captured");
        assert!(latency.is_slow);
        assert!(latency.p95_ms >= 500.0, "p95 should cross slow threshold");
    }

    #[test]
    fn reset_clears_latency_histograms_between_snapshots() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        record_call("send_message");
        record_latency("send_message", 200_000);
        let before = tool_metrics_snapshot();
        let before_entry = before
            .iter()
            .find(|e| e.name == "send_message")
            .expect("send_message present");
        assert!(before_entry.latency.is_some());

        reset_tool_latencies();

        let after = tool_metrics_snapshot();
        let after_entry = after
            .iter()
            .find(|e| e.name == "send_message")
            .expect("send_message present after reset");
        assert_eq!(
            after_entry.calls, 1,
            "call counters should remain after latency reset"
        );
        assert!(
            after_entry.latency.is_none(),
            "latency histogram should be cleared while call count remains"
        );
    }

    #[test]
    fn slow_tools_only_reports_tools_above_threshold() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        record_call("health_check");
        record_latency("health_check", 200_000); // 200ms

        record_call("send_message");
        record_latency("send_message", 800_000); // 800ms

        let slow = slow_tools();
        assert!(
            slow.iter().any(|e| e.name == "send_message"),
            "send_message should be flagged as slow"
        );
        assert!(
            !slow.iter().any(|e| e.name == "health_check"),
            "health_check should not be flagged as slow"
        );
    }

    #[test]
    fn empty_snapshot_after_full_reset() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        // No calls recorded → snapshot should be empty.
        let snapshot = tool_metrics_snapshot();
        assert!(
            snapshot.is_empty(),
            "snapshot should be empty after full reset with no new calls"
        );
    }

    #[test]
    fn call_then_error_both_tracked() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        record_call("fetch_inbox");
        record_error("fetch_inbox");

        let snapshot = tool_metrics_snapshot();
        let entry = snapshot
            .iter()
            .find(|e| e.name == "fetch_inbox")
            .expect("tool should appear in snapshot after call+error");
        assert!(entry.calls >= 1, "call count should be positive");
        assert!(entry.errors >= 1, "error count should be positive");
    }

    #[test]
    fn multiple_tools_all_present_and_sorted() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        let tools = ["whois", "send_message", "health_check", "fetch_inbox"];
        for name in &tools {
            record_call(name);
            record_latency(name, 100_000);
        }

        let snapshot = tool_metrics_snapshot();
        // All tools should be present.
        for name in &tools {
            assert!(
                snapshot.iter().any(|e| e.name == *name),
                "{name} should appear in snapshot"
            );
        }
        // Sorted alphabetically.
        for window in snapshot.windows(2) {
            assert!(
                window[0].name <= window[1].name,
                "not sorted: {} > {}",
                window[0].name,
                window[1].name
            );
        }
    }

    #[test]
    fn slow_tools_empty_when_all_fast() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        record_call("health_check");
        record_latency("health_check", 50_000); // 50ms — well under 500ms threshold

        let slow = slow_tools();
        assert!(
            !slow.iter().any(|e| e.name == "health_check"),
            "50ms tool should not be flagged slow"
        );
    }

    #[test]
    fn concurrent_record_calls_accumulate_counts() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        let threads = 8usize;
        let per_thread = 25usize;
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    for i in 0..per_thread {
                        record_call("health_check");
                        if i % 5 == 0 {
                            record_error("health_check");
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread join");
        }

        let snapshot = tool_metrics_snapshot();
        let entry = snapshot
            .iter()
            .find(|e| e.name == "health_check")
            .expect("health_check present");
        let expected_calls = u64::try_from(threads * per_thread).unwrap();
        let expected_errors = u64::try_from(threads * 5).unwrap();
        // Use >= instead of == because parallel tests in other crates may
        // also record health_check calls with their own locks. The key
        // invariant is that our concurrent calls all accumulate.
        assert!(
            entry.calls >= expected_calls,
            "expected at least {expected_calls} calls, got {}",
            entry.calls
        );
        assert!(
            entry.errors >= expected_errors,
            "expected at least {expected_errors} errors, got {}",
            entry.errors
        );
    }

    #[test]
    fn persists_and_loads_latest_snapshots() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tool_metrics_snapshots.db");
        let database_url = format!("sqlite://{}", db_path.display());
        let conn = open_metrics_connection(&database_url).expect("open metrics sqlite");
        ensure_metrics_schema(&conn);

        record_call("send_message");
        record_call("send_message");
        record_error("send_message");
        record_latency("send_message", 800_000);

        let snapshot = tool_metrics_snapshot();
        persist_snapshot_rows(&conn, now_micros(), &snapshot).expect("persist snapshot");

        let rows = load_latest_persisted_metrics(&database_url, 50);
        let row = rows
            .iter()
            .find(|r| r.tool_name == "send_message")
            .expect("send_message persisted");
        assert!(row.calls >= 2);
        assert!(row.errors >= 1);
        assert!(row.p95_ms >= 500.0);
        assert!(row.collected_ts > 0);
    }

    #[test]
    fn persisted_store_size_counts_rows() {
        let _guard = lock_metrics_test();
        reset_tool_metrics();

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tool_metrics_store_size.db");
        let database_url = format!("sqlite://{}", db_path.display());
        let conn = open_metrics_connection(&database_url).expect("open metrics sqlite");
        ensure_metrics_schema(&conn);

        record_call("health_check");
        record_latency("health_check", 10_000);
        let snapshot = tool_metrics_snapshot();
        persist_snapshot_rows(&conn, now_micros(), &snapshot).expect("persist snapshot");

        let count = persisted_metric_store_size(&database_url);
        assert!(count >= 1);
    }
}
