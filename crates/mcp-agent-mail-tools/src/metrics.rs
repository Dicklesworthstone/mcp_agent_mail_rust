//! Global tool metrics tracking.
//!
//! Mirrors legacy Python `TOOL_METRICS` defaultdict:
//! - Thread-safe atomic counters for calls/errors per tool
//! - `tool_metrics_snapshot()` returns sorted snapshot with metadata
//!
//! Call `record_call(tool_name)` / `record_error(tool_name)` from tool handlers.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::TOOL_CLUSTER_MAP;

const TOOL_COUNT: usize = TOOL_CLUSTER_MAP.len();

static TOOL_CALLS: LazyLock<[AtomicU64; TOOL_COUNT]> =
    LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static TOOL_ERRORS: LazyLock<[AtomicU64; TOOL_COUNT]> =
    LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));

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

/// Clear all tool metrics counters.
///
/// Intended for tests that need deterministic snapshots across multiple tool calls.
pub fn reset_tool_metrics() {
    for c in TOOL_CALLS.iter() {
        c.store(0, Ordering::Relaxed);
    }
    for e in TOOL_ERRORS.iter() {
        e.store(0, Ordering::Relaxed);
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

/// A single entry in a metrics snapshot.
///
/// Matches legacy Python `_tool_metrics_snapshot()` dict structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshotEntry {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    pub cluster: String,
    pub capabilities: Vec<String>,
    pub complexity: String,
}

/// Produce a sorted metrics snapshot.
///
/// Mirrors legacy Python `_tool_metrics_snapshot()`:
/// - Returns all tools that have been called (calls > 0)
/// - Sorted alphabetically by name
/// - Enriched with cluster, capabilities, complexity from metadata
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
            }
        })
        .collect();

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
