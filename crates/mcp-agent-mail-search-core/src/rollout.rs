//! Search V3 rollout controls and shadow comparison infrastructure.
//!
//! This module provides the operational controls for safely rolling out Search V3:
//!
//! - [`RolloutController`] — central orchestration for engine routing and shadow mode
//! - [`ShadowComparison`] — metrics from running both legacy and V3 engines
//! - [`ShadowMetrics`] — aggregate shadow comparison statistics
//!
//! # Rollout Strategy
//!
//! Search V3 is rolled out in phases:
//!
//! 1. **Legacy-only** — Default, stable baseline (`AM_SEARCH_ENGINE=legacy`)
//! 2. **Shadow/LogOnly** — Run both, log comparison, return legacy results
//! 3. **Shadow/Compare** — Run both, log comparison, return V3 results
//! 4. **V3-only** — Full cutover to Search V3 (`AM_SEARCH_ENGINE=lexical|hybrid`)
//!
//! Kill switches (`AM_SEARCH_SEMANTIC_ENABLED`, `AM_SEARCH_RERANK_ENABLED`) allow
//! graceful degradation without full rollback.

// Allow numeric casts for metrics calculations where precision loss is acceptable
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]

use mcp_agent_mail_core::config::{SearchEngine, SearchRolloutConfig, SearchShadowMode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ────────────────────────────────────────────────────────────────────────────
// Shadow Comparison Types
// ────────────────────────────────────────────────────────────────────────────

/// Result of comparing legacy and V3 search outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowComparison {
    /// Percentage of top-10 results shared between legacy and V3 (0.0 - 1.0).
    pub result_overlap_pct: f64,
    /// Kendall tau rank correlation for shared results (-1.0 to 1.0).
    pub rank_correlation: f64,
    /// V3 latency minus legacy latency in milliseconds (positive = V3 slower).
    pub latency_delta_ms: i64,
    /// Whether V3 encountered any errors (should not affect user results in shadow mode).
    pub v3_had_error: bool,
    /// V3 error message if any.
    pub v3_error_message: Option<String>,
    /// Number of results from legacy engine.
    pub legacy_result_count: usize,
    /// Number of results from V3 engine.
    pub v3_result_count: usize,
    /// Query that was executed.
    pub query_text: String,
    /// Timestamp of comparison (micros since epoch).
    pub timestamp_us: i64,
}

impl ShadowComparison {
    /// Create a comparison result from legacy and V3 outputs.
    pub fn compute(
        legacy_ids: &[i64],
        v3_ids: &[i64],
        legacy_latency: Duration,
        v3_latency: Duration,
        v3_error: Option<&str>,
        query_text: &str,
    ) -> Self {
        let legacy_set: std::collections::HashSet<_> = legacy_ids.iter().copied().collect();
        let v3_set: std::collections::HashSet<_> = v3_ids.iter().copied().collect();

        // Result overlap (top-10)
        let legacy_top10: std::collections::HashSet<_> =
            legacy_ids.iter().take(10).copied().collect();
        let v3_top10: std::collections::HashSet<_> = v3_ids.iter().take(10).copied().collect();
        let overlap_count = legacy_top10.intersection(&v3_top10).count();
        let max_top10 = legacy_top10.len().max(v3_top10.len()).max(1);
        let result_overlap_pct = overlap_count as f64 / max_top10 as f64;

        // Kendall tau for shared results (simplified: count concordant/discordant pairs)
        let shared: Vec<i64> = legacy_set.intersection(&v3_set).copied().collect();
        let rank_correlation = if shared.len() >= 2 {
            compute_kendall_tau(legacy_ids, v3_ids, &shared)
        } else {
            0.0
        };

        let latency_delta_ms = v3_latency.as_millis() as i64 - legacy_latency.as_millis() as i64;

        Self {
            result_overlap_pct,
            rank_correlation,
            latency_delta_ms,
            v3_had_error: v3_error.is_some(),
            v3_error_message: v3_error.map(String::from),
            legacy_result_count: legacy_ids.len(),
            v3_result_count: v3_ids.len(),
            query_text: query_text.to_string(),
            timestamp_us: chrono::Utc::now().timestamp_micros(),
        }
    }

    /// Returns `true` if the results are considered equivalent (high overlap, no V3 errors).
    #[must_use]
    pub fn is_equivalent(&self) -> bool {
        self.result_overlap_pct >= 0.8 && !self.v3_had_error
    }

    /// Returns `true` if V3 performed better (faster, no errors, good overlap).
    #[must_use]
    pub fn v3_is_better(&self) -> bool {
        self.latency_delta_ms < 0 && !self.v3_had_error && self.result_overlap_pct >= 0.7
    }
}

/// Compute Kendall tau rank correlation for shared items.
fn compute_kendall_tau(list_a: &[i64], list_b: &[i64], shared: &[i64]) -> f64 {
    if shared.len() < 2 {
        return 0.0;
    }

    // Build position maps
    let pos_a: HashMap<i64, usize> = list_a.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let pos_b: HashMap<i64, usize> = list_b.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // Count concordant and discordant pairs
    let mut concordant = 0i64;
    let mut discordant = 0i64;

    for i in 0..shared.len() {
        for j in (i + 1)..shared.len() {
            let id_i = shared[i];
            let id_j = shared[j];

            if let (Some(&a_i), Some(&a_j), Some(&b_i), Some(&b_j)) = (
                pos_a.get(&id_i),
                pos_a.get(&id_j),
                pos_b.get(&id_i),
                pos_b.get(&id_j),
            ) {
                let a_order = a_i.cmp(&a_j);
                let b_order = b_i.cmp(&b_j);
                if a_order == b_order {
                    concordant += 1;
                } else {
                    discordant += 1;
                }
            }
        }
    }

    let total = concordant + discordant;
    if total == 0 {
        return 0.0;
    }
    (concordant - discordant) as f64 / total as f64
}

// ────────────────────────────────────────────────────────────────────────────
// Aggregate Shadow Metrics
// ────────────────────────────────────────────────────────────────────────────

/// Aggregate statistics from shadow comparisons.
#[derive(Debug, Default)]
pub struct ShadowMetrics {
    /// Total number of shadow comparisons executed.
    pub total_comparisons: AtomicU64,
    /// Number of comparisons where results were equivalent.
    pub equivalent_count: AtomicU64,
    /// Number of comparisons where V3 had errors.
    pub v3_error_count: AtomicU64,
    /// Sum of overlap percentages (for computing average).
    overlap_sum: AtomicU64,
    /// Sum of latency deltas (for computing average).
    latency_delta_sum: AtomicU64,
}

impl ShadowMetrics {
    /// Create a new metrics tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a shadow comparison result.
    pub fn record(&self, comparison: &ShadowComparison) {
        self.total_comparisons.fetch_add(1, Ordering::Relaxed);
        if comparison.is_equivalent() {
            self.equivalent_count.fetch_add(1, Ordering::Relaxed);
        }
        if comparison.v3_had_error {
            self.v3_error_count.fetch_add(1, Ordering::Relaxed);
        }
        // Store overlap as fixed-point (pct * 10000)
        let overlap_fp = (comparison.result_overlap_pct * 10000.0) as u64;
        self.overlap_sum.fetch_add(overlap_fp, Ordering::Relaxed);
        // Store latency delta with offset to handle negatives
        let latency_offset = (comparison.latency_delta_ms + 1_000_000) as u64;
        self.latency_delta_sum
            .fetch_add(latency_offset, Ordering::Relaxed);
    }

    /// Get snapshot of current metrics.
    ///
    /// Precision loss in u64→f64 casts is acceptable for percentage calculations.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn snapshot(&self) -> ShadowMetricsSnapshot {
        let total = self.total_comparisons.load(Ordering::Relaxed);
        let equivalent = self.equivalent_count.load(Ordering::Relaxed);
        let errors = self.v3_error_count.load(Ordering::Relaxed);
        let overlap_sum = self.overlap_sum.load(Ordering::Relaxed);
        let latency_sum = self.latency_delta_sum.load(Ordering::Relaxed);

        let avg_overlap = if total > 0 {
            (overlap_sum as f64 / total as f64) / 10000.0
        } else {
            0.0
        };

        let avg_latency_delta = if total > 0 {
            #[allow(clippy::cast_possible_wrap)]
            let result = (latency_sum as i64 / total as i64) - 1_000_000;
            result
        } else {
            0
        };

        ShadowMetricsSnapshot {
            total_comparisons: total,
            equivalent_count: equivalent,
            equivalent_pct: if total > 0 {
                equivalent as f64 / total as f64 * 100.0
            } else {
                0.0
            },
            v3_error_count: errors,
            v3_error_pct: if total > 0 {
                errors as f64 / total as f64 * 100.0
            } else {
                0.0
            },
            avg_overlap_pct: avg_overlap * 100.0,
            avg_latency_delta_ms: avg_latency_delta,
        }
    }
}

/// Point-in-time snapshot of shadow metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowMetricsSnapshot {
    /// Total number of shadow comparisons.
    pub total_comparisons: u64,
    /// Number of equivalent results.
    pub equivalent_count: u64,
    /// Percentage of equivalent results.
    pub equivalent_pct: f64,
    /// Number of V3 errors.
    pub v3_error_count: u64,
    /// Percentage of V3 errors.
    pub v3_error_pct: f64,
    /// Average result overlap percentage.
    pub avg_overlap_pct: f64,
    /// Average latency delta (V3 - legacy) in milliseconds.
    pub avg_latency_delta_ms: i64,
}

// ────────────────────────────────────────────────────────────────────────────
// Rollout Controller
// ────────────────────────────────────────────────────────────────────────────

/// Central controller for Search V3 rollout orchestration.
///
/// Handles engine routing, shadow mode execution, and metrics collection.
pub struct RolloutController {
    /// Configuration from environment.
    config: SearchRolloutConfig,
    /// Shadow comparison metrics.
    metrics: Arc<ShadowMetrics>,
}

impl RolloutController {
    /// Create a new rollout controller from configuration.
    #[must_use]
    pub fn new(config: SearchRolloutConfig) -> Self {
        Self {
            config,
            metrics: Arc::new(ShadowMetrics::new()),
        }
    }

    /// Get the effective search engine for a given surface (tool name).
    ///
    /// Applies per-surface overrides and kill switch degradation.
    #[must_use]
    pub fn effective_engine(&self, surface: &str) -> SearchEngine {
        self.config.effective_engine(surface)
    }

    /// Returns `true` if shadow mode is active.
    #[must_use]
    pub const fn should_shadow(&self) -> bool {
        self.config.should_shadow()
    }

    /// Get the current shadow mode.
    #[must_use]
    pub const fn shadow_mode(&self) -> SearchShadowMode {
        self.config.shadow_mode
    }

    /// Returns `true` if V3 results should be returned to the user.
    #[must_use]
    pub const fn should_return_v3(&self) -> bool {
        self.config.shadow_mode.returns_v3()
    }

    /// Returns `true` if legacy FTS should be used as fallback on V3 errors.
    #[must_use]
    pub const fn should_fallback_on_error(&self) -> bool {
        self.config.fallback_on_error
    }

    /// Record a shadow comparison result.
    pub fn record_shadow_comparison(&self, comparison: &ShadowComparison) {
        self.metrics.record(comparison);

        // Log the comparison for operators
        if comparison.v3_had_error {
            tracing::warn!(
                query = %comparison.query_text,
                error = ?comparison.v3_error_message,
                "Search V3 shadow comparison: V3 error"
            );
        } else if !comparison.is_equivalent() {
            tracing::info!(
                query = %comparison.query_text,
                overlap_pct = comparison.result_overlap_pct * 100.0,
                rank_correlation = comparison.rank_correlation,
                latency_delta_ms = comparison.latency_delta_ms,
                "Search V3 shadow comparison: divergent results"
            );
        } else {
            tracing::debug!(
                query = %comparison.query_text,
                overlap_pct = comparison.result_overlap_pct * 100.0,
                latency_delta_ms = comparison.latency_delta_ms,
                "Search V3 shadow comparison: equivalent"
            );
        }
    }

    /// Get current shadow metrics snapshot.
    #[must_use]
    pub fn metrics_snapshot(&self) -> ShadowMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Get reference to the underlying config.
    #[must_use]
    pub const fn config(&self) -> &SearchRolloutConfig {
        &self.config
    }
}

impl Default for RolloutController {
    fn default() -> Self {
        Self::new(SearchRolloutConfig::default())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shadow_comparison_equivalent() {
        let comparison = ShadowComparison::compute(
            &[1, 2, 3, 4, 5],
            &[1, 2, 3, 4, 5],
            Duration::from_millis(10),
            Duration::from_millis(8),
            None,
            "test query",
        );
        assert!(comparison.is_equivalent());
        assert_eq!(comparison.result_overlap_pct, 1.0);
        assert!(!comparison.v3_had_error);
    }

    #[test]
    fn test_shadow_comparison_divergent() {
        let comparison = ShadowComparison::compute(
            &[1, 2, 3, 4, 5],
            &[6, 7, 8, 9, 10],
            Duration::from_millis(10),
            Duration::from_millis(15),
            None,
            "test query",
        );
        assert!(!comparison.is_equivalent());
        assert_eq!(comparison.result_overlap_pct, 0.0);
    }

    #[test]
    fn test_shadow_comparison_with_v3_error() {
        let comparison = ShadowComparison::compute(
            &[1, 2, 3],
            &[],
            Duration::from_millis(10),
            Duration::from_millis(100),
            Some("index not ready"),
            "test query",
        );
        assert!(!comparison.is_equivalent());
        assert!(comparison.v3_had_error);
        assert_eq!(
            comparison.v3_error_message.as_deref(),
            Some("index not ready")
        );
    }

    #[test]
    fn test_kendall_tau_perfect_agreement() {
        let tau = compute_kendall_tau(&[1, 2, 3, 4], &[1, 2, 3, 4], &[1, 2, 3, 4]);
        assert!((tau - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_kendall_tau_perfect_disagreement() {
        let tau = compute_kendall_tau(&[1, 2, 3, 4], &[4, 3, 2, 1], &[1, 2, 3, 4]);
        assert!((tau - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_shadow_metrics_recording() {
        let metrics = ShadowMetrics::new();

        let comparison1 = ShadowComparison::compute(
            &[1, 2, 3],
            &[1, 2, 3],
            Duration::from_millis(10),
            Duration::from_millis(8),
            None,
            "query1",
        );
        metrics.record(&comparison1);

        let comparison2 = ShadowComparison::compute(
            &[1, 2, 3],
            &[4, 5, 6],
            Duration::from_millis(10),
            Duration::from_millis(20),
            None,
            "query2",
        );
        metrics.record(&comparison2);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_comparisons, 2);
        assert_eq!(snapshot.equivalent_count, 1);
    }

    #[test]
    fn test_rollout_controller_effective_engine() {
        let mut config = SearchRolloutConfig::default();
        config.engine = SearchEngine::Lexical;
        config
            .surface_overrides
            .insert("summarize_thread".to_string(), SearchEngine::Legacy);

        let controller = RolloutController::new(config);

        assert_eq!(
            controller.effective_engine("search_messages"),
            SearchEngine::Lexical
        );
        assert_eq!(
            controller.effective_engine("summarize_thread"),
            SearchEngine::Legacy
        );
    }

    #[test]
    fn test_rollout_controller_kill_switch_degradation() {
        let mut config = SearchRolloutConfig::default();
        config.engine = SearchEngine::Hybrid;
        config.semantic_enabled = false; // Kill switch

        let controller = RolloutController::new(config);

        // Hybrid degrades to Lexical when semantic is disabled
        assert_eq!(
            controller.effective_engine("search_messages"),
            SearchEngine::Lexical
        );
    }

    #[test]
    fn test_rollout_controller_shadow_mode() {
        let mut config = SearchRolloutConfig::default();
        config.shadow_mode = SearchShadowMode::LogOnly;

        let controller = RolloutController::new(config);

        assert!(controller.should_shadow());
        assert!(!controller.should_return_v3());
    }

    #[test]
    fn test_rollout_controller_shadow_compare_mode() {
        let mut config = SearchRolloutConfig::default();
        config.shadow_mode = SearchShadowMode::Compare;

        let controller = RolloutController::new(config);

        assert!(controller.should_shadow());
        assert!(controller.should_return_v3());
    }
}
