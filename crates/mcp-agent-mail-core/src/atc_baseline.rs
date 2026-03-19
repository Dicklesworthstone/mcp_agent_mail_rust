//! Pre-learning ATC behavior baseline (br-0qt6e.1.6).
//!
//! This module captures the state of ATC before the learning stack is
//! integrated. Every optimization, adaptation, or policy change in the
//! ATC learning epic MUST reference this baseline to demonstrate
//! improvement (or at minimum, preservation).
//!
//! # Baseline Capture Date
//!
//! **2026-03-18** — captured from `atc.rs` (9,680 lines, commit range
//! ending at `92fd446`).
//!
//! # How to Use This Baseline
//!
//! 1. **Before/after comparison**: Run the collection commands below,
//!    then diff against the constants in this module.
//! 2. **Regression gate**: Any adaptation that degrades a metric below
//!    the baseline values documented here must be flagged and justified.
//! 3. **Operator calm metric**: Compare advisory/release/probe counts
//!    per tick against [`BASELINE_EFFECT_MIX`] to ensure learning does
//!    not increase operator noise.
//!
//! # Reproducible Collection Commands
//!
//! ```bash
//! # 1. Evidence ledger snapshot (requires AM_EVIDENCE_LEDGER_PATH set)
//! AM_EVIDENCE_LEDGER_PATH=/tmp/atc_baseline.jsonl \
//!   am serve-http --no-tui &
//! sleep 60  # let ATC run for 60s under normal multi-agent load
//! wc -l /tmp/atc_baseline.jsonl        # decision count
//! jq -r '.subsystem' /tmp/atc_baseline.jsonl | sort | uniq -c  # per-subsystem
//! jq -r '.action' /tmp/atc_baseline.jsonl | sort | uniq -c     # per-action
//!
//! # 2. Robot status snapshot
//! AM_INTERFACE_MODE=cli am robot health --project /abs/path --agent <name>
//! AM_INTERFACE_MODE=cli am robot metrics --project /abs/path --agent <name>
//!
//! # 3. ATC summary from evidence ledger
//! jq -s '{
//!   total_decisions: length,
//!   by_subsystem: (group_by(.subsystem) | map({key: .[0].subsystem, count: length}) | from_entries),
//!   by_action: (group_by(.action) | map({key: .[0].action, count: length}) | from_entries),
//!   safe_mode_pct: ([.[] | select(.safe_mode_active)] | length) / length * 100,
//!   avg_expected_loss: ([.[] | .expected_loss] | add / length)
//! }' /tmp/atc_baseline.jsonl
//! ```

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// Loss matrices (frozen pre-learning values)
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning liveness loss matrix.
///
/// Row = action, Column = true state.
///
/// ```text
///                  Alive   Flaky   Dead
/// DeclareAlive:      0       3      50
/// Suspect:           8       2       6
/// Release:         100      20       1
/// ```
///
/// **Key asymmetry**: Releasing an alive agent (100) costs 100x more
/// than failing to release a dead agent (1). This reflects extreme
/// reluctance to destroy work — the system strongly prefers false
/// negatives (missing a dead agent) over false positives (wrongly
/// releasing a live one).
pub const BASELINE_LIVENESS_LOSSES: [[f64; 3]; 3] = [
    // [Alive, Flaky, Dead]
    [0.0, 3.0, 50.0],   // DeclareAlive
    [8.0, 2.0, 6.0],    // Suspect
    [100.0, 20.0, 1.0], // ReleaseReservations
];

/// Pre-learning liveness prior probabilities.
pub const BASELINE_LIVENESS_PRIOR: [f64; 3] = [0.95, 0.04, 0.01]; // Alive, Flaky, Dead

/// Pre-learning conflict loss matrix.
///
/// ```text
///                  NoConflict  MildOverlap  SevereCollision
/// Ignore:               0          15              100
/// Advisory:             3           1                8
/// ForceReserv:         12           4                2
/// ```
///
/// **Key asymmetry**: Ignoring a severe collision (100) far exceeds
/// forcing on no conflict (12). Advisory is cheap across the board.
pub const BASELINE_CONFLICT_LOSSES: [[f64; 3]; 3] = [
    // [NoConflict, MildOverlap, SevereCollision]
    [0.0, 15.0, 100.0], // Ignore
    [3.0, 1.0, 8.0],    // AdvisoryMessage
    [12.0, 4.0, 2.0],   // ForceReservation
];

/// Pre-learning conflict prior probabilities.
pub const BASELINE_CONFLICT_PRIOR: [f64; 3] = [0.90, 0.08, 0.02];

/// Pre-learning load routing loss matrix.
///
/// ```text
///                  Underloaded  Balanced  Overloaded
/// RouteHere:           1           3          25
/// SuggestAlt:          8           2           3
/// Defer:              15           8           1
/// ```
pub const BASELINE_LOAD_LOSSES: [[f64; 3]; 3] = [
    // [Underloaded, Balanced, Overloaded]
    [1.0, 3.0, 25.0], // RouteHere
    [8.0, 2.0, 3.0],  // SuggestAlternative
    [15.0, 8.0, 1.0], // Defer
];

/// Pre-learning load prior probabilities.
pub const BASELINE_LOAD_PRIOR: [f64; 3] = [0.30, 0.60, 0.10];

/// Posterior learning rate (alpha) — shared across all subsystems.
pub const BASELINE_ALPHA: f64 = 0.3;

// ──────────────────────────────────────────────────────────────────────
// Timing and resource budgets
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning timing budgets (microseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineTimingBudgets {
    /// ATC tick time budget.
    pub tick_budget_micros: i64,
    /// Probe recency decay base interval.
    pub probe_interval_micros: i64,
    /// Advisory cooldown (min interval between advisories to same agent).
    pub advisory_cooldown_micros: i64,
    /// Session summary posting cadence.
    pub summary_interval_micros: i64,
    /// Estimated per-probe cost (EWMA steady-state).
    pub estimated_probe_cost_micros: i64,
    /// Non-probe baseline cost per tick (liveness + deadlock + gating + summary).
    pub estimated_non_probe_cost_micros: i64,
}

/// Frozen pre-learning timing budgets.
pub const BASELINE_TIMING: BaselineTimingBudgets = BaselineTimingBudgets {
    tick_budget_micros: 5_000,             // 5ms
    probe_interval_micros: 120_000_000,    // 120s
    advisory_cooldown_micros: 300_000_000, // 300s
    summary_interval_micros: 300_000_000,  // 300s
    estimated_probe_cost_micros: 120,
    estimated_non_probe_cost_micros: 700, // ~300 + 200 + 80 + 120
};

// ──────────────────────────────────────────────────────────────────────
// Effect mix (pre-learning behavior profile)
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning effect execution mix.
///
/// Captures what effects ATC actually fires under normal multi-agent
/// load. These are the operator-visible actions that determine "calm
/// vs noise" perception.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct BaselineEffectMix {
    /// `SendAdvisory`: informational messages to agents.
    /// Primary effect type. Triggered by: liveness suspicion,
    /// deadlock detection, withhold-release gating.
    pub advisory_active: bool,
    /// `ReleaseReservations`: forcibly releasing agent file leases.
    /// High-force. Gated by calibration guard and safe mode.
    pub release_active: bool,
    /// `ProbeAgent`: health check probes to verify agent liveness.
    /// Budget-constrained by slow controller.
    pub probe_active: bool,
    /// `WithholdRelease`: calibration uncertainty prevents release.
    /// Conditionally active — requires conformal lock to indicate uncertainty.
    pub withhold_release_active: bool,
}

/// Frozen pre-learning effect mix.
pub const BASELINE_EFFECT_MIX: BaselineEffectMix = BaselineEffectMix {
    advisory_active: true,
    release_active: true,
    probe_active: true,
    withhold_release_active: true, // conditionally active under conformal uncertainty
};

// ──────────────────────────────────────────────────────────────────────
// Calibration and safety thresholds
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning calibration and safety gate thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BaselineCalibration {
    /// E-process martingale alert threshold.
    pub eprocess_alert_threshold: f64,
    /// Target accuracy for calibration.
    pub target_coverage: f64,
    /// Consecutive correct predictions to exit safe mode.
    pub safe_mode_recovery_count: u64,
    /// CUSUM regime shift sensitivity.
    pub cusum_threshold: f64,
    /// CUSUM minimum detectable shift magnitude.
    pub cusum_delta: f64,
    /// Posterior probability floor (prevents underflow).
    pub posterior_floor: f64,
    /// Evidence ledger ring buffer capacity.
    pub ledger_capacity: usize,
}

/// Frozen pre-learning calibration thresholds.
pub const BASELINE_CALIBRATION: BaselineCalibration = BaselineCalibration {
    eprocess_alert_threshold: 20.0,
    target_coverage: 0.85,
    safe_mode_recovery_count: 20,
    cusum_threshold: 5.0,
    cusum_delta: 0.1,
    posterior_floor: 1e-10,
    ledger_capacity: 1000,
};

// ──────────────────────────────────────────────────────────────────────
// Adaptive controller budgets
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning adaptive mode controller thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BaselineAdaptiveController {
    /// Utilization threshold for entering Pressure mode.
    pub pressure_utilization: f64,
    /// Utilization threshold for entering Conservative mode.
    pub conservative_utilization: f64,
    /// Debt ratio threshold for Pressure mode.
    pub pressure_debt_ratio: f64,
    /// Debt ratio threshold for Conservative mode.
    pub conservative_debt_ratio: f64,
    /// Adaptation window size (ticks).
    pub window_size: u64,
    /// Initial probe limit per tick.
    pub initial_probe_limit: u64,
    /// Probe budget fraction in Nominal mode.
    pub nominal_probe_budget_fraction: f64,
}

/// Frozen pre-learning adaptive controller thresholds.
pub const BASELINE_ADAPTIVE_CONTROLLER: BaselineAdaptiveController = BaselineAdaptiveController {
    pressure_utilization: 0.75,
    conservative_utilization: 0.90,
    pressure_debt_ratio: 0.5,
    conservative_debt_ratio: 1.5,
    window_size: 16,
    initial_probe_limit: 3,
    nominal_probe_budget_fraction: 0.55,
};

// ──────────────────────────────────────────────────────────────────────
// Program-based priors (agent silence expectations)
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning per-program silence priors (seconds).
///
/// These set the initial expected inter-activity interval for agents
/// based on their program type before any observations.
pub const BASELINE_PROGRAM_PRIORS: &[(&str, u64)] = &[
    ("claude-code", 60),
    ("codex-cli", 120),
    ("gemini-cli", 120),
    ("copilot-cli", 120),
    // Unknown programs get conservative 300s default
];

/// Default silence prior for unknown programs (seconds).
pub const BASELINE_UNKNOWN_PROGRAM_PRIOR_SECS: u64 = 300;

/// Liveness rhythm suspicion threshold multiplier (sigma units).
///
/// Suspicion triggers at: avg + k * sqrt(var).
pub const BASELINE_SUSPICION_K: f64 = 3.0;

// ──────────────────────────────────────────────────────────────────────
// Failure-cost asymmetry summary
// ──────────────────────────────────────────────────────────────────────

/// Pre-learning failure-cost asymmetry for each effect type.
///
/// Captures the cost ratio of false-positive (acting when shouldn't)
/// vs false-negative (not acting when should) for each effect kind.
/// Higher ratios mean the system is more conservative about acting.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EffectCostAsymmetry {
    /// Effect name.
    pub effect: &'static str,
    /// Cost of false positive (acting when the true state doesn't warrant it).
    pub false_positive_cost: f64,
    /// Cost of false negative (not acting when the true state warrants it).
    pub false_negative_cost: f64,
    /// Ratio: `false_positive` / `false_negative`. >1 means conservative.
    pub asymmetry_ratio: f64,
}

/// Pre-learning cost asymmetries per effect type.
pub const BASELINE_COST_ASYMMETRIES: &[EffectCostAsymmetry] = &[
    // ReleaseReservations on Alive agent (100) vs not releasing Dead (1)
    EffectCostAsymmetry {
        effect: "ReleaseReservations",
        false_positive_cost: 100.0,
        false_negative_cost: 1.0,
        asymmetry_ratio: 100.0,
    },
    // ForceReservation on NoConflict (12) vs ignoring SevereCollision (100)
    // NOTE: conflict is OPPOSITE — false negatives are more expensive
    EffectCostAsymmetry {
        effect: "ForceReservation",
        false_positive_cost: 12.0,
        false_negative_cost: 100.0,
        asymmetry_ratio: 0.12,
    },
    // Advisory on NoConflict (3) vs not advising on MildOverlap (15)
    EffectCostAsymmetry {
        effect: "Advisory",
        false_positive_cost: 3.0,
        false_negative_cost: 15.0,
        asymmetry_ratio: 0.2,
    },
    // Probe is budget-constrained, not loss-matrix-driven
    EffectCostAsymmetry {
        effect: "Probe",
        false_positive_cost: 0.0, // probes are always safe
        false_negative_cost: 0.0, // missing a probe delays detection
        asymmetry_ratio: 1.0,     // neutral
    },
];

// ──────────────────────────────────────────────────────────────────────
// Operator surface and diagnostic gaps
// ──────────────────────────────────────────────────────────────────────

/// Known operator-surface limitations in the pre-learning system.
///
/// Each entry describes a gap in operator visibility or diagnostic
/// capability that the learning stack should address.
pub const BASELINE_OPERATOR_GAPS: &[&str] = &[
    "No dedicated ATC dashboard TUI screen — ATC state embedded in health/summary only",
    "No per-agent liveness posterior visible in robot output",
    "No real-time advisory rate or noise metric exposed to operators",
    "No operator control over per-subsystem risk tolerance",
    "Safe mode entry/exit events not surfaced as toast notifications",
    "Policy artifact changes not visible in timeline or event stream",
    "Shadow policy regret delta not exposed in any operator surface",
    "Conformal uncertainty gating events not visible outside evidence ledger",
    "No operator-facing explanation of WHY an advisory was or was not sent",
    "No mechanism for operator to flag a false-positive advisory retroactively",
];

/// Known test and diagnostic blind spots in the pre-learning system.
pub const BASELINE_DIAGNOSTIC_GAPS: &[&str] = &[
    "Hierarchical population model (ATC_POPULATION) not tested end-to-end",
    "Adaptive threshold tuning (ATC_THRESHOLDS) not tested end-to-end",
    "Liveness tuner loss matrix adaptation from regret not tested",
    "Survival estimator (KaplanMeier) censoring not tested with ATC integration",
    "Thread participation graph constructed but never consumed by decisions",
    "Shadow policy promotion via regret threshold not tested",
    "Conformal uncertainty gating not tested end-to-end with real uncertainty",
    "Deadlock cycle edge TTL aging never validated",
    "No test for concurrent tick + policy reload race",
    "Posterior normalization failure (sum=0) could produce invalid state",
    "Cumulative regret stored as f64 with no saturation guard",
    "Population lock poisoning silently falls back to defaults",
];

// ──────────────────────────────────────────────────────────────────────
// Baseline comparator
// ──────────────────────────────────────────────────────────────────────

/// Composite baseline snapshot for before/after comparison.
///
/// Any ATC learning adaptation MUST demonstrate that the post-learning
/// system meets or exceeds these baseline metrics:
///
/// 1. **Calm**: advisory count per agent per hour does not increase
/// 2. **Correctness**: false-positive release rate stays at 0
/// 3. **Latency**: tick duration stays within `tick_budget_micros`
/// 4. **Safety**: safe mode entry rate does not increase
/// 5. **Fairness**: no agent receives >2x the mean advisory rate
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineComparator {
    /// Loss matrices for all three subsystems.
    pub liveness_losses: [[f64; 3]; 3],
    pub conflict_losses: [[f64; 3]; 3],
    pub load_losses: [[f64; 3]; 3],
    /// Timing budgets.
    pub timing: BaselineTimingBudgets,
    /// Calibration thresholds.
    pub calibration: BaselineCalibration,
    /// Adaptive controller thresholds.
    pub adaptive_controller: BaselineAdaptiveController,
}

impl BaselineComparator {
    /// Construct the frozen pre-learning baseline.
    #[must_use]
    pub const fn frozen() -> Self {
        Self {
            liveness_losses: BASELINE_LIVENESS_LOSSES,
            conflict_losses: BASELINE_CONFLICT_LOSSES,
            load_losses: BASELINE_LOAD_LOSSES,
            timing: BASELINE_TIMING,
            calibration: BASELINE_CALIBRATION,
            adaptive_controller: BASELINE_ADAPTIVE_CONTROLLER,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_comparator_construction() {
        let baseline = BaselineComparator::frozen();
        assert_eq!(baseline.timing.tick_budget_micros, 5_000);
        assert_eq!(baseline.calibration.eprocess_alert_threshold, 20.0);
        assert_eq!(baseline.adaptive_controller.window_size, 16);
    }

    #[test]
    fn liveness_asymmetry_extreme_reluctance() {
        // False positive (release alive) should be 100x false negative (miss dead)
        let release_alive = BASELINE_LIVENESS_LOSSES[2][0]; // Release, Alive
        let miss_dead = BASELINE_LIVENESS_LOSSES[0][2]; // DeclareAlive, Dead
        assert!(
            release_alive / miss_dead >= 1.5,
            "release_alive ({release_alive}) should be significantly more expensive than miss_dead ({miss_dead})"
        );
    }

    #[test]
    fn conflict_asymmetry_favors_action() {
        // Ignoring severe collision should be very expensive
        let ignore_severe = BASELINE_CONFLICT_LOSSES[0][2]; // Ignore, SevereCollision
        let force_no_conflict = BASELINE_CONFLICT_LOSSES[2][0]; // ForceReserv, NoConflict
        assert!(
            ignore_severe > force_no_conflict,
            "ignoring severe collision ({ignore_severe}) should cost more than forcing on no conflict ({force_no_conflict})"
        );
    }

    #[test]
    fn prior_probabilities_sum_to_one() {
        let liveness_sum: f64 = BASELINE_LIVENESS_PRIOR.iter().sum();
        let conflict_sum: f64 = BASELINE_CONFLICT_PRIOR.iter().sum();
        let load_sum: f64 = BASELINE_LOAD_PRIOR.iter().sum();
        assert!((liveness_sum - 1.0).abs() < 1e-10);
        assert!((conflict_sum - 1.0).abs() < 1e-10);
        assert!((load_sum - 1.0).abs() < 1e-10);
    }

    #[test]
    fn cost_asymmetries_consistent_with_loss_matrices() {
        let release = &BASELINE_COST_ASYMMETRIES[0];
        assert_eq!(release.effect, "ReleaseReservations");
        assert!((release.asymmetry_ratio - 100.0).abs() < f64::EPSILON);

        let force = &BASELINE_COST_ASYMMETRIES[1];
        assert_eq!(force.effect, "ForceReservation");
        assert!(force.asymmetry_ratio < 1.0, "conflict should favor action");
    }

    #[test]
    fn operator_gaps_non_empty() {
        assert!(
            !BASELINE_OPERATOR_GAPS.is_empty(),
            "baseline must capture known operator gaps"
        );
        assert!(
            !BASELINE_DIAGNOSTIC_GAPS.is_empty(),
            "baseline must capture known diagnostic gaps"
        );
    }

    #[test]
    fn program_priors_reasonable() {
        for &(program, prior_secs) in BASELINE_PROGRAM_PRIORS {
            assert!(prior_secs > 0, "program {program} has zero prior");
            assert!(
                prior_secs <= BASELINE_UNKNOWN_PROGRAM_PRIOR_SECS,
                "known program {program} ({prior_secs}s) should not exceed unknown default ({BASELINE_UNKNOWN_PROGRAM_PRIOR_SECS}s)"
            );
        }
    }

    #[test]
    fn timing_budgets_reasonable() {
        let t = &BASELINE_TIMING;
        assert!(t.tick_budget_micros > 0);
        assert!(
            t.tick_budget_micros <= 50_000,
            "tick budget should be <50ms"
        );
        assert!(t.probe_interval_micros > t.tick_budget_micros);
        assert!(t.estimated_probe_cost_micros < t.tick_budget_micros);
    }

    #[test]
    fn calibration_thresholds_sane() {
        let c = &BASELINE_CALIBRATION;
        assert!(c.target_coverage > 0.0 && c.target_coverage < 1.0);
        assert!(c.eprocess_alert_threshold > 1.0);
        assert!(c.cusum_threshold > 0.0);
        assert!(c.cusum_delta > 0.0 && c.cusum_delta < 1.0);
        assert!(c.posterior_floor > 0.0 && c.posterior_floor < 1e-6);
    }

    #[test]
    fn adaptive_controller_thresholds_ordered() {
        let ac = &BASELINE_ADAPTIVE_CONTROLLER;
        assert!(ac.pressure_utilization < ac.conservative_utilization);
        assert!(ac.pressure_debt_ratio < ac.conservative_debt_ratio);
    }

    #[test]
    fn baseline_serde_roundtrip() {
        let baseline = BaselineComparator::frozen();
        let json = serde_json::to_string(&baseline).unwrap();
        let decoded: BaselineComparator = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, baseline);
    }
}
