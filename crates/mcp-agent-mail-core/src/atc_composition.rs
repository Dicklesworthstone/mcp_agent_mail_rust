#![allow(
    clippy::cast_precision_loss,
    clippy::struct_excessive_bools,
    clippy::doc_markdown
)]
//! Controller composition rules, timescale separation, and anti-oscillation
//! design (br-0qt6e.3.8).
//!
//! The ATC learning stack combines multiple adaptive controllers:
//!
//! | Controller              | Timescale | Authority                          |
//! |-------------------------|-----------|------------------------------------|
//! | CalibrationGuard        | Fast      | Safe-mode entry/exit               |
//! | ConformalRiskBudget     | Fast      | Per-stratum false-action gating    |
//! | EProcessMonitor         | Fast      | Sustained miscalibration alarm     |
//! | CusumDetector           | Fast      | Regime shift detection             |
//! | AdmissibilityGates      | Fast      | Per-action go/no-go filtering      |
//! | EffectSemantics         | Fast      | Cooldown/escalation per action     |
//! | FairnessGuards          | Medium    | Per-target burden concentration    |
//! | SlowController (PI)     | Medium    | Probe budget fraction adjustment   |
//! | RegretTracker           | Medium    | Regret-bounded policy evaluation   |
//! | ShrinkageEstimator      | Medium    | Sparse-stratum estimate smoothing  |
//! | AdaptationEngine        | Slow      | Policy promotion/rollback          |
//! | RegimeManager           | Slow      | Regime phase transitions           |
//! | PolicyCertificates      | Slow      | Doubly-robust promotion evidence   |
//! | VoIControl              | Slow      | Experiment budget allocation       |
//!
//! Without an explicit composition contract these controllers can interfere
//! or oscillate.  This module defines:
//!
//! 1. **Update order** — the sequence in which controllers run within a tick.
//! 2. **Authority boundaries** — which controller owns which decisions, and
//!    which controller may veto which others.
//! 3. **Timescale separation** — fast guards, medium-horizon policy, slow
//!    regime changes, and the minimum intervals between each.
//! 4. **Anti-oscillation hysteresis** — deadbands and hold-off rules that
//!    prevent rapid flapping when multiple controllers disagree.
//! 5. **Compatibility matrix** — a structured checklist that future
//!    contributors must evaluate before adding a new controller.
//!
//! # Design Principles
//!
//! - **Fast controllers veto; slow controllers steer.**  A fast guard can
//!   suppress an action in a single tick but cannot promote a policy change.
//!   A slow controller can change the policy but cannot override a fast
//!   guard's veto.
//! - **Monotone veto stacking.**  If any upstream controller vetoes an
//!   action, all downstream controllers treat the action as suppressed.
//!   There is no "override" path that bypasses an earlier veto (except
//!   explicit operator override).
//! - **Timescale dominance.**  A faster controller's state must have
//!   settled before a slower controller reads it.  This is enforced by
//!   the update order: fast controllers run first, their outputs become
//!   inputs to medium controllers, which in turn feed slow controllers.
//! - **Fairness is a first-class veto.**  Fairness guards sit in the
//!   medium tier and can veto actions that fast guards approved.  This
//!   prevents the system from concentrating harm on a single agent just
//!   because the per-action risk budget has capacity.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Controller identity and timescale classification
// ---------------------------------------------------------------------------

/// Every controller in the ATC stack has a unique identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerId {
    /// E-process martingale monitor (sustained miscalibration alarm).
    EProcess,
    /// CUSUM change-point detector (regime shift detection).
    Cusum,
    /// Calibration guard (safe-mode entry/exit based on e-process + CUSUM).
    CalibrationGuard,
    /// Conformal risk budget (per-stratum false-action rate gating).
    ConformalRiskBudget,
    /// Admissibility gates (per-action go/no-go filter combining signals).
    AdmissibilityGates,
    /// Effect semantics (cooldown enforcement, escalation ladder).
    EffectSemantics,
    /// Fairness and anti-starvation guards (per-target burden budgets).
    FairnessGuards,
    /// PI slow controller (probe budget fraction adjustment).
    SlowController,
    /// Regret tracker (rolling regret-bounded policy evaluation).
    RegretTracker,
    /// Empirical-Bayes shrinkage (sparse-stratum estimate smoothing).
    ShrinkageEstimator,
    /// Adaptation engine (policy promotion/rollback via shadow evaluation).
    AdaptationEngine,
    /// Regime manager (phase transitions and cool-down timers).
    RegimeManager,
    /// Policy certificates (doubly-robust promotion evidence).
    PolicyCertificates,
    /// Value-of-information control (experiment budget allocation).
    VoIControl,
}

impl ControllerId {
    /// Return the timescale tier for this controller.
    #[must_use]
    pub const fn timescale(self) -> Timescale {
        match self {
            Self::EProcess
            | Self::Cusum
            | Self::CalibrationGuard
            | Self::ConformalRiskBudget
            | Self::AdmissibilityGates
            | Self::EffectSemantics => Timescale::Fast,

            Self::FairnessGuards
            | Self::SlowController
            | Self::RegretTracker
            | Self::ShrinkageEstimator => Timescale::Medium,

            Self::AdaptationEngine
            | Self::RegimeManager
            | Self::PolicyCertificates
            | Self::VoIControl => Timescale::Slow,
        }
    }

    /// Canonical evaluation order within a single tick.
    ///
    /// Lower values run first.  The ordering guarantees that faster
    /// controllers produce their outputs before slower controllers read
    /// them, and that veto sources run before the actions they can veto.
    #[must_use]
    pub const fn eval_order(self) -> u8 {
        match self {
            // --- Fast tier (0..49) ---
            Self::EProcess => 0,
            Self::Cusum => 1,
            Self::CalibrationGuard => 2,
            Self::ConformalRiskBudget => 10,
            Self::EffectSemantics => 11,
            Self::AdmissibilityGates => 20, // reads from all above

            // --- Medium tier (50..99) ---
            Self::FairnessGuards => 50,
            Self::RegretTracker => 60,
            Self::ShrinkageEstimator => 61,
            Self::SlowController => 70,

            // --- Slow tier (100..149) ---
            Self::RegimeManager => 100,
            Self::AdaptationEngine => 110,
            Self::PolicyCertificates => 120,
            Self::VoIControl => 130,
        }
    }

    /// All controller IDs in canonical evaluation order.
    pub const ALL: [Self; 14] = [
        Self::EProcess,
        Self::Cusum,
        Self::CalibrationGuard,
        Self::ConformalRiskBudget,
        Self::EffectSemantics,
        Self::AdmissibilityGates,
        Self::FairnessGuards,
        Self::RegretTracker,
        Self::ShrinkageEstimator,
        Self::SlowController,
        Self::RegimeManager,
        Self::AdaptationEngine,
        Self::PolicyCertificates,
        Self::VoIControl,
    ];

    /// Human-readable short name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EProcess => "e_process",
            Self::Cusum => "cusum",
            Self::CalibrationGuard => "calibration_guard",
            Self::ConformalRiskBudget => "conformal_risk_budget",
            Self::AdmissibilityGates => "admissibility_gates",
            Self::EffectSemantics => "effect_semantics",
            Self::FairnessGuards => "fairness_guards",
            Self::SlowController => "slow_controller",
            Self::RegretTracker => "regret_tracker",
            Self::ShrinkageEstimator => "shrinkage_estimator",
            Self::AdaptationEngine => "adaptation_engine",
            Self::RegimeManager => "regime_manager",
            Self::PolicyCertificates => "policy_certificates",
            Self::VoIControl => "voi_control",
        }
    }
}

/// Timescale tier.  Fast controllers run every tick; medium controllers
/// run every N ticks; slow controllers run on coarser windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Timescale {
    /// Every tick (sub-millisecond decisions).
    Fast,
    /// Every 10-50 ticks (second-scale adjustments).
    Medium,
    /// Every 100+ ticks or on explicit triggers (minute-scale policy changes).
    Slow,
}

impl Timescale {
    /// Suggested minimum ticks between updates for this timescale.
    ///
    /// Controllers SHOULD NOT run their adaptive logic more often than
    /// this.  Fast controllers run every tick.  Medium controllers
    /// accumulate evidence over a window.  Slow controllers wait for
    /// statistical significance.
    #[must_use]
    pub const fn min_update_interval_ticks(self) -> u64 {
        match self {
            Self::Fast => 1,
            Self::Medium => 10,
            Self::Slow => 100,
        }
    }

    /// Suggested minimum wall-clock microseconds between state changes.
    ///
    /// This is the *hysteresis hold-off*: even if new evidence arrives,
    /// a controller should not reverse its previous state change until
    /// at least this many microseconds have elapsed.
    #[must_use]
    pub const fn hysteresis_hold_off_micros(self) -> i64 {
        match self {
            Self::Fast => 500_000,     // 0.5 seconds
            Self::Medium => 5_000_000, // 5 seconds
            Self::Slow => 30_000_000,  // 30 seconds
        }
    }
}

// ---------------------------------------------------------------------------
// Authority boundaries
// ---------------------------------------------------------------------------

/// Describes what a controller is authoritative over.
#[derive(Debug, Clone)]
pub struct AuthorityBoundary {
    /// Which controller this describes.
    pub controller: ControllerId,
    /// What this controller is allowed to decide.
    pub owns: &'static [&'static str],
    /// What this controller can veto (actions proposed by others).
    pub can_veto: &'static [&'static str],
    /// What this controller MUST NOT do (hard boundary).
    pub must_not: &'static [&'static str],
    /// Other controllers whose output this controller reads.
    pub reads_from: &'static [ControllerId],
}

/// Static authority boundary table.
///
/// This is the canonical reference for "who owns what" and "who can
/// veto what".  The `run_tick` method in `AtcEngine` must respect
/// these boundaries.
pub const AUTHORITY_BOUNDARIES: [AuthorityBoundary; 14] = [
    AuthorityBoundary {
        controller: ControllerId::EProcess,
        owns: &["miscalibration_alarm"],
        can_veto: &[],
        must_not: &["modify_policy", "modify_loss_matrix", "suppress_actions"],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::Cusum,
        owns: &["regime_shift_detection", "degradation_alarm"],
        can_veto: &[],
        must_not: &["modify_policy", "suppress_actions"],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::CalibrationGuard,
        owns: &["safe_mode_entry", "safe_mode_exit"],
        can_veto: &[
            "release_reservations",
            "force_reservation",
            "policy_promotion",
        ],
        must_not: &["modify_loss_matrix", "modify_probe_budget"],
        reads_from: &[ControllerId::EProcess, ControllerId::Cusum],
    },
    AuthorityBoundary {
        controller: ControllerId::ConformalRiskBudget,
        owns: &["false_action_rate_gating"],
        can_veto: &["release_reservations", "force_reservation", "probe_agent"],
        must_not: &["modify_policy", "modify_loss_matrix"],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::EffectSemantics,
        owns: &["cooldown_enforcement", "escalation_ladder"],
        can_veto: &["send_advisory", "probe_agent"],
        must_not: &["modify_policy", "release_reservations"],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::AdmissibilityGates,
        owns: &["action_admissibility_verdict"],
        can_veto: &[
            "release_reservations",
            "force_reservation",
            "probe_agent",
            "send_advisory",
            "policy_promotion",
        ],
        must_not: &["modify_policy", "modify_loss_matrix"],
        reads_from: &[
            ControllerId::CalibrationGuard,
            ControllerId::ConformalRiskBudget,
            ControllerId::EffectSemantics,
        ],
    },
    AuthorityBoundary {
        controller: ControllerId::FairnessGuards,
        owns: &[
            "burden_concentration_gating",
            "anti_starvation",
            "fairness_budget_accounting",
        ],
        can_veto: &[
            "release_reservations",
            "force_reservation",
            "probe_agent",
            "send_advisory",
        ],
        must_not: &["modify_policy", "modify_loss_matrix", "modify_probe_budget"],
        reads_from: &[ControllerId::AdmissibilityGates],
    },
    AuthorityBoundary {
        controller: ControllerId::RegretTracker,
        owns: &["regret_accumulation", "regret_alarm"],
        can_veto: &[],
        must_not: &["modify_policy", "suppress_actions"],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::ShrinkageEstimator,
        owns: &["stratum_estimate_smoothing"],
        can_veto: &[],
        must_not: &["modify_policy", "suppress_actions"],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::SlowController,
        owns: &["probe_budget_fraction", "probe_limit"],
        can_veto: &[],
        must_not: &[
            "modify_policy",
            "modify_loss_matrix",
            "release_reservations",
        ],
        reads_from: &[],
    },
    AuthorityBoundary {
        controller: ControllerId::RegimeManager,
        owns: &["regime_phase_transition", "regime_cooldown"],
        can_veto: &["policy_promotion"],
        must_not: &["suppress_actions", "modify_loss_matrix"],
        reads_from: &[ControllerId::Cusum],
    },
    AuthorityBoundary {
        controller: ControllerId::AdaptationEngine,
        owns: &["policy_selection", "shadow_evaluation", "policy_rollback"],
        can_veto: &[],
        must_not: &["bypass_calibration_guard", "bypass_fairness_guards"],
        reads_from: &[
            ControllerId::RegretTracker,
            ControllerId::CalibrationGuard,
            ControllerId::RegimeManager,
        ],
    },
    AuthorityBoundary {
        controller: ControllerId::PolicyCertificates,
        owns: &["promotion_evidence_evaluation"],
        can_veto: &["policy_promotion"],
        must_not: &["suppress_actions", "modify_loss_matrix"],
        reads_from: &[
            ControllerId::AdaptationEngine,
            ControllerId::RegimeManager,
            ControllerId::FairnessGuards,
        ],
    },
    AuthorityBoundary {
        controller: ControllerId::VoIControl,
        owns: &[
            "experiment_budget_allocation",
            "identifiability_debt_tracking",
        ],
        can_veto: &[],
        must_not: &["modify_policy", "release_reservations", "force_reservation"],
        reads_from: &[
            ControllerId::AdmissibilityGates,
            ControllerId::FairnessGuards,
            ControllerId::RegimeManager,
        ],
    },
];

/// Look up the authority boundary for a controller.
#[must_use]
pub fn authority_for(id: ControllerId) -> &'static AuthorityBoundary {
    &AUTHORITY_BOUNDARIES[ControllerId::ALL
        .iter()
        .position(|c| *c == id)
        .expect("ControllerId not in ALL")]
}

// ---------------------------------------------------------------------------
// Veto chain
// ---------------------------------------------------------------------------

/// A veto issued by a controller to suppress a proposed action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerVeto {
    /// Who issued the veto.
    pub source: ControllerId,
    /// Which action was vetoed.
    pub action: String,
    /// Human-readable reason.
    pub reason: String,
    /// Timestamp of the veto (microseconds since epoch).
    pub timestamp_micros: i64,
}

/// The result of running the veto chain for a proposed action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VetoChainResult {
    /// The proposed action string.
    pub action: String,
    /// Whether the action is permitted (no vetoes).
    pub permitted: bool,
    /// All vetoes collected (empty if permitted).
    pub vetoes: Vec<ControllerVeto>,
}

impl VetoChainResult {
    /// True if any controller vetoed this action.
    #[must_use]
    pub const fn is_vetoed(&self) -> bool {
        !self.permitted
    }

    /// The first veto reason, if any.
    #[must_use]
    pub fn first_veto_reason(&self) -> Option<&str> {
        self.vetoes.first().map(|v| v.reason.as_str())
    }
}

/// Evaluate the veto chain for a proposed action.
///
/// Walks the authority table in evaluation order.  Any controller whose
/// `can_veto` list includes the proposed action name gets to decide.
/// The `check_veto` callback is called for each such controller; it
/// should return `Some(reason)` to veto or `None` to allow.
///
/// Veto evaluation stops at the first veto in strict mode (the default
/// in production), or collects all vetoes in diagnostic mode.
pub fn evaluate_veto_chain(
    action: &str,
    timestamp_micros: i64,
    check_veto: &dyn Fn(ControllerId) -> Option<String>,
) -> VetoChainResult {
    let mut vetoes = Vec::new();
    for boundary in &AUTHORITY_BOUNDARIES {
        if boundary.can_veto.contains(&action)
            && let Some(reason) = check_veto(boundary.controller)
        {
            vetoes.push(ControllerVeto {
                source: boundary.controller,
                action: action.to_string(),
                reason,
                timestamp_micros,
            });
        }
    }
    VetoChainResult {
        action: action.to_string(),
        permitted: vetoes.is_empty(),
        vetoes,
    }
}

// ---------------------------------------------------------------------------
// Anti-oscillation hysteresis
// ---------------------------------------------------------------------------

/// Hysteresis state for a single controller output.
///
/// Prevents rapid flapping by requiring a state change to survive a
/// hold-off period before it becomes effective.  If the controller
/// reverses its decision within the hold-off window, the change is
/// suppressed and the previous state is retained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HysteresisGuard {
    /// The controller this guard protects.
    pub controller: ControllerId,
    /// The current committed state label (e.g. "nominal", "safe_mode").
    pub committed_state: String,
    /// A pending state change, if any, and when it was proposed.
    pending: Option<PendingStateChange>,
    /// Minimum microseconds between state transitions.
    hold_off_micros: i64,
    /// Number of consecutive ticks the pending state has been stable.
    pending_stable_ticks: u64,
    /// Minimum consecutive stable ticks before committing.
    min_stable_ticks: u64,
    /// Count of suppressed oscillations (for diagnostics).
    suppressed_count: u64,
    /// Count of committed transitions (for diagnostics).
    committed_count: u64,
    /// Timestamp of the last committed transition.
    last_transition_micros: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingStateChange {
    proposed_state: String,
    proposed_at_micros: i64,
}

/// Outcome of proposing a state change to a hysteresis guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HysteresisOutcome {
    /// No change needed; the proposed state matches the committed state.
    NoChange,
    /// Change is pending; hold-off period has not elapsed.
    Pending,
    /// Change committed; the guard transitioned to the new state.
    Committed,
    /// Change was suppressed because a reversal arrived during hold-off.
    Suppressed,
}

impl HysteresisGuard {
    /// Create a new hysteresis guard with the controller's default hold-off.
    #[must_use]
    pub fn new(controller: ControllerId, initial_state: impl Into<String>) -> Self {
        Self {
            hold_off_micros: controller.timescale().hysteresis_hold_off_micros(),
            min_stable_ticks: match controller.timescale() {
                Timescale::Fast => 2,
                Timescale::Medium => 5,
                Timescale::Slow => 10,
            },
            controller,
            committed_state: initial_state.into(),
            pending: None,
            pending_stable_ticks: 0,
            suppressed_count: 0,
            committed_count: 0,
            last_transition_micros: 0,
        }
    }

    /// Create a hysteresis guard with custom hold-off parameters.
    #[must_use]
    pub fn with_params(
        controller: ControllerId,
        initial_state: impl Into<String>,
        hold_off_micros: i64,
        min_stable_ticks: u64,
    ) -> Self {
        Self {
            controller,
            committed_state: initial_state.into(),
            pending: None,
            hold_off_micros,
            pending_stable_ticks: 0,
            min_stable_ticks,
            suppressed_count: 0,
            committed_count: 0,
            last_transition_micros: 0,
        }
    }

    /// Propose a new state.  Returns the outcome of the proposal.
    ///
    /// If `proposed_state` matches the committed state, any pending
    /// transition is cancelled (a reversal during hold-off).
    ///
    /// If `proposed_state` is new, it enters the pending queue.  It will
    /// only commit once both the tick stability requirement AND the
    /// wall-clock hold-off have elapsed.
    pub fn propose(&mut self, proposed_state: &str, now_micros: i64) -> HysteresisOutcome {
        // Already in the proposed state — cancel any pending reversal.
        if proposed_state == self.committed_state {
            if self.pending.is_some() {
                self.pending = None;
                self.pending_stable_ticks = 0;
                self.suppressed_count = self.suppressed_count.saturating_add(1);
                return HysteresisOutcome::Suppressed;
            }
            return HysteresisOutcome::NoChange;
        }

        // Check if this is the same pending proposal or a new one.
        if let Some(ref pending) = self.pending {
            if pending.proposed_state == proposed_state {
                // Same proposal; tick the stability counter.
                self.pending_stable_ticks = self.pending_stable_ticks.saturating_add(1);

                // Check both conditions for committing.
                let wall_clock_ok =
                    now_micros.saturating_sub(pending.proposed_at_micros) >= self.hold_off_micros;
                let tick_ok = self.pending_stable_ticks >= self.min_stable_ticks;

                if wall_clock_ok && tick_ok {
                    self.committed_state = proposed_state.to_string();
                    self.pending = None;
                    self.pending_stable_ticks = 0;
                    self.committed_count = self.committed_count.saturating_add(1);
                    self.last_transition_micros = now_micros;
                    return HysteresisOutcome::Committed;
                }
                return HysteresisOutcome::Pending;
            }
            // Different proposal — the previous pending change was a false
            // alarm.  Replace it with the new proposal.
            self.suppressed_count = self.suppressed_count.saturating_add(1);
        }

        // Start a new pending proposal.
        self.pending = Some(PendingStateChange {
            proposed_state: proposed_state.to_string(),
            proposed_at_micros: now_micros,
        });
        self.pending_stable_ticks = 1;
        HysteresisOutcome::Pending
    }

    /// Current committed state.
    #[must_use]
    pub fn committed_state(&self) -> &str {
        &self.committed_state
    }

    /// Whether a state change is pending.
    #[must_use]
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The pending proposed state, if any.
    #[must_use]
    pub fn pending_state(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.proposed_state.as_str())
    }

    /// Number of suppressed oscillations.
    #[must_use]
    pub const fn suppressed_count(&self) -> u64 {
        self.suppressed_count
    }

    /// Number of committed transitions.
    #[must_use]
    pub const fn committed_count(&self) -> u64 {
        self.committed_count
    }

    /// Microseconds since the last committed transition.
    #[must_use]
    pub const fn micros_since_last_transition(&self, now_micros: i64) -> i64 {
        if self.last_transition_micros == 0 {
            return i64::MAX;
        }
        now_micros.saturating_sub(self.last_transition_micros)
    }
}

// ---------------------------------------------------------------------------
// Interaction rules (cross-controller constraints)
// ---------------------------------------------------------------------------

/// An interaction rule that constrains how two controllers may affect
/// each other's decisions.
#[derive(Debug, Clone)]
pub struct InteractionRule {
    /// The controller that produces the signal.
    pub source: ControllerId,
    /// The controller that consumes the signal.
    pub target: ControllerId,
    /// The type of interaction.
    pub kind: InteractionKind,
    /// Human-readable description.
    pub description: &'static str,
}

/// Types of cross-controller interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    /// Source's output gates (enables/disables) target's operation.
    Gates,
    /// Source provides input data that target uses for its calculations.
    Feeds,
    /// Source can veto target's proposed actions.
    Vetoes,
    /// Source's state change forces target to reset or roll back.
    ForcesReset,
    /// Source's output modulates target's aggressiveness.
    Modulates,
}

/// The canonical set of cross-controller interaction rules.
///
/// These rules encode the dependencies and constraints between controllers.
/// Adding a new controller requires adding entries here and verifying that
/// the new controller does not violate existing interaction invariants.
pub const INTERACTION_RULES: [InteractionRule; 19] = [
    // --- E-Process and CUSUM feed CalibrationGuard ---
    InteractionRule {
        source: ControllerId::EProcess,
        target: ControllerId::CalibrationGuard,
        kind: InteractionKind::Feeds,
        description: "e-process miscalibration alarm triggers safe-mode entry",
    },
    InteractionRule {
        source: ControllerId::Cusum,
        target: ControllerId::CalibrationGuard,
        kind: InteractionKind::Feeds,
        description: "CUSUM degradation alarm triggers safe-mode entry",
    },
    // --- CalibrationGuard gates AdmissibilityGates ---
    InteractionRule {
        source: ControllerId::CalibrationGuard,
        target: ControllerId::AdmissibilityGates,
        kind: InteractionKind::Gates,
        description: "safe mode disables high-risk action admissibility",
    },
    // --- CalibrationGuard vetoes AdaptationEngine promotion ---
    InteractionRule {
        source: ControllerId::CalibrationGuard,
        target: ControllerId::AdaptationEngine,
        kind: InteractionKind::Vetoes,
        description: "safe-mode entry vetoes candidate promotion and triggers rollback to incumbent",
    },
    // --- ConformalRiskBudget gates AdmissibilityGates ---
    InteractionRule {
        source: ControllerId::ConformalRiskBudget,
        target: ControllerId::AdmissibilityGates,
        kind: InteractionKind::Gates,
        description: "exhausted risk budget denies action admissibility",
    },
    // --- EffectSemantics gates AdmissibilityGates ---
    InteractionRule {
        source: ControllerId::EffectSemantics,
        target: ControllerId::AdmissibilityGates,
        kind: InteractionKind::Gates,
        description: "cooldown/escalation state can deny action admissibility",
    },
    // --- AdmissibilityGates vetoes all downstream action execution ---
    InteractionRule {
        source: ControllerId::AdmissibilityGates,
        target: ControllerId::FairnessGuards,
        kind: InteractionKind::Feeds,
        description: "admissibility verdict is input to fairness assessment",
    },
    // --- FairnessGuards vetoes actions even if admissibility said OK ---
    InteractionRule {
        source: ControllerId::FairnessGuards,
        target: ControllerId::VoIControl,
        kind: InteractionKind::Gates,
        description: "fairness-impacted targets suppress exploration on them",
    },
    // --- RegretTracker feeds AdaptationEngine ---
    InteractionRule {
        source: ControllerId::RegretTracker,
        target: ControllerId::AdaptationEngine,
        kind: InteractionKind::Feeds,
        description: "cumulative regret drives policy evaluation decisions",
    },
    // --- ShrinkageEstimator modulates ConformalRiskBudget (cross-tick) ---
    InteractionRule {
        source: ControllerId::ShrinkageEstimator,
        target: ControllerId::ConformalRiskBudget,
        kind: InteractionKind::Modulates,
        description: "shrunk estimates improve sparse-stratum risk budgets (applied next tick)",
    },
    // --- SlowController modulates probe budget ---
    InteractionRule {
        source: ControllerId::SlowController,
        target: ControllerId::AdmissibilityGates,
        kind: InteractionKind::Modulates,
        description: "PI controller adjusts probe budget fraction",
    },
    // --- Cusum feeds RegimeManager ---
    InteractionRule {
        source: ControllerId::Cusum,
        target: ControllerId::RegimeManager,
        kind: InteractionKind::Feeds,
        description: "CUSUM change direction triggers regime phase transition",
    },
    // --- RegimeManager gates AdaptationEngine ---
    InteractionRule {
        source: ControllerId::RegimeManager,
        target: ControllerId::AdaptationEngine,
        kind: InteractionKind::Gates,
        description: "unstable regime prevents policy promotion",
    },
    // --- RegimeManager forces FairnessGuards discount ---
    InteractionRule {
        source: ControllerId::RegimeManager,
        target: ControllerId::FairnessGuards,
        kind: InteractionKind::ForcesReset,
        description: "regime shift discounts pre-regime fairness history",
    },
    // --- AdaptationEngine feeds PolicyCertificates ---
    InteractionRule {
        source: ControllerId::AdaptationEngine,
        target: ControllerId::PolicyCertificates,
        kind: InteractionKind::Feeds,
        description: "shadow regret data is input to promotion certification",
    },
    // --- PolicyCertificates vetoes policy promotion ---
    InteractionRule {
        source: ControllerId::PolicyCertificates,
        target: ControllerId::AdaptationEngine,
        kind: InteractionKind::Vetoes,
        description: "insufficient evidence blocks policy promotion",
    },
    // --- CalibrationGuard vetoes PolicyCertificates ---
    InteractionRule {
        source: ControllerId::CalibrationGuard,
        target: ControllerId::PolicyCertificates,
        kind: InteractionKind::Vetoes,
        description: "safe mode blocks policy promotion certification",
    },
    // --- VoIControl modulates AdmissibilityGates (cross-tick) ---
    InteractionRule {
        source: ControllerId::VoIControl,
        target: ControllerId::AdmissibilityGates,
        kind: InteractionKind::Modulates,
        description: "experiment budget availability modulates exploration admissibility (applied next tick)",
    },
    // --- FairnessGuards feeds PolicyCertificates ---
    InteractionRule {
        source: ControllerId::FairnessGuards,
        target: ControllerId::PolicyCertificates,
        kind: InteractionKind::Feeds,
        description: "fairness assessment is required input for certification",
    },
];

/// Return all interaction rules where the given controller is the source.
#[must_use]
pub fn outgoing_interactions(id: ControllerId) -> Vec<&'static InteractionRule> {
    INTERACTION_RULES
        .iter()
        .filter(|rule| rule.source == id)
        .collect()
}

/// Return all interaction rules where the given controller is the target.
#[must_use]
pub fn incoming_interactions(id: ControllerId) -> Vec<&'static InteractionRule> {
    INTERACTION_RULES
        .iter()
        .filter(|rule| rule.target == id)
        .collect()
}

// ---------------------------------------------------------------------------
// Compatibility matrix for new controllers
// ---------------------------------------------------------------------------

/// Checklist entry for evaluating whether a new controller is compatible
/// with the existing ATC stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityCheck {
    /// Short label for this check.
    pub id: &'static str,
    /// Question the contributor must answer.
    pub question: &'static str,
    /// What happens if the answer is "no" or "unknown".
    pub failure_consequence: &'static str,
}

/// The compatibility checklist that must be evaluated before adding a new
/// controller to the ATC stack.  Each check must have an explicit "yes"
/// answer with evidence before the controller can be merged.
pub const COMPATIBILITY_CHECKLIST: [CompatibilityCheck; 10] = [
    CompatibilityCheck {
        id: "timescale_assigned",
        question: "Does the new controller have an explicit timescale (Fast/Medium/Slow)?",
        failure_consequence: "Cannot determine update ordering or hysteresis parameters.",
    },
    CompatibilityCheck {
        id: "eval_order_assigned",
        question: "Does the new controller have a unique eval_order position?",
        failure_consequence: "May run in wrong order relative to its dependencies.",
    },
    CompatibilityCheck {
        id: "authority_defined",
        question: "Is there an AuthorityBoundary entry listing owns/can_veto/must_not?",
        failure_consequence: "Authority overlaps with existing controllers go undetected.",
    },
    CompatibilityCheck {
        id: "no_authority_overlap",
        question: "Does the new controller's 'owns' list NOT overlap with any existing controller's 'owns'?",
        failure_consequence: "Two controllers will fight over the same decision.",
    },
    CompatibilityCheck {
        id: "veto_chain_documented",
        question: "If the controller can veto actions, is the veto chain updated?",
        failure_consequence: "Actions may bypass the new veto or be double-vetoed.",
    },
    CompatibilityCheck {
        id: "interaction_rules_added",
        question: "Are all cross-controller data flows documented in INTERACTION_RULES?",
        failure_consequence: "Hidden dependencies cause order-of-evaluation bugs.",
    },
    CompatibilityCheck {
        id: "hysteresis_configured",
        question: "Does the controller use HysteresisGuard for state changes?",
        failure_consequence: "Rapid flapping under noisy evidence.",
    },
    CompatibilityCheck {
        id: "fairness_impact_assessed",
        question: "Has the controller's impact on fairness budget been evaluated?",
        failure_consequence: "May concentrate harm on agents/cohorts without detection.",
    },
    CompatibilityCheck {
        id: "safe_mode_behavior_defined",
        question: "Is the controller's behavior under safe mode explicitly defined?",
        failure_consequence: "Controller may act aggressively during calibration failure.",
    },
    CompatibilityCheck {
        id: "rollback_behavior_defined",
        question: "Does the controller handle policy rollback cleanly?",
        failure_consequence: "Stale state after rollback causes inconsistent decisions.",
    },
];

// ---------------------------------------------------------------------------
// Composition health diagnostics
// ---------------------------------------------------------------------------

/// A detected composition violation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionViolation {
    /// Type of violation.
    pub kind: ViolationKind,
    /// Which controllers are involved.
    pub controllers: Vec<ControllerId>,
    /// Human-readable description.
    pub description: String,
}

/// Types of composition violations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// A controller reads from another that runs after it in eval order.
    OrderInversion,
    /// Two controllers both claim ownership of the same decision.
    AuthorityOverlap,
    /// A controller vetoes an action it also owns (self-veto).
    SelfVeto,
    /// A fast controller is modifying slow state.
    TimescaleCrossing,
    /// A controller bypasses a required upstream gate.
    GateBypass,
}

/// Validate the static composition tables for internal consistency.
///
/// This is a compile-time-adjacent check (called in tests or at startup)
/// that ensures the authority boundaries, interaction rules, and eval
/// order do not contain contradictions.
#[must_use]
pub fn validate_composition() -> Vec<CompositionViolation> {
    let mut violations = Vec::new();

    // Check 1: eval order respects reads_from dependencies.
    for boundary in &AUTHORITY_BOUNDARIES {
        for dep in boundary.reads_from {
            if dep.eval_order() >= boundary.controller.eval_order() {
                violations.push(CompositionViolation {
                    kind: ViolationKind::OrderInversion,
                    controllers: vec![boundary.controller, *dep],
                    description: format!(
                        "{} (order {}) reads from {} (order {}) which runs at the same time or later",
                        boundary.controller.as_str(),
                        boundary.controller.eval_order(),
                        dep.as_str(),
                        dep.eval_order(),
                    ),
                });
            }
        }
    }

    // Check 2: no authority overlap (two controllers owning the same thing).
    for (i, a) in AUTHORITY_BOUNDARIES.iter().enumerate() {
        for b in AUTHORITY_BOUNDARIES.iter().skip(i + 1) {
            for owned in a.owns {
                if b.owns.contains(owned) {
                    violations.push(CompositionViolation {
                        kind: ViolationKind::AuthorityOverlap,
                        controllers: vec![a.controller, b.controller],
                        description: format!(
                            "both {} and {} claim ownership of '{}'",
                            a.controller.as_str(),
                            b.controller.as_str(),
                            owned,
                        ),
                    });
                }
            }
        }
    }

    // Check 3: no self-vetoes (a controller vetoing its own actions).
    for boundary in &AUTHORITY_BOUNDARIES {
        for vetoed in boundary.can_veto {
            if boundary.owns.contains(vetoed) {
                violations.push(CompositionViolation {
                    kind: ViolationKind::SelfVeto,
                    controllers: vec![boundary.controller],
                    description: format!(
                        "{} both owns and vetoes '{}'",
                        boundary.controller.as_str(),
                        vetoed,
                    ),
                });
            }
        }
    }

    // Check 4: interaction rules respect timescale ordering.
    //
    // ForcesReset from a slower controller to a faster controller is
    // architecturally correct: regime shifts propagate downward to reset
    // stale state (e.g. RegimeManager resets FairnessGuards history after
    // a regime change).  The violation is the opposite direction: a fast
    // controller forcing a reset on a slower controller would be an
    // authority inversion.
    for rule in &INTERACTION_RULES {
        if rule.kind == InteractionKind::ForcesReset
            && rule.source.timescale() < rule.target.timescale()
        {
            violations.push(CompositionViolation {
                kind: ViolationKind::TimescaleCrossing,
                controllers: vec![rule.source, rule.target],
                description: format!(
                    "fast controller {} forces reset on slower controller {} (authority inversion)",
                    rule.source.as_str(),
                    rule.target.as_str(),
                ),
            });
        }
    }

    // Check 5: interaction rules don't have source running after target.
    for rule in &INTERACTION_RULES {
        if matches!(rule.kind, InteractionKind::Feeds | InteractionKind::Gates)
            && rule.source.eval_order() > rule.target.eval_order()
        {
            violations.push(CompositionViolation {
                kind: ViolationKind::OrderInversion,
                controllers: vec![rule.source, rule.target],
                description: format!(
                    "{} (order {}) feeds/gates {} (order {}) but runs later",
                    rule.source.as_str(),
                    rule.source.eval_order(),
                    rule.target.as_str(),
                    rule.target.eval_order(),
                ),
            });
        }
    }

    violations
}

// ---------------------------------------------------------------------------
// Tick-phase envelope
// ---------------------------------------------------------------------------

/// The phases of a single ATC tick, in execution order.
///
/// This enum documents the canonical tick structure.  `run_tick` in the
/// engine must execute these phases in order.  Each phase maps to one
/// or more controllers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickPhase {
    /// Phase 0: Read sensor inputs and resolve stale feedback.
    SensorIngestion,
    /// Phase 1: Fast guards evaluate (e-process, CUSUM, calibration).
    FastGuardEvaluation,
    /// Phase 2: Per-action admissibility gating (conformal, cooldown, gates).
    ActionGating,
    /// Phase 3: Liveness evaluation and decision core queries.
    LivenessEvaluation,
    /// Phase 4: Deadlock detection and conflict analysis.
    DeadlockDetection,
    /// Phase 5: Fairness assessment of proposed actions.
    FairnessAssessment,
    /// Phase 6: Probe scheduling with budget constraints.
    ProbeScheduling,
    /// Phase 7: Conformal uncertainty withholding (post-gating veto).
    ConformalWithholding,
    /// Phase 8: Effect emission (advisory messages, releases, probes).
    EffectEmission,
    /// Phase 9: Slow controller PI update and budget adjustment.
    SlowControlUpdate,
    /// Phase 10: Regime detection and phase transition.
    RegimeDetection,
    /// Phase 11: Policy evaluation, shadow comparison, promotion/rollback.
    PolicyEvaluation,
    /// Phase 12: Summary and telemetry snapshot.
    SummarySnapshot,
}

impl TickPhase {
    /// All phases in execution order.
    pub const ALL: [Self; 13] = [
        Self::SensorIngestion,
        Self::FastGuardEvaluation,
        Self::ActionGating,
        Self::LivenessEvaluation,
        Self::DeadlockDetection,
        Self::FairnessAssessment,
        Self::ProbeScheduling,
        Self::ConformalWithholding,
        Self::EffectEmission,
        Self::SlowControlUpdate,
        Self::RegimeDetection,
        Self::PolicyEvaluation,
        Self::SummarySnapshot,
    ];

    /// Which controllers are active during this phase.
    #[must_use]
    pub const fn active_controllers(self) -> &'static [ControllerId] {
        match self {
            Self::SensorIngestion
            | Self::LivenessEvaluation
            | Self::DeadlockDetection
            | Self::SummarySnapshot => &[],
            Self::FastGuardEvaluation => &[
                ControllerId::EProcess,
                ControllerId::Cusum,
                ControllerId::CalibrationGuard,
            ],
            Self::ActionGating => &[
                ControllerId::ConformalRiskBudget,
                ControllerId::EffectSemantics,
                ControllerId::AdmissibilityGates,
            ],
            Self::FairnessAssessment => &[ControllerId::FairnessGuards],
            Self::ProbeScheduling => &[ControllerId::VoIControl],
            Self::ConformalWithholding => &[ControllerId::ConformalRiskBudget],
            Self::EffectEmission => &[ControllerId::EffectSemantics],
            Self::SlowControlUpdate => &[
                ControllerId::SlowController,
                ControllerId::ShrinkageEstimator,
                ControllerId::RegretTracker,
            ],
            Self::RegimeDetection => &[ControllerId::RegimeManager],
            Self::PolicyEvaluation => &[
                ControllerId::AdaptationEngine,
                ControllerId::PolicyCertificates,
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Stress-mode veto precedence
// ---------------------------------------------------------------------------

/// Under system stress, multiple controllers may issue conflicting signals.
///
/// Safe mode, regime transition, high regret, and budget debt can all surface
/// at once. This table
/// defines the precedence order: higher-precedence vetoes override lower
/// ones.  The engine never needs to "break ties" because the precedence
/// is strict.
///
/// Precedence (highest first):
/// 1. Operator override (always wins)
/// 2. CalibrationGuard safe-mode (forces conservative behavior)
/// 3. ConformalRiskBudget exhaustion (hard false-action limit)
/// 4. FairnessGuards burden-concentration (prevents agent harm)
/// 5. AdmissibilityGates composite verdict (aggregates the above)
/// 6. EffectSemantics cooldown (prevents spam)
/// 7. RegimeManager instability (prevents premature policy changes)
/// 8. VoIControl experiment-budget (prevents over-exploration)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VetoPrecedence {
    /// Lowest precedence.
    ExperimentBudget = 0,
    RegimeInstability = 1,
    Cooldown = 2,
    AdmissibilityComposite = 3,
    BurdenConcentration = 4,
    RiskBudgetExhaustion = 5,
    SafeMode = 6,
    /// Highest precedence (always wins, never overridden).
    OperatorOverride = 7,
}

impl VetoPrecedence {
    /// Map a controller to its veto precedence when it vetoes an action.
    #[must_use]
    pub const fn for_controller(id: ControllerId) -> Self {
        match id {
            ControllerId::CalibrationGuard => Self::SafeMode,
            ControllerId::ConformalRiskBudget => Self::RiskBudgetExhaustion,
            ControllerId::FairnessGuards => Self::BurdenConcentration,
            ControllerId::EffectSemantics => Self::Cooldown,
            ControllerId::RegimeManager => Self::RegimeInstability,
            ControllerId::VoIControl => Self::ExperimentBudget,
            // AdmissibilityGates and all controllers without a stronger
            // precedence mapping fall back to the composite admissibility tier.
            _ => Self::AdmissibilityComposite,
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
    fn all_controllers_have_unique_eval_order() {
        let mut orders: Vec<u8> = ControllerId::ALL.iter().map(|c| c.eval_order()).collect();
        let original_len = orders.len();
        orders.sort_unstable();
        orders.dedup();
        assert_eq!(
            orders.len(),
            original_len,
            "duplicate eval_order values in ControllerId::ALL"
        );
    }

    #[test]
    fn all_controllers_sorted_in_eval_order() {
        for window in ControllerId::ALL.windows(2) {
            assert!(
                window[0].eval_order() < window[1].eval_order(),
                "{} (order {}) should come before {} (order {})",
                window[0].as_str(),
                window[0].eval_order(),
                window[1].as_str(),
                window[1].eval_order(),
            );
        }
    }

    #[test]
    fn authority_boundaries_cover_all_controllers() {
        for controller in ControllerId::ALL {
            assert!(
                AUTHORITY_BOUNDARIES
                    .iter()
                    .any(|b| b.controller == controller),
                "missing AuthorityBoundary for {}",
                controller.as_str()
            );
        }
    }

    #[test]
    fn no_authority_overlap_in_owns() {
        let violations: Vec<_> = validate_composition()
            .into_iter()
            .filter(|v| v.kind == ViolationKind::AuthorityOverlap)
            .collect();
        assert!(
            violations.is_empty(),
            "authority overlaps detected: {violations:?}"
        );
    }

    #[test]
    fn no_self_vetoes() {
        let violations: Vec<_> = validate_composition()
            .into_iter()
            .filter(|v| v.kind == ViolationKind::SelfVeto)
            .collect();
        assert!(
            violations.is_empty(),
            "self-vetoes detected: {violations:?}"
        );
    }

    #[test]
    fn no_order_inversions() {
        let violations: Vec<_> = validate_composition()
            .into_iter()
            .filter(|v| v.kind == ViolationKind::OrderInversion)
            .collect();
        assert!(
            violations.is_empty(),
            "order inversions detected: {violations:?}"
        );
    }

    #[test]
    fn no_timescale_crossings() {
        let violations: Vec<_> = validate_composition()
            .into_iter()
            .filter(|v| v.kind == ViolationKind::TimescaleCrossing)
            .collect();
        assert!(
            violations.is_empty(),
            "timescale crossings detected: {violations:?}"
        );
    }

    #[test]
    fn full_composition_validation_passes() {
        let violations = validate_composition();
        assert!(
            violations.is_empty(),
            "composition violations: {violations:?}"
        );
    }

    #[test]
    fn fast_controllers_run_before_medium_before_slow() {
        for controller in ControllerId::ALL {
            let timescale = controller.timescale();
            let order = controller.eval_order();
            match timescale {
                Timescale::Fast => assert!(
                    order < 50,
                    "{} is fast but order {}",
                    controller.as_str(),
                    order
                ),
                Timescale::Medium => assert!(
                    (50..100).contains(&order),
                    "{} is medium but order {}",
                    controller.as_str(),
                    order
                ),
                Timescale::Slow => assert!(
                    order >= 100,
                    "{} is slow but order {}",
                    controller.as_str(),
                    order
                ),
            }
        }
    }

    #[test]
    fn hysteresis_guard_no_change_for_same_state() {
        let mut guard = HysteresisGuard::new(ControllerId::CalibrationGuard, "nominal");
        assert_eq!(
            guard.propose("nominal", 1_000_000),
            HysteresisOutcome::NoChange
        );
        assert_eq!(guard.committed_state(), "nominal");
        assert!(!guard.has_pending());
    }

    #[test]
    fn hysteresis_guard_pending_then_committed() {
        let mut guard = HysteresisGuard::new(ControllerId::CalibrationGuard, "nominal");
        let hold_off = Timescale::Fast.hysteresis_hold_off_micros();

        // First proposal: enters pending.
        assert_eq!(
            guard.propose("safe_mode", 1_000_000),
            HysteresisOutcome::Pending
        );
        assert!(guard.has_pending());
        assert_eq!(guard.committed_state(), "nominal");

        // Second proposal: still pending (need wall-clock hold-off).
        let result = guard.propose("safe_mode", 1_000_000 + hold_off);
        // With 2 ticks and enough wall-clock time, should commit.
        assert_eq!(result, HysteresisOutcome::Committed);
        assert_eq!(guard.committed_state(), "safe_mode");
        assert!(!guard.has_pending());
        assert_eq!(guard.committed_count(), 1);
    }

    #[test]
    fn hysteresis_guard_suppresses_reversal() {
        let mut guard = HysteresisGuard::new(ControllerId::CalibrationGuard, "nominal");

        // Propose change.
        assert_eq!(
            guard.propose("safe_mode", 1_000_000),
            HysteresisOutcome::Pending
        );

        // Reverse before hold-off expires.
        assert_eq!(
            guard.propose("nominal", 1_100_000),
            HysteresisOutcome::Suppressed
        );
        assert!(!guard.has_pending());
        assert_eq!(guard.committed_state(), "nominal");
        assert_eq!(guard.suppressed_count(), 1);
    }

    #[test]
    fn hysteresis_guard_custom_params() {
        let mut guard = HysteresisGuard::with_params(
            ControllerId::RegimeManager,
            "stable",
            10_000_000, // 10 second hold-off
            3,          // 3 stable ticks
        );

        // Need 3 ticks AND 10s hold-off.
        assert_eq!(
            guard.propose("transitioning", 0),
            HysteresisOutcome::Pending
        );
        assert_eq!(
            guard.propose("transitioning", 3_000_000),
            HysteresisOutcome::Pending
        );
        // Third tick at 10s mark should commit.
        assert_eq!(
            guard.propose("transitioning", 10_000_000),
            HysteresisOutcome::Committed
        );
        assert_eq!(guard.committed_state(), "transitioning");
    }

    #[test]
    fn hysteresis_guard_different_pending_replaces() {
        let mut guard = HysteresisGuard::new(ControllerId::SlowController, "nominal");

        // Propose A.
        assert_eq!(
            guard.propose("pressure", 1_000_000),
            HysteresisOutcome::Pending
        );
        // Propose B (different from A and from committed).
        assert_eq!(
            guard.propose("conservative", 2_000_000),
            HysteresisOutcome::Pending
        );
        assert_eq!(guard.pending_state(), Some("conservative"));
        // The switch from A to B counted as a suppression.
        assert_eq!(guard.suppressed_count(), 1);
    }

    #[test]
    fn veto_chain_no_vetoes() {
        let result = evaluate_veto_chain("send_advisory", 0, &|_| None);
        assert!(result.permitted);
        assert!(result.vetoes.is_empty());
    }

    #[test]
    fn veto_chain_single_veto() {
        let result = evaluate_veto_chain("release_reservations", 0, &|id| {
            if id == ControllerId::CalibrationGuard {
                Some("safe mode active".to_string())
            } else {
                None
            }
        });
        assert!(result.is_vetoed());
        assert_eq!(result.vetoes.len(), 1);
        assert_eq!(result.vetoes[0].source, ControllerId::CalibrationGuard);
    }

    #[test]
    fn veto_chain_multiple_vetoes() {
        let result = evaluate_veto_chain("release_reservations", 0, &|id| match id {
            ControllerId::CalibrationGuard => Some("safe mode".to_string()),
            ControllerId::ConformalRiskBudget => Some("budget exhausted".to_string()),
            ControllerId::FairnessGuards => Some("burden concentration".to_string()),
            _ => None,
        });
        assert!(result.is_vetoed());
        // CalibrationGuard, ConformalRiskBudget, AdmissibilityGates, FairnessGuards
        // all can veto release_reservations; 3 of them fire.
        assert!(result.vetoes.len() >= 3);
    }

    #[test]
    fn veto_precedence_ordering() {
        assert!(VetoPrecedence::OperatorOverride > VetoPrecedence::SafeMode);
        assert!(VetoPrecedence::SafeMode > VetoPrecedence::RiskBudgetExhaustion);
        assert!(VetoPrecedence::RiskBudgetExhaustion > VetoPrecedence::BurdenConcentration);
        assert!(VetoPrecedence::BurdenConcentration > VetoPrecedence::AdmissibilityComposite);
        assert!(VetoPrecedence::AdmissibilityComposite > VetoPrecedence::Cooldown);
        assert!(VetoPrecedence::Cooldown > VetoPrecedence::RegimeInstability);
        assert!(VetoPrecedence::RegimeInstability > VetoPrecedence::ExperimentBudget);
    }

    #[test]
    fn tick_phases_are_in_order() {
        // All phases should have distinct indices when iterated in ALL order.
        assert_eq!(TickPhase::ALL.len(), 13);
    }

    #[test]
    fn tick_phase_controllers_have_valid_ids() {
        for phase in TickPhase::ALL {
            for controller in phase.active_controllers() {
                assert!(
                    ControllerId::ALL.contains(controller),
                    "phase {phase:?} references unknown controller {controller:?}"
                );
            }
        }
    }

    #[test]
    fn outgoing_interactions_for_calibration_guard() {
        let outgoing = outgoing_interactions(ControllerId::CalibrationGuard);
        assert!(
            outgoing.len() >= 2,
            "CalibrationGuard should have at least 2 outgoing interactions"
        );
        let targets: Vec<_> = outgoing.iter().map(|r| r.target).collect();
        assert!(targets.contains(&ControllerId::AdmissibilityGates));
        assert!(targets.contains(&ControllerId::AdaptationEngine));
    }

    #[test]
    fn incoming_interactions_for_admissibility_gates() {
        let incoming = incoming_interactions(ControllerId::AdmissibilityGates);
        assert!(
            incoming.len() >= 3,
            "AdmissibilityGates should have at least 3 incoming interactions"
        );
        let sources: Vec<_> = incoming.iter().map(|r| r.source).collect();
        assert!(sources.contains(&ControllerId::CalibrationGuard));
        assert!(sources.contains(&ControllerId::ConformalRiskBudget));
        assert!(sources.contains(&ControllerId::EffectSemantics));
    }

    #[test]
    fn compatibility_checklist_has_all_required_checks() {
        let ids: Vec<_> = COMPATIBILITY_CHECKLIST.iter().map(|c| c.id).collect();
        assert!(ids.contains(&"timescale_assigned"));
        assert!(ids.contains(&"eval_order_assigned"));
        assert!(ids.contains(&"authority_defined"));
        assert!(ids.contains(&"no_authority_overlap"));
        assert!(ids.contains(&"veto_chain_documented"));
        assert!(ids.contains(&"interaction_rules_added"));
        assert!(ids.contains(&"hysteresis_configured"));
        assert!(ids.contains(&"fairness_impact_assessed"));
        assert!(ids.contains(&"safe_mode_behavior_defined"));
        assert!(ids.contains(&"rollback_behavior_defined"));
    }

    #[test]
    fn timescale_hysteresis_increases_with_timescale() {
        assert!(
            Timescale::Fast.hysteresis_hold_off_micros()
                < Timescale::Medium.hysteresis_hold_off_micros()
        );
        assert!(
            Timescale::Medium.hysteresis_hold_off_micros()
                < Timescale::Slow.hysteresis_hold_off_micros()
        );
    }

    #[test]
    fn timescale_min_update_interval_increases() {
        assert!(
            Timescale::Fast.min_update_interval_ticks()
                < Timescale::Medium.min_update_interval_ticks()
        );
        assert!(
            Timescale::Medium.min_update_interval_ticks()
                < Timescale::Slow.min_update_interval_ticks()
        );
    }

    #[test]
    fn controller_id_as_str_round_trip() {
        for controller in ControllerId::ALL {
            let s = controller.as_str();
            assert!(!s.is_empty(), "empty as_str for {controller:?}");
        }
    }

    #[test]
    fn hysteresis_micros_since_last_transition() {
        let mut guard = HysteresisGuard::new(ControllerId::CalibrationGuard, "nominal");
        // No transition yet.
        assert_eq!(guard.micros_since_last_transition(1_000_000), i64::MAX);

        // Force a transition through the hold-off.
        let hold_off = Timescale::Fast.hysteresis_hold_off_micros();
        guard.propose("safe_mode", 0);
        guard.propose("safe_mode", hold_off);
        assert_eq!(guard.committed_state(), "safe_mode");

        // Now check micros_since.
        assert_eq!(
            guard.micros_since_last_transition(hold_off + 1_000_000),
            1_000_000
        );
    }
}
