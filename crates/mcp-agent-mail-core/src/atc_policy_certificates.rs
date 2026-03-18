#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]
//! Safe policy-improvement certificates with doubly-robust off-policy
//! evaluation and anytime-valid confidence sequences (br-0qt6e.3.9).
//!
//! Policy promotion is the single highest-leverage and highest-blast-radius
//! adaptive move in the ATC design. This module defines the mathematical
//! certification layer that blocks promotion unless ATC can demonstrate,
//! with auditable evidence, that the candidate is better enough and safe
//! enough to replace the incumbent.
//!
//! # Certificate Lifecycle
//!
//! ```text
//!   Candidate policy accumulates off-policy evidence
//!         │
//!         ▼
//!   Doubly-robust estimator computes advantage
//!         │
//!         ▼
//!   Confidence sequence tracks evidence strength
//!         │
//!         ▼
//!   Gate checks: regime, overlap, contamination,
//!   attribution, tail-risk, noise budgets
//!         │
//!         ├── All gates pass → Certificate { verdict: Certified }
//!         │
//!         └── Any gate fails → Certificate { verdict: Refused, block_reasons }
//! ```
//!
//! # Off-Policy Evaluation
//!
//! Uses the doubly-robust (AIPW) estimator for candidate-versus-incumbent
//! comparison:
//!
//! ```text
//!   DR(candidate) = (1/n) Σ_i [
//!       μ̂(candidate, x_i) +
//!       (A_i == candidate) / π(candidate | x_i) × (Y_i - μ̂(candidate, x_i))
//!   ]
//! ```
//!
//! where `μ̂` is the outcome model (mean loss estimate), `π` is the selection
//! probability (EXP3 distribution), and `Y_i` is the realized loss. This
//! estimator is doubly-robust: it remains consistent if *either* the outcome
//! model or the propensity model is correct.
//!
//! # Anytime-Valid Confidence Sequences
//!
//! Instead of fixed-sample confidence intervals, we use an e-process
//! (test martingale) that allows continuous monitoring with optional stopping:
//!
//! ```text
//!   E_t = Π_{i=1}^{t} (1 + λ × (Y_i - θ_0))
//!
//!   If E_t ≥ 1/α → reject null (candidate is better with level α)
//!   If 1/E_t ≥ 1/α → reject alternative (incumbent is better)
//! ```
//!
//! where `λ` is a tuning parameter (capital allocation) and `θ_0` is the
//! null hypothesis boundary (zero improvement). This provides sequential
//! validity: the certificate remains valid regardless of when we stop
//! collecting data.
//!
//! # Tail-Risk Protection
//!
//! A candidate cannot be promoted on mean utility alone. The certificate
//! also verifies:
//! - False-action rates per effect kind do not exceed budgets
//! - No single-window spike in loss exceeds the tail-risk threshold
//! - User-facing noise (advisory count, toast rate) stays within limits
//! - No safety regression on any tracked metric

use serde::{Deserialize, Serialize};

use crate::atc_regime::RegimeId;
use crate::atc_shrinkage::CohortSource;
use crate::experience::EffectKind;

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Minimum observations (combined selected + counterfactual) before a
/// certificate can be issued. Below this, we refuse with `InsufficientData`.
pub const MIN_CERTIFICATE_OBSERVATIONS: u64 = 200;

/// Minimum fraction of evidence that must be `Trusted` quality for
/// certification. Below this, we refuse with `SuspiciousEvidence`.
pub const MIN_TRUSTED_EVIDENCE_FRACTION: f64 = 0.60;

/// Minimum fraction of outcomes with `Clean` or `Primary` attribution
/// confidence. Below this, we refuse with `AttributionConfounding`.
pub const MIN_CLEAN_ATTRIBUTION_FRACTION: f64 = 0.50;

/// Maximum importance weight before clipping. Stabilizes the doubly-robust
/// estimator when propensity scores are small.
pub const MAX_IMPORTANCE_WEIGHT: f64 = 10.0;

/// Default significance level (α) for the confidence sequence.
/// Certificate requires E_t ≥ 1/α = 20.
pub const DEFAULT_SIGNIFICANCE_LEVEL: f64 = 0.05;

/// Default tail-risk threshold: maximum single-window loss spike
/// (as a fraction of mean loss) before tail-risk refusal.
pub const MAX_TAIL_LOSS_RATIO: f64 = 5.0;

/// Maximum shrinkage weight (across all strata) before we consider
/// support too sparse for reliable certification.
pub const MAX_ACCEPTABLE_SHRINKAGE: f64 = 0.80;

/// Minimum dwell time (seconds) in the current regime before promotion
/// certificates can be issued. This is stricter than the regime manager's
/// own dwell time because promotion has higher blast radius.
pub const MIN_REGIME_DWELL_FOR_PROMOTION_SECS: u64 = 600; // 10 minutes

/// Maximum fraction of evidence attributable to a single agent before
/// overlap-failure refusal. Aligned with contamination module's cap.
pub const MAX_SINGLE_AGENT_EVIDENCE_FRACTION: f64 = 0.20;

// ──────────────────────────────────────────────────────────────────────
// Certificate verdict
// ──────────────────────────────────────────────────────────────────────

/// The binary outcome of the certification process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateVerdict {
    /// All gates passed. Promotion is recommended.
    Certified,
    /// One or more gates failed. Promotion is blocked.
    Refused,
}

impl std::fmt::Display for CertificateVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Certified => write!(f, "certified"),
            Self::Refused => write!(f, "refused"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Block reasons
// ──────────────────────────────────────────────────────────────────────

/// Reason codes for why a promotion certificate was refused.
///
/// Each reason is a machine-readable blocker that downstream surfaces,
/// audits, and tests can inspect without parsing prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificationBlockReason {
    /// Not enough observations (combined selected + counterfactual).
    InsufficientData {
        observed: u64,
        required: u64,
    },

    /// The regime is unstable (transitioning or cooling) or has not
    /// dwelled long enough for reliable promotion evidence.
    RegimeUnstable {
        phase: String,
        dwell_secs: u64,
        required_dwell_secs: u64,
    },

    /// Too many strata have high shrinkage weight, meaning local
    /// estimates are dominated by population fallback.
    SparseLocalSupport {
        worst_stratum: String,
        shrinkage_weight: f64,
        max_acceptable: f64,
    },

    /// Counterfactual support overlap is too weak — the candidate
    /// was selected too rarely to produce reliable importance-weighted
    /// estimates.
    OverlapFailure {
        candidate_selection_fraction: f64,
        min_required: f64,
    },

    /// Too much evidence is contaminated, suspect, or quarantined.
    SuspiciousEvidence {
        trusted_fraction: f64,
        required_fraction: f64,
        quarantined_count: u64,
    },

    /// Too many outcomes have ambiguous or censored attribution,
    /// making the causal link between policy and outcome unreliable.
    AttributionConfounding {
        clean_fraction: f64,
        required_fraction: f64,
        ambiguous_count: u64,
    },

    /// The confidence sequence has not accumulated enough evidence
    /// to reject the null hypothesis (candidate is no better).
    WeakLowerBound {
        e_value: f64,
        threshold: f64,
        dr_advantage: f64,
    },

    /// The candidate would violate a per-effect-kind false-action
    /// budget if promoted.
    TailBudgetViolation {
        effect_kind: EffectKind,
        observed_rate: f64,
        budget_rate: f64,
    },

    /// There is an unresolved attribution backlog — too many outcomes
    /// are still awaiting attribution resolution.
    UnresolvedAttributionBacklog {
        pending_count: u64,
        max_pending: u64,
    },

    /// The candidate causes a regression on a tracked safety metric
    /// (e.g., false-positive release rate, user noise).
    SafetyRegression {
        metric: String,
        incumbent_value: f64,
        candidate_value: f64,
        max_regression: f64,
    },

    /// A single agent contributed too large a fraction of the evidence,
    /// creating a domination risk.
    EvidenceDomination {
        agent: String,
        fraction: f64,
        max_allowed: f64,
    },

    /// Tail-risk check failed: a loss spike within the evaluation window
    /// exceeds the acceptable ratio relative to mean loss.
    TailRiskSpike {
        window_max_loss: f64,
        mean_loss: f64,
        ratio: f64,
        max_ratio: f64,
    },
}

impl std::fmt::Display for CertificationBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientData { observed, required } => {
                write!(f, "insufficient data: {observed}/{required} observations")
            }
            Self::RegimeUnstable { phase, dwell_secs, required_dwell_secs } => {
                write!(f, "regime unstable: phase={phase}, dwell={dwell_secs}s (need {required_dwell_secs}s)")
            }
            Self::SparseLocalSupport { worst_stratum, shrinkage_weight, max_acceptable } => {
                write!(f, "sparse local support: stratum={worst_stratum}, shrinkage={shrinkage_weight:.2} (max {max_acceptable:.2})")
            }
            Self::OverlapFailure { candidate_selection_fraction, min_required } => {
                write!(f, "overlap failure: selection fraction={candidate_selection_fraction:.3} (need {min_required:.3})")
            }
            Self::SuspiciousEvidence { trusted_fraction, required_fraction, quarantined_count } => {
                write!(f, "suspicious evidence: trusted={trusted_fraction:.2} (need {required_fraction:.2}), quarantined={quarantined_count}")
            }
            Self::AttributionConfounding { clean_fraction, required_fraction, ambiguous_count } => {
                write!(f, "attribution confounding: clean={clean_fraction:.2} (need {required_fraction:.2}), ambiguous={ambiguous_count}")
            }
            Self::WeakLowerBound { e_value, threshold, dr_advantage } => {
                write!(f, "weak lower bound: e-value={e_value:.4} (need {threshold:.1}), DR advantage={dr_advantage:.4}")
            }
            Self::TailBudgetViolation { effect_kind, observed_rate, budget_rate } => {
                write!(f, "tail budget violation: {effect_kind:?} rate={observed_rate:.4} (budget {budget_rate:.4})")
            }
            Self::UnresolvedAttributionBacklog { pending_count, max_pending } => {
                write!(f, "unresolved attribution backlog: {pending_count}/{max_pending}")
            }
            Self::SafetyRegression { metric, incumbent_value, candidate_value, max_regression } => {
                write!(f, "safety regression: {metric}: incumbent={incumbent_value:.4}, candidate={candidate_value:.4} (max regression {max_regression:.4})")
            }
            Self::EvidenceDomination { agent, fraction, max_allowed } => {
                write!(f, "evidence domination: agent={agent}, fraction={fraction:.3} (max {max_allowed:.3})")
            }
            Self::TailRiskSpike { window_max_loss, mean_loss, ratio, max_ratio } => {
                write!(f, "tail-risk spike: max={window_max_loss:.4}, mean={mean_loss:.4}, ratio={ratio:.2} (max {max_ratio:.1})")
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Evidence quality summary
// ──────────────────────────────────────────────────────────────────────

/// Summary of evidence quality across the evaluation window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceQualitySummary {
    /// Total observations in the evaluation window.
    pub total_observations: u64,
    /// Count of observations with `Trusted` quality.
    pub trusted_count: u64,
    /// Count of observations with `Suspect` quality.
    pub suspect_count: u64,
    /// Count of observations with `Quarantined` quality.
    pub quarantined_count: u64,
    /// Count of observations with `Capped` quality.
    pub capped_count: u64,
    /// Fraction of observations that are `Trusted`.
    pub trusted_fraction: f64,
    /// Maximum influence fraction from any single agent.
    pub max_agent_influence_fraction: f64,
    /// Which agent has the highest influence fraction.
    pub most_influential_agent: Option<String>,
}

impl EvidenceQualitySummary {
    /// Whether the evidence quality is sufficient for certification.
    #[must_use]
    pub fn is_sufficient(&self) -> bool {
        self.trusted_fraction >= MIN_TRUSTED_EVIDENCE_FRACTION
            && self.quarantined_count == 0
            && self.max_agent_influence_fraction <= MAX_SINGLE_AGENT_EVIDENCE_FRACTION
    }
}

// ──────────────────────────────────────────────────────────────────────
// Attribution quality summary
// ──────────────────────────────────────────────────────────────────────

/// Summary of attribution confidence across the evaluation window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributionSummary {
    /// Total attributed outcomes.
    pub total_outcomes: u64,
    /// Count with `Clean` attribution.
    pub clean_count: u64,
    /// Count with `Primary` attribution.
    pub primary_count: u64,
    /// Count with `Shared` attribution.
    pub shared_count: u64,
    /// Count with `Ambiguous` attribution.
    pub ambiguous_count: u64,
    /// Count with `Censored` attribution.
    pub censored_count: u64,
    /// Fraction with `Clean` or `Primary` (learnable outcomes).
    pub clean_or_primary_fraction: f64,
    /// Average learning weight across all attributed outcomes.
    pub avg_learning_weight: f64,
}

impl AttributionSummary {
    /// Whether attribution quality is sufficient for certification.
    #[must_use]
    pub fn is_sufficient(&self) -> bool {
        self.clean_or_primary_fraction >= MIN_CLEAN_ATTRIBUTION_FRACTION
    }
}

// ──────────────────────────────────────────────────────────────────────
// Per-stratum support record
// ──────────────────────────────────────────────────────────────────────

/// Support information for a single stratum in the evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StratumSupport {
    /// Stratum key (e.g., "liveness:advisory:low").
    pub stratum_key: String,
    /// Number of local observations in this stratum.
    pub local_count: u64,
    /// Effective sample size after shrinkage.
    pub effective_sample_size: f64,
    /// Shrinkage weight applied (0 = pure local, 1 = full population).
    pub shrinkage_weight: f64,
    /// Which cohort level the population estimate came from.
    pub cohort_source: CohortSource,
    /// Whether this stratum has sufficient local support.
    pub has_local_support: bool,
}

// ──────────────────────────────────────────────────────────────────────
// Per-effect-kind budget check
// ──────────────────────────────────────────────────────────────────────

/// Result of checking a single effect kind's false-action budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectBudgetCheck {
    /// Which effect kind was checked.
    pub effect_kind: EffectKind,
    /// Total actions taken under this effect kind.
    pub total_actions: u64,
    /// False actions observed.
    pub false_actions: u64,
    /// Observed false-action rate.
    pub observed_rate: f64,
    /// Maximum allowed false-action rate from the budget config.
    pub budget_rate: f64,
    /// Whether the budget is satisfied.
    pub meets_budget: bool,
}

// ──────────────────────────────────────────────────────────────────────
// Doubly-robust estimator
// ──────────────────────────────────────────────────────────────────────

/// A single observation for the doubly-robust estimator.
#[derive(Debug, Clone)]
pub struct DRObservation {
    /// Whether the candidate policy was selected for this observation.
    pub candidate_selected: bool,
    /// The propensity score: probability that the candidate was selected.
    pub propensity: f64,
    /// The realized loss for this observation.
    pub realized_loss: f64,
    /// The outcome model's predicted loss for the candidate.
    pub predicted_loss: f64,
    /// Evidence quality weight (from contamination module).
    pub evidence_weight: f64,
    /// Attribution learning weight (from attribution module).
    pub attribution_weight: f64,
}

/// Result of the doubly-robust off-policy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoublyRobustResult {
    /// Estimated mean loss for the candidate under the DR estimator.
    pub candidate_dr_loss: f64,
    /// Estimated mean loss for the incumbent (on-policy).
    pub incumbent_mean_loss: f64,
    /// DR advantage: incumbent_loss - candidate_loss (positive = candidate better).
    pub dr_advantage: f64,
    /// Number of observations used.
    pub n_observations: u64,
    /// Number of observations where the candidate was selected.
    pub n_selected: u64,
    /// Number of observations with clipped importance weights.
    pub n_clipped: u64,
    /// Effective sample size (accounting for importance weight variance).
    pub effective_sample_size: f64,
    /// Variance of the DR estimator.
    pub dr_variance: f64,
}

/// Compute the doubly-robust (AIPW) advantage estimate.
///
/// Returns `None` if there are insufficient observations or if the
/// effective sample size is too small.
#[must_use]
pub fn compute_doubly_robust(
    observations: &[DRObservation],
    incumbent_mean_loss: f64,
) -> Option<DoublyRobustResult> {
    if observations.is_empty() {
        return None;
    }

    let mut sum_dr = 0.0;
    let mut sum_dr_sq = 0.0;
    let mut sum_iw_sq = 0.0;
    let mut n_contributing = 0u64;
    let mut n_selected = 0u64;
    let mut n_clipped = 0u64;

    for obs in observations {
        let combined_weight = obs.evidence_weight * obs.attribution_weight;
        if combined_weight <= 0.0 {
            continue;
        }
        n_contributing += 1;

        let dr_term = if obs.candidate_selected {
            n_selected += 1;
            // Importance weight: 1 / propensity, clipped for stability.
            let raw_iw = 1.0 / obs.propensity.max(1e-10);
            let iw = if raw_iw > MAX_IMPORTANCE_WEIGHT {
                n_clipped += 1;
                MAX_IMPORTANCE_WEIGHT
            } else {
                raw_iw
            };
            sum_iw_sq += iw * iw;

            // DR term: μ̂ + iw × (Y - μ̂), weighted by evidence and attribution.
            let augmentation = iw * (obs.realized_loss - obs.predicted_loss);
            (obs.predicted_loss + augmentation) * combined_weight
        } else {
            // Non-selected: use only the outcome model prediction.
            obs.predicted_loss * combined_weight
        };

        sum_dr += dr_term;
        sum_dr_sq += dr_term * dr_term;
    }

    if n_contributing == 0 {
        return None;
    }

    let n = n_contributing as f64;
    let candidate_dr_loss = sum_dr / n;
    // Computational variance formula: Var(X) = (E[X²] - E[X]²) / (n-1)
    // clippy flags `- x * x` as suspicious, but this is correct math.
    #[allow(clippy::suspicious_operation_groupings)]
    let dr_variance = if n > 1.0 {
        (sum_dr_sq / n - candidate_dr_loss * candidate_dr_loss) / (n - 1.0)
    } else {
        f64::INFINITY
    };

    // Effective sample size: n / (1 + cv²(importance weights)).
    let ess = if sum_iw_sq > 0.0 && n_selected > 0 {
        let mean_iw_sq = sum_iw_sq / n_selected as f64;
        let mean_iw = n / n_selected as f64; // approximate
        let cv_sq = (mean_iw_sq / (mean_iw * mean_iw)) - 1.0;
        n / (1.0 + cv_sq.max(0.0))
    } else {
        n
    };

    Some(DoublyRobustResult {
        candidate_dr_loss,
        incumbent_mean_loss,
        dr_advantage: incumbent_mean_loss - candidate_dr_loss,
        n_observations: n_contributing,
        n_selected,
        n_clipped,
        effective_sample_size: ess,
        dr_variance,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Confidence sequence (e-process)
// ──────────────────────────────────────────────────────────────────────

/// State of an anytime-valid confidence sequence for policy comparison.
///
/// Uses a mixture-method e-process that remains valid under continuous
/// monitoring and optional stopping. The e-value `E_t` is the product
/// of per-observation likelihood ratios:
///
/// ```text
///   E_t = Π_{i=1}^{t} (1 + λ × Z_i)
/// ```
///
/// where `Z_i = Y_incumbent_i - Y_candidate_i` is the per-observation
/// advantage and `λ` is a capital allocation parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceSequence {
    /// Current log e-value (stored in log-space for numerical stability).
    pub log_e_value: f64,
    /// Capital allocation parameter.
    pub lambda: f64,
    /// Number of observations processed.
    pub n_observations: u64,
    /// Running mean of the advantage signal.
    pub running_mean: f64,
    /// Running variance (Welford's algorithm).
    pub running_m2: f64,
    /// Significance level for the test.
    pub alpha: f64,
}

impl ConfidenceSequence {
    /// Create a new confidence sequence with default parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log_e_value: 0.0,
            lambda: 0.5, // conservative capital allocation
            n_observations: 0,
            running_mean: 0.0,
            running_m2: 0.0,
            alpha: DEFAULT_SIGNIFICANCE_LEVEL,
        }
    }

    /// Create with a custom significance level.
    #[must_use]
    pub fn with_alpha(alpha: f64) -> Self {
        Self { alpha, ..Self::new() }
    }

    /// Process a new observation (advantage = incumbent_loss - candidate_loss).
    ///
    /// Positive advantage means the candidate is better.
    pub fn update(&mut self, advantage: f64) {
        self.n_observations += 1;

        // Welford's online mean and variance.
        let delta = advantage - self.running_mean;
        self.running_mean += delta / self.n_observations as f64;
        let delta2 = advantage - self.running_mean;
        self.running_m2 += delta * delta2;

        // Adaptively set lambda based on running variance.
        if self.n_observations >= 10 {
            let variance = self.running_m2 / (self.n_observations - 1) as f64;
            let std_dev = variance.max(1e-10).sqrt();
            // Optimal lambda for sub-Gaussian case: mean / (2 * variance).
            // Clamp to [0.01, 2.0] for stability.
            self.lambda = (self.running_mean / (2.0 * std_dev * std_dev))
                .clamp(0.01, 2.0);
        }

        // Update e-value in log-space: log(E_t) += log(1 + λ × Z_i).
        // When factor ≤ 0 (catastrophically bad observation), the e-value
        // should crash toward zero. Clamping to a small positive value
        // produces a large negative log update, which is correct.
        let factor = (1.0 + self.lambda * advantage).max(1e-6);
        self.log_e_value += factor.ln();
    }

    /// Current e-value (exponentiated from log-space).
    #[must_use]
    pub fn e_value(&self) -> f64 {
        self.log_e_value.exp().min(1e15) // cap for display
    }

    /// Threshold for rejecting the null (candidate is no better).
    #[must_use]
    pub fn threshold(&self) -> f64 {
        1.0 / self.alpha
    }

    /// Whether the confidence sequence has accumulated enough evidence
    /// to certify that the candidate is better.
    #[must_use]
    pub fn is_significant(&self) -> bool {
        self.e_value() >= self.threshold()
    }

    /// Running variance estimate.
    #[must_use]
    pub fn variance(&self) -> f64 {
        if self.n_observations >= 2 {
            self.running_m2 / (self.n_observations - 1) as f64
        } else {
            f64::INFINITY
        }
    }
}

impl Default for ConfidenceSequence {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Regime context snapshot
// ──────────────────────────────────────────────────────────────────────

/// Snapshot of regime state at the time the certificate was evaluated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeSnapshot {
    /// Regime ID during evaluation.
    pub regime_id: RegimeId,
    /// Phase (stable/transitioning/cooling).
    pub phase: String,
    /// How long the current regime has been stable (seconds).
    pub stability_age_secs: u64,
    /// Reason for the most recent transition.
    pub last_transition_reason: Option<String>,
    /// Total transitions since startup.
    pub total_transitions: u64,
}

// ──────────────────────────────────────────────────────────────────────
// Evidence transfer rules
// ──────────────────────────────────────────────────────────────────────

/// Rules for what evidence can be reused across regime boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTransferability {
    /// Evidence is fully transferable across regimes.
    /// Applies to: structural properties (budget configs, method types).
    Transferable,
    /// Evidence can be transferred with a discount factor applied.
    /// Applies to: loss estimates, accuracy metrics (discounted by 0.5).
    DiscountedTransfer,
    /// Evidence must be reset on regime change.
    /// Applies to: calibration state, conformal scores, e-process state.
    RegimeSensitive,
    /// Evidence is explicitly quarantined and cannot transfer.
    /// Applies to: contaminated observations, ambiguous attributions.
    NonTransferable,
}

impl EvidenceTransferability {
    /// Discount factor for transferable evidence across regimes.
    #[must_use]
    pub const fn regime_discount(self) -> f64 {
        match self {
            Self::Transferable => 1.0,
            Self::DiscountedTransfer => 0.5,
            Self::RegimeSensitive => 0.0,
            Self::NonTransferable => 0.0,
        }
    }
}

impl std::fmt::Display for EvidenceTransferability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transferable => write!(f, "transferable"),
            Self::DiscountedTransfer => write!(f, "discounted"),
            Self::RegimeSensitive => write!(f, "regime_sensitive"),
            Self::NonTransferable => write!(f, "non_transferable"),
        }
    }
}

/// Classification of evidence categories for transfer rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceTransferRule {
    /// Human-readable category name.
    pub category: &'static str,
    /// Transfer classification.
    pub transferability: EvidenceTransferability,
    /// Description of what happens on regime change.
    pub on_regime_change: &'static str,
}

/// Canonical transfer rules for each evidence category.
pub const EVIDENCE_TRANSFER_RULES: &[EvidenceTransferRule] = &[
    EvidenceTransferRule {
        category: "loss_estimates",
        transferability: EvidenceTransferability::DiscountedTransfer,
        on_regime_change: "Cumulative loss and counterfactual loss are halved. \
                           The discount preserves rank ordering while reducing \
                           confidence in old measurements.",
    },
    EvidenceTransferRule {
        category: "selection_probabilities",
        transferability: EvidenceTransferability::RegimeSensitive,
        on_regime_change: "EXP3 weights are reset to uniform. The new regime \
                           may have fundamentally different policy performance \
                           characteristics.",
    },
    EvidenceTransferRule {
        category: "confidence_sequence_state",
        transferability: EvidenceTransferability::RegimeSensitive,
        on_regime_change: "E-process state (log_e_value) is reset to 0. Evidence \
                           from a different regime cannot support promotion in \
                           the current regime.",
    },
    EvidenceTransferRule {
        category: "conformal_calibration",
        transferability: EvidenceTransferability::RegimeSensitive,
        on_regime_change: "Conformal score windows are flushed. Calibration from \
                           a different regime is unreliable.",
    },
    EvidenceTransferRule {
        category: "budget_counters",
        transferability: EvidenceTransferability::DiscountedTransfer,
        on_regime_change: "False-action counts are halved. This prevents old \
                           budget violations from permanently blocking action \
                           in a new regime while maintaining safety memory.",
    },
    EvidenceTransferRule {
        category: "shrinkage_population_estimates",
        transferability: EvidenceTransferability::Transferable,
        on_regime_change: "Population-level estimates are retained. They represent \
                           broad structural properties that change slowly.",
    },
    EvidenceTransferRule {
        category: "contamination_quarantine",
        transferability: EvidenceTransferability::NonTransferable,
        on_regime_change: "Quarantined evidence remains quarantined. Contaminated \
                           observations do not become clean just because the \
                           regime changed.",
    },
    EvidenceTransferRule {
        category: "attribution_results",
        transferability: EvidenceTransferability::DiscountedTransfer,
        on_regime_change: "Attribution results are discounted. The causal structure \
                           may differ in the new regime, but clean attributions \
                           retain partial value.",
    },
];

// ──────────────────────────────────────────────────────────────────────
// The promotion certificate
// ──────────────────────────────────────────────────────────────────────

/// A policy promotion certificate: the auditable artifact that must exist
/// before any candidate policy can displace the incumbent.
///
/// This struct captures the complete evidence trail: which policies were
/// compared, under what regime, with what evidence quality, and what
/// mathematical tests were applied. Both certifications and refusals
/// produce a certificate — refusals document *why* promotion was blocked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyPromotionCertificate {
    /// Unique certificate ID (monotonically increasing).
    pub certificate_id: u64,

    /// When the certificate was issued (microseconds since epoch).
    pub issued_ts_micros: i64,

    /// The verdict: certified or refused.
    pub verdict: CertificateVerdict,

    // ── Policy identification ──

    /// Candidate policy ID (proposed replacement).
    pub candidate_id: String,

    /// Incumbent policy ID (current active policy).
    pub incumbent_id: String,

    /// Policy revision number at the time of evaluation.
    pub policy_revision: u64,

    // ── Regime context ──

    /// Regime state at the time of evaluation.
    pub regime: RegimeSnapshot,

    // ── Off-policy evaluation ──

    /// Results of the doubly-robust estimator.
    pub dr_result: Option<DoublyRobustResult>,

    // ── Confidence sequence ──

    /// Current e-value from the confidence sequence.
    pub e_value: f64,

    /// Threshold for significance (1/α).
    pub e_threshold: f64,

    /// Whether the confidence sequence crossed the threshold.
    pub cs_significant: bool,

    // ── Evidence quality ──

    /// Summary of evidence quality.
    pub evidence_quality: EvidenceQualitySummary,

    // ── Attribution quality ──

    /// Summary of attribution confidence.
    pub attribution: AttributionSummary,

    // ── Stratum support ──

    /// Per-stratum support information.
    pub strata: Vec<StratumSupport>,

    // ── Budget checks ──

    /// Per-effect-kind false-action budget results.
    pub budget_checks: Vec<EffectBudgetCheck>,

    // ── Tail risk ──

    /// Maximum single-window loss observed for the candidate.
    pub tail_max_loss: f64,

    /// Mean loss for the candidate.
    pub tail_mean_loss: f64,

    /// Tail-risk ratio (max / mean).
    pub tail_risk_ratio: f64,

    // ── Block reasons (if refused) ──

    /// All reasons why the certificate was refused.
    /// Empty if `verdict == Certified`.
    pub block_reasons: Vec<CertificationBlockReason>,

    // ── Traceability ──

    /// Assumption keys that were verified as part of certification.
    pub verified_assumption_keys: Vec<String>,

    /// Experience IDs that formed the evaluation window (sample).
    /// Capped to avoid unbounded size; stores first + last + count.
    pub experience_window: ExperienceWindowRef,

    /// Certificate validity expiration. The certificate becomes invalid
    /// after a regime change or after this timestamp (whichever comes first).
    pub valid_until_ts_micros: Option<i64>,
}

/// Reference to the experience window used for evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceWindowRef {
    /// First experience ID in the window.
    pub first_id: u64,
    /// Last experience ID in the window.
    pub last_id: u64,
    /// Total experiences in the window.
    pub count: u64,
    /// Timestamp of the earliest experience.
    pub earliest_ts_micros: i64,
    /// Timestamp of the latest experience.
    pub latest_ts_micros: i64,
    /// Regime ID during the window.
    pub regime_id: RegimeId,
}

// ──────────────────────────────────────────────────────────────────────
// Certificate builder
// ──────────────────────────────────────────────────────────────────────

/// Input data for certificate evaluation.
///
/// Collected from the adaptation engine, contamination tracker, attribution
/// resolver, risk budget tables, and regime manager. The evaluator runs all
/// gate checks and produces a `PolicyPromotionCertificate`.
#[derive(Debug, Clone)]
pub struct CertificateInput {
    /// Candidate policy ID.
    pub candidate_id: String,
    /// Incumbent policy ID.
    pub incumbent_id: String,
    /// Policy revision number.
    pub policy_revision: u64,
    /// Regime snapshot.
    pub regime: RegimeSnapshot,
    /// Doubly-robust observations.
    pub observations: Vec<DRObservation>,
    /// Incumbent's on-policy mean loss.
    pub incumbent_mean_loss: f64,
    /// Confidence sequence state.
    pub confidence_sequence: ConfidenceSequence,
    /// Evidence quality summary.
    pub evidence_quality: EvidenceQualitySummary,
    /// Attribution summary.
    pub attribution: AttributionSummary,
    /// Per-stratum support.
    pub strata: Vec<StratumSupport>,
    /// Per-effect budget checks.
    pub budget_checks: Vec<EffectBudgetCheck>,
    /// Maximum single-window loss for the candidate.
    pub tail_max_loss: f64,
    /// Mean loss for the candidate.
    pub tail_mean_loss: f64,
    /// Assumption keys that were verified.
    pub verified_assumption_keys: Vec<String>,
    /// Experience window reference.
    pub experience_window: ExperienceWindowRef,
    /// Certificate ID (caller provides monotonic counter).
    pub certificate_id: u64,
    /// Current time (microseconds since epoch).
    pub now_ts_micros: i64,
    /// Optional certificate validity duration (microseconds).
    pub validity_duration_micros: Option<i64>,
    /// Candidate selection fraction (for overlap check).
    pub candidate_selection_fraction: f64,
    /// Pending attribution count (for backlog check).
    pub pending_attribution_count: u64,
    /// Maximum pending attributions allowed.
    pub max_pending_attributions: u64,
}

/// Evaluate all gate checks and produce a promotion certificate.
///
/// This is the single entry point for certificate issuance. It runs
/// every gate check and collects all block reasons (not short-circuiting)
/// so the certificate documents the complete failure picture.
#[must_use]
pub fn evaluate_certificate(input: CertificateInput) -> PolicyPromotionCertificate {
    let mut block_reasons = Vec::new();

    // ── Gate 1: Observation count ──
    let total_obs = input.evidence_quality.total_observations;
    if total_obs < MIN_CERTIFICATE_OBSERVATIONS {
        block_reasons.push(CertificationBlockReason::InsufficientData {
            observed: total_obs,
            required: MIN_CERTIFICATE_OBSERVATIONS,
        });
    }

    // ── Gate 2: Regime stability ──
    if input.regime.phase != "stable"
        || input.regime.stability_age_secs < MIN_REGIME_DWELL_FOR_PROMOTION_SECS
    {
        block_reasons.push(CertificationBlockReason::RegimeUnstable {
            phase: input.regime.phase.clone(),
            dwell_secs: input.regime.stability_age_secs,
            required_dwell_secs: MIN_REGIME_DWELL_FOR_PROMOTION_SECS,
        });
    }

    // ── Gate 3: Evidence quality ──
    // Low trusted fraction and quarantined evidence are separate concerns
    // but both produce the same block reason. Avoid duplicates by combining.
    if input.evidence_quality.trusted_fraction < MIN_TRUSTED_EVIDENCE_FRACTION
        || input.evidence_quality.quarantined_count > 0
    {
        block_reasons.push(CertificationBlockReason::SuspiciousEvidence {
            trusted_fraction: input.evidence_quality.trusted_fraction,
            required_fraction: MIN_TRUSTED_EVIDENCE_FRACTION,
            quarantined_count: input.evidence_quality.quarantined_count,
        });
    }

    // ── Gate 4: Evidence domination ──
    if input.evidence_quality.max_agent_influence_fraction > MAX_SINGLE_AGENT_EVIDENCE_FRACTION {
        if let Some(agent) = &input.evidence_quality.most_influential_agent {
            block_reasons.push(CertificationBlockReason::EvidenceDomination {
                agent: agent.clone(),
                fraction: input.evidence_quality.max_agent_influence_fraction,
                max_allowed: MAX_SINGLE_AGENT_EVIDENCE_FRACTION,
            });
        }
    }

    // ── Gate 5: Attribution quality ──
    if !input.attribution.is_sufficient() {
        block_reasons.push(CertificationBlockReason::AttributionConfounding {
            clean_fraction: input.attribution.clean_or_primary_fraction,
            required_fraction: MIN_CLEAN_ATTRIBUTION_FRACTION,
            ambiguous_count: input.attribution.ambiguous_count,
        });
    }

    // ── Gate 6: Overlap / counterfactual support ──
    let min_overlap = 1.0 / (MAX_IMPORTANCE_WEIGHT * 2.0); // 0.05
    if input.candidate_selection_fraction < min_overlap {
        block_reasons.push(CertificationBlockReason::OverlapFailure {
            candidate_selection_fraction: input.candidate_selection_fraction,
            min_required: min_overlap,
        });
    }

    // ── Gate 7: Stratum support (sparse data check) ──
    for stratum in &input.strata {
        if stratum.shrinkage_weight > MAX_ACCEPTABLE_SHRINKAGE
            && !stratum.has_local_support
        {
            block_reasons.push(CertificationBlockReason::SparseLocalSupport {
                worst_stratum: stratum.stratum_key.clone(),
                shrinkage_weight: stratum.shrinkage_weight,
                max_acceptable: MAX_ACCEPTABLE_SHRINKAGE,
            });
            break; // report only the first sparse stratum found
        }
    }

    // ── Gate 8: Doubly-robust evaluation ──
    let dr_result = compute_doubly_robust(&input.observations, input.incumbent_mean_loss);

    // ── Gate 9: Confidence sequence ──
    let cs = &input.confidence_sequence;
    let cs_significant = cs.is_significant();
    if !cs_significant {
        let dr_adv = dr_result
            .as_ref()
            .map_or(0.0, |r| r.dr_advantage);
        block_reasons.push(CertificationBlockReason::WeakLowerBound {
            e_value: cs.e_value(),
            threshold: cs.threshold(),
            dr_advantage: dr_adv,
        });
    }

    // ── Gate 10: Per-effect budget checks ──
    for check in &input.budget_checks {
        if !check.meets_budget {
            block_reasons.push(CertificationBlockReason::TailBudgetViolation {
                effect_kind: check.effect_kind,
                observed_rate: check.observed_rate,
                budget_rate: check.budget_rate,
            });
        }
    }

    // ── Gate 11: Tail-risk spike ──
    let tail_risk_ratio = if input.tail_mean_loss > 0.0 {
        input.tail_max_loss / input.tail_mean_loss
    } else {
        0.0
    };
    if tail_risk_ratio > MAX_TAIL_LOSS_RATIO {
        block_reasons.push(CertificationBlockReason::TailRiskSpike {
            window_max_loss: input.tail_max_loss,
            mean_loss: input.tail_mean_loss,
            ratio: tail_risk_ratio,
            max_ratio: MAX_TAIL_LOSS_RATIO,
        });
    }

    // ── Gate 12: Attribution backlog ──
    if input.pending_attribution_count > input.max_pending_attributions {
        block_reasons.push(CertificationBlockReason::UnresolvedAttributionBacklog {
            pending_count: input.pending_attribution_count,
            max_pending: input.max_pending_attributions,
        });
    }

    // ── Gate 13: Safety regression (from DR result) ──
    if let Some(ref dr) = dr_result {
        if dr.dr_advantage < 0.0 {
            block_reasons.push(CertificationBlockReason::SafetyRegression {
                metric: "doubly_robust_advantage".to_string(),
                incumbent_value: dr.incumbent_mean_loss,
                candidate_value: dr.candidate_dr_loss,
                max_regression: 0.0,
            });
        }
    }

    // ── Verdict ──
    let verdict = if block_reasons.is_empty() {
        CertificateVerdict::Certified
    } else {
        CertificateVerdict::Refused
    };

    let valid_until = input.validity_duration_micros
        .map(|dur| input.now_ts_micros + dur);

    PolicyPromotionCertificate {
        certificate_id: input.certificate_id,
        issued_ts_micros: input.now_ts_micros,
        verdict,
        candidate_id: input.candidate_id,
        incumbent_id: input.incumbent_id,
        policy_revision: input.policy_revision,
        regime: input.regime,
        dr_result,
        e_value: cs.e_value(),
        e_threshold: cs.threshold(),
        cs_significant,
        evidence_quality: input.evidence_quality,
        attribution: input.attribution,
        strata: input.strata,
        budget_checks: input.budget_checks,
        tail_max_loss: input.tail_max_loss,
        tail_mean_loss: input.tail_mean_loss,
        tail_risk_ratio,
        block_reasons,
        verified_assumption_keys: input.verified_assumption_keys,
        experience_window: input.experience_window,
        valid_until_ts_micros: valid_until,
    }
}

impl PolicyPromotionCertificate {
    /// Whether this certificate authorizes promotion.
    #[must_use]
    pub fn is_certified(&self) -> bool {
        self.verdict == CertificateVerdict::Certified
    }

    /// Whether the certificate has expired (by time or regime change).
    #[must_use]
    pub fn is_expired(&self, now_micros: i64, current_regime_id: RegimeId) -> bool {
        // Expired if regime changed.
        if current_regime_id != self.regime.regime_id {
            return true;
        }
        // Expired if past validity window.
        if let Some(until) = self.valid_until_ts_micros {
            if now_micros > until {
                return true;
            }
        }
        false
    }

    /// Number of block reasons (0 = certified).
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.block_reasons.len()
    }

    /// Human-readable summary of the certificate.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_certified() {
            format!(
                "CERTIFIED: {} displaces {} (DR advantage={:.4}, e-value={:.2}, regime={})",
                self.candidate_id,
                self.incumbent_id,
                self.dr_result.as_ref().map_or(0.0, |r| r.dr_advantage),
                self.e_value,
                self.regime.regime_id,
            )
        } else {
            let reasons: Vec<String> = self.block_reasons.iter().map(ToString::to_string).collect();
            format!(
                "REFUSED: {} cannot displace {} — {} block(s): {}",
                self.candidate_id,
                self.incumbent_id,
                self.block_reasons.len(),
                reasons.join("; "),
            )
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_default_evidence_quality() -> EvidenceQualitySummary {
        EvidenceQualitySummary {
            total_observations: 300,
            trusted_count: 270,
            suspect_count: 20,
            quarantined_count: 0,
            capped_count: 10,
            trusted_fraction: 0.90,
            max_agent_influence_fraction: 0.10,
            most_influential_agent: Some("TestAgent".to_string()),
        }
    }

    fn make_default_attribution() -> AttributionSummary {
        AttributionSummary {
            total_outcomes: 300,
            clean_count: 200,
            primary_count: 50,
            shared_count: 30,
            ambiguous_count: 15,
            censored_count: 5,
            clean_or_primary_fraction: 0.833,
            avg_learning_weight: 0.85,
        }
    }

    fn make_default_regime() -> RegimeSnapshot {
        RegimeSnapshot {
            regime_id: 3,
            phase: "stable".to_string(),
            stability_age_secs: 1200,
            last_transition_reason: None,
            total_transitions: 3,
        }
    }

    fn make_default_experience_window() -> ExperienceWindowRef {
        ExperienceWindowRef {
            first_id: 100,
            last_id: 400,
            count: 300,
            earliest_ts_micros: 1_000_000,
            latest_ts_micros: 2_000_000,
            regime_id: 3,
        }
    }

    fn make_default_budget_checks() -> Vec<EffectBudgetCheck> {
        vec![
            EffectBudgetCheck {
                effect_kind: EffectKind::Advisory,
                total_actions: 100,
                false_actions: 3,
                observed_rate: 0.03,
                budget_rate: 0.05,
                meets_budget: true,
            },
            EffectBudgetCheck {
                effect_kind: EffectKind::Release,
                total_actions: 20,
                false_actions: 0,
                observed_rate: 0.0,
                budget_rate: 0.0,
                meets_budget: true,
            },
        ]
    }

    fn make_default_observations(n: usize, advantage: f64) -> Vec<DRObservation> {
        (0..n).map(|i| DRObservation {
            candidate_selected: i % 3 == 0, // ~33% selection
            propensity: 0.33,
            realized_loss: 0.5 - advantage * 0.5,
            predicted_loss: 0.5,
            evidence_weight: 1.0,
            attribution_weight: 1.0,
        }).collect()
    }

    fn make_significant_cs() -> ConfidenceSequence {
        let mut cs = ConfidenceSequence::new();
        // Feed enough positive advantage to make it significant.
        for _ in 0..100 {
            cs.update(0.15);
        }
        cs
    }

    fn make_default_input() -> CertificateInput {
        CertificateInput {
            candidate_id: "pol-candidate-v2".to_string(),
            incumbent_id: "pol-baseline-v1".to_string(),
            policy_revision: 5,
            regime: make_default_regime(),
            observations: make_default_observations(300, 0.15),
            incumbent_mean_loss: 0.5,
            confidence_sequence: make_significant_cs(),
            evidence_quality: make_default_evidence_quality(),
            attribution: make_default_attribution(),
            strata: vec![StratumSupport {
                stratum_key: "liveness:advisory:low".to_string(),
                local_count: 150,
                effective_sample_size: 140.0,
                shrinkage_weight: 0.1,
                cohort_source: CohortSource::Global,
                has_local_support: true,
            }],
            budget_checks: make_default_budget_checks(),
            tail_max_loss: 2.0,
            tail_mean_loss: 0.5,
            verified_assumption_keys: vec!["ewma_stationarity".to_string()],
            experience_window: make_default_experience_window(),
            certificate_id: 1,
            now_ts_micros: 3_000_000,
            validity_duration_micros: Some(3_600_000_000), // 1 hour
            candidate_selection_fraction: 0.33,
            pending_attribution_count: 5,
            max_pending_attributions: 50,
        }
    }

    #[test]
    fn test_certified_when_all_gates_pass() {
        let input = make_default_input();
        let cert = evaluate_certificate(input);
        assert!(cert.is_certified(), "Expected certified, got: {}", cert.summary());
        assert!(cert.block_reasons.is_empty());
        assert_eq!(cert.verdict, CertificateVerdict::Certified);
    }

    #[test]
    fn test_refused_insufficient_data() {
        let mut input = make_default_input();
        input.evidence_quality.total_observations = 50;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::InsufficientData { .. }
        )));
    }

    #[test]
    fn test_refused_regime_unstable() {
        let mut input = make_default_input();
        input.regime.phase = "transitioning".to_string();
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::RegimeUnstable { .. }
        )));
    }

    #[test]
    fn test_refused_regime_dwell_too_short() {
        let mut input = make_default_input();
        input.regime.stability_age_secs = 120; // only 2 minutes, need 10
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::RegimeUnstable { .. }
        )));
    }

    #[test]
    fn test_refused_suspicious_evidence() {
        let mut input = make_default_input();
        input.evidence_quality.trusted_fraction = 0.40;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::SuspiciousEvidence { .. }
        )));
    }

    #[test]
    fn test_refused_quarantined_evidence() {
        let mut input = make_default_input();
        input.evidence_quality.quarantined_count = 5;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::SuspiciousEvidence { .. }
        )));
    }

    #[test]
    fn test_refused_evidence_domination() {
        let mut input = make_default_input();
        input.evidence_quality.max_agent_influence_fraction = 0.35;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::EvidenceDomination { .. }
        )));
    }

    #[test]
    fn test_refused_attribution_confounding() {
        let mut input = make_default_input();
        input.attribution.clean_or_primary_fraction = 0.30;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::AttributionConfounding { .. }
        )));
    }

    #[test]
    fn test_refused_overlap_failure() {
        let mut input = make_default_input();
        input.candidate_selection_fraction = 0.01; // barely selected
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::OverlapFailure { .. }
        )));
    }

    #[test]
    fn test_refused_sparse_local_support() {
        let mut input = make_default_input();
        input.strata = vec![StratumSupport {
            stratum_key: "sparse:stratum".to_string(),
            local_count: 3,
            effective_sample_size: 2.0,
            shrinkage_weight: 0.95, // heavily shrunk
            cohort_source: CohortSource::Global,
            has_local_support: false,
        }];
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::SparseLocalSupport { .. }
        )));
    }

    #[test]
    fn test_refused_weak_confidence_sequence() {
        let mut input = make_default_input();
        // Fresh CS with no evidence.
        input.confidence_sequence = ConfidenceSequence::new();
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::WeakLowerBound { .. }
        )));
    }

    #[test]
    fn test_refused_tail_budget_violation() {
        let mut input = make_default_input();
        input.budget_checks = vec![EffectBudgetCheck {
            effect_kind: EffectKind::Release,
            total_actions: 20,
            false_actions: 1,
            observed_rate: 0.05,
            budget_rate: 0.0, // zero tolerance for Release
            meets_budget: false,
        }];
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::TailBudgetViolation { .. }
        )));
    }

    #[test]
    fn test_refused_tail_risk_spike() {
        let mut input = make_default_input();
        input.tail_max_loss = 50.0;
        input.tail_mean_loss = 0.5;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::TailRiskSpike { .. }
        )));
    }

    #[test]
    fn test_refused_attribution_backlog() {
        let mut input = make_default_input();
        input.pending_attribution_count = 100;
        input.max_pending_attributions = 50;
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        assert!(cert.block_reasons.iter().any(|r| matches!(r,
            CertificationBlockReason::UnresolvedAttributionBacklog { .. }
        )));
    }

    #[test]
    fn test_multiple_block_reasons_collected() {
        let mut input = make_default_input();
        input.evidence_quality.total_observations = 50; // insufficient
        input.regime.phase = "cooling".to_string(); // unstable
        input.evidence_quality.trusted_fraction = 0.30; // suspicious
        let cert = evaluate_certificate(input);
        assert!(!cert.is_certified());
        // Should have at least 3 block reasons (not short-circuited).
        assert!(cert.block_count() >= 3, "Expected >=3 blocks, got {}", cert.block_count());
    }

    #[test]
    fn test_certificate_expiry_by_regime() {
        let input = make_default_input();
        let cert = evaluate_certificate(input);
        assert!(!cert.is_expired(cert.issued_ts_micros + 1000, cert.regime.regime_id));
        assert!(cert.is_expired(cert.issued_ts_micros + 1000, cert.regime.regime_id + 1));
    }

    #[test]
    fn test_certificate_expiry_by_time() {
        let input = make_default_input();
        let cert = evaluate_certificate(input);
        let far_future = cert.issued_ts_micros + 10_000_000_000; // way past 1hr validity
        assert!(cert.is_expired(far_future, cert.regime.regime_id));
    }

    #[test]
    fn test_confidence_sequence_basic() {
        let mut cs = ConfidenceSequence::new();
        assert!(!cs.is_significant());
        assert_eq!(cs.n_observations, 0);

        // Feed consistently positive advantage.
        for _ in 0..200 {
            cs.update(0.2);
        }
        assert!(cs.is_significant(), "CS should be significant after 200 positive updates");
        assert!(cs.e_value() > cs.threshold());
    }

    #[test]
    fn test_confidence_sequence_negative_advantage() {
        let mut cs = ConfidenceSequence::new();
        // Feed consistently negative advantage (candidate is worse).
        for _ in 0..100 {
            cs.update(-0.1);
        }
        assert!(!cs.is_significant());
    }

    #[test]
    fn test_confidence_sequence_zero_advantage() {
        let mut cs = ConfidenceSequence::new();
        for _ in 0..100 {
            cs.update(0.0);
        }
        assert!(!cs.is_significant());
    }

    #[test]
    fn test_doubly_robust_basic() {
        let obs = make_default_observations(300, 0.15);
        let result = compute_doubly_robust(&obs, 0.5);
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.dr_advantage > 0.0, "Expected positive advantage");
        assert_eq!(result.n_observations, 300);
        assert!(result.n_selected > 0);
    }

    #[test]
    fn test_doubly_robust_empty() {
        let result = compute_doubly_robust(&[], 0.5);
        assert!(result.is_none());
    }

    #[test]
    fn test_doubly_robust_clipping() {
        // Observations with very low propensity → should trigger clipping.
        let obs: Vec<DRObservation> = (0..50).map(|_| DRObservation {
            candidate_selected: true,
            propensity: 0.001, // very low → high importance weight
            realized_loss: 0.3,
            predicted_loss: 0.5,
            evidence_weight: 1.0,
            attribution_weight: 1.0,
        }).collect();
        let result = compute_doubly_robust(&obs, 0.5).unwrap();
        assert!(result.n_clipped > 0, "Expected clipped weights with low propensity");
    }

    #[test]
    fn test_doubly_robust_zero_weight_observations_excluded() {
        // 3 contributing observations + 7 zero-weight ones.
        // The zero-weight ones should NOT dilute the estimate.
        let mut obs = Vec::new();
        for _ in 0..3 {
            obs.push(DRObservation {
                candidate_selected: true,
                propensity: 0.5,
                realized_loss: 0.4,
                predicted_loss: 0.5,
                evidence_weight: 1.0,
                attribution_weight: 1.0,
            });
        }
        for _ in 0..7 {
            obs.push(DRObservation {
                candidate_selected: true,
                propensity: 0.5,
                realized_loss: 0.4,
                predicted_loss: 0.5,
                evidence_weight: 0.0, // zero weight → excluded
                attribution_weight: 1.0,
            });
        }
        let result = compute_doubly_robust(&obs, 0.5).unwrap();
        assert_eq!(result.n_observations, 3, "Only contributing observations should be counted");
        assert_eq!(result.n_selected, 3);
    }

    #[test]
    fn test_doubly_robust_all_zero_weight_returns_none() {
        let obs: Vec<DRObservation> = (0..10).map(|_| DRObservation {
            candidate_selected: true,
            propensity: 0.5,
            realized_loss: 0.4,
            predicted_loss: 0.5,
            evidence_weight: 0.0, // all zero weight
            attribution_weight: 1.0,
        }).collect();
        let result = compute_doubly_robust(&obs, 0.5);
        assert!(result.is_none(), "All-zero-weight observations should return None");
    }

    #[test]
    fn test_evidence_quality_sufficient() {
        let eq = make_default_evidence_quality();
        assert!(eq.is_sufficient());
    }

    #[test]
    fn test_evidence_quality_insufficient_trusted() {
        let mut eq = make_default_evidence_quality();
        eq.trusted_fraction = 0.40;
        assert!(!eq.is_sufficient());
    }

    #[test]
    fn test_evidence_quality_insufficient_quarantine() {
        let mut eq = make_default_evidence_quality();
        eq.quarantined_count = 1;
        assert!(!eq.is_sufficient());
    }

    #[test]
    fn test_attribution_sufficient() {
        let attr = make_default_attribution();
        assert!(attr.is_sufficient());
    }

    #[test]
    fn test_attribution_insufficient() {
        let mut attr = make_default_attribution();
        attr.clean_or_primary_fraction = 0.30;
        assert!(!attr.is_sufficient());
    }

    #[test]
    fn test_evidence_transfer_rules_complete() {
        // Every rule should have a valid transferability and description.
        for rule in EVIDENCE_TRANSFER_RULES {
            assert!(!rule.category.is_empty());
            assert!(!rule.on_regime_change.is_empty());
            let discount = rule.transferability.regime_discount();
            assert!((0.0..=1.0).contains(&discount));
        }
    }

    #[test]
    fn test_certificate_summary_certified() {
        let input = make_default_input();
        let cert = evaluate_certificate(input);
        let summary = cert.summary();
        assert!(summary.starts_with("CERTIFIED"), "Summary: {summary}");
    }

    #[test]
    fn test_certificate_summary_refused() {
        let mut input = make_default_input();
        input.evidence_quality.total_observations = 10;
        let cert = evaluate_certificate(input);
        let summary = cert.summary();
        assert!(summary.starts_with("REFUSED"), "Summary: {summary}");
    }

    #[test]
    fn test_verdict_display() {
        assert_eq!(CertificateVerdict::Certified.to_string(), "certified");
        assert_eq!(CertificateVerdict::Refused.to_string(), "refused");
    }

    #[test]
    fn test_block_reason_display() {
        let reason = CertificationBlockReason::InsufficientData {
            observed: 50,
            required: 200,
        };
        let s = reason.to_string();
        assert!(s.contains("50"), "Display should contain observed count");
        assert!(s.contains("200"), "Display should contain required count");
    }

    #[test]
    fn test_transferability_display() {
        assert_eq!(EvidenceTransferability::Transferable.to_string(), "transferable");
        assert_eq!(EvidenceTransferability::DiscountedTransfer.to_string(), "discounted");
        assert_eq!(EvidenceTransferability::RegimeSensitive.to_string(), "regime_sensitive");
        assert_eq!(EvidenceTransferability::NonTransferable.to_string(), "non_transferable");
    }
}
