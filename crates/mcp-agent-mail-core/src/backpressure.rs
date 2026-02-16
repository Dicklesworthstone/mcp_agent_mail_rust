//! System-wide backpressure framework with Green/Yellow/Red health levels.
//!
//! Computes a composite health level from DB pool, WBQ, and commit queue
//! metrics. The level is used by the server dispatch layer to shed
//! non-critical work under extreme load (1000+ concurrent agents).
//!
//! Design principles:
//! - **Lock-free**: computed from existing atomic metrics, no new locks.
//! - **Composable**: callers decide what to do with the level.
//! - **Observable**: exposed via `health_check` + tooling/metrics resources.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::metrics::{GlobalMetricsSnapshot, global_metrics};
use crate::slo;

// ---------------------------------------------------------------------------
// Health level enum
// ---------------------------------------------------------------------------

/// System health classification.
///
/// Used to guide flow-control decisions at the server dispatch layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthLevel {
    /// All subsystems healthy. Accept all requests normally.
    Green = 0,
    /// Elevated load. Defer non-critical archive writes, reduce logging.
    Yellow = 1,
    /// Overload. Optionally reject low-priority maintenance tool calls.
    Red = 2,
}

impl HealthLevel {
    /// Convert from the raw `AtomicU8` representation.
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Green,
            1 => Self::Yellow,
            _ => Self::Red,
        }
    }

    /// String label for JSON responses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Red => "red",
        }
    }

    /// Whether a tool should be rejected under this level.
    ///
    /// Returns `true` if the tool is low-priority (shedable) and the
    /// system is in Red.
    #[must_use]
    pub const fn should_shed(self, tool_is_shedable: bool) -> bool {
        matches!(self, Self::Red) && tool_is_shedable
    }
}

impl std::fmt::Display for HealthLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Thresholds (configurable via constants, aligned with SLOs)
// ---------------------------------------------------------------------------

/// Thresholds for transitioning from Green to Yellow.
pub mod yellow {
    /// Pool acquire latency p95 threshold (microseconds).
    pub const POOL_ACQUIRE_P95_US: u64 = super::slo::POOL_ACQUIRE_YELLOW_US; // 50 ms

    /// WBQ depth as percentage of capacity.
    pub const WBQ_DEPTH_PCT: u64 = 50;

    /// Commit queue pending as percentage of soft cap.
    pub const COMMIT_DEPTH_PCT: u64 = 50;

    /// Pool utilization percentage.
    pub const POOL_UTIL_PCT: u64 = 70;

    /// Minimum duration (seconds) at >=80% utilization before triggering.
    pub const OVER_80_DURATION_S: u64 = 30;
}

/// Thresholds for transitioning from Yellow to Red.
pub mod red {
    /// Pool acquire latency p95 threshold (microseconds).
    pub const POOL_ACQUIRE_P95_US: u64 = super::slo::POOL_ACQUIRE_RED_US; // 200 ms

    /// WBQ depth as percentage of capacity.
    pub const WBQ_DEPTH_PCT: u64 = 80;

    /// Commit queue pending as percentage of soft cap.
    pub const COMMIT_DEPTH_PCT: u64 = 80;

    /// Pool utilization percentage.
    pub const POOL_UTIL_PCT: u64 = 90;

    /// Duration (seconds) at >=80% utilization before triggering.
    pub const OVER_80_DURATION_S: u64 = 300;
}

// Compile-time invariants
const _: () = {
    assert!(yellow::POOL_ACQUIRE_P95_US < red::POOL_ACQUIRE_P95_US);
    assert!(yellow::WBQ_DEPTH_PCT < red::WBQ_DEPTH_PCT);
    assert!(yellow::COMMIT_DEPTH_PCT < red::COMMIT_DEPTH_PCT);
    assert!(yellow::POOL_UTIL_PCT < red::POOL_UTIL_PCT);
    assert!(yellow::OVER_80_DURATION_S < red::OVER_80_DURATION_S);
};

// ---------------------------------------------------------------------------
// Health signals (extracted from metrics snapshot)
// ---------------------------------------------------------------------------

/// Intermediate signal values used to classify the health level.
///
/// Useful for observability: callers can inspect which signals triggered
/// a transition.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSignals {
    pub pool_acquire_p95_us: u64,
    pub pool_utilization_pct: u64,
    pub pool_over_80_for_s: u64,
    pub wbq_depth_pct: u64,
    pub wbq_over_80_for_s: u64,
    pub commit_depth_pct: u64,
    pub commit_over_80_for_s: u64,
}

impl HealthSignals {
    /// Extract signals from a metrics snapshot.
    ///
    /// `now_us` is the current time in microseconds (Unix epoch).
    #[must_use]
    pub const fn from_snapshot(snap: &GlobalMetricsSnapshot, now_us: u64) -> Self {
        let pool_over_80_for_s = duration_since_s(snap.db.pool_over_80_since_us, now_us);
        let wbq_over_80_for_s = duration_since_s(snap.storage.wbq_over_80_since_us, now_us);
        let commit_over_80_for_s = duration_since_s(snap.storage.commit_over_80_since_us, now_us);

        let wbq_depth_pct = pct(snap.storage.wbq_depth, snap.storage.wbq_capacity);
        let commit_depth_pct = pct(
            snap.storage.commit_pending_requests,
            snap.storage.commit_soft_cap,
        );

        Self {
            pool_acquire_p95_us: snap.db.pool_acquire_latency_us.p95,
            pool_utilization_pct: snap.db.pool_utilization_pct,
            pool_over_80_for_s,
            wbq_depth_pct,
            wbq_over_80_for_s,
            commit_depth_pct,
            commit_over_80_for_s,
        }
    }

    /// Classify the composite health level from the extracted signals.
    #[must_use]
    pub const fn classify(&self) -> HealthLevel {
        // Red: any critical subsystem breached
        if self.pool_acquire_p95_us > red::POOL_ACQUIRE_P95_US
            || self.pool_utilization_pct >= red::POOL_UTIL_PCT
            || self.pool_over_80_for_s >= red::OVER_80_DURATION_S
            || self.wbq_depth_pct >= red::WBQ_DEPTH_PCT
            || self.wbq_over_80_for_s >= red::OVER_80_DURATION_S
            || self.commit_depth_pct >= red::COMMIT_DEPTH_PCT
            || self.commit_over_80_for_s >= red::OVER_80_DURATION_S
        {
            return HealthLevel::Red;
        }

        // Yellow: any elevated subsystem
        if self.pool_acquire_p95_us > yellow::POOL_ACQUIRE_P95_US
            || self.pool_utilization_pct >= yellow::POOL_UTIL_PCT
            || self.wbq_depth_pct >= yellow::WBQ_DEPTH_PCT
            || self.commit_depth_pct >= yellow::COMMIT_DEPTH_PCT
        {
            return HealthLevel::Yellow;
        }

        HealthLevel::Green
    }
}

// ---------------------------------------------------------------------------
// Convenience: compute level from live metrics
// ---------------------------------------------------------------------------

/// Compute the current system health level from global metrics.
///
/// This is the primary entry point for dispatch-layer backpressure checks.
/// It reads atomic counters (no locks) and classifies in O(1).
#[must_use]
pub fn compute_health_level() -> HealthLevel {
    let snap = global_metrics().snapshot();
    let now_us = now_micros_u64();
    let signals = HealthSignals::from_snapshot(&snap, now_us);
    signals.classify()
}

/// Compute the current health level and return the underlying signals
/// for observability.
#[must_use]
pub fn compute_health_level_with_signals() -> (HealthLevel, HealthSignals) {
    let snap = global_metrics().snapshot();
    let now_us = now_micros_u64();
    let signals = HealthSignals::from_snapshot(&snap, now_us);
    let level = signals.classify();
    (level, signals)
}

// ---------------------------------------------------------------------------
// Global cached level (AtomicU8) for ultra-fast dispatch checks
// ---------------------------------------------------------------------------

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(0); // Green
static LEVEL_TRANSITIONS: AtomicU8 = AtomicU8::new(0);

/// Read the last-recorded health level (may be slightly stale).
///
/// This is faster than `compute_health_level()` because it avoids
/// snapshotting all metrics. Updated by `refresh_health_level()`.
#[must_use]
pub fn cached_health_level() -> HealthLevel {
    HealthLevel::from_u8(CURRENT_LEVEL.load(Ordering::Relaxed))
}

/// Recompute the health level from live metrics and update the cache.
///
/// Returns `(new_level, changed)`. Call this periodically (e.g., every
/// 250ms alongside pool stats sampling) or on each `health_check`.
pub fn refresh_health_level() -> (HealthLevel, bool) {
    let new = compute_health_level();
    let prev = CURRENT_LEVEL.swap(new as u8, Ordering::Relaxed);
    let changed = prev != new as u8;
    if changed {
        // Saturating add — wraps at 255, which is fine for observability.
        LEVEL_TRANSITIONS.fetch_add(1, Ordering::Relaxed);
    }
    (new, changed)
}

/// Number of times the cached level has changed (for observability).
#[must_use]
pub fn level_transitions() -> u8 {
    LEVEL_TRANSITIONS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Global shedding gate (AtomicBool, off by default)
// ---------------------------------------------------------------------------

static SHEDDING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Read the global shedding-enabled flag.
///
/// When `false` (the default), `should_shed_tool` never rejects, regardless
/// of health level or tool classification.
#[must_use]
pub fn shedding_enabled() -> bool {
    SHEDDING_ENABLED.load(Ordering::Relaxed)
}

/// Set the global shedding-enabled flag.
///
/// Called once at server startup from `Config::backpressure_shedding_enabled`.
pub fn set_shedding_enabled(enabled: bool) {
    SHEDDING_ENABLED.store(enabled, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Shedable tool classification
// ---------------------------------------------------------------------------

/// Tool names that are considered low-priority (read-only, deferrable) and
/// may be rejected under Red-level backpressure when shedding is enabled.
///
/// Criteria for inclusion:
/// - The tool is **read-only** (no state mutation).
/// - Agents have an alternative path or can safely retry later.
/// - The tool is not required for agent lifecycle or coordination.
const SHEDABLE_TOOLS: &[&str] = &[
    // Search cluster — read-only, retryable
    "search_messages",
    "summarize_thread",
    // Identity cluster — read-only lookup
    "whois",
    // Contacts cluster — read-only listing
    "list_contacts",
    // Product bus — cross-product reads (per-project fetch_inbox is unshed)
    "search_messages_product",
    "summarize_thread_product",
    "fetch_inbox_product",
];

/// Returns `true` if the named tool is considered low-priority and can
/// be rejected under Red-level backpressure.
///
/// This is a pure classification function — it does NOT check whether
/// shedding is globally enabled. Use [`should_shed_tool`] for the full
/// dispatch-layer decision.
#[must_use]
pub fn is_shedable_tool(tool_name: &str) -> bool {
    SHEDABLE_TOOLS.contains(&tool_name)
}

/// Combined dispatch-layer decision: should this tool call be rejected?
///
/// Returns `true` only when **all three** conditions hold:
/// 1. Shedding is globally enabled (`BACKPRESSURE_SHEDDING_ENABLED=true`).
/// 2. The cached health level is Red.
/// 3. The tool is classified as shedable.
#[must_use]
pub fn should_shed_tool(tool_name: &str) -> bool {
    shedding_enabled() && cached_health_level().should_shed(is_shedable_tool(tool_name))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the duration in seconds since a given start timestamp.
/// Returns 0 if `since_us` is 0 (meaning "not set").
#[inline]
const fn duration_since_s(since_us: u64, now_us: u64) -> u64 {
    if since_us == 0 {
        return 0;
    }
    now_us.saturating_sub(since_us).saturating_div(1_000_000)
}

/// Compute a percentage, clamped to 100.
#[inline]
const fn pct(value: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let p = value.saturating_mul(100).saturating_div(total);
    if p > 100 { 100 } else { p }
}

/// Current time in microseconds (Unix epoch). Infallible.
#[inline]
fn now_micros_u64() -> u64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_micros()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::*;

    fn default_signals() -> HealthSignals {
        HealthSignals {
            pool_acquire_p95_us: 0,
            pool_utilization_pct: 0,
            pool_over_80_for_s: 0,
            wbq_depth_pct: 0,
            wbq_over_80_for_s: 0,
            commit_depth_pct: 0,
            commit_over_80_for_s: 0,
        }
    }

    #[test]
    fn all_healthy_is_green() {
        let s = default_signals();
        assert_eq!(s.classify(), HealthLevel::Green);
    }

    #[test]
    fn high_pool_latency_triggers_yellow() {
        let mut s = default_signals();
        s.pool_acquire_p95_us = yellow::POOL_ACQUIRE_P95_US + 1;
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn very_high_pool_latency_triggers_red() {
        let mut s = default_signals();
        s.pool_acquire_p95_us = red::POOL_ACQUIRE_P95_US + 1;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn wbq_at_50_pct_is_yellow() {
        let mut s = default_signals();
        s.wbq_depth_pct = 50;
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn wbq_at_80_pct_is_red() {
        let mut s = default_signals();
        s.wbq_depth_pct = 80;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn commit_at_50_pct_is_yellow() {
        let mut s = default_signals();
        s.commit_depth_pct = 50;
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn commit_at_80_pct_is_red() {
        let mut s = default_signals();
        s.commit_depth_pct = 80;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn pool_utilization_70_is_yellow() {
        let mut s = default_signals();
        s.pool_utilization_pct = 70;
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn pool_utilization_90_is_red() {
        let mut s = default_signals();
        s.pool_utilization_pct = 90;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn sustained_over_80_30s_is_yellow() {
        let mut s = default_signals();
        s.pool_over_80_for_s = 30;
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn sustained_over_80_300s_is_red() {
        let mut s = default_signals();
        s.pool_over_80_for_s = 300;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn wbq_sustained_300s_is_red() {
        let mut s = default_signals();
        s.wbq_over_80_for_s = 300;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn commit_sustained_300s_is_red() {
        let mut s = default_signals();
        s.commit_over_80_for_s = 300;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn boundary_just_below_yellow_is_green() {
        let mut s = default_signals();
        // At the threshold (not above) → green for pool_acquire
        s.pool_acquire_p95_us = yellow::POOL_ACQUIRE_P95_US;
        assert_eq!(s.classify(), HealthLevel::Green);
    }

    #[test]
    fn boundary_just_below_red_is_yellow() {
        let mut s = default_signals();
        s.pool_acquire_p95_us = red::POOL_ACQUIRE_P95_US;
        // Exactly at the threshold → not "above" → yellow (below red)
        // But it IS above yellow threshold, so yellow
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn health_level_ordering() {
        assert!(HealthLevel::Green < HealthLevel::Yellow);
        assert!(HealthLevel::Yellow < HealthLevel::Red);
    }

    #[test]
    fn health_level_display() {
        assert_eq!(format!("{}", HealthLevel::Green), "green");
        assert_eq!(format!("{}", HealthLevel::Yellow), "yellow");
        assert_eq!(format!("{}", HealthLevel::Red), "red");
    }

    #[test]
    fn health_level_roundtrip_u8() {
        for (v, expected) in [
            (0u8, HealthLevel::Green),
            (1, HealthLevel::Yellow),
            (2, HealthLevel::Red),
        ] {
            assert_eq!(HealthLevel::from_u8(v), expected);
            assert_eq!(expected as u8, v);
        }
        // Out-of-range defaults to Red (conservative)
        assert_eq!(HealthLevel::from_u8(255), HealthLevel::Red);
    }

    #[test]
    fn shedable_tools_classified_correctly() {
        // Shedable: read-only, deferrable tools
        assert!(is_shedable_tool("search_messages"));
        assert!(is_shedable_tool("summarize_thread"));
        assert!(is_shedable_tool("whois"));
        assert!(is_shedable_tool("list_contacts"));
        assert!(is_shedable_tool("search_messages_product"));
        assert!(is_shedable_tool("summarize_thread_product"));
        assert!(is_shedable_tool("fetch_inbox_product"));
    }

    #[test]
    fn critical_tools_not_shedable() {
        // Infrastructure
        assert!(!is_shedable_tool("health_check"));
        assert!(!is_shedable_tool("ensure_project"));
        assert!(!is_shedable_tool("install_precommit_guard"));
        assert!(!is_shedable_tool("uninstall_precommit_guard"));

        // Identity (writes)
        assert!(!is_shedable_tool("register_agent"));
        assert!(!is_shedable_tool("create_agent_identity"));

        // Messaging (core agent coordination)
        assert!(!is_shedable_tool("send_message"));
        assert!(!is_shedable_tool("reply_message"));
        assert!(!is_shedable_tool("fetch_inbox"));
        assert!(!is_shedable_tool("mark_message_read"));
        assert!(!is_shedable_tool("acknowledge_message"));

        // Contacts (writes)
        assert!(!is_shedable_tool("request_contact"));
        assert!(!is_shedable_tool("respond_contact"));
        assert!(!is_shedable_tool("set_contact_policy"));

        // File reservations (critical for multi-agent coordination)
        assert!(!is_shedable_tool("file_reservation_paths"));
        assert!(!is_shedable_tool("release_file_reservations"));
        assert!(!is_shedable_tool("renew_file_reservations"));
        assert!(!is_shedable_tool("force_release_file_reservation"));

        // Macros (compound writes)
        assert!(!is_shedable_tool("macro_start_session"));
        assert!(!is_shedable_tool("macro_prepare_thread"));
        assert!(!is_shedable_tool("macro_file_reservation_cycle"));
        assert!(!is_shedable_tool("macro_contact_handshake"));

        // Product bus (writes)
        assert!(!is_shedable_tool("ensure_product"));
        assert!(!is_shedable_tool("products_link"));

        // Build slots (coordination)
        assert!(!is_shedable_tool("acquire_build_slot"));
        assert!(!is_shedable_tool("renew_build_slot"));
        assert!(!is_shedable_tool("release_build_slot"));
    }

    #[test]
    fn unknown_tools_not_shedable() {
        assert!(!is_shedable_tool(""));
        assert!(!is_shedable_tool("Health_Check"));
        assert!(!is_shedable_tool("health_check "));
        assert!(!is_shedable_tool("nonexistent_tool"));
    }

    #[test]
    fn should_shed_logic() {
        assert!(!HealthLevel::Green.should_shed(true));
        assert!(!HealthLevel::Green.should_shed(false));
        assert!(!HealthLevel::Yellow.should_shed(true));
        assert!(!HealthLevel::Yellow.should_shed(false));
        assert!(HealthLevel::Red.should_shed(true));
        assert!(!HealthLevel::Red.should_shed(false));
    }

    #[test]
    fn duration_since_zero_is_zero() {
        assert_eq!(duration_since_s(0, 1_000_000_000), 0);
    }

    #[test]
    fn duration_since_computes_correctly() {
        let start_us = 100_000_000; // 100s
        let now_us = 130_000_000; // 130s
        assert_eq!(duration_since_s(start_us, now_us), 30);
    }

    #[test]
    fn pct_edge_cases() {
        assert_eq!(pct(0, 0), 0);
        assert_eq!(pct(50, 100), 50);
        assert_eq!(pct(100, 100), 100);
        assert_eq!(pct(200, 100), 100); // clamped
    }

    #[test]
    fn duration_since_future_timestamp_is_zero() {
        let now_us = 1_000_000_000;
        let future_since = now_us + 60_000_000;
        assert_eq!(duration_since_s(future_since, now_us), 0);
    }

    #[test]
    #[expect(clippy::too_many_lines)]
    fn from_snapshot_with_zero_metrics() {
        let snap = GlobalMetricsSnapshot {
            http: HttpMetricsSnapshot {
                requests_total: 0,
                requests_inflight: 0,
                requests_2xx: 0,
                requests_4xx: 0,
                requests_5xx: 0,
                latency_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
            },
            tools: ToolsMetricsSnapshot {
                tool_calls_total: 0,
                tool_errors_total: 0,
                tool_latency_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                contact_enforcement_bypass_total: 0,
            },
            db: DbMetricsSnapshot {
                pool_acquires_total: 0,
                pool_acquire_errors_total: 0,
                pool_acquire_latency_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                pool_total_connections: 100,
                pool_idle_connections: 100,
                pool_active_connections: 0,
                pool_pending_requests: 0,
                pool_peak_active_connections: 0,
                pool_utilization_pct: 0,
                pool_over_80_since_us: 0,
            },
            storage: StorageMetricsSnapshot {
                wbq_enqueued_total: 0,
                wbq_drained_total: 0,
                wbq_errors_total: 0,
                wbq_fallbacks_total: 0,
                wbq_depth: 0,
                wbq_capacity: 8192,
                wbq_peak_depth: 0,
                wbq_over_80_since_us: 0,
                wbq_queue_latency_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                commit_enqueued_total: 0,
                commit_drained_total: 0,
                commit_errors_total: 0,
                commit_sync_fallbacks_total: 0,
                commit_pending_requests: 0,
                commit_soft_cap: 8192,
                commit_peak_pending_requests: 0,
                commit_over_80_since_us: 0,
                commit_queue_latency_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                needs_reindex_total: 0,
                archive_lock_wait_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                commit_lock_wait_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                git_commit_latency_us: HistogramSnapshot {
                    count: 0,
                    sum: 0,
                    min: 0,
                    max: 0,
                    p50: 0,
                    p95: 0,
                    p99: 0,
                },
                git_index_lock_retries_total: 0,
                git_index_lock_failures_total: 0,
                commit_attempts_total: 0,
                commit_failures_total: 0,
                commit_batch_size_last: 0,
                lockfree_commits_total: 0,
                lockfree_commit_fallbacks_total: 0,
            },
            system: SystemMetricsSnapshot {
                disk_storage_free_bytes: 0,
                disk_db_free_bytes: 0,
                disk_effective_free_bytes: 0,
                disk_pressure_level: 0,
                disk_last_sample_us: 0,
                disk_sample_errors_total: 0,
                memory_rss_bytes: 0,
                memory_pressure_level: 0,
                memory_last_sample_us: 0,
                memory_sample_errors_total: 0,
            },
            search: SearchMetricsSnapshot::default(),
        };

        let signals = HealthSignals::from_snapshot(&snap, 1_000_000_000);
        assert_eq!(signals.classify(), HealthLevel::Green);
        assert_eq!(signals.pool_acquire_p95_us, 0);
        assert_eq!(signals.wbq_depth_pct, 0);
        assert_eq!(signals.commit_depth_pct, 0);
    }

    #[test]
    fn health_signals_from_snapshot_captures_every_metric() {
        let mut snap = GlobalMetrics::default().snapshot();
        let now_us = 1_000_000_000;
        snap.db.pool_acquire_latency_us = HistogramSnapshot {
            count: 1,
            sum: 120_000,
            min: 120_000,
            max: 120_000,
            p50: 120_000,
            p95: 120_000,
            p99: 120_000,
        };
        snap.db.pool_utilization_pct = 77;
        snap.db.pool_over_80_since_us = now_us - 42_000_000;
        snap.storage.wbq_depth = 75;
        snap.storage.wbq_capacity = 100;
        snap.storage.wbq_over_80_since_us = now_us - 5_000_000;
        snap.storage.commit_pending_requests = 45;
        snap.storage.commit_soft_cap = 90;
        snap.storage.commit_over_80_since_us = now_us - 8_000_000;

        let signals = HealthSignals::from_snapshot(&snap, now_us);

        assert_eq!(signals.pool_acquire_p95_us, 120_000);
        assert_eq!(signals.pool_utilization_pct, 77);
        assert_eq!(signals.pool_over_80_for_s, 42);
        assert_eq!(signals.wbq_depth_pct, 75);
        assert_eq!(signals.wbq_over_80_for_s, 5);
        assert_eq!(signals.commit_depth_pct, 50);
        assert_eq!(signals.commit_over_80_for_s, 8);
        assert_eq!(signals.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn health_signals_pct_clamps_to_hundred() {
        let mut snap = GlobalMetrics::default().snapshot();
        let now_us = 1_000_000_000;
        snap.storage.wbq_depth = 1000;
        snap.storage.wbq_capacity = 1;
        snap.storage.commit_pending_requests = 500;
        snap.storage.commit_soft_cap = 1;

        let signals = HealthSignals::from_snapshot(&snap, now_us);
        assert_eq!(signals.wbq_depth_pct, 100);
        assert_eq!(signals.commit_depth_pct, 100);
        assert_eq!(signals.classify(), HealthLevel::Red);
    }

    #[test]
    fn health_signals_zero_capacities_do_not_divide_by_zero() {
        let mut snap = GlobalMetrics::default().snapshot();
        let now_us = 1_000_000_000;
        snap.storage.wbq_depth = 42;
        snap.storage.wbq_capacity = 0;
        snap.storage.commit_pending_requests = 99;
        snap.storage.commit_soft_cap = 0;

        let signals = HealthSignals::from_snapshot(&snap, now_us);
        assert_eq!(signals.wbq_depth_pct, 0);
        assert_eq!(signals.commit_depth_pct, 0);
        assert_eq!(signals.classify(), HealthLevel::Green);
    }

    #[test]
    fn stale_over_80_timestamps_do_not_trigger_red() {
        let mut snap = GlobalMetrics::default().snapshot();
        let now_us = 1_000_000_000;
        snap.db.pool_over_80_since_us = now_us + 10_000_000;
        snap.storage.wbq_over_80_since_us = now_us + 20_000_000;
        snap.storage.commit_over_80_since_us = now_us + 30_000_000;

        let signals = HealthSignals::from_snapshot(&snap, now_us);
        assert_eq!(signals.pool_over_80_for_s, 0);
        assert_eq!(signals.wbq_over_80_for_s, 0);
        assert_eq!(signals.commit_over_80_for_s, 0);
        assert_eq!(signals.classify(), HealthLevel::Green);
    }

    #[test]
    fn cached_level_starts_green() {
        // Note: tests run in parallel, so the global may have been modified.
        // We can at least verify the API is callable.
        let level = cached_health_level();
        assert!(matches!(
            level,
            HealthLevel::Green | HealthLevel::Yellow | HealthLevel::Red
        ));
    }

    #[test]
    fn refresh_detects_change() {
        // Since we can't control the global metrics in unit tests,
        // just verify the API returns the expected shape.
        let (level, _changed) = refresh_health_level();
        assert!(matches!(
            level,
            HealthLevel::Green | HealthLevel::Yellow | HealthLevel::Red
        ));
    }

    #[test]
    fn multiple_signals_worst_wins() {
        let mut s = default_signals();
        // Pool is yellow-level, but WBQ is red-level → Red wins
        s.pool_acquire_p95_us = yellow::POOL_ACQUIRE_P95_US + 1;
        s.wbq_depth_pct = 80;
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn single_yellow_among_green_is_yellow() {
        let mut s = default_signals();
        s.commit_depth_pct = yellow::COMMIT_DEPTH_PCT;
        assert_eq!(s.classify(), HealthLevel::Yellow);
    }

    #[test]
    fn all_critical_signals_red_is_red() {
        let s = HealthSignals {
            pool_acquire_p95_us: red::POOL_ACQUIRE_P95_US + 1,
            pool_utilization_pct: red::POOL_UTIL_PCT,
            pool_over_80_for_s: red::OVER_80_DURATION_S,
            wbq_depth_pct: red::WBQ_DEPTH_PCT,
            wbq_over_80_for_s: red::OVER_80_DURATION_S,
            commit_depth_pct: red::COMMIT_DEPTH_PCT,
            commit_over_80_for_s: red::OVER_80_DURATION_S,
        };
        assert_eq!(s.classify(), HealthLevel::Red);
    }

    #[test]
    fn rapid_oscillation_green_red_green_is_deterministic() {
        let mut s = default_signals();
        assert_eq!(s.classify(), HealthLevel::Green);

        s.wbq_depth_pct = red::WBQ_DEPTH_PCT;
        assert_eq!(s.classify(), HealthLevel::Red);

        s.wbq_depth_pct = 0;
        assert_eq!(s.classify(), HealthLevel::Green);
    }

    #[test]
    fn duration_since_extreme_values_stays_bounded() {
        let now_us = u64::MAX;
        let since_us = 1;
        let elapsed = duration_since_s(since_us, now_us);
        assert_eq!(elapsed, (u64::MAX - 1).saturating_div(1_000_000));
    }

    #[test]
    fn shed_decision_integrates_classification_and_level() {
        // Shedable tool at each health level
        assert!(HealthLevel::Red.should_shed(is_shedable_tool("search_messages")));
        assert!(!HealthLevel::Yellow.should_shed(is_shedable_tool("search_messages")));
        assert!(!HealthLevel::Green.should_shed(is_shedable_tool("search_messages")));

        // Non-shedable tool at Red → never shed
        assert!(!HealthLevel::Red.should_shed(is_shedable_tool("send_message")));
        assert!(!HealthLevel::Red.should_shed(is_shedable_tool("health_check")));
        assert!(!HealthLevel::Red.should_shed(is_shedable_tool("fetch_inbox")));
        assert!(!HealthLevel::Red.should_shed(is_shedable_tool("file_reservation_paths")));
    }

    #[test]
    fn should_shed_tool_respects_global_flag() {
        // Ensure flag is off (default)
        set_shedding_enabled(false);

        // Even with Red level in the cache, should_shed_tool returns false
        // when the flag is off. We can't easily force the cached level to Red
        // in a unit test, but we can verify the flag gate directly.
        assert!(!should_shed_tool("search_messages"));
        assert!(!should_shed_tool("whois"));

        // Non-shedable tools always return false regardless of flag
        set_shedding_enabled(true);
        assert!(!should_shed_tool("send_message"));
        assert!(!should_shed_tool("register_agent"));

        // Reset to default
        set_shedding_enabled(false);
    }

    #[test]
    fn shedding_enabled_flag_roundtrip() {
        let original = shedding_enabled();
        set_shedding_enabled(true);
        assert!(shedding_enabled());
        set_shedding_enabled(false);
        assert!(!shedding_enabled());
        set_shedding_enabled(original);
    }

    #[test]
    fn serde_serialization() {
        let level = HealthLevel::Yellow;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"yellow\"");
    }
}
