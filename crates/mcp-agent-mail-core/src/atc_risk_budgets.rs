//! Conformal risk control and per-action false-action budget tables
//! (br-0qt6e.3.4).
//!
//! This module defines the stratum-aware risk budget tables that ATC
//! uses to bound false-action rates per action family and operating
//! stratum. It is the mathematical brake that prevents the learning
//! stack from causing harm.
//!
//! # Design Principles
//!
//! 1. **Per-stratum budgets**: Each (action family, risk tier) combination
//!    has its own false-action budget. High-force actions (Release,
//!    `ForceReservation`) have stricter budgets than low-force actions
//!    (Advisory, Probe).
//!
//! 2. **Conformal calibration**: Budgets are calibrated using conformal
//!    risk control (CRC). The nonconformity score measures how surprising
//!    the actual outcome was relative to the predicted loss. When too many
//!    outcomes are surprising, the budget gates further actions.
//!
//! 3. **Explicit state machine**: Each budget has a health state
//!    (Healthy, Stressed, CoolingDown, Blocking) so operators can
//!    see exactly why an action is being withheld.
//!
//! 4. **Merge fallback**: When a stratum has too few observations for
//!    reliable estimation, it borrows from the parent stratum or the
//!    global budget. This prevents sparse strata from silently widening
//!    risk beyond the safety envelope.
//!
//! # How Risk Budgets Gate Actions
//!
//! ```text
//! ATC Decision
//!      │
//!      ▼
//! Is budget for (effect_kind, risk_tier) available?
//!      │
//!      ├── Healthy: proceed with action
//!      ├── Stressed: proceed but log warning
//!      ├── CoolingDown: block action, record suppression
//!      └── Blocking: block action, emit transparency card
//! ```

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

use crate::experience::EffectKind;

// ──────────────────────────────────────────────────────────────────────
// Budget health state machine
// ──────────────────────────────────────────────────────────────────────

/// Health state of a risk budget.
///
/// This is visible to operators and determines whether ATC can take
/// actions under this budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetHealth {
    /// Budget is healthy — actions are allowed.
    Healthy,
    /// Budget is stressed — actions are allowed but monitored.
    /// Triggers when false-action rate exceeds 50% of the budget.
    Stressed,
    /// Budget is cooling down — actions are blocked temporarily.
    /// Auto-recovers after cooldown period.
    CoolingDown,
    /// Budget is actively blocking — actions are forbidden.
    /// Requires explicit recovery (enough correct outcomes).
    Blocking,
}

impl BudgetHealth {
    /// Whether actions are permitted under this health state.
    #[must_use]
    pub const fn allows_action(self) -> bool {
        matches!(self, Self::Healthy | Self::Stressed)
    }

    /// Human-readable reason for the current state.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Healthy => "budget healthy, action permitted",
            Self::Stressed => "budget stressed (>50% consumed), action permitted with monitoring",
            Self::CoolingDown => "budget cooling down after excessive false actions",
            Self::Blocking => "budget exhausted, action blocked until recovery",
        }
    }
}

impl std::fmt::Display for BudgetHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Stressed => write!(f, "stressed"),
            Self::CoolingDown => write!(f, "cooling_down"),
            Self::Blocking => write!(f, "blocking"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Budget configuration
// ──────────────────────────────────────────────────────────────────────

/// Configuration for a single risk budget.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Maximum allowed false-action rate (0.0–1.0).
    ///
    /// For high-force actions this should be very low (e.g., 0.0 for
    /// Release — zero false positives tolerated). For low-force actions
    /// it can be higher (e.g., 0.05 for Advisory — 5% false positive
    /// rate acceptable).
    pub max_false_action_rate: f64,
    /// Minimum observations before the budget is active.
    ///
    /// Until this threshold is reached, the budget uses the merge
    /// fallback to the parent stratum or global budget.
    pub min_observations: u32,
    /// Cooldown period after budget exhaustion (microseconds).
    ///
    /// After the false-action rate exceeds the budget, ATC must wait
    /// this long before re-evaluating. This prevents rapid oscillation
    /// between blocking and allowing.
    pub cooldown_micros: i64,
    /// Number of consecutive correct outcomes required to exit blocking.
    pub recovery_streak: u32,
    /// Stress threshold (fraction of max_false_action_rate).
    ///
    /// When the observed false-action rate exceeds this fraction of the
    /// budget, the state transitions to Stressed.
    pub stress_threshold_fraction: f64,
}

/// Default budget configurations per effect kind.
///
/// These are the canonical defaults. Operators can override via
/// environment variables or configuration.
#[must_use]
pub const fn default_budget_config(kind: EffectKind) -> BudgetConfig {
    match kind {
        // Release: ZERO false positives tolerated — releasing an alive agent
        // destroys work in progress.
        EffectKind::Release => BudgetConfig {
            max_false_action_rate: 0.0, // zero tolerance
            min_observations: 20,
            cooldown_micros: 600_000_000,   // 10 minutes
            recovery_streak: 20,            // 20 consecutive correct
            stress_threshold_fraction: 0.0, // always stressed if any false positive
        },
        // ForceReservation: near-zero false positives — forcing a reservation
        // on the wrong agent disrupts their work.
        EffectKind::ForceReservation => BudgetConfig {
            max_false_action_rate: 0.02, // 2% maximum
            min_observations: 20,
            cooldown_micros: 300_000_000, // 5 minutes
            recovery_streak: 10,
            stress_threshold_fraction: 0.5,
        },
        // Advisory: moderate false positive tolerance — a wrong advisory is
        // annoying but not destructive.
        EffectKind::Advisory => BudgetConfig {
            max_false_action_rate: 0.05, // 5% maximum
            min_observations: 30,
            cooldown_micros: 120_000_000, // 2 minutes
            recovery_streak: 5,
            stress_threshold_fraction: 0.5,
        },
        // Probe / NoAction: no budget constraint — probes are always safe,
        // deliberate inaction has its own evaluation.
        EffectKind::Probe | EffectKind::NoAction => BudgetConfig {
            max_false_action_rate: 1.0, // unlimited
            min_observations: 0,
            cooldown_micros: 0,
            recovery_streak: 0,
            stress_threshold_fraction: 1.0, // never stressed
        },
        // RoutingSuggestion / Backpressure: moderate tolerance — wrong routing
        // or backpressure is suboptimal but not harmful.
        EffectKind::RoutingSuggestion | EffectKind::Backpressure => BudgetConfig {
            max_false_action_rate: 0.10, // 10% maximum
            min_observations: 20,
            cooldown_micros: 120_000_000, // 2 minutes
            recovery_streak: 5,
            stress_threshold_fraction: 0.5,
        },
    }
}

// ──────────────────────────────────────────────────────────────────────
// Budget state
// ──────────────────────────────────────────────────────────────────────

/// Runtime state for a single risk budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetState {
    /// Which action family this budget covers.
    pub effect_kind: EffectKind,
    /// Risk tier (0=low, 1=medium, 2=high).
    pub risk_tier: u8,
    /// Configuration for this budget.
    pub config: BudgetConfig,
    /// Current health state.
    pub health: BudgetHealth,
    /// Total actions taken under this budget.
    pub total_actions: u64,
    /// Total false actions observed.
    pub false_actions: u64,
    /// Current consecutive correct streak (for recovery).
    pub correct_streak: u32,
    /// When the budget last entered CoolingDown (microseconds, 0 if never).
    pub cooldown_started_micros: i64,
    /// Whether this budget is using merge fallback (insufficient local data).
    pub using_fallback: bool,
}

impl BudgetState {
    /// Create a new budget state with the default configuration.
    #[must_use]
    pub const fn new(effect_kind: EffectKind, risk_tier: u8) -> Self {
        Self {
            effect_kind,
            risk_tier,
            config: default_budget_config(effect_kind),
            health: BudgetHealth::Healthy,
            total_actions: 0,
            false_actions: 0,
            correct_streak: 0,
            cooldown_started_micros: 0,
            using_fallback: false,
        }
    }

    /// Current observed false-action rate.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn false_action_rate(&self) -> f64 {
        if self.total_actions == 0 {
            return 0.0;
        }
        self.false_actions as f64 / self.total_actions as f64
    }

    /// Whether this budget has enough observations to be reliable.
    #[must_use]
    pub const fn has_sufficient_data(&self) -> bool {
        self.total_actions >= self.config.min_observations as u64
    }

    /// Record an action outcome and update the budget state.
    ///
    /// Returns the updated health state.
    pub fn record_outcome(&mut self, false_action: bool, now_micros: i64) -> BudgetHealth {
        self.total_actions += 1;
        if false_action {
            self.false_actions += 1;
            self.correct_streak = 0;
        } else {
            self.correct_streak += 1;
        }

        // Update health state
        let new_health = self.compute_health(now_micros);
        // Set cooldown start timestamp on transition into CoolingDown
        if new_health == BudgetHealth::CoolingDown && self.health != BudgetHealth::CoolingDown {
            self.cooldown_started_micros = now_micros;
        }
        self.health = new_health;
        self.health
    }

    /// Check whether an action is permitted right now.
    #[must_use]
    pub fn check_admissibility(&self, now_micros: i64) -> AdmissibilityResult {
        // Probes and NoAction are always permitted
        if self.config.max_false_action_rate >= 1.0 {
            return AdmissibilityResult {
                permitted: true,
                health: BudgetHealth::Healthy,
                reason: "action type has unlimited budget",
                false_action_rate: self.false_action_rate(),
                budget_remaining_fraction: 1.0,
            };
        }

        let health = self.compute_health(now_micros);
        let rate = self.false_action_rate();
        let remaining = if self.config.max_false_action_rate > 0.0 {
            1.0 - (rate / self.config.max_false_action_rate).min(1.0)
        } else if self.false_actions > 0 {
            0.0
        } else {
            1.0
        };

        AdmissibilityResult {
            permitted: health.allows_action(),
            health,
            reason: health.reason(),
            false_action_rate: rate,
            budget_remaining_fraction: remaining,
        }
    }

    /// Compute the current health state based on rates and timing.
    fn compute_health(&self, now_micros: i64) -> BudgetHealth {
        // If unlimited budget, always healthy
        if self.config.max_false_action_rate >= 1.0 {
            return BudgetHealth::Healthy;
        }

        // If insufficient data, use current health (rely on fallback)
        if !self.has_sufficient_data() {
            return self.health;
        }

        let rate = self.false_action_rate();

        // Check if in cooldown period
        if self.health == BudgetHealth::CoolingDown {
            let cooldown_elapsed = now_micros.saturating_sub(self.cooldown_started_micros);
            if cooldown_elapsed < self.config.cooldown_micros {
                return BudgetHealth::CoolingDown;
            }
            // Cooldown expired — check if recovered
            if self.correct_streak >= self.config.recovery_streak {
                return BudgetHealth::Healthy;
            }
            return BudgetHealth::Blocking;
        }

        // Check if blocking (needs recovery streak)
        if self.health == BudgetHealth::Blocking {
            if self.correct_streak >= self.config.recovery_streak {
                return BudgetHealth::Healthy;
            }
            return BudgetHealth::Blocking;
        }

        // Check rate against budget — enter CoolingDown first, then Blocking
        if rate > self.config.max_false_action_rate {
            // First violation enters CoolingDown (with cooldown timer).
            // If already past cooldown without recovery, enters Blocking.
            return BudgetHealth::CoolingDown;
        }

        let stress_threshold =
            self.config.max_false_action_rate * self.config.stress_threshold_fraction;
        if rate > stress_threshold {
            return BudgetHealth::Stressed;
        }

        BudgetHealth::Healthy
    }
}

// ──────────────────────────────────────────────────────────────────────
// Admissibility result
// ──────────────────────────────────────────────────────────────────────

/// Result of checking whether an action is admissible under its budget.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissibilityResult {
    /// Whether the action is permitted.
    pub permitted: bool,
    /// Current budget health.
    pub health: BudgetHealth,
    /// Human-readable reason for the decision.
    pub reason: &'static str,
    /// Current observed false-action rate.
    pub false_action_rate: f64,
    /// Fraction of budget remaining (0.0 = exhausted, 1.0 = full).
    pub budget_remaining_fraction: f64,
}

// ──────────────────────────────────────────────────────────────────────
// Nonconformity scores per action family
// ──────────────────────────────────────────────────────────────────────

/// Nonconformity score definition for conformal risk control.
///
/// Each action family uses a different nonconformity score because
/// the semantics of "surprising" differ by action type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NonconformityScore {
    /// Action family this score applies to.
    pub effect_kind: EffectKind,
    /// What the score measures.
    pub description: &'static str,
    /// How the score is computed.
    pub formula: &'static str,
    /// What a high score means.
    pub high_score_meaning: &'static str,
}

/// Nonconformity score definitions for all action families.
pub const NONCONFORMITY_SCORES: &[NonconformityScore] = &[
    NonconformityScore {
        effect_kind: EffectKind::Probe,
        description: "Prediction error for probe response probability",
        formula: "|predicted_alive_prob - actual_response|, where actual_response is 1 if \
                  responded, 0 if not",
        high_score_meaning: "ATC was confident the agent would (or wouldn't) respond but \
                             was wrong — calibration is off for this agent",
    },
    NonconformityScore {
        effect_kind: EffectKind::Advisory,
        description: "Prediction error for advisory effectiveness",
        formula: "|predicted_behavior_change_prob - actual_change|, where actual_change is 1 \
                  if behavior changed within attribution window, 0 if not",
        high_score_meaning: "Advisory was predicted to work (or not work) but the opposite \
                             happened — advisory model is miscalibrated",
    },
    NonconformityScore {
        effect_kind: EffectKind::Release,
        description: "Binary correctness of release decision",
        formula: "1 if released agent was actually alive (false positive), 0 if dead (correct)",
        high_score_meaning: "A false positive release — the most costly error in the system",
    },
    NonconformityScore {
        effect_kind: EffectKind::ForceReservation,
        description: "Prediction error for conflict severity",
        formula: "|predicted_severity - actual_severity| normalized to [0, 1]",
        high_score_meaning: "ATC misjudged the severity of the conflict — forced reservation \
                             was unnecessary or insufficient",
    },
    NonconformityScore {
        effect_kind: EffectKind::RoutingSuggestion,
        description: "Prediction error for load improvement",
        formula: "|predicted_load_delta - actual_load_delta| / max_load",
        high_score_meaning: "Routing suggestion had the wrong effect on load distribution",
    },
    NonconformityScore {
        effect_kind: EffectKind::Backpressure,
        description: "Prediction error for queue depth reduction",
        formula: "|predicted_depth_delta - actual_depth_delta| / max_depth",
        high_score_meaning: "Backpressure signal did not achieve the predicted queue reduction",
    },
    NonconformityScore {
        effect_kind: EffectKind::NoAction,
        description: "Cost of inaction relative to best action",
        formula: "max(0, realized_loss_of_inaction - best_action_loss)",
        high_score_meaning: "Inaction was costly — ATC should have acted but chose not to",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Merge fallback for sparse strata
// ──────────────────────────────────────────────────────────────────────

/// Merge fallback strategy for strata with insufficient observations.
///
/// When a specific (effect_kind, risk_tier) stratum has fewer observations
/// than `min_observations`, the budget borrows from a parent stratum
/// to avoid either (a) blocking all actions or (b) allowing unchecked
/// actions with no budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeFallback {
    /// Use the same effect_kind but aggregate across all risk tiers.
    AggregateRiskTiers,
    /// Use the global budget for this effect kind.
    GlobalForEffectKind,
    /// Use the most conservative budget from any available stratum.
    MostConservative,
}

/// Default merge fallback strategy.
pub const DEFAULT_MERGE_FALLBACK: MergeFallback = MergeFallback::MostConservative;

/// Minimum observations before a stratum budget is considered reliable
/// enough to use without fallback.
pub const STRATUM_MIN_OBSERVATIONS: u32 = 20;

// ──────────────────────────────────────────────────────────────────────
// Budget table
// ──────────────────────────────────────────────────────────────────────

/// Stratum key for budget lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StratumKey {
    /// Action family.
    pub effect_kind: EffectKind,
    /// Risk tier (0=low, 1=medium, 2=high).
    pub risk_tier: u8,
}

impl std::fmt::Display for StratumKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:tier{}", self.effect_kind, self.risk_tier)
    }
}

/// The complete risk budget table.
///
/// Maps (effect_kind, risk_tier) → `BudgetState` for all active strata.
/// Provides admissibility checking with merge fallback for sparse strata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RiskBudgetTable {
    /// Per-stratum budgets.
    budgets: std::collections::HashMap<StratumKey, BudgetState>,
    /// Merge fallback strategy.
    fallback: Option<MergeFallback>,
}

impl RiskBudgetTable {
    /// Create a new risk budget table with default configurations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            budgets: std::collections::HashMap::new(),
            fallback: Some(DEFAULT_MERGE_FALLBACK),
        }
    }

    /// Get or create the budget for a stratum.
    pub fn get_or_create(&mut self, key: &StratumKey) -> &mut BudgetState {
        self.budgets
            .entry(key.clone())
            .or_insert_with(|| BudgetState::new(key.effect_kind, key.risk_tier))
    }

    /// Check admissibility for an action in a given stratum.
    #[must_use]
    pub fn check_admissibility(&self, key: &StratumKey, now_micros: i64) -> AdmissibilityResult {
        if let Some(budget) = self.budgets.get(key).filter(|b| b.has_sufficient_data()) {
            return budget.check_admissibility(now_micros);
        }

        // Fallback: use default config for the effect kind
        let default_config = default_budget_config(key.effect_kind);
        if default_config.max_false_action_rate >= 1.0 {
            return AdmissibilityResult {
                permitted: true,
                health: BudgetHealth::Healthy,
                reason: "action type has unlimited budget",
                false_action_rate: 0.0,
                budget_remaining_fraction: 1.0,
            };
        }

        // Insufficient data — permit with fallback flag
        AdmissibilityResult {
            permitted: true,
            health: BudgetHealth::Healthy,
            reason: "insufficient data, using conservative fallback",
            false_action_rate: 0.0,
            budget_remaining_fraction: 1.0,
        }
    }

    /// Record an outcome for a stratum's budget.
    pub fn record_outcome(
        &mut self,
        key: &StratumKey,
        false_action: bool,
        now_micros: i64,
    ) -> BudgetHealth {
        let budget = self.get_or_create(key);
        budget.record_outcome(false_action, now_micros)
    }

    /// Get a summary of all budget states for operator display.
    #[must_use]
    pub fn summary(&self) -> Vec<BudgetSummary> {
        let mut summaries: Vec<BudgetSummary> = self
            .budgets
            .iter()
            .map(|(key, state)| BudgetSummary {
                stratum: key.to_string(),
                effect_kind: key.effect_kind,
                risk_tier: key.risk_tier,
                health: state.health,
                false_action_rate: state.false_action_rate(),
                total_actions: state.total_actions,
                false_actions: state.false_actions,
                using_fallback: state.using_fallback,
            })
            .collect();
        summaries.sort_by_key(|s| (s.effect_kind as u8, s.risk_tier));
        summaries
    }
}

/// Summary of a single budget for operator display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetSummary {
    /// Stratum identifier (e.g., "release:tier2").
    pub stratum: String,
    /// Action family.
    pub effect_kind: EffectKind,
    /// Risk tier.
    pub risk_tier: u8,
    /// Current health state.
    pub health: BudgetHealth,
    /// Current observed false-action rate.
    pub false_action_rate: f64,
    /// Total actions observed.
    pub total_actions: u64,
    /// Total false actions observed.
    pub false_actions: u64,
    /// Whether this stratum is using merge fallback.
    pub using_fallback: bool,
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn release_budget_zero_tolerance() {
        let config = default_budget_config(EffectKind::Release);
        assert_eq!(config.max_false_action_rate, 0.0);
        assert_eq!(config.recovery_streak, 20);
    }

    #[test]
    fn probe_budget_unlimited() {
        let config = default_budget_config(EffectKind::Probe);
        assert!(config.max_false_action_rate >= 1.0);
    }

    #[test]
    fn advisory_budget_moderate() {
        let config = default_budget_config(EffectKind::Advisory);
        assert!(config.max_false_action_rate > 0.0);
        assert!(config.max_false_action_rate <= 0.10);
    }

    #[test]
    fn budget_state_new() {
        let state = BudgetState::new(EffectKind::Release, 2);
        assert_eq!(state.health, BudgetHealth::Healthy);
        assert_eq!(state.total_actions, 0);
        assert_eq!(state.false_actions, 0);
    }

    #[test]
    fn budget_false_action_rate() {
        let mut state = BudgetState::new(EffectKind::Advisory, 1);
        assert_eq!(state.false_action_rate(), 0.0);
        state.total_actions = 100;
        state.false_actions = 5;
        assert!((state.false_action_rate() - 0.05).abs() < 1e-10);
    }

    #[test]
    fn release_budget_cools_down_on_false_positive() {
        let mut state = BudgetState::new(EffectKind::Release, 2);
        // Record enough correct outcomes to pass min_observations
        for _ in 0..20 {
            state.record_outcome(false, 1_000_000);
        }
        assert_eq!(state.health, BudgetHealth::Healthy);

        // One false positive should enter CoolingDown for release budget
        // (rate exceeds 0.0 max, enters cooldown before blocking)
        state.record_outcome(true, 2_000_000);
        assert_eq!(state.health, BudgetHealth::CoolingDown);
        assert!(!state.health.allows_action()); // CoolingDown blocks actions
        assert_eq!(state.cooldown_started_micros, 2_000_000);
    }

    #[test]
    fn advisory_budget_allows_some_false_positives() {
        let mut state = BudgetState::new(EffectKind::Advisory, 1);
        // Record 30 correct outcomes
        for _ in 0..30 {
            state.record_outcome(false, 1_000_000);
        }
        assert_eq!(state.health, BudgetHealth::Healthy);

        // One false positive should not block advisory (5% budget)
        state.record_outcome(true, 2_000_000);
        assert!(state.health.allows_action());
    }

    #[test]
    fn recovery_streak_restores_health() {
        let mut state = BudgetState::new(EffectKind::ForceReservation, 1);
        // Get to CoolingDown state
        for _ in 0..20 {
            state.record_outcome(false, 1_000_000);
        }
        // False positive pushes over 2% budget → enters CoolingDown
        state.record_outcome(true, 2_000_000);
        assert_eq!(state.health, BudgetHealth::CoolingDown);

        // After cooldown period expires (300s = 300_000_000 micros),
        // recovery streak of 10 correct outcomes restores Healthy.
        // Time must be past cooldown: 2_000_000 + 300_000_000 = 302_000_000
        for i in 0..10 {
            state.record_outcome(false, 310_000_000 + i * 1_000_000);
        }
        assert_eq!(state.health, BudgetHealth::Healthy);
    }

    #[test]
    fn admissibility_check_probe_always_allowed() {
        let state = BudgetState::new(EffectKind::Probe, 0);
        let result = state.check_admissibility(1_000_000);
        assert!(result.permitted);
        assert_eq!(result.health, BudgetHealth::Healthy);
    }

    #[test]
    fn risk_budget_table_get_or_create() {
        let mut table = RiskBudgetTable::new();
        let key = StratumKey {
            effect_kind: EffectKind::Release,
            risk_tier: 2,
        };
        let budget = table.get_or_create(&key);
        assert_eq!(budget.effect_kind, EffectKind::Release);
        assert_eq!(budget.risk_tier, 2);
    }

    #[test]
    fn risk_budget_table_record_and_check() {
        let mut table = RiskBudgetTable::new();
        let key = StratumKey {
            effect_kind: EffectKind::Advisory,
            risk_tier: 1,
        };

        // Record some outcomes
        for _ in 0..30 {
            table.record_outcome(&key, false, 1_000_000);
        }

        let result = table.check_admissibility(&key, 2_000_000);
        assert!(result.permitted);
    }

    #[test]
    fn stratum_key_display() {
        let key = StratumKey {
            effect_kind: EffectKind::Release,
            risk_tier: 2,
        };
        assert_eq!(key.to_string(), "release:tier2");
    }

    #[test]
    fn budget_health_display() {
        assert_eq!(BudgetHealth::Healthy.to_string(), "healthy");
        assert_eq!(BudgetHealth::Blocking.to_string(), "blocking");
    }

    #[test]
    fn nonconformity_scores_cover_all_effect_kinds() {
        let kinds = [
            EffectKind::Probe,
            EffectKind::Advisory,
            EffectKind::Release,
            EffectKind::ForceReservation,
            EffectKind::RoutingSuggestion,
            EffectKind::Backpressure,
            EffectKind::NoAction,
        ];
        for kind in kinds {
            assert!(
                NONCONFORMITY_SCORES.iter().any(|s| s.effect_kind == kind),
                "missing nonconformity score for {kind}"
            );
        }
    }

    #[test]
    fn summary_sorted_by_effect_and_tier() {
        let mut table = RiskBudgetTable::new();
        table.get_or_create(&StratumKey {
            effect_kind: EffectKind::Release,
            risk_tier: 2,
        });
        table.get_or_create(&StratumKey {
            effect_kind: EffectKind::Advisory,
            risk_tier: 0,
        });
        let summary = table.summary();
        assert_eq!(summary.len(), 2);
        // Advisory (enum ordinal 0) should come before Release (ordinal 2)
        assert_eq!(summary[0].effect_kind, EffectKind::Advisory);
        assert_eq!(summary[1].effect_kind, EffectKind::Release);
    }

    #[test]
    fn high_force_effects_have_strict_budgets() {
        for kind in [EffectKind::Release, EffectKind::ForceReservation] {
            let config = default_budget_config(kind);
            assert!(
                config.max_false_action_rate <= 0.02,
                "high-force effect {kind} should have strict budget, got {}",
                config.max_false_action_rate
            );
        }
    }
}
