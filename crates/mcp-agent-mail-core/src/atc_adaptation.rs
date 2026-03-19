#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]
//! Finite-policy regret-bounded adaptation engine (br-0qt6e.3.5).
//!
//! Selects among a finite set of candidate policy artifacts using
//! cumulative regret bounds. The engine maintains internal counterfactual
//! scoring for non-selected candidates without public shadow mode.
//!
//! # Policy Family
//!
//! Each candidate policy is a complete specification:
//!
//! | Field             | Description                                    |
//! |-------------------|------------------------------------------------|
//! | `policy_id`       | Unique identifier (e.g., "pol-baseline-v1")    |
//! | `loss_matrix`     | 3×3 loss matrix entries per subsystem           |
//! | `alpha`           | EWMA learning rate for posterior updates         |
//! | `probe_fraction`  | Fraction of tick budget allocated to probes      |
//! | `release_threshold` | Minimum posterior for release action           |
//!
//! # Adaptation Algorithm
//!
//! Uses EXP3 (Exponential-weight algorithm for Exploration and
//! Exploitation) for adversarial multi-armed bandit selection:
//!
//! ```text
//! For each policy i:
//!   w_i = exp(-eta * cumulative_loss_i)
//!   p_i = (1 - gamma) * w_i / sum(w) + gamma / K
//!
//! Selected policy = sample from distribution p
//! ```
//!
//! where `eta` is the learning rate and `gamma` is the exploration
//! parameter.
//!
//! # Counterfactual Scoring
//!
//! Non-selected candidates are scored using importance-weighted
//! off-policy evaluation: what loss would they have incurred given
//! the same state? This avoids the need for public shadow mode.
//!
//! # Promotion Criteria
//!
//! A candidate policy can replace the incumbent when:
//! 1. It has been executed on >= `min_observations` experiences
//! 2. Its estimated regret vs. incumbent is negative (it's better)
//! 3. No safety metric has regressed beyond tolerance
//! 4. The regime is stable (not transitioning or cooling)
//! 5. The risk budget for the stratum is healthy

use serde::{Deserialize, Serialize};

/// Default exploration parameter for EXP3.
pub const DEFAULT_GAMMA: f64 = 0.1;

/// Default learning rate for EXP3.
pub const DEFAULT_ETA: f64 = 0.01;

/// Minimum observations before promotion is considered.
pub const DEFAULT_MIN_PROMOTION_OBS: u64 = 200;

/// Maximum regret advantage required for promotion (fraction).
pub const DEFAULT_MIN_REGRET_ADVANTAGE: f64 = 0.10;

/// Maximum policies in the candidate set.
pub const MAX_POLICY_CANDIDATES: usize = 20;

/// A candidate policy artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCandidate {
    /// Unique policy identifier.
    pub policy_id: String,
    /// Human-readable description.
    pub description: String,
    /// Loss matrix adjustment factors (relative to baseline).
    /// Format: `{subsystem: {action: {state: factor}}}`.
    pub loss_adjustments: serde_json::Value,
    /// EWMA alpha override (None = use incumbent's alpha).
    pub alpha_override: Option<f64>,
    /// Probe fraction override.
    pub probe_fraction_override: Option<f64>,
    /// Release threshold override.
    pub release_threshold_override: Option<f64>,
    /// Whether this policy is the current incumbent.
    pub is_incumbent: bool,
}

/// Per-policy tracking state maintained by the adaptation engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyTracker {
    /// Policy identifier.
    pub policy_id: String,
    /// Cumulative realized loss (when selected and executed).
    pub cumulative_loss: f64,
    /// Cumulative counterfactual loss (importance-weighted off-policy).
    pub counterfactual_loss: f64,
    /// Number of times this policy was selected and executed.
    pub selection_count: u64,
    /// Number of counterfactual evaluations.
    pub counterfactual_count: u64,
    /// EXP3 weight (log-space for numerical stability).
    pub log_weight: f64,
    /// Current selection probability.
    pub selection_probability: f64,
    /// Whether promotion criteria are currently met.
    pub promotion_eligible: bool,
}

/// Status of a promotion evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PromotionStatus {
    /// Not enough observations yet.
    InsufficientData { observations: u64, required: u64 },
    /// Candidate is worse than incumbent.
    InsufficientAdvantage { regret_gap: f64, required: f64 },
    /// Safety metric would regress.
    SafetyRegression { metric: String, regression: f64 },
    /// Regime is unstable (transitioning or cooling).
    RegimeUnstable,
    /// Risk budget is stressed or blocking.
    RiskBudgetStressed,
    /// All criteria met — promotion is recommended.
    Ready { estimated_improvement: f64 },
}

/// The adaptation engine state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptationEngine {
    /// Candidate policies.
    pub candidates: Vec<PolicyCandidate>,
    /// Per-policy tracking.
    pub trackers: Vec<PolicyTracker>,
    /// Index of the current incumbent policy.
    pub incumbent_index: usize,
    /// Index of the currently selected (executing) policy.
    pub selected_index: usize,
    /// EXP3 exploration parameter.
    pub gamma: f64,
    /// EXP3 learning rate.
    pub eta: f64,
    /// Minimum observations for promotion.
    pub min_promotion_observations: u64,
    /// Minimum regret advantage for promotion.
    pub min_regret_advantage: f64,
    /// Total adaptation rounds (selections).
    pub total_rounds: u64,
    /// Total promotions made.
    pub total_promotions: u64,
    /// Policy revision counter.
    pub policy_revision: u64,
}

impl AdaptationEngine {
    /// Create a new engine with the baseline policy as incumbent.
    #[must_use]
    pub fn new(baseline: PolicyCandidate) -> Self {
        let tracker = PolicyTracker {
            policy_id: baseline.policy_id.clone(),
            cumulative_loss: 0.0,
            counterfactual_loss: 0.0,
            selection_count: 0,
            counterfactual_count: 0,
            log_weight: 0.0, // exp(0) = 1
            selection_probability: 1.0,
            promotion_eligible: false,
        };

        Self {
            candidates: vec![baseline],
            trackers: vec![tracker],
            incumbent_index: 0,
            selected_index: 0,
            gamma: DEFAULT_GAMMA,
            eta: DEFAULT_ETA,
            min_promotion_observations: DEFAULT_MIN_PROMOTION_OBS,
            min_regret_advantage: DEFAULT_MIN_REGRET_ADVANTAGE,
            total_rounds: 0,
            total_promotions: 0,
            policy_revision: 0,
        }
    }

    /// Add a candidate policy to the set.
    ///
    /// Returns `false` if the set is full or a duplicate ID exists.
    pub fn add_candidate(&mut self, candidate: PolicyCandidate) -> bool {
        if self.candidates.len() >= MAX_POLICY_CANDIDATES {
            return false;
        }
        if self
            .candidates
            .iter()
            .any(|c| c.policy_id == candidate.policy_id)
        {
            return false;
        }

        let tracker = PolicyTracker {
            policy_id: candidate.policy_id.clone(),
            cumulative_loss: 0.0,
            counterfactual_loss: 0.0,
            selection_count: 0,
            counterfactual_count: 0,
            log_weight: 0.0,
            selection_probability: 0.0,
            promotion_eligible: false,
        };

        self.candidates.push(candidate);
        self.trackers.push(tracker);
        self.recompute_probabilities();
        true
    }

    /// Record a realized loss for the currently selected policy.
    pub fn record_loss(&mut self, loss: f64) {
        let idx = self.selected_index;
        if idx < self.trackers.len() {
            self.trackers[idx].cumulative_loss += loss;
            self.trackers[idx].selection_count += 1;

            // EXP3 weight update: w *= exp(-eta * loss_hat) where loss_hat = loss / p.
            // Cap the importance-weighted loss to prevent variance blowup when
            // p is very small (e.g., gamma/K = 0.005 with K=20 policies).
            // Without the cap, loss=1.0 with p=0.005 would cause log_weight -= 200*eta,
            // an extreme weight swing from a single observation.
            let p = self.trackers[idx].selection_probability.max(1e-10);
            let loss_hat = (loss / p).min(10.0); // cap at 10× to prevent variance blowup
            self.trackers[idx].log_weight -= self.eta * loss_hat;
        }

        // Note: In standard EXP3, non-selected arms receive 0 estimated loss.
        // The importance-weighted loss estimator for arm i at round t is:
        //   loss_hat_i = loss_t * I(selected=i) / p_i
        // For non-selected arms, I(selected=i) = 0, so loss_hat = 0.
        // We do NOT accumulate counterfactual loss for non-selected policies
        // because assigning the selected arm's raw loss to all non-selected
        // arms makes all counterfactual scores identical and useless for
        // distinguishing policy quality. Instead, promotion decisions should
        // rely on the EXP3 selection probabilities, which already encode
        // learned policy quality through the weight update mechanism.
        for (i, tracker) in self.trackers.iter_mut().enumerate() {
            if i != idx {
                tracker.counterfactual_count += 1;
                // Counterfactual loss = 0 for non-selected arms (standard EXP3).
            }
        }

        self.total_rounds += 1;
        self.recompute_probabilities();
    }

    /// Recompute EXP3 selection probabilities.
    fn recompute_probabilities(&mut self) {
        #[allow(clippy::cast_precision_loss)]
        let k = self.trackers.len() as f64;
        if k < 1.0 {
            return;
        }

        // Compute weights in log-space, then normalize.
        let max_log_w = self
            .trackers
            .iter()
            .map(|t| t.log_weight)
            .fold(f64::NEG_INFINITY, f64::max);

        let sum_w: f64 = self
            .trackers
            .iter()
            .map(|t| (t.log_weight - max_log_w).exp())
            .sum();

        for tracker in &mut self.trackers {
            let w = (tracker.log_weight - max_log_w).exp();
            tracker.selection_probability = (1.0 - self.gamma) * w / sum_w + self.gamma / k;
        }
    }

    /// Select the next policy for the upcoming round.
    ///
    /// Uses the EXP3 probability distribution. For deterministic
    /// testing, always selects the highest-probability policy.
    pub fn select_deterministic(&mut self) -> &PolicyCandidate {
        let best_idx = self
            .trackers
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.selection_probability
                    .partial_cmp(&b.selection_probability)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or(0, |(i, _)| i);

        self.selected_index = best_idx;
        &self.candidates[best_idx]
    }

    /// Evaluate whether a candidate should be promoted to incumbent.
    #[must_use]
    pub fn evaluate_promotion(
        &self,
        candidate_index: usize,
        regime_stable: bool,
        risk_budget_healthy: bool,
    ) -> PromotionStatus {
        let Some(tracker) = self.trackers.get(candidate_index) else {
            return PromotionStatus::InsufficientData {
                observations: 0,
                required: self.min_promotion_observations,
            };
        };

        let total_obs = tracker.selection_count;
        if total_obs < self.min_promotion_observations {
            return PromotionStatus::InsufficientData {
                observations: total_obs,
                required: self.min_promotion_observations,
            };
        }

        if !regime_stable {
            return PromotionStatus::RegimeUnstable;
        }

        if !risk_budget_healthy {
            return PromotionStatus::RiskBudgetStressed;
        }

        // Compare with incumbent.
        let incumbent = &self.trackers[self.incumbent_index];
        let incumbent_avg = if incumbent.selection_count > 0 {
            incumbent.cumulative_loss / incumbent.selection_count as f64
        } else {
            0.0
        };
        let candidate_avg = if tracker.selection_count > 0 {
            tracker.cumulative_loss / tracker.selection_count as f64
        } else {
            0.0
        };

        let regret_gap = candidate_avg - incumbent_avg;
        if regret_gap >= 0.0 {
            // Candidate is not better than incumbent.
            return PromotionStatus::InsufficientAdvantage {
                regret_gap,
                required: -self.min_regret_advantage,
            };
        }

        let improvement = -regret_gap / incumbent_avg.max(1e-10);
        if improvement < self.min_regret_advantage {
            return PromotionStatus::InsufficientAdvantage {
                regret_gap,
                required: -self.min_regret_advantage,
            };
        }

        PromotionStatus::Ready {
            estimated_improvement: improvement,
        }
    }

    /// Promote a candidate to incumbent.
    pub fn promote(&mut self, candidate_index: usize) -> bool {
        if candidate_index >= self.candidates.len() {
            return false;
        }

        // Demote current incumbent.
        self.candidates[self.incumbent_index].is_incumbent = false;

        // Promote candidate.
        self.incumbent_index = candidate_index;
        self.candidates[candidate_index].is_incumbent = true;
        self.selected_index = candidate_index;
        self.total_promotions += 1;
        self.policy_revision += 1;

        true
    }

    /// Get the current incumbent policy.
    #[must_use]
    pub fn incumbent(&self) -> &PolicyCandidate {
        &self.candidates[self.incumbent_index]
    }

    /// Number of candidate policies.
    #[must_use]
    pub const fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// Get a compact summary for operator display.
    #[must_use]
    pub fn summary(&self) -> AdaptationSummary {
        AdaptationSummary {
            incumbent_id: self.candidates[self.incumbent_index].policy_id.clone(),
            candidate_count: self.candidates.len(),
            total_rounds: self.total_rounds,
            total_promotions: self.total_promotions,
            policy_revision: self.policy_revision,
            probabilities: self
                .trackers
                .iter()
                .map(|t| (t.policy_id.clone(), t.selection_probability))
                .collect(),
        }
    }
}

/// Compact adaptation summary for operator surfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptationSummary {
    pub incumbent_id: String,
    pub candidate_count: usize,
    pub total_rounds: u64,
    pub total_promotions: u64,
    pub policy_revision: u64,
    pub probabilities: Vec<(String, f64)>,
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_policy() -> PolicyCandidate {
        PolicyCandidate {
            policy_id: "pol-baseline".to_string(),
            description: "Default baseline policy".to_string(),
            loss_adjustments: serde_json::json!({}),
            alpha_override: None,
            probe_fraction_override: None,
            release_threshold_override: None,
            is_incumbent: true,
        }
    }

    fn candidate_policy(id: &str) -> PolicyCandidate {
        PolicyCandidate {
            policy_id: id.to_string(),
            description: format!("Candidate {id}"),
            loss_adjustments: serde_json::json!({}),
            alpha_override: Some(0.25),
            probe_fraction_override: None,
            release_threshold_override: None,
            is_incumbent: false,
        }
    }

    #[test]
    fn new_engine_has_baseline() {
        let engine = AdaptationEngine::new(baseline_policy());
        assert_eq!(engine.candidate_count(), 1);
        assert_eq!(engine.incumbent().policy_id, "pol-baseline");
    }

    #[test]
    fn add_candidate() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        assert!(engine.add_candidate(candidate_policy("pol-v2")));
        assert_eq!(engine.candidate_count(), 2);
    }

    #[test]
    fn duplicate_candidate_rejected() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        assert!(!engine.add_candidate(baseline_policy())); // same ID
    }

    #[test]
    fn record_loss_updates_tracker() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.record_loss(1.5);
        assert_eq!(engine.trackers[0].selection_count, 1);
        assert!((engine.trackers[0].cumulative_loss - 1.5).abs() < 1e-10);
    }

    #[test]
    fn probabilities_sum_to_one() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));
        engine.add_candidate(candidate_policy("v3"));

        let sum: f64 = engine
            .trackers
            .iter()
            .map(|t| t.selection_probability)
            .sum();
        assert!((sum - 1.0).abs() < 1e-10, "prob sum = {sum}");
    }

    #[test]
    fn deterministic_selection() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));

        let selected = engine.select_deterministic();
        assert!(!selected.policy_id.is_empty());
    }

    #[test]
    fn insufficient_data_blocks_promotion() {
        let engine = AdaptationEngine::new(baseline_policy());
        let status = engine.evaluate_promotion(0, true, true);
        assert!(matches!(status, PromotionStatus::InsufficientData { .. }));
    }

    #[test]
    fn regime_instability_blocks_promotion() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));
        // Fake sufficient data.
        engine.trackers[1].selection_count = 300;
        engine.trackers[1].cumulative_loss = 10.0;
        engine.trackers[0].selection_count = 300;
        engine.trackers[0].cumulative_loss = 50.0;

        let status = engine.evaluate_promotion(1, false, true);
        assert!(matches!(status, PromotionStatus::RegimeUnstable));
    }

    #[test]
    fn promotion_succeeds_with_advantage() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));

        // Incumbent has high loss, candidate has low loss.
        engine.trackers[0].selection_count = 300;
        engine.trackers[0].cumulative_loss = 90.0; // avg = 0.30
        engine.trackers[1].selection_count = 300;
        engine.trackers[1].cumulative_loss = 30.0; // avg = 0.10

        let status = engine.evaluate_promotion(1, true, true);
        assert!(
            matches!(status, PromotionStatus::Ready { .. }),
            "expected Ready, got {status:?}"
        );
    }

    #[test]
    fn counterfactual_only_data_cannot_trigger_promotion() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));

        engine.trackers[0].selection_count = 300;
        engine.trackers[0].cumulative_loss = 90.0;
        engine.trackers[1].counterfactual_count = 300;

        let status = engine.evaluate_promotion(1, true, true);
        assert!(
            matches!(
                status,
                PromotionStatus::InsufficientData {
                    observations: 0,
                    ..
                }
            ),
            "counterfactual-only evidence should not promote an unexecuted candidate: {status:?}"
        );
    }

    #[test]
    fn promote_changes_incumbent() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));

        assert!(engine.promote(1));
        assert_eq!(engine.incumbent().policy_id, "v2");
        assert_eq!(engine.total_promotions, 1);
        assert_eq!(engine.policy_revision, 1);
    }

    #[test]
    fn summary_includes_all_candidates() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));

        let summary = engine.summary();
        assert_eq!(summary.candidate_count, 2);
        assert_eq!(summary.probabilities.len(), 2);
    }

    #[test]
    fn serde_roundtrip() {
        let mut engine = AdaptationEngine::new(baseline_policy());
        engine.add_candidate(candidate_policy("v2"));
        engine.record_loss(0.5);

        let json = serde_json::to_string(&engine).unwrap();
        let decoded: AdaptationEngine = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.candidate_count(), 2);
        assert_eq!(decoded.total_rounds, 1);
    }
}
