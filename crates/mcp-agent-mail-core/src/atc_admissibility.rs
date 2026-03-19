#![allow(
    clippy::cast_precision_loss,
    clippy::struct_excessive_bools,
    clippy::doc_markdown
)]
//! Admissibility gates, exploration budget, and automatic rollback
//! (br-0qt6e.3.7).
//!
//! Defines the safety envelope for ATC adaptive actions: when ATC may
//! act, when it may explore, when evidence is too suspicious to trust,
//! and when it must roll back to the incumbent policy.
//!
//! # Action Tiers
//!
//! | Tier     | Actions                          | Exploration | Budget        |
//! |----------|----------------------------------|-------------|---------------|
//! | Low-risk | Advisory, RoutingSuggestion      | Allowed     | 10% of ticks  |
//! | Medium   | Probe, Backpressure              | Conditional | 5% of ticks   |
//! | High-risk| Release, ForceReservation        | Never       | 0 (incumbent) |
//!
//! # Admissibility Gate
//!
//! Before any adapted action is executed, it must pass ALL of:
//! 1. **Calibration healthy** — e-process not in alert, safe mode not active
//! 2. **Conformal budget available** — per-stratum false-action budget not exhausted
//! 3. **Regime stable** — not transitioning or cooling
//! 4. **Evidence quality sufficient** — no quarantined evidence dominating
//! 5. **Exploration budget available** — for non-incumbent actions only
//!
//! # Automatic Rollback
//!
//! Rollback to incumbent policy is triggered by ANY of:
//! - Cumulative regret exceeds 2× baseline over 100-decision window
//! - Safe mode entry (calibration failure)
//! - Regime change detection (CUSUM)
//! - Evidence contamination exceeds 30% of recent observations
//! - Operator explicit override

use serde::{Deserialize, Serialize};

use crate::experience::EffectKind;

/// Action risk tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionTier {
    /// Advisory, RoutingSuggestion — informational, easily reversible.
    LowRisk,
    /// Probe, Backpressure — some cost but recoverable.
    MediumRisk,
    /// Release, ForceReservation — destructive, hard to reverse.
    HighRisk,
}

impl ActionTier {
    /// Classify an effect kind into a risk tier.
    #[must_use]
    pub const fn from_effect_kind(kind: EffectKind) -> Self {
        match kind {
            EffectKind::Advisory | EffectKind::RoutingSuggestion | EffectKind::NoAction => {
                Self::LowRisk
            }
            EffectKind::Probe | EffectKind::Backpressure => Self::MediumRisk,
            EffectKind::Release | EffectKind::ForceReservation => Self::HighRisk,
        }
    }

    /// Whether exploration is allowed for this tier.
    /// LowRisk always allows, MediumRisk allows conditionally (smaller budget),
    /// HighRisk never allows.
    #[must_use]
    pub const fn allows_exploration(self) -> bool {
        matches!(self, Self::LowRisk | Self::MediumRisk)
    }

    /// Maximum exploration budget fraction for this tier.
    #[must_use]
    pub const fn exploration_budget_fraction(self) -> f64 {
        match self {
            Self::LowRisk => 0.10,   // 10% of ticks
            Self::MediumRisk => 0.05, // 5% conditionally
            Self::HighRisk => 0.0,    // never explore
        }
    }
}

impl std::fmt::Display for ActionTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LowRisk => write!(f, "low_risk"),
            Self::MediumRisk => write!(f, "medium_risk"),
            Self::HighRisk => write!(f, "high_risk"),
        }
    }
}

/// Result of checking the admissibility gate.
#[derive(Debug, Clone, Serialize)]
pub struct AdmissibilityResult {
    /// Whether the action is admitted.
    pub admitted: bool,
    /// Individual gate results.
    pub gates: Vec<GateResult>,
    /// Overall reason for admission/denial (human-readable).
    pub reason: String,
    /// Machine-readable denial code (None if admitted).
    pub denial_code: Option<DenialCode>,
}

/// Result of a single admissibility gate check.
#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    /// Name of the gate.
    pub gate: &'static str,
    /// Whether this gate passed.
    pub passed: bool,
    /// Reason for pass/fail.
    pub detail: String,
}

/// Machine-readable denial codes for blocked actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialCode {
    /// Calibration is unhealthy (e-process alert or safe mode).
    CalibrationUnhealthy,
    /// Per-stratum risk budget exhausted.
    RiskBudgetExhausted,
    /// Regime is transitioning or cooling.
    RegimeUnstable,
    /// Evidence quality too low (contamination).
    EvidenceQualityLow,
    /// Exploration budget exhausted for this tick.
    ExplorationBudgetExhausted,
    /// High-risk action during exploration.
    HighRiskExploration,
    /// Rollback triggered — incumbent policy enforced.
    RollbackActive,
}

impl std::fmt::Display for DenialCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CalibrationUnhealthy => write!(f, "calibration_unhealthy"),
            Self::RiskBudgetExhausted => write!(f, "risk_budget_exhausted"),
            Self::RegimeUnstable => write!(f, "regime_unstable"),
            Self::EvidenceQualityLow => write!(f, "evidence_quality_low"),
            Self::ExplorationBudgetExhausted => write!(f, "exploration_budget_exhausted"),
            Self::HighRiskExploration => write!(f, "high_risk_exploration"),
            Self::RollbackActive => write!(f, "rollback_active"),
        }
    }
}

/// Inputs for the admissibility gate evaluation.
#[derive(Debug, Clone)]
pub struct AdmissibilityContext {
    /// Effect kind being evaluated.
    pub effect_kind: EffectKind,
    /// Whether this is an incumbent or candidate policy action.
    pub is_incumbent_policy: bool,
    /// Calibration state: is the e-process healthy?
    pub calibration_healthy: bool,
    /// Safe mode active?
    pub safe_mode_active: bool,
    /// Is the risk budget for this stratum available?
    pub risk_budget_available: bool,
    /// Is the regime stable?
    pub regime_stable: bool,
    /// Fraction of recent evidence that is contaminated/suspect.
    pub contamination_fraction: f64,
    /// How much exploration budget has been used this tick (fraction).
    pub exploration_used_fraction: f64,
    /// Is a rollback currently active?
    pub rollback_active: bool,
}

/// Evaluate the admissibility gate for a proposed action.
///
/// Returns whether the action is admitted and detailed gate results.
#[must_use]
pub fn evaluate_admissibility(ctx: &AdmissibilityContext) -> AdmissibilityResult {
    let tier = ActionTier::from_effect_kind(ctx.effect_kind);
    let mut gates = Vec::new();

    // Gate 1: Rollback check (trumps everything).
    let rollback_ok = !ctx.rollback_active || ctx.is_incumbent_policy;
    gates.push(GateResult {
        gate: "rollback",
        passed: rollback_ok,
        detail: if rollback_ok {
            "no active rollback or action is incumbent".to_string()
        } else {
            "rollback active — only incumbent policy allowed".to_string()
        },
    });
    if !rollback_ok {
        return AdmissibilityResult {
            admitted: false,
            gates,
            reason: "rollback active — only incumbent policy actions are allowed".to_string(),
            denial_code: Some(DenialCode::RollbackActive),
        };
    }

    // Gate 2: Calibration.
    let cal_ok = ctx.calibration_healthy && !ctx.safe_mode_active;
    gates.push(GateResult {
        gate: "calibration",
        passed: cal_ok,
        detail: if cal_ok {
            "calibration healthy, safe mode inactive".to_string()
        } else {
            format!(
                "calibration_healthy={}, safe_mode={}",
                ctx.calibration_healthy, ctx.safe_mode_active
            )
        },
    });

    // Gate 3: Risk budget.
    gates.push(GateResult {
        gate: "risk_budget",
        passed: ctx.risk_budget_available,
        detail: if ctx.risk_budget_available {
            "budget available for this stratum".to_string()
        } else {
            "risk budget exhausted for this stratum".to_string()
        },
    });

    // Gate 4: Regime stability.
    gates.push(GateResult {
        gate: "regime",
        passed: ctx.regime_stable,
        detail: if ctx.regime_stable {
            "regime stable".to_string()
        } else {
            "regime transitioning or cooling".to_string()
        },
    });

    // Gate 5: Evidence quality.
    let evidence_ok = ctx.contamination_fraction < 0.30;
    gates.push(GateResult {
        gate: "evidence_quality",
        passed: evidence_ok,
        detail: format!(
            "contamination {:.0}% (threshold 30%)",
            ctx.contamination_fraction * 100.0
        ),
    });

    // Gate 6: Exploration budget (only for non-incumbent actions).
    let exploration_ok = if ctx.is_incumbent_policy {
        true // incumbent actions always allowed
    } else {
        let max_budget = tier.exploration_budget_fraction();
        if max_budget <= 0.0 {
            false // high-risk actions cannot explore
        } else {
            ctx.exploration_used_fraction < max_budget
        }
    };
    gates.push(GateResult {
        gate: "exploration_budget",
        passed: exploration_ok,
        detail: if ctx.is_incumbent_policy {
            "incumbent policy — no exploration budget needed".to_string()
        } else if !tier.allows_exploration() && tier == ActionTier::HighRisk {
            format!("{tier} actions never explore")
        } else {
            format!(
                "exploration {:.0}% used (max {:.0}%)",
                ctx.exploration_used_fraction * 100.0,
                tier.exploration_budget_fraction() * 100.0
            )
        },
    });

    // Check all gates. Find first failure by index to avoid borrow conflict.
    let failure_idx = gates.iter().position(|g| !g.passed);

    if let Some(idx) = failure_idx {
        let gate_name = gates[idx].gate;
        let detail = gates[idx].detail.clone();
        let denial_code = match gate_name {
            "calibration" => DenialCode::CalibrationUnhealthy,
            "risk_budget" => DenialCode::RiskBudgetExhausted,
            "regime" => DenialCode::RegimeUnstable,
            "evidence_quality" => DenialCode::EvidenceQualityLow,
            "exploration_budget" => {
                if tier == ActionTier::HighRisk {
                    DenialCode::HighRiskExploration
                } else {
                    DenialCode::ExplorationBudgetExhausted
                }
            }
            _ => DenialCode::CalibrationUnhealthy,
        };

        AdmissibilityResult {
            admitted: false,
            gates,
            reason: format!("denied by {gate_name} gate: {detail}"),
            denial_code: Some(denial_code),
        }
    } else {
        AdmissibilityResult {
            admitted: true,
            gates,
            reason: format!("admitted ({tier}, all gates passed)"),
            denial_code: None,
        }
    }
}

/// Rollback trigger conditions.
///
/// ANY of these triggers causes automatic rollback to the incumbent policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackTriggers {
    /// Regret exceeded 2× baseline over window.
    pub regret_exceeded: bool,
    /// Safe mode was entered.
    pub safe_mode_entered: bool,
    /// Regime change detected.
    pub regime_change_detected: bool,
    /// Evidence contamination exceeds threshold.
    pub contamination_exceeded: bool,
    /// Operator explicit override.
    pub operator_override: bool,
}

impl RollbackTriggers {
    /// Whether any trigger is active.
    #[must_use]
    pub const fn any_triggered(&self) -> bool {
        self.regret_exceeded
            || self.safe_mode_entered
            || self.regime_change_detected
            || self.contamination_exceeded
            || self.operator_override
    }

    /// Get the primary trigger reason.
    #[must_use]
    pub const fn primary_reason(&self) -> Option<&'static str> {
        if self.safe_mode_entered {
            Some("safe mode entered")
        } else if self.regret_exceeded {
            Some("cumulative regret exceeded 2x baseline")
        } else if self.regime_change_detected {
            Some("regime change detected")
        } else if self.contamination_exceeded {
            Some("evidence contamination >30%")
        } else if self.operator_override {
            Some("operator override")
        } else {
            None
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_context() -> AdmissibilityContext {
        AdmissibilityContext {
            effect_kind: EffectKind::Advisory,
            is_incumbent_policy: true,
            calibration_healthy: true,
            safe_mode_active: false,
            risk_budget_available: true,
            regime_stable: true,
            contamination_fraction: 0.0,
            exploration_used_fraction: 0.0,
            rollback_active: false,
        }
    }

    #[test]
    fn clean_context_is_admitted() {
        let result = evaluate_admissibility(&clean_context());
        assert!(result.admitted);
        assert!(result.denial_code.is_none());
    }

    #[test]
    fn rollback_blocks_non_incumbent() {
        let ctx = AdmissibilityContext {
            is_incumbent_policy: false,
            rollback_active: true,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(!result.admitted);
        assert_eq!(result.denial_code, Some(DenialCode::RollbackActive));
    }

    #[test]
    fn rollback_allows_incumbent() {
        let ctx = AdmissibilityContext {
            is_incumbent_policy: true,
            rollback_active: true,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(result.admitted);
    }

    #[test]
    fn safe_mode_blocks_action() {
        let ctx = AdmissibilityContext {
            safe_mode_active: true,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(!result.admitted);
        assert_eq!(result.denial_code, Some(DenialCode::CalibrationUnhealthy));
    }

    #[test]
    fn risk_budget_exhausted_blocks() {
        let ctx = AdmissibilityContext {
            risk_budget_available: false,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(!result.admitted);
        assert_eq!(result.denial_code, Some(DenialCode::RiskBudgetExhausted));
    }

    #[test]
    fn regime_unstable_blocks() {
        let ctx = AdmissibilityContext {
            regime_stable: false,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(!result.admitted);
        assert_eq!(result.denial_code, Some(DenialCode::RegimeUnstable));
    }

    #[test]
    fn high_contamination_blocks() {
        let ctx = AdmissibilityContext {
            contamination_fraction: 0.35,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(!result.admitted);
        assert_eq!(result.denial_code, Some(DenialCode::EvidenceQualityLow));
    }

    #[test]
    fn high_risk_exploration_blocked() {
        let ctx = AdmissibilityContext {
            effect_kind: EffectKind::Release,
            is_incumbent_policy: false,
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(!result.admitted);
        assert_eq!(result.denial_code, Some(DenialCode::HighRiskExploration));
    }

    #[test]
    fn low_risk_exploration_allowed_within_budget() {
        let ctx = AdmissibilityContext {
            effect_kind: EffectKind::Advisory,
            is_incumbent_policy: false,
            exploration_used_fraction: 0.05, // within 10% budget
            ..clean_context()
        };
        let result = evaluate_admissibility(&ctx);
        assert!(result.admitted);
    }

    #[test]
    fn action_tier_classification() {
        assert_eq!(
            ActionTier::from_effect_kind(EffectKind::Advisory),
            ActionTier::LowRisk
        );
        assert_eq!(
            ActionTier::from_effect_kind(EffectKind::Probe),
            ActionTier::MediumRisk
        );
        assert_eq!(
            ActionTier::from_effect_kind(EffectKind::Release),
            ActionTier::HighRisk
        );
    }

    #[test]
    fn rollback_triggers() {
        let triggers = RollbackTriggers {
            regret_exceeded: false,
            safe_mode_entered: true,
            regime_change_detected: false,
            contamination_exceeded: false,
            operator_override: false,
        };
        assert!(triggers.any_triggered());
        assert_eq!(triggers.primary_reason(), Some("safe mode entered"));
    }

    #[test]
    fn no_triggers_means_no_rollback() {
        let triggers = RollbackTriggers {
            regret_exceeded: false,
            safe_mode_entered: false,
            regime_change_detected: false,
            contamination_exceeded: false,
            operator_override: false,
        };
        assert!(!triggers.any_triggered());
        assert!(triggers.primary_reason().is_none());
    }

    #[test]
    fn gate_results_are_detailed() {
        let result = evaluate_admissibility(&clean_context());
        assert!(result.gates.len() >= 5);
        for gate in &result.gates {
            assert!(gate.passed);
            assert!(!gate.detail.is_empty());
        }
    }
}
