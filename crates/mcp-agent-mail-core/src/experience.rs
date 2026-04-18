//! Canonical experience tuple, lifecycle, and identifiers for ATC learning.
//!
//! This module defines the durable experience row that records what ATC did,
//! why, what happened next, and how future policy should change. It is the
//! foundational data-model contract for the ATC learning epic (br-0qt6e).
//!
//! # Identifier Model
//!
//! Each identifier serves a distinct correlation purpose:
//!
//! | ID               | Scope                        | Format            | Assigned by      |
//! |------------------|------------------------------|-------------------|------------------|
//! | `experience_id`  | One experience row           | `exp-{u64}`       | Experience store |
//! | `decision_id`    | One ATC decision             | `dec-{u64}`       | Evidence ledger  |
//! | `effect_id`      | One effect plan              | `eff-{u64}`       | Effect planner   |
//! | `trace_id`       | One causal chain             | `trc-{fnv64}`     | Decision builder |
//! | `claim_id`       | One artifact-graph assertion | `clm-{u64}`       | Decision builder |
//! | `evidence_id`    | One evidence snapshot        | `evi-{u64}`       | Decision builder |
//!
//! A single decision can produce multiple effects. Each effect becomes its own
//! experience row, sharing the same `decision_id` and `trace_id` but having
//! distinct `experience_id` and `effect_id` values. This is the
//! one-decision-to-many-effect rule.
//!
//! # Lifecycle State Machine
//!
//! ```text
//!                           ┌─────────────────────────────────────────────┐
//!                           │                                             │
//!   ┌─────────┐    ┌───────┴───┐    ┌───────────┐    ┌────────┐         │
//!   │ Planned  │───►│Dispatched │───►│ Executed  │───►│  Open  │──┬──►Resolved
//!   └─────────┘    └─────┬─────┘    └───────────┘    └────────┘  │
//!                        │                                        ├──►Censored
//!                        ├──────────►Failed                       │
//!                        ├──────────►Throttled                    └──►Expired
//!                        ├──────────►Suppressed
//!                        └──────────►Skipped
//! ```
//!
//! ## State Definitions
//!
//! | State        | Meaning                                                    |
//! |--------------|------------------------------------------------------------|
//! | `Planned`    | Decision made, effect planned, not yet dispatched          |
//! | `Dispatched` | Effect handed to executor, outcome unknown                 |
//! | `Executed`   | Effect ran to completion (success or not)                  |
//! | `Failed`     | Effect dispatch or execution failed (infra/runtime error)  |
//! | `Throttled`  | Effect suppressed by rate-limit or budget constraint       |
//! | `Suppressed` | Effect blocked by safety gate (risk too high)              |
//! | `Skipped`    | Deliberate no-action: ATC decided inaction was optimal     |
//! | `Open`       | Executed but outcome not yet observed (delayed attribution)|
//! | `Resolved`   | Outcome observed and attributed to this experience         |
//! | `Censored`   | Outcome unobservable (agent departed, project closed, etc) |
//! | `Expired`    | Resolution window elapsed without observation              |
//!
//! ## Transition Rules
//!
//! - `Planned` is the initial state for all experience rows.
//! - `Planned → Dispatched`: Effect sent to executor.
//! - `Dispatched → Executed | Failed | Throttled | Suppressed | Skipped`:
//!   Terminal dispatch outcomes. These are set synchronously at dispatch time.
//! - `Executed → Open`: When the effect completed but the outcome is delayed.
//! - `Open → Resolved | Censored | Expired`: Terminal resolution states.
//!   Resolution is monotone: once terminal, the state never changes.
//! - `Skipped` is NOT a failure. It represents deliberate inaction where ATC
//!   determined that doing nothing was the best policy. Learning from skipped
//!   experiences is critical for validating conservative strategies.
//!
//! ## Idempotent Resolution
//!
//! Resolution transitions are idempotent: resolving an already-resolved
//! experience returns success without mutation. This guarantees that
//! duplicate outcome events (from retries, replays, or concurrent observers)
//! never corrupt the experience store.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ──────────────────────────────────────────────────────────────────────
// Identifiers
// ──────────────────────────────────────────────────────────────────────

/// Unique identifier for an experience row.
///
/// Assigned by the experience store on insertion. Monotonically increasing
/// within a single process. Format: `exp-{u64}`.
pub type ExperienceId = u64;

/// Unique identifier for a decision.
///
/// Assigned by the evidence ledger when the decision is recorded.
/// One decision can produce many experience rows (one per effect).
/// Format: `dec-{u64}`.
pub type DecisionId = u64;

/// Unique identifier for a planned effect.
///
/// Assigned by the effect planner. Each effect maps 1:1 to an experience row.
/// Format: `eff-{u64}`.
pub type EffectId = u64;

/// Stable causal-chain identifier.
///
/// Computed as a stable FNV-1a 64-bit hash over the decision context.
/// All effects from the same causal chain share the same `trace_id`.
/// Format: `trc-{hex}`.
pub type TraceId = String;

/// Artifact-graph claim identifier.
///
/// Links the experience to its originating claim in the artifact graph.
/// Format: `clm-{u64}`.
pub type ClaimId = String;

/// Artifact-graph evidence identifier.
///
/// Links the experience to the evidence snapshot that drove the decision.
/// Format: `evi-{u64}`.
pub type EvidenceIdStr = String;

// ──────────────────────────────────────────────────────────────────────
// Lifecycle state machine
// ──────────────────────────────────────────────────────────────────────

/// Lifecycle state of an ATC experience.
///
/// See module-level docs for the state machine diagram and transition rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceState {
    /// Decision made, effect planned, not yet dispatched.
    Planned,
    /// Effect handed to executor, outcome unknown.
    Dispatched,
    /// Effect ran to completion.
    Executed,
    /// Effect dispatch or execution failed (infra/runtime error).
    Failed,
    /// Effect suppressed by rate-limit or budget constraint.
    Throttled,
    /// Effect blocked by safety gate.
    Suppressed,
    /// Deliberate no-action: ATC decided inaction was optimal.
    Skipped,
    /// Executed but outcome not yet observed.
    Open,
    /// Outcome observed and attributed.
    Resolved,
    /// Outcome unobservable (agent departed, project closed, etc).
    Censored,
    /// Resolution window elapsed without observation.
    Expired,
}

impl ExperienceState {
    /// Whether this state is terminal (no further transitions allowed).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Failed
                | Self::Throttled
                | Self::Suppressed
                | Self::Skipped
                | Self::Resolved
                | Self::Censored
                | Self::Expired
        )
    }

    /// Whether this experience has an observable outcome that learning
    /// can use. Terminal dispatch states (Failed, Throttled, Suppressed)
    /// still carry information about *why* the effect didn't execute.
    #[must_use]
    pub const fn has_learning_signal(self) -> bool {
        matches!(
            self,
            Self::Executed
                | Self::Open
                | Self::Resolved
                | Self::Skipped
                | Self::Failed
                | Self::Throttled
                | Self::Suppressed
        )
    }

    /// Whether this state represents deliberate non-execution.
    ///
    /// Skipped, Throttled, and Suppressed all represent cases where ATC
    /// chose or was forced not to act. Learning from these is critical
    /// for validating conservative strategies and safety gates.
    #[must_use]
    pub const fn is_non_execution(self) -> bool {
        matches!(self, Self::Skipped | Self::Throttled | Self::Suppressed)
    }
}

impl std::fmt::Display for ExperienceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Planned => write!(f, "planned"),
            Self::Dispatched => write!(f, "dispatched"),
            Self::Executed => write!(f, "executed"),
            Self::Failed => write!(f, "failed"),
            Self::Throttled => write!(f, "throttled"),
            Self::Suppressed => write!(f, "suppressed"),
            Self::Skipped => write!(f, "skipped"),
            Self::Open => write!(f, "open"),
            Self::Resolved => write!(f, "resolved"),
            Self::Censored => write!(f, "censored"),
            Self::Expired => write!(f, "expired"),
        }
    }
}

/// Validate a state transition. Returns `Ok(())` if the transition is valid,
/// `Err(reason)` if it is not.
pub fn validate_transition(from: ExperienceState, to: ExperienceState) -> Result<(), &'static str> {
    match (from, to) {
        // Same-state transitions are always idempotent no-ops.
        (a, b) if a == b => Ok(()),

        // Valid forward transitions (see state machine diagram).
        (ExperienceState::Planned, ExperienceState::Dispatched)
        | (
            ExperienceState::Dispatched,
            ExperienceState::Executed
            | ExperienceState::Failed
            | ExperienceState::Throttled
            | ExperienceState::Suppressed
            | ExperienceState::Skipped,
        )
        | (ExperienceState::Executed, ExperienceState::Open)
        | (
            ExperienceState::Open,
            ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired,
        ) => Ok(()),

        // All other transitions are invalid.
        _ => Err("invalid experience state transition"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// ATC subsystem (shared with atc.rs, re-exported for experience use)
// ──────────────────────────────────────────────────────────────────────

/// Which ATC subsystem originated the experience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceSubsystem {
    /// Agent liveness detection.
    Liveness,
    /// File reservation conflict management.
    Conflict,
    /// Load balancing and routing.
    LoadRouting,
    /// Cross-subsystem synthesis decisions.
    Synthesis,
    /// Calibration and self-tuning.
    Calibration,
}

impl std::fmt::Display for ExperienceSubsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Liveness => write!(f, "liveness"),
            Self::Conflict => write!(f, "conflict"),
            Self::LoadRouting => write!(f, "load_routing"),
            Self::Synthesis => write!(f, "synthesis"),
            Self::Calibration => write!(f, "calibration"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Effect kind taxonomy
// ──────────────────────────────────────────────────────────────────────

/// The kind of effect that was planned or executed.
///
/// This taxonomy classifies effects by their force level and reversibility.
/// Higher-force effects require more conservative risk budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    /// Informational message sent to an agent (lowest force).
    Advisory,
    /// Liveness probe sent to check agent status.
    Probe,
    /// Reservation released or modified.
    Release,
    /// Reservation forcibly acquired or reassigned (high force).
    ForceReservation,
    /// Load routing suggestion sent.
    RoutingSuggestion,
    /// Backpressure signal applied.
    Backpressure,
    /// No action taken (deliberate inaction).
    NoAction,
}

impl EffectKind {
    /// Whether this effect kind is considered high-force.
    ///
    /// High-force effects require conservative risk budgets and are subject
    /// to tighter safety gate thresholds.
    #[must_use]
    pub const fn is_high_force(self) -> bool {
        matches!(self, Self::ForceReservation | Self::Release)
    }

    /// Whether this is a deliberate no-action.
    #[must_use]
    pub const fn is_no_action(self) -> bool {
        matches!(self, Self::NoAction)
    }
}

impl std::fmt::Display for EffectKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Advisory => write!(f, "advisory"),
            Self::Probe => write!(f, "probe"),
            Self::Release => write!(f, "release"),
            Self::ForceReservation => write!(f, "force_reservation"),
            Self::RoutingSuggestion => write!(f, "routing_suggestion"),
            Self::Backpressure => write!(f, "backpressure"),
            Self::NoAction => write!(f, "no_action"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Non-execution reason
// ──────────────────────────────────────────────────────────────────────

/// Why an effect was not executed.
///
/// Recorded for Throttled, Suppressed, and Skipped states so later
/// analysis can distinguish safety gates from budget limits from
/// deliberate inaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NonExecutionReason {
    /// Rate limit or budget constraint prevented execution.
    BudgetExhausted {
        /// Which budget was exceeded.
        budget_name: String,
        /// Current budget value at suppression time.
        current: f64,
        /// Threshold that was exceeded.
        threshold: f64,
    },
    /// Safety gate blocked execution (risk too high).
    SafetyGate {
        /// Which gate blocked the effect.
        gate_name: String,
        /// Risk score that exceeded the gate threshold.
        risk_score: f64,
        /// Gate threshold.
        gate_threshold: f64,
    },
    /// ATC decided inaction was the optimal policy.
    DeliberateInaction {
        /// Expected loss of the chosen no-action vs the best action.
        no_action_loss: f64,
        /// Expected loss of the best alternative action.
        best_action_loss: f64,
    },
    /// Calibration system was unhealthy; conservative fallback applied.
    CalibrationFallback {
        /// Description of the calibration issue.
        reason: String,
    },
}

// ──────────────────────────────────────────────────────────────────────
// Outcome
// ──────────────────────────────────────────────────────────────────────

/// The observed outcome of an experience.
///
/// Populated during resolution (Open → Resolved). Carries the information
/// needed for delayed outcome attribution and learning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceOutcome {
    /// When the outcome was observed (microseconds since epoch).
    pub observed_ts_micros: i64,
    /// Free-form label describing what happened.
    pub label: String,
    /// Whether the decision was correct in hindsight.
    pub correct: bool,
    /// Actual loss incurred (if computable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_loss: Option<f64>,
    /// Regret: `actual_loss - best_possible_loss` (if computable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regret: Option<f64>,
    /// Structured outcome evidence for audit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

// ──────────────────────────────────────────────────────────────────────
// Resolution kind (unified terminal Open→ transition)
// ──────────────────────────────────────────────────────────────────────

/// Which terminal state an Open experience should transition to.
///
/// Used by `resolve_experience()` in the DB layer to unify the three
/// Open→{Resolved, Censored, Expired} transitions behind a single
/// entry point with idempotency and conflict detection.
#[derive(Debug, Clone)]
pub enum ResolutionKind {
    /// Outcome observed — carries the full attribution record.
    Resolved(ExperienceOutcome),
    /// Outcome unobservable (agent departed, project closed, etc).
    Censored { ts_micros: i64 },
    /// Resolution window elapsed without observation.
    Expired { ts_micros: i64 },
}

impl ResolutionKind {
    /// The target [`ExperienceState`] for this resolution.
    #[must_use]
    pub const fn target_state(&self) -> ExperienceState {
        match self {
            Self::Resolved(_) => ExperienceState::Resolved,
            Self::Censored { .. } => ExperienceState::Censored,
            Self::Expired { .. } => ExperienceState::Expired,
        }
    }

    /// The resolution timestamp (microseconds since epoch).
    #[must_use]
    pub fn ts_micros(&self) -> i64 {
        match self {
            Self::Resolved(outcome) => outcome.observed_ts_micros,
            Self::Censored { ts_micros } | Self::Expired { ts_micros } => *ts_micros,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Canonical experience row
// ──────────────────────────────────────────────────────────────────────

/// The canonical ATC experience row.
///
/// This is the durable record that survives long enough for delayed
/// outcome attribution and learning. Every ATC decision that produces
/// (or deliberately does not produce) an effect generates one experience
/// row per effect.
///
/// # Required vs Optional Fields
///
/// **Required for learning:**
/// - `experience_id`, `decision_id`, `effect_id`, `trace_id`
/// - `state`, `subsystem`, `effect_kind`
/// - `action`, `expected_loss`
/// - `created_ts_micros`
///
/// **Required for audit but optional for learning:**
/// - `claim_id`, `evidence_id`
/// - `subject`, `project_key`
/// - `posterior`, `loss_table`
/// - `evidence_summary`
///
/// **Populated during resolution (optional until then):**
/// - `outcome`
/// - `resolved_ts_micros`
///
/// **Optional metadata:**
/// - `policy_id`, `non_execution_reason`
/// - `runner_up_action`, `runner_up_loss`
/// - `context`: arbitrary structured metadata for audit
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceRow {
    // ── Identifiers ──────────────────────────────────────────────────
    /// Unique experience row ID. Monotonically increasing.
    pub experience_id: ExperienceId,
    /// Decision that originated this experience.
    pub decision_id: DecisionId,
    /// Effect that this experience tracks.
    pub effect_id: EffectId,
    /// Causal-chain correlation ID (stable across related effects).
    pub trace_id: TraceId,
    /// Artifact-graph claim ID.
    pub claim_id: ClaimId,
    /// Artifact-graph evidence ID.
    pub evidence_id: EvidenceIdStr,

    // ── Lifecycle ────────────────────────────────────────────────────
    /// Current lifecycle state.
    pub state: ExperienceState,

    // ── Decision context ─────────────────────────────────────────────
    /// Which ATC subsystem made the decision.
    pub subsystem: ExperienceSubsystem,
    /// Fine-grained decision class within the subsystem.
    pub decision_class: String,
    /// The entity the decision concerns (agent name or thread ID).
    pub subject: String,
    /// Project key for routing context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// Active policy artifact at decision time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,

    // ── Effect ───────────────────────────────────────────────────────
    /// Kind of effect planned or executed.
    pub effect_kind: EffectKind,
    /// Action label (e.g., "DeclareAlive", "AdvisoryMessage").
    pub action: String,

    // ── Decision quality ─────────────────────────────────────────────
    /// Posterior belief at decision time: `[(state_label, probability)]`.
    pub posterior: Vec<(String, f64)>,
    /// Expected loss of the chosen action.
    pub expected_loss: f64,
    /// Runner-up action label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_up_action: Option<String>,
    /// Expected loss of the runner-up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_up_loss: Option<f64>,
    /// Key evidence that drove this decision (human-readable summary).
    pub evidence_summary: String,
    /// Whether calibration was healthy at decision time.
    pub calibration_healthy: bool,
    /// Whether safe mode was active.
    pub safe_mode_active: bool,

    // ── Non-execution ────────────────────────────────────────────────
    /// Why the effect was not executed (for Throttled/Suppressed/Skipped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_execution_reason: Option<NonExecutionReason>,

    // ── Outcome (populated during resolution) ────────────────────────
    /// Observed outcome (populated when state → Resolved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ExperienceOutcome>,

    // ── Timestamps ───────────────────────────────────────────────────
    /// When the experience row was created (microseconds since epoch).
    pub created_ts_micros: i64,
    /// When the effect was dispatched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatched_ts_micros: Option<i64>,
    /// When the effect execution completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_ts_micros: Option<i64>,
    /// When the outcome was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_ts_micros: Option<i64>,

    // ── Feature vector (br-0qt6e.1.2) ──────────────────────────────
    /// Compact feature vector for learning (fixed-width, cheap to scan).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub features: Option<FeatureVector>,

    /// Rare/future context that doesn't fit in the fixed-width vector.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature_ext: Option<FeatureExtension>,

    // ── Extensible metadata ──────────────────────────────────────────
    /// Arbitrary structured metadata for audit and debugging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

impl ExperienceRow {
    /// Transition to a new state with validation.
    ///
    /// Returns `Ok(())` if the transition is valid, `Err(reason)` otherwise.
    /// Idempotent: transitioning to the current state is a no-op.
    pub fn transition_to(&mut self, new_state: ExperienceState) -> Result<(), &'static str> {
        validate_transition(self.state, new_state)?;
        self.state = new_state;
        Ok(())
    }

    /// Resolve this experience with an observed outcome.
    ///
    /// Only valid when state is `Open`. Sets state to `Resolved` and
    /// populates the outcome and resolution timestamp.
    ///
    /// Idempotent: resolving an already-resolved experience returns `Ok(())`.
    pub fn resolve(&mut self, outcome: ExperienceOutcome) -> Result<(), &'static str> {
        if self.state == ExperienceState::Resolved {
            return Ok(()); // idempotent
        }
        validate_transition(self.state, ExperienceState::Resolved)?;
        self.resolved_ts_micros = Some(outcome.observed_ts_micros);
        self.outcome = Some(outcome);
        self.state = ExperienceState::Resolved;
        Ok(())
    }

    /// Censor this experience (outcome unobservable).
    ///
    /// Only valid when state is `Open`. Sets state to `Censored`.
    ///
    /// Idempotent: censoring an already-censored experience returns `Ok(())`.
    pub fn censor(&mut self, ts_micros: i64) -> Result<(), &'static str> {
        if self.state == ExperienceState::Censored {
            return Ok(());
        }
        validate_transition(self.state, ExperienceState::Censored)?;
        self.resolved_ts_micros = Some(ts_micros);
        self.state = ExperienceState::Censored;
        Ok(())
    }

    /// Expire this experience (resolution window elapsed).
    ///
    /// Only valid when state is `Open`. Sets state to `Expired`.
    ///
    /// Idempotent: expiring an already-expired experience returns `Ok(())`.
    pub fn expire(&mut self, ts_micros: i64) -> Result<(), &'static str> {
        if self.state == ExperienceState::Expired {
            return Ok(());
        }
        validate_transition(self.state, ExperienceState::Expired)?;
        self.resolved_ts_micros = Some(ts_micros);
        self.state = ExperienceState::Expired;
        Ok(())
    }

    /// How long between creation and resolution (if resolved).
    #[must_use]
    pub fn resolution_latency_micros(&self) -> Option<i64> {
        self.resolved_ts_micros
            .map(|r| r.saturating_sub(self.created_ts_micros))
    }

    /// Validate structural invariants for the current state.
    ///
    /// Returns a list of violations. An empty vec means the row is valid.
    ///
    /// **State-to-required-fields mapping:**
    /// - `Throttled | Suppressed | Skipped`: `non_execution_reason` SHOULD be set
    /// - `Resolved`: `outcome` and `resolved_ts_micros` MUST be set
    /// - `Censored | Expired`: `resolved_ts_micros` MUST be set
    /// - All states: `posterior` MUST be non-empty
    #[must_use]
    pub fn validate(&self) -> Vec<&'static str> {
        let mut issues = Vec::new();
        if self.posterior.is_empty() {
            issues.push("posterior vector must be non-empty");
        }
        if matches!(
            self.state,
            ExperienceState::Throttled | ExperienceState::Suppressed | ExperienceState::Skipped
        ) && self.non_execution_reason.is_none()
        {
            issues.push("non_execution_reason should be set for throttled/suppressed/skipped");
        }
        if self.state == ExperienceState::Resolved && self.outcome.is_none() {
            issues.push("outcome must be set for resolved state");
        }
        if matches!(
            self.state,
            ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired
        ) && self.resolved_ts_micros.is_none()
        {
            issues.push("resolved_ts_micros must be set for resolved/censored/expired state");
        }
        issues
    }
}

// ──────────────────────────────────────────────────────────────────────
// Compact feature vector (br-0qt6e.1.2)
// ──────────────────────────────────────────────────────────────────────

/// Feature vector version. Increment when the feature layout changes.
///
/// **Evolution rules:**
/// - Adding optional trailing fields: keep the same version.
/// - Changing the meaning/scale of an existing field: bump version.
/// - Removing a field: bump version.
/// - Readers MUST ignore unknown versions gracefully (treat as opaque).
/// - Writers MUST set the version to the current constant.
pub const FEATURE_VERSION: u16 = 1;

/// Compact feature vector for a single ATC experience.
///
/// Fixed-width, numerically stable, cheap to persist and scan.
/// All floating-point values are in basis points (0..10000) or bounded
/// counters. No free-form blobs on the hot path.
///
/// **Storage budget:** 64 bytes max for the core features.
/// **Latency budget:** <1us to construct from decision context.
///
/// # Stratum Key
///
/// The stratum key groups experiences for conformal risk control and
/// empirical-Bayes shrinkage. It is the tuple:
///   `(subsystem, effect_kind, risk_tier)`
///
/// This gives O(15) strata (5 subsystems x 7 effect kinds x ~3 risk
/// tiers in practice), which is small enough for reliable shrinkage
/// but specific enough to detect per-stratum calibration drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FeatureVector {
    /// Feature vector version (for forward compatibility).
    pub version: u16,

    // ── Liveness context (quantized) ─────────────────────────────────
    /// Posterior probability of Alive state, in basis points (0..10000).
    pub posterior_alive_bp: u16,
    /// Posterior probability of Flaky state, in basis points (0..10000).
    pub posterior_flaky_bp: u16,
    /// Agent silence duration in seconds (capped at u16::MAX = 18.2h).
    pub silence_secs: u16,
    /// Agent observation count (capped at u16::MAX).
    pub observation_count: u16,

    // ── Conflict context (quantized) ─────────────────────────────────
    /// Number of active exclusive reservations for this agent (capped at 255).
    pub reservation_count: u8,
    /// Number of overlapping reservation conflicts (capped at 255).
    pub conflict_count: u8,
    /// Whether the agent is in a deadlock cycle.
    pub in_deadlock_cycle: bool,

    // ── Load context (quantized) ─────────────────────────────────────
    /// Agent message throughput (messages/min, capped at 255).
    pub throughput_per_min: u8,
    /// Inbox depth (capped at 255).
    pub inbox_depth: u8,

    // ── Decision quality context ─────────────────────────────────────
    /// Expected loss of chosen action, in basis points (0..10000).
    pub expected_loss_bp: u16,
    /// Loss gap: (runner_up_loss - chosen_loss), in basis points.
    /// Larger gap = more confident decision.
    pub loss_gap_bp: u16,
    /// Whether calibration was healthy at decision time.
    pub calibration_healthy: bool,
    /// Whether safe mode was active.
    pub safe_mode_active: bool,

    // ── Budget context ───────────────────────────────────────────────
    /// Tick budget utilization in basis points (0..10000).
    pub tick_utilization_bp: u16,
    /// Adaptive controller mode: 0=Nominal, 1=Pressure, 2=Conservative.
    pub controller_mode: u8,

    // ── Risk tier (for stratification) ───────────────────────────────
    /// Risk tier for this experience: 0=low, 1=medium, 2=high.
    /// Determined by effect kind: Advisory/Probe/NoAction=0,
    /// RoutingSuggestion/Backpressure=1, Release/ForceReservation=2.
    pub risk_tier: u8,
}

impl FeatureVector {
    /// Construct a zeroed feature vector (version set to current).
    #[must_use]
    pub const fn zeroed() -> Self {
        Self {
            version: FEATURE_VERSION,
            posterior_alive_bp: 0,
            posterior_flaky_bp: 0,
            silence_secs: 0,
            observation_count: 0,
            reservation_count: 0,
            conflict_count: 0,
            in_deadlock_cycle: false,
            throughput_per_min: 0,
            inbox_depth: 0,
            expected_loss_bp: 0,
            loss_gap_bp: 0,
            calibration_healthy: true,
            safe_mode_active: false,
            tick_utilization_bp: 0,
            controller_mode: 0,
            risk_tier: 0,
        }
    }

    /// Compute the stratum key for conformal risk control and shrinkage.
    ///
    /// Returns `(subsystem, effect_kind, risk_tier)` as a string key
    /// suitable for HashMap lookup.
    #[must_use]
    pub fn stratum_key(&self, subsystem: &ExperienceSubsystem, effect_kind: &EffectKind) -> String {
        format!("{subsystem}:{effect_kind}:{}", self.risk_tier)
    }

    /// Derive risk tier from an effect kind.
    ///
    /// - Low (0): Advisory, Probe, NoAction — safe, always reversible
    /// - Medium (1): RoutingSuggestion, Backpressure — affects flow
    /// - High (2): Release, ForceReservation — destroys agent work
    #[must_use]
    pub const fn risk_tier_for(kind: EffectKind) -> u8 {
        match kind {
            EffectKind::Advisory | EffectKind::Probe | EffectKind::NoAction => 0,
            EffectKind::RoutingSuggestion | EffectKind::Backpressure => 1,
            EffectKind::Release | EffectKind::ForceReservation => 2,
        }
    }
}

/// Quantize a probability (0.0..1.0) to basis points (0..10000).
///
/// Clamps out-of-range values.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub const fn prob_to_bp(p: f64) -> u16 {
    // NaN comparisons are always false, so check explicitly.
    if p.is_nan() || p <= 0.0 {
        return 0;
    }
    if p >= 1.0 {
        return 10000;
    }
    (p * 10000.0) as u16
}

/// Quantize a loss value to basis points.
///
/// Loss values are typically in 0..100 range. We scale to 0..10000
/// by multiplying by 100 and clamping.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub const fn loss_to_bp(loss: f64) -> u16 {
    // NaN comparisons are always false, so check explicitly.
    if loss.is_nan() || loss <= 0.0 {
        return 0;
    }
    let scaled = loss * 100.0;
    if scaled >= 10000.0 {
        return 10000;
    }
    scaled as u16
}

/// Saturating cast of u64/usize to u16.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub const fn saturating_u16(v: u64) -> u16 {
    if v > u16::MAX as u64 {
        u16::MAX
    } else {
        v as u16
    }
}

/// Saturating cast of u64/usize to u8.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub const fn saturating_u8(v: u64) -> u8 {
    if v > u8::MAX as u64 { u8::MAX } else { v as u8 }
}

/// Versioned extension payload for rare or future-facing context.
///
/// This is NOT on the hot path. Use for audit data, rare signals,
/// or experimental features that haven't earned a column in the
/// fixed-width [`FeatureVector`].
///
/// **Storage budget:** 256 bytes max (enforced by callers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureExtension {
    /// Extension version (independent of [`FEATURE_VERSION`]).
    pub ext_version: u16,
    /// Compact key-value pairs for rare context.
    pub fields: Vec<(String, i64)>,
}

impl FeatureExtension {
    /// Empty extension.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            ext_version: 1,
            fields: Vec::new(),
        }
    }

    /// Add a field to the extension.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: i64) -> Self {
        self.fields.push((key.into(), value));
        self
    }

    /// Estimated serialized size in bytes.
    #[must_use]
    pub fn estimated_size(&self) -> usize {
        // 2 bytes version + ~(key_len + 8) per field
        2 + self.fields.iter().map(|(k, _)| k.len() + 8).sum::<usize>()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Builder for creating experience rows
// ──────────────────────────────────────────────────────────────────────

/// Builder for constructing an [`ExperienceRow`] from a decision and effect.
///
/// Collects the required fields and optional metadata, then produces an
/// experience row in the `Planned` state.
pub struct ExperienceBuilder {
    decision_id: DecisionId,
    effect_id: EffectId,
    trace_id: TraceId,
    claim_id: ClaimId,
    evidence_id: EvidenceIdStr,
    subsystem: ExperienceSubsystem,
    decision_class: String,
    subject: String,
    effect_kind: EffectKind,
    action: String,
    posterior: Vec<(String, f64)>,
    expected_loss: f64,
    evidence_summary: String,
    calibration_healthy: bool,
    safe_mode_active: bool,
    // Optional fields
    project_key: Option<String>,
    policy_id: Option<String>,
    runner_up_action: Option<String>,
    runner_up_loss: Option<f64>,
    context: Option<Value>,
    features: Option<FeatureVector>,
    feature_ext: Option<FeatureExtension>,
}

impl ExperienceBuilder {
    /// Create a new builder with all required fields.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        decision_id: DecisionId,
        effect_id: EffectId,
        trace_id: TraceId,
        claim_id: ClaimId,
        evidence_id: EvidenceIdStr,
        subsystem: ExperienceSubsystem,
        decision_class: impl Into<String>,
        subject: impl Into<String>,
        effect_kind: EffectKind,
        action: impl Into<String>,
        posterior: Vec<(String, f64)>,
        expected_loss: f64,
        evidence_summary: impl Into<String>,
        calibration_healthy: bool,
        safe_mode_active: bool,
    ) -> Self {
        Self {
            decision_id,
            effect_id,
            trace_id,
            claim_id,
            evidence_id,
            subsystem,
            decision_class: decision_class.into(),
            subject: subject.into(),
            effect_kind,
            action: action.into(),
            posterior,
            expected_loss,
            evidence_summary: evidence_summary.into(),
            calibration_healthy,
            safe_mode_active,
            project_key: None,
            policy_id: None,
            runner_up_action: None,
            runner_up_loss: None,
            context: None,
            features: None,
            feature_ext: None,
        }
    }

    /// Set the project key.
    #[must_use]
    pub fn project_key(mut self, key: impl Into<String>) -> Self {
        self.project_key = Some(key.into());
        self
    }

    /// Set the active policy ID.
    #[must_use]
    pub fn policy_id(mut self, id: impl Into<String>) -> Self {
        self.policy_id = Some(id.into());
        self
    }

    /// Set the runner-up action and its expected loss.
    #[must_use]
    pub fn runner_up(mut self, action: impl Into<String>, loss: f64) -> Self {
        self.runner_up_action = Some(action.into());
        self.runner_up_loss = Some(loss);
        self
    }

    /// Set arbitrary structured metadata.
    #[must_use]
    pub fn context(mut self, ctx: Value) -> Self {
        self.context = Some(ctx);
        self
    }

    /// Attach a compact feature vector for learning.
    #[must_use]
    pub const fn features(mut self, fv: FeatureVector) -> Self {
        self.features = Some(fv);
        self
    }

    /// Attach a feature extension payload for rare context.
    #[must_use]
    pub fn feature_ext(mut self, ext: FeatureExtension) -> Self {
        self.feature_ext = Some(ext);
        self
    }

    /// Build the experience row.
    ///
    /// The `experience_id` and `created_ts_micros` are set by the caller
    /// (typically the experience store assigns the ID at insertion time).
    #[must_use]
    pub fn build(self, experience_id: ExperienceId, created_ts_micros: i64) -> ExperienceRow {
        ExperienceRow {
            experience_id,
            decision_id: self.decision_id,
            effect_id: self.effect_id,
            trace_id: self.trace_id,
            claim_id: self.claim_id,
            evidence_id: self.evidence_id,
            state: ExperienceState::Planned,
            subsystem: self.subsystem,
            decision_class: self.decision_class,
            subject: self.subject,
            project_key: self.project_key,
            policy_id: self.policy_id,
            effect_kind: self.effect_kind,
            action: self.action,
            posterior: self.posterior,
            expected_loss: self.expected_loss,
            runner_up_action: self.runner_up_action,
            runner_up_loss: self.runner_up_loss,
            evidence_summary: self.evidence_summary,
            calibration_healthy: self.calibration_healthy,
            safe_mode_active: self.safe_mode_active,
            non_execution_reason: None,
            outcome: None,
            created_ts_micros,
            dispatched_ts_micros: None,
            executed_ts_micros: None,
            resolved_ts_micros: None,
            features: self.features,
            feature_ext: self.feature_ext,
            context: self.context,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use proptest::prelude::*;

    fn sample_builder() -> ExperienceBuilder {
        ExperienceBuilder::new(
            1, // decision_id
            1, // effect_id
            "trc-abc123".to_string(),
            "clm-1".to_string(),
            "evi-1".to_string(),
            ExperienceSubsystem::Liveness,
            "liveness.check",
            "AgentAlpha",
            EffectKind::Probe,
            "Suspect",
            vec![
                ("Alive".to_string(), 0.3),
                ("Flaky".to_string(), 0.5),
                ("Dead".to_string(), 0.2),
            ],
            2.0,
            "No heartbeat for 90s",
            true,
            false,
        )
    }

    fn sample_row() -> ExperienceRow {
        sample_builder().build(1, Utc::now().timestamp_micros())
    }

    fn state_strategy() -> impl Strategy<Value = ExperienceState> {
        prop_oneof![
            Just(ExperienceState::Planned),
            Just(ExperienceState::Dispatched),
            Just(ExperienceState::Executed),
            Just(ExperienceState::Failed),
            Just(ExperienceState::Throttled),
            Just(ExperienceState::Suppressed),
            Just(ExperienceState::Skipped),
            Just(ExperienceState::Open),
            Just(ExperienceState::Resolved),
            Just(ExperienceState::Censored),
            Just(ExperienceState::Expired),
        ]
    }

    fn subsystem_strategy() -> impl Strategy<Value = ExperienceSubsystem> {
        prop_oneof![
            Just(ExperienceSubsystem::Liveness),
            Just(ExperienceSubsystem::Conflict),
            Just(ExperienceSubsystem::LoadRouting),
            Just(ExperienceSubsystem::Synthesis),
            Just(ExperienceSubsystem::Calibration),
        ]
    }

    fn effect_kind_strategy() -> impl Strategy<Value = EffectKind> {
        prop_oneof![
            Just(EffectKind::Advisory),
            Just(EffectKind::Probe),
            Just(EffectKind::Release),
            Just(EffectKind::ForceReservation),
            Just(EffectKind::RoutingSuggestion),
            Just(EffectKind::Backpressure),
            Just(EffectKind::NoAction),
        ]
    }

    fn token_strategy() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[a-z][a-z0-9_.-]{0,12}").unwrap()
    }

    fn project_key_strategy() -> impl Strategy<Value = String> {
        proptest::string::string_regex("/data/[a-z0-9_./-]{1,24}").unwrap()
    }

    fn sample_non_execution_reason() -> NonExecutionReason {
        NonExecutionReason::CalibrationFallback {
            reason: "contract_test_non_execution".to_string(),
        }
    }

    fn row_in_state(state: ExperienceState, created_ts_micros: i64) -> ExperienceRow {
        let mut row = sample_builder().build(1, created_ts_micros);
        match state {
            ExperienceState::Planned => {}
            ExperienceState::Dispatched => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
            }
            ExperienceState::Executed => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.transition_to(ExperienceState::Executed).unwrap();
            }
            ExperienceState::Failed => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.transition_to(ExperienceState::Failed).unwrap();
            }
            ExperienceState::Throttled => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.non_execution_reason = Some(sample_non_execution_reason());
                row.transition_to(ExperienceState::Throttled).unwrap();
            }
            ExperienceState::Suppressed => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.non_execution_reason = Some(sample_non_execution_reason());
                row.transition_to(ExperienceState::Suppressed).unwrap();
            }
            ExperienceState::Skipped => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.non_execution_reason = Some(sample_non_execution_reason());
                row.transition_to(ExperienceState::Skipped).unwrap();
            }
            ExperienceState::Open => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.transition_to(ExperienceState::Executed).unwrap();
                row.transition_to(ExperienceState::Open).unwrap();
            }
            ExperienceState::Resolved => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.transition_to(ExperienceState::Executed).unwrap();
                row.transition_to(ExperienceState::Open).unwrap();
                row.resolve(ExperienceOutcome {
                    observed_ts_micros: created_ts_micros.saturating_add(1_000),
                    label: "resolved".to_string(),
                    correct: true,
                    actual_loss: Some(0.25),
                    regret: Some(0.0),
                    evidence: Some(serde_json::json!({"sealed": true})),
                })
                .unwrap();
            }
            ExperienceState::Censored => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.transition_to(ExperienceState::Executed).unwrap();
                row.transition_to(ExperienceState::Open).unwrap();
                row.censor(created_ts_micros.saturating_add(2_000)).unwrap();
            }
            ExperienceState::Expired => {
                row.transition_to(ExperienceState::Dispatched).unwrap();
                row.transition_to(ExperienceState::Executed).unwrap();
                row.transition_to(ExperienceState::Open).unwrap();
                row.expire(created_ts_micros.saturating_add(3_000)).unwrap();
            }
        }
        row
    }

    fn transition_allowed(from: ExperienceState, to: ExperienceState) -> bool {
        from == to
            || matches!(
                (from, to),
                (ExperienceState::Planned, ExperienceState::Dispatched)
                    | (
                        ExperienceState::Dispatched,
                        ExperienceState::Executed
                            | ExperienceState::Failed
                            | ExperienceState::Throttled
                            | ExperienceState::Suppressed
                            | ExperienceState::Skipped
                    )
                    | (ExperienceState::Executed, ExperienceState::Open)
                    | (
                        ExperienceState::Open,
                        ExperienceState::Resolved
                            | ExperienceState::Censored
                            | ExperienceState::Expired
                    )
            )
    }

    #[test]
    fn new_experience_starts_as_planned() {
        let row = sample_row();
        assert_eq!(row.state, ExperienceState::Planned);
        assert!(!row.state.is_terminal());
    }

    #[test]
    fn valid_lifecycle_planned_to_resolved() {
        let mut row = sample_row();
        assert!(row.transition_to(ExperienceState::Dispatched).is_ok());
        assert!(row.transition_to(ExperienceState::Executed).is_ok());
        assert!(row.transition_to(ExperienceState::Open).is_ok());

        let outcome = ExperienceOutcome {
            observed_ts_micros: Utc::now().timestamp_micros(),
            label: "agent_responded".to_string(),
            correct: true,
            actual_loss: Some(0.5),
            regret: Some(0.0),
            evidence: None,
        };
        assert!(row.resolve(outcome).is_ok());
        assert_eq!(row.state, ExperienceState::Resolved);
        assert!(row.state.is_terminal());
    }

    #[test]
    fn valid_lifecycle_dispatched_to_skipped() {
        let mut row = sample_row();
        assert!(row.transition_to(ExperienceState::Dispatched).is_ok());
        assert!(row.transition_to(ExperienceState::Skipped).is_ok());
        assert!(row.state.is_terminal());
        assert!(row.state.is_non_execution());
    }

    #[test]
    fn valid_lifecycle_dispatched_to_failed() {
        let mut row = sample_row();
        assert!(row.transition_to(ExperienceState::Dispatched).is_ok());
        assert!(row.transition_to(ExperienceState::Failed).is_ok());
        assert!(row.state.is_terminal());
    }

    #[test]
    fn valid_lifecycle_dispatched_to_throttled() {
        let mut row = sample_row();
        assert!(row.transition_to(ExperienceState::Dispatched).is_ok());
        assert!(row.transition_to(ExperienceState::Throttled).is_ok());
        assert!(row.state.is_terminal());
        assert!(row.state.is_non_execution());
    }

    #[test]
    fn valid_lifecycle_dispatched_to_suppressed() {
        let mut row = sample_row();
        assert!(row.transition_to(ExperienceState::Dispatched).is_ok());
        assert!(row.transition_to(ExperienceState::Suppressed).is_ok());
        assert!(row.state.is_terminal());
        assert!(row.state.is_non_execution());
    }

    #[test]
    fn valid_lifecycle_open_to_censored() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();

        let ts = Utc::now().timestamp_micros();
        assert!(row.censor(ts).is_ok());
        assert_eq!(row.state, ExperienceState::Censored);
        assert!(row.state.is_terminal());
    }

    #[test]
    fn valid_lifecycle_open_to_expired() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();

        let ts = Utc::now().timestamp_micros();
        assert!(row.expire(ts).is_ok());
        assert_eq!(row.state, ExperienceState::Expired);
        assert!(row.state.is_terminal());
    }

    #[test]
    fn invalid_transition_planned_to_executed() {
        let mut row = sample_row();
        assert!(row.transition_to(ExperienceState::Executed).is_err());
    }

    #[test]
    fn invalid_transition_from_terminal() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Failed).unwrap();
        // Cannot transition from terminal state
        assert!(row.transition_to(ExperienceState::Open).is_err());
    }

    #[test]
    fn idempotent_same_state_transition() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        // Same state is a no-op
        assert!(row.transition_to(ExperienceState::Dispatched).is_ok());
    }

    #[test]
    fn idempotent_resolution() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();

        let outcome = ExperienceOutcome {
            observed_ts_micros: 1000,
            label: "ok".to_string(),
            correct: true,
            actual_loss: None,
            regret: None,
            evidence: None,
        };
        assert!(row.resolve(outcome.clone()).is_ok());
        // Second resolution is idempotent
        assert!(row.resolve(outcome).is_ok());
        assert_eq!(row.state, ExperienceState::Resolved);
    }

    #[test]
    fn idempotent_censoring() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();
        assert!(row.censor(1000).is_ok());
        assert!(row.censor(2000).is_ok()); // idempotent
        assert_eq!(row.state, ExperienceState::Censored);
    }

    #[test]
    fn idempotent_expiry() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();
        assert!(row.expire(1000).is_ok());
        assert!(row.expire(2000).is_ok()); // idempotent
        assert_eq!(row.state, ExperienceState::Expired);
    }

    #[test]
    fn resolution_latency() {
        let created = 1_000_000;
        let resolved = 5_000_000;
        let mut row = sample_builder().build(1, created);
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();

        assert!(row.resolution_latency_micros().is_none());

        let outcome = ExperienceOutcome {
            observed_ts_micros: resolved,
            label: "done".to_string(),
            correct: true,
            actual_loss: None,
            regret: None,
            evidence: None,
        };
        row.resolve(outcome).unwrap();

        assert_eq!(row.resolution_latency_micros(), Some(4_000_000));
    }

    #[test]
    fn builder_with_optional_fields() {
        let row = sample_builder()
            .project_key("/data/projects/test")
            .policy_id("pol-1")
            .runner_up("DeclareAlive", 0.5)
            .context(serde_json::json!({"extra": "data"}))
            .build(42, 999_999);

        assert_eq!(row.experience_id, 42);
        assert_eq!(row.created_ts_micros, 999_999);
        assert_eq!(row.project_key.as_deref(), Some("/data/projects/test"));
        assert_eq!(row.policy_id.as_deref(), Some("pol-1"));
        assert_eq!(row.runner_up_action.as_deref(), Some("DeclareAlive"));
        assert_eq!(row.runner_up_loss, Some(0.5));
        assert!(row.context.is_some());
    }

    #[test]
    fn experience_state_display() {
        assert_eq!(ExperienceState::Planned.to_string(), "planned");
        assert_eq!(ExperienceState::Dispatched.to_string(), "dispatched");
        assert_eq!(ExperienceState::Executed.to_string(), "executed");
        assert_eq!(ExperienceState::Failed.to_string(), "failed");
        assert_eq!(ExperienceState::Throttled.to_string(), "throttled");
        assert_eq!(ExperienceState::Suppressed.to_string(), "suppressed");
        assert_eq!(ExperienceState::Skipped.to_string(), "skipped");
        assert_eq!(ExperienceState::Open.to_string(), "open");
        assert_eq!(ExperienceState::Resolved.to_string(), "resolved");
        assert_eq!(ExperienceState::Censored.to_string(), "censored");
        assert_eq!(ExperienceState::Expired.to_string(), "expired");
    }

    #[test]
    fn effect_kind_classification() {
        assert!(EffectKind::ForceReservation.is_high_force());
        assert!(EffectKind::Release.is_high_force());
        assert!(!EffectKind::Advisory.is_high_force());
        assert!(!EffectKind::Probe.is_high_force());
        assert!(EffectKind::NoAction.is_no_action());
        assert!(!EffectKind::Advisory.is_no_action());
    }

    #[test]
    fn has_learning_signal_classification() {
        assert!(ExperienceState::Executed.has_learning_signal());
        assert!(ExperienceState::Open.has_learning_signal());
        assert!(ExperienceState::Resolved.has_learning_signal());
        assert!(ExperienceState::Skipped.has_learning_signal());
        assert!(ExperienceState::Failed.has_learning_signal());
        assert!(ExperienceState::Throttled.has_learning_signal());
        assert!(ExperienceState::Suppressed.has_learning_signal());
        // Non-learning states
        assert!(!ExperienceState::Planned.has_learning_signal());
        assert!(!ExperienceState::Dispatched.has_learning_signal());
        assert!(!ExperienceState::Censored.has_learning_signal());
        assert!(!ExperienceState::Expired.has_learning_signal());
    }

    #[test]
    fn serde_roundtrip() {
        let row = sample_builder()
            .project_key("/test")
            .runner_up("DeclareAlive", 0.3)
            .build(1, 1_000_000);

        let json = serde_json::to_string(&row).unwrap();
        let decoded: ExperienceRow = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.experience_id, row.experience_id);
        assert_eq!(decoded.state, ExperienceState::Planned);
        assert_eq!(decoded.subsystem, ExperienceSubsystem::Liveness);
        assert_eq!(decoded.effect_kind, EffectKind::Probe);
        assert_eq!(decoded.action, "Suspect");
        assert_eq!(decoded.posterior.len(), 3);
    }

    #[test]
    fn non_execution_reason_serde() {
        let reasons = vec![
            NonExecutionReason::BudgetExhausted {
                budget_name: "advisory_rate".to_string(),
                current: 10.0,
                threshold: 5.0,
            },
            NonExecutionReason::SafetyGate {
                gate_name: "force_reservation_risk".to_string(),
                risk_score: 0.95,
                gate_threshold: 0.8,
            },
            NonExecutionReason::DeliberateInaction {
                no_action_loss: 1.0,
                best_action_loss: 3.0,
            },
            NonExecutionReason::CalibrationFallback {
                reason: "posterior divergence > 0.3".to_string(),
            },
        ];

        for reason in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            let decoded: NonExecutionReason = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, reason);
        }
    }

    // ── Feature vector tests (br-0qt6e.1.2) ─────────────────────────

    #[test]
    fn feature_vector_zeroed() {
        let fv = FeatureVector::zeroed();
        assert_eq!(fv.version, FEATURE_VERSION);
        assert_eq!(fv.posterior_alive_bp, 0);
        assert_eq!(fv.risk_tier, 0);
    }

    #[test]
    fn prob_to_bp_quantization() {
        assert_eq!(prob_to_bp(0.0), 0);
        assert_eq!(prob_to_bp(0.5), 5000);
        assert_eq!(prob_to_bp(0.95), 9500);
        assert_eq!(prob_to_bp(1.0), 10000);
        // Clamp out-of-range
        assert_eq!(prob_to_bp(-0.1), 0);
        assert_eq!(prob_to_bp(1.5), 10000);
    }

    #[test]
    fn loss_to_bp_quantization() {
        assert_eq!(loss_to_bp(0.0), 0);
        assert_eq!(loss_to_bp(1.0), 100);
        assert_eq!(loss_to_bp(50.0), 5000);
        assert_eq!(loss_to_bp(100.0), 10000);
        assert_eq!(loss_to_bp(-5.0), 0);
        assert_eq!(loss_to_bp(200.0), 10000);
    }

    #[test]
    fn saturating_cast_u16() {
        assert_eq!(saturating_u16(0), 0);
        assert_eq!(saturating_u16(100), 100);
        assert_eq!(saturating_u16(65535), u16::MAX);
        assert_eq!(saturating_u16(100_000), u16::MAX);
    }

    #[test]
    fn saturating_cast_u8() {
        assert_eq!(saturating_u8(0), 0);
        assert_eq!(saturating_u8(100), 100);
        assert_eq!(saturating_u8(255), u8::MAX);
        assert_eq!(saturating_u8(1000), u8::MAX);
    }

    #[test]
    fn risk_tier_classification() {
        assert_eq!(FeatureVector::risk_tier_for(EffectKind::Advisory), 0);
        assert_eq!(FeatureVector::risk_tier_for(EffectKind::Probe), 0);
        assert_eq!(FeatureVector::risk_tier_for(EffectKind::NoAction), 0);
        assert_eq!(
            FeatureVector::risk_tier_for(EffectKind::RoutingSuggestion),
            1
        );
        assert_eq!(FeatureVector::risk_tier_for(EffectKind::Backpressure), 1);
        assert_eq!(FeatureVector::risk_tier_for(EffectKind::Release), 2);
        assert_eq!(
            FeatureVector::risk_tier_for(EffectKind::ForceReservation),
            2
        );
    }

    #[test]
    fn stratum_key_format() {
        let fv = FeatureVector {
            risk_tier: 2,
            ..FeatureVector::zeroed()
        };
        let key = fv.stratum_key(&ExperienceSubsystem::Liveness, &EffectKind::Release);
        assert_eq!(key, "liveness:release:2");
    }

    #[test]
    fn feature_vector_serde_roundtrip() {
        let mut fv = FeatureVector::zeroed();
        fv.posterior_alive_bp = prob_to_bp(0.95);
        fv.silence_secs = 120;
        fv.risk_tier = 2;
        fv.calibration_healthy = true;

        let json = serde_json::to_string(&fv).unwrap();
        let decoded: FeatureVector = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, fv);
    }

    #[test]
    fn feature_extension_builder() {
        let ext = FeatureExtension::empty()
            .with_field("deadlock_depth", 3)
            .with_field("probe_cost_micros", 150);
        assert_eq!(ext.fields.len(), 2);
        assert!(ext.estimated_size() < 256, "extension should fit in budget");
    }

    #[test]
    fn feature_vector_size_budget() {
        // FeatureVector should be <= 64 bytes
        let size = std::mem::size_of::<FeatureVector>();
        assert!(size <= 64, "FeatureVector is {size} bytes, budget is 64");
    }

    #[test]
    fn feature_version_sealed() {
        assert_eq!(
            FEATURE_VERSION, 1,
            "FEATURE_VERSION must remain 1 until a migration is committed"
        );
    }

    #[test]
    fn validate_resolved_without_outcome() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.transition_to(ExperienceState::Executed).unwrap();
        row.transition_to(ExperienceState::Open).unwrap();
        // Force state without using resolve() to simulate corrupt data
        row.state = ExperienceState::Resolved;
        let issues = row.validate();
        assert!(
            issues.iter().any(|i| i.contains("outcome")),
            "should flag missing outcome: {issues:?}"
        );
    }

    #[test]
    fn validate_skipped_without_reason() {
        let mut row = sample_row();
        row.transition_to(ExperienceState::Dispatched).unwrap();
        row.state = ExperienceState::Skipped;
        let issues = row.validate();
        assert!(
            issues.iter().any(|i| i.contains("non_execution_reason")),
            "should flag missing non_execution_reason: {issues:?}"
        );
    }

    #[test]
    fn validate_clean_row_passes() {
        let row = sample_row();
        let issues = row.validate();
        assert!(
            issues.is_empty(),
            "clean row should have no issues: {issues:?}"
        );
    }

    #[test]
    fn sealed_contract_all_types_roundtrip() {
        // Every public type must survive JSON roundtrip
        let row = sample_builder()
            .project_key("/test")
            .runner_up("DeclareAlive", 0.3)
            .build(1, 1_000_000);
        let json = serde_json::to_string(&row).unwrap();
        let _: ExperienceRow = serde_json::from_str(&json).unwrap();

        for state in [
            ExperienceState::Planned,
            ExperienceState::Dispatched,
            ExperienceState::Executed,
            ExperienceState::Failed,
            ExperienceState::Throttled,
            ExperienceState::Suppressed,
            ExperienceState::Skipped,
            ExperienceState::Open,
            ExperienceState::Resolved,
            ExperienceState::Censored,
            ExperienceState::Expired,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: ExperienceState = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, state);
        }

        for kind in [
            EffectKind::Advisory,
            EffectKind::Probe,
            EffectKind::Release,
            EffectKind::ForceReservation,
            EffectKind::RoutingSuggestion,
            EffectKind::Backpressure,
            EffectKind::NoAction,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let decoded: EffectKind = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, kind);
        }

        for sub in [
            ExperienceSubsystem::Liveness,
            ExperienceSubsystem::Conflict,
            ExperienceSubsystem::LoadRouting,
            ExperienceSubsystem::Synthesis,
            ExperienceSubsystem::Calibration,
        ] {
            let json = serde_json::to_string(&sub).unwrap();
            let decoded: ExperienceSubsystem = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, sub);
        }

        let fv = FeatureVector::zeroed();
        let json = serde_json::to_string(&fv).unwrap();
        let _: FeatureVector = serde_json::from_str(&json).unwrap();

        let outcome = ExperienceOutcome {
            observed_ts_micros: 1000,
            label: "ok".into(),
            correct: true,
            actual_loss: Some(0.5),
            regret: Some(0.1),
            evidence: None,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let _: ExperienceOutcome = serde_json::from_str(&json).unwrap();
    }

    proptest! {
        #[test]
        fn experience_row_roundtrip_property(
            state in state_strategy(),
            subsystem in subsystem_strategy(),
            effect_kind in effect_kind_strategy(),
            subject in token_strategy(),
            action in token_strategy(),
            project_key in prop::option::of(project_key_strategy()),
            policy_id in prop::option::of(token_strategy()),
            runner_up_action in prop::option::of(token_strategy()),
            runner_up_loss_bp in prop::option::of(0u16..=10_000u16),
            calibration_healthy in any::<bool>(),
            safe_mode_active in any::<bool>(),
            created_ts_micros in 1i64..1_000_000i64,
        ) {
            let mut row = row_in_state(state, created_ts_micros);
            row.subsystem = subsystem;
            row.effect_kind = effect_kind;
            row.subject = subject;
            row.action = action;
            row.project_key = project_key;
            row.policy_id = policy_id;
            row.runner_up_action = runner_up_action;
            row.runner_up_loss = row
                .runner_up_action
                .as_ref()
                .and(runner_up_loss_bp)
                .map(|bp| f64::from(bp) / 100.0);
            row.calibration_healthy = calibration_healthy;
            row.safe_mode_active = safe_mode_active;
            row.context = Some(serde_json::json!({
                "state": state.to_string(),
                "sealed": true,
            }));

            let encoded = serde_json::to_string(&row).unwrap();
            let decoded: ExperienceRow = serde_json::from_str(&encoded).unwrap();

            prop_assert!(decoded.validate().is_empty(), "decoded row should remain structurally valid after roundtrip");
            prop_assert_eq!(decoded, row);
        }

        #[test]
        fn validate_transition_matches_contract(
            from in state_strategy(),
            to in state_strategy(),
        ) {
            prop_assert_eq!(
                validate_transition(from, to).is_ok(),
                transition_allowed(from, to),
                "transition contract drifted for {:?} -> {:?}",
                from,
                to
            );
        }
    }

    #[test]
    fn builder_with_features() {
        let fv = FeatureVector {
            posterior_alive_bp: 9500,
            posterior_flaky_bp: 400,
            silence_secs: 60,
            risk_tier: 0,
            ..FeatureVector::zeroed()
        };
        let row = sample_builder()
            .features(fv)
            .build(1, Utc::now().timestamp_micros());
        assert!(row.features.is_some());
        assert_eq!(row.features.unwrap().posterior_alive_bp, 9500);
    }
}
