//! Background worker for tool metrics emission.
//!
//! Mirrors legacy Python `_worker_tool_metrics` in `http.py`:
//! - Periodically snapshots tool call/error counters
//! - Logs via structlog `tool.metrics` logger with `tool_metrics_snapshot` event
//!
//! The worker runs on a dedicated OS thread with `std::thread::sleep` between
//! iterations, matching the pattern in `cleanup.rs` and `ack_ttl.rs`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use mcp_agent_mail_tools::tool_metrics_snapshot;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

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
        }

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
    use mcp_agent_mail_tools::{record_call, record_error};

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
}
