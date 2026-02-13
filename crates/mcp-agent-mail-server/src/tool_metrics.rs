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

use mcp_agent_mail_core::Config;
use mcp_agent_mail_tools::{reset_tool_latencies, slow_tools, tool_metrics_snapshot};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Global shutdown flag for the tool metrics worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

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

    info!(
        interval_secs = interval.as_secs(),
        "tool metrics emit worker started"
    );

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("tool metrics emit worker shutting down");
            return;
        }

        // Take a snapshot and emit if non-empty (legacy: only log if snapshot is truthy).
        let snapshot = tool_metrics_snapshot();
        if !snapshot.is_empty() {
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
    }
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
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
}
