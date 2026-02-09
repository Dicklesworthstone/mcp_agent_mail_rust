//! Global tool metrics tracking.
//!
//! Mirrors legacy Python `TOOL_METRICS` defaultdict:
//! - Thread-safe atomic counters for calls/errors per tool
//! - Per-tool latency histograms with streaming P50/P95/P99 (br-15dv.8.4)
//! - `tool_metrics_snapshot()` returns sorted snapshot with metadata + latency
//!
//! Call `record_call(tool_name)` / `record_error(tool_name)` from tool handlers.
//! Call `record_latency_idx(tool_index, latency_us)` from `InstrumentedTool`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Log2Histogram;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::TOOL_CLUSTER_MAP;

const TOOL_COUNT: usize = TOOL_CLUSTER_MAP.len();

/// Threshold in microseconds: tools with p95 above this are flagged as slow.
const SLOW_TOOL_P95_THRESHOLD_US: u64 = 500_000; // 500ms

static TOOL_CALLS: LazyLock<[AtomicU64; TOOL_COUNT]> =
    LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static TOOL_ERRORS: LazyLock<[AtomicU64; TOOL_COUNT]> =
    LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static TOOL_LATENCIES: LazyLock<[Log2Histogram; TOOL_COUNT]> =
    LazyLock::new(|| std::array::from_fn(|_| Log2Histogram::new()));

/// Convert tool name -> stable index into the pre-allocated counter arrays.
///
/// The index corresponds to the tool's position in `TOOL_CLUSTER_MAP`.
#[must_use]
pub fn tool_index(tool_name: &str) -> Option<usize> {
    TOOL_CLUSTER_MAP
        .iter()
        .position(|(name, _cluster)| *name == tool_name)
}

#[inline]
pub fn record_call_idx(tool_index: usize) {
    debug_assert!(tool_index < TOOL_COUNT);
    TOOL_CALLS[tool_index].fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn record_error_idx(tool_index: usize) {
    debug_assert!(tool_index < TOOL_COUNT);
    TOOL_ERRORS[tool_index].fetch_add(1, Ordering::Relaxed);
}

/// Record a successful tool call.
pub fn record_call(tool_name: &str) {
    if let Some(idx) = tool_index(tool_name) {
        record_call_idx(idx);
    } else {
        debug_assert!(
            false,
            "record_call called with unknown tool name: {tool_name}"
        );
    }
}

/// Record a tool error.
pub fn record_error(tool_name: &str) {
    if let Some(idx) = tool_index(tool_name) {
        record_error_idx(idx);
    } else {
        debug_assert!(
            false,
            "record_error called with unknown tool name: {tool_name}"
        );
    }
}

/// Record per-tool latency in microseconds (called from `InstrumentedTool`).
#[inline]
pub fn record_latency_idx(tool_index: usize, latency_us: u64) {
    debug_assert!(tool_index < TOOL_COUNT);
    TOOL_LATENCIES[tool_index].record(latency_us);
}

/// Record per-tool latency by name (convenience wrapper).
pub fn record_latency(tool_name: &str, latency_us: u64) {
    if let Some(idx) = tool_index(tool_name) {
        record_latency_idx(idx, latency_us);
    }
}

/// Clear all tool metrics counters (calls, errors, and latency histograms).
///
/// Intended for tests that need deterministic snapshots across multiple tool calls.
pub fn reset_tool_metrics() {
    for c in TOOL_CALLS.iter() {
        c.store(0, Ordering::Relaxed);
    }
    for e in TOOL_ERRORS.iter() {
        e.store(0, Ordering::Relaxed);
    }
    for h in TOOL_LATENCIES.iter() {
        h.reset();
    }
}

/// Reset only the per-tool latency histograms (rolling window support).
///
/// Called periodically by the tool metrics emit worker to provide a rolling
/// window view of latency rather than cumulative all-time stats.
pub fn reset_tool_latencies() {
    for h in TOOL_LATENCIES.iter() {
        h.reset();
    }
}

/// Static metadata for each tool (capabilities, complexity).
///
/// Mirrors legacy Python `TOOL_METADATA` and `_instrument_tool` decorator kwargs.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub capabilities: &'static [&'static str],
    pub complexity: &'static str,
}

/// Tool metadata registry keyed by tool name.
///
/// Matches the hardcoded data from legacy Python `_instrument_tool` decorators.
pub const TOOL_META_MAP: &[(&str, ToolMeta)] = &[
    // Infrastructure
    (
        "health_check",
        ToolMeta {
            capabilities: &["infrastructure"],
            complexity: "low",
        },
    ),
    (
        "ensure_project",
        ToolMeta {
            capabilities: &["infrastructure", "storage"],
            complexity: "low",
        },
    ),
    (
        "install_precommit_guard",
        ToolMeta {
            capabilities: &["infrastructure", "repository"],
            complexity: "medium",
        },
    ),
    (
        "uninstall_precommit_guard",
        ToolMeta {
            capabilities: &["infrastructure", "repository"],
            complexity: "medium",
        },
    ),
    // Identity
    (
        "register_agent",
        ToolMeta {
            capabilities: &["identity"],
            complexity: "medium",
        },
    ),
    (
        "create_agent_identity",
        ToolMeta {
            capabilities: &["identity"],
            complexity: "medium",
        },
    ),
    (
        "whois",
        ToolMeta {
            capabilities: &["audit", "identity"],
            complexity: "medium",
        },
    ),
    // Messaging
    (
        "send_message",
        ToolMeta {
            capabilities: &["messaging", "write"],
            complexity: "medium",
        },
    ),
    (
        "reply_message",
        ToolMeta {
            capabilities: &["messaging", "write"],
            complexity: "medium",
        },
    ),
    (
        "fetch_inbox",
        ToolMeta {
            capabilities: &["messaging", "read"],
            complexity: "medium",
        },
    ),
    (
        "mark_message_read",
        ToolMeta {
            capabilities: &["messaging", "read"],
            complexity: "medium",
        },
    ),
    (
        "acknowledge_message",
        ToolMeta {
            capabilities: &["ack", "messaging"],
            complexity: "medium",
        },
    ),
    // Contact
    (
        "request_contact",
        ToolMeta {
            capabilities: &["contact"],
            complexity: "medium",
        },
    ),
    (
        "respond_contact",
        ToolMeta {
            capabilities: &["contact"],
            complexity: "medium",
        },
    ),
    (
        "list_contacts",
        ToolMeta {
            capabilities: &["audit", "contact"],
            complexity: "medium",
        },
    ),
    (
        "set_contact_policy",
        ToolMeta {
            capabilities: &["configure", "contact"],
            complexity: "medium",
        },
    ),
    // File reservations
    (
        "file_reservation_paths",
        ToolMeta {
            capabilities: &["file_reservations", "repository"],
            complexity: "medium",
        },
    ),
    (
        "release_file_reservations",
        ToolMeta {
            capabilities: &["file_reservations"],
            complexity: "medium",
        },
    ),
    (
        "renew_file_reservations",
        ToolMeta {
            capabilities: &["file_reservations"],
            complexity: "medium",
        },
    ),
    (
        "force_release_file_reservation",
        ToolMeta {
            capabilities: &["file_reservations", "repository"],
            complexity: "medium",
        },
    ),
    // Search
    (
        "search_messages",
        ToolMeta {
            capabilities: &["search"],
            complexity: "medium",
        },
    ),
    (
        "summarize_thread",
        ToolMeta {
            capabilities: &["search", "summarization"],
            complexity: "medium",
        },
    ),
    // Workflow macros
    (
        "macro_start_session",
        ToolMeta {
            capabilities: &["file_reservations", "identity", "messaging", "workflow"],
            complexity: "medium",
        },
    ),
    (
        "macro_prepare_thread",
        ToolMeta {
            capabilities: &["messaging", "summarization", "workflow"],
            complexity: "medium",
        },
    ),
    (
        "macro_file_reservation_cycle",
        ToolMeta {
            capabilities: &["file_reservations", "repository", "workflow"],
            complexity: "medium",
        },
    ),
    (
        "macro_contact_handshake",
        ToolMeta {
            capabilities: &["contact", "messaging", "workflow"],
            complexity: "medium",
        },
    ),
    // Product bus
    (
        "ensure_product",
        ToolMeta {
            capabilities: &["product"],
            complexity: "medium",
        },
    ),
    (
        "products_link",
        ToolMeta {
            capabilities: &["product"],
            complexity: "medium",
        },
    ),
    (
        "search_messages_product",
        ToolMeta {
            capabilities: &["search"],
            complexity: "medium",
        },
    ),
    (
        "fetch_inbox_product",
        ToolMeta {
            capabilities: &["messaging", "read"],
            complexity: "medium",
        },
    ),
    (
        "summarize_thread_product",
        ToolMeta {
            capabilities: &["search", "summarization"],
            complexity: "medium",
        },
    ),
    // Build slots
    (
        "acquire_build_slot",
        ToolMeta {
            capabilities: &["build"],
            complexity: "medium",
        },
    ),
    (
        "renew_build_slot",
        ToolMeta {
            capabilities: &["build"],
            complexity: "medium",
        },
    ),
    (
        "release_build_slot",
        ToolMeta {
            capabilities: &["build"],
            complexity: "medium",
        },
    ),
];

/// Look up static metadata for a tool.
#[must_use]
pub fn tool_meta(tool_name: &str) -> Option<&'static ToolMeta> {
    TOOL_META_MAP
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, meta)| meta)
}

/// Per-tool latency statistics in a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySnapshot {
    /// Average latency in milliseconds.
    pub avg_ms: f64,
    /// Minimum observed latency in milliseconds.
    pub min_ms: f64,
    /// Maximum observed latency in milliseconds.
    pub max_ms: f64,
    /// 50th percentile latency in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: f64,
    /// 99th percentile latency in milliseconds.
    pub p99_ms: f64,
    /// True if p95 exceeds the slow-tool threshold (500ms).
    pub is_slow: bool,
}

/// A single entry in a metrics snapshot.
///
/// Includes call/error counters, cluster metadata, and per-tool latency
/// histogram statistics (P50/P95/P99).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshotEntry {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    pub cluster: String,
    pub capabilities: Vec<String>,
    pub complexity: String,
    /// Per-tool latency statistics. `None` if no latency has been recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencySnapshot>,
}

/// Convert microseconds to milliseconds as f64.
#[inline]
#[allow(clippy::cast_precision_loss)] // microsecond values fit comfortably in f64
fn us_to_ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// Build a `LatencySnapshot` from a tool's histogram, or `None` if no data.
fn latency_snapshot_for(idx: usize) -> Option<LatencySnapshot> {
    let hs = TOOL_LATENCIES[idx].snapshot();
    if hs.count == 0 {
        return None;
    }
    let avg_us = hs.sum.checked_div(hs.count).unwrap_or(0);
    Some(LatencySnapshot {
        avg_ms: us_to_ms(avg_us),
        min_ms: us_to_ms(hs.min),
        max_ms: us_to_ms(hs.max),
        p50_ms: us_to_ms(hs.p50),
        p95_ms: us_to_ms(hs.p95),
        p99_ms: us_to_ms(hs.p99),
        is_slow: hs.p95 > SLOW_TOOL_P95_THRESHOLD_US,
    })
}

/// Produce a sorted metrics snapshot.
///
/// Returns all tools that have been called (calls > 0), sorted alphabetically
/// by name, enriched with cluster, capabilities, complexity, and per-tool
/// latency histogram statistics (P50/P95/P99).
#[must_use]
pub fn tool_metrics_snapshot() -> Vec<MetricsSnapshotEntry> {
    let mut entries: Vec<MetricsSnapshotEntry> = TOOL_CLUSTER_MAP
        .iter()
        .enumerate()
        .filter_map(|(idx, (name, cluster))| {
            let calls = TOOL_CALLS[idx].load(Ordering::Relaxed);
            if calls == 0 {
                return None;
            }

            let errors = TOOL_ERRORS[idx].load(Ordering::Relaxed);
            let meta = tool_meta(name);
            Some(MetricsSnapshotEntry {
                name: (*name).to_string(),
                calls,
                errors,
                cluster: (*cluster).to_string(),
                capabilities: meta
                    .map(|m| m.capabilities.iter().map(|s| (*s).to_string()).collect())
                    .unwrap_or_default(),
                complexity: meta.map_or("unknown", |m| m.complexity).to_string(),
                latency: latency_snapshot_for(idx),
            })
        })
        .collect();

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Return a snapshot including all known tools (even those with zero calls).
///
/// Used by the tooling metrics resource to always show the full catalogue.
#[must_use]
pub fn tool_metrics_snapshot_full() -> Vec<MetricsSnapshotEntry> {
    let mut entries: Vec<MetricsSnapshotEntry> = TOOL_CLUSTER_MAP
        .iter()
        .enumerate()
        .map(|(idx, (name, cluster))| {
            let meta = tool_meta(name);
            MetricsSnapshotEntry {
                name: (*name).to_string(),
                calls: TOOL_CALLS[idx].load(Ordering::Relaxed),
                errors: TOOL_ERRORS[idx].load(Ordering::Relaxed),
                cluster: (*cluster).to_string(),
                capabilities: meta
                    .map(|m| m.capabilities.iter().map(|s| (*s).to_string()).collect())
                    .unwrap_or_default(),
                complexity: meta.map_or("unknown", |m| m.complexity).to_string(),
                latency: latency_snapshot_for(idx),
            }
        })
        .collect();

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Return only tools flagged as slow (p95 > 500ms).
///
/// Useful for alerting and diagnostic reports.
#[must_use]
pub fn slow_tools() -> Vec<MetricsSnapshotEntry> {
    tool_metrics_snapshot()
        .into_iter()
        .filter(|e| e.latency.as_ref().is_some_and(|l| l.is_slow))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that reset global metrics and assert exact counts must be
    /// serialized to prevent parallel tests from polluting each other.
    static METRICS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn record_and_snapshot() {
        // Record some calls.
        record_call("health_check");
        record_call("health_check");
        record_call("send_message");
        record_error("send_message");

        let snapshot = tool_metrics_snapshot();
        assert!(!snapshot.is_empty());

        // Snapshot should be sorted alphabetically.
        for window in snapshot.windows(2) {
            assert!(window[0].name <= window[1].name, "not sorted");
        }

        // Find health_check.
        let hc = snapshot.iter().find(|e| e.name == "health_check");
        assert!(hc.is_some());
        let hc = hc.unwrap();
        assert!(hc.calls >= 2);
        assert_eq!(hc.cluster, "infrastructure");
        assert_eq!(hc.complexity, "low");

        // Find send_message.
        let sm = snapshot.iter().find(|e| e.name == "send_message");
        assert!(sm.is_some());
        let sm = sm.unwrap();
        assert!(sm.calls >= 1);
        assert!(sm.errors >= 1);
        assert_eq!(sm.cluster, "messaging");
    }

    #[test]
    fn snapshot_full_includes_all_tools() {
        let full = tool_metrics_snapshot_full();
        // Should include all tools from TOOL_CLUSTER_MAP.
        assert_eq!(full.len(), TOOL_CLUSTER_MAP.len());

        // Sorted alphabetically.
        for window in full.windows(2) {
            assert!(window[0].name <= window[1].name, "not sorted");
        }
    }

    #[test]
    fn tool_meta_lookup() {
        let meta = tool_meta("health_check");
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.complexity, "low");
        assert!(meta.capabilities.contains(&"infrastructure"));

        // Unknown tool returns None.
        assert!(tool_meta("nonexistent_tool").is_none());
    }

    #[test]
    fn snapshot_entry_metadata_matches() {
        record_call("ensure_project");
        let snapshot = tool_metrics_snapshot();
        let ep = snapshot.iter().find(|e| e.name == "ensure_project");
        assert!(ep.is_some());
        let ep = ep.unwrap();
        assert_eq!(ep.cluster, "infrastructure");
        assert_eq!(ep.complexity, "low");
        assert!(ep.capabilities.contains(&"infrastructure".to_string()));
        assert!(ep.capabilities.contains(&"storage".to_string()));
    }

    #[test]
    fn latency_tracking_basic() {
        // Reset and record under lock to prevent parallel test interference.
        // Note: external tests (macros.rs) also call record_call() so we use
        // >= for call counts while latency stats are deterministic after reset.
        let _guard = METRICS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_tool_metrics();
        let idx = tool_index("health_check").unwrap();

        // Record calls with latency.
        record_call_idx(idx);
        record_latency_idx(idx, 1_000); // 1ms
        record_call_idx(idx);
        record_latency_idx(idx, 2_000); // 2ms
        record_call_idx(idx);
        record_latency_idx(idx, 3_000); // 3ms

        let snapshot = tool_metrics_snapshot();
        let hc = snapshot.iter().find(|e| e.name == "health_check").unwrap();
        assert!(hc.calls >= 3, "expected >= 3 calls, got {}", hc.calls);

        let lat = hc.latency.as_ref().expect("latency should be present");
        assert!(
            lat.min_ms >= 0.5 && lat.min_ms <= 1.5,
            "min_ms={}",
            lat.min_ms
        );
        assert!(
            lat.max_ms >= 2.5 && lat.max_ms <= 4.0,
            "max_ms={}",
            lat.max_ms
        );
        assert!(!lat.is_slow, "3ms p95 should not be flagged as slow");
    }

    #[test]
    fn latency_no_data_returns_none() {
        let _guard = METRICS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_tool_metrics();
        // Record a call without latency.
        record_call("whois");

        let snapshot = tool_metrics_snapshot();
        let w = snapshot.iter().find(|e| e.name == "whois").unwrap();
        assert!(w.latency.is_none(), "no latency recorded, should be None");
    }

    #[test]
    fn slow_tool_detection() {
        let _guard = METRICS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_tool_metrics();
        let idx = tool_index("send_message").unwrap();

        // Record a mix of fast and slow calls.
        for _ in 0..20 {
            record_call_idx(idx);
            record_latency_idx(idx, 600_000); // 600ms — above 500ms threshold
        }

        let snapshot = tool_metrics_snapshot();
        let sm = snapshot.iter().find(|e| e.name == "send_message").unwrap();
        let lat = sm.latency.as_ref().unwrap();
        assert!(lat.is_slow, "p95 at 600ms should be flagged as slow");
        assert!(lat.p95_ms >= 400.0, "p95_ms should be high: {}", lat.p95_ms);

        let slow = slow_tools();
        assert!(
            slow.iter().any(|e| e.name == "send_message"),
            "send_message should appear in slow_tools()"
        );
    }

    #[test]
    fn reset_clears_latency_histograms() {
        let _guard = METRICS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_tool_metrics();
        let idx = tool_index("fetch_inbox").unwrap();
        record_call_idx(idx);
        record_latency_idx(idx, 5_000);

        // Verify latency is present.
        let snap1 = tool_metrics_snapshot();
        let fi = snap1.iter().find(|e| e.name == "fetch_inbox").unwrap();
        assert!(fi.latency.is_some());

        // Reset only latencies.
        reset_tool_latencies();

        // Calls should still be present but latency gone.
        let snap2 = tool_metrics_snapshot();
        let fi2 = snap2.iter().find(|e| e.name == "fetch_inbox").unwrap();
        assert!(fi2.calls >= 1, "expected >= 1 call, got {}", fi2.calls);
        assert!(
            fi2.latency.is_none(),
            "latency should be cleared after reset"
        );
    }

    #[test]
    fn latency_snapshot_json_serializable() {
        let _guard = METRICS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_tool_metrics();
        let idx = tool_index("register_agent").unwrap();
        record_call_idx(idx);
        record_latency_idx(idx, 10_000);

        let snapshot = tool_metrics_snapshot();
        let json = serde_json::to_value(&snapshot).expect("should serialize");
        let arr = json.as_array().unwrap();
        let entry = arr.iter().find(|v| v["name"] == "register_agent").unwrap();
        assert!(entry.get("latency").is_some());
        let lat = &entry["latency"];
        assert!(lat.get("avg_ms").is_some());
        assert!(lat.get("p50_ms").is_some());
        assert!(lat.get("p95_ms").is_some());
        assert!(lat.get("p99_ms").is_some());
        assert!(lat.get("is_slow").is_some());
    }
}
