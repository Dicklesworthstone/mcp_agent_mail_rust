//! Structured diagnostic report combining all system health metrics.
//!
//! Provides a comprehensive snapshot for operators debugging issues with
//! 1000+ concurrent agents. Includes system info, database, storage,
//! tools, lock contention, health level, and automated recommendations.
//!
//! # Usage
//!
//! ```rust,ignore
//! let report = DiagnosticReport::build(tool_snapshot, slow_tools);
//! let json = serde_json::to_string_pretty(&report).unwrap();
//! ```

#![forbid(unsafe_code)]

use serde::Serialize;

use crate::backpressure::{self, HealthLevel, HealthSignals};
use crate::lock_order::{LockContentionEntry, lock_contention_snapshot};
use crate::metrics::{
    DbMetricsSnapshot, GlobalMetricsSnapshot, HttpMetricsSnapshot, SearchMetricsSnapshot,
    StorageMetricsSnapshot, SystemMetricsSnapshot, ToolsMetricsSnapshot, global_metrics,
};

/// Maximum serialized report size in bytes (100KB).
const MAX_REPORT_BYTES: usize = 100 * 1024;

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// Top-level diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    /// Report generation timestamp (ISO-8601).
    pub generated_at: String,
    /// System information (uptime, Rust version, OS, CPU count).
    pub system: SystemInfo,
    /// Health level assessment.
    pub health: HealthInfo,
    /// HTTP request metrics.
    pub http: HttpMetricsSnapshot,
    /// Aggregate tool call metrics.
    pub tools_aggregate: ToolsMetricsSnapshot,
    /// Per-tool call/error/latency snapshots (passed in from tools crate).
    pub tools_detail: Vec<serde_json::Value>,
    /// Slow tools (p95 > 500ms), passed in from tools crate.
    pub slow_tools: Vec<serde_json::Value>,
    /// Database pool metrics.
    pub database: DbMetricsSnapshot,
    /// Storage (WBQ + commit queue) metrics.
    pub storage: StorageMetricsSnapshot,
    /// Search V3 metrics (query volume, fallback, shadow, index health).
    pub search: SearchMetricsSnapshot,
    /// Disk usage metrics.
    pub disk: SystemMetricsSnapshot,
    /// Lock contention metrics.
    pub locks: Vec<LockContentionEntry>,
    /// Automated recommendations based on current metrics.
    pub recommendations: Vec<Recommendation>,
}

/// System information gathered at report time.
#[derive(Debug, Clone, Serialize)]
pub struct SystemInfo {
    /// Process uptime in seconds.
    pub uptime_secs: u64,
    /// Rust compiler version used to build.
    pub rust_version: &'static str,
    /// Target architecture.
    pub target: &'static str,
    /// Operating system description.
    pub os: String,
    /// Number of available CPUs.
    pub cpu_count: usize,
}

/// Health level with underlying signal breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct HealthInfo {
    /// Current health level: `"green"`, `"yellow"`, or `"red"`.
    pub level: String,
    /// Underlying signals that drive the health classification.
    pub signals: HealthSignals,
}

/// A single recommendation for the operator.
#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    /// Severity: `"info"`, `"warning"`, `"critical"`.
    pub severity: &'static str,
    /// Which subsystem the recommendation relates to.
    pub subsystem: &'static str,
    /// Human-readable recommendation text.
    pub message: String,
}

// ---------------------------------------------------------------------------
// Static system info
// ---------------------------------------------------------------------------

/// Process start time for uptime calculation.
static PROCESS_START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// Call early in `main()` to anchor uptime measurement.
pub fn init_process_start() {
    let _ = &*PROCESS_START;
}

#[inline]
pub fn process_uptime() -> std::time::Duration {
    PROCESS_START.elapsed()
}

fn system_info() -> SystemInfo {
    let uptime = process_uptime();
    SystemInfo {
        uptime_secs: uptime.as_secs(),
        rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("nightly"),
        target: std::env::consts::ARCH,
        os: std::env::consts::OS.to_string(),
        cpu_count: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
    }
}

// ---------------------------------------------------------------------------
// Recommendation engine
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)] // deliberate: metric values fit in f64
fn health_recommendations(
    health: HealthLevel,
    signals: &HealthSignals,
    recs: &mut Vec<Recommendation>,
) {
    match health {
        HealthLevel::Red => recs.push(Recommendation {
            severity: "critical",
            subsystem: "health",
            message: "System is in RED health state. Shedding low-priority tool calls. \
                      Investigate pool utilization, WBQ depth, and commit queue."
                .into(),
        }),
        HealthLevel::Yellow => recs.push(Recommendation {
            severity: "warning",
            subsystem: "health",
            message: "System is in YELLOW health state. Load is elevated but not critical.".into(),
        }),
        HealthLevel::Green => {}
    }

    // Pool utilization
    if signals.pool_utilization_pct >= 90 {
        recs.push(Recommendation {
            severity: "critical",
            subsystem: "database",
            message: format!(
                "Pool utilization at {}%. Consider increasing DATABASE_POOL_SIZE.",
                signals.pool_utilization_pct,
            ),
        });
    } else if signals.pool_utilization_pct >= 70 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "database",
            message: format!(
                "Pool utilization at {}%. Monitor for growth.",
                signals.pool_utilization_pct,
            ),
        });
    }

    // Pool acquire latency
    if signals.pool_acquire_p95_us > 100_000 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "database",
            message: format!(
                "Pool acquire p95 latency is {:.1}ms. Consider increasing pool size or \
                 reducing concurrent tool calls.",
                signals.pool_acquire_p95_us as f64 / 1000.0,
            ),
        });
    }

    // WBQ depth
    if signals.wbq_depth_pct >= 80 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "storage",
            message: format!(
                "Write-back queue at {}% capacity. Archive writes may be backing up.",
                signals.wbq_depth_pct,
            ),
        });
    }

    // Commit queue
    if signals.commit_depth_pct >= 80 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "storage",
            message: format!(
                "Commit queue at {}% capacity. Git commits may be falling behind.",
                signals.commit_depth_pct,
            ),
        });
    }
}

#[allow(clippy::cast_precision_loss)] // deliberate: metric values fit in f64
fn operational_recommendations(
    snap: &GlobalMetricsSnapshot,
    lock_snap: &[LockContentionEntry],
    slow_tool_count: usize,
    recs: &mut Vec<Recommendation>,
) {
    // Slow tools
    if slow_tool_count > 0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "tools",
            message: format!(
                "{slow_tool_count} tool(s) have p95 latency > 500ms. Check tools_detail for specifics.",
            ),
        });
    }

    // High error rate
    let tool_calls = snap.tools.tool_calls_total;
    let tool_errors = snap.tools.tool_errors_total;
    if tool_calls > 100 {
        let error_pct = (tool_errors as f64 / tool_calls as f64) * 100.0;
        if error_pct > 10.0 {
            recs.push(Recommendation {
                severity: "warning",
                subsystem: "tools",
                message: format!(
                    "Tool error rate is {error_pct:.1}% ({tool_errors}/{tool_calls}). Investigate failing tools.",
                ),
            });
        }
    }

    // Lock contention
    for entry in lock_snap {
        if entry.contention_ratio > 0.1 && entry.acquire_count > 100 {
            recs.push(Recommendation {
                severity: "warning",
                subsystem: "locks",
                message: format!(
                    "Lock '{}' has {:.1}% contention rate ({} contended / {} acquires). \
                     Max wait: {:.2}ms.",
                    entry.lock_name,
                    entry.contention_ratio * 100.0,
                    entry.contended_count,
                    entry.acquire_count,
                    entry.max_wait_ns as f64 / 1_000_000.0,
                ),
            });
        }
    }

    // Disk pressure
    if snap.system.disk_pressure_level >= 2 {
        recs.push(Recommendation {
            severity: "critical",
            subsystem: "disk",
            message: format!(
                "Disk pressure level {} \u{2014} storage free: {} bytes, DB free: {} bytes.",
                snap.system.disk_pressure_level,
                snap.system.disk_storage_free_bytes,
                snap.system.disk_db_free_bytes,
            ),
        });
    }

    // Search rollout health
    let search = &snap.search;
    if search.fallback_to_legacy_total > 0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "search",
            message: format!(
                "Search V3 fallback-to-legacy count is {}. Investigate Tantivy/V3 availability.",
                search.fallback_to_legacy_total
            ),
        });
    }
    if search.shadow_v3_errors_total > 0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "search",
            message: format!(
                "Shadow mode observed {} V3 errors. Review Search V3 logs before widening rollout.",
                search.shadow_v3_errors_total
            ),
        });
    }
    if search.shadow_comparisons_total >= 10 && search.shadow_equivalent_pct < 80.0 {
        recs.push(Recommendation {
            severity: "warning",
            subsystem: "search",
            message: format!(
                "Shadow equivalence is {:.1}% over {} comparisons; below 80% parity target.",
                search.shadow_equivalent_pct, search.shadow_comparisons_total
            ),
        });
    }
    if search.queries_v3_total > 0 && search.tantivy_doc_count == 0 {
        recs.push(Recommendation {
            severity: "critical",
            subsystem: "search",
            message: "V3 queries are executing but Tantivy doc_count is 0. Validate index build and ingest.".to_string(),
        });
    }
}

fn generate_recommendations(
    snap: &GlobalMetricsSnapshot,
    health: HealthLevel,
    signals: &HealthSignals,
    lock_snap: &[LockContentionEntry],
    slow_tool_count: usize,
) -> Vec<Recommendation> {
    let mut recs = Vec::with_capacity(8);
    health_recommendations(health, signals, &mut recs);
    operational_recommendations(snap, lock_snap, slow_tool_count, &mut recs);
    recs
}

// ---------------------------------------------------------------------------
// Report builder
// ---------------------------------------------------------------------------

impl DiagnosticReport {
    /// Build a comprehensive diagnostic report.
    ///
    /// `tools_detail` and `slow_tools` are passed in as `serde_json::Value`
    /// because the per-tool `MetricsSnapshotEntry` type lives in the tools
    /// crate (which depends on core, not the other way around). The server or
    /// tools crate serializes these before passing them in.
    #[must_use]
    pub fn build(tools_detail: Vec<serde_json::Value>, slow_tools: Vec<serde_json::Value>) -> Self {
        let snap = global_metrics().snapshot();
        let (health_level, signals) = backpressure::compute_health_level_with_signals();
        let lock_snap = lock_contention_snapshot();
        let slow_tool_count = slow_tools.len();

        let recs =
            generate_recommendations(&snap, health_level, &signals, &lock_snap, slow_tool_count);

        Self {
            generated_at: chrono::Utc::now().to_rfc3339(),
            system: system_info(),
            health: HealthInfo {
                level: health_level.as_str().to_string(),
                signals,
            },
            http: snap.http,
            tools_aggregate: snap.tools,
            tools_detail,
            slow_tools,
            database: snap.db,
            storage: snap.storage,
            search: snap.search,
            disk: snap.system,
            locks: lock_snap,
            recommendations: recs,
        }
    }

    /// Serialize to JSON, truncating if the report exceeds 100KB.
    #[must_use]
    pub fn to_json(&self) -> String {
        match serde_json::to_string_pretty(self) {
            Ok(json) if json.len() <= MAX_REPORT_BYTES => json,
            Ok(json) => {
                // Truncate tools_detail to fit within budget.
                let mut truncated = self.clone();
                let mut tools_truncated = false;
                let mut locks_truncated = false;
                while serde_json::to_string(&truncated).map_or(0, |s| s.len()) > MAX_REPORT_BYTES {
                    if !tools_truncated && truncated.tools_detail.len() > 5 {
                        truncated.tools_detail.truncate(5);
                        truncated.tools_detail.push(serde_json::json!({
                            "_truncated": true,
                            "_message": "tools_detail truncated to fit 100KB report limit"
                        }));
                        tools_truncated = true;
                    } else if !locks_truncated && truncated.locks.len() > 5 {
                        truncated.locks.truncate(5);
                        locks_truncated = true;
                    } else {
                        // Give up. Return a valid JSON error object instead of broken JSON.
                        return serde_json::json!({
                            "error": "report too large",
                            "message": "diagnostic report exceeded 100KB limit even after truncation",
                            "size_bytes": json.len()
                        })
                        .to_string();
                    }
                }
                serde_json::to_string_pretty(&truncated).unwrap_or(json)
            }
            Err(_) => r#"{"error":"failed to serialize diagnostic report"}"#.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_process_start_is_idempotent() {
        init_process_start();
        let before = process_uptime();

        std::thread::sleep(std::time::Duration::from_millis(25));
        init_process_start();
        let after = process_uptime();

        assert!(
            after >= before + std::time::Duration::from_millis(10),
            "process uptime appears to have been reset: before={before:?} after={after:?}"
        );
    }

    #[test]
    fn report_builds_without_panic() {
        let report = DiagnosticReport::build(vec![], vec![]);
        assert!(!report.generated_at.is_empty());
        assert!(report.system.cpu_count >= 1);
        assert_eq!(report.health.level, "green");
    }

    #[test]
    fn report_json_serializable() {
        let report = DiagnosticReport::build(vec![], vec![]);
        let json = report.to_json();
        assert!(!json.is_empty());
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.get("generated_at").is_some());
        assert!(parsed.get("health").is_some());
        assert!(parsed.get("search").is_some());
        assert!(parsed.get("recommendations").is_some());
    }

    #[test]
    fn report_respects_size_limit() {
        // Build a report with lots of tool detail to test truncation.
        let big_tools: Vec<serde_json::Value> = (0..1000)
            .map(|i| {
                serde_json::json!({
                    "name": format!("tool_{i}"),
                    "calls": i,
                    "errors": 0,
                    "cluster": "test",
                    "padding": "x".repeat(200),
                })
            })
            .collect();
        let report = DiagnosticReport::build(big_tools, vec![]);
        let json = report.to_json();
        assert!(
            json.len() <= MAX_REPORT_BYTES + 1024, // small grace for truncation boundary
            "report too large: {} bytes",
            json.len()
        );
    }

    #[test]
    fn recommendations_for_healthy_system() {
        let report = DiagnosticReport::build(vec![], vec![]);
        assert_eq!(report.health.level, "green");
        assert!(
            !report
                .recommendations
                .iter()
                .any(|r| r.severity == "critical"),
            "healthy system should have no critical recommendations"
        );
    }

    #[test]
    fn slow_tools_generates_recommendation() {
        let slow = vec![serde_json::json!({
            "name": "send_message",
            "p95_ms": 600.0,
        })];
        let report = DiagnosticReport::build(vec![], slow);
        assert!(
            report
                .recommendations
                .iter()
                .any(|r| r.subsystem == "tools" && r.message.contains("p95")),
            "should warn about slow tools"
        );
    }

    #[test]
    fn system_info_populated() {
        let info = system_info();
        assert!(info.cpu_count >= 1);
        assert!(!info.os.is_empty());
    }

    // -- health_recommendations direct tests --

    fn zero_signals() -> HealthSignals {
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
    fn health_rec_red_emits_critical() {
        let signals = zero_signals();
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Red, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "health"),
            "RED health should produce a critical health recommendation"
        );
    }

    #[test]
    fn health_rec_yellow_emits_warning() {
        let signals = zero_signals();
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Yellow, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "warning" && r.subsystem == "health"),
            "YELLOW health should produce a warning"
        );
    }

    #[test]
    fn health_rec_green_no_health_rec() {
        let signals = zero_signals();
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            !recs.iter().any(|r| r.subsystem == "health"),
            "GREEN health should not produce a health recommendation"
        );
    }

    #[test]
    fn health_rec_pool_90_pct_critical() {
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 95;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "database"),
            "95% pool utilization should trigger critical database recommendation"
        );
    }

    #[test]
    fn health_rec_pool_75_pct_warning() {
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 75;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "warning" && r.subsystem == "database"),
            "75% pool utilization should trigger warning"
        );
    }

    #[test]
    fn health_rec_pool_50_pct_no_rec() {
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 50;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            !recs.iter().any(|r| r.subsystem == "database"),
            "50% pool utilization should not trigger any database recommendation"
        );
    }

    #[test]
    fn health_rec_high_acquire_latency() {
        let mut signals = zero_signals();
        signals.pool_acquire_p95_us = 150_000; // 150ms
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "database" && r.message.contains("latency")),
            "high acquire latency should trigger recommendation"
        );
    }

    #[test]
    fn health_rec_wbq_depth_80() {
        let mut signals = zero_signals();
        signals.wbq_depth_pct = 85;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "storage" && r.message.contains("Write-back")),
            "high WBQ depth should trigger storage recommendation"
        );
    }

    #[test]
    fn health_rec_commit_depth_80() {
        let mut signals = zero_signals();
        signals.commit_depth_pct = 90;
        let mut recs = Vec::new();
        health_recommendations(HealthLevel::Green, &signals, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "storage" && r.message.contains("Commit queue")),
            "high commit depth should trigger storage recommendation"
        );
    }

    // -- operational_recommendations direct tests --

    #[test]
    fn ops_rec_slow_tools() {
        let snap = GlobalMetricsSnapshot::default();
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 3, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "tools" && r.message.contains("3 tool(s)")),
            "should warn about slow tools"
        );
    }

    #[test]
    fn ops_rec_high_error_rate() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 200;
        snap.tools.tool_errors_total = 50; // 25%
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "tools" && r.message.contains("error rate")),
            "25% error rate should trigger warning"
        );
    }

    #[test]
    fn ops_rec_low_error_rate_no_warning() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 200;
        snap.tools.tool_errors_total = 5; // 2.5%
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            !recs.iter().any(|r| r.message.contains("error rate")),
            "2.5% error rate should not trigger warning"
        );
    }

    #[test]
    fn ops_rec_few_calls_skips_error_rate() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 10;
        snap.tools.tool_errors_total = 5; // 50% but only 10 calls
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            !recs.iter().any(|r| r.message.contains("error rate")),
            "should not warn about error rate with < 100 calls"
        );
    }

    #[test]
    fn ops_rec_lock_contention() {
        let snap = GlobalMetricsSnapshot::default();
        let locks = vec![LockContentionEntry {
            lock_name: "TestLock".to_string(),
            rank: 1,
            acquire_count: 500,
            contended_count: 100,
            total_wait_ns: 5_000_000,
            total_hold_ns: 50_000_000,
            max_wait_ns: 1_000_000,
            max_hold_ns: 2_000_000,
            contention_ratio: 0.2, // 20%
        }];
        let mut recs = Vec::new();
        operational_recommendations(&snap, &locks, 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "locks" && r.message.contains("TestLock")),
            "20% contention with 500 acquires should trigger warning"
        );
    }

    #[test]
    fn ops_rec_low_contention_no_warning() {
        let snap = GlobalMetricsSnapshot::default();
        let locks = vec![LockContentionEntry {
            lock_name: "TestLock".to_string(),
            rank: 1,
            acquire_count: 500,
            contended_count: 10,
            total_wait_ns: 100_000,
            total_hold_ns: 5_000_000,
            max_wait_ns: 50_000,
            max_hold_ns: 200_000,
            contention_ratio: 0.02, // 2%
        }];
        let mut recs = Vec::new();
        operational_recommendations(&snap, &locks, 0, &mut recs);
        assert!(
            !recs.iter().any(|r| r.subsystem == "locks"),
            "2% contention should not trigger warning"
        );
    }

    #[test]
    fn ops_rec_disk_pressure() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.system.disk_pressure_level = 2;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "disk"),
            "disk pressure level 2 should trigger critical recommendation"
        );
    }

    #[test]
    fn ops_rec_search_fallback() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.fallback_to_legacy_total = 5;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "search" && r.message.contains("fallback")),
            "search fallback-to-legacy should trigger warning"
        );
    }

    #[test]
    fn ops_rec_shadow_errors() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.shadow_v3_errors_total = 3;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "search" && r.message.contains("Shadow mode")),
            "shadow V3 errors should trigger warning"
        );
    }

    #[test]
    fn ops_rec_low_shadow_equivalence() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.shadow_comparisons_total = 20;
        snap.search.shadow_equivalent_pct = 60.0;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.subsystem == "search" && r.message.contains("equivalence")),
            "60% equivalence with 20+ comparisons should trigger warning"
        );
    }

    #[test]
    fn ops_rec_v3_queries_no_docs() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.search.queries_v3_total = 10;
        snap.search.tantivy_doc_count = 0;
        let mut recs = Vec::new();
        operational_recommendations(&snap, &[], 0, &mut recs);
        assert!(
            recs.iter()
                .any(|r| r.severity == "critical" && r.subsystem == "search"),
            "V3 queries with empty index should trigger critical warning"
        );
    }

    // -- generate_recommendations aggregation --

    #[test]
    fn generate_recs_combines_health_and_ops() {
        let mut snap = GlobalMetricsSnapshot::default();
        snap.tools.tool_calls_total = 200;
        snap.tools.tool_errors_total = 50;
        let mut signals = zero_signals();
        signals.pool_utilization_pct = 95;
        let recs = generate_recommendations(&snap, HealthLevel::Red, &signals, &[], 2);
        // Should have health (red), database (pool 95%), tools (slow + error rate)
        assert!(recs.len() >= 3, "expected at least 3 recommendations, got {}", recs.len());
        assert!(recs.iter().any(|r| r.subsystem == "health"));
        assert!(recs.iter().any(|r| r.subsystem == "database"));
        assert!(recs.iter().any(|r| r.subsystem == "tools"));
    }
}
