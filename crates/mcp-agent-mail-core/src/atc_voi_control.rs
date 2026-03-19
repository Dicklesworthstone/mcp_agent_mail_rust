//! Safe value-of-information control, identifiability debt, and targeted
//! low-risk experiment design (br-0qt6e.3.14).
//!
//! Upgrades ATC from passive learning to safe active information acquisition
//! within a strict low-risk and low-noise envelope. The system knows when
//! uncertainty is blocking improvement, maintains an explicit identifiability-debt
//! ledger, and spends tiny, budgeted experiments only when expected information
//! value clearly beats user-noise and safety cost.
//!
//! # Design Principle
//!
//! "Do not experiment" is a first-class safe outcome. Active information
//! acquisition is the exception, not the rule, and is only justified when:
//! 1. The normal policy is uncertainty-limited (not already decision-sufficient)
//! 2. The candidate experiment is low-risk and reversibly bounded
//! 3. The expected information value exceeds the user-noise cost
//! 4. The experiment fits within fairness, contamination, and budget constraints
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  Identifiability-Debt Ledger                                    │
//! │  - Tracks where evidence is insufficient for confident          │
//! │    adaptation, certification, or attribution                    │
//! │  - Per action family, cohort, policy question                   │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  Value-of-Information Scorer                                    │
//! │  - Estimates expected learning value of candidate experiments   │
//! │  - Compares against user-noise, fairness, and tail-risk cost   │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  Experiment Eligibility Gates                                   │
//! │  - Only low-risk, reversible actions qualify                    │
//! │  - Only when normal policy is uncertainty-limited               │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  Experiment Budget Tables                                       │
//! │  - Per action family, cohort, regime, time window               │
//! │  - Prevents learning pressure from becoming spam                │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  Experiment Logger                                              │
//! │  - Links experiments to experience ledger                       │
//! │  - Announces, previews, and logs information-seeking actions    │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

use crate::experience::EffectKind;

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Minimum observations before an action family can be considered
/// "decision-sufficient" (not needing active experiments).
pub const DECISION_SUFFICIENT_OBS: u64 = 100;

/// Minimum confidence level (1 - uncertainty) before experiments stop.
pub const DECISION_SUFFICIENT_CONFIDENCE: f64 = 0.80;

/// Maximum fraction of total actions that can be experiments (global cap).
pub const MAX_EXPERIMENT_FRACTION_GLOBAL: f64 = 0.05; // 5%

/// Maximum experiments per agent per hour.
pub const MAX_EXPERIMENTS_PER_AGENT_PER_HOUR: u32 = 3;

/// Maximum experiments per cohort per hour.
pub const MAX_EXPERIMENTS_PER_COHORT_PER_HOUR: u32 = 10;

/// Minimum value-of-information score before an experiment is justified.
/// Below this, "do not experiment" wins.
pub const MIN_VOI_SCORE: f64 = 0.10;

/// Maximum user-noise cost multiplier. An experiment whose noise cost
/// exceeds this fraction of its VoI is refused.
pub const MAX_NOISE_TO_VOI_RATIO: f64 = 0.50;

/// Default experiment validity window (microseconds). After this,
/// the experiment budget entry expires and must be re-evaluated.
pub const DEFAULT_EXPERIMENT_WINDOW_MICROS: i64 = 3_600_000_000; // 1 hour

// ──────────────────────────────────────────────────────────────────────
// Identifiability debt
// ──────────────────────────────────────────────────────────────────────

/// Type of identifiability gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebtType {
    /// Too few observations for reliable estimation.
    SparseData,
    /// Observations exist but are confounded by overlapping interventions.
    ConfoundedAttribution,
    /// Regime changed and old evidence was discounted.
    RegimeDiscounted,
    /// Evidence is contaminated (high suspect/quarantined fraction).
    ContaminatedEvidence,
    /// Policy comparison is uncertain because the candidate was rarely selected.
    WeakCounterfactual,
    /// Fairness constraints prevent learning from certain cohorts.
    FairnessBound,
}

impl std::fmt::Display for DebtType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SparseData => write!(f, "sparse_data"),
            Self::ConfoundedAttribution => write!(f, "confounded_attribution"),
            Self::RegimeDiscounted => write!(f, "regime_discounted"),
            Self::ContaminatedEvidence => write!(f, "contaminated_evidence"),
            Self::WeakCounterfactual => write!(f, "weak_counterfactual"),
            Self::FairnessBound => write!(f, "fairness_bound"),
        }
    }
}

/// A single entry in the identifiability-debt ledger.
///
/// Each entry tracks where ATC's evidence is insufficient for confident
/// adaptation, certification, or attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifiabilityDebt {
    /// Unique debt identifier.
    pub debt_id: u64,
    /// Which action family is affected.
    pub action_family: String,
    /// Which cohort/stratum is affected (e.g., "claude-code:project-x:advisory").
    pub cohort_key: String,
    /// What kind of identifiability gap exists.
    pub debt_type: DebtType,
    /// Current severity (0.0 = minor gap, 1.0 = completely blind).
    pub severity: f64,
    /// How many observations exist for this stratum.
    pub observation_count: u64,
    /// How many are needed for decision-sufficiency.
    pub required_count: u64,
    /// Current confidence level (0.0-1.0).
    pub current_confidence: f64,
    /// Target confidence level for this stratum.
    pub target_confidence: f64,
    /// When the debt was first identified (microseconds).
    pub identified_ts_micros: i64,
    /// When the debt was last updated (microseconds).
    pub updated_ts_micros: i64,
    /// Whether an active experiment is currently addressing this debt.
    pub experiment_active: bool,
    /// How much information gain an experiment would provide (estimated).
    pub estimated_information_gain: f64,
}

impl IdentifiabilityDebt {
    /// Whether this debt is severe enough to consider active experimentation.
    #[must_use]
    pub fn warrants_experiment(&self) -> bool {
        self.severity >= 0.30
            && self.current_confidence < DECISION_SUFFICIENT_CONFIDENCE
            && !self.experiment_active
            && self.estimated_information_gain >= MIN_VOI_SCORE
    }

    /// Whether this debt has been sufficiently resolved.
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        self.current_confidence >= self.target_confidence
            && self.observation_count >= self.required_count
    }
}

/// The identifiability-debt ledger: tracks all known evidence gaps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebtLedger {
    /// All active debt entries.
    pub entries: Vec<IdentifiabilityDebt>,
    /// Counter for generating unique debt IDs.
    pub next_debt_id: u64,
    /// When the ledger was last scanned/updated.
    pub last_scan_ts_micros: i64,
}

impl DebtLedger {
    /// Create a new empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new debt entry to the ledger.
    pub fn add_debt(
        &mut self,
        action_family: String,
        cohort_key: String,
        debt_type: DebtType,
        severity: f64,
        observation_count: u64,
        current_confidence: f64,
        now_micros: i64,
    ) -> u64 {
        let debt_id = self.next_debt_id;
        self.next_debt_id += 1;

        self.entries.push(IdentifiabilityDebt {
            debt_id,
            action_family,
            cohort_key,
            debt_type,
            severity: severity.clamp(0.0, 1.0),
            observation_count,
            required_count: DECISION_SUFFICIENT_OBS,
            current_confidence: current_confidence.clamp(0.0, 1.0),
            target_confidence: DECISION_SUFFICIENT_CONFIDENCE,
            identified_ts_micros: now_micros,
            updated_ts_micros: now_micros,
            experiment_active: false,
            estimated_information_gain: 0.0,
        });

        debt_id
    }

    /// Remove resolved debts from the ledger.
    pub fn prune_resolved(&mut self) {
        self.entries.retain(|e| !e.is_resolved());
    }

    /// Count of debts that warrant experimentation.
    #[must_use]
    pub fn experiment_worthy_count(&self) -> usize {
        self.entries.iter().filter(|e| e.warrants_experiment()).count()
    }

    /// Total active (unresolved) debts.
    #[must_use]
    pub const fn active_count(&self) -> usize {
        self.entries.len()
    }

    /// Debts sorted by severity (highest first).
    #[must_use]
    pub fn by_severity(&self) -> Vec<&IdentifiabilityDebt> {
        let mut sorted: Vec<_> = self.entries.iter().collect();
        sorted.sort_by(|a, b| b.severity.partial_cmp(&a.severity).unwrap_or(std::cmp::Ordering::Equal));
        sorted
    }
}

// ──────────────────────────────────────────────────────────────────────
// Value-of-information scoring
// ──────────────────────────────────────────────────────────────────────

/// Input for value-of-information scoring.
#[derive(Debug, Clone)]
pub struct VoIInput {
    /// The debt entry this experiment would address.
    pub debt_severity: f64,
    /// Estimated information gain from the experiment (bits or equivalent).
    pub estimated_info_gain: f64,
    /// Current posterior entropy for the relevant stratum.
    pub posterior_entropy: f64,
    /// Time since last observation in this stratum (microseconds).
    pub staleness_micros: i64,
    /// User-noise cost of the experiment (0.0-1.0).
    pub noise_cost: f64,
    /// Fairness burden increment if the experiment runs.
    pub fairness_cost: f64,
    /// Tail-risk of the experiment (probability of harmful outcome).
    pub tail_risk: f64,
    /// Whether the experiment is reversible.
    pub is_reversible: bool,
}

/// Result of value-of-information scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoIScore {
    /// Raw VoI score (information value before costs).
    pub raw_voi: f64,
    /// Noise cost penalty.
    pub noise_penalty: f64,
    /// Fairness cost penalty.
    pub fairness_penalty: f64,
    /// Tail-risk penalty.
    pub tail_risk_penalty: f64,
    /// Net VoI score after all penalties.
    pub net_voi: f64,
    /// Whether the experiment is justified (net_voi >= MIN_VOI_SCORE).
    pub justified: bool,
    /// Reason code for the decision.
    pub reason: VoIDecision,
}

/// Decision outcome for a VoI evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoIDecision {
    /// Experiment is justified: information value exceeds costs.
    ExperimentJustified,
    /// Do not experiment: information value is too low.
    InformationValueTooLow,
    /// Do not experiment: noise cost is too high relative to benefit.
    NoiseCostExcessive,
    /// Do not experiment: fairness budget would be violated.
    FairnessBudgetViolated,
    /// Do not experiment: tail risk is unacceptable.
    TailRiskUnacceptable,
    /// Do not experiment: action is irreversible.
    Irreversible,
    /// Do not experiment: already decision-sufficient.
    AlreadySufficient,
}

impl std::fmt::Display for VoIDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExperimentJustified => write!(f, "experiment_justified"),
            Self::InformationValueTooLow => write!(f, "information_value_too_low"),
            Self::NoiseCostExcessive => write!(f, "noise_cost_excessive"),
            Self::FairnessBudgetViolated => write!(f, "fairness_budget_violated"),
            Self::TailRiskUnacceptable => write!(f, "tail_risk_unacceptable"),
            Self::Irreversible => write!(f, "irreversible"),
            Self::AlreadySufficient => write!(f, "already_sufficient"),
        }
    }
}

/// Compute the value-of-information score for a candidate experiment.
///
/// Returns a `VoIScore` with the net value and decision. "Do not experiment"
/// is the default outcome — justification requires positive net value.
#[must_use]
pub fn score_voi(input: &VoIInput) -> VoIScore {
    // Irreversible actions never qualify for experimentation.
    if !input.is_reversible {
        return VoIScore {
            raw_voi: 0.0,
            noise_penalty: 0.0,
            fairness_penalty: 0.0,
            tail_risk_penalty: 0.0,
            net_voi: 0.0,
            justified: false,
            reason: VoIDecision::Irreversible,
        };
    }

    // Raw VoI: combination of information gain, debt severity, and staleness.
    // Higher entropy and staleness increase the value of new information.
    let staleness_factor = (input.staleness_micros.max(0) as f64 / 600_000_000.0).min(2.0); // clamp [0, 2×]
    let raw_voi = input.estimated_info_gain
        * input.debt_severity
        * 0.3f64.mul_add(input.posterior_entropy, 1.0)
        * 0.2f64.mul_add(staleness_factor, 1.0);

    // Cost penalties.
    let noise_penalty = input.noise_cost * 2.0; // noise is weighted 2× (user trust is precious)
    let fairness_penalty = input.fairness_cost * 1.5;
    let tail_risk_penalty = input.tail_risk * 5.0; // tail risk is weighted 5×

    let net_voi = raw_voi - noise_penalty - fairness_penalty - tail_risk_penalty;

    // Decision logic: check each refusal condition.
    let reason = if raw_voi < MIN_VOI_SCORE {
        VoIDecision::InformationValueTooLow
    } else if noise_penalty > raw_voi * MAX_NOISE_TO_VOI_RATIO {
        VoIDecision::NoiseCostExcessive
    } else if fairness_penalty > raw_voi * 0.30 {
        VoIDecision::FairnessBudgetViolated
    } else if tail_risk_penalty > raw_voi * 0.20 {
        VoIDecision::TailRiskUnacceptable
    } else if net_voi >= MIN_VOI_SCORE {
        VoIDecision::ExperimentJustified
    } else {
        VoIDecision::InformationValueTooLow
    };

    VoIScore {
        raw_voi,
        noise_penalty,
        fairness_penalty,
        tail_risk_penalty,
        net_voi,
        justified: reason == VoIDecision::ExperimentJustified,
        reason,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Experiment eligibility gates
// ──────────────────────────────────────────────────────────────────────

/// Result of checking experiment eligibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EligibilityResult {
    /// Whether the experiment is eligible.
    pub eligible: bool,
    /// All gate results (not short-circuited).
    pub gates: Vec<EligibilityGate>,
    /// Overall refusal reason (if not eligible).
    pub refusal: Option<ExperimentRefusalCode>,
}

/// A single eligibility gate check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EligibilityGate {
    /// Gate name.
    pub name: String,
    /// Whether this gate passed.
    pub passed: bool,
    /// Description of why it passed or failed.
    pub description: String,
}

/// Machine-readable refusal codes for experiment denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentRefusalCode {
    /// Action is high-risk (Release, ForceReservation).
    HighRiskAction,
    /// Regime is unstable.
    RegimeUnstable,
    /// Evidence is contaminated.
    EvidenceContaminated,
    /// Budget for this cohort/family is exhausted.
    BudgetExhausted,
    /// Normal policy is already decision-sufficient.
    AlreadySufficient,
    /// Fairness budget would be violated.
    FairnessViolation,
    /// Safe mode is active.
    SafeModeActive,
    /// VoI score is too low.
    VoIInsufficient,
}

impl std::fmt::Display for ExperimentRefusalCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HighRiskAction => write!(f, "high_risk_action"),
            Self::RegimeUnstable => write!(f, "regime_unstable"),
            Self::EvidenceContaminated => write!(f, "evidence_contaminated"),
            Self::BudgetExhausted => write!(f, "budget_exhausted"),
            Self::AlreadySufficient => write!(f, "already_sufficient"),
            Self::FairnessViolation => write!(f, "fairness_violation"),
            Self::SafeModeActive => write!(f, "safe_mode_active"),
            Self::VoIInsufficient => write!(f, "voi_insufficient"),
        }
    }
}

/// Context for experiment eligibility checking.
#[derive(Debug, Clone)]
pub struct EligibilityContext {
    /// The effect kind of the proposed experiment.
    pub effect_kind: EffectKind,
    /// Whether the regime is currently stable.
    pub regime_stable: bool,
    /// Whether safe mode is active.
    pub safe_mode_active: bool,
    /// Fraction of evidence that is trusted quality.
    pub trusted_evidence_fraction: f64,
    /// Whether the stratum is already decision-sufficient.
    pub decision_sufficient: bool,
    /// Current experiment budget usage for this cohort.
    pub budget_used: u32,
    /// Maximum experiments allowed for this cohort.
    pub budget_max: u32,
    /// Whether the fairness budget allows this experiment.
    pub fairness_allows: bool,
    /// The VoI score for this experiment.
    pub voi_score: f64,
}

/// Check all eligibility gates for a candidate experiment.
///
/// Returns all gate results (not short-circuited) so the caller can
/// explain every reason an experiment was denied.
#[must_use]
pub fn check_eligibility(ctx: &EligibilityContext) -> EligibilityResult {
    let mut gates = Vec::new();
    let mut first_refusal = None;

    // Gate 1: Effect kind must be low or medium risk.
    let kind_ok = !ctx.effect_kind.is_high_force();
    gates.push(EligibilityGate {
        name: "risk_level".to_string(),
        passed: kind_ok,
        description: if kind_ok {
            "effect kind is low/medium risk".to_string()
        } else {
            "high-risk effects (Release, ForceReservation) cannot be experiments".to_string()
        },
    });
    if !kind_ok && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::HighRiskAction);
    }

    // Gate 2: Regime must be stable.
    gates.push(EligibilityGate {
        name: "regime_stability".to_string(),
        passed: ctx.regime_stable,
        description: if ctx.regime_stable {
            "regime is stable".to_string()
        } else {
            "experiments are blocked during regime transitions".to_string()
        },
    });
    if !ctx.regime_stable && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::RegimeUnstable);
    }

    // Gate 3: Safe mode must not be active.
    gates.push(EligibilityGate {
        name: "safe_mode".to_string(),
        passed: !ctx.safe_mode_active,
        description: if ctx.safe_mode_active {
            "experiments are blocked during safe mode".to_string()
        } else {
            "safe mode inactive".to_string()
        },
    });
    if ctx.safe_mode_active && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::SafeModeActive);
    }

    // Gate 4: Evidence quality must be sufficient.
    let evidence_ok = ctx.trusted_evidence_fraction >= 0.50;
    gates.push(EligibilityGate {
        name: "evidence_quality".to_string(),
        passed: evidence_ok,
        description: if evidence_ok {
            format!("trusted evidence fraction {:.0}% >= 50%", ctx.trusted_evidence_fraction * 100.0)
        } else {
            format!("trusted evidence fraction {:.0}% < 50%", ctx.trusted_evidence_fraction * 100.0)
        },
    });
    if !evidence_ok && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::EvidenceContaminated);
    }

    // Gate 5: Must not already be decision-sufficient.
    gates.push(EligibilityGate {
        name: "uncertainty_limited".to_string(),
        passed: !ctx.decision_sufficient,
        description: if ctx.decision_sufficient {
            "stratum is already decision-sufficient, no experiment needed".to_string()
        } else {
            "stratum is uncertainty-limited, experiments may help".to_string()
        },
    });
    if ctx.decision_sufficient && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::AlreadySufficient);
    }

    // Gate 6: Budget must not be exhausted.
    let budget_ok = ctx.budget_used < ctx.budget_max;
    gates.push(EligibilityGate {
        name: "experiment_budget".to_string(),
        passed: budget_ok,
        description: if budget_ok {
            format!("budget: {}/{} used", ctx.budget_used, ctx.budget_max)
        } else {
            format!("budget exhausted: {}/{}", ctx.budget_used, ctx.budget_max)
        },
    });
    if !budget_ok && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::BudgetExhausted);
    }

    // Gate 7: Fairness must allow.
    gates.push(EligibilityGate {
        name: "fairness".to_string(),
        passed: ctx.fairness_allows,
        description: if ctx.fairness_allows {
            "fairness budget allows this experiment".to_string()
        } else {
            "experiment would violate fairness constraints".to_string()
        },
    });
    if !ctx.fairness_allows && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::FairnessViolation);
    }

    // Gate 8: VoI must be sufficient.
    let voi_ok = ctx.voi_score >= MIN_VOI_SCORE;
    gates.push(EligibilityGate {
        name: "voi_threshold".to_string(),
        passed: voi_ok,
        description: if voi_ok {
            format!("VoI score {:.3} >= {MIN_VOI_SCORE:.3}", ctx.voi_score)
        } else {
            format!("VoI score {:.3} < {MIN_VOI_SCORE:.3}: not worth experimenting", ctx.voi_score)
        },
    });
    if !voi_ok && first_refusal.is_none() {
        first_refusal = Some(ExperimentRefusalCode::VoIInsufficient);
    }

    let eligible = first_refusal.is_none();

    EligibilityResult {
        eligible,
        gates,
        refusal: first_refusal,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Experiment budget tables
// ──────────────────────────────────────────────────────────────────────

/// Budget entry for a specific (action family, cohort, time window) triple.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentBudget {
    /// Action family this budget applies to.
    pub action_family: String,
    /// Cohort key (e.g., "claude-code:project-x").
    pub cohort_key: String,
    /// Maximum experiments in the current window.
    pub max_experiments: u32,
    /// Experiments used in the current window.
    pub used_experiments: u32,
    /// Window start (microseconds).
    pub window_start_micros: i64,
    /// Window duration (microseconds).
    pub window_duration_micros: i64,
}

impl ExperimentBudget {
    /// Whether the budget has capacity for another experiment.
    #[must_use] 
    pub const fn has_capacity(&self) -> bool {
        self.used_experiments < self.max_experiments
    }

    /// Whether the window has expired.
    #[must_use] 
    pub const fn is_expired(&self, now_micros: i64) -> bool {
        now_micros.saturating_sub(self.window_start_micros) > self.window_duration_micros
    }

    /// Consume one experiment from the budget. Returns false if exhausted.
    pub const fn consume(&mut self) -> bool {
        if self.has_capacity() {
            self.used_experiments += 1;
            true
        } else {
            false
        }
    }

    /// Reset the budget for a new window.
    pub const fn reset(&mut self, now_micros: i64) {
        self.used_experiments = 0;
        self.window_start_micros = now_micros;
    }
}

/// Budget table: manages experiment budgets across families and cohorts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExperimentBudgetTable {
    /// All budget entries, keyed by "family:cohort".
    pub entries: Vec<ExperimentBudget>,
    /// Global experiment counter for the current regime.
    pub global_experiment_count: u64,
    /// Total actions (for computing experiment fraction).
    pub global_action_count: u64,
}

impl ExperimentBudgetTable {
    /// Create a new empty budget table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create a budget entry for the given family and cohort.
    pub fn get_or_create(
        &mut self,
        action_family: &str,
        cohort_key: &str,
        now_micros: i64,
    ) -> &mut ExperimentBudget {
        // Find existing entry.
        let idx = self.entries.iter().position(|e| {
            e.action_family == action_family && e.cohort_key == cohort_key
        });

        if let Some(i) = idx {
            let entry = &mut self.entries[i];
            if entry.is_expired(now_micros) {
                entry.reset(now_micros);
            }
            return &mut self.entries[i];
        }

        // Create new entry.
        self.entries.push(ExperimentBudget {
            action_family: action_family.to_string(),
            cohort_key: cohort_key.to_string(),
            max_experiments: MAX_EXPERIMENTS_PER_COHORT_PER_HOUR,
            used_experiments: 0,
            window_start_micros: now_micros,
            window_duration_micros: DEFAULT_EXPERIMENT_WINDOW_MICROS,
        });
        self.entries.last_mut().unwrap()
    }

    /// Whether the global experiment fraction is within limits.
    #[must_use] 
    pub fn global_fraction_ok(&self) -> bool {
        if self.global_action_count == 0 {
            return true;
        }
        (self.global_experiment_count as f64 / self.global_action_count as f64)
            <= MAX_EXPERIMENT_FRACTION_GLOBAL
    }

    /// Record an experiment being executed.
    pub const fn record_experiment(&mut self) {
        self.global_experiment_count += 1;
    }

    /// Record any action being executed (for fraction tracking).
    pub const fn record_action(&mut self) {
        self.global_action_count += 1;
    }

    /// Global experiment rate (as a fraction).
    #[must_use] 
    pub fn global_experiment_rate(&self) -> f64 {
        if self.global_action_count == 0 {
            return 0.0;
        }
        
        // Use lossy cast as acceptable for monitoring rates
        #[allow(clippy::cast_precision_loss)]
        (self.global_experiment_count as f64 / self.global_action_count as f64)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Experiment record
// ──────────────────────────────────────────────────────────────────────

/// A logged experiment with its motivation, design, and linkage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentRecord {
    /// Unique experiment ID.
    pub experiment_id: u64,
    /// Which debt entry motivated this experiment.
    pub debt_id: u64,
    /// The action family being experimented on.
    pub action_family: String,
    /// The cohort/stratum being targeted.
    pub cohort_key: String,
    /// The effect kind used for the experiment.
    pub effect_kind: EffectKind,
    /// The agent targeted by the experiment.
    pub target_agent: String,
    /// VoI score that justified the experiment.
    pub voi_score: f64,
    /// VoI decision that authorized it.
    pub voi_decision: VoIDecision,
    /// Whether the experiment has both control and information value.
    pub dual_purpose: bool,
    /// Experience ID linked to this experiment (when available).
    pub experience_id: Option<u64>,
    /// Decision ID that generated this experiment.
    pub decision_id: u64,
    /// When the experiment was created (microseconds).
    pub created_ts_micros: i64,
    /// Status of the experiment.
    pub status: ExperimentStatus,
    /// Regime ID during the experiment.
    pub regime_id: u64,
}

/// Status of an experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    /// Experiment is planned but not yet executed.
    Planned,
    /// Experiment has been dispatched.
    Dispatched,
    /// Experiment outcome has been observed.
    Observed,
    /// Experiment was cancelled (e.g., regime change, budget exhaustion).
    Cancelled,
}

impl std::fmt::Display for ExperimentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Planned => write!(f, "planned"),
            Self::Dispatched => write!(f, "dispatched"),
            Self::Observed => write!(f, "observed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Debt ledger ──

    #[test]
    fn test_debt_ledger_add_and_count() {
        let mut ledger = DebtLedger::new();
        ledger.add_debt(
            "advisory".to_string(),
            "claude-code:project-x".to_string(),
            DebtType::SparseData,
            0.7,
            30,
            0.50,
            1_000_000,
        );
        assert_eq!(ledger.active_count(), 1);
    }

    #[test]
    fn test_debt_resolved_when_sufficient() {
        let mut ledger = DebtLedger::new();
        ledger.add_debt(
            "probe".to_string(),
            "cohort".to_string(),
            DebtType::SparseData,
            0.5,
            200, // meets required_count
            0.90, // meets target_confidence
            1_000_000,
        );
        assert!(ledger.entries[0].is_resolved());
        ledger.prune_resolved();
        assert_eq!(ledger.active_count(), 0);
    }

    #[test]
    fn test_debt_warrants_experiment() {
        let mut ledger = DebtLedger::new();
        let id = ledger.add_debt(
            "advisory".to_string(),
            "cohort".to_string(),
            DebtType::SparseData,
            0.5, // severity >= 0.30
            30,
            0.40, // below DECISION_SUFFICIENT_CONFIDENCE
            1_000_000,
        );
        ledger.entries[0].estimated_information_gain = 0.20; // above MIN_VOI_SCORE
        assert!(ledger.entries[0].warrants_experiment());
        assert_eq!(id, 0);
    }

    #[test]
    fn test_debt_does_not_warrant_experiment_when_active() {
        let mut ledger = DebtLedger::new();
        ledger.add_debt(
            "advisory".to_string(),
            "cohort".to_string(),
            DebtType::SparseData,
            0.5,
            30,
            0.40,
            1_000_000,
        );
        ledger.entries[0].estimated_information_gain = 0.20;
        ledger.entries[0].experiment_active = true;
        assert!(!ledger.entries[0].warrants_experiment());
    }

    #[test]
    fn test_debt_by_severity() {
        let mut ledger = DebtLedger::new();
        ledger.add_debt("a".into(), "c".into(), DebtType::SparseData, 0.3, 10, 0.5, 0);
        ledger.add_debt("b".into(), "c".into(), DebtType::SparseData, 0.9, 10, 0.5, 0);
        ledger.add_debt("c".into(), "c".into(), DebtType::SparseData, 0.6, 10, 0.5, 0);
        let sorted = ledger.by_severity();
        assert_eq!(sorted[0].action_family, "b");
        assert_eq!(sorted[1].action_family, "c");
        assert_eq!(sorted[2].action_family, "a");
    }

    // ── VoI scoring ──

    #[test]
    fn test_voi_irreversible_refused() {
        let input = VoIInput {
            debt_severity: 0.8,
            estimated_info_gain: 0.5,
            posterior_entropy: 1.0,
            staleness_micros: 600_000_000,
            noise_cost: 0.0,
            fairness_cost: 0.0,
            tail_risk: 0.0,
            is_reversible: false,
        };
        let score = score_voi(&input);
        assert!(!score.justified);
        assert_eq!(score.reason, VoIDecision::Irreversible);
    }

    #[test]
    fn test_voi_justified_with_good_inputs() {
        let input = VoIInput {
            debt_severity: 0.7,
            estimated_info_gain: 0.5,
            posterior_entropy: 1.5,
            staleness_micros: 300_000_000,
            noise_cost: 0.02,
            fairness_cost: 0.01,
            tail_risk: 0.001,
            is_reversible: true,
        };
        let score = score_voi(&input);
        assert!(score.justified, "Expected justified: score={:.4}, reason={}", score.net_voi, score.reason);
        assert_eq!(score.reason, VoIDecision::ExperimentJustified);
    }

    #[test]
    fn test_voi_refused_high_noise() {
        let input = VoIInput {
            debt_severity: 0.3,
            estimated_info_gain: 0.2,
            posterior_entropy: 0.5,
            staleness_micros: 60_000_000,
            noise_cost: 0.5, // very noisy
            fairness_cost: 0.0,
            tail_risk: 0.0,
            is_reversible: true,
        };
        let score = score_voi(&input);
        assert!(!score.justified);
        // Either noise_cost_excessive or information_value_too_low.
        assert!(matches!(
            score.reason,
            VoIDecision::NoiseCostExcessive | VoIDecision::InformationValueTooLow
        ));
    }

    #[test]
    fn test_voi_refused_high_tail_risk() {
        let input = VoIInput {
            debt_severity: 0.5,
            estimated_info_gain: 0.3,
            posterior_entropy: 1.0,
            staleness_micros: 300_000_000,
            noise_cost: 0.01,
            fairness_cost: 0.01,
            tail_risk: 0.3, // high tail risk
            is_reversible: true,
        };
        let score = score_voi(&input);
        assert!(!score.justified);
        assert_eq!(score.reason, VoIDecision::TailRiskUnacceptable);
    }

    // ── Eligibility gates ──

    fn default_eligibility_ctx() -> EligibilityContext {
        EligibilityContext {
            effect_kind: EffectKind::Advisory,
            regime_stable: true,
            safe_mode_active: false,
            trusted_evidence_fraction: 0.80,
            decision_sufficient: false,
            budget_used: 2,
            budget_max: 10,
            fairness_allows: true,
            voi_score: 0.20,
        }
    }

    #[test]
    fn test_eligibility_all_pass() {
        let ctx = default_eligibility_ctx();
        let result = check_eligibility(&ctx);
        assert!(result.eligible, "Expected eligible: {:?}", result.gates);
        assert!(result.refusal.is_none());
    }

    #[test]
    fn test_eligibility_refuses_high_risk() {
        let mut ctx = default_eligibility_ctx();
        ctx.effect_kind = EffectKind::Release;
        let result = check_eligibility(&ctx);
        assert!(!result.eligible);
        assert_eq!(result.refusal, Some(ExperimentRefusalCode::HighRiskAction));
    }

    #[test]
    fn test_eligibility_refuses_unstable_regime() {
        let mut ctx = default_eligibility_ctx();
        ctx.regime_stable = false;
        let result = check_eligibility(&ctx);
        assert!(!result.eligible);
        assert_eq!(result.refusal, Some(ExperimentRefusalCode::RegimeUnstable));
    }

    #[test]
    fn test_eligibility_refuses_safe_mode() {
        let mut ctx = default_eligibility_ctx();
        ctx.safe_mode_active = true;
        let result = check_eligibility(&ctx);
        assert!(!result.eligible);
        assert_eq!(result.refusal, Some(ExperimentRefusalCode::SafeModeActive));
    }

    #[test]
    fn test_eligibility_refuses_already_sufficient() {
        let mut ctx = default_eligibility_ctx();
        ctx.decision_sufficient = true;
        let result = check_eligibility(&ctx);
        assert!(!result.eligible);
        assert_eq!(result.refusal, Some(ExperimentRefusalCode::AlreadySufficient));
    }

    #[test]
    fn test_eligibility_refuses_budget_exhausted() {
        let mut ctx = default_eligibility_ctx();
        ctx.budget_used = 10;
        ctx.budget_max = 10;
        let result = check_eligibility(&ctx);
        assert!(!result.eligible);
        assert_eq!(result.refusal, Some(ExperimentRefusalCode::BudgetExhausted));
    }

    #[test]
    fn test_eligibility_all_gates_checked() {
        let mut ctx = default_eligibility_ctx();
        ctx.effect_kind = EffectKind::Release;
        ctx.regime_stable = false;
        ctx.safe_mode_active = true;
        let result = check_eligibility(&ctx);
        assert!(!result.eligible);
        // All 8 gates should be checked (not short-circuited).
        assert_eq!(result.gates.len(), 8);
    }

    // ── Budget table ──

    #[test]
    fn test_budget_has_capacity() {
        let budget = ExperimentBudget {
            action_family: "advisory".into(),
            cohort_key: "test".into(),
            max_experiments: 5,
            used_experiments: 3,
            window_start_micros: 0,
            window_duration_micros: DEFAULT_EXPERIMENT_WINDOW_MICROS,
        };
        assert!(budget.has_capacity());
    }

    #[test]
    fn test_budget_exhausted() {
        let budget = ExperimentBudget {
            action_family: "advisory".into(),
            cohort_key: "test".into(),
            max_experiments: 5,
            used_experiments: 5,
            window_start_micros: 0,
            window_duration_micros: DEFAULT_EXPERIMENT_WINDOW_MICROS,
        };
        assert!(!budget.has_capacity());
    }

    #[test]
    fn test_budget_consume() {
        let mut budget = ExperimentBudget {
            action_family: "advisory".into(),
            cohort_key: "test".into(),
            max_experiments: 2,
            used_experiments: 1,
            window_start_micros: 0,
            window_duration_micros: DEFAULT_EXPERIMENT_WINDOW_MICROS,
        };
        assert!(budget.consume());
        assert!(!budget.consume()); // exhausted
    }

    #[test]
    fn test_budget_table_global_fraction() {
        let mut table = ExperimentBudgetTable::new();
        table.global_action_count = 100;
        table.global_experiment_count = 4;
        assert!(table.global_fraction_ok()); // 4% < 5%

        table.global_experiment_count = 6;
        assert!(!table.global_fraction_ok()); // 6% > 5%
    }

    #[test]
    fn test_budget_table_get_or_create() {
        let mut table = ExperimentBudgetTable::new();
        let entry = table.get_or_create("advisory", "cohort", 1_000_000);
        assert!(entry.has_capacity());
        assert_eq!(entry.used_experiments, 0);
    }

    // ── Debt type display ──

    #[test]
    fn test_debt_type_display() {
        assert_eq!(DebtType::SparseData.to_string(), "sparse_data");
        assert_eq!(DebtType::ConfoundedAttribution.to_string(), "confounded_attribution");
        assert_eq!(DebtType::RegimeDiscounted.to_string(), "regime_discounted");
        assert_eq!(DebtType::ContaminatedEvidence.to_string(), "contaminated_evidence");
        assert_eq!(DebtType::WeakCounterfactual.to_string(), "weak_counterfactual");
        assert_eq!(DebtType::FairnessBound.to_string(), "fairness_bound");
    }

    #[test]
    fn test_experiment_status_display() {
        assert_eq!(ExperimentStatus::Planned.to_string(), "planned");
        assert_eq!(ExperimentStatus::Dispatched.to_string(), "dispatched");
        assert_eq!(ExperimentStatus::Observed.to_string(), "observed");
        assert_eq!(ExperimentStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_refusal_code_display() {
        assert_eq!(ExperimentRefusalCode::HighRiskAction.to_string(), "high_risk_action");
        assert_eq!(ExperimentRefusalCode::VoIInsufficient.to_string(), "voi_insufficient");
    }

    #[test]
    fn test_voi_decision_display() {
        assert_eq!(VoIDecision::ExperimentJustified.to_string(), "experiment_justified");
        assert_eq!(VoIDecision::Irreversible.to_string(), "irreversible");
    }
}
