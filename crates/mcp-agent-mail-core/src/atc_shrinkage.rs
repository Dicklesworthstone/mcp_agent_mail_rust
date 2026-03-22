#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]
//! Empirical-Bayes shrinkage across agent, project, and program strata
//! (br-0qt6e.3.3).
//!
//! Prevents sparse strata from producing aggressive policy movements by
//! borrowing strength from broader cohorts. The shrinkage hierarchy is:
//!
//! ```text
//!   Global (all strata pooled)
//!     └── Program cohort (claude-code, codex-cli, etc.)
//!           └── Project cohort (per-project within program)
//!                 └── Agent stratum (per-agent within project)
//!                       └── Action family (advisory/probe/release per agent)
//! ```
//!
//! # Shrinkage Model
//!
//! Uses the James-Stein estimator for loss/accuracy estimates:
//!
//! ```text
//!   θ_shrunk = (1 - B) × θ_local + B × θ_global
//!
//!   B = max(0, 1 - (k - 2) × σ²_within / Σ(θ_i - θ_global)²)
//! ```
//!
//! where `B` is the shrinkage factor (0 = no shrinkage, 1 = full pooling),
//! `k` is the number of strata, and `σ²_within` is the within-stratum
//! variance.
//!
//! In practice, we use a simplified version: the shrinkage weight is
//! proportional to the relative precision (sample size) of the local
//! vs. population estimate.
//!
//! # Minimum-Support Behavior
//!
//! When a stratum has fewer than `MIN_LOCAL_SUPPORT` observations (default
//! 10), the estimator falls back entirely to the parent cohort. This
//! prevents overfitting on tiny sample sizes.
//!
//! # When to Disable Shrinkage
//!
//! Shrinkage should be reduced or disabled when:
//! - Local sample size exceeds 100 observations (strong local evidence)
//! - Population variance is very high (cohorts are too heterogeneous)
//! - An operator explicitly flags a stratum as "independent"

use serde::{Deserialize, Serialize};

/// Minimum local observations before local estimates are used at all.
/// Below this threshold, the estimator uses pure population pooling.
pub const MIN_LOCAL_SUPPORT: u64 = 10;

/// Default minimum strata count for meaningful shrinkage.
/// Below this, shrinkage degenerates to per-stratum or full pooling.
pub const MIN_STRATA_FOR_SHRINKAGE: usize = 3;

/// Maximum shrinkage weight (caps how much we trust the population).
pub const MAX_SHRINKAGE_WEIGHT: f64 = 0.95;

/// A stratum-level estimate with sample size for shrinkage computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StratumEstimate {
    /// Stratum identifier (e.g., "liveness:advisory:low").
    pub stratum_key: String,
    /// Local mean estimate (e.g., loss rate, accuracy).
    pub local_mean: f64,
    /// Number of local observations.
    pub local_count: u64,
    /// Local variance estimate (if computable).
    pub local_variance: Option<f64>,
}

/// Result of applying shrinkage to a stratum estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShrunkEstimate {
    /// Stratum identifier.
    pub stratum_key: String,
    /// Shrunk estimate (blended local + population).
    pub shrunk_mean: f64,
    /// Shrinkage weight applied (0 = pure local, 1 = pure population).
    pub shrinkage_weight: f64,
    /// Effective sample size after shrinkage.
    pub effective_sample_size: f64,
    /// Which cohort the population estimate came from.
    pub cohort_source: CohortSource,
    /// Whether the estimate is based on sufficient local evidence.
    pub has_local_support: bool,
}

/// Which level of the cohort hierarchy provided the population estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CohortSource {
    /// Global average across all strata.
    Global,
    /// Program-type cohort (e.g., claude-code agents).
    Program,
    /// Project-level cohort.
    Project,
    /// Agent-level (no shrinkage applied — pure local).
    Local,
}

impl std::fmt::Display for CohortSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "global"),
            Self::Program => write!(f, "program"),
            Self::Project => write!(f, "project"),
            Self::Local => write!(f, "local"),
        }
    }
}

/// Population-level statistics for a cohort.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CohortStats {
    /// Grand mean across all strata in this cohort.
    pub grand_mean: f64,
    /// Between-strata variance.
    pub between_variance: f64,
    /// Average within-stratum variance.
    pub within_variance: f64,
    /// Number of strata in this cohort.
    pub strata_count: usize,
    /// Total observations across all strata.
    pub total_count: u64,
}

/// Apply empirical-Bayes shrinkage to a set of stratum estimates.
///
/// Returns shrunk estimates that blend each local estimate toward the
/// population mean, with shrinkage strength proportional to the
/// relative precision of local vs. population evidence.
///
/// # Shrinkage Formula
///
/// For each stratum `i`:
/// ```text
/// B_i = within_variance / (within_variance + local_count_i × between_variance)
/// shrunk_i = (1 - B_i) × local_i + B_i × grand_mean
/// ```
///
/// When `between_variance ≈ 0` (homogeneous population), `B ≈ 1` → full pooling.
/// When `between_variance >> within_variance`, `B ≈ 0` → no shrinkage.
#[must_use]
pub fn shrink_estimates(
    estimates: &[StratumEstimate],
    population: &CohortStats,
) -> Vec<ShrunkEstimate> {
    estimates
        .iter()
        .map(|est| shrink_single(est, population))
        .collect()
}

/// Apply shrinkage to a single stratum estimate.
#[must_use]
pub fn shrink_single(estimate: &StratumEstimate, population: &CohortStats) -> ShrunkEstimate {
    // Insufficient local support → pure population pooling.
    if estimate.local_count < MIN_LOCAL_SUPPORT {
        return ShrunkEstimate {
            stratum_key: estimate.stratum_key.clone(),
            shrunk_mean: population.grand_mean,
            shrinkage_weight: 1.0,
            effective_sample_size: population.total_count as f64,
            cohort_source: CohortSource::Global,
            has_local_support: false,
        };
    }

    // Not enough strata for meaningful shrinkage → use local.
    if population.strata_count < MIN_STRATA_FOR_SHRINKAGE {
        return ShrunkEstimate {
            stratum_key: estimate.stratum_key.clone(),
            shrunk_mean: estimate.local_mean,
            shrinkage_weight: 0.0,
            effective_sample_size: estimate.local_count as f64,
            cohort_source: CohortSource::Local,
            has_local_support: true,
        };
    }

    // Compute shrinkage weight.
    // B = σ²_within / (σ²_within + n_local × σ²_between)
    let within = population.within_variance.max(1e-10);
    let n = estimate.local_count as f64;
    let between = population.between_variance;

    let shrinkage_weight = if between > 0.0 && n > 0.0 {
        let raw = within / n.mul_add(between, within);
        raw.clamp(0.0, MAX_SHRINKAGE_WEIGHT)
    } else {
        MAX_SHRINKAGE_WEIGHT // cap population trust even under full pooling
    };

    let shrunk = (1.0 - shrinkage_weight).mul_add(
        estimate.local_mean,
        shrinkage_weight * population.grand_mean,
    );

    // Effective sample size: local_count + weight × population_count.
    let effective = n + shrinkage_weight * population.total_count as f64;

    ShrunkEstimate {
        stratum_key: estimate.stratum_key.clone(),
        shrunk_mean: shrunk,
        shrinkage_weight,
        effective_sample_size: effective,
        cohort_source: if shrinkage_weight > 0.5 {
            CohortSource::Global
        } else {
            CohortSource::Local
        },
        has_local_support: true,
    }
}

/// Compute population statistics from a set of stratum estimates.
///
/// Returns the grand mean, between-strata variance, and average
/// within-stratum variance needed for shrinkage computation.
#[must_use]
pub fn compute_cohort_stats(estimates: &[StratumEstimate]) -> CohortStats {
    if estimates.is_empty() {
        return CohortStats::default();
    }

    let total_count: u64 = estimates.iter().map(|e| e.local_count).sum();
    let strata_count = estimates.len();

    // Grand mean (weighted by sample size).
    let weighted_sum: f64 = estimates
        .iter()
        .map(|e| e.local_mean * e.local_count as f64)
        .sum();
    let grand_mean = if total_count > 0 {
        weighted_sum / total_count as f64
    } else {
        0.0
    };

    // Between-strata variance: Var(θ_i around grand_mean).
    let between_variance = if strata_count > 1 {
        let sum_sq: f64 = estimates
            .iter()
            .map(|e| {
                let diff = e.local_mean - grand_mean;
                diff * diff
            })
            .sum();
        sum_sq / (strata_count - 1) as f64
    } else {
        0.0
    };

    // Average within-stratum variance.
    let within_variance = {
        let vars: Vec<f64> = estimates.iter().filter_map(|e| e.local_variance).collect();
        if vars.is_empty() {
            // Fallback: use between-strata variance directly as a proxy
            // for within-variance. This is conservative: when within ≈
            // between, the shrinkage weight B = within/(within + n*between)
            // ≈ 1/(1+n), giving moderate pooling that trusts local estimates
            // proportionally to their sample size. We do NOT divide by
            // strata_count here because that would make within << between,
            // biasing toward LESS shrinkage (more trust in sparse locals),
            // which is the opposite of conservative behavior.
            between_variance.max(0.01)
        } else {
            vars.iter().sum::<f64>() / vars.len() as f64
        }
    };

    CohortStats {
        grand_mean,
        between_variance,
        within_variance,
        strata_count,
        total_count,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    fn make_estimate(key: &str, mean: f64, count: u64) -> StratumEstimate {
        StratumEstimate {
            stratum_key: key.to_string(),
            local_mean: mean,
            local_count: count,
            local_variance: Some(mean * (1.0 - mean) / count as f64),
        }
    }

    #[test]
    fn sparse_stratum_uses_population() {
        let sparse = StratumEstimate {
            stratum_key: "sparse".to_string(),
            local_mean: 0.9,
            local_count: 3, // below MIN_LOCAL_SUPPORT
            local_variance: None,
        };
        let pop = CohortStats {
            grand_mean: 0.5,
            between_variance: 0.1,
            within_variance: 0.05,
            strata_count: 10,
            total_count: 500,
        };

        let shrunk = shrink_single(&sparse, &pop);
        assert_eq!(shrunk.shrinkage_weight, 1.0);
        assert!((shrunk.shrunk_mean - 0.5).abs() < 1e-10);
        assert!(!shrunk.has_local_support);
        assert_eq!(shrunk.cohort_source, CohortSource::Global);
    }

    #[test]
    fn sufficient_local_evidence_reduces_shrinkage() {
        let strong = make_estimate("strong", 0.8, 200);
        let pop = CohortStats {
            grand_mean: 0.5,
            between_variance: 0.1,
            within_variance: 0.01,
            strata_count: 10,
            total_count: 1000,
        };

        let shrunk = shrink_single(&strong, &pop);
        // With 200 observations and large between-variance, local should dominate.
        assert!(
            shrunk.shrinkage_weight < 0.1,
            "weight={}",
            shrunk.shrinkage_weight
        );
        assert!((shrunk.shrunk_mean - 0.8).abs() < 0.05);
        assert!(shrunk.has_local_support);
    }

    #[test]
    fn homogeneous_population_pools_fully() {
        let est = make_estimate("test", 0.6, 50);
        let pop = CohortStats {
            grand_mean: 0.5,
            between_variance: 0.0, // no between-variance
            within_variance: 0.05,
            strata_count: 10,
            total_count: 500,
        };

        let shrunk = shrink_single(&est, &pop);
        assert!((shrunk.shrinkage_weight - MAX_SHRINKAGE_WEIGHT).abs() < 1e-10);
    }

    #[test]
    fn too_few_strata_uses_local() {
        let est = make_estimate("test", 0.7, 50);
        let pop = CohortStats {
            grand_mean: 0.5,
            between_variance: 0.1,
            within_variance: 0.05,
            strata_count: 2, // below MIN_STRATA_FOR_SHRINKAGE
            total_count: 100,
        };

        let shrunk = shrink_single(&est, &pop);
        assert_eq!(shrunk.shrinkage_weight, 0.0);
        assert!((shrunk.shrunk_mean - 0.7).abs() < 1e-10);
        assert_eq!(shrunk.cohort_source, CohortSource::Local);
    }

    #[test]
    fn compute_cohort_stats_works() {
        let estimates = vec![
            make_estimate("a", 0.8, 100),
            make_estimate("b", 0.6, 100),
            make_estimate("c", 0.7, 100),
        ];

        let stats = compute_cohort_stats(&estimates);
        assert_eq!(stats.strata_count, 3);
        assert_eq!(stats.total_count, 300);
        assert!((stats.grand_mean - 0.7).abs() < 1e-10);
        assert!(stats.between_variance > 0.0);
    }

    #[test]
    fn shrink_estimates_batch() {
        let estimates = vec![
            make_estimate("a", 0.8, 100),
            make_estimate("b", 0.6, 100),
            make_estimate("c", 0.7, 100),
            StratumEstimate {
                stratum_key: "sparse".to_string(),
                local_mean: 0.9,
                local_count: 5, // sparse
                local_variance: None,
            },
        ];

        let pop = compute_cohort_stats(&estimates);
        let shrunk = shrink_estimates(&estimates, &pop);
        assert_eq!(shrunk.len(), 4);

        // Sparse stratum should be fully pooled.
        let sparse_result = &shrunk[3];
        assert_eq!(sparse_result.shrinkage_weight, 1.0);
        assert!(!sparse_result.has_local_support);
    }

    #[test]
    fn empty_estimates() {
        let stats = compute_cohort_stats(&[]);
        assert_eq!(stats.strata_count, 0);
        assert_eq!(stats.total_count, 0);
    }

    #[test]
    fn single_stratum_no_shrinkage() {
        let estimates = vec![make_estimate("solo", 0.75, 50)];
        let stats = compute_cohort_stats(&estimates);
        let shrunk = shrink_estimates(&estimates, &stats);

        // Single stratum → too few for shrinkage → use local.
        assert_eq!(shrunk[0].shrinkage_weight, 0.0);
    }

    #[test]
    fn effective_sample_size_increases_with_shrinkage() {
        let est = make_estimate("test", 0.7, 20);
        let pop = CohortStats {
            grand_mean: 0.5,
            between_variance: 0.01,
            within_variance: 0.05,
            strata_count: 10,
            total_count: 500,
        };

        let shrunk = shrink_single(&est, &pop);
        // Effective sample size should be larger than local count.
        assert!(
            shrunk.effective_sample_size > 20.0,
            "effective_n={} should exceed local_n=20",
            shrunk.effective_sample_size
        );
    }

    #[test]
    fn serde_roundtrip() {
        let est = ShrunkEstimate {
            stratum_key: "test".to_string(),
            shrunk_mean: 0.65,
            shrinkage_weight: 0.3,
            effective_sample_size: 150.0,
            cohort_source: CohortSource::Global,
            has_local_support: true,
        };
        let json = serde_json::to_string(&est).unwrap();
        let decoded: ShrunkEstimate = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stratum_key, "test");
        assert!((decoded.shrunk_mean - 0.65).abs() < 1e-10);
    }
}
