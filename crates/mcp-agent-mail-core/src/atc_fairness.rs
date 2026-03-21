#![allow(
    clippy::cast_precision_loss,
    clippy::struct_excessive_bools,
    clippy::doc_markdown
)]
//! Fairness, anti-starvation, and harm-budget contracts for ATC learning
//! (`br-0qt6e.3.11`).
//!
//! This module defines the fairness guardrails that keep the ATC learning
//! stack from concentrating burden on the same agents, projects, or cohorts
//! while still claiming aggregate utility gains.
//!
//! # Design Contract
//!
//! 1. **Track burden explicitly** across agents, projects, programs, or other
//!    cohorts instead of assuming global utility is sufficient.
//! 2. **Separate hard vetoes from soft penalties** so fairness can either block
//!    a candidate action/policy move or merely tax its utility score.
//! 3. **Anti-starvation is first-class**: targets that repeatedly signal need
//!    but do not receive attention become fairness blockers for unrelated work.
//! 4. **Regime changes discount history** rather than either forgetting it
//!    instantly or letting stale concentration poison the future forever.
//! 5. **Asymmetry is allowed only when justified** by safety, calibrated need,
//!    operator intent, or bounded cold-start exploration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{atc_admissibility::ActionTier, atc_regime::RegimeId, experience::EffectKind};

/// One fairness share point equals 1 basis point = 0.01%.
pub const FAIRNESS_BASIS_POINTS: u16 = 10_000;

/// Default history retention after a regime shift: halve prior burden.
pub const DEFAULT_REGIME_DISCOUNT_BP: u16 = 5_000;

/// Default anti-starvation window for one agent.
pub const AGENT_STARVATION_WINDOW_MICROS: i64 = 6 * 60 * 60 * 1_000_000;

/// Default anti-starvation window for one project.
pub const PROJECT_STARVATION_WINDOW_MICROS: i64 = 12 * 60 * 60 * 1_000_000;

/// Default anti-starvation window for one program or broad cohort.
pub const COHORT_STARVATION_WINDOW_MICROS: i64 = 24 * 60 * 60 * 1_000_000;

const BURDEN_UNIT: u32 = 100;

/// What population the fairness budget is protecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairnessScope {
    /// One named agent.
    Agent,
    /// One project/worktree.
    Project,
    /// One program/provider family.
    Program,
    /// Any other explicit tracked cohort.
    Cohort,
}

impl FairnessScope {
    /// Default anti-starvation window for this scope.
    #[must_use]
    pub const fn default_starvation_window_micros(self) -> i64 {
        match self {
            Self::Agent => AGENT_STARVATION_WINDOW_MICROS,
            Self::Project => PROJECT_STARVATION_WINDOW_MICROS,
            Self::Program | Self::Cohort => COHORT_STARVATION_WINDOW_MICROS,
        }
    }

    const fn scale_bp(self) -> u16 {
        match self {
            Self::Agent => 10_000,
            Self::Project => 15_000,
            Self::Program => 20_000,
            Self::Cohort => 25_000,
        }
    }

    const fn max_concentration_share_bp(self) -> u16 {
        match self {
            Self::Agent => 2_500,
            Self::Project => 3_500,
            Self::Program => 5_000,
            Self::Cohort => 6_500,
        }
    }
}

impl std::fmt::Display for FairnessScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent => write!(f, "agent"),
            Self::Project => write!(f, "project"),
            Self::Program => write!(f, "program"),
            Self::Cohort => write!(f, "cohort"),
        }
    }
}

/// Which burden or neglect observable is being tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairnessMetric {
    /// How often the target receives interventions at all.
    InterventionFrequency,
    /// Harm caused by false or unnecessary actions.
    FalseActionExposure,
    /// Harm caused by being repeatedly suppressed or throttled.
    SuppressionExposure,
    /// Outstanding unresolved burden or neglected work.
    UnresolvedBacklogBurden,
    /// Specific burden from liveness or utility probes.
    ProbeBurden,
    /// Notification, messaging, and operator-facing disruption burden.
    OperatorNoiseBurden,
}

impl FairnessMetric {
    const fn base_soft_limit_points(self) -> u32 {
        match self {
            Self::InterventionFrequency => 600,
            Self::FalseActionExposure | Self::ProbeBurden => 250,
            Self::SuppressionExposure | Self::OperatorNoiseBurden => 300,
            Self::UnresolvedBacklogBurden => 900,
        }
    }

    const fn base_hard_limit_points(self) -> u32 {
        match self {
            Self::InterventionFrequency => 900,
            Self::FalseActionExposure => 400,
            Self::SuppressionExposure | Self::OperatorNoiseBurden => 450,
            Self::UnresolvedBacklogBurden => 1_400,
            Self::ProbeBurden => 350,
        }
    }
}

impl std::fmt::Display for FairnessMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InterventionFrequency => write!(f, "intervention_frequency"),
            Self::FalseActionExposure => write!(f, "false_action_exposure"),
            Self::SuppressionExposure => write!(f, "suppression_exposure"),
            Self::UnresolvedBacklogBurden => write!(f, "unresolved_backlog_burden"),
            Self::ProbeBurden => write!(f, "probe_burden"),
            Self::OperatorNoiseBurden => write!(f, "operator_noise_burden"),
        }
    }
}

/// One protected fairness target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FairnessTarget {
    /// What kind of population this target belongs to.
    pub scope: FairnessScope,
    /// Stable identifier inside that scope.
    pub id: String,
}

impl FairnessTarget {
    /// Create a new fairness target.
    #[must_use]
    pub fn new(scope: FairnessScope, id: impl Into<String>) -> Self {
        Self {
            scope,
            id: id.into(),
        }
    }
}

/// Explicit fairness budget and concentration policy for one
/// `(scope, metric)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FairnessBudget {
    /// Protected population scope.
    pub scope: FairnessScope,
    /// Observable protected by this budget.
    pub metric: FairnessMetric,
    /// Soft budget. Above this threshold, fairness debt accumulates and
    /// utility is penalized.
    pub soft_limit_points: u32,
    /// Hard budget. Above this threshold, the candidate should be vetoed.
    pub hard_limit_points: u32,
    /// Maximum share of the total burden that any one target may absorb before
    /// a hard concentration veto triggers once multiple targets carry burden.
    pub max_concentration_share_bp: u16,
    /// How long a target may keep signaling need without attention before it
    /// becomes starved.
    pub starvation_window_micros: Option<i64>,
    /// How much burden history survives a regime shift.
    pub regime_discount_bp: u16,
}

impl FairnessBudget {
    /// Default budget for the given scope and metric.
    #[must_use]
    pub const fn default_for(scope: FairnessScope, metric: FairnessMetric) -> Self {
        let scale_bp = scope.scale_bp() as u32;
        Self {
            scope,
            metric,
            soft_limit_points: metric.base_soft_limit_points().saturating_mul(scale_bp)
                / (FAIRNESS_BASIS_POINTS as u32),
            hard_limit_points: metric.base_hard_limit_points().saturating_mul(scale_bp)
                / (FAIRNESS_BASIS_POINTS as u32),
            max_concentration_share_bp: scope.max_concentration_share_bp(),
            starvation_window_micros: Some(scope.default_starvation_window_micros()),
            regime_discount_bp: DEFAULT_REGIME_DISCOUNT_BP,
        }
    }
}

/// One fairness-relevant event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FairnessEvent {
    /// Target receiving or failing to receive attention.
    pub target: FairnessTarget,
    /// What kind of fairness signal this event carries.
    pub kind: FairnessEventKind,
    /// When the event occurred.
    pub observed_ts_micros: i64,
}

/// Event categories that fairness accounting understands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairnessEventKind {
    /// A concrete intervention was applied.
    Intervention { effect_kind: EffectKind },
    /// A false or harmful action was observed.
    FalseAction { effect_kind: EffectKind },
    /// Work was suppressed or throttled rather than served.
    Suppression { effect_kind: EffectKind },
    /// Backlog or neglected work was detected.
    UnresolvedBacklog { open_items: u16 },
    /// A probe was sent.
    Probe,
    /// Operator/user-facing noise burden was emitted.
    OperatorNoise { units: u16 },
    /// Need was detected but no burden points are directly added.
    NeedDetected,
}

impl FairnessEventKind {
    #[must_use]
    pub const fn records_action(&self) -> bool {
        matches!(
            self,
            Self::Intervention { .. } | Self::FalseAction { .. } | Self::Probe
        )
    }

    #[must_use]
    pub const fn records_need(&self) -> bool {
        matches!(
            self,
            Self::Suppression { .. } | Self::UnresolvedBacklog { .. } | Self::NeedDetected
        )
    }

    /// Convert this event into metric-specific burden points.
    #[must_use]
    pub const fn burden_points(self, metric: FairnessMetric) -> u32 {
        match (self, metric) {
            (Self::Intervention { effect_kind }, FairnessMetric::InterventionFrequency) => {
                intervention_points(effect_kind)
            }
            (Self::FalseAction { effect_kind }, FairnessMetric::FalseActionExposure) => {
                false_action_points(effect_kind)
            }
            (Self::Suppression { effect_kind }, FairnessMetric::SuppressionExposure) => {
                suppression_points(effect_kind)
            }
            (Self::UnresolvedBacklog { open_items }, FairnessMetric::UnresolvedBacklogBurden) => {
                (open_items as u32).saturating_mul(BURDEN_UNIT)
            }
            (Self::Probe, FairnessMetric::ProbeBurden) => BURDEN_UNIT,
            (Self::OperatorNoise { units }, FairnessMetric::OperatorNoiseBurden) => {
                (units as u32).saturating_mul(BURDEN_UNIT)
            }
            _ => 0,
        }
    }
}

/// Mutable fairness state for one target within one `(scope, metric)` view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFairnessState {
    /// Cumulative decayed burden points.
    pub burden_points: u64,
    /// Total number of recorded events for this target.
    pub event_count: u64,
    /// Most recent event of any fairness-relevant kind.
    pub last_event_ts_micros: i64,
    /// Most recent time this target signaled a need for attention.
    pub last_need_ts_micros: Option<i64>,
    /// Most recent time this target actually received attention.
    pub last_action_ts_micros: Option<i64>,
}

impl TargetFairnessState {
    const fn new(now_micros: i64) -> Self {
        Self {
            burden_points: 0,
            event_count: 0,
            last_event_ts_micros: now_micros,
            last_need_ts_micros: None,
            last_action_ts_micros: None,
        }
    }
}

/// One scope/metric snapshot used by action-time guards and later summary
/// surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FairnessSnapshot {
    /// Protected scope for this snapshot.
    pub scope: FairnessScope,
    /// Metric represented by this snapshot.
    pub metric: FairnessMetric,
    /// Active regime this snapshot is calibrated against.
    pub active_regime_id: RegimeId,
    /// Total decayed burden across all known targets.
    pub total_burden_points: u64,
    /// Per-target burden state.
    pub targets: BTreeMap<String, TargetFairnessState>,
}

impl FairnessSnapshot {
    /// Create an empty snapshot.
    #[must_use]
    pub const fn new(
        scope: FairnessScope,
        metric: FairnessMetric,
        active_regime_id: RegimeId,
    ) -> Self {
        Self {
            scope,
            metric,
            active_regime_id,
            total_burden_points: 0,
            targets: BTreeMap::new(),
        }
    }

    /// Record one event into this snapshot. Events with a mismatched scope are
    /// ignored by design so callers can fan them out safely.
    pub fn record(&mut self, event: FairnessEvent) -> u32 {
        if event.target.scope != self.scope {
            return 0;
        }
        let now = event.observed_ts_micros;
        let points = event.kind.clone().burden_points(self.metric);
        let entry = self
            .targets
            .entry(event.target.id)
            .or_insert_with(|| TargetFairnessState::new(now));
        entry.last_event_ts_micros = now;
        entry.event_count = entry.event_count.saturating_add(1);
        if event.kind.records_need() {
            entry.last_need_ts_micros = Some(now);
        }
        if event.kind.records_action() {
            entry.last_action_ts_micros = Some(now);
        }
        entry.burden_points = entry.burden_points.saturating_add(u64::from(points));
        self.total_burden_points = self.total_burden_points.saturating_add(u64::from(points));
        points
    }

    /// Discount history across a confirmed regime shift.
    pub fn apply_regime_shift(&mut self, new_regime_id: RegimeId, discount_bp: u16) {
        if new_regime_id == self.active_regime_id {
            return;
        }
        let discount = u64::from(discount_bp);
        self.total_burden_points = 0;
        for state in self.targets.values_mut() {
            state.burden_points = state
                .burden_points
                .saturating_mul(discount)
                .saturating_div(u64::from(FAIRNESS_BASIS_POINTS));
            self.total_burden_points = self.total_burden_points.saturating_add(state.burden_points);
        }
        self.active_regime_id = new_regime_id;
    }

    /// Rank the currently most impacted targets by burden concentration.
    #[must_use]
    pub fn most_impacted_targets(
        &self,
        limit: usize,
        budget: &FairnessBudget,
    ) -> Vec<ImpactedTarget> {
        let mut impacted: Vec<_> = self
            .targets
            .iter()
            .map(|(id, state)| ImpactedTarget {
                target: FairnessTarget::new(self.scope, id.clone()),
                burden_points: state.burden_points,
                concentration_share_bp: share_bp(state.burden_points, self.total_burden_points),
                fairness_debt_points: state
                    .burden_points
                    .saturating_sub(u64::from(budget.soft_limit_points)),
            })
            .collect();
        impacted.sort_by(|left, right| {
            right
                .burden_points
                .cmp(&left.burden_points)
                .then_with(|| {
                    right
                        .concentration_share_bp
                        .cmp(&left.concentration_share_bp)
                })
                .then_with(|| left.target.id.cmp(&right.target.id))
        });
        impacted.truncate(limit);
        impacted
    }

    /// Find targets that have kept signaling need without receiving attention
    /// inside the allowed starvation window.
    #[must_use]
    pub fn starved_targets(&self, budget: &FairnessBudget, now_micros: i64) -> Vec<StarvedTarget> {
        let Some(window) = budget.starvation_window_micros else {
            return Vec::new();
        };

        let mut starved = Vec::new();
        for (id, state) in &self.targets {
            let Some(last_need) = state.last_need_ts_micros else {
                continue;
            };
            let acted_since_need = state
                .last_action_ts_micros
                .is_some_and(|last_action| last_action >= last_need);
            if acted_since_need {
                continue;
            }
            let waited = now_micros.saturating_sub(last_need);
            if waited >= window {
                starved.push(StarvedTarget {
                    target: FairnessTarget::new(self.scope, id.clone()),
                    waited_micros: waited,
                    last_need_ts_micros: last_need,
                });
            }
        }
        starved.sort_by(|left, right| {
            right
                .waited_micros
                .cmp(&left.waited_micros)
                .then_with(|| left.target.id.cmp(&right.target.id))
        });
        starved
    }

    /// Assess whether assigning more burden to `target` is fair enough to
    /// allow, merely penalize, or veto.
    #[must_use]
    pub fn assess_candidate(
        &self,
        budget: &FairnessBudget,
        target: &FairnessTarget,
        additional_burden_points: u32,
        raw_expected_utility_bp: i32,
        now_micros: i64,
    ) -> FairnessAssessment {
        if budget.scope != self.scope || budget.metric != self.metric || target.scope != self.scope
        {
            return FairnessAssessment {
                disposition: FairnessDisposition::Veto,
                violations: vec![FairnessViolationCode::SnapshotContractMismatch],
                projected_burden_points: 0,
                projected_concentration_share_bp: 0,
                fairness_debt_points: 0,
                most_impacted_targets: Vec::new(),
                starved_targets: Vec::new(),
                tradeoff: UtilityTradeoff {
                    raw_expected_utility_bp,
                    fairness_penalty_bp: 0,
                    net_expected_utility_bp: raw_expected_utility_bp,
                    winner: TradeoffWinner::FairnessHardGuard,
                    vetoed: true,
                },
            };
        }

        let current = self.targets.get(&target.id);
        let current_points = current.map_or(0, |state| state.burden_points);
        let projected_burden_points =
            current_points.saturating_add(u64::from(additional_burden_points));
        let projected_total = self
            .total_burden_points
            .saturating_add(u64::from(additional_burden_points));
        let projected_concentration_share_bp = share_bp(projected_burden_points, projected_total);
        let populated_targets = self
            .targets
            .values()
            .filter(|state| state.burden_points > 0)
            .count();
        let target_has_burden = current.is_some_and(|state| state.burden_points > 0);
        let projected_populated_targets = if additional_burden_points == 0 || target_has_burden {
            populated_targets
        } else {
            populated_targets.saturating_add(1)
        };
        let concentration_applicable = projected_populated_targets >= 2;
        let fairness_debt_points =
            projected_burden_points.saturating_sub(u64::from(budget.soft_limit_points));

        let starved_targets = self.starved_targets(budget, now_micros);
        let target_is_starved = starved_targets.iter().any(|entry| entry.target == *target);
        let starved_elsewhere = starved_targets.iter().any(|entry| entry.target != *target);

        let mut violations = Vec::new();
        if projected_burden_points > u64::from(budget.hard_limit_points) {
            violations.push(FairnessViolationCode::HardBudgetExceeded);
        } else if fairness_debt_points > 0 {
            violations.push(FairnessViolationCode::SoftBudgetExceeded);
        }
        if concentration_applicable
            && projected_concentration_share_bp > budget.max_concentration_share_bp
        {
            violations.push(FairnessViolationCode::ConcentrationExceeded);
        }
        if target_is_starved {
            violations.push(FairnessViolationCode::TargetWasStarved);
        }
        if starved_elsewhere {
            violations.push(FairnessViolationCode::OtherTargetStarved);
        }

        let vetoed = violations.iter().any(|code| {
            matches!(
                code,
                FairnessViolationCode::HardBudgetExceeded
                    | FairnessViolationCode::ConcentrationExceeded
                    | FairnessViolationCode::OtherTargetStarved
            )
        });

        let fairness_penalty_bp = if vetoed {
            saturating_i32_from_u64(fairness_debt_points.max(u64::from(additional_burden_points)))
        } else {
            saturating_i32_from_u64(fairness_debt_points)
        };
        let net_expected_utility_bp = raw_expected_utility_bp.saturating_sub(fairness_penalty_bp);
        let winner = if vetoed {
            TradeoffWinner::FairnessHardGuard
        } else if fairness_penalty_bp > 0 {
            TradeoffWinner::FairnessSoftGuard
        } else {
            TradeoffWinner::Utility
        };
        let disposition = if vetoed {
            FairnessDisposition::Veto
        } else if fairness_penalty_bp > 0 {
            FairnessDisposition::Warn
        } else {
            FairnessDisposition::Allow
        };

        FairnessAssessment {
            disposition,
            violations,
            projected_burden_points,
            projected_concentration_share_bp,
            fairness_debt_points,
            most_impacted_targets: self.most_impacted_targets(3, budget),
            starved_targets,
            tradeoff: UtilityTradeoff {
                raw_expected_utility_bp,
                fairness_penalty_bp,
                net_expected_utility_bp,
                winner,
                vetoed,
            },
        }
    }
}

/// Result of a fairness evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairnessDisposition {
    /// Fairness allows the candidate unchanged.
    Allow,
    /// Fairness allows the candidate but requires an explicit warning and
    /// utility penalty.
    Warn,
    /// Fairness vetoes the candidate.
    Veto,
}

/// Machine-readable reasons for fairness penalties or vetoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairnessViolationCode {
    /// Caller mixed incompatible snapshot/budget/target dimensions.
    SnapshotContractMismatch,
    /// Burden crossed the soft budget.
    SoftBudgetExceeded,
    /// Burden crossed the hard budget.
    HardBudgetExceeded,
    /// One target is carrying too much of the total burden.
    ConcentrationExceeded,
    /// The candidate target itself had been starved and is now receiving
    /// overdue attention.
    TargetWasStarved,
    /// Another target is starved, so working elsewhere should be blocked.
    OtherTargetStarved,
}

/// Machine-readable tradeoff summary for fairness vs. utility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtilityTradeoff {
    /// Utility before any fairness adjustment.
    pub raw_expected_utility_bp: i32,
    /// Penalty applied for fairness debt.
    pub fairness_penalty_bp: i32,
    /// Utility remaining after the fairness penalty.
    pub net_expected_utility_bp: i32,
    /// Which side won the conflict.
    pub winner: TradeoffWinner,
    /// Whether the candidate was vetoed.
    pub vetoed: bool,
}

/// Which side won the utility-vs-fairness conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeoffWinner {
    /// Raw expected utility remains acceptable.
    Utility,
    /// Fairness did not veto, but taxed the utility score.
    FairnessSoftGuard,
    /// Fairness hard-blocked the candidate.
    FairnessHardGuard,
}

/// One impacted target visible in summaries or audit snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactedTarget {
    /// Which target is impacted.
    pub target: FairnessTarget,
    /// Absolute decayed burden points.
    pub burden_points: u64,
    /// Share of total burden in basis points.
    pub concentration_share_bp: u16,
    /// Debt above the soft budget.
    pub fairness_debt_points: u64,
}

/// One target currently suffering starvation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarvedTarget {
    /// The neglected target.
    pub target: FairnessTarget,
    /// How long it has waited since signaling need.
    pub waited_micros: i64,
    /// Timestamp of the last unresolved need signal.
    pub last_need_ts_micros: i64,
}

/// Full fairness assessment returned to controllers or summary surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FairnessAssessment {
    /// High-level decision.
    pub disposition: FairnessDisposition,
    /// All triggered machine-readable reasons.
    pub violations: Vec<FairnessViolationCode>,
    /// Projected burden for the target after the candidate.
    pub projected_burden_points: u64,
    /// Projected concentration share after the candidate.
    pub projected_concentration_share_bp: u16,
    /// Debt above the soft budget.
    pub fairness_debt_points: u64,
    /// Current most-impacted targets for operator visibility.
    pub most_impacted_targets: Vec<ImpactedTarget>,
    /// Current starved targets.
    pub starved_targets: Vec<StarvedTarget>,
    /// Utility-vs-fairness resolution.
    pub tradeoff: UtilityTradeoff,
}

/// Explicit categories of asymmetry that may be justified despite fairness
/// pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptableAsymmetryKind {
    /// More burden is justified because the target has materially greater need.
    NeedProportionalPriority,
    /// A dangerous target may deserve temporarily concentrated intervention.
    SafetyCriticalContainment,
    /// A human explicitly chose uneven treatment.
    OperatorDirected,
    /// Temporary extra coverage is acceptable to break starvation or cold-start.
    ColdStartCoverageBoost,
    /// A fresh regime may temporarily require uneven stabilization work.
    RegimeRecoveryStabilization,
}

/// Human/audit-facing justification contract for one allowed asymmetry class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcceptableAsymmetryRule {
    /// Category of asymmetry.
    pub kind: AcceptableAsymmetryKind,
    /// What evidence must exist before the asymmetry is accepted.
    pub evidence_required: &'static str,
    /// Optional time bound on how long the asymmetry may persist.
    pub max_window_micros: Option<i64>,
    /// What makes the asymmetry acceptable rather than suspicious.
    pub explanation: &'static str,
}

/// Canonical, auditable forms of unequal treatment that the fairness contract
/// allows.
pub const ACCEPTABLE_ASYMMETRY_RULES: &[AcceptableAsymmetryRule] = &[
    AcceptableAsymmetryRule {
        kind: AcceptableAsymmetryKind::NeedProportionalPriority,
        evidence_required: "calibrated evidence that the target carries materially larger unresolved need or risk",
        max_window_micros: Some(PROJECT_STARVATION_WINDOW_MICROS),
        explanation: "Need can justify temporarily higher attention, but the evidence and duration must stay explicit.",
    },
    AcceptableAsymmetryRule {
        kind: AcceptableAsymmetryKind::SafetyCriticalContainment,
        evidence_required: "safety evidence showing broader harm if the target is not prioritized",
        max_window_micros: Some(AGENT_STARVATION_WINDOW_MICROS),
        explanation: "Containment is acceptable when failing to concentrate effort would create larger systemic harm.",
    },
    AcceptableAsymmetryRule {
        kind: AcceptableAsymmetryKind::OperatorDirected,
        evidence_required: "explicit operator instruction or override recorded in audit surfaces",
        max_window_micros: Some(PROJECT_STARVATION_WINDOW_MICROS),
        explanation: "Human-directed asymmetry is acceptable only when traceable and time-bounded.",
    },
    AcceptableAsymmetryRule {
        kind: AcceptableAsymmetryKind::ColdStartCoverageBoost,
        evidence_required: "cold-start or starvation evidence showing the target lacks enough observations for safe estimation",
        max_window_micros: Some(AGENT_STARVATION_WINDOW_MICROS),
        explanation: "Temporary extra coverage is acceptable when it repairs neglect rather than entrenching it.",
    },
    AcceptableAsymmetryRule {
        kind: AcceptableAsymmetryKind::RegimeRecoveryStabilization,
        evidence_required: "confirmed regime change plus replay or summary evidence that the target is central to the new regime",
        max_window_micros: Some(COHORT_STARVATION_WINDOW_MICROS),
        explanation: "Uneven treatment is acceptable during regime recovery only while the new regime is still stabilizing.",
    },
];

const fn intervention_points(effect_kind: EffectKind) -> u32 {
    match effect_kind {
        EffectKind::NoAction => 0,
        EffectKind::Advisory
        | EffectKind::Probe
        | EffectKind::RoutingSuggestion
        | EffectKind::Backpressure
        | EffectKind::Release
        | EffectKind::ForceReservation => BURDEN_UNIT,
    }
}

const fn false_action_points(effect_kind: EffectKind) -> u32 {
    match ActionTier::from_effect_kind(effect_kind) {
        ActionTier::LowRisk => BURDEN_UNIT,
        ActionTier::MediumRisk => BURDEN_UNIT * 2,
        ActionTier::HighRisk => BURDEN_UNIT * 4,
    }
}

const fn suppression_points(effect_kind: EffectKind) -> u32 {
    match ActionTier::from_effect_kind(effect_kind) {
        ActionTier::LowRisk => BURDEN_UNIT,
        ActionTier::MediumRisk => BURDEN_UNIT + 50,
        ActionTier::HighRisk => BURDEN_UNIT + 150,
    }
}

fn share_bp(numerator: u64, denominator: u64) -> u16 {
    if numerator == 0 || denominator == 0 {
        return 0;
    }
    // Use u128 intermediate to avoid saturation at u64::MAX which would
    // produce incorrect share values (e.g., u64::MAX / denominator >> 10000).
    let share_128 =
        u128::from(numerator) * u128::from(FAIRNESS_BASIS_POINTS) / u128::from(denominator);
    // Cap at FAIRNESS_BASIS_POINTS (10000 = 100%) since shares cannot exceed 100%.
    u16::try_from(share_128.min(u128::from(FAIRNESS_BASIS_POINTS))).unwrap_or(FAIRNESS_BASIS_POINTS)
}

fn saturating_i32_from_u64(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_target(id: &str) -> FairnessTarget {
        FairnessTarget::new(FairnessScope::Agent, id)
    }

    fn project_target(id: &str) -> FairnessTarget {
        FairnessTarget::new(FairnessScope::Project, id)
    }

    #[test]
    fn default_budgets_scale_by_scope() {
        let agent =
            FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::FalseActionExposure);
        let project = FairnessBudget::default_for(
            FairnessScope::Project,
            FairnessMetric::FalseActionExposure,
        );
        assert!(project.soft_limit_points > agent.soft_limit_points);
        assert!(project.hard_limit_points > agent.hard_limit_points);
        assert!(project.max_concentration_share_bp > agent.max_concentration_share_bp);
    }

    #[test]
    fn false_action_points_scale_with_risk_tier() {
        assert!(false_action_points(EffectKind::Probe) > false_action_points(EffectKind::Advisory));
        assert!(false_action_points(EffectKind::Release) > false_action_points(EffectKind::Probe));
    }

    #[test]
    fn snapshot_records_points_and_actions() {
        let mut snapshot = FairnessSnapshot::new(
            FairnessScope::Agent,
            FairnessMetric::InterventionFrequency,
            0,
        );
        let recorded = snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::Intervention {
                effect_kind: EffectKind::Advisory,
            },
            observed_ts_micros: 10,
        });
        assert_eq!(recorded, BURDEN_UNIT);
        let state = snapshot.targets.get("agent-a").expect("target state");
        assert_eq!(state.burden_points, u64::from(BURDEN_UNIT));
        assert_eq!(state.last_action_ts_micros, Some(10));
        assert_eq!(snapshot.total_burden_points, u64::from(BURDEN_UNIT));
    }

    #[test]
    fn regime_shift_discounts_existing_burden() {
        let mut snapshot =
            FairnessSnapshot::new(FairnessScope::Agent, FairnessMetric::ProbeBurden, 0);
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::Probe,
            observed_ts_micros: 10,
        });
        snapshot.apply_regime_shift(1, DEFAULT_REGIME_DISCOUNT_BP);
        let state = snapshot.targets.get("agent-a").expect("target state");
        assert_eq!(snapshot.active_regime_id, 1);
        assert_eq!(state.burden_points, u64::from(BURDEN_UNIT / 2));
        assert_eq!(snapshot.total_burden_points, u64::from(BURDEN_UNIT / 2));
    }

    #[test]
    fn starved_target_blocks_unrelated_work() {
        let budget = FairnessBudget::default_for(
            FairnessScope::Agent,
            FairnessMetric::UnresolvedBacklogBurden,
        );
        let mut snapshot = FairnessSnapshot::new(
            FairnessScope::Agent,
            FairnessMetric::UnresolvedBacklogBurden,
            0,
        );
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::NeedDetected,
            observed_ts_micros: 0,
        });

        let assessment = snapshot.assess_candidate(
            &budget,
            &agent_target("agent-b"),
            BURDEN_UNIT,
            150,
            AGENT_STARVATION_WINDOW_MICROS + 1,
        );
        assert_eq!(assessment.disposition, FairnessDisposition::Veto);
        assert!(
            assessment
                .violations
                .contains(&FairnessViolationCode::OtherTargetStarved)
        );
    }

    #[test]
    fn acting_on_starved_target_is_not_blocked_by_other_starvation() {
        let budget = FairnessBudget::default_for(
            FairnessScope::Agent,
            FairnessMetric::InterventionFrequency,
        );
        let mut snapshot = FairnessSnapshot::new(
            FairnessScope::Agent,
            FairnessMetric::InterventionFrequency,
            0,
        );
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::NeedDetected,
            observed_ts_micros: 0,
        });

        let assessment = snapshot.assess_candidate(
            &budget,
            &agent_target("agent-a"),
            BURDEN_UNIT,
            80,
            AGENT_STARVATION_WINDOW_MICROS + 1,
        );
        assert_ne!(assessment.disposition, FairnessDisposition::Veto);
        assert!(
            assessment
                .violations
                .contains(&FairnessViolationCode::TargetWasStarved)
        );
    }

    #[test]
    fn concentration_cap_vetoes_dominant_target() {
        let budget = FairnessBudget {
            max_concentration_share_bp: 2_000,
            ..FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::OperatorNoiseBurden)
        };
        let mut snapshot =
            FairnessSnapshot::new(FairnessScope::Agent, FairnessMetric::OperatorNoiseBurden, 0);
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::OperatorNoise { units: 2 },
            observed_ts_micros: 1,
        });
        snapshot.record(FairnessEvent {
            target: agent_target("agent-b"),
            kind: FairnessEventKind::OperatorNoise { units: 1 },
            observed_ts_micros: 2,
        });

        let assessment =
            snapshot.assess_candidate(&budget, &agent_target("agent-a"), BURDEN_UNIT, 120, 3);
        assert_eq!(assessment.disposition, FairnessDisposition::Veto);
        assert!(
            assessment
                .violations
                .contains(&FairnessViolationCode::ConcentrationExceeded)
        );
    }

    #[test]
    fn soft_budget_penalizes_but_does_not_veto() {
        let budget = FairnessBudget {
            soft_limit_points: 100,
            hard_limit_points: 500,
            ..FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::FalseActionExposure)
        };
        let mut snapshot =
            FairnessSnapshot::new(FairnessScope::Agent, FairnessMetric::FalseActionExposure, 0);
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::FalseAction {
                effect_kind: EffectKind::Advisory,
            },
            observed_ts_micros: 1,
        });

        let assessment =
            snapshot.assess_candidate(&budget, &agent_target("agent-a"), BURDEN_UNIT, 300, 2);
        assert_eq!(assessment.disposition, FairnessDisposition::Warn);
        assert_eq!(
            assessment.tradeoff.winner,
            TradeoffWinner::FairnessSoftGuard
        );
        assert!(assessment.tradeoff.fairness_penalty_bp > 0);
        assert!(assessment.tradeoff.net_expected_utility_bp < 300);
    }

    #[test]
    fn single_target_histories_do_not_trip_concentration_cap() {
        let budget = FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::ProbeBurden);
        let mut snapshot =
            FairnessSnapshot::new(FairnessScope::Agent, FairnessMetric::ProbeBurden, 0);
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::Probe,
            observed_ts_micros: 1,
        });

        let assessment =
            snapshot.assess_candidate(&budget, &agent_target("agent-a"), BURDEN_UNIT, 150, 2);
        assert_eq!(assessment.disposition, FairnessDisposition::Allow);
        assert!(
            !assessment
                .violations
                .contains(&FairnessViolationCode::ConcentrationExceeded)
        );
    }

    #[test]
    fn most_impacted_targets_sort_by_burden() {
        let budget = FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::ProbeBurden);
        let mut snapshot =
            FairnessSnapshot::new(FairnessScope::Agent, FairnessMetric::ProbeBurden, 0);
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::Probe,
            observed_ts_micros: 1,
        });
        snapshot.record(FairnessEvent {
            target: agent_target("agent-a"),
            kind: FairnessEventKind::Probe,
            observed_ts_micros: 2,
        });
        snapshot.record(FairnessEvent {
            target: agent_target("agent-b"),
            kind: FairnessEventKind::Probe,
            observed_ts_micros: 3,
        });

        let impacted = snapshot.most_impacted_targets(2, &budget);
        assert_eq!(impacted.len(), 2);
        assert_eq!(impacted[0].target.id, "agent-a");
        assert!(impacted[0].burden_points > impacted[1].burden_points);
    }

    #[test]
    fn acceptable_asymmetries_are_explicit_and_bounded() {
        assert!(!ACCEPTABLE_ASYMMETRY_RULES.is_empty());
        assert!(
            ACCEPTABLE_ASYMMETRY_RULES
                .iter()
                .all(|rule| !rule.evidence_required.is_empty() && !rule.explanation.is_empty())
        );
        assert!(
            ACCEPTABLE_ASYMMETRY_RULES
                .iter()
                .any(|rule| rule.max_window_micros.is_some())
        );
    }

    #[test]
    fn contract_mismatch_vetoes_candidate() {
        let snapshot = FairnessSnapshot::new(FairnessScope::Agent, FairnessMetric::ProbeBurden, 0);
        let budget = FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::ProbeBurden);

        let target_scope_mismatch =
            snapshot.assess_candidate(&budget, &project_target("agent-a"), BURDEN_UNIT, 100, 1);
        assert_eq!(target_scope_mismatch.disposition, FairnessDisposition::Veto);
        assert!(
            target_scope_mismatch
                .violations
                .contains(&FairnessViolationCode::SnapshotContractMismatch)
        );

        let budget_metric_mismatch = snapshot.assess_candidate(
            &FairnessBudget::default_for(FairnessScope::Agent, FairnessMetric::FalseActionExposure),
            &agent_target("agent-a"),
            BURDEN_UNIT,
            100,
            1,
        );
        assert_eq!(
            budget_metric_mismatch.disposition,
            FairnessDisposition::Veto
        );
        assert!(
            budget_metric_mismatch
                .violations
                .contains(&FairnessViolationCode::SnapshotContractMismatch)
        );
    }
}
