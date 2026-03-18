//! Assumptions ledger, EV gate, and transparency cards for ATC learning
//! decisions (br-0qt6e.1.7).
//!
//! This module is the **reasoning contract** that keeps the ATC learning stack
//! auditable, understandable, and worth operating. Every mathematical method
//! family in the learning design is tied to:
//!
//! 1. **User value**: what problem it solves for operators and agents.
//! 2. **Assumptions**: what must hold for the method to be valid.
//! 3. **EV gate**: what expected-value evidence justifies its inclusion.
//! 4. **Sunset criteria**: what evidence would justify simplifying, disabling,
//!    or removing it.
//! 5. **Transparency cards**: structured audit data for operator visibility.
//!
//! # Design Principle
//!
//! Math-for-math's-sake complexity is the enemy. Every method family in the
//! ATC learning stack must earn its keep by demonstrating measurable
//! improvement over the frozen baseline ([`super::atc_baseline`]).
//!
//! # How to Use This Module
//!
//! 1. **Before adding a new method**: check [`MethodFamily`] — if none fits,
//!    define a new variant and fill out a [`MethodLedgerEntry`].
//! 2. **Before promoting a method to live**: verify it passes the
//!    [`EvGate`] criteria.
//! 3. **After a policy change**: emit a [`TransparencyCard`] so operators
//!    can audit why the change happened and what evidence supported it.
//! 4. **During periodic review**: check [`AssumptionStatus`] flags — any
//!    `Suspect` or `Invalidated` entry requires investigation.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// Method family registry
// ──────────────────────────────────────────────────────────────────────

/// Every major mathematical mechanism in the ATC learning stack.
///
/// Each variant represents a distinct mathematical family whose assumptions,
/// costs, and benefits must be tracked independently. Adding a new variant
/// requires filling out a corresponding [`MethodLedgerEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodFamily {
    /// Bayesian posterior updates with EWMA smoothing.
    ///
    /// **What it does**: Maintains per-agent, per-subsystem belief state
    /// (probability distribution over hidden states) and updates them as
    /// new observations arrive.
    ///
    /// **User value**: Agents get more accurate liveness detection and
    /// conflict assessment over time, reducing false advisories.
    BayesianPosterior,

    /// Expected-loss minimization via loss matrices.
    ///
    /// **What it does**: Given a posterior over states and a matrix of
    /// action-state costs, picks the action with lowest expected loss.
    ///
    /// **User value**: ATC actions are principled rather than threshold-based,
    /// allowing asymmetric cost preferences (e.g., reluctance to release).
    ExpectedLossMinimization,

    /// Conformal risk control with finite-sample coverage guarantees.
    ///
    /// **What it does**: Gates high-force actions (release, force reservation)
    /// behind calibrated uncertainty bounds. Withholds action when the
    /// system cannot guarantee coverage.
    ///
    /// **User value**: Operators can trust that ATC will not take drastic
    /// actions when its own confidence is low. Safety gate for new installs.
    ConformalRiskControl,

    /// Empirical-Bayes shrinkage across strata.
    ///
    /// **What it does**: Pools information across agent/project/program
    /// groups to improve estimates when individual strata have few
    /// observations.
    ///
    /// **User value**: New agents benefit from population-level priors
    /// instead of starting from uninformative defaults.
    EmpiricalBayesShrinkage,

    /// E-process martingale calibration monitoring.
    ///
    /// **What it does**: Continuously tests whether the posterior updates
    /// are well-calibrated by running an e-process (test martingale)
    /// against prediction outcomes.
    ///
    /// **User value**: Detects when the model's predictions have drifted
    /// and triggers safe mode before bad decisions accumulate.
    EprocessCalibration,

    /// CUSUM regime change detection.
    ///
    /// **What it does**: Monitors the cumulative sum of prediction residuals
    /// to detect abrupt changes in the underlying process (regime shifts).
    ///
    /// **User value**: Enables fast adaptation when workload patterns change
    /// (e.g., swarm starts/stops, project switches) by resetting stale
    /// priors and discounting historical data.
    CusumRegimeDetection,

    /// Regret-bounded policy adaptation.
    ///
    /// **What it does**: Compares cumulative loss of the current policy
    /// against a shadow policy. When regret exceeds a threshold, promotes
    /// the shadow policy.
    ///
    /// **User value**: ATC automatically improves over time without
    /// requiring manual parameter tuning. Bounded regret guarantees
    /// worst-case performance.
    RegretBoundedAdaptation,

    /// Survival analysis (Kaplan-Meier) for liveness estimation.
    ///
    /// **What it does**: Models the expected time-to-death for agents
    /// using survival curves, properly handling censored observations
    /// (agents that were alive at last observation).
    ///
    /// **User value**: More accurate liveness detection for agents with
    /// irregular activity patterns. Reduces false suspicion on slow
    /// but active agents.
    SurvivalAnalysis,

    /// Adaptive budget controller (Nominal/Pressure/Conservative).
    ///
    /// **What it does**: Dynamically adjusts probe budgets and tick
    /// utilization targets based on recent workload pressure.
    ///
    /// **User value**: ATC remains responsive under load without
    /// consuming excessive resources during quiet periods.
    AdaptiveBudgetControl,

    /// Value-of-information (VoI) guided probe scheduling.
    ///
    /// **What it does**: Prioritizes probes toward agents where the
    /// expected information gain is highest (highest posterior entropy
    /// or longest time since last observation).
    ///
    /// **User value**: Probe budget is spent where it matters most,
    /// improving detection speed for at-risk agents without increasing
    /// overall probe rate.
    ValueOfInformation,
}

impl MethodFamily {
    /// All known method families, in dependency order.
    pub const ALL: &[Self] = &[
        Self::BayesianPosterior,
        Self::ExpectedLossMinimization,
        Self::ConformalRiskControl,
        Self::EmpiricalBayesShrinkage,
        Self::EprocessCalibration,
        Self::CusumRegimeDetection,
        Self::RegretBoundedAdaptation,
        Self::SurvivalAnalysis,
        Self::AdaptiveBudgetControl,
        Self::ValueOfInformation,
    ];
}

impl std::fmt::Display for MethodFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BayesianPosterior => write!(f, "bayesian_posterior"),
            Self::ExpectedLossMinimization => write!(f, "expected_loss_minimization"),
            Self::ConformalRiskControl => write!(f, "conformal_risk_control"),
            Self::EmpiricalBayesShrinkage => write!(f, "empirical_bayes_shrinkage"),
            Self::EprocessCalibration => write!(f, "eprocess_calibration"),
            Self::CusumRegimeDetection => write!(f, "cusum_regime_detection"),
            Self::RegretBoundedAdaptation => write!(f, "regret_bounded_adaptation"),
            Self::SurvivalAnalysis => write!(f, "survival_analysis"),
            Self::AdaptiveBudgetControl => write!(f, "adaptive_budget_control"),
            Self::ValueOfInformation => write!(f, "value_of_information"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Assumption status tracking
// ──────────────────────────────────────────────────────────────────────

/// Health status of a mathematical assumption.
///
/// Every method family rests on assumptions. When those assumptions are
/// violated, the method may produce harmful recommendations. This enum
/// tracks whether each assumption is still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssumptionStatus {
    /// Assumption holds — no evidence of violation.
    Valid,
    /// Warning signals detected but not conclusive. Monitor closely.
    Suspect,
    /// Assumption conclusively violated. Method should be disabled or
    /// its output discounted.
    Invalidated,
    /// Assumption has not been tested yet (insufficient data).
    Untested,
}

impl std::fmt::Display for AssumptionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valid => write!(f, "valid"),
            Self::Suspect => write!(f, "suspect"),
            Self::Invalidated => write!(f, "invalidated"),
            Self::Untested => write!(f, "untested"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Assumption record
// ──────────────────────────────────────────────────────────────────────

/// A single mathematical assumption that a method family depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Assumption {
    /// Short machine-readable key (e.g., "ewma_stationarity").
    pub key: &'static str,
    /// Human-readable statement of what must hold.
    pub statement: &'static str,
    /// What observable evidence would indicate this assumption is violated.
    pub invalidation_signal: &'static str,
    /// Current status.
    pub status: AssumptionStatus,
    /// Which method families depend on this assumption.
    pub affects: &'static [MethodFamily],
}

// ──────────────────────────────────────────────────────────────────────
// Assumptions ledger
// ──────────────────────────────────────────────────────────────────────

/// The complete assumptions ledger for all ATC learning method families.
///
/// This is the canonical list of assumptions that underpin the learning
/// stack. Each entry specifies what must hold, what signals violation,
/// and which methods are affected. When an assumption is flagged as
/// `Suspect` or `Invalidated`, the affected methods should be gated or
/// disabled.
///
/// # Maintenance Contract
///
/// When adding a new method family to the ATC learning stack:
///
/// 1. Add a variant to [`MethodFamily`].
/// 2. Add its assumptions to [`ASSUMPTIONS_LEDGER`].
/// 3. Fill out a [`MethodLedgerEntry`] in [`METHOD_LEDGER`].
/// 4. Define its [`EvGate`] promotion criteria.
pub const ASSUMPTIONS_LEDGER: &[Assumption] = &[
    // ── Bayesian Posterior ──
    Assumption {
        key: "ewma_stationarity",
        statement: "Agent activity patterns are locally stationary within the EWMA window \
                    (alpha=0.3, effective window ~6 observations)",
        invalidation_signal: "CUSUM alarm fires on the prediction residual stream for a stratum, \
                              indicating a regime shift that EWMA is too slow to track",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::BayesianPosterior],
    },
    Assumption {
        key: "state_space_complete",
        statement: "The 3-state models (Alive/Flaky/Dead, NoConflict/Mild/Severe, \
                    Under/Balanced/Over) cover all operationally relevant agent states",
        invalidation_signal: "Persistent high residuals (predicted probability of observed state \
                              consistently < 0.2) across a stratum suggests a missing state",
        status: AssumptionStatus::Valid,
        affects: &[
            MethodFamily::BayesianPosterior,
            MethodFamily::ExpectedLossMinimization,
        ],
    },
    Assumption {
        key: "conditional_independence",
        statement: "Observations are conditionally independent given the hidden state — \
                    an agent's activity at time t does not influence its activity at t+1 \
                    beyond what the hidden state captures",
        invalidation_signal: "Significant autocorrelation in prediction residuals \
                              (lag-1 correlation > 0.3) indicates temporal dependence",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::BayesianPosterior],
    },
    // ── Expected-Loss Minimization ──
    Assumption {
        key: "loss_matrix_calibrated",
        statement: "The loss matrices in atc_baseline reflect the true operator-perceived \
                    cost of each action-state pair. Specifically: releasing an alive agent \
                    (cost=100) is genuinely ~100x worse than missing a dead agent (cost=1)",
        invalidation_signal: "Operator feedback or retroactive false-positive flagging \
                              contradicts the assumed cost ratios. A single confirmed \
                              false-positive release that operators considered minor would \
                              invalidate the 100:1 ratio",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::ExpectedLossMinimization],
    },
    Assumption {
        key: "action_space_complete",
        statement: "The action set for each subsystem is complete — there are no useful \
                    actions outside the defined 3-action sets",
        invalidation_signal: "Operators consistently take manual actions outside the ATC \
                              action space (e.g., partial releases, graduated warnings)",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::ExpectedLossMinimization],
    },
    // ── Conformal Risk Control ──
    Assumption {
        key: "exchangeability",
        statement: "Calibration samples (prediction, outcome) are exchangeable — \
                    future outcomes are drawn from the same distribution as past ones, \
                    modulo regime shifts detected by CUSUM",
        invalidation_signal: "E-process alert fires (martingale exceeds threshold=20.0) \
                              indicating systematic prediction bias",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::ConformalRiskControl],
    },
    Assumption {
        key: "coverage_target_meaningful",
        statement: "The target coverage of 85% is operationally appropriate — it catches \
                    most genuine issues while allowing reasonable false-negative rates",
        invalidation_signal: "Operators consistently complain about either too many missed \
                              issues (target too low) or too many false alarms (target too high)",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::ConformalRiskControl],
    },
    // ── Empirical-Bayes Shrinkage ──
    Assumption {
        key: "stratum_homogeneity",
        statement: "Agents within the same (subsystem, effect_kind, risk_tier) stratum \
                    are sufficiently similar that pooling information improves estimates",
        invalidation_signal: "Between-stratum variance is comparable to within-stratum variance \
                              (shrinkage factor collapses to 0, meaning pooling adds no information)",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::EmpiricalBayesShrinkage],
    },
    Assumption {
        key: "sufficient_strata_population",
        statement: "Most strata contain at least 10 observations for reliable shrinkage",
        invalidation_signal: "More than 50% of strata have fewer than 5 observations \
                              after 24 hours of operation",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::EmpiricalBayesShrinkage],
    },
    // ── E-process Calibration ──
    Assumption {
        key: "martingale_bounded",
        statement: "The e-process martingale has finite variance under the null hypothesis \
                    (well-calibrated predictions), ensuring false alert rate control",
        invalidation_signal: "Frequent spurious alerts (>1 per hour) when no genuine \
                              miscalibration is present",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::EprocessCalibration],
    },
    // ── CUSUM Regime Detection ──
    Assumption {
        key: "regime_shifts_are_abrupt",
        statement: "Workload regime changes (swarm start/stop, project switch) are \
                    approximately step changes, not gradual drifts",
        invalidation_signal: "CUSUM fires many small alarms instead of clean regime \
                              boundaries, indicating gradual drift that CUSUM cannot \
                              cleanly separate",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::CusumRegimeDetection],
    },
    Assumption {
        key: "cusum_sensitivity_appropriate",
        statement: "CUSUM parameters (threshold=5.0, delta=0.1) detect meaningful shifts \
                    without excessive false alarms",
        invalidation_signal: "Either no alarms fire during known regime changes (too insensitive) \
                              or alarms fire during stable periods (too sensitive)",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::CusumRegimeDetection],
    },
    // ── Regret-Bounded Adaptation ──
    Assumption {
        key: "shadow_policy_meaningful",
        statement: "The shadow policy (alternative parameter set) explores a meaningfully \
                    different strategy space from the current policy",
        invalidation_signal: "Shadow and current policy make the same decision >95% of the time, \
                              meaning regret tracking provides no useful signal",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::RegretBoundedAdaptation],
    },
    Assumption {
        key: "regret_finite_horizon",
        statement: "Cumulative regret is evaluated over finite windows (not unbounded), \
                    so f64 accumulation does not suffer precision loss",
        invalidation_signal: "Cumulative regret exceeds 1e12, approaching f64 precision limits \
                              for meaningful differences",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::RegretBoundedAdaptation],
    },
    // ── Survival Analysis ──
    Assumption {
        key: "noninformative_censoring",
        statement: "Agent departure (censoring) is independent of the agent's true \
                    liveness state — agents don't preferentially crash when they're \
                    about to die",
        invalidation_signal: "Censored agents are disproportionately in the Flaky posterior \
                              state (>3x the base rate), suggesting censoring correlates with \
                              poor health",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::SurvivalAnalysis],
    },
    // ── Adaptive Budget Control ──
    Assumption {
        key: "utilization_measurable",
        statement: "Tick utilization (time spent / budget) is an accurate proxy for \
                    system load, and the mode transitions at 75%/90% are appropriate \
                    boundaries",
        invalidation_signal: "System exhibits overload symptoms (dropped probes, advisory \
                              delays) while reported utilization is below threshold",
        status: AssumptionStatus::Valid,
        affects: &[MethodFamily::AdaptiveBudgetControl],
    },
    // ── Value of Information ──
    Assumption {
        key: "entropy_proxy_for_value",
        statement: "Posterior entropy is a reasonable proxy for the value of an additional \
                    observation — high-entropy agents benefit more from probing",
        invalidation_signal: "Probes to high-entropy agents consistently fail to reduce \
                              posterior entropy (information gain near zero despite high \
                              prior uncertainty)",
        status: AssumptionStatus::Untested,
        affects: &[MethodFamily::ValueOfInformation],
    },
];

// ──────────────────────────────────────────────────────────────────────
// Method ledger — per-method justification and sunset criteria
// ──────────────────────────────────────────────────────────────────────

/// Per-method-family ledger entry recording justification, value, risk,
/// and sunset criteria.
///
/// This is the "reasoning contract" for each mathematical mechanism.
/// A future contributor reading this entry should be able to determine:
///
/// - Why the method exists and what user-visible problem it solves.
/// - What evidence justified including it.
/// - What evidence would justify removing or simplifying it.
/// - What the method costs in complexity and compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MethodLedgerEntry {
    /// Which method family this entry describes.
    pub family: MethodFamily,
    /// What user-visible problem this method solves (1–2 sentences).
    pub user_value: &'static str,
    /// What failure mode this method controls (1–2 sentences).
    pub failure_mode_controlled: &'static str,
    /// What measurable improvement over baseline justifies its inclusion.
    pub ev_justification: &'static str,
    /// Computational cost (per-tick or per-decision).
    pub compute_cost: &'static str,
    /// Complexity cost (lines of code, new types introduced, test burden).
    pub complexity_cost: &'static str,
    /// Under what conditions this method should be disabled or removed.
    pub sunset_criteria: &'static str,
    /// Dependencies on other method families.
    pub depends_on: &'static [MethodFamily],
    /// Whether this method is currently active in the live system.
    pub active: bool,
}

/// The complete method ledger for the ATC learning stack.
///
/// Each entry ties a mathematical mechanism back to user value, safety,
/// and explicit sunset criteria. This is the master reference for
/// "why does this exist and when should we remove it?"
pub const METHOD_LEDGER: &[MethodLedgerEntry] = &[
    MethodLedgerEntry {
        family: MethodFamily::BayesianPosterior,
        user_value: "Agents get more accurate liveness/conflict/load assessments \
                     over time, reducing false advisory noise.",
        failure_mode_controlled: "Threshold-based decisions ignore confidence levels. \
                                  Bayesian posteriors encode 'how sure am I?' explicitly.",
        ev_justification: "False-advisory rate drops measurably vs fixed-threshold baseline. \
                           Minimum bar: 10% fewer false positives at same recall.",
        compute_cost: "O(|states|) per observation per agent. ~3 multiplies + normalization. \
                       Under 1 microsecond per update.",
        complexity_cost: "~200 lines in atc.rs. DecisionCore<S,A> generic. 3 state enums. \
                          Well-understood textbook math.",
        sunset_criteria: "If EWMA updates show no measurable improvement over fixed priors \
                          after 10,000 observations (across all strata), replace with static \
                          priors and remove the update machinery.",
        depends_on: &[],
        active: true,
    },
    MethodLedgerEntry {
        family: MethodFamily::ExpectedLossMinimization,
        user_value: "ATC picks actions that minimize operator-perceived harm rather than \
                     using ad-hoc thresholds. Asymmetric costs (reluctance to release) \
                     are encoded directly.",
        failure_mode_controlled: "Threshold-based decisions cannot express 'releasing an \
                                  alive agent is 100x worse than missing a dead one' — \
                                  they pick a cutoff and hope.",
        ev_justification: "Expected-loss decisions are provably optimal given correct loss \
                           matrices and posteriors. This is the core of the decision engine.",
        compute_cost: "O(|actions| * |states|) per decision. 9 multiplies + argmin. \
                       Under 1 microsecond.",
        complexity_cost: "~100 lines. Matrix multiply + argmin. Loss matrices are constants \
                          (atc_baseline.rs). Minimal test surface.",
        sunset_criteria: "Never — this is the foundational decision primitive. If the loss \
                          matrices prove wrong, fix the matrices, don't remove the mechanism.",
        depends_on: &[MethodFamily::BayesianPosterior],
        active: true,
    },
    MethodLedgerEntry {
        family: MethodFamily::ConformalRiskControl,
        user_value: "High-force actions (release, force reservation) are blocked when \
                     ATC's own predictions are unreliable. New installs are safe by default.",
        failure_mode_controlled: "Without conformal gating, a miscalibrated posterior could \
                                  trigger confident-but-wrong releases. Conformal control provides \
                                  finite-sample coverage guarantees regardless of distribution.",
        ev_justification: "False-positive rate for high-force actions stays below coverage target \
                           (85%) with at least 20 calibration samples. No other mechanism provides \
                           distribution-free coverage guarantees.",
        compute_cost: "O(n) per calibration check where n = calibration window size. \
                       Typically n < 100. Under 10 microseconds.",
        complexity_cost: "~300 lines (conformal.rs). Requires calibration sample buffer. \
                          Tests must verify coverage under distribution shift.",
        sunset_criteria: "If the posterior (Bayesian updates) proves consistently well-calibrated \
                          (e-process never fires after initial 1,000 observations across all strata), \
                          conformal gating can be relaxed to advisory-only mode. Do NOT remove it — \
                          degrade gracefully.",
        depends_on: &[
            MethodFamily::BayesianPosterior,
            MethodFamily::EprocessCalibration,
        ],
        active: true,
    },
    MethodLedgerEntry {
        family: MethodFamily::EmpiricalBayesShrinkage,
        user_value: "New agents immediately benefit from population-level priors rather \
                     than starting from uninformative defaults. Reduces cold-start advisory \
                     noise.",
        failure_mode_controlled: "Without shrinkage, agents with few observations get noisy \
                                  posteriors that trigger unnecessary advisories. Shrinkage \
                                  pools information from similar agents.",
        ev_justification: "Cold-start false-advisory rate drops by at least 20% compared to \
                           per-agent-only priors, measured over first 50 observations per new \
                           agent across at least 5 strata.",
        compute_cost: "O(k) per stratum where k = number of agents in the stratum. \
                       Computed at rollup time, not per-tick. Under 100 microseconds per stratum.",
        complexity_cost: "~200 lines. Requires population-level variance estimation. \
                          Shrinkage factor computation. Moderate test burden (need multi-agent \
                          fixtures).",
        sunset_criteria: "If shrinkage factor consistently near 0 (within-stratum variance \
                          >> between-stratum variance) across all strata after 10,000 total \
                          observations, disable shrinkage and use per-agent priors only.",
        depends_on: &[MethodFamily::BayesianPosterior],
        active: false, // Phase 3 — not yet implemented
    },
    MethodLedgerEntry {
        family: MethodFamily::EprocessCalibration,
        user_value: "Detects when ATC's predictions have drifted from reality and \
                     automatically triggers safe mode before bad decisions accumulate.",
        failure_mode_controlled: "Without calibration monitoring, a slowly drifting posterior \
                                  could make increasingly bad decisions without any visible alert. \
                                  E-process catches this with anytime-valid guarantees.",
        ev_justification: "Detects genuine miscalibration (prediction accuracy drops > 15%) \
                           within 20 observations with false alarm rate < 5%. No simpler \
                           mechanism provides anytime-valid detection.",
        compute_cost: "O(1) per observation: one multiply + compare. Under 1 microsecond.",
        complexity_cost: "~100 lines. Martingale update + threshold check. \
                          Well-understood sequential testing math.",
        sunset_criteria: "If safe mode is never triggered (e-process never fires) after 50,000 \
                          observations across all strata, the e-process is either (a) too insensitive \
                          or (b) the system is genuinely well-calibrated. In case (a), lower the \
                          threshold. In case (b), keep it as a safety net — the cost is negligible.",
        depends_on: &[MethodFamily::BayesianPosterior],
        active: true,
    },
    MethodLedgerEntry {
        family: MethodFamily::CusumRegimeDetection,
        user_value: "Fast adaptation when workload patterns change (swarm starts/stops, \
                     project switches). Agents get accurate assessments within minutes \
                     of a regime change instead of waiting for EWMA to catch up.",
        failure_mode_controlled: "EWMA with alpha=0.3 has ~6-observation memory. A sharp \
                                  regime shift (e.g., 5 agents suddenly stop) takes 6+ observations \
                                  to fully adapt. CUSUM detects the shift immediately and triggers \
                                  prior reset.",
        ev_justification: "Time-to-detection for genuine regime shifts drops by >50% compared \
                           to EWMA-only adaptation. Measured as time from true shift to posterior \
                           reflecting new regime at >80% confidence.",
        compute_cost: "O(1) per observation: cumulative sum update + threshold check. \
                       Under 1 microsecond.",
        complexity_cost: "~80 lines. CUSUM accumulator + reset logic. \
                          Minimal — well-understood industrial statistics.",
        sunset_criteria: "If regime shifts are never detected (CUSUM never fires) after 30 days \
                          of multi-agent operation, the system may not experience regime shifts. \
                          Keep as safety net — cost is negligible. If CUSUM fires > 10 times/hour, \
                          delta/threshold parameters need recalibration.",
        depends_on: &[],
        active: true,
    },
    MethodLedgerEntry {
        family: MethodFamily::RegretBoundedAdaptation,
        user_value: "ATC automatically improves its decision quality over time without \
                     manual parameter tuning. Worst-case performance is bounded.",
        failure_mode_controlled: "Without regret tracking, there is no principled way to \
                                  decide when to change policy parameters. Manual tuning is \
                                  fragile and doesn't scale.",
        ev_justification: "Cumulative regret (actual loss - best-in-hindsight loss) grows \
                           sub-linearly in the number of decisions. Shadow policy finds at least \
                           one improvement that reduces per-decision loss by > 5%.",
        compute_cost: "O(1) per decision: one loss lookup + accumulation. Policy promotion \
                       check at rollup time. Under 1 microsecond per decision.",
        complexity_cost: "~150 lines. Shadow policy maintenance + regret accumulation + \
                          promotion logic. Moderate — requires careful testing of promotion \
                          thresholds.",
        sunset_criteria: "If shadow and current policy agree > 95% of the time for > 10,000 \
                          decisions, the current policy is near-optimal and the shadow can be \
                          disabled. Re-enable if regime detection fires.",
        depends_on: &[MethodFamily::ExpectedLossMinimization],
        active: false, // Phase 3 — not yet implemented
    },
    MethodLedgerEntry {
        family: MethodFamily::SurvivalAnalysis,
        user_value: "More accurate liveness detection for agents with irregular activity \
                     patterns. Reduces false suspicion on slow-but-active agents.",
        failure_mode_controlled: "Fixed-interval suspicion (3*sigma of inter-activity time) \
                                  assumes normally distributed activity. Real agents have \
                                  heavy-tailed patterns. Survival analysis handles this properly.",
        ev_justification: "False-suspicion rate for agents with inter-activity coefficient \
                           of variation > 1.0 drops by > 30% compared to fixed-interval model.",
        compute_cost: "O(n) for Kaplan-Meier curve construction where n = observation count. \
                       Done at rollup time, not per-tick. Under 100 microseconds for n < 200.",
        complexity_cost: "~200 lines. Kaplan-Meier estimator + censoring logic. \
                          Moderate — requires test fixtures for censored observations.",
        sunset_criteria: "If agent activity patterns are consistently regular (coefficient of \
                          variation < 0.5 for > 80% of agents), the simpler interval-based model \
                          suffices. Remove survival analysis and use EWMA-only liveness.",
        depends_on: &[],
        active: false, // Referenced in atc.rs but not integrated with learning loop yet
    },
    MethodLedgerEntry {
        family: MethodFamily::AdaptiveBudgetControl,
        user_value: "ATC stays responsive under load without wasting resources during \
                     quiet periods. Operators don't need to manually tune probe budgets.",
        failure_mode_controlled: "Fixed probe budgets either waste resources during quiet \
                                  periods or cause probe starvation under load. Adaptive \
                                  control adjusts automatically.",
        ev_justification: "Probe utilization stays within 40-80% across varying workloads \
                           (1 to 30 agents). Without adaptation, utilization swings from \
                           <10% to >95%.",
        compute_cost: "O(1) per tick: utilization check + mode transition. Under 1 microsecond.",
        complexity_cost: "~100 lines. Three-mode state machine (Nominal/Pressure/Conservative) \
                          with hysteresis. Simple and well-tested.",
        sunset_criteria: "Never — adaptive budgeting is essential for production. If the mode \
                          transitions prove too aggressive, tune the thresholds; don't remove \
                          the mechanism.",
        depends_on: &[],
        active: true,
    },
    MethodLedgerEntry {
        family: MethodFamily::ValueOfInformation,
        user_value: "Limited probe budget is spent where it produces the most diagnostic \
                     value, improving detection speed for at-risk agents.",
        failure_mode_controlled: "Round-robin or random probing wastes budget on agents whose \
                                  state is already well-known while neglecting agents with \
                                  high uncertainty.",
        ev_justification: "Time-to-detection for genuinely dead agents drops by > 25% compared \
                           to round-robin probing at the same probe budget. Measured as time from \
                           agent death to posterior(Dead) > 0.5.",
        compute_cost: "O(k) per scheduling decision where k = number of registered agents. \
                       Entropy computation per agent. Under 10 microseconds for k < 50.",
        complexity_cost: "~100 lines. Entropy computation + argmax scheduling. \
                          Minimal — straightforward information theory.",
        sunset_criteria: "If all agents have similar posterior entropy (coefficient of variation \
                          < 0.2 across agent entropies) for > 1,000 scheduling decisions, VoI \
                          degenerates to round-robin. Replace with simpler recency-based scheduling.",
        depends_on: &[MethodFamily::BayesianPosterior],
        active: false, // Phase 3 — not yet implemented
    },
];

// ──────────────────────────────────────────────────────────────────────
// EV gate — expected-value promotion criteria
// ──────────────────────────────────────────────────────────────────────

/// Expected-value gate for promoting a method from shadow to live.
///
/// A method family must pass ALL criteria in its EV gate before it can
/// be promoted from shadow (evaluation) mode to live (decision-affecting)
/// mode. This prevents math-for-math's-sake additions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvGate {
    /// Which method family this gate applies to.
    pub family: MethodFamily,
    /// Minimum number of observations required before evaluation.
    pub min_observations: u64,
    /// Minimum improvement over baseline (as a fraction, e.g., 0.10 = 10%).
    pub min_improvement_fraction: f64,
    /// Maximum acceptable regression in any safety metric (fraction).
    pub max_safety_regression: f64,
    /// Whether all dependent assumptions must be `Valid` or `Untested`.
    pub requires_valid_assumptions: bool,
    /// Human-readable description of what "improvement" means for this method.
    pub improvement_metric: &'static str,
    /// Human-readable description of the safety metric that must not regress.
    pub safety_metric: &'static str,
}

/// EV gates for all method families.
///
/// Each gate defines the minimum evidence required before a method can be
/// promoted from shadow to live mode. Methods that fail their gate remain
/// in shadow mode (tracking regret without affecting decisions).
pub const EV_GATES: &[EvGate] = &[
    EvGate {
        family: MethodFamily::BayesianPosterior,
        min_observations: 100,
        min_improvement_fraction: 0.10,
        max_safety_regression: 0.0, // no safety regression allowed
        requires_valid_assumptions: true,
        improvement_metric: "False-advisory rate reduction vs fixed-prior baseline",
        safety_metric: "False-positive release rate (must stay at 0)",
    },
    EvGate {
        family: MethodFamily::ExpectedLossMinimization,
        min_observations: 0, // foundational — always active
        min_improvement_fraction: 0.0,
        max_safety_regression: 0.0,
        requires_valid_assumptions: true,
        improvement_metric: "N/A — foundational decision primitive, always active",
        safety_metric: "False-positive release rate (must stay at 0)",
    },
    EvGate {
        family: MethodFamily::ConformalRiskControl,
        min_observations: 20,
        min_improvement_fraction: 0.0, // safety mechanism, not improvement
        max_safety_regression: 0.0,
        requires_valid_assumptions: true,
        improvement_metric: "Coverage: fraction of high-force actions where the true state \
                             was within the prediction set",
        safety_metric: "False-positive release rate under conformal gating vs ungated",
    },
    EvGate {
        family: MethodFamily::EmpiricalBayesShrinkage,
        min_observations: 500, // need population-level data
        min_improvement_fraction: 0.20,
        max_safety_regression: 0.05,
        requires_valid_assumptions: true,
        improvement_metric: "Cold-start false-advisory rate for new agents (first 50 observations)",
        safety_metric: "Overall false-positive release rate across all agents",
    },
    EvGate {
        family: MethodFamily::EprocessCalibration,
        min_observations: 50,
        min_improvement_fraction: 0.0, // monitoring mechanism, not improvement
        max_safety_regression: 0.0,
        requires_valid_assumptions: true,
        improvement_metric: "Detection delay for genuine miscalibration (predictions > 15% off)",
        safety_metric: "False alarm rate (spurious safe-mode entries per hour)",
    },
    EvGate {
        family: MethodFamily::CusumRegimeDetection,
        min_observations: 200,
        min_improvement_fraction: 0.50,
        max_safety_regression: 0.05,
        requires_valid_assumptions: true,
        improvement_metric: "Time-to-detection for genuine regime shifts",
        safety_metric: "False regime-shift detection rate",
    },
    EvGate {
        family: MethodFamily::RegretBoundedAdaptation,
        min_observations: 1000,
        min_improvement_fraction: 0.05,
        max_safety_regression: 0.02,
        requires_valid_assumptions: true,
        improvement_metric: "Per-decision expected loss reduction via policy promotion",
        safety_metric: "Maximum per-stratum false-positive release rate",
    },
    EvGate {
        family: MethodFamily::SurvivalAnalysis,
        min_observations: 200,
        min_improvement_fraction: 0.30,
        max_safety_regression: 0.05,
        requires_valid_assumptions: true,
        improvement_metric: "False-suspicion rate for agents with high activity variance (CV > 1.0)",
        safety_metric: "Time-to-detection for genuinely dead agents",
    },
    EvGate {
        family: MethodFamily::AdaptiveBudgetControl,
        min_observations: 0, // foundational — always active
        min_improvement_fraction: 0.0,
        max_safety_regression: 0.0,
        requires_valid_assumptions: true,
        improvement_metric: "Probe utilization stability across workload variation (target 40-80%)",
        safety_metric: "Probe starvation events (probes dropped due to budget exhaustion)",
    },
    EvGate {
        family: MethodFamily::ValueOfInformation,
        min_observations: 500,
        min_improvement_fraction: 0.25,
        max_safety_regression: 0.05,
        requires_valid_assumptions: true,
        improvement_metric: "Time-to-detection for genuinely dead agents vs round-robin probing",
        safety_metric: "Overall detection recall (fraction of dead agents eventually detected)",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Transparency cards
// ──────────────────────────────────────────────────────────────────────

/// Structured audit record for a policy change or major ATC decision.
///
/// Transparency cards are emitted whenever ATC makes a significant
/// change to its behavior (policy promotion, safe mode entry/exit,
/// assumption invalidation, etc.). They provide operators with:
///
/// 1. **What happened**: the change type and affected scope.
/// 2. **Why**: the evidence and reasoning that led to the change.
/// 3. **What alternative was considered**: the rejected action and its cost.
/// 4. **What assumptions underlie the decision**: links to the ledger.
/// 5. **How to override**: what operator action would reverse or modify.
///
/// Cards are serialized to JSON and stored in the evidence ledger and
/// (optionally) the Git archive for human review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransparencyCard {
    /// Unique card identifier (monotonic within process).
    pub card_id: u64,
    /// When the card was emitted (microseconds since epoch).
    pub emitted_ts_micros: i64,
    /// What kind of change this card describes.
    pub change_type: ChangeType,
    /// Which method family or subsystem is affected.
    pub scope: CardScope,
    /// The policy or configuration that was active before the change.
    pub prior_policy_id: Option<String>,
    /// The policy or configuration after the change.
    pub new_policy_id: Option<String>,
    /// Evidence that justified the change.
    pub evidence: CardEvidence,
    /// The alternative action that was considered and rejected.
    pub rejected_alternative: Option<RejectedAlternative>,
    /// Assumptions that this decision depends on (keys from the ledger).
    pub assumption_keys: Vec<String>,
    /// What operator action would reverse or modify this change.
    pub override_guidance: String,
    /// Regime context at the time of the change.
    pub regime_context: Option<RegimeContext>,
}

/// What kind of change a transparency card describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    /// A shadow policy was promoted to live.
    PolicyPromotion,
    /// ATC entered safe mode due to calibration failure.
    SafeModeEntry,
    /// ATC exited safe mode after recovery.
    SafeModeExit,
    /// An assumption was flagged as suspect or invalidated.
    AssumptionFlagged,
    /// A method family was enabled or disabled.
    MethodToggle,
    /// A regime shift was detected and priors were reset.
    RegimeShift,
    /// Loss matrix parameters were updated.
    LossMatrixUpdate,
    /// Calibration thresholds were adjusted.
    CalibrationAdjustment,
    /// Budget controller mode changed.
    BudgetModeChange,
    /// Manual operator override.
    OperatorOverride,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PolicyPromotion => write!(f, "policy_promotion"),
            Self::SafeModeEntry => write!(f, "safe_mode_entry"),
            Self::SafeModeExit => write!(f, "safe_mode_exit"),
            Self::AssumptionFlagged => write!(f, "assumption_flagged"),
            Self::MethodToggle => write!(f, "method_toggle"),
            Self::RegimeShift => write!(f, "regime_shift"),
            Self::LossMatrixUpdate => write!(f, "loss_matrix_update"),
            Self::CalibrationAdjustment => write!(f, "calibration_adjustment"),
            Self::BudgetModeChange => write!(f, "budget_mode_change"),
            Self::OperatorOverride => write!(f, "operator_override"),
        }
    }
}

/// Scope of a transparency card — what part of ATC is affected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardScope {
    /// Which method family is affected (if applicable).
    pub method: Option<MethodFamily>,
    /// Which subsystem is affected (if applicable).
    pub subsystem: Option<String>,
    /// Which stratum is affected (e.g., "liveness:advisory:low").
    pub stratum: Option<String>,
    /// Which agents are affected (if scoped to specific agents).
    pub agents: Vec<String>,
    /// Which project is affected (if project-scoped).
    pub project_key: Option<String>,
}

/// Evidence that justified a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardEvidence {
    /// Reference to the evidence snapshot (evidence_id).
    pub evidence_id: Option<String>,
    /// Number of observations that informed this decision.
    pub observation_count: u64,
    /// Time window over which evidence was collected (microseconds).
    pub window_micros: i64,
    /// The key metric value that triggered the change.
    pub trigger_metric: String,
    /// The threshold that was crossed.
    pub trigger_threshold: f64,
    /// The observed value of the trigger metric.
    pub observed_value: f64,
    /// Human-readable summary of why this evidence is sufficient.
    pub rationale: String,
}

/// An alternative action that was considered and rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedAlternative {
    /// What the alternative was.
    pub action: String,
    /// Why it was rejected.
    pub reason: String,
    /// What its expected cost would have been.
    pub expected_cost: f64,
}

/// Regime context at the time of a transparency card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegimeContext {
    /// Budget controller mode (nominal/pressure/conservative).
    pub budget_mode: String,
    /// Whether safe mode is active.
    pub safe_mode_active: bool,
    /// Number of active agents.
    pub active_agent_count: u32,
    /// Number of active projects.
    pub active_project_count: u32,
    /// Recent tick utilization (basis points, 0–10000).
    pub tick_utilization_bp: u16,
}

// ──────────────────────────────────────────────────────────────────────
// Escalation and simplification criteria
// ──────────────────────────────────────────────────────────────────────

/// Criteria for escalating mathematical sophistication.
///
/// A new mathematical mechanism should only be added when ALL of these
/// criteria are met. This prevents math-for-math's-sake complexity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct EscalationCriteria {
    /// The problem must be observable in production data (not theoretical).
    pub observable_problem: &'static str,
    /// A simpler approach must have been tried and proven insufficient.
    pub simpler_approach_tried: &'static str,
    /// The proposed mechanism must have a clear EV gate with measurable criteria.
    pub ev_gate_defined: bool,
    /// The mechanism must have explicit sunset criteria.
    pub sunset_criteria_defined: bool,
    /// The mechanism's assumptions must be documented in the ledger.
    pub assumptions_documented: bool,
    /// The computational cost must be within the tick budget (5ms).
    pub within_tick_budget: bool,
}

/// The escalation criteria that MUST be met before adding a new method.
pub const ESCALATION_CRITERIA: EscalationCriteria = EscalationCriteria {
    observable_problem: "The failure mode must be reproducible in the synthetic scenario \
                         corpus or observed in production evidence ledger data. Theoretical \
                         concerns without data are not sufficient.",
    simpler_approach_tried: "Before adding a new mechanism, document what simpler approach \
                             was tried (e.g., tuning existing thresholds, adjusting alpha, \
                             changing loss matrix entries) and why it was insufficient.",
    ev_gate_defined: true,
    sunset_criteria_defined: true,
    assumptions_documented: true,
    within_tick_budget: true,
};

/// Criteria for simplifying or removing an existing method.
///
/// When evidence accumulates that a method is not earning its keep,
/// these criteria define when it should be disabled or removed.
pub const SIMPLIFICATION_TRIGGERS: &[&str] = &[
    "Method's EV gate has not been passed after 2x the required observation count",
    "Shadow policy and current policy agree > 95% of decisions for > 10,000 decisions",
    "Method's key assumption has been Invalidated with no path to recovery",
    "Method adds > 1ms to per-tick latency without corresponding improvement",
    "Method has been in shadow mode for > 30 days without showing improvement",
    "Operator feedback explicitly requests disabling the method",
    "The failure mode the method controls has never been observed in production",
];

// ──────────────────────────────────────────────────────────────────────
// Lookup helpers
// ──────────────────────────────────────────────────────────────────────

/// Find the method ledger entry for a given method family.
#[must_use]
pub fn method_entry(family: MethodFamily) -> Option<&'static MethodLedgerEntry> {
    METHOD_LEDGER.iter().find(|e| e.family == family)
}

/// Find the EV gate for a given method family.
#[must_use]
pub fn ev_gate(family: MethodFamily) -> Option<&'static EvGate> {
    EV_GATES.iter().find(|g| g.family == family)
}

/// Find all assumptions that affect a given method family.
#[must_use]
pub fn assumptions_for(family: MethodFamily) -> Vec<&'static Assumption> {
    ASSUMPTIONS_LEDGER
        .iter()
        .filter(|a| a.affects.contains(&family))
        .collect()
}

/// Check whether all assumptions for a method family are healthy
/// (either `Valid` or `Untested`).
#[must_use]
pub fn assumptions_healthy(family: MethodFamily) -> bool {
    assumptions_for(family).iter().all(|a| {
        matches!(
            a.status,
            AssumptionStatus::Valid | AssumptionStatus::Untested
        )
    })
}

/// Check whether a method family passes its EV gate given observed metrics.
///
/// Returns `Ok(())` if the gate is passed, `Err(reason)` if not.
pub fn check_ev_gate(
    family: MethodFamily,
    observations: u64,
    improvement: f64,
    safety_regression: f64,
) -> Result<(), String> {
    let gate = ev_gate(family).ok_or_else(|| format!("no EV gate defined for {family}"))?;

    if observations < gate.min_observations {
        return Err(format!(
            "insufficient observations: {observations} < {} required",
            gate.min_observations
        ));
    }

    if improvement < gate.min_improvement_fraction {
        return Err(format!(
            "insufficient improvement: {improvement:.3} < {:.3} required ({})",
            gate.min_improvement_fraction, gate.improvement_metric
        ));
    }

    if safety_regression > gate.max_safety_regression {
        return Err(format!(
            "safety regression too high: {safety_regression:.3} > {:.3} max ({})",
            gate.max_safety_regression, gate.safety_metric
        ));
    }

    if gate.requires_valid_assumptions && !assumptions_healthy(family) {
        let suspect: Vec<_> = assumptions_for(family)
            .iter()
            .filter(|a| {
                matches!(
                    a.status,
                    AssumptionStatus::Suspect | AssumptionStatus::Invalidated
                )
            })
            .map(|a| a.key)
            .collect();
        return Err(format!("unhealthy assumptions: {}", suspect.join(", ")));
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_method_families_have_ledger_entries() {
        for family in MethodFamily::ALL {
            assert!(
                method_entry(*family).is_some(),
                "missing ledger entry for {family}"
            );
        }
    }

    #[test]
    fn all_method_families_have_ev_gates() {
        for family in MethodFamily::ALL {
            assert!(ev_gate(*family).is_some(), "missing EV gate for {family}");
        }
    }

    #[test]
    fn all_method_families_have_assumptions() {
        // Not every method needs explicit assumptions (e.g., budget control
        // is purely mechanical), but most should have at least one.
        let families_with_assumptions: Vec<_> = MethodFamily::ALL
            .iter()
            .filter(|f| !assumptions_for(**f).is_empty())
            .collect();
        assert!(
            families_with_assumptions.len() >= 8,
            "expected at least 8 method families with documented assumptions, got {}",
            families_with_assumptions.len()
        );
    }

    #[test]
    fn assumption_keys_are_unique() {
        let mut keys: Vec<&str> = ASSUMPTIONS_LEDGER.iter().map(|a| a.key).collect();
        keys.sort_unstable();
        let original_len = keys.len();
        keys.dedup();
        assert_eq!(original_len, keys.len(), "duplicate assumption keys found");
    }

    #[test]
    fn ev_gate_thresholds_are_sane() {
        for gate in EV_GATES {
            assert!(
                gate.min_improvement_fraction >= 0.0 && gate.min_improvement_fraction <= 1.0,
                "improvement fraction for {} out of range: {}",
                gate.family,
                gate.min_improvement_fraction
            );
            assert!(
                gate.max_safety_regression >= 0.0 && gate.max_safety_regression <= 1.0,
                "safety regression for {} out of range: {}",
                gate.family,
                gate.max_safety_regression
            );
        }
    }

    #[test]
    fn ev_gate_check_passes_for_foundational_methods() {
        // ExpectedLossMinimization has min_observations=0, min_improvement=0
        assert!(check_ev_gate(MethodFamily::ExpectedLossMinimization, 0, 0.0, 0.0).is_ok());
        // AdaptiveBudgetControl has min_observations=0, min_improvement=0
        assert!(check_ev_gate(MethodFamily::AdaptiveBudgetControl, 0, 0.0, 0.0).is_ok());
    }

    #[test]
    fn ev_gate_check_rejects_insufficient_observations() {
        let result = check_ev_gate(MethodFamily::BayesianPosterior, 50, 0.15, 0.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("insufficient observations"));
    }

    #[test]
    fn ev_gate_check_rejects_insufficient_improvement() {
        let result = check_ev_gate(MethodFamily::BayesianPosterior, 200, 0.05, 0.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("insufficient improvement"));
    }

    #[test]
    fn ev_gate_check_rejects_safety_regression() {
        let result = check_ev_gate(MethodFamily::BayesianPosterior, 200, 0.15, 0.01);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("safety regression"));
    }

    #[test]
    fn ev_gate_check_passes_when_all_criteria_met() {
        assert!(check_ev_gate(MethodFamily::BayesianPosterior, 200, 0.15, 0.0).is_ok());
    }

    #[test]
    fn assumptions_healthy_check() {
        // All assumptions start as Valid or Untested, so all should be healthy.
        for family in MethodFamily::ALL {
            assert!(
                assumptions_healthy(*family),
                "assumptions for {family} should be healthy initially"
            );
        }
    }

    #[test]
    fn method_ledger_sunset_criteria_non_empty() {
        for entry in METHOD_LEDGER {
            assert!(
                !entry.sunset_criteria.is_empty(),
                "missing sunset criteria for {}",
                entry.family
            );
        }
    }

    #[test]
    fn method_ledger_user_value_non_empty() {
        for entry in METHOD_LEDGER {
            assert!(
                !entry.user_value.is_empty(),
                "missing user value for {}",
                entry.family
            );
        }
    }

    #[test]
    fn method_ledger_dependencies_valid() {
        for entry in METHOD_LEDGER {
            for dep in entry.depends_on {
                assert!(
                    MethodFamily::ALL.contains(dep),
                    "{} depends on unknown family {:?}",
                    entry.family,
                    dep
                );
            }
        }
    }

    #[test]
    fn transparency_card_serde_roundtrip() {
        let card = TransparencyCard {
            card_id: 1,
            emitted_ts_micros: 1_710_741_600_000_000,
            change_type: ChangeType::PolicyPromotion,
            scope: CardScope {
                method: Some(MethodFamily::BayesianPosterior),
                subsystem: Some("liveness".to_string()),
                stratum: Some("liveness:advisory:low".to_string()),
                agents: vec!["GreenCastle".to_string()],
                project_key: Some("/data/projects/test".to_string()),
            },
            prior_policy_id: Some("pol-001".to_string()),
            new_policy_id: Some("pol-002".to_string()),
            evidence: CardEvidence {
                evidence_id: Some("evi-42".to_string()),
                observation_count: 500,
                window_micros: 3_600_000_000,
                trigger_metric: "false_advisory_rate".to_string(),
                trigger_threshold: 0.10,
                observed_value: 0.15,
                rationale: "Shadow policy reduced false advisory rate by 15% over 500 \
                            observations in the liveness:advisory:low stratum."
                    .to_string(),
            },
            rejected_alternative: Some(RejectedAlternative {
                action: "Keep current policy".to_string(),
                reason: "Current policy has 15% higher false advisory rate".to_string(),
                expected_cost: 0.15,
            }),
            assumption_keys: vec![
                "ewma_stationarity".to_string(),
                "state_space_complete".to_string(),
            ],
            override_guidance: "Set ATC_DISABLE_POLICY_PROMOTION=1 to prevent automatic \
                                policy promotion, then manually inspect the evidence ledger."
                .to_string(),
            regime_context: Some(RegimeContext {
                budget_mode: "nominal".to_string(),
                safe_mode_active: false,
                active_agent_count: 5,
                active_project_count: 2,
                tick_utilization_bp: 3500,
            }),
        };

        let json = serde_json::to_string_pretty(&card).unwrap();
        let decoded: TransparencyCard = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.card_id, card.card_id);
        assert_eq!(decoded.change_type, card.change_type);
        assert_eq!(decoded.evidence.observation_count, 500);
        assert_eq!(decoded.assumption_keys.len(), 2);
        assert!(decoded.rejected_alternative.is_some());
        assert!(decoded.regime_context.is_some());
    }

    #[test]
    fn change_type_display() {
        assert_eq!(ChangeType::PolicyPromotion.to_string(), "policy_promotion");
        assert_eq!(ChangeType::SafeModeEntry.to_string(), "safe_mode_entry");
        assert_eq!(ChangeType::RegimeShift.to_string(), "regime_shift");
    }

    #[test]
    fn escalation_criteria_all_required() {
        assert!(ESCALATION_CRITERIA.ev_gate_defined);
        assert!(ESCALATION_CRITERIA.sunset_criteria_defined);
        assert!(ESCALATION_CRITERIA.assumptions_documented);
        assert!(ESCALATION_CRITERIA.within_tick_budget);
    }

    #[test]
    fn simplification_triggers_non_empty() {
        assert!(
            SIMPLIFICATION_TRIGGERS.len() >= 5,
            "expected at least 5 simplification triggers"
        );
    }

    #[test]
    fn method_family_all_is_exhaustive() {
        // Verify ALL contains every variant by checking count matches.
        assert_eq!(
            MethodFamily::ALL.len(),
            10,
            "MethodFamily::ALL should contain all 10 variants"
        );
    }

    #[test]
    fn ledger_entry_count_matches_family_count() {
        assert_eq!(
            METHOD_LEDGER.len(),
            MethodFamily::ALL.len(),
            "every method family must have exactly one ledger entry"
        );
    }

    #[test]
    fn ev_gate_count_matches_family_count() {
        assert_eq!(
            EV_GATES.len(),
            MethodFamily::ALL.len(),
            "every method family must have exactly one EV gate"
        );
    }
}
