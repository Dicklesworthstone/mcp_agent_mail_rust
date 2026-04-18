//! Air Traffic Controller (ATC) — Proactive Multi-Agent Coordination Engine.
//!
//! The ATC is a built-in agent that monitors mail traffic, file reservations,
//! and agent activity, then proactively intervenes to prevent coordination
//! failures.  It uses expected-loss decision theory (not hardcoded rules) for
//! every action, with a full evidence ledger for auditability.
//!
//! ## Architecture
//!
//! ```text
//! DecisionCore<S,A>   — generic expected-loss minimization engine
//! EvidenceLedger      — bounded ring buffer of auditable decision records
//! ```
//!
//! All downstream subsystems (liveness, conflict, routing, calibration)
//! are built on top of these two primitives.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::time::Instant;

fn u64_to_f64(value: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        value as f64
    }
}

fn usize_to_f64(value: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        value as f64
    }
}

fn nonnegative_i64_to_f64(value: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        value.max(0) as f64
    }
}

fn micros_f64_to_i64(value: f64) -> i64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    // Direct cast — value is already in microseconds. The previous
    // roundtrip through Duration::from_secs_f64 could panic on large
    // values (>~2^63 nanoseconds). Clamp to i64::MAX instead.
    if value > i64::MAX as f64 {
        return i64::MAX;
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        value as i64
    }
}

fn elapsed_micros(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

fn floor_f64_to_u64(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value.floor() as u64
    }
}

fn stable_fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ──────────────────────────────────────────────────────────────────────
// Decision Core (Track 1)
// ──────────────────────────────────────────────────────────────────────

/// Trait bound for discrete states the ATC reasons about.
pub trait AtcState: Copy + Eq + Hash + fmt::Debug + 'static {}

/// Trait bound for actions the ATC can take.
pub trait AtcAction: Copy + Eq + Hash + fmt::Debug + 'static {}

/// Generic expected-loss decision engine.
///
/// The core of every ATC decision.  Given a posterior belief distribution
/// over discrete states and a loss matrix `L[action][state]`, it selects
/// the action minimizing expected loss:
///
/// ```text
/// a* = argmin_a  Σ_s  L(a, s) × P(s | evidence)
/// ```
///
/// The posterior is updated incrementally via likelihood-weighted EWMA,
/// not full Bayesian conjugate updates — keeping computation O(|states|)
/// per update with no matrix inversions.
#[derive(Debug, Clone)]
pub struct DecisionCore<S: AtcState, A: AtcAction> {
    /// Dense row-major loss matrix aligned to `actions x posterior`.
    loss_matrix: Vec<f64>,
    /// Current posterior belief over states.  Values sum to 1.0.
    posterior: Vec<(S, f64)>,
    /// How fast the posterior moves toward new evidence (0.0–1.0).
    /// Default 0.3 = moderately responsive.
    alpha: f64,
    /// All known actions (for argmin enumeration).  Always non-empty.
    actions: Vec<A>,
    /// Dense lookup for actions.
    action_index: HashMap<A, usize>,
    /// Dense lookup for states.
    state_index: HashMap<S, usize>,
}

impl<S: AtcState, A: AtcAction> DecisionCore<S, A> {
    /// Create a new decision core with the given loss matrix and initial prior.
    ///
    /// `prior` must be a valid probability distribution (non-negative, sums to ~1).
    /// `loss_entries` are `(action, state, cost)` triples.
    pub fn new(prior: &[(S, f64)], loss_entries: &[(A, S, f64)], alpha: f64) -> Self {
        let mut actions = Vec::new();
        let mut action_index = HashMap::new();
        for &(action, _, _) in loss_entries {
            if let std::collections::hash_map::Entry::Vacant(slot) = action_index.entry(action) {
                slot.insert(actions.len());
                actions.push(action);
            }
        }
        assert!(
            !actions.is_empty(),
            "DecisionCore requires at least one action in loss_entries"
        );
        assert!(
            !prior.is_empty(),
            "DecisionCore requires at least one state in prior"
        );
        let mut state_index = HashMap::with_capacity(prior.len());
        for (idx, &(state, _)) in prior.iter().enumerate() {
            let previous = state_index.insert(state, idx);
            assert!(
                previous.is_none(),
                "DecisionCore prior contains duplicate state entries"
            );
        }
        let state_count = prior.len();
        let mut loss_matrix = vec![0.0; actions.len() * state_count];
        for &(action, state, cost) in loss_entries {
            let action_idx = action_index
                .get(&action)
                .copied()
                .expect("DecisionCore action index must exist");
            let state_idx = state_index.get(&state).copied().unwrap_or_else(|| {
                panic!("DecisionCore loss entry references unknown state: {state:?}")
            });
            loss_matrix[action_idx * state_count + state_idx] = cost;
        }
        Self {
            loss_matrix,
            posterior: prior.to_vec(),
            alpha: alpha.clamp(0.01, 1.0),
            actions,
            action_index,
            state_index,
        }
    }

    #[inline]
    const fn state_count(&self) -> usize {
        self.posterior.len()
    }

    #[inline]
    fn loss_offset(&self, action_idx: usize, state_idx: usize) -> usize {
        action_idx * self.state_count() + state_idx
    }

    fn expected_loss_for_index(&self, action_idx: usize) -> f64 {
        let state_count = self.state_count();
        let start = action_idx * state_count;
        self.posterior
            .iter()
            .zip(&self.loss_matrix[start..start + state_count])
            .map(|((_, prob), cost)| *prob * *cost)
            .sum()
    }

    /// Choose the action that minimizes expected loss under the current posterior.
    ///
    /// Returns `(best_action, expected_loss, runner_up_loss)`.
    /// `runner_up_loss` is the expected loss of the next-best action — useful
    /// for the evidence ledger ("how close was this decision?").
    #[must_use]
    pub fn choose_action(&self) -> (A, f64, f64) {
        let mut best_action = self.actions[0];
        let mut best_loss = f64::INFINITY;
        let mut runner_up_loss = f64::INFINITY;

        for (action_idx, &action) in self.actions.iter().enumerate() {
            let expected_loss = self.expected_loss_for_index(action_idx);
            if expected_loss < best_loss {
                runner_up_loss = best_loss;
                best_loss = expected_loss;
                best_action = action;
            } else if expected_loss < runner_up_loss {
                runner_up_loss = expected_loss;
            }
        }

        (best_action, best_loss, runner_up_loss)
    }

    /// Compute expected loss for a specific action under current posterior.
    pub fn expected_loss_for(&self, action: A) -> f64 {
        self.action_index
            .get(&action)
            .copied()
            .map_or(0.0, |action_idx| self.expected_loss_for_index(action_idx))
    }

    /// Update the posterior given observed evidence.
    ///
    /// `likelihoods` maps each state to `P(evidence | state)`.  States not
    /// present in the map are assumed to have likelihood 1.0 (uninformative).
    ///
    /// Uses likelihood-weighted EWMA:
    /// ```text
    /// P(s) ← normalize( P(s) × likelihood(s)^α )
    /// ```
    pub fn update_posterior(&mut self, likelihoods: &[(S, f64)]) {
        /// Minimum probability floor to prevent float underflow from
        /// collapsing the posterior to all-zeros after many updates with
        /// small likelihoods.  1e-10 is small enough to not bias decisions
        /// but large enough to keep the posterior recoverable.
        const PROB_FLOOR: f64 = 1e-10;

        for entry in &mut self.posterior {
            let lk = likelihoods
                .iter()
                .find_map(|&(candidate, likelihood)| (candidate == entry.0).then_some(likelihood))
                .unwrap_or(1.0)
                .max(0.0); // clamp negative likelihoods — they're nonsensical
            // Raise likelihood to alpha power for EWMA-style blending
            entry.1 = (entry.1 * lk.powf(self.alpha)).max(PROB_FLOOR);
        }

        // Normalize to sum to 1.0
        let total: f64 = self.posterior.iter().map(|(_, p)| *p).sum();
        if total > 0.0 {
            for entry in &mut self.posterior {
                entry.1 /= total;
            }
        }
    }

    /// Get the current posterior as a slice.
    #[must_use]
    pub fn posterior(&self) -> &[(S, f64)] {
        &self.posterior
    }

    /// Return the maximum possible loss value in the loss matrix.
    #[must_use]
    pub fn max_possible_loss(&self) -> f64 {
        self.loss_matrix
            .iter()
            .copied()
            .fold(0.0, |a: f64, b: f64| a.max(b))
    }

    /// Get the current posterior formatted for the evidence ledger.
    #[must_use]
    pub fn posterior_summary(&self) -> Vec<(String, f64)> {
        self.posterior
            .iter()
            .map(|(s, p)| (format!("{s:?}"), *p))
            .collect()
    }

    /// Get the posterior mass for a single state.
    #[must_use]
    pub fn posterior_probability(&self, state: S) -> f64 {
        self.state_index
            .get(&state)
            .copied()
            .map_or(0.0, |state_idx| self.posterior[state_idx].1)
    }

    /// Look up a single loss matrix entry.
    #[must_use]
    pub fn loss_entry(&self, action: A, state: S) -> f64 {
        let Some(action_idx) = self.action_index.get(&action).copied() else {
            return 0.0;
        };
        let Some(state_idx) = self.state_index.get(&state).copied() else {
            return 0.0;
        };
        self.loss_matrix[self.loss_offset(action_idx, state_idx)]
    }

    /// Get the best action for a known true state (for regret computation).
    #[must_use]
    pub fn best_action_for_state(&self, state: S) -> A {
        let Some(state_idx) = self.state_index.get(&state).copied() else {
            return self.actions[0];
        };
        self.actions
            .iter()
            .enumerate()
            .min_by(|(left_idx, _), (right_idx, _)| {
                let la = self.loss_matrix[self.loss_offset(*left_idx, state_idx)];
                let lb = self.loss_matrix[self.loss_offset(*right_idx, state_idx)];
                la.total_cmp(&lb)
            })
            .map_or(self.actions[0], |(_, &action)| action)
    }

    /// Update a single loss entry while keeping the dense matrix coherent.
    pub fn set_loss_entry(&mut self, action: A, state: S, cost: f64) {
        let action_idx =
            self.action_index.get(&action).copied().unwrap_or_else(|| {
                panic!("DecisionCore unknown action in set_loss_entry: {action:?}")
            });
        let state_idx =
            self.state_index.get(&state).copied().unwrap_or_else(|| {
                panic!("DecisionCore unknown state in set_loss_entry: {state:?}")
            });
        let offset = self.loss_offset(action_idx, state_idx);
        self.loss_matrix[offset] = cost;
    }

    /// Refresh the decision policy from another core while preserving the
    /// receiver's posterior state.
    pub fn sync_policy_from(&mut self, other: &Self) {
        self.actions.clone_from(&other.actions);
        self.action_index.clear();
        for (idx, action) in self.actions.iter().copied().enumerate() {
            self.action_index.insert(action, idx);
        }
        self.loss_matrix.clear();
        self.loss_matrix
            .resize(self.actions.len() * self.state_count(), 0.0);
        for (action_idx, action) in self.actions.iter().copied().enumerate() {
            for (state_idx, (state, _)) in self.posterior.iter().copied().enumerate() {
                let offset = self.loss_offset(action_idx, state_idx);
                self.loss_matrix[offset] = other.loss_entry(action, state);
            }
        }
        self.alpha = other.alpha;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Concrete state/action enums
// ──────────────────────────────────────────────────────────────────────

/// Agent liveness states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LivenessState {
    Alive,
    Flaky,
    Dead,
}
impl AtcState for LivenessState {}

/// Liveness actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LivenessAction {
    DeclareAlive,
    Suspect,
    ReleaseReservations,
}
impl AtcAction for LivenessAction {}

/// Conflict states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictState {
    NoConflict,
    MildOverlap,
    SevereCollision,
}
impl AtcState for ConflictState {}

/// Conflict actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictAction {
    Ignore,
    AdvisoryMessage,
    ForceReservation,
}
impl AtcAction for ConflictAction {}

/// Load states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoadState {
    Underloaded,
    Balanced,
    Overloaded,
}
impl AtcState for LoadState {}

/// Load routing actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoadAction {
    RouteHere,
    SuggestAlternative,
    Defer,
}
impl AtcAction for LoadAction {}

// ──────────────────────────────────────────────────────────────────────
// Default loss matrices (from Track 1 bead)
// ──────────────────────────────────────────────────────────────────────

/// Build the default liveness decision core.
#[must_use]
pub fn default_liveness_core() -> DecisionCore<LivenessState, LivenessAction> {
    DecisionCore::new(
        &[
            (LivenessState::Alive, 0.95),
            (LivenessState::Flaky, 0.04),
            (LivenessState::Dead, 0.01),
        ],
        &[
            (LivenessAction::DeclareAlive, LivenessState::Alive, 0.0),
            (LivenessAction::DeclareAlive, LivenessState::Flaky, 3.0),
            (LivenessAction::DeclareAlive, LivenessState::Dead, 50.0),
            (LivenessAction::Suspect, LivenessState::Alive, 8.0),
            (LivenessAction::Suspect, LivenessState::Flaky, 2.0),
            (LivenessAction::Suspect, LivenessState::Dead, 6.0),
            (
                LivenessAction::ReleaseReservations,
                LivenessState::Alive,
                100.0,
            ),
            (
                LivenessAction::ReleaseReservations,
                LivenessState::Flaky,
                20.0,
            ),
            (
                LivenessAction::ReleaseReservations,
                LivenessState::Dead,
                1.0,
            ),
        ],
        0.3,
    )
}

/// Build the default conflict decision core.
#[must_use]
pub fn default_conflict_core() -> DecisionCore<ConflictState, ConflictAction> {
    DecisionCore::new(
        &[
            (ConflictState::NoConflict, 0.90),
            (ConflictState::MildOverlap, 0.08),
            (ConflictState::SevereCollision, 0.02),
        ],
        &[
            (ConflictAction::Ignore, ConflictState::NoConflict, 0.0),
            (ConflictAction::Ignore, ConflictState::MildOverlap, 15.0),
            (
                ConflictAction::Ignore,
                ConflictState::SevereCollision,
                100.0,
            ),
            (
                ConflictAction::AdvisoryMessage,
                ConflictState::NoConflict,
                3.0,
            ),
            (
                ConflictAction::AdvisoryMessage,
                ConflictState::MildOverlap,
                1.0,
            ),
            (
                ConflictAction::AdvisoryMessage,
                ConflictState::SevereCollision,
                8.0,
            ),
            (
                ConflictAction::ForceReservation,
                ConflictState::NoConflict,
                12.0,
            ),
            (
                ConflictAction::ForceReservation,
                ConflictState::MildOverlap,
                4.0,
            ),
            (
                ConflictAction::ForceReservation,
                ConflictState::SevereCollision,
                2.0,
            ),
        ],
        0.3,
    )
}

/// Build the default load routing decision core.
#[must_use]
pub fn default_load_core() -> DecisionCore<LoadState, LoadAction> {
    DecisionCore::new(
        &[
            (LoadState::Balanced, 0.60),
            (LoadState::Underloaded, 0.30),
            (LoadState::Overloaded, 0.10),
        ],
        &[
            (LoadAction::RouteHere, LoadState::Underloaded, 1.0),
            (LoadAction::RouteHere, LoadState::Balanced, 3.0),
            (LoadAction::RouteHere, LoadState::Overloaded, 25.0),
            (LoadAction::SuggestAlternative, LoadState::Underloaded, 8.0),
            (LoadAction::SuggestAlternative, LoadState::Balanced, 2.0),
            (LoadAction::SuggestAlternative, LoadState::Overloaded, 3.0),
            (LoadAction::Defer, LoadState::Underloaded, 15.0),
            (LoadAction::Defer, LoadState::Balanced, 8.0),
            (LoadAction::Defer, LoadState::Overloaded, 1.0),
        ],
        0.3,
    )
}

// ──────────────────────────────────────────────────────────────────────
// Evidence Ledger (Track 4)
// ──────────────────────────────────────────────────────────────────────

/// Which ATC subsystem produced a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AtcSubsystem {
    Liveness,
    Conflict,
    LoadRouting,
    Synthesis,
    Calibration,
}

impl fmt::Display for AtcSubsystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Liveness => write!(f, "liveness"),
            Self::Conflict => write!(f, "conflict"),
            Self::LoadRouting => write!(f, "load_routing"),
            Self::Synthesis => write!(f, "synthesis"),
            Self::Calibration => write!(f, "calibration"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AtcLossTableEntry {
    pub action: String,
    pub expected_loss: f64,
}

/// A single auditable decision record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AtcDecisionRecord {
    /// Unique decision ID (monotonically increasing).
    pub id: u64,
    /// Stable artifact-graph claim identifier.
    pub claim_id: String,
    /// Stable artifact-graph evidence identifier.
    pub evidence_id: String,
    /// Stable artifact-graph trace identifier.
    pub trace_id: String,
    /// Timestamp of the decision (microseconds since epoch).
    pub timestamp_micros: i64,
    /// Which subsystem made the decision.
    pub subsystem: AtcSubsystem,
    /// Fine-grained decision class within the subsystem.
    pub decision_class: String,
    /// The entity the decision concerns (agent name or thread ID).
    pub subject: String,
    /// Policy artifact that was active when the decision was made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    /// Posterior belief at decision time.
    pub posterior: Vec<(String, f64)>,
    /// Action chosen.
    pub action: String,
    /// Expected loss of chosen action.
    pub expected_loss: f64,
    /// Expected loss of the next-best alternative.
    pub runner_up_loss: f64,
    /// Expected-loss table for the visible candidate actions.
    pub loss_table: Vec<AtcLossTableEntry>,
    /// Key evidence that drove this decision.
    pub evidence_summary: String,
    /// Whether the calibration guard was healthy at decision time.
    pub calibration_healthy: bool,
    /// Whether safe mode was active.
    pub safe_mode_active: bool,
    /// Explicit fallback reason when deterministic conservative mode was active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

impl AtcDecisionRecord {
    /// Format a human-readable message for the #atc-decisions thread.
    #[must_use]
    pub fn format_message(&self) -> String {
        let posterior_str: Vec<String> = self
            .posterior
            .iter()
            .map(|(s, p)| format!("P({s})={p:.2}"))
            .collect();

        format!(
            "[ATC Decision #{id}] {action} on {subject}.\n\
             Evidence: {evidence}.\n\
             Posterior: {posterior}.\n\
             Expected loss: {el:.1} (runner-up: {ru:.1}).{safe}",
            id = self.id,
            action = self.action,
            subject = self.subject,
            evidence = self.evidence_summary,
            posterior = posterior_str.join(", "),
            el = self.expected_loss,
            ru = self.runner_up_loss,
            safe = if self.safe_mode_active {
                "\n[SAFE MODE ACTIVE]"
            } else {
                ""
            },
        )
    }
}

/// Bounded ring buffer of auditable ATC decision records.
#[derive(Debug)]
pub struct EvidenceLedger {
    /// Decision records (bounded, oldest evicted first).
    records: VecDeque<AtcDecisionRecord>,
    /// Maximum capacity.
    capacity: usize,
    /// Next decision ID.
    next_id: u64,
}

/// Builder for recording a decision to the evidence ledger.
///
/// Avoids the 11-argument `record()` method that clippy rightly rejects.
pub struct DecisionBuilder<'a, S: AtcState, A: AtcAction> {
    pub subsystem: AtcSubsystem,
    pub decision_class: &'a str,
    pub subject: &'a str,
    pub core: &'a DecisionCore<S, A>,
    pub action: A,
    pub expected_loss: f64,
    pub runner_up_loss: f64,
    pub evidence_summary: &'a str,
    pub calibration_healthy: bool,
    pub safe_mode_active: bool,
    pub policy_id: Option<&'a str>,
    pub fallback_reason: Option<&'a str>,
    pub timestamp_micros: i64,
}

pub struct EventDecisionBuilder<'a> {
    pub subsystem: AtcSubsystem,
    pub decision_class: &'a str,
    pub subject: &'a str,
    pub policy_id: Option<&'a str>,
    pub posterior: Vec<(String, f64)>,
    pub action: &'a str,
    pub expected_loss: f64,
    pub runner_up_loss: f64,
    pub loss_table: Vec<AtcLossTableEntry>,
    pub evidence_summary: &'a str,
    pub calibration_healthy: bool,
    pub safe_mode_active: bool,
    pub fallback_reason: Option<&'a str>,
    pub timestamp_micros: i64,
}

impl EvidenceLedger {
    fn make_trace_id(
        id: u64,
        subsystem: AtcSubsystem,
        decision_class: &str,
        subject: &str,
        action: &str,
        policy_id: Option<&str>,
    ) -> String {
        let mut seed = format!("{id}:{subsystem}:{decision_class}:{subject}:{action}");
        if let Some(policy_id) = policy_id {
            seed.push(':');
            seed.push_str(policy_id);
        }
        format!("atc-trace-{:016x}", stable_fnv1a64(seed.as_bytes()))
    }

    fn loss_table_for_core<S: AtcState, A: AtcAction>(
        core: &DecisionCore<S, A>,
    ) -> Vec<AtcLossTableEntry> {
        let mut losses: Vec<AtcLossTableEntry> = core
            .actions
            .iter()
            .copied()
            .map(|action| AtcLossTableEntry {
                action: format!("{action:?}"),
                expected_loss: core.expected_loss_for(action),
            })
            .collect();
        losses.sort_by(|left, right| {
            left.expected_loss
                .total_cmp(&right.expected_loss)
                .then_with(|| left.action.cmp(&right.action))
        });
        losses
    }

    /// Create a new ledger with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
            next_id: 1,
        }
    }

    /// Record a decision.  Returns the assigned decision ID.
    pub fn record<S: AtcState, A: AtcAction>(
        &mut self,
        builder: &DecisionBuilder<'_, S, A>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let action = format!("{:?}", builder.action);
        let policy_id = builder.policy_id.map(ToOwned::to_owned);
        let trace_id = Self::make_trace_id(
            id,
            builder.subsystem,
            builder.decision_class,
            builder.subject,
            &action,
            builder.policy_id,
        );

        let record = AtcDecisionRecord {
            id,
            claim_id: format!("atc-claim-{id}"),
            evidence_id: format!("atc-evidence-{id}"),
            trace_id,
            timestamp_micros: builder.timestamp_micros,
            subsystem: builder.subsystem,
            decision_class: builder.decision_class.to_string(),
            subject: builder.subject.to_string(),
            policy_id,
            posterior: builder.core.posterior_summary(),
            action,
            expected_loss: builder.expected_loss,
            runner_up_loss: builder.runner_up_loss,
            loss_table: Self::loss_table_for_core(builder.core),
            evidence_summary: builder.evidence_summary.to_string(),
            calibration_healthy: builder.calibration_healthy,
            safe_mode_active: builder.safe_mode_active,
            fallback_reason: builder.fallback_reason.map(ToOwned::to_owned),
        };

        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);

        id
    }

    /// Record a deterministic event-style decision that is not backed by a single `DecisionCore`.
    pub fn record_event(&mut self, builder: &EventDecisionBuilder<'_>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let trace_id = Self::make_trace_id(
            id,
            builder.subsystem,
            builder.decision_class,
            builder.subject,
            builder.action,
            builder.policy_id,
        );

        let record = AtcDecisionRecord {
            id,
            claim_id: format!("atc-claim-{id}"),
            evidence_id: format!("atc-evidence-{id}"),
            trace_id,
            timestamp_micros: builder.timestamp_micros,
            subsystem: builder.subsystem,
            decision_class: builder.decision_class.to_string(),
            subject: builder.subject.to_string(),
            policy_id: builder.policy_id.map(ToOwned::to_owned),
            posterior: builder.posterior.clone(),
            action: builder.action.to_string(),
            expected_loss: builder.expected_loss,
            runner_up_loss: builder.runner_up_loss,
            loss_table: builder.loss_table.clone(),
            evidence_summary: builder.evidence_summary.to_string(),
            calibration_healthy: builder.calibration_healthy,
            safe_mode_active: builder.safe_mode_active,
            fallback_reason: builder.fallback_reason.map(ToOwned::to_owned),
        };

        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);

        id
    }

    /// Get the most recent N decision records.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &AtcDecisionRecord> {
        self.records.iter().rev().take(n)
    }

    /// Get all records (oldest first).
    pub fn all(&self) -> impl Iterator<Item = &AtcDecisionRecord> {
        self.records.iter()
    }

    /// Insert a pre-existing record directly into the ledger (used for replay).
    pub fn insert_raw(&mut self, record: AtcDecisionRecord) {
        self.next_id = self.next_id.max(record.id + 1);
        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Number of records in the ledger.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the ledger is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Get a record by decision ID.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&AtcDecisionRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Get the most recent decision ID.
    #[must_use]
    pub const fn latest_id(&self) -> u64 {
        self.next_id.saturating_sub(1)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Decision Core tests ──────────────────────────────────────────

    #[test]
    fn argmin_selects_lowest_expected_loss() {
        let core = default_liveness_core();
        // With strong alive prior (0.95), DeclareAlive should have lowest loss
        let (action, loss, _runner_up) = core.choose_action();
        assert_eq!(action, LivenessAction::DeclareAlive);
        assert!(loss < 5.0, "expected low loss with alive prior, got {loss}");
    }

    #[test]
    fn argmin_shifts_with_posterior_update() {
        let mut core = default_liveness_core();

        // Push strong evidence toward Dead
        for _ in 0..20 {
            core.update_posterior(&[
                (LivenessState::Alive, 0.01),
                (LivenessState::Flaky, 0.1),
                (LivenessState::Dead, 0.95),
            ]);
        }

        let (action, _loss, _runner_up) = core.choose_action();
        assert_eq!(
            action,
            LivenessAction::ReleaseReservations,
            "strong dead evidence should trigger release"
        );
    }

    #[test]
    fn posterior_update_converges_to_true_state() {
        let mut core = default_liveness_core();

        // Repeatedly observe evidence consistent with Flaky
        for _ in 0..50 {
            core.update_posterior(&[
                (LivenessState::Alive, 0.2),
                (LivenessState::Flaky, 0.9),
                (LivenessState::Dead, 0.1),
            ]);
        }

        let flaky_prob = core
            .posterior()
            .iter()
            .find(|(s, _)| *s == LivenessState::Flaky)
            .map_or(0.0, |(_, p)| *p);

        assert!(
            flaky_prob > 0.5,
            "posterior should converge toward Flaky, got P(flaky)={flaky_prob:.3}"
        );
    }

    #[test]
    fn posterior_stays_normalized() {
        let mut core = default_liveness_core();

        core.update_posterior(&[(LivenessState::Alive, 0.5), (LivenessState::Dead, 0.8)]);

        let total: f64 = core.posterior().iter().map(|(_, p)| *p).sum();
        assert!(
            (total - 1.0).abs() < 1e-10,
            "posterior should sum to 1.0, got {total}"
        );
    }

    #[test]
    fn initial_prior_does_not_trigger_aggressive_actions() {
        // With default priors, no core should pick the most aggressive action
        let (liveness_action, _, _) = default_liveness_core().choose_action();
        assert_ne!(
            liveness_action,
            LivenessAction::ReleaseReservations,
            "initial prior must not trigger reservation release"
        );

        let (conflict_action, _, _) = default_conflict_core().choose_action();
        assert_ne!(
            conflict_action,
            ConflictAction::ForceReservation,
            "initial prior must not force reservations"
        );
    }

    #[test]
    fn loss_matrix_asymmetry_produces_correct_ordering() {
        // Releasing alive agent (100) >> failing to release dead (50)
        // So the core should be VERY reluctant to release
        let core = default_liveness_core();
        let release_loss = core.expected_loss_for(LivenessAction::ReleaseReservations);
        let alive_loss = core.expected_loss_for(LivenessAction::DeclareAlive);
        assert!(
            release_loss > alive_loss * 5.0,
            "release should be much more costly than declare_alive under alive prior"
        );
    }

    #[test]
    fn best_action_for_known_state() {
        let core = default_liveness_core();
        assert_eq!(
            core.best_action_for_state(LivenessState::Alive),
            LivenessAction::DeclareAlive
        );
        assert_eq!(
            core.best_action_for_state(LivenessState::Dead),
            LivenessAction::ReleaseReservations
        );
    }

    #[test]
    fn runner_up_loss_is_second_best() {
        let core = default_liveness_core();
        let (_best, best_loss, runner_up) = core.choose_action();
        assert!(
            runner_up >= best_loss,
            "runner-up loss must be >= best loss"
        );
        // Runner-up should be different from best (non-trivial matrix)
        assert!(
            runner_up > best_loss,
            "runner-up should be strictly greater for a non-degenerate matrix"
        );
    }

    #[test]
    fn conflict_core_prefers_advisory_for_mild_overlap() {
        let mut core = default_conflict_core();

        // Push evidence toward mild overlap
        for _ in 0..15 {
            core.update_posterior(&[
                (ConflictState::NoConflict, 0.1),
                (ConflictState::MildOverlap, 0.9),
                (ConflictState::SevereCollision, 0.1),
            ]);
        }

        let (action, _, _) = core.choose_action();
        assert_eq!(
            action,
            ConflictAction::AdvisoryMessage,
            "mild overlap should trigger advisory, not force or ignore"
        );
    }

    // ── Evidence Ledger tests ────────────────────────────────────────

    fn test_decision<'a>(
        core: &'a DecisionCore<LivenessState, LivenessAction>,
        subject: &'a str,
        ts: i64,
    ) -> DecisionBuilder<'a, LivenessState, LivenessAction> {
        DecisionBuilder {
            subsystem: AtcSubsystem::Liveness,
            decision_class: "test",
            subject,
            core,
            action: LivenessAction::DeclareAlive,
            expected_loss: 1.0,
            runner_up_loss: 2.0,
            evidence_summary: "test",
            calibration_healthy: true,
            safe_mode_active: false,
            policy_id: None,
            fallback_reason: None,
            timestamp_micros: ts,
        }
    }

    #[test]
    fn ledger_records_and_retrieves() {
        let mut ledger = EvidenceLedger::new(100);
        let core = default_liveness_core();
        let id = ledger.record(&DecisionBuilder {
            subsystem: AtcSubsystem::Liveness,
            decision_class: "test",
            subject: "TestAgent",
            core: &core,
            action: LivenessAction::DeclareAlive,
            expected_loss: 1.5,
            runner_up_loss: 8.0,
            evidence_summary: "agent sent message 3s ago",
            calibration_healthy: true,
            safe_mode_active: false,
            policy_id: Some("policy-test"),
            fallback_reason: None,
            timestamp_micros: 1_000_000,
        });
        assert_eq!(id, 1);
        assert_eq!(ledger.len(), 1);

        let record = ledger.get(1).expect("should find record by ID");
        assert_eq!(record.subject, "TestAgent");
        assert_eq!(record.subsystem, AtcSubsystem::Liveness);
        assert!(!record.safe_mode_active);
    }

    #[test]
    fn ledger_evicts_oldest_when_full() {
        let mut ledger = EvidenceLedger::new(3);
        let core = default_liveness_core();

        for i in 0..5 {
            let subject = format!("Agent{i}");
            ledger.record(&test_decision(&core, &subject, i64::from(i) * 1_000_000));
        }

        assert_eq!(ledger.len(), 3, "should cap at capacity");
        assert!(ledger.get(1).is_none(), "oldest records should be evicted");
        assert!(ledger.get(2).is_none(), "second oldest should be evicted");
        assert!(ledger.get(3).is_some(), "third should survive");
        assert!(ledger.get(5).is_some(), "newest should survive");
    }

    #[test]
    fn ledger_recent_returns_newest_first() {
        let mut ledger = EvidenceLedger::new(100);
        let core = default_liveness_core();

        for i in 0..5 {
            let subject = format!("Agent{i}");
            ledger.record(&test_decision(&core, &subject, i64::from(i) * 1_000_000));
        }

        let recent: Vec<u64> = ledger.recent(3).map(|r| r.id).collect();
        assert_eq!(recent, vec![5, 4, 3], "recent should return newest first");
    }

    #[test]
    fn decision_record_formats_readable_message() {
        let record = AtcDecisionRecord {
            id: 42,
            claim_id: "atc-claim-42".to_string(),
            evidence_id: "atc-evidence-42".to_string(),
            trace_id: "atc-trace-42".to_string(),
            timestamp_micros: 1_000_000,
            subsystem: AtcSubsystem::Liveness,
            decision_class: "liveness_transition".to_string(),
            subject: "BlueFox".to_string(),
            policy_id: Some("policy-test".to_string()),
            posterior: vec![
                ("Alive".to_string(), 0.12),
                ("Flaky".to_string(), 0.41),
                ("Dead".to_string(), 0.47),
            ],
            action: "Suspect".to_string(),
            expected_loss: 3.2,
            runner_up_loss: 18.1,
            loss_table: vec![AtcLossTableEntry {
                action: "Suspect".to_string(),
                expected_loss: 3.2,
            }],
            evidence_summary: "no activity for 847s".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            fallback_reason: None,
        };

        let msg = record.format_message();
        assert!(msg.contains("Decision #42"), "should include decision ID");
        assert!(msg.contains("BlueFox"), "should include subject");
        assert!(msg.contains("P(Alive)=0.12"), "should include posterior");
        assert!(msg.contains("847s"), "should include evidence");
        assert!(
            !msg.contains("SAFE MODE"),
            "should not mention safe mode when inactive"
        );
    }

    #[test]
    fn decision_record_shows_safe_mode() {
        let record = AtcDecisionRecord {
            id: 1,
            claim_id: "atc-claim-1".to_string(),
            evidence_id: "atc-evidence-1".to_string(),
            trace_id: "atc-trace-1".to_string(),
            timestamp_micros: 0,
            subsystem: AtcSubsystem::Calibration,
            decision_class: "safe_mode".to_string(),
            subject: "system".to_string(),
            policy_id: None,
            posterior: vec![],
            action: "SafeMode".to_string(),
            expected_loss: 0.0,
            runner_up_loss: 0.0,
            loss_table: Vec::new(),
            evidence_summary: "coverage dropped".to_string(),
            calibration_healthy: false,
            safe_mode_active: true,
            fallback_reason: Some("calibration_safe_mode".to_string()),
        };

        let msg = record.format_message();
        assert!(msg.contains("SAFE MODE"), "should show safe mode warning");
    }

    // ── Property tests ───────────────────────────────────────────────

    #[test]
    fn argmin_is_truly_minimal_across_all_actions() {
        // Verify for all three default cores that the chosen action has
        // the lowest expected loss among all alternatives.
        check_argmin_core(&default_liveness_core());
        check_argmin_core(&default_conflict_core());
        check_argmin_core(&default_load_core());
    }

    fn check_argmin_core<S: AtcState, A: AtcAction>(core: &DecisionCore<S, A>) {
        let (best, best_loss, _) = core.choose_action();
        for &action in &core.actions {
            let loss = core.expected_loss_for(action);
            assert!(
                best_loss <= loss + f64::EPSILON,
                "action {best:?} (loss={best_loss}) should be <= {action:?} (loss={loss})"
            );
        }
    }

    #[test]
    fn posterior_recovers_from_near_zero_likelihoods() {
        let mut core = default_liveness_core();

        // Slam the posterior toward Dead with near-zero alive likelihood
        for _ in 0..100 {
            core.update_posterior(&[
                (LivenessState::Alive, 1e-15),
                (LivenessState::Flaky, 1e-15),
                (LivenessState::Dead, 1.0),
            ]);
        }
        // Posterior should still be valid (sum to 1.0, no NaN)
        let total: f64 = core.posterior().iter().map(|(_, p)| *p).sum();
        assert!(
            (total - 1.0).abs() < 1e-6,
            "posterior should stay normalized after extreme updates, got {total}"
        );
        assert!(
            core.posterior().iter().all(|(_, p)| p.is_finite()),
            "no NaN or Inf in posterior"
        );

        // Now push back toward Alive — should recover, not stay stuck
        for _ in 0..100 {
            core.update_posterior(&[
                (LivenessState::Alive, 1.0),
                (LivenessState::Flaky, 0.01),
                (LivenessState::Dead, 1e-15),
            ]);
        }
        let alive_prob = core
            .posterior()
            .iter()
            .find(|(s, _)| *s == LivenessState::Alive)
            .map_or(0.0, |(_, p)| *p);
        assert!(
            alive_prob > 0.5,
            "posterior should recover toward Alive after evidence shift, got {alive_prob:.6}"
        );
    }

    // ── DecisionCore edge case tests (br-1u8gy) ────────────────────────

    #[test]
    fn partial_likelihoods_leave_unspecified_states_uninformative() {
        let mut core = default_liveness_core();
        // Initial posterior: Alive ~0.95, Flaky ~0.04, Dead ~0.01
        let initial_posterior: Vec<(LivenessState, f64)> = core.posterior().to_vec();

        // Update with likelihoods for only Alive and Dead — Flaky is absent
        // and should get likelihood 1.0 (uninformative).
        core.update_posterior(&[
            (LivenessState::Alive, 0.1), // strong evidence against Alive
            (LivenessState::Dead, 5.0),  // strong evidence for Dead
        ]);

        let posterior: Vec<(LivenessState, f64)> = core.posterior().to_vec();

        // Alive probability should have decreased
        let alive_before = initial_posterior
            .iter()
            .find(|(s, _)| *s == LivenessState::Alive)
            .unwrap()
            .1;
        let alive_after = posterior
            .iter()
            .find(|(s, _)| *s == LivenessState::Alive)
            .unwrap()
            .1;
        assert!(
            alive_after < alive_before,
            "Alive should decrease with low likelihood, posterior: {posterior:?}"
        );

        // Dead probability should have increased
        let dead_before = initial_posterior
            .iter()
            .find(|(s, _)| *s == LivenessState::Dead)
            .unwrap()
            .1;
        let dead_after = posterior
            .iter()
            .find(|(s, _)| *s == LivenessState::Dead)
            .unwrap()
            .1;
        assert!(
            dead_after > dead_before,
            "Dead should increase with high likelihood, posterior: {posterior:?}"
        );

        // Posterior should still be normalized
        let total: f64 = posterior.iter().map(|(_, p)| *p).sum();
        assert!(
            (total - 1.0).abs() < 1e-10,
            "posterior should be normalized, got total={total}, posterior: {posterior:?}"
        );
    }

    #[test]
    fn two_action_core_runner_up_is_other_action() {
        // A core with exactly 2 actions — runner_up should be the loss
        // of the other action, not INFINITY.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        enum TwoState {
            Good,
            Bad,
        }
        impl AtcState for TwoState {}
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        enum TwoAction {
            Act,
            Wait,
        }
        impl AtcAction for TwoAction {}

        let core = DecisionCore::new(
            &[(TwoState::Good, 0.7), (TwoState::Bad, 0.3)],
            &[
                (TwoAction::Act, TwoState::Good, 0.0),
                (TwoAction::Act, TwoState::Bad, 10.0),
                (TwoAction::Wait, TwoState::Good, 3.0),
                (TwoAction::Wait, TwoState::Bad, 1.0),
            ],
            0.3,
        );

        let (best, best_loss, runner_up_loss) = core.choose_action();
        let posterior = core.posterior();

        // runner_up should be finite (the loss of the other action)
        assert!(
            runner_up_loss.is_finite(),
            "runner_up should be finite with 2 actions, got {runner_up_loss}, posterior: {posterior:?}"
        );
        assert!(
            runner_up_loss >= best_loss,
            "runner_up ({runner_up_loss}) should be >= best ({best_loss}), posterior: {posterior:?}"
        );
        // Verify it's the loss of the other action
        let other_loss = if best == TwoAction::Act {
            core.expected_loss_for(TwoAction::Wait)
        } else {
            core.expected_loss_for(TwoAction::Act)
        };
        assert!(
            (runner_up_loss - other_loss).abs() < f64::EPSILON,
            "runner_up ({runner_up_loss}) should equal other action loss ({other_loss}), posterior: {posterior:?}"
        );
    }

    #[test]
    fn uniform_posterior_picks_lowest_loss_action() {
        // With P(s) = 1/3 for all states, choose_action should pick
        // the action with lowest average loss across all states.
        let core = DecisionCore::new(
            &[
                (LivenessState::Alive, 1.0 / 3.0),
                (LivenessState::Flaky, 1.0 / 3.0),
                (LivenessState::Dead, 1.0 / 3.0),
            ],
            &[
                (LivenessAction::DeclareAlive, LivenessState::Alive, 0.0),
                (LivenessAction::DeclareAlive, LivenessState::Flaky, 15.0),
                (LivenessAction::DeclareAlive, LivenessState::Dead, 50.0),
                (LivenessAction::Suspect, LivenessState::Alive, 5.0),
                (LivenessAction::Suspect, LivenessState::Flaky, 2.0),
                (LivenessAction::Suspect, LivenessState::Dead, 10.0),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Alive,
                    100.0,
                ),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Flaky,
                    20.0,
                ),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Dead,
                    1.0,
                ),
            ],
            0.3,
        );

        let (best, best_loss, _) = core.choose_action();
        let posterior = core.posterior();

        // Under uniform prior, Suspect has average loss (5+2+10)/3 ≈ 5.67
        // which should be lowest. DeclareAlive = (0+15+50)/3 ≈ 21.67,
        // ReleaseReservations = (100+20+1)/3 ≈ 40.33.
        assert_eq!(
            best,
            LivenessAction::Suspect,
            "uniform prior should pick Suspect (lowest avg loss), got {best:?}, loss={best_loss}, posterior: {posterior:?}"
        );
    }

    #[test]
    fn negative_alpha_clamped_to_minimum() {
        // Alpha = -1.0 should be clamped to 0.01
        let core = DecisionCore::new(
            &[
                (LivenessState::Alive, 0.95),
                (LivenessState::Flaky, 0.04),
                (LivenessState::Dead, 0.01),
            ],
            &[
                (LivenessAction::DeclareAlive, LivenessState::Alive, 0.0),
                (LivenessAction::DeclareAlive, LivenessState::Dead, 50.0),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Alive,
                    100.0,
                ),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Dead,
                    1.0,
                ),
            ],
            -1.0,
        );

        // Verify alpha was clamped by checking posterior behavior:
        // with alpha=0.01, updates should be very slow (nearly no change).
        let mut slow_core = core;
        let before: Vec<f64> = slow_core.posterior().iter().map(|(_, p)| *p).collect();

        slow_core.update_posterior(&[(LivenessState::Alive, 0.01), (LivenessState::Dead, 10.0)]);

        let after: Vec<f64> = slow_core.posterior().iter().map(|(_, p)| *p).collect();
        let posterior = slow_core.posterior();

        // With alpha clamped to 0.01, the change should be tiny (< 0.05)
        let delta: f64 = before
            .iter()
            .zip(after.iter())
            .map(|(b, a)| (b - a).abs())
            .sum();
        assert!(
            delta < 0.05,
            "alpha=-1.0 should clamp to 0.01, making updates very slow; \
             delta={delta:.6}, posterior: {posterior:?}"
        );
    }

    #[test]
    fn empty_likelihoods_leave_posterior_unchanged() {
        let mut core = default_liveness_core();
        let before: Vec<(LivenessState, f64)> = core.posterior().to_vec();

        // Empty likelihoods: all states get default likelihood 1.0.
        // P(s) × 1.0^alpha = P(s), so posterior should not change.
        core.update_posterior(&[]);

        let after = core.posterior();
        for ((s_b, p_b), (s_a, p_a)) in before.iter().zip(after.iter()) {
            assert_eq!(s_b, s_a);
            assert!(
                (p_b - p_a).abs() < 1e-12,
                "empty likelihoods should leave posterior unchanged; \
                 state {s_b:?}: {p_b} → {p_a}, posterior: {after:?}"
            );
        }
    }

    // ── EvidenceLedger + AtcDecisionRecord edge cases (br-w5phh) ───────

    #[test]
    fn ledger_all_returns_oldest_first() {
        let mut ledger = EvidenceLedger::new(100);
        let core = default_liveness_core();

        for i in 0..5 {
            let subject = format!("Agent{i}");
            ledger.record(&test_decision(&core, &subject, i64::from(i) * 1_000_000));
        }

        let ids: Vec<u64> = ledger.all().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5], "all() should return oldest first");
    }

    #[test]
    fn ledger_latest_id_when_empty() {
        let ledger = EvidenceLedger::new(100);
        // next_id starts at 1, saturating_sub(1) = 0
        assert_eq!(
            ledger.latest_id(),
            0,
            "latest_id on empty ledger should return 0 (sentinel)"
        );
    }

    #[test]
    fn format_message_with_empty_posterior() {
        let record = AtcDecisionRecord {
            id: 1,
            claim_id: "atc-claim-1".to_string(),
            evidence_id: "atc-evidence-1".to_string(),
            trace_id: "atc-trace-1".to_string(),
            timestamp_micros: 1_000_000,
            subsystem: AtcSubsystem::Liveness,
            decision_class: "test".to_string(),
            subject: "EmptyAgent".to_string(),
            policy_id: None,
            posterior: vec![], // empty posterior
            action: "DeclareAlive".to_string(),
            expected_loss: 0.5,
            runner_up_loss: 3.0,
            loss_table: Vec::new(),
            evidence_summary: "no evidence".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            fallback_reason: None,
        };

        let msg = record.format_message();
        // Should not panic, should produce readable output
        assert!(
            msg.contains("DeclareAlive"),
            "message should contain action"
        );
        assert!(msg.contains("EmptyAgent"), "message should contain subject");
        assert!(
            msg.contains("Posterior: "),
            "message should contain Posterior label"
        );
        // With empty posterior, the posterior section should be empty but not crash
        assert!(!msg.contains("NaN"), "message should not contain NaN");
    }

    #[test]
    fn format_message_with_many_posterior_entries() {
        let posterior: Vec<(String, f64)> = (0..10).map(|i| (format!("State{i}"), 0.1)).collect();

        let record = AtcDecisionRecord {
            id: 99,
            claim_id: "atc-claim-99".to_string(),
            evidence_id: "atc-evidence-99".to_string(),
            trace_id: "atc-trace-99".to_string(),
            timestamp_micros: 5_000_000,
            subsystem: AtcSubsystem::Conflict,
            decision_class: "test".to_string(),
            subject: "BigAgent".to_string(),
            policy_id: None,
            posterior,
            action: "Ignore".to_string(),
            expected_loss: 2.5,
            runner_up_loss: 4.0,
            loss_table: Vec::new(),
            evidence_summary: "lots of states".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            fallback_reason: None,
        };

        let msg = record.format_message();
        // All 10 states should appear
        for i in 0..10 {
            assert!(
                msg.contains(&format!("P(State{i})")),
                "message should contain P(State{i}), got: {msg}"
            );
        }
        // Probability formatting
        assert!(
            msg.contains("0.10"),
            "probabilities should be formatted to 2 decimal places"
        );
    }

    #[test]
    fn ledger_capacity_one_keeps_only_last() {
        let mut ledger = EvidenceLedger::new(1);
        let core = default_liveness_core();

        for i in 0..3 {
            let subject = format!("Agent{i}");
            ledger.record(&test_decision(&core, &subject, i64::from(i) * 1_000_000));
        }

        assert_eq!(
            ledger.len(),
            1,
            "capacity-1 ledger should hold exactly 1 record"
        );
        assert!(ledger.get(1).is_none(), "first record should be evicted");
        assert!(ledger.get(2).is_none(), "second record should be evicted");
        let last = ledger.get(3).expect("third (last) record should survive");
        assert_eq!(last.subject, "Agent2", "last record should be Agent2");
        assert_eq!(
            ledger.latest_id(),
            3,
            "latest_id should be 3 after 3 insertions"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Liveness Detector (Track 2)
// ──────────────────────────────────────────────────────────────────────

/// Per-agent rhythm tracker for adaptive liveness detection.
#[derive(Debug, Clone)]
pub struct AgentRhythm {
    /// EWMA of inter-activity intervals (microseconds).
    pub avg_interval: f64,
    /// EWMA of squared deviations (variance estimate).
    pub var_interval: f64,
    /// Ring buffer of recent intervals for Hill Estimator (tail risk).
    pub recent_intervals: std::collections::VecDeque<f64>,
    /// Number of observations.
    pub observation_count: u64,
    /// EWMA decay factor (default 0.1 = ~10 sample half-life).
    pub alpha: f64,
    /// Program-type prior interval (microseconds).
    pub prior_interval: f64,
    /// Timestamp of last observed activity (microseconds).
    pub last_activity_ts: i64,
}

impl AgentRhythm {
    /// Create a new rhythm tracker with a program-type prior.
    ///
    /// `prior_interval_secs` is the expected inter-activity interval based on
    /// program type (e.g., 60s for claude-code, 120s for codex-cli).
    #[must_use]
    pub fn new(prior_interval_secs: f64) -> Self {
        let prior_micros = prior_interval_secs * 1_000_000.0;
        Self {
            avg_interval: prior_micros,
            var_interval: (prior_micros * 0.5).powi(2), // initial variance = (half the mean)²
            recent_intervals: std::collections::VecDeque::with_capacity(50),
            observation_count: 0,
            alpha: 0.1,
            prior_interval: prior_micros,
            last_activity_ts: 0,
        }
    }

    /// Record a new activity observation.
    pub fn observe(&mut self, timestamp_micros: i64) {
        if self.last_activity_ts > 0 && timestamp_micros > self.last_activity_ts {
            let delta_micros = timestamp_micros - self.last_activity_ts;
            let delta = nonnegative_i64_to_f64(delta_micros);
            let old_avg = self.effective_avg();
            self.avg_interval = self
                .alpha
                .mul_add(delta, (1.0 - self.alpha) * self.avg_interval);
            self.var_interval = self.alpha.mul_add(
                (delta - old_avg).powi(2),
                (1.0 - self.alpha) * self.var_interval,
            );
            if self.recent_intervals.len() >= 50 {
                self.recent_intervals.pop_front();
            }
            self.recent_intervals.push_back(delta);
            self.observation_count = self.observation_count.saturating_add(1);
        }
        self.last_activity_ts = self.last_activity_ts.max(timestamp_micros);
    }

    /// Effective average interval, blending observed data with the prior.
    ///
    /// For the first ~10 observations, the prior dominates.  After that,
    /// the observed average takes over.
    #[must_use]
    pub fn effective_avg(&self) -> f64 {
        let n = u64_to_f64(self.observation_count);
        let prior_weight = 3.0; // pseudo-count for the prior
        n.mul_add(self.avg_interval, prior_weight * self.prior_interval) / (n + prior_weight)
    }

    /// Standard deviation of inter-activity interval.
    #[must_use]
    pub fn std_dev(&self) -> f64 {
        self.var_interval.max(0.0).sqrt()
    }

    /// Suspicion threshold using Tail-Risk DRO (Conditional Value at Risk)
    /// via a Hill Estimator for the Pareto tail index.
    #[must_use]
    pub fn suspicion_threshold(&self, k: f64) -> f64 {
        if self.recent_intervals.len() < 10 {
            return self.effective_avg() + k * self.std_dev();
        }

        let mut sorted: Vec<f64> = self.recent_intervals.iter().copied().collect();
        sorted.sort_by(|a, b| b.total_cmp(a)); // Descending

        // Top 20% for tail estimation, clamped to valid index range
        let tail_count = (sorted.len() / 5).max(3).min(sorted.len() - 1);
        let threshold_val = sorted[tail_count].max(1.0);

        let mut sum_log_ratio = 0.0;
        for i in 0..tail_count {
            sum_log_ratio += (sorted[i] / threshold_val).ln();
        }

        let tail_index = if sum_log_ratio > 0.0 {
            (tail_count as f64) / sum_log_ratio
        } else {
            2.0
        };

        // Ensure alpha > 1.0 for valid CVaR
        let alpha = tail_index.max(1.05);

        // Map k to a quantile q (e.g. k=3 -> 0.99)
        let q = 1.0 - (0.03 / k.max(1.0));

        let tail_prob = (tail_count as f64) / (sorted.len() as f64);

        let cvar = if q >= 1.0 - tail_prob {
            let tail_q = 1.0 - (1.0 - q) / tail_prob;
            let var = threshold_val * (1.0 - tail_q).powf(-1.0 / alpha);
            var * alpha / (alpha - 1.0)
        } else {
            self.effective_avg() + k * self.std_dev()
        };

        cvar.max(self.effective_avg() + k * self.std_dev())
    }

    /// How long since the last activity (microseconds).
    #[must_use]
    pub fn silence_duration(&self, now_micros: i64) -> i64 {
        if self.last_activity_ts > 0 {
            now_micros.saturating_sub(self.last_activity_ts).max(0)
        } else {
            0
        }
    }

    /// Whether the agent has exceeded the suspicion threshold.
    #[must_use]
    pub fn is_suspicious(&self, now_micros: i64, k: f64) -> bool {
        let silence = nonnegative_i64_to_f64(self.silence_duration(now_micros));
        self.last_activity_ts > 0 && silence > self.suspicion_threshold(k)
    }
}

/// Per-agent liveness state tracked by the ATC.
#[derive(Debug, Clone)]
pub struct AgentLivenessEntry {
    /// Agent name.
    pub name: String,
    /// Project key used to route advisories/effects back into Agent Mail.
    pub project_key: Option<String>,
    /// Agent program (used for hierarchical priors and cohort updates).
    pub program: String,
    /// Current state.
    pub state: LivenessState,
    /// Rhythm tracker.
    pub rhythm: AgentRhythm,
    /// When the agent entered Suspect state (0 if not suspect).
    pub suspect_since: i64,
    /// When a health probe was last sent (0 if none outstanding).
    pub probe_sent_at: i64,
    /// SPRT log-likelihood ratio for Suspect → Dead transition.
    pub sprt_log_lr: f64,
    /// Per-agent decision core (shares loss matrix structure but
    /// maintains its own posterior).
    pub core: DecisionCore<LivenessState, LivenessAction>,
    /// Monotonic schedule version for stale-heap suppression.
    pub schedule_version: u64,
    /// When this agent should next be reevaluated.
    pub next_review_micros: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ScheduledAgentReview {
    review_at_micros: i64,
    schedule_version: u64,
    agent: String,
}

#[derive(Debug, Clone)]
struct PendingLivenessFeedback {
    action: LivenessAction,
    expected_loss: f64,
    issued_at_micros: i64,
}

/// Infer a reasonable inter-activity prior from program name.
#[must_use]
pub fn program_prior_interval_secs(program: &str) -> f64 {
    match program.to_ascii_lowercase().as_str() {
        "claude-code" | "claude_code" => 60.0,
        "codex-cli" | "codex_cli" | "codex" | "gemini-cli" | "gemini_cli" | "gemini"
        | "copilot-cli" | "copilot_cli" | "copilot" => 120.0,
        _ => 300.0, // conservative default for unknown programs
    }
}

/// The ATC agent name (for self-exclusion filtering).
pub const ATC_AGENT_NAME: &str = "AirTrafficControl";

// ──────────────────────────────────────────────────────────────────────
// Conflict Detector (Track 3)
// ──────────────────────────────────────────────────────────────────────

/// A hard conflict edge: agent `holder` has an exclusive reservation
/// overlapping with agent `blocked`.
#[derive(Debug, Clone)]
pub struct HardEdge {
    /// Agent holding the contested reservation.
    pub holder: String,
    /// Agent blocked by the holder's reservation.
    pub blocked: String,
    /// Overlapping file patterns.
    pub contested_patterns: Vec<String>,
    /// When this edge was first detected (microseconds).
    pub since: i64,
}

/// Per-project conflict graph.
#[derive(Debug, Clone, Default)]
pub struct ProjectConflictGraph {
    /// Hard edges: overlapping exclusive reservations.
    /// Keyed by holder agent name → list of edges.
    pub hard_edges: HashMap<String, Vec<HardEdge>>,
    /// Generation counter for incremental computation.
    pub generation: u64,
}

impl ProjectConflictGraph {
    fn patterns_overlap(left: &str, right: &str) -> bool {
        let left = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(left);
        let right = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(right);
        left.overlaps(&right)
    }

    fn is_wildcard_path_marker(path: &str) -> bool {
        matches!(path, "<all-active>" | "<unknown>")
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    fn record_blocking_conflict(
        &mut self,
        holder: &str,
        blocked: &str,
        requested_path: &str,
        holder_path_pattern: &str,
        since: i64,
    ) -> bool {
        if holder.is_empty() || blocked.is_empty() || holder == blocked {
            return false;
        }

        let edges = self.hard_edges.entry(holder.to_string()).or_default();
        if let Some(edge) = edges.iter_mut().find(|edge| edge.blocked == blocked) {
            let mut changed = false;
            for pattern in [requested_path, holder_path_pattern] {
                if !pattern.is_empty()
                    && !edge
                        .contested_patterns
                        .iter()
                        .any(|existing| existing == pattern)
                {
                    edge.contested_patterns.push(pattern.to_string());
                    changed = true;
                }
            }
            if since > 0 && (edge.since == 0 || since < edge.since) {
                edge.since = since;
                changed = true;
            }
            if changed {
                edge.contested_patterns.sort();
                edge.contested_patterns.dedup();
                self.bump_generation();
            }
            return changed;
        }

        let mut contested_patterns = Vec::new();
        for pattern in [requested_path, holder_path_pattern] {
            if !pattern.is_empty() {
                contested_patterns.push(pattern.to_string());
            }
        }
        contested_patterns.sort();
        contested_patterns.dedup();
        edges.push(HardEdge {
            holder: holder.to_string(),
            blocked: blocked.to_string(),
            contested_patterns,
            since,
        });
        self.bump_generation();
        true
    }

    fn clear_blocked_conflicts_for_grant(
        &mut self,
        blocked: &str,
        granted_paths: &[String],
    ) -> usize {
        if blocked.is_empty() {
            return 0;
        }
        let clear_all = granted_paths
            .iter()
            .any(|path| Self::is_wildcard_path_marker(path));
        let mut removed = 0_usize;
        self.hard_edges.retain(|_, edges| {
            edges.retain_mut(|edge| {
                if edge.blocked != blocked {
                    return true;
                }
                if clear_all {
                    removed = removed.saturating_add(edge.contested_patterns.len().max(1));
                    return false;
                }
                let before = edge.contested_patterns.len();
                edge.contested_patterns.retain(|pattern| {
                    !granted_paths
                        .iter()
                        .any(|granted| Self::patterns_overlap(pattern, granted))
                });
                removed =
                    removed.saturating_add(before.saturating_sub(edge.contested_patterns.len()));
                !edge.contested_patterns.is_empty()
            });
            !edges.is_empty()
        });
        if removed > 0 {
            self.bump_generation();
        }
        removed
    }

    fn clear_holder_conflicts_for_release(
        &mut self,
        holder: &str,
        released_paths: &[String],
    ) -> usize {
        let Some(edges) = self.hard_edges.get_mut(holder) else {
            return 0;
        };
        let clear_all = released_paths
            .iter()
            .any(|path| Self::is_wildcard_path_marker(path));
        let mut removed = 0_usize;
        edges.retain_mut(|edge| {
            if clear_all {
                removed = removed.saturating_add(edge.contested_patterns.len().max(1));
                return false;
            }
            let before = edge.contested_patterns.len();
            edge.contested_patterns.retain(|pattern| {
                !released_paths
                    .iter()
                    .any(|released| Self::patterns_overlap(pattern, released))
            });
            removed = removed.saturating_add(before.saturating_sub(edge.contested_patterns.len()));
            !edge.contested_patterns.is_empty()
        });
        let remove_holder = edges.is_empty();
        if remove_holder {
            let _ = edges;
            self.hard_edges.remove(holder);
        }
        if removed > 0 {
            self.bump_generation();
        }
        removed
    }

    fn prune_stale_edges(&mut self, cutoff_micros: i64) -> usize {
        let mut removed = 0_usize;
        self.hard_edges.retain(|_, edges| {
            let before = edges.len();
            edges.retain(|edge| edge.since == 0 || edge.since >= cutoff_micros);
            removed = removed.saturating_add(before.saturating_sub(edges.len()));
            !edges.is_empty()
        });
        if removed > 0 {
            self.bump_generation();
        }
        removed
    }

    fn remove_agent(&mut self, agent: &str) -> usize {
        let mut removed = 0_usize;
        // Remove edges where agent is the holder
        if let Some(edges) = self.hard_edges.remove(agent) {
            removed = removed.saturating_add(edges.len());
        }
        // Remove edges where agent is blocked
        self.hard_edges.retain(|_, edges| {
            let before = edges.len();
            edges.retain(|edge| edge.blocked != agent);
            removed = removed.saturating_add(before.saturating_sub(edges.len()));
            !edges.is_empty()
        });
        if removed > 0 {
            self.bump_generation();
        }
        removed
    }
}

struct TarjanScc<'a> {
    adj: &'a [Vec<usize>],
    agents: &'a [&'a str],
    index_counter: usize,
    stack: Vec<usize>,
    on_stack: Vec<bool>,
    indices: Vec<usize>,
    lowlinks: Vec<usize>,
    sccs: Vec<Vec<String>>,
}

impl<'a> TarjanScc<'a> {
    fn new(adj: &'a [Vec<usize>], agents: &'a [&'a str]) -> Self {
        let node_count = agents.len();
        Self {
            adj,
            agents,
            index_counter: 0,
            stack: Vec::new(),
            on_stack: vec![false; node_count],
            indices: vec![usize::MAX; node_count],
            lowlinks: vec![0; node_count],
            sccs: Vec::new(),
        }
    }

    fn strongconnect(&mut self, node: usize) {
        self.indices[node] = self.index_counter;
        self.lowlinks[node] = self.index_counter;
        self.index_counter += 1;
        self.stack.push(node);
        self.on_stack[node] = true;

        for &neighbor in &self.adj[node] {
            if self.indices[neighbor] == usize::MAX {
                self.strongconnect(neighbor);
                self.lowlinks[node] = self.lowlinks[node].min(self.lowlinks[neighbor]);
            } else if self.on_stack[neighbor] {
                self.lowlinks[node] = self.lowlinks[node].min(self.indices[neighbor]);
            }
        }

        if self.lowlinks[node] == self.indices[node] {
            let mut scc = Vec::new();
            while let Some(member) = self.stack.pop() {
                self.on_stack[member] = false;
                scc.push(self.agents[member].to_string());
                if member == node {
                    break;
                }
            }

            if scc.len() > 1 {
                self.sccs.push(scc);
            }
        }
    }
}

/// Find all deadlock cycles in the hard conflict graph using Tarjan's SCC.
///
/// Returns only SCCs with |V| > 1 (true cycles).  O(V+E).
#[must_use]
pub fn find_deadlock_cycles(graph: &ProjectConflictGraph) -> Vec<Vec<String>> {
    // Collect all agents that appear in the graph.
    let mut agents: Vec<&str> = Vec::new();
    let mut index_of: HashMap<&str, usize> = HashMap::new();

    for (holder, edges) in &graph.hard_edges {
        if !index_of.contains_key(holder.as_str()) {
            index_of.insert(holder.as_str(), agents.len());
            agents.push(holder.as_str());
        }
        for edge in edges {
            if !index_of.contains_key(edge.blocked.as_str()) {
                index_of.insert(edge.blocked.as_str(), agents.len());
                agents.push(edge.blocked.as_str());
            }
        }
    }

    if agents.len() < 2 {
        return Vec::new();
    }

    let n = agents.len();

    // Build adjacency list.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (holder, edges) in &graph.hard_edges {
        if let Some(&from) = index_of.get(holder.as_str()) {
            for edge in edges {
                let Some(&to) = index_of.get(edge.blocked.as_str()) else {
                    continue;
                };
                if !adj[from].contains(&to) {
                    adj[from].push(to);
                }
            }
        }
    }

    let mut tarjan = TarjanScc::new(&adj, &agents);
    for node in 0..n {
        if tarjan.indices[node] == usize::MAX {
            tarjan.strongconnect(node);
        }
    }

    tarjan.sccs
}

// ──────────────────────────────────────────────────────────────────────
// Track 2 & 3 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod liveness_tests {
    use super::*;

    #[test]
    fn new_rhythm_uses_prior() {
        let rhythm = AgentRhythm::new(60.0);
        assert!(
            (rhythm.effective_avg() - 60_000_000.0).abs() < 1.0,
            "new rhythm should use the prior interval"
        );
    }

    #[test]
    fn observe_updates_average() {
        let mut rhythm = AgentRhythm::new(60.0);
        // First observation sets the anchor
        rhythm.observe(1_000_000);
        assert_eq!(
            rhythm.observation_count, 0,
            "first observe just sets anchor"
        );

        // Second observation at +30s → delta = 30s
        rhythm.observe(31_000_000);
        assert_eq!(rhythm.observation_count, 1);
        // Average should move toward 30s from the 60s prior
        assert!(
            rhythm.effective_avg() < 60_000_000.0,
            "average should decrease toward observed 30s interval"
        );
    }

    #[test]
    fn suspicion_threshold_increases_with_variance() {
        let mut r1 = AgentRhythm::new(60.0);
        let mut r2 = AgentRhythm::new(60.0);

        // r1: consistent 60s intervals
        for i in 0..20 {
            r1.observe(i * 60_000_000);
        }
        // r2: wildly varying intervals (30s, 90s, 30s, 90s, ...)
        for i in 0..20 {
            let interval = if i % 2 == 0 { 30_000_000 } else { 90_000_000 };
            r2.observe(i * 60_000_000 + interval);
        }

        let t1 = r1.suspicion_threshold(3.0);
        let t2 = r2.suspicion_threshold(3.0);
        assert!(
            t2 > t1,
            "higher variance should produce higher suspicion threshold"
        );
    }

    #[test]
    fn is_suspicious_with_long_silence() {
        let mut rhythm = AgentRhythm::new(60.0);
        // Establish a 60s rhythm
        for i in 0..10 {
            rhythm.observe(i * 60_000_000);
        }

        let last_ts = 9 * 60_000_000;
        // 5 minutes of silence (5× the 60s avg) should be suspicious with k=3
        let now = last_ts + 300_000_000;
        assert!(
            rhythm.is_suspicious(now, 3.0),
            "5× average silence should be suspicious"
        );

        // 65 seconds of silence should NOT be suspicious
        let now = last_ts + 65_000_000;
        assert!(
            !rhythm.is_suspicious(now, 3.0),
            "slightly above average silence should not be suspicious"
        );
    }

    #[test]
    fn program_prior_selects_correct_interval() {
        assert!((program_prior_interval_secs("claude-code") - 60.0).abs() < f64::EPSILON);
        assert!((program_prior_interval_secs("codex-cli") - 120.0).abs() < f64::EPSILON);
        assert!((program_prior_interval_secs("unknown-tool") - 300.0).abs() < f64::EPSILON);
    }

    #[test]
    fn silence_duration_zero_before_first_observation() {
        let rhythm = AgentRhythm::new(60.0);
        assert_eq!(rhythm.silence_duration(1_000_000), 0);
    }

    #[test]
    fn atc_agent_name_is_consistent() {
        assert_eq!(ATC_AGENT_NAME, "AirTrafficControl");
    }

    // ── AgentRhythm boundary condition tests (br-oco5x) ───────────────

    #[test]
    fn observe_with_out_of_order_timestamps() {
        let mut rhythm = AgentRhythm::new(60.0);
        rhythm.observe(100_000_000); // anchor at 100s
        rhythm.observe(50_000_000); // earlier timestamp (clock skew)

        // Out of order should be ignored, so avg remains the prior
        assert!(
            rhythm.avg_interval >= 0.0,
            "avg_interval should not go negative after out-of-order timestamps, got {}",
            rhythm.avg_interval
        );
        assert_eq!(rhythm.last_activity_ts, 100_000_000);
    }

    #[test]
    fn observe_with_same_timestamp_twice() {
        let mut rhythm = AgentRhythm::new(60.0);
        rhythm.observe(100_000_000);
        rhythm.observe(100_000_000); // delta = 0

        // No division by zero or NaN, should just be ignored to prevent variance collapse
        assert!(
            rhythm.avg_interval.is_finite(),
            "avg should be finite with zero delta"
        );
        assert!(
            rhythm.var_interval.is_finite(),
            "var should be finite with zero delta"
        );
        assert!(rhythm.std_dev().is_finite(), "std_dev should be finite");
        assert_eq!(
            rhythm.observation_count, 0,
            "should ignore identical timestamps to prevent variance collapse"
        );
    }

    #[test]
    fn is_suspicious_before_any_observation() {
        let rhythm = AgentRhythm::new(60.0);
        // last_activity_ts = 0 (sentinel), so is_suspicious should return false
        assert!(
            !rhythm.is_suspicious(1_000_000_000, 3.0),
            "should not be suspicious before any observation (last_activity_ts = 0)"
        );
    }

    #[test]
    fn suspicion_threshold_with_zero_variance() {
        let mut rhythm = AgentRhythm::new(60.0);
        // Many observations at exactly the same interval → variance → 0
        for i in 0..100 {
            rhythm.observe(i * 60_000_000); // exactly 60s apart
        }

        let std = rhythm.std_dev();
        // After many identical intervals, variance should be very small
        // (not exactly zero due to EWMA blending with initial variance).
        let threshold = rhythm.suspicion_threshold(3.0);
        let avg = rhythm.effective_avg();

        // The first `recent_intervals < 10` branch of `suspicion_threshold`
        // returns `avg + k*std`, but with 100 observations we're firmly in
        // the Hill-estimator / CVaR branch. There, a degenerate all-equal
        // sample yields `sum_log_ratio = 0`, which drops to the fallback
        // Pareto index (alpha = 2) and produces
        // `cvar = threshold_val * (1-tail_q)^(-1/alpha) * alpha/(alpha-1)`.
        // That's intentionally much larger than `avg` — a tail-risk
        // estimator should stay conservative even when observed variance
        // collapsed to the EWMA floor. What we *can* verify is (a) the
        // threshold is finite and strictly above the mean, and (b) the
        // CVaR branch still clamps above `avg + k*std`, which is the
        // lower-bound contract in the final `.max(...)` on line 1721.
        assert!(
            threshold.is_finite(),
            "CVaR threshold must stay finite with degenerate variance (std={std})"
        );
        assert!(
            threshold > avg,
            "zero-variance threshold ({threshold}) must still exceed avg ({avg}) under the tail-risk model"
        );
        assert!(
            threshold >= avg + 3.0 * std,
            "CVaR output must remain at least as conservative as avg + k*std (threshold={threshold}, avg={avg}, std={std})"
        );

        // Silence just above the threshold should trigger suspicion — the
        // core contract this test protects.
        let last_ts = rhythm.last_activity_ts;
        let barely_above = last_ts
            .saturating_add(micros_f64_to_i64(threshold))
            .saturating_add(1);
        assert!(
            rhythm.is_suspicious(barely_above, 3.0),
            "silence just above threshold should be suspicious"
        );
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    fn empty_graph_no_cycles() {
        let graph = ProjectConflictGraph::default();
        let cycles = find_deadlock_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn single_edge_no_cycle() {
        let mut graph = ProjectConflictGraph::default();
        graph.hard_edges.insert(
            "AgentA".to_string(),
            vec![HardEdge {
                holder: "AgentA".to_string(),
                blocked: "AgentB".to_string(),
                contested_patterns: vec!["src/lib.rs".to_string()],
                since: 1,
            }],
        );
        let cycles = find_deadlock_cycles(&graph);
        assert!(cycles.is_empty(), "single directed edge is not a cycle");
    }

    #[test]
    fn two_agent_cycle_detected() {
        let mut graph = ProjectConflictGraph::default();
        graph.hard_edges.insert(
            "AgentA".to_string(),
            vec![HardEdge {
                holder: "AgentA".to_string(),
                blocked: "AgentB".to_string(),
                contested_patterns: vec!["src/lib.rs".to_string()],
                since: 1,
            }],
        );
        graph.hard_edges.insert(
            "AgentB".to_string(),
            vec![HardEdge {
                holder: "AgentB".to_string(),
                blocked: "AgentA".to_string(),
                contested_patterns: vec!["src/main.rs".to_string()],
                since: 2,
            }],
        );

        let cycles = find_deadlock_cycles(&graph);
        assert_eq!(cycles.len(), 1, "should detect exactly one cycle");
        let cycle = &cycles[0];
        assert_eq!(cycle.len(), 2, "cycle should have 2 agents");
        assert!(cycle.contains(&"AgentA".to_string()));
        assert!(cycle.contains(&"AgentB".to_string()));
    }

    #[test]
    fn three_agent_cycle_detected() {
        let mut graph = ProjectConflictGraph::default();
        graph.hard_edges.insert(
            "A".to_string(),
            vec![HardEdge {
                holder: "A".to_string(),
                blocked: "B".to_string(),
                contested_patterns: vec!["f1".to_string()],
                since: 1,
            }],
        );
        graph.hard_edges.insert(
            "B".to_string(),
            vec![HardEdge {
                holder: "B".to_string(),
                blocked: "C".to_string(),
                contested_patterns: vec!["f2".to_string()],
                since: 2,
            }],
        );
        graph.hard_edges.insert(
            "C".to_string(),
            vec![HardEdge {
                holder: "C".to_string(),
                blocked: "A".to_string(),
                contested_patterns: vec!["f3".to_string()],
                since: 3,
            }],
        );

        let cycles = find_deadlock_cycles(&graph);
        assert_eq!(cycles.len(), 1, "should detect one 3-agent cycle");
        assert_eq!(cycles[0].len(), 3);
    }

    #[test]
    fn no_cycle_in_dag() {
        // A → B → C (no back edge)
        let mut graph = ProjectConflictGraph::default();
        graph.hard_edges.insert(
            "A".to_string(),
            vec![HardEdge {
                holder: "A".to_string(),
                blocked: "B".to_string(),
                contested_patterns: vec!["f1".to_string()],
                since: 1,
            }],
        );
        graph.hard_edges.insert(
            "B".to_string(),
            vec![HardEdge {
                holder: "B".to_string(),
                blocked: "C".to_string(),
                contested_patterns: vec!["f2".to_string()],
                since: 2,
            }],
        );

        let cycles = find_deadlock_cycles(&graph);
        assert!(cycles.is_empty(), "DAG should have no cycles");
    }

    #[test]
    fn multiple_independent_cycles() {
        // Cycle 1: A ↔ B
        // Cycle 2: C ↔ D
        let mut graph = ProjectConflictGraph::default();
        for (h, b) in [("A", "B"), ("B", "A"), ("C", "D"), ("D", "C")] {
            graph
                .hard_edges
                .entry(h.to_string())
                .or_default()
                .push(HardEdge {
                    holder: h.to_string(),
                    blocked: b.to_string(),
                    contested_patterns: vec!["f".to_string()],
                    since: 1,
                });
        }

        let cycles = find_deadlock_cycles(&graph);
        assert_eq!(cycles.len(), 2, "should detect two independent cycles");
    }

    #[test]
    fn self_loop_not_a_deadlock() {
        // Agent with edge to itself — not a multi-agent deadlock
        let mut graph = ProjectConflictGraph::default();
        graph.hard_edges.insert(
            "A".to_string(),
            vec![HardEdge {
                holder: "A".to_string(),
                blocked: "A".to_string(),
                contested_patterns: vec!["f".to_string()],
                since: 1,
            }],
        );

        let cycles = find_deadlock_cycles(&graph);
        // A self-loop creates an SCC of size 1, which we filter out
        assert!(
            cycles.is_empty(),
            "self-loop should not be reported as deadlock"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Conformal Martingale Regret Engine (Track 11)
// ──────────────────────────────────────────────────────────────────────

/// Independent ONS (Online Newton Step) state for one e-process instance.
#[derive(Debug, Clone)]
struct OnsState {
    /// Running e-process value.
    e_value: f64,
    /// ONS sufficient statistics.
    sum_centered: f64,
    sum_sq: f64,
}

impl OnsState {
    const fn new() -> Self {
        Self {
            e_value: 1.0,
            sum_centered: 0.0,
            sum_sq: 0.0,
        }
    }

    /// Update this e-process with a new observation.
    fn update(&mut self, centered: f64, alpha: f64) {
        self.sum_centered += centered;
        self.sum_sq += centered * centered;

        let lambda = self.adaptive_bet_size(alpha);
        let factor = lambda.mul_add(centered, 1.0);
        // factor is guaranteed non-negative by the lambda bounds.
        // Cap at 1e100 to prevent f64 overflow during sustained miscalibration
        // (the e-value is evidence, not a probability — any value > threshold
        // is equally actionable, so capping doesn't lose information).
        self.e_value = (self.e_value * factor).clamp(1e-30, 1e100);
    }

    /// ONS adaptive bet sizing with CORRECT bounds for non-negative factors.
    ///
    /// Constraint: `1 + λ(z - α) ≥ 0` for all z ∈ {0, 1}
    ///   z=0 → λ ≤ 1/α       (upper bound)
    ///   z=1 → λ ≥ -1/(1-α)  (lower bound)
    fn adaptive_bet_size(&self, alpha: f64) -> f64 {
        let lambda_raw = self.sum_centered / (self.sum_sq + 1.0);
        let lambda_min = if alpha < 1.0 {
            -1.0 / (1.0 - alpha)
        } else {
            -100.0
        };
        let lambda_max = if alpha > 0.0 { 1.0 / alpha } else { 100.0 };
        lambda_raw.clamp(lambda_min, lambda_max)
    }
}

/// E-process martingale monitor for anytime-valid miscalibration detection.
///
/// Maintains a non-negative supermartingale `E_t` starting at `1.0` under `H0`
/// ("predictions are well-calibrated"). If `E_t >= threshold` at any time,
/// we have statistically valid evidence of miscalibration — no multiple-
/// testing correction needed.
///
/// Also maintains per-subsystem and per-agent e-processes to PINPOINT
/// which component is drifting.
#[derive(Debug, Clone)]
pub struct EProcessMonitor {
    /// Global e-process (with its own ONS state).
    global: OnsState,
    /// Target coverage rate (e.g., 0.85 = 85% accuracy).
    target_coverage: f64,
    /// Alert threshold (default 20.0 ≈ 5% significance).
    alert_threshold: f64,
    /// Per-subsystem e-processes (each with independent ONS state).
    per_subsystem: HashMap<AtcSubsystem, OnsState>,
    /// Per-agent e-processes (each with independent ONS state).
    per_agent: HashMap<String, OnsState>,
}

impl EProcessMonitor {
    /// Create a new monitor with the given coverage target and alert threshold.
    #[must_use]
    pub fn new(target_coverage: f64, alert_threshold: f64) -> Self {
        Self {
            global: OnsState::new(),
            target_coverage,
            alert_threshold,
            per_subsystem: HashMap::new(),
            per_agent: HashMap::new(),
        }
    }

    /// Update after observing a prediction outcome.
    ///
    /// Each e-process (global, per-subsystem, per-agent) maintains its own
    /// independent ONS state and bet sizing.  This prevents a drifting
    /// subsystem from contaminating the bet size for well-calibrated ones.
    pub fn update(&mut self, correct: bool, subsystem: AtcSubsystem, agent: Option<&str>) {
        let z = if correct { 0.0 } else { 1.0 };
        let alpha = 1.0 - self.target_coverage;
        let centered = z - alpha;

        // Global e-process (independent ONS)
        self.global.update(centered, alpha);

        // Per-subsystem e-process (independent ONS)
        self.per_subsystem
            .entry(subsystem)
            .or_insert_with(OnsState::new)
            .update(centered, alpha);

        // Per-agent e-process (independent ONS)
        if let Some(agent_name) = agent {
            self.per_agent
                .entry(agent_name.to_string())
                .or_insert_with(OnsState::new)
                .update(centered, alpha);
        }
    }

    /// Whether the global e-process indicates miscalibration.
    #[must_use]
    pub fn miscalibrated(&self) -> bool {
        self.global.e_value >= self.alert_threshold
    }

    /// Current global e-value.
    #[must_use]
    pub const fn e_value(&self) -> f64 {
        self.global.e_value
    }

    /// Identify which subsystems or agents are drifting.
    ///
    /// Returns `(entity, e_value)` pairs sorted by evidence strength,
    /// filtered to those above 50% of the alert threshold (early warning).
    #[must_use]
    pub fn drift_sources(&self) -> Vec<(String, f64)> {
        let early_warning = self.alert_threshold * 0.5;
        let mut sources = Vec::new();
        for (sub, ons) in &self.per_subsystem {
            if ons.e_value >= early_warning {
                sources.push((format!("subsystem:{sub}"), ons.e_value));
            }
        }
        for (agent, ons) in &self.per_agent {
            if ons.e_value >= early_warning {
                sources.push((format!("agent:{agent}"), ons.e_value));
            }
        }
        sources.sort_by(|a, b| b.1.total_cmp(&a.1));
        sources
    }
}

/// CUSUM (Cumulative Sum) change-point detector for regime shifts.
///
/// Detects when the ATC's error rate has changed meaningfully —
/// either degradation (more errors) or improvement (fewer errors).
/// This is complementary to the e-process: the e-process detects
/// *overall* miscalibration; CUSUM detects *when* behavior changed.
#[derive(Debug, Clone)]
pub struct CusumDetector {
    /// Running CUSUM statistic for upward shift (more errors).
    s_pos: f64,
    /// Running CUSUM statistic for downward shift (fewer errors).
    s_neg: f64,
    /// Expected error rate under null hypothesis.
    expected_rate: f64,
    /// Detection threshold.
    threshold: f64,
    /// Minimum shift magnitude to detect.
    delta: f64,
    /// When the current regime started (microseconds).
    regime_start: i64,
    /// History of detected regime changes.
    regime_changes: VecDeque<RegimeChange>,
    /// Maximum regime change history to retain.
    max_history: usize,
}

/// A detected regime change.
#[derive(Debug, Clone)]
pub struct RegimeChange {
    /// When the change was detected.
    pub timestamp: i64,
    /// Direction of the change.
    pub direction: ChangeDirection,
    /// CUSUM statistic at detection.
    pub cusum_value: f64,
}

/// Direction of a regime change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeDirection {
    /// Error rate increased (more errors than expected).
    Degradation,
    /// Error rate decreased (fewer errors than expected).
    Improvement,
}

impl CusumDetector {
    /// Create a new detector.
    ///
    /// - `expected_rate`: baseline error rate (e.g., 0.15 for 85% accuracy)
    /// - `threshold`: detection sensitivity (higher = fewer false alarms)
    /// - `delta`: minimum shift to detect (e.g., 0.1 = 10% shift)
    #[must_use]
    pub const fn new(expected_rate: f64, threshold: f64, delta: f64) -> Self {
        Self {
            s_pos: 0.0,
            s_neg: 0.0,
            expected_rate,
            threshold,
            delta,
            regime_start: 0,
            regime_changes: VecDeque::new(),
            max_history: 50,
        }
    }

    /// Update with a new observation.  Returns `Some(direction)` if a
    /// regime change was detected on this update.
    pub fn update(&mut self, error_occurred: bool, timestamp: i64) -> Option<ChangeDirection> {
        let x = if error_occurred { 1.0 } else { 0.0 };

        // Page's CUSUM: accumulate deviations from expected, reset at zero
        self.s_pos = (self.s_pos + x - self.expected_rate - self.delta / 2.0).max(0.0);
        self.s_neg = (self.s_neg - x + self.expected_rate - self.delta / 2.0).max(0.0);

        if self.s_pos > self.threshold {
            self.declare_regime_change(ChangeDirection::Degradation, timestamp);
            return Some(ChangeDirection::Degradation);
        }
        if self.s_neg > self.threshold {
            self.declare_regime_change(ChangeDirection::Improvement, timestamp);
            return Some(ChangeDirection::Improvement);
        }
        None
    }

    fn declare_regime_change(&mut self, direction: ChangeDirection, timestamp: i64) {
        let cusum_value = match direction {
            ChangeDirection::Degradation => self.s_pos,
            ChangeDirection::Improvement => self.s_neg,
        };
        if self.regime_changes.len() >= self.max_history {
            self.regime_changes.pop_front();
        }
        self.regime_changes.push_back(RegimeChange {
            timestamp,
            direction,
            cusum_value,
        });
        // Reset after detection
        self.s_pos = 0.0;
        self.s_neg = 0.0;
        self.regime_start = timestamp;
    }

    /// Whether a degradation has been detected since the last reset.
    #[must_use]
    pub fn degradation_detected(&self) -> bool {
        self.regime_changes
            .back()
            .is_some_and(|r| r.direction == ChangeDirection::Degradation)
    }

    /// Most recent regime changes.
    pub fn recent_changes(&self, n: usize) -> impl Iterator<Item = &RegimeChange> {
        self.regime_changes.iter().rev().take(n)
    }

    /// When the current regime started (microseconds since epoch).
    #[must_use]
    pub const fn regime_start(&self) -> i64 {
        self.regime_start
    }

    /// Total number of regime changes in the history buffer.
    #[must_use]
    pub fn regime_change_count(&self) -> usize {
        self.regime_changes.len()
    }
}

/// Counterfactual regret tracker.
///
/// For every ATC decision, computes what WOULD have happened if the ATC
/// had chosen differently.  Tracks cumulative regret per action type and
/// recent regret trend for loss matrix tuning.
#[derive(Debug, Clone)]
pub struct RegretTracker {
    /// Cumulative regret per action (`action_name` → total regret).
    cumulative_regret: HashMap<String, f64>,
    /// Recent regret entries for trend analysis.
    recent: VecDeque<RegretEntry>,
    /// Maximum recent history.
    max_recent: usize,
    /// Total regret across all actions.
    total_regret: f64,
    /// Number of outcomes recorded.
    outcome_count: u64,
}

/// A single regret observation.
#[derive(Debug, Clone)]
pub struct RegretEntry {
    /// Decision ID from the evidence ledger.
    pub decision_id: u64,
    /// Action that was chosen.
    pub chosen_action: String,
    /// Loss actually incurred (given the true state).
    pub actual_loss: f64,
    /// Best action in hindsight.
    pub best_action: String,
    /// Loss the best action would have incurred.
    pub best_loss: f64,
    /// `regret = actual_loss - best_loss` (always `>= 0`).
    pub regret: f64,
    /// Timestamp.
    pub timestamp: i64,
}

impl RegretTracker {
    /// Create a new regret tracker.
    #[must_use]
    pub fn new(max_recent: usize) -> Self {
        Self {
            cumulative_regret: HashMap::new(),
            recent: VecDeque::with_capacity(max_recent.min(256)),
            max_recent,
            total_regret: 0.0,
            outcome_count: 0,
        }
    }

    /// Record the outcome of a past decision.
    ///
    /// `chosen_action`: what the ATC did (e.g., "Suspect")
    /// `actual_loss`: loss incurred given the true state
    /// `best_action`: what the ATC should have done in hindsight
    /// `best_loss`: loss the best action would have incurred
    pub fn record_outcome(
        &mut self,
        decision_id: u64,
        chosen_action: &str,
        actual_loss: f64,
        best_action: &str,
        best_loss: f64,
        timestamp: i64,
    ) {
        let regret = (actual_loss - best_loss).max(0.0);
        self.total_regret += regret;
        self.outcome_count += 1;

        *self
            .cumulative_regret
            .entry(chosen_action.to_string())
            .or_insert(0.0) += regret;

        if self.recent.len() >= self.max_recent {
            self.recent.pop_front();
        }
        self.recent.push_back(RegretEntry {
            decision_id,
            chosen_action: chosen_action.to_string(),
            actual_loss,
            best_action: best_action.to_string(),
            best_loss,
            regret,
            timestamp,
        });
    }

    /// Average regret per decision.
    #[must_use]
    pub fn average_regret(&self) -> f64 {
        if self.outcome_count == 0 {
            0.0
        } else {
            self.total_regret / u64_to_f64(self.outcome_count)
        }
    }

    /// Average regret over the recent window only.
    #[must_use]
    pub fn recent_average_regret(&self) -> f64 {
        if self.recent.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.recent.iter().map(|r| r.regret).sum();
        sum / usize_to_f64(self.recent.len())
    }

    /// Which actions have the highest cumulative regret (candidates for
    /// loss matrix adjustment).
    #[must_use]
    pub fn worst_actions(&self, n: usize) -> Vec<(String, f64)> {
        let mut sorted: Vec<(String, f64)> = self
            .cumulative_regret
            .iter()
            .map(|(a, r)| (a.clone(), *r))
            .collect();
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1));
        sorted.truncate(n);
        sorted
    }

    /// Total number of outcomes recorded.
    #[must_use]
    pub const fn outcome_count(&self) -> u64 {
        self.outcome_count
    }
}

// ──────────────────────────────────────────────────────────────────────
// Track 11 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod martingale_tests {
    use super::*;

    // ── E-Process tests ──────────────────────────────────────────────

    #[test]
    fn eprocess_starts_at_one() {
        let monitor = EProcessMonitor::new(0.85, 20.0);
        assert!((monitor.e_value() - 1.0).abs() < f64::EPSILON);
        assert!(!monitor.miscalibrated());
    }

    #[test]
    fn eprocess_stays_below_threshold_under_good_calibration() {
        let mut monitor = EProcessMonitor::new(0.85, 20.0);
        // Simulate 85% accuracy (well-calibrated)
        for i in 0..200 {
            let correct = i % 7 != 0; // ~85.7% correct
            monitor.update(correct, AtcSubsystem::Liveness, Some("AgentA"));
        }
        assert!(
            !monitor.miscalibrated(),
            "well-calibrated predictions should not trigger alert, e={:.2}",
            monitor.e_value()
        );
    }

    #[test]
    fn eprocess_detects_sustained_miscalibration() {
        let mut monitor = EProcessMonitor::new(0.85, 20.0);
        // Simulate 50% accuracy (badly miscalibrated)
        for i in 0..200 {
            let correct = i % 2 == 0; // 50% correct
            monitor.update(correct, AtcSubsystem::Liveness, Some("AgentA"));
        }
        assert!(
            monitor.miscalibrated(),
            "50% accuracy should trigger alert, e={:.2}",
            monitor.e_value()
        );
    }

    #[test]
    fn eprocess_per_subsystem_identifies_drifting_component() {
        let mut monitor = EProcessMonitor::new(0.85, 20.0);
        // Liveness predictions: mostly wrong
        for _ in 0..100 {
            monitor.update(false, AtcSubsystem::Liveness, None);
        }
        // Conflict predictions: mostly right
        for i in 0..100 {
            monitor.update(i % 10 != 0, AtcSubsystem::Conflict, None);
        }

        let sources = monitor.drift_sources();
        assert!(!sources.is_empty(), "should identify drift source");
        // Liveness should have higher e-value than conflict
        let liveness_e = monitor
            .per_subsystem
            .get(&AtcSubsystem::Liveness)
            .map_or(0.0, |o| o.e_value);
        let conflict_e = monitor
            .per_subsystem
            .get(&AtcSubsystem::Conflict)
            .map_or(0.0, |o| o.e_value);
        assert!(
            liveness_e > conflict_e,
            "liveness (all wrong) should have higher e-value than conflict (mostly right)"
        );
    }

    #[test]
    fn eprocess_per_agent_identifies_problematic_agent() {
        let mut monitor = EProcessMonitor::new(0.85, 20.0);
        // AgentA: all wrong, AgentB: all right
        for _ in 0..50 {
            monitor.update(false, AtcSubsystem::Liveness, Some("AgentA"));
            monitor.update(true, AtcSubsystem::Liveness, Some("AgentB"));
        }

        let wrong_prediction_e_value = monitor.per_agent.get("AgentA").map_or(0.0, |o| o.e_value);
        let correct_prediction_e_value = monitor.per_agent.get("AgentB").map_or(0.0, |o| o.e_value);
        assert!(
            wrong_prediction_e_value > correct_prediction_e_value,
            "AgentA (all wrong) should have higher e-value than AgentB (all right)"
        );
    }

    #[test]
    fn eprocess_values_stay_finite() {
        let mut monitor = EProcessMonitor::new(0.85, 20.0);
        for i in 0..10000 {
            monitor.update(i % 3 == 0, AtcSubsystem::Liveness, None);
        }
        assert!(monitor.e_value().is_finite(), "e-value must stay finite");
        assert!(monitor.e_value() > 0.0, "e-value must stay positive");
    }

    // ── CUSUM tests ──────────────────────────────────────────────────

    #[test]
    fn cusum_no_alarm_under_stationary_errors() {
        let mut cusum = CusumDetector::new(0.15, 5.0, 0.1);
        // Simulate stationary 15% error rate
        for i in 0..500 {
            let error = i % 7 == 0; // ~14.3% error rate
            let result = cusum.update(error, i * 1_000_000);
            if i < 100 {
                assert!(
                    result.is_none(),
                    "early stationary phase should not alarm at i={i}"
                );
            }
        }
    }

    #[test]
    fn cusum_detects_degradation() {
        let mut cusum = CusumDetector::new(0.15, 5.0, 0.1);
        // Start with normal 15% error rate
        for i in 0..50 {
            cusum.update(i % 7 == 0, i * 1_000_000);
        }
        // Shift to 50% error rate
        let mut detected = false;
        for i in 50..200 {
            let result = cusum.update(i % 2 == 0, i * 1_000_000);
            if result == Some(ChangeDirection::Degradation) {
                detected = true;
                break;
            }
        }
        assert!(detected, "should detect degradation after error rate shift");
    }

    #[test]
    fn cusum_detects_improvement() {
        let mut cusum = CusumDetector::new(0.50, 5.0, 0.1);
        // Start with 50% error rate
        for i in 0..50 {
            cusum.update(i % 2 == 0, i * 1_000_000);
        }
        // Shift to 5% error rate
        let mut detected = false;
        for i in 50..200 {
            let result = cusum.update(i % 20 == 0, i * 1_000_000);
            if result == Some(ChangeDirection::Improvement) {
                detected = true;
                break;
            }
        }
        assert!(detected, "should detect improvement after error rate drop");
    }

    #[test]
    fn cusum_resets_after_detection() {
        let mut cusum = CusumDetector::new(0.15, 5.0, 0.1);
        // Feed errors until first detection fires, then stop
        let mut detected = false;
        for i in 0..100 {
            if cusum.update(true, i * 1_000_000) == Some(ChangeDirection::Degradation) {
                detected = true;
                break;
            }
        }
        assert!(detected, "should detect degradation");
        assert!(!cusum.regime_changes.is_empty());

        // Immediately after detection, CUSUM statistics are reset
        assert!(
            cusum.s_pos.abs() < f64::EPSILON,
            "CUSUM should reset s_pos after detection, got {}",
            cusum.s_pos
        );
    }

    // ── Regret Tracker tests ─────────────────────────────────────────

    #[test]
    fn regret_tracker_zero_when_optimal() {
        let mut tracker = RegretTracker::new(100);
        // Every decision was optimal (actual = best)
        for i in 0_i64..10 {
            tracker.record_outcome(
                u64::try_from(i).unwrap_or(0),
                "DeclareAlive",
                0.0,
                "DeclareAlive",
                0.0,
                i,
            );
        }
        assert!(
            tracker.average_regret().abs() < f64::EPSILON,
            "regret should be zero when all decisions were optimal"
        );
    }

    #[test]
    fn regret_tracker_positive_when_suboptimal() {
        let mut tracker = RegretTracker::new(100);
        // Chose Suspect (loss=8) but should have chosen DeclareAlive (loss=0)
        tracker.record_outcome(1, "Suspect", 8.0, "DeclareAlive", 0.0, 1);
        assert!(
            (tracker.average_regret() - 8.0).abs() < f64::EPSILON,
            "regret should be 8.0"
        );
    }

    #[test]
    fn regret_tracker_worst_actions() {
        let mut tracker = RegretTracker::new(100);
        tracker.record_outcome(1, "Suspect", 8.0, "DeclareAlive", 0.0, 1);
        tracker.record_outcome(2, "Suspect", 8.0, "DeclareAlive", 0.0, 2);
        tracker.record_outcome(3, "Release", 100.0, "DeclareAlive", 0.0, 3);

        let worst = tracker.worst_actions(2);
        assert_eq!(worst[0].0, "Release", "Release should be worst action");
        assert_eq!(worst[1].0, "Suspect", "Suspect should be second worst");
    }

    #[test]
    fn regret_tracker_recent_window() {
        let mut tracker = RegretTracker::new(3);
        for i in 0_u32..5 {
            tracker.record_outcome(
                u64::from(i),
                "A",
                f64::from(i) * 2.0,
                "B",
                0.0,
                i64::from(i),
            );
        }
        assert_eq!(tracker.recent.len(), 3, "should cap at max_recent");
        // Recent window should contain the last 3 entries (ids 2,3,4)
        let recent_ids: Vec<u64> = tracker.recent.iter().map(|r| r.decision_id).collect();
        assert_eq!(recent_ids, vec![2, 3, 4]);
    }

    #[test]
    fn regret_never_negative() {
        let mut tracker = RegretTracker::new(100);
        // actual_loss < best_loss (impossible in theory, but test the clamp)
        tracker.record_outcome(1, "A", 1.0, "B", 5.0, 1);
        assert!(
            tracker.average_regret() >= 0.0,
            "regret should be clamped to non-negative"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Calibration Guard (Track 5)
// ──────────────────────────────────────────────────────────────────────

/// Safe mode policy that consumes signals from the martingale engine
/// (Track 11) and decides when to enter/exit conservative mode.
///
/// Track 5 owns the POLICY (when to switch modes).
/// Track 11 owns the EVIDENCE (e-process, CUSUM, regret).
#[derive(Debug, Clone)]
pub struct CalibrationGuard {
    /// Whether safe mode is currently active.
    safe_mode: bool,
    /// When safe mode was last activated (0 if never).
    safe_mode_since: i64,
    /// Consecutive correct predictions since last incorrect (for recovery).
    consecutive_correct: u64,
    /// Required consecutive correct predictions to exit safe mode.
    recovery_count: u64,
}

impl CalibrationGuard {
    /// Create a new calibration guard.
    #[must_use]
    pub const fn new(recovery_count: u64) -> Self {
        Self {
            safe_mode: false,
            safe_mode_since: 0,
            consecutive_correct: 0,
            recovery_count,
        }
    }

    /// Update the guard based on the latest martingale engine signals.
    ///
    /// Returns `true` if the safe mode state changed (entered or exited).
    pub fn update(
        &mut self,
        eprocess: &EProcessMonitor,
        cusum: &CusumDetector,
        prediction_correct: bool,
        timestamp_micros: i64,
    ) -> bool {
        let was_safe = self.safe_mode;

        // Track consecutive correct predictions for recovery
        if prediction_correct {
            self.consecutive_correct = self.consecutive_correct.saturating_add(1);
        } else {
            self.consecutive_correct = 0;
        }

        // Enter safe mode if ANY of these signals fire:
        // 1. E-process exceeds threshold (sustained miscalibration)
        // 2. CUSUM detected a degradation regime change
        if !self.safe_mode && (eprocess.miscalibrated() || cusum.degradation_detected()) {
            self.safe_mode = true;
            self.safe_mode_since = timestamp_micros;
            self.consecutive_correct = 0;
        }

        // Exit safe mode only when ALL of:
        // 1. Enough consecutive correct predictions (hysteresis)
        // 2. E-process has recovered (below threshold)
        // 3. CUSUM not showing active degradation
        if self.safe_mode
            && self.consecutive_correct >= self.recovery_count
            && !eprocess.miscalibrated()
            && !cusum.degradation_detected()
        {
            self.safe_mode = false;
            self.safe_mode_since = 0;
        }

        self.safe_mode != was_safe
    }

    /// Whether safe mode is currently active.
    #[must_use]
    pub const fn is_safe_mode(&self) -> bool {
        self.safe_mode
    }

    /// When safe mode was last activated (0 if never or not active).
    #[must_use]
    pub const fn safe_mode_since(&self) -> i64 {
        self.safe_mode_since
    }

    /// How many consecutive correct predictions since the last incorrect.
    #[must_use]
    pub const fn consecutive_correct(&self) -> u64 {
        self.consecutive_correct
    }

    /// Required consecutive correct predictions to exit safe mode.
    #[must_use]
    pub const fn recovery_count(&self) -> u64 {
        self.recovery_count
    }

    /// Force safe mode on/off (operator override).
    pub const fn set_safe_mode(&mut self, active: bool, timestamp_micros: i64) {
        self.safe_mode = active;
        self.safe_mode_since = if active { timestamp_micros } else { 0 };
        if active {
            self.consecutive_correct = 0;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Engine Integration (Track 8) — AtcEngine
// ──────────────────────────────────────────────────────────────────────

/// Configuration for the ATC engine.
#[derive(Debug, Clone)]
pub struct AtcConfig {
    /// Master switch.
    pub enabled: bool,
    /// Optional JSON policy bundle path for policy-as-data execution.
    pub policy_bundle_path: Option<String>,
    /// Health probe interval (microseconds).
    pub probe_interval_micros: i64,
    /// Minimum interval between advisories to the same agent (microseconds).
    pub advisory_cooldown_micros: i64,
    /// Session summary posting interval (microseconds).
    pub summary_interval_micros: i64,
    /// Calibration guard recovery count.
    pub safe_mode_recovery_count: u64,
    /// E-process alert threshold.
    pub eprocess_alert_threshold: f64,
    /// CUSUM detection threshold.
    pub cusum_threshold: f64,
    /// CUSUM minimum shift to detect.
    pub cusum_delta: f64,
    /// Evidence ledger ring buffer capacity.
    pub ledger_capacity: usize,
    /// Tick budget (microseconds) — warn if exceeded.
    pub tick_budget_micros: u64,
    /// Suspicion k-factor for rhythm-based detection.
    pub suspicion_k: f64,
}

impl Default for AtcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy_bundle_path: None,
            probe_interval_micros: 120 * 1_000_000,
            advisory_cooldown_micros: 300 * 1_000_000,
            summary_interval_micros: 300 * 1_000_000,
            safe_mode_recovery_count: 20,
            eprocess_alert_threshold: 20.0,
            cusum_threshold: 5.0,
            cusum_delta: 0.1,
            ledger_capacity: 1000,
            tick_budget_micros: 5_000,
            suspicion_k: 3.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub enum AtcBudgetMode {
    #[default]
    Nominal,
    Pressure,
    Conservative,
}

impl AtcBudgetMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Pressure => "pressure",
            Self::Conservative => "conservative",
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AtcStageTimings {
    pub liveness_micros: u64,
    pub deadlock_micros: u64,
    pub probe_micros: u64,
    pub gating_micros: u64,
    pub slow_control_micros: u64,
    pub summary_micros: u64,
    pub total_micros: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AtcKernelTelemetry {
    pub due_agents: usize,
    pub scheduled_agents: usize,
    pub next_due_micros: Option<i64>,
    pub dirty_agents: usize,
    pub dirty_projects: usize,
    pub pending_effects: usize,
    pub lock_wait_micros: u64,
    pub deadlock_cache_hits: u64,
    pub deadlock_cache_misses: u64,
    pub deadlock_cache_hit_rate: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AtcBudgetTelemetry {
    pub mode: String,
    pub tick_budget_micros: u64,
    pub probe_budget_micros: u64,
    pub estimated_probe_cost_micros: u64,
    pub max_probes_this_tick: usize,
    pub budget_debt_micros: u64,
    pub utilization_ratio: f64,
    pub slow_window_utilization: f64,
    pub kernel_total_micros: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AtcPolicyTelemetry {
    pub bundle_id: String,
    pub bundle_hash: String,
    pub incumbent_policy_id: String,
    pub incumbent_artifact_hash: String,
    pub candidate_policy_id: Option<String>,
    pub candidate_artifact_hash: Option<String>,
    pub shadow_enabled: bool,
    pub shadow_disagreements: u64,
    pub shadow_regret_avg: f64,
    pub decision_mode: String,
    pub fallback_active: bool,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AtcLivenessPolicyArtifact {
    schema_version: u32,
    policy_id: String,
    artifact_hash: String,
    suspicion_k: f64,
    max_probes_per_tick: usize,
    probe_recency_decay_secs: f64,
    probe_gain_floor: f64,
    probe_budget_fraction: f64,
    conservative_probe_budget_fraction: f64,
    release_guard_enabled: bool,
    losses: [[f64; 3]; 3],
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AtcLivenessPolicyBundle {
    schema_version: u32,
    bundle_id: String,
    bundle_hash: String,
    incumbent: AtcLivenessPolicyArtifact,
    candidate: Option<AtcLivenessPolicyArtifact>,
}

impl AtcLivenessPolicyArtifact {
    fn compute_artifact_hash(&self) -> String {
        // IMPORTANT: format every f64 with a fixed number of decimals instead
        // of using Debug/Display. The candidate bundle derives some losses
        // via binary-inexact multiplications (e.g. `6.0 * 0.9`), and both
        // `{:?}` and `{}` render those values at their shortest-round-trip
        // form — which can flip by 1 ULP across a JSON serialize/deserialize
        // pair. That would turn a perfectly round-trippable bundle into a
        // `invalid_candidate_policy_hash` on load. A fixed-precision
        // formatter (6 decimals is what all the other hashed scalar fields
        // already use) keeps the hash input stable across the pipeline.
        use std::fmt::Write as _;
        let mut losses_str = String::with_capacity(64);
        losses_str.push('[');
        for (i, row) in self.losses.iter().enumerate() {
            if i > 0 {
                losses_str.push(';');
            }
            losses_str.push('[');
            for (j, v) in row.iter().enumerate() {
                if j > 0 {
                    losses_str.push(',');
                }
                let _ = write!(losses_str, "{v:.6}");
            }
            losses_str.push(']');
        }
        losses_str.push(']');
        let serialized = format!(
            "{}|{}|{:.6}|{}|{:.6}|{:.6}|{:.6}|{:.6}|{}|{}",
            self.schema_version,
            self.policy_id,
            self.suspicion_k,
            self.max_probes_per_tick,
            self.probe_recency_decay_secs,
            self.probe_gain_floor,
            self.probe_budget_fraction,
            self.conservative_probe_budget_fraction,
            self.release_guard_enabled,
            losses_str,
        );
        format!("{:016x}", stable_fnv1a64(serialized.as_bytes()))
    }

    fn finalize(mut self) -> Self {
        self.artifact_hash = self.compute_artifact_hash();
        self
    }

    fn apply_to_core(&self, core: &mut DecisionCore<LivenessState, LivenessAction>) {
        for (action_idx, action) in [
            LivenessAction::DeclareAlive,
            LivenessAction::Suspect,
            LivenessAction::ReleaseReservations,
        ]
        .into_iter()
        .enumerate()
        {
            for (state_idx, state) in [
                LivenessState::Alive,
                LivenessState::Flaky,
                LivenessState::Dead,
            ]
            .into_iter()
            .enumerate()
            {
                core.set_loss_entry(action, state, self.losses[action_idx][state_idx]);
            }
        }
    }

    #[must_use]
    fn from_core(
        policy_id: String,
        core: &DecisionCore<LivenessState, LivenessAction>,
        base_k: f64,
    ) -> Self {
        let mut losses = [[0.0; 3]; 3];
        for (action_idx, action) in [
            LivenessAction::DeclareAlive,
            LivenessAction::Suspect,
            LivenessAction::ReleaseReservations,
        ]
        .into_iter()
        .enumerate()
        {
            for (state_idx, state) in [
                LivenessState::Alive,
                LivenessState::Flaky,
                LivenessState::Dead,
            ]
            .into_iter()
            .enumerate()
            {
                losses[action_idx][state_idx] = core.loss_entry(action, state);
            }
        }
        Self {
            schema_version: 1,
            policy_id,
            artifact_hash: String::new(),
            suspicion_k: base_k,
            max_probes_per_tick: 3,
            probe_recency_decay_secs: 60.0,
            probe_gain_floor: 0.01,
            probe_budget_fraction: 0.55,
            conservative_probe_budget_fraction: 0.25,
            release_guard_enabled: true,
            losses,
        }
        .finalize()
    }

    #[must_use]
    fn candidate_from_incumbent(incumbent: &Self) -> Self {
        let mut candidate = incumbent.clone();
        candidate.policy_id = "liveness-shadow-cautious-v1".to_string();
        candidate.suspicion_k += 0.35;
        candidate.max_probes_per_tick = candidate.max_probes_per_tick.saturating_sub(1).max(1);
        candidate.probe_recency_decay_secs = 45.0;
        candidate.probe_gain_floor = 0.02;
        candidate.probe_budget_fraction = 0.35;
        candidate.conservative_probe_budget_fraction = 0.18;
        candidate.losses[2][0] *= 1.15;
        candidate.losses[2][1] *= 1.35;
        candidate.losses[1][2] *= 0.9;
        candidate.finalize()
    }

    #[must_use]
    fn max_probes(&self, mode: AtcBudgetMode) -> usize {
        match mode {
            AtcBudgetMode::Nominal => self.max_probes_per_tick,
            AtcBudgetMode::Pressure => self.max_probes_per_tick.min(2),
            AtcBudgetMode::Conservative => self.max_probes_per_tick.min(1),
        }
    }

    #[must_use]
    fn probe_budget_fraction(&self, mode: AtcBudgetMode) -> f64 {
        match mode {
            AtcBudgetMode::Nominal => self.probe_budget_fraction,
            AtcBudgetMode::Pressure => (self.probe_budget_fraction * 0.75).clamp(0.05, 1.0),
            AtcBudgetMode::Conservative => self.conservative_probe_budget_fraction,
        }
    }

    #[must_use]
    fn expected_loss(&self, action: LivenessAction, posterior: &[(LivenessState, f64)]) -> f64 {
        let action_idx = match action {
            LivenessAction::DeclareAlive => 0,
            LivenessAction::Suspect => 1,
            LivenessAction::ReleaseReservations => 2,
        };
        posterior.iter().fold(0.0, |acc, (state, probability)| {
            let state_idx = match state {
                LivenessState::Alive => 0,
                LivenessState::Flaky => 1,
                LivenessState::Dead => 2,
            };
            acc + (*probability * self.losses[action_idx][state_idx])
        })
    }

    #[must_use]
    fn choose_action(
        &self,
        posterior: &[(LivenessState, f64)],
        release_guard_active: bool,
    ) -> (LivenessAction, f64, f64) {
        let mut best_action = LivenessAction::DeclareAlive;
        let mut best_loss = f64::INFINITY;
        let mut runner_up = f64::INFINITY;
        for action in [
            LivenessAction::DeclareAlive,
            LivenessAction::Suspect,
            LivenessAction::ReleaseReservations,
        ] {
            if release_guard_active
                && self.release_guard_enabled
                && action == LivenessAction::ReleaseReservations
            {
                continue;
            }
            let expected_loss = self.expected_loss(action, posterior);
            if expected_loss < best_loss {
                runner_up = best_loss;
                best_loss = expected_loss;
                best_action = action;
            } else if expected_loss < runner_up {
                runner_up = expected_loss;
            }
        }
        (best_action, best_loss, runner_up)
    }
}

impl AtcLivenessPolicyBundle {
    fn load_from_path(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("read bundle '{}': {error}", path.display()))?;
        let bundle: Self = serde_json::from_str(&raw)
            .map_err(|error| format!("parse bundle '{}': {error}", path.display()))?;
        bundle
            .validate()
            .map_err(|error| format!("validate bundle '{}': {error}", path.display()))?;
        Ok(bundle)
    }

    fn compute_bundle_hash(&self) -> String {
        let serialized = format!(
            "{}|{}|{}|{}|{}",
            self.schema_version,
            self.bundle_id,
            self.incumbent.artifact_hash,
            self.candidate
                .as_ref()
                .map_or("-", |candidate| candidate.artifact_hash.as_str()),
            self.incumbent.policy_id,
        );
        format!("{:016x}", stable_fnv1a64(serialized.as_bytes()))
    }

    fn finalize(mut self) -> Self {
        self.bundle_hash = self.compute_bundle_hash();
        self
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version == 0 || self.bundle_id.is_empty() {
            return Err("invalid_policy_bundle_metadata");
        }
        if self.incumbent.compute_artifact_hash() != self.incumbent.artifact_hash {
            return Err("invalid_incumbent_policy_hash");
        }
        if let Some(candidate) = self.candidate.as_ref()
            && candidate.compute_artifact_hash() != candidate.artifact_hash
        {
            return Err("invalid_candidate_policy_hash");
        }
        if self.compute_bundle_hash() != self.bundle_hash {
            return Err("invalid_policy_bundle_hash");
        }
        Ok(())
    }

    fn from_live_policies(
        incumbent: &AtcLivenessPolicyArtifact,
        candidate: Option<&AtcLivenessPolicyArtifact>,
        revision: u64,
    ) -> Self {
        Self {
            schema_version: 1,
            bundle_id: format!("atc-liveness-bundle-r{revision}"),
            bundle_hash: String::new(),
            incumbent: incumbent.clone(),
            candidate: candidate.cloned(),
        }
        .finalize()
    }
}

#[derive(Debug, Clone, Default)]
struct DeadlockCacheEntry {
    generation: u64,
    cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
struct AtcCostModel {
    liveness_avg_micros: f64,
    deadlock_avg_micros: f64,
    gating_avg_micros: f64,
    summary_avg_micros: f64,
    per_probe_avg_micros: f64,
    alpha: f64,
}

impl Default for AtcCostModel {
    fn default() -> Self {
        Self {
            liveness_avg_micros: 300.0,
            deadlock_avg_micros: 200.0,
            gating_avg_micros: 80.0,
            summary_avg_micros: 120.0,
            per_probe_avg_micros: 120.0,
            alpha: 0.2,
        }
    }
}

impl AtcCostModel {
    fn update(&mut self, timings: &AtcStageTimings, probes_selected: usize) {
        self.liveness_avg_micros = self.alpha.mul_add(
            u64_to_f64(timings.liveness_micros),
            (1.0 - self.alpha) * self.liveness_avg_micros,
        );
        self.deadlock_avg_micros = self.alpha.mul_add(
            u64_to_f64(timings.deadlock_micros),
            (1.0 - self.alpha) * self.deadlock_avg_micros,
        );
        self.gating_avg_micros = self.alpha.mul_add(
            u64_to_f64(timings.gating_micros),
            (1.0 - self.alpha) * self.gating_avg_micros,
        );
        self.summary_avg_micros = self.alpha.mul_add(
            u64_to_f64(timings.summary_micros),
            (1.0 - self.alpha) * self.summary_avg_micros,
        );
        if probes_selected > 0 {
            let per_probe = u64_to_f64(timings.probe_micros) / usize_to_f64(probes_selected);
            self.per_probe_avg_micros = self.alpha.mul_add(
                per_probe.max(1.0),
                (1.0 - self.alpha) * self.per_probe_avg_micros,
            );
        }
    }

    #[must_use]
    fn estimated_non_probe_micros(&self) -> u64 {
        micros_f64_to_i64(
            self.liveness_avg_micros
                + self.deadlock_avg_micros
                + self.gating_avg_micros
                + self.summary_avg_micros,
        )
        .max(0)
        .try_into()
        .unwrap_or(u64::MAX)
    }

    #[must_use]
    fn estimated_probe_cost_micros(&self) -> u64 {
        micros_f64_to_i64(self.per_probe_avg_micros.max(1.0))
            .max(1)
            .try_into()
            .unwrap_or(1)
    }
}

#[derive(Debug, Clone)]
struct AtcSlowControllerState {
    pub probe_budget_fraction: f64,
    pub probe_limit: usize,
    budget_debt_micros: f64,
    integral_error: f64,
    prev_error: f64,
    kp: f64,
    ki: f64,
    last_utilization_ratio: f64,
}

impl Default for AtcSlowControllerState {
    fn default() -> Self {
        Self {
            probe_budget_fraction: 0.5,
            probe_limit: 3,
            budget_debt_micros: 0.0,
            integral_error: 0.0,
            prev_error: 0.0,
            kp: 0.1,
            ki: 0.05,
            last_utilization_ratio: 0.0,
        }
    }
}

impl AtcSlowControllerState {
    fn note_tick(
        &mut self,
        total_micros: u64,
        tick_budget_micros: u64,
        utilization_ratio: f64,
        budget_exceeded: bool,
        baseline_probe_limit: usize,
    ) {
        self.last_utilization_ratio = utilization_ratio;
        let debt_delta = i64::try_from(total_micros).unwrap_or(i64::MAX)
            - i64::try_from(tick_budget_micros).unwrap_or(i64::MAX);
        let next_debt = self.budget_debt_micros + nonnegative_i64_to_f64(debt_delta);
        self.budget_debt_micros = if debt_delta < 0 {
            (self.budget_debt_micros - nonnegative_i64_to_f64(-debt_delta)).max(0.0)
        } else {
            next_debt
        };

        // PI Controller logic
        // Target: utilization < 0.75 and minimal debt
        let target_utilization = 0.75;
        let debt_ratio = self.budget_debt_micros / u64_to_f64(tick_budget_micros.max(1));

        // Error is positive when we have slack (under-utilized, no debt)
        // Error is negative when overloaded (over-utilized, high debt)
        let mut error = target_utilization - utilization_ratio - (0.5 * debt_ratio);
        if budget_exceeded {
            error -= 0.5; // Strong penalty for exceeding hard budget
        }

        self.integral_error = (self.integral_error + error).clamp(-10.0, 10.0);

        let p_term = self.kp * error;
        let i_term = self.ki * self.integral_error;
        let nominal_bias = 0.5;

        // Update fraction with PI output (position form)
        self.probe_budget_fraction = (nominal_bias + p_term + i_term).clamp(0.05, 1.0);
        // Map fraction to discrete probe limit
        let float_limit = (self.probe_budget_fraction * usize_to_f64(baseline_probe_limit)).round();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.probe_limit = (float_limit as usize).clamp(1, baseline_probe_limit.max(1));
        }

        self.prev_error = error;
    }

    #[must_use]
    fn budget_debt_micros(&self) -> u64 {
        floor_f64_to_u64(self.budget_debt_micros)
    }
}

#[derive(Debug, Clone)]
struct AtcShadowPolicyState {
    candidate: Option<AtcLivenessPolicyArtifact>,
    disagreements: u64,
    regret_samples: u64,
    total_estimated_regret: f64,
    recent_estimated_regret: VecDeque<f64>,
}

impl Default for AtcShadowPolicyState {
    fn default() -> Self {
        Self {
            candidate: None,
            disagreements: 0,
            regret_samples: 0,
            total_estimated_regret: 0.0,
            recent_estimated_regret: VecDeque::with_capacity(64),
        }
    }
}

impl AtcShadowPolicyState {
    fn record_decision_pair(
        &mut self,
        incumbent_action: LivenessAction,
        candidate_action: LivenessAction,
        incumbent_loss: f64,
        candidate_loss: f64,
    ) {
        self.regret_samples = self.regret_samples.saturating_add(1);
        if incumbent_action != candidate_action {
            self.disagreements = self.disagreements.saturating_add(1);
        }
        let estimated_regret = (incumbent_loss - candidate_loss).max(0.0);
        self.total_estimated_regret += estimated_regret;
        if self.recent_estimated_regret.len() >= 64 {
            self.recent_estimated_regret.pop_front();
        }
        self.recent_estimated_regret.push_back(estimated_regret);
    }

    fn record_probe_disagreement(
        &mut self,
        incumbent_agents: &[String],
        candidate_agents: &[String],
    ) {
        if incumbent_agents != candidate_agents {
            self.disagreements = self.disagreements.saturating_add(1);
        }
    }

    #[must_use]
    fn average_regret(&self) -> f64 {
        if self.regret_samples == 0 {
            0.0
        } else {
            self.total_estimated_regret / u64_to_f64(self.regret_samples)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AtcEffectSemantics {
    pub family: String,
    pub risk_level: String,
    pub utility_model: String,
    pub operator_action: String,
    pub remediation: String,
    pub escalation_policy: String,
    pub evidence_summary: String,
    pub cooldown_key: String,
    pub cooldown_micros: i64,
    pub requires_project: bool,
    pub ack_required: bool,
    pub high_risk_intervention: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AtcEffectPlan {
    pub decision_id: u64,
    pub effect_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experience_id: Option<u64>,
    pub claim_id: String,
    pub evidence_id: String,
    pub trace_id: String,
    pub timestamp_micros: i64,
    pub kind: String,
    pub category: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    pub policy_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_loss: Option<f64>,
    pub semantics: AtcEffectSemantics,
}

#[derive(Debug, Clone)]
pub struct AtcTickReport {
    pub actions: Vec<AtcTickAction>,
    pub effects: Vec<AtcEffectPlan>,
    pub summary: AtcSummarySnapshot,
}

/// Top-level ATC engine that orchestrates all subsystems.
#[derive(Debug)]
pub struct AtcEngine {
    /// Configuration.
    config: AtcConfig,
    /// Whether the ATC has registered itself as an agent.
    registered: bool,
    /// Liveness decision core.
    #[allow(dead_code)]
    liveness_core: DecisionCore<LivenessState, LivenessAction>,
    /// Conflict decision core.
    #[allow(dead_code)]
    conflict_core: DecisionCore<ConflictState, ConflictAction>,
    /// Per-agent liveness tracking.
    agents: HashMap<String, AgentLivenessEntry>,
    /// Cached deterministic agent ordering for summary rendering.
    sorted_agent_names: Vec<String>,
    /// Whether the cached agent ordering must be rebuilt.
    agent_order_dirty: bool,
    /// Incremental liveness review schedule.
    liveness_schedule: BinaryHeap<Reverse<ScheduledAgentReview>>,
    /// Agents that must be reevaluated immediately due to policy changes.
    dirty_agents: HashSet<String>,
    /// Per-project conflict graphs.
    conflict_graphs: HashMap<String, ProjectConflictGraph>,
    /// Projects whose conflict state has changed since the last cached render.
    dirty_projects: HashSet<String>,
    /// Monotone communication/session synthesis for operator summaries.
    session_summary: SessionSummary,
    /// Cross-thread participation graph for communication-aware coordination.
    thread_participation: ThreadParticipationGraph,
    /// Cached deadlock SCCs by project.
    deadlock_cache: HashMap<String, DeadlockCacheEntry>,
    /// Total number of currently cached deadlock cycles across all projects.
    deadlock_cycle_total: usize,
    /// Cumulative cache hits for deadlock reuse.
    deadlock_cache_hits: u64,
    /// Cumulative cache misses for deadlock reuse.
    deadlock_cache_misses: u64,
    /// Calibration guard (safe mode policy).
    calibration: CalibrationGuard,
    /// E-process martingale monitor.
    eprocess: EProcessMonitor,
    /// CUSUM regime change detector.
    cusum: CusumDetector,
    /// Counterfactual regret tracker.
    regret: RegretTracker,
    /// Evidence ledger.
    ledger: EvidenceLedger,
    /// Incremental stage cost model for budget-aware probe selection.
    cost_model: AtcCostModel,
    /// Slow controller that adapts probe pressure on a coarser timescale.
    slow_controller: AtcSlowControllerState,
    /// Policy-as-data artifact for live liveness decisions.
    incumbent_policy: AtcLivenessPolicyArtifact,
    /// Shadow-mode candidate policy.
    shadow_policy: AtcShadowPolicyState,
    /// Deterministic policy bundle used for replay and validation.
    policy_bundle: AtcLivenessPolicyBundle,
    /// Monotonic policy revision for operator visibility.
    policy_revision: u64,
    /// Most recent stage timings emitted by the kernel.
    last_stage_timings: AtcStageTimings,
    /// Most recent kernel telemetry snapshot.
    last_kernel_telemetry: AtcKernelTelemetry,
    /// Most recent budget telemetry snapshot.
    last_budget_telemetry: AtcBudgetTelemetry,
    /// Most recent policy telemetry snapshot.
    last_policy_telemetry: AtcPolicyTelemetry,
    /// Last processed event sequence number (incremental computation).
    #[allow(dead_code)]
    last_event_seq: u64,
    /// Outstanding liveness decisions awaiting eventual ground-truth feedback.
    pending_liveness_feedback: HashMap<String, PendingLivenessFeedback>,
    /// Engine tick count.
    tick_count: u64,
    /// Monotonic snapshot sequence counter.
    snapshot_seq: u64,
    /// Number of feedback resolutions received (resolved experiences).
    resolved_experience_count: u64,
    /// Per-agent intervention counts for concentration detection.
    agent_intervention_counts: HashMap<String, u64>,
    /// Cached state from the previous snapshot for delta computation.
    prev_snapshot_state: PrevSnapshotState,
}

/// Minimal cached state from the prior summary snapshot for delta computation.
#[derive(Debug, Clone, Default)]
struct PrevSnapshotState {
    posture: Option<ExecutionPosture>,
    regime: Option<SummaryRegimeState>,
    policy_revision: u64,
    agent_count: usize,
    safe_mode: bool,
}

#[derive(Debug, Default)]
pub(crate) struct LivenessEvaluation {
    due_agents: usize,
    actions: Vec<(String, LivenessAction)>,
    decision_metadata: HashMap<String, LivenessDecisionMetadata>,
}

#[derive(Debug, Clone)]
pub(crate) struct LivenessDecisionMetadata {
    decision_id: u64,
}

impl AtcEngine {
    fn seeded_rhythm(program: &str) -> AgentRhythm {
        ATC_POPULATION
            .get()
            .and_then(|lock| lock.lock().ok())
            .map_or_else(
                || AgentRhythm::new(program_prior_interval_secs(program)),
                |pop| pop.create_rhythm(program),
            )
    }

    /// Create a new ATC engine with the given configuration.
    #[must_use]
    pub fn new(config: AtcConfig) -> Self {
        let calibration = CalibrationGuard::new(config.safe_mode_recovery_count);
        let eprocess = EProcessMonitor::new(0.85, config.eprocess_alert_threshold);
        let cusum = CusumDetector::new(0.15, config.cusum_threshold, config.cusum_delta);
        let regret = RegretTracker::new(100);
        let ledger = EvidenceLedger::new(config.ledger_capacity);
        let mut liveness_core = default_liveness_core();
        let mut incumbent_policy = AtcLivenessPolicyArtifact::from_core(
            "liveness-incumbent-r1".to_string(),
            &liveness_core,
            config.suspicion_k,
        );
        let mut shadow_policy = AtcShadowPolicyState {
            candidate: Some(AtcLivenessPolicyArtifact::candidate_from_incumbent(
                &incumbent_policy,
            )),
            ..AtcShadowPolicyState::default()
        };
        let mut policy_bundle = AtcLivenessPolicyBundle::from_live_policies(
            &incumbent_policy,
            shadow_policy.candidate.as_ref(),
            1,
        );
        let policy_revision = 1;
        if let Some(path) = config.policy_bundle_path.as_deref() {
            match AtcLivenessPolicyBundle::load_from_path(std::path::Path::new(path)) {
                Ok(bundle) => {
                    bundle.incumbent.apply_to_core(&mut liveness_core);
                    incumbent_policy = bundle.incumbent.clone();
                    shadow_policy = AtcShadowPolicyState {
                        candidate: bundle.candidate.clone(),
                        ..AtcShadowPolicyState::default()
                    };
                    policy_bundle = bundle;
                    tracing::info!(path, bundle = %policy_bundle.bundle_id, "loaded ATC policy bundle from disk");
                }
                Err(error) => {
                    tracing::warn!(path, %error, "failed to load ATC policy bundle; using in-process defaults");
                }
            }
        }
        debug_assert!(policy_bundle.validate().is_ok());

        Self {
            config,
            registered: false,
            liveness_core,
            conflict_core: default_conflict_core(),
            agents: HashMap::new(),
            sorted_agent_names: Vec::new(),
            agent_order_dirty: false,
            liveness_schedule: BinaryHeap::new(),
            dirty_agents: HashSet::new(),
            conflict_graphs: HashMap::new(),
            dirty_projects: HashSet::new(),
            session_summary: SessionSummary::default(),
            thread_participation: ThreadParticipationGraph::default(),
            deadlock_cache: HashMap::new(),
            deadlock_cycle_total: 0,
            deadlock_cache_hits: 0,
            deadlock_cache_misses: 0,
            calibration,
            eprocess,
            cusum,
            regret,
            ledger,
            cost_model: AtcCostModel::default(),
            slow_controller: AtcSlowControllerState {
                probe_limit: incumbent_policy.max_probes_per_tick,
                ..AtcSlowControllerState::default()
            },
            incumbent_policy,
            shadow_policy,
            policy_bundle,
            policy_revision,
            last_stage_timings: AtcStageTimings::default(),
            last_kernel_telemetry: AtcKernelTelemetry::default(),
            last_budget_telemetry: AtcBudgetTelemetry::default(),
            last_policy_telemetry: AtcPolicyTelemetry::default(),
            last_event_seq: 0,
            pending_liveness_feedback: HashMap::new(),
            tick_count: 0,
            snapshot_seq: 0,
            resolved_experience_count: 0,
            agent_intervention_counts: HashMap::new(),
            prev_snapshot_state: PrevSnapshotState::default(),
        }
    }

    /// Create an engine for testing (disabled by default, in-memory).
    #[cfg(test)]
    #[must_use]
    pub fn new_for_testing() -> Self {
        Self::new(AtcConfig {
            enabled: true,
            ..AtcConfig::default()
        })
    }

    /// Whether the ATC is enabled.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Whether the ATC has registered itself.
    #[must_use]
    pub const fn registered(&self) -> bool {
        self.registered
    }

    /// Whether safe mode is active.
    #[must_use]
    pub const fn is_safe_mode(&self) -> bool {
        self.calibration.is_safe_mode()
    }

    /// Access the evidence ledger.
    #[must_use]
    pub const fn ledger(&self) -> &EvidenceLedger {
        &self.ledger
    }

    /// Access the e-process monitor.
    #[must_use]
    pub const fn eprocess(&self) -> &EProcessMonitor {
        &self.eprocess
    }

    /// Access the CUSUM detector.
    #[must_use]
    pub const fn cusum(&self) -> &CusumDetector {
        &self.cusum
    }

    /// Access the regret tracker.
    #[must_use]
    pub const fn regret(&self) -> &RegretTracker {
        &self.regret
    }

    /// Current tick count.
    #[must_use]
    pub const fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Get liveness state for an agent.
    #[must_use]
    pub fn agent_liveness(&self, name: &str) -> Option<LivenessState> {
        self.agents.get(name).map(|e| e.state)
    }

    /// Get all tracked agent names.
    #[must_use]
    pub fn tracked_agents(&self) -> Vec<&str> {
        self.agents.keys().map(String::as_str).collect()
    }

    fn agent_project_key(&self, agent: &str) -> Option<String> {
        self.agents
            .get(agent)
            .and_then(|entry| entry.project_key.clone())
    }

    /// Get the alive-state posterior probability for an agent.
    #[must_use]
    pub fn agent_alive_posterior(&self, name: &str) -> Option<f64> {
        self.agents.get(name).map(|entry| {
            entry
                .core
                .posterior()
                .iter()
                .find(|(s, _)| *s == LivenessState::Alive)
                .map_or(0.0, |(_, p)| *p)
        })
    }

    /// Mutable access to the e-process monitor (for replay harness).
    pub(crate) fn eprocess_mut(&mut self) -> &mut EProcessMonitor {
        &mut self.eprocess
    }

    /// Mutable access to the CUSUM detector (for replay harness).
    pub(crate) fn cusum_mut(&mut self) -> &mut CusumDetector {
        &mut self.cusum
    }

    fn conflict_edge_ttl_micros(&self) -> i64 {
        self.config
            .advisory_cooldown_micros
            .max(self.config.probe_interval_micros.saturating_mul(4))
            .max(60_000_000)
    }

    fn note_message_sent(
        &mut self,
        from: &str,
        to: &[String],
        thread_id: Option<&str>,
        timestamp_micros: i64,
    ) {
        self.session_summary.absorb(&SynthesisEvent::MessageSent {
            from: from.to_string(),
            to: to.to_vec(),
            thread_id: thread_id.map(str::to_string),
            timestamp_micros,
        });
        if let Some(thread_id) =
            thread_id.filter(|thread_id| !thread_id.is_empty() && *thread_id != "unthreaded")
        {
            self.thread_participation
                .record_participation(from, thread_id);
            for recipient in to {
                self.thread_participation
                    .record_participation(recipient, thread_id);
            }
        }
    }

    fn note_message_received(
        &mut self,
        agent: &str,
        thread_id: Option<&str>,
        timestamp_micros: i64,
    ) {
        self.session_summary
            .absorb(&SynthesisEvent::MessageReceived {
                agent: agent.to_string(),
                timestamp_micros,
            });
        if let Some(thread_id) =
            thread_id.filter(|thread_id| !thread_id.is_empty() && *thread_id != "unthreaded")
        {
            self.thread_participation
                .record_participation(agent, thread_id);
        }
    }

    pub(crate) fn note_reservation_granted(
        &mut self,
        agent: &str,
        paths: &[String],
        exclusive: bool,
        project: &str,
        timestamp_micros: i64,
    ) {
        self.session_summary
            .absorb(&SynthesisEvent::ReservationGranted {
                agent: agent.to_string(),
                timestamp_micros,
            });
        if !exclusive || paths.is_empty() {
            return;
        }
        let graph = self.conflict_graphs.entry(project.to_string()).or_default();
        let cleared = graph.clear_blocked_conflicts_for_grant(agent, paths);
        if cleared > 0 {
            self.dirty_projects.insert(project.to_string());
            self.session_summary
                .absorb(&SynthesisEvent::ConflictResolved { timestamp_micros });
        }
    }

    pub(crate) fn note_reservation_released(
        &mut self,
        agent: &str,
        paths: &[String],
        project: &str,
        timestamp_micros: i64,
    ) {
        self.session_summary
            .absorb(&SynthesisEvent::ReservationReleased {
                agent: agent.to_string(),
                timestamp_micros,
            });
        let graph = self.conflict_graphs.entry(project.to_string()).or_default();
        let cleared = graph.clear_holder_conflicts_for_release(agent, paths);
        if cleared > 0 {
            self.dirty_projects.insert(project.to_string());
            self.session_summary
                .absorb(&SynthesisEvent::ConflictResolved { timestamp_micros });
        }
    }

    pub(crate) fn note_reservation_renewed(
        &mut self,
        agent: &str,
        paths: &[String],
        project: &str,
        timestamp_micros: i64,
    ) {
        self.session_summary
            .absorb(&SynthesisEvent::ReservationRenewed {
                agent: agent.to_string(),
                timestamp_micros,
            });
        if !paths.is_empty() {
            let graph = self.conflict_graphs.entry(project.to_string()).or_default();
            let cleared = graph.clear_blocked_conflicts_for_grant(agent, paths);
            if cleared > 0 {
                self.dirty_projects.insert(project.to_string());
                self.session_summary
                    .absorb(&SynthesisEvent::ConflictResolved { timestamp_micros });
            }
        }
    }

    pub(crate) fn note_reservation_force_released(
        &mut self,
        agent: &str,
        paths: &[String],
        project: &str,
        timestamp_micros: i64,
    ) {
        self.session_summary
            .absorb(&SynthesisEvent::ReservationForceReleased {
                agent: agent.to_string(),
                timestamp_micros,
            });
        let graph = self.conflict_graphs.entry(project.to_string()).or_default();
        let cleared = graph.clear_holder_conflicts_for_release(agent, paths);
        if cleared > 0 {
            self.dirty_projects.insert(project.to_string());
            self.session_summary
                .absorb(&SynthesisEvent::ConflictResolved { timestamp_micros });
        }
    }

    pub(crate) fn note_reservation_conflicts(
        &mut self,
        requester: &str,
        project: &str,
        conflicts: &[(String, String, String)],
        timestamp_micros: i64,
    ) {
        if requester.is_empty() || conflicts.is_empty() {
            return;
        }
        let graph = self.conflict_graphs.entry(project.to_string()).or_default();
        let mut added = false;
        for (holder, requested_path, holder_path_pattern) in conflicts {
            added |= graph.record_blocking_conflict(
                holder,
                requester,
                requested_path,
                holder_path_pattern,
                timestamp_micros,
            );
        }
        if added {
            self.dirty_projects.insert(project.to_string());
            self.session_summary
                .absorb(&SynthesisEvent::ConflictDetected { timestamp_micros });
        }
    }

    fn note_atc_intervention(&mut self, timestamp_micros: i64) {
        self.session_summary
            .absorb(&SynthesisEvent::AtcIntervention { timestamp_micros });
    }

    fn record_pending_liveness_feedback(
        &mut self,
        agent: &str,
        action: LivenessAction,
        expected_loss: f64,
        issued_at_micros: i64,
    ) {
        if action == LivenessAction::DeclareAlive {
            return;
        }
        self.pending_liveness_feedback.insert(
            agent.to_string(),
            PendingLivenessFeedback {
                action,
                expected_loss,
                issued_at_micros,
            },
        );
    }

    fn apply_liveness_feedback(
        &mut self,
        agent: &str,
        action: LivenessAction,
        true_state: LivenessState,
        predicted_loss: f64,
        actual_loss: f64,
        correct: bool,
        timestamp_micros: i64,
    ) {
        self.resolved_experience_count = self.resolved_experience_count.saturating_add(1);
        *self
            .agent_intervention_counts
            .entry(agent.to_string())
            .or_insert(0) += 1;
        self.eprocess
            .update(correct, AtcSubsystem::Liveness, Some(agent));
        self.cusum.update(!correct, timestamp_micros);

        let ep_snapshot = self.eprocess.clone();
        let cusum_snapshot = self.cusum.clone();
        if self
            .calibration
            .update(&ep_snapshot, &cusum_snapshot, correct, timestamp_micros)
        {
            self.mark_agents_dirty();
        }

        if let Some(conformal_lock) = ATC_CONFORMAL.get()
            && let Ok(mut conformal) = conformal_lock.lock()
        {
            conformal.observe(AtcSubsystem::Liveness, predicted_loss, actual_loss);
        }

        if let Some(thresholds_lock) = ATC_THRESHOLDS.get()
            && let Ok(mut thresholds) = thresholds_lock.lock()
            && let Some(adaptive) = thresholds.get_mut(agent)
        {
            adaptive.record_outcome(correct);
        }

        if let Some(tuner_lock) = ATC_LIVENESS_TUNER.get()
            && let Ok(mut tuner) = tuner_lock.lock()
        {
            let regret = (predicted_loss - actual_loss).abs();
            tuner.record_outcome(action, true_state, regret);
            if tuner.maybe_update(&mut self.liveness_core) {
                self.propagate_liveness_policy();
            }
        }

        let silence = self
            .agents
            .get(agent)
            .map_or(0, |entry| entry.rhythm.silence_duration(timestamp_micros));
        if let Some(survival_lock) = ATC_SURVIVAL.get()
            && let Ok(mut survival) = survival_lock.lock()
        {
            let estimator = survival
                .entry("all".to_string())
                .or_insert_with(|| KaplanMeierEstimator::new(1000));
            estimator.observe(silence, true_state != LivenessState::Alive);
        }
    }

    fn resolve_pending_feedback_on_activity(&mut self, agent: &str, timestamp_micros: i64) {
        let Some(pending) = self.pending_liveness_feedback.remove(agent) else {
            return;
        };
        let (actual_loss, correct) = match pending.action {
            LivenessAction::ReleaseReservations => (100.0, false),
            LivenessAction::Suspect => (2.0, true),
            LivenessAction::DeclareAlive => (0.0, true),
        };
        self.apply_liveness_feedback(
            agent,
            pending.action,
            LivenessState::Alive,
            pending.expected_loss,
            actual_loss,
            correct,
            timestamp_micros,
        );
    }

    fn resolve_stale_liveness_feedback(&mut self, now_micros: i64) {
        let feedback_window_micros = self.config.probe_interval_micros.max(60_000_000);
        let expired_agents: Vec<String> = self
            .pending_liveness_feedback
            .iter()
            .filter_map(|(agent, pending)| {
                (now_micros.saturating_sub(pending.issued_at_micros) >= feedback_window_micros)
                    .then_some(agent.clone())
            })
            .collect();
        for agent in expired_agents {
            let Some(pending) = self.pending_liveness_feedback.remove(&agent) else {
                continue;
            };
            let true_state = self
                .agents
                .get(&agent)
                .map_or(LivenessState::Dead, |entry| entry.state);
            let (actual_loss, correct) = match pending.action {
                LivenessAction::ReleaseReservations => match true_state {
                    LivenessState::Dead => (1.0, true),
                    LivenessState::Flaky => (20.0, false),
                    LivenessState::Alive => (100.0, false),
                },
                LivenessAction::Suspect => match true_state {
                    LivenessState::Alive => (8.0, false),
                    LivenessState::Flaky => (2.0, true),
                    LivenessState::Dead => (6.0, true),
                },
                LivenessAction::DeclareAlive => (0.0, true),
            };
            self.apply_liveness_feedback(
                &agent,
                pending.action,
                true_state,
                pending.expected_loss,
                actual_loss,
                correct,
                now_micros,
            );
        }
    }

    fn prune_stale_conflicts(&mut self, now_micros: i64) {
        let cutoff = now_micros.saturating_sub(self.conflict_edge_ttl_micros());
        let dirty_projects: Vec<String> = self.conflict_graphs.keys().cloned().collect();
        for project in dirty_projects {
            let removed = self
                .conflict_graphs
                .get_mut(&project)
                .map_or(0, |graph| graph.prune_stale_edges(cutoff));
            if removed > 0 {
                self.dirty_projects.insert(project);
                self.session_summary
                    .absorb(&SynthesisEvent::ConflictResolved {
                        timestamp_micros: now_micros,
                    });
            }
        }
    }

    fn prune_agent_from_conflict_graphs(&mut self, agent_name: &str) {
        let dirty_projects: Vec<String> = self.conflict_graphs.keys().cloned().collect();
        for project in dirty_projects {
            let removed = self
                .conflict_graphs
                .get_mut(&project)
                .map_or(0, |graph| graph.remove_agent(agent_name));
            if removed > 0 {
                self.dirty_projects.insert(project);
                // Also note conflict resolved so UI updates
                self.session_summary
                    .absorb(&SynthesisEvent::ConflictResolved {
                        timestamp_micros: mcp_agent_mail_core::timestamps::now_micros(),
                    });
            }
        }
    }

    fn current_release_guard_reason(&self) -> Option<String> {
        if self.policy_bundle.validate().is_err() {
            Some("policy_bundle_invalid".to_string())
        } else if self.calibration.is_safe_mode() {
            Some("calibration_safe_mode".to_string())
        } else if self.slow_controller.probe_budget_fraction < 0.2 {
            Some("budget_pressure".to_string())
        } else {
            None
        }
    }

    fn mark_agents_dirty(&mut self) {
        self.dirty_agents.extend(self.agents.keys().cloned());
    }

    fn mark_agent_order_dirty(&mut self) {
        self.agent_order_dirty = true;
    }

    fn sorted_agent_names(&mut self) -> &[String] {
        if self.agent_order_dirty {
            self.sorted_agent_names = self.agents.keys().cloned().collect();
            self.sorted_agent_names.sort();
            self.agent_order_dirty = false;
        }
        &self.sorted_agent_names
    }

    fn refresh_incumbent_policy_from_core(&mut self) {
        self.policy_revision = self.policy_revision.saturating_add(1);
        self.incumbent_policy = AtcLivenessPolicyArtifact::from_core(
            format!("liveness-incumbent-r{}", self.policy_revision),
            &self.liveness_core,
            self.config.suspicion_k,
        );
        self.shadow_policy.candidate = Some(AtcLivenessPolicyArtifact::candidate_from_incumbent(
            &self.incumbent_policy,
        ));
        self.slow_controller.probe_limit = self
            .slow_controller
            .probe_limit
            .min(self.incumbent_policy.max_probes_per_tick.max(1));
        self.policy_bundle = AtcLivenessPolicyBundle::from_live_policies(
            &self.incumbent_policy,
            self.shadow_policy.candidate.as_ref(),
            self.policy_revision,
        );
        self.mark_agents_dirty();
    }

    fn schedule_entry_for_push(&mut self, agent: &str, review_at_micros: i64) {
        let scheduled = {
            let Some(entry) = self.agents.get_mut(agent) else {
                return;
            };
            entry.schedule_version = entry.schedule_version.saturating_add(1);
            entry.next_review_micros = review_at_micros;
            let schedule_version = entry.schedule_version;
            (review_at_micros < i64::MAX).then(|| {
                Reverse(ScheduledAgentReview {
                    review_at_micros,
                    schedule_version,
                    agent: agent.to_string(),
                })
            })
        };
        if let Some(item) = scheduled {
            self.liveness_schedule.push(item);
            self.compact_liveness_schedule_if_needed();
        }
    }

    fn scheduled_agent_count(&self) -> usize {
        self.agents
            .values()
            .filter(|entry| entry.next_review_micros < i64::MAX)
            .count()
    }

    fn compact_liveness_schedule_if_needed(&mut self) {
        let live_scheduled = self.scheduled_agent_count();
        let max_heap_entries = live_scheduled.saturating_mul(4).max(64);
        if self.liveness_schedule.len() <= max_heap_entries {
            return;
        }

        let mut rebuilt = BinaryHeap::new();
        for entry in self.agents.values() {
            if entry.next_review_micros == i64::MAX {
                continue;
            }
            rebuilt.push(Reverse(ScheduledAgentReview {
                review_at_micros: entry.next_review_micros,
                schedule_version: entry.schedule_version,
                agent: entry.name.clone(),
            }));
        }
        self.liveness_schedule = rebuilt;
    }

    fn effective_threshold(base_k: f64, adaptive_k: Option<f64>, fallback_active: bool) -> f64 {
        let adaptive_k = adaptive_k.unwrap_or(base_k);
        if fallback_active {
            adaptive_k.max(base_k + 0.5)
        } else {
            adaptive_k
        }
    }

    fn next_review_time_for_policy(
        &self,
        entry: &AgentLivenessEntry,
        threshold_k: f64,
        now_micros: i64,
    ) -> i64 {
        if entry.state == LivenessState::Dead || entry.rhythm.last_activity_ts <= 0 {
            return i64::MAX;
        }
        let threshold =
            micros_f64_to_i64(entry.rhythm.suspicion_threshold(threshold_k).max(1.0)).max(1);
        let mut next_review = entry.rhythm.last_activity_ts.saturating_add(threshold);
        if entry.state == LivenessState::Flaky {
            next_review = next_review.min(
                now_micros.saturating_add((self.config.probe_interval_micros / 2).max(250_000)),
            );
        }
        if entry.probe_sent_at > 0 {
            next_review = next_review.min(
                entry
                    .probe_sent_at
                    .saturating_add((self.config.probe_interval_micros / 2).max(250_000)),
            );
        }
        next_review
    }

    fn reschedule_agent(&mut self, agent: &str, now_micros: i64, incumbent_k: f64) {
        let Some(entry) = self.agents.get(agent) else {
            return;
        };
        let incumbent_review = self.next_review_time_for_policy(entry, incumbent_k, now_micros);
        let candidate_review = self
            .shadow_policy
            .candidate
            .as_ref()
            .map_or(i64::MAX, |candidate| {
                self.next_review_time_for_policy(entry, candidate.suspicion_k, now_micros)
            });
        self.schedule_entry_for_push(agent, incumbent_review.min(candidate_review));
    }

    fn pop_due_agents(&mut self, now_micros: i64) -> Vec<String> {
        let mut due = Vec::new();
        let mut seen = HashSet::new();
        while let Some(Reverse(next)) = self.liveness_schedule.peek().cloned() {
            if next.review_at_micros > now_micros {
                break;
            }
            self.liveness_schedule.pop();
            let Some(entry) = self.agents.get(&next.agent) else {
                continue;
            };
            let stale_version = entry.schedule_version != next.schedule_version;
            let stale_review_time = entry.next_review_micros != next.review_at_micros;
            if stale_version || stale_review_time {
                continue;
            }
            if seen.insert(next.agent.clone()) {
                due.push(next.agent);
            }
        }
        for agent in self.dirty_agents.drain() {
            if seen.insert(agent.clone()) {
                due.push(agent);
            }
        }
        due
    }

    fn next_scheduled_review_micros(&mut self) -> Option<i64> {
        while let Some(Reverse(next)) = self.liveness_schedule.peek().cloned() {
            let Some(entry) = self.agents.get(&next.agent) else {
                self.liveness_schedule.pop();
                continue;
            };
            let stale_version = entry.schedule_version != next.schedule_version;
            let stale_review_time = entry.next_review_micros != next.review_at_micros;
            if stale_version || stale_review_time {
                self.liveness_schedule.pop();
                continue;
            }
            return Some(next.review_at_micros);
        }
        None
    }

    fn normalize_deadlock_cycles(mut cycles: Vec<Vec<String>>) -> Vec<Vec<String>> {
        for cycle in &mut cycles {
            cycle.sort();
        }
        cycles.sort();
        cycles
    }

    fn update_deadlock_cache_for_project(&mut self, project: &str) {
        let Some(graph) = self.conflict_graphs.get(project) else {
            self.deadlock_cache.remove(project);
            return;
        };
        let should_recompute = self
            .deadlock_cache
            .get(project)
            .is_none_or(|cached| cached.generation != graph.generation);
        if should_recompute {
            self.deadlock_cache_misses = self.deadlock_cache_misses.saturating_add(1);
            let cycles = Self::normalize_deadlock_cycles(find_deadlock_cycles(graph));
            self.deadlock_cache.insert(
                project.to_string(),
                DeadlockCacheEntry {
                    generation: graph.generation,
                    cycles,
                },
            );
        } else {
            self.deadlock_cache_hits = self.deadlock_cache_hits.saturating_add(1);
        }
    }

    fn recompute_deadlock_totals(&mut self) {
        self.deadlock_cycle_total = self
            .deadlock_cache
            .values()
            .map(|entry| entry.cycles.len())
            .sum();
    }

    fn budget_mode(&self) -> AtcBudgetMode {
        if self.calibration.is_safe_mode() {
            AtcBudgetMode::Conservative
        } else if self.slow_controller.probe_budget_fraction > 0.6 {
            AtcBudgetMode::Nominal
        } else if self.slow_controller.probe_budget_fraction > 0.2 {
            AtcBudgetMode::Pressure
        } else {
            AtcBudgetMode::Conservative
        }
    }

    /// Force safe mode on/off (operator override).
    pub fn set_safe_mode(&mut self, active: bool, timestamp_micros: i64) {
        self.calibration.set_safe_mode(active, timestamp_micros);
        self.mark_agents_dirty();
    }

    /// Check whether an event is from/to the ATC itself (self-exclusion).
    #[allow(dead_code)]
    fn is_self_event(from: &str, to: &[String]) -> bool {
        from == ATC_AGENT_NAME || to.iter().any(|t| t == ATC_AGENT_NAME)
    }

    /// Register a new agent for liveness tracking.
    pub fn register_agent(&mut self, name: &str, program: &str, project_key: Option<&str>) {
        if name == ATC_AGENT_NAME {
            return; // self-exclusion
        }
        let project_key = project_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        match self.agents.entry(name.to_string()) {
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                let entry = occupied.get_mut();
                let program_changed = entry.program != program;
                if project_key.is_some() {
                    entry.project_key = project_key;
                }
                entry.program = program.to_string();
                if program_changed {
                    let seeded = Self::seeded_rhythm(program);
                    if entry.rhythm.observation_count == 0 && entry.rhythm.last_activity_ts == 0 {
                        entry.rhythm = seeded;
                    } else {
                        entry.rhythm.prior_interval = seeded.prior_interval;
                    }
                }
                entry.core.sync_policy_from(&self.liveness_core);
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(AgentLivenessEntry {
                    name: name.to_string(),
                    project_key,
                    program: program.to_string(),
                    state: LivenessState::Alive,
                    rhythm: Self::seeded_rhythm(program),
                    suspect_since: 0,
                    probe_sent_at: 0,
                    sprt_log_lr: 0.0,
                    core: self.liveness_core.clone(),
                    schedule_version: 0,
                    next_review_micros: i64::MAX,
                });
                self.mark_agent_order_dirty();
            }
        }
        let fallback_active = self.current_release_guard_reason().is_some();
        let incumbent_k = Self::effective_threshold(
            self.incumbent_policy.suspicion_k,
            current_adaptive_threshold_k(name),
            fallback_active,
        );
        self.reschedule_agent(
            name,
            mcp_agent_mail_core::timestamps::now_micros(),
            incumbent_k,
        );
    }

    /// Process an agent activity signal (message, reservation, commit).
    pub fn observe_activity(
        &mut self,
        agent: &str,
        project_key: Option<&str>,
        timestamp_micros: i64,
    ) {
        if agent == ATC_AGENT_NAME {
            return;
        }
        self.resolve_pending_feedback_on_activity(agent, timestamp_micros);
        let mut resurrected = false;
        if let Some(entry) = self.agents.get_mut(agent) {
            if let Some(project_key) = project_key.map(str::trim).filter(|value| !value.is_empty())
            {
                entry.project_key = Some(project_key.to_string());
            }
            entry.rhythm.observe(timestamp_micros);
            // Any activity resets to Alive (resurrection)
            if entry.state != LivenessState::Alive {
                entry.state = LivenessState::Alive;
                entry.suspect_since = 0;
                entry.probe_sent_at = 0;
                entry.sprt_log_lr = 0.0;
                resurrected = true;
            }
        }
        let fallback_active = self.current_release_guard_reason().is_some();
        let incumbent_k = Self::effective_threshold(
            self.incumbent_policy.suspicion_k,
            current_adaptive_threshold_k(agent),
            fallback_active,
        );
        self.reschedule_agent(agent, timestamp_micros, incumbent_k);
        if resurrected {
            self.dirty_agents.insert(agent.to_string());
        }
    }

    /// Evaluate liveness for all tracked agents.
    ///
    /// Returns the number of due agents that were inspected together with any
    /// `(agent_name, recommended_action)` pairs that require intervention.
    #[must_use]
    pub(crate) fn evaluate_liveness(&mut self, now_micros: i64) -> LivenessEvaluation {
        let fallback_reason = self.current_release_guard_reason();
        let fallback_active = fallback_reason.is_some();
        let incumbent_policy = self.incumbent_policy.clone();
        let candidate_policy = self.shadow_policy.candidate.clone();
        let due_agents = self.pop_due_agents(now_micros);
        let mut evaluation = LivenessEvaluation {
            due_agents: due_agents.len(),
            ..LivenessEvaluation::default()
        };

        for agent_name in due_agents {
            let incumbent_k = Self::effective_threshold(
                incumbent_policy.suspicion_k,
                current_adaptive_threshold_k(&agent_name),
                fallback_active,
            );
            let candidate_k = candidate_policy
                .as_ref()
                .map_or(incumbent_policy.suspicion_k, |candidate| {
                    candidate.suspicion_k
                });

            let mut shadow_pair: Option<(LivenessAction, LivenessAction, f64, f64)> = None;
            let mut decision_log: Option<(String, LivenessAction, f64, f64, String)> = None;
            let mut action_to_emit: Option<(String, LivenessAction)> = None;
            let mut newly_dead = false;

            if let Some(entry) = self.agents.get_mut(&agent_name) {
                if entry.state == LivenessState::Dead {
                    entry.next_review_micros = i64::MAX;
                    continue;
                }

                let incumbent_suspicious = entry.rhythm.is_suspicious(now_micros, incumbent_k);
                let candidate_suspicious = entry.rhythm.is_suspicious(now_micros, candidate_k);
                if incumbent_suspicious || candidate_suspicious {
                    let silence_ratio =
                        nonnegative_i64_to_f64(entry.rhythm.silence_duration(now_micros))
                            / entry.rhythm.effective_avg().max(1.0);
                    let alive_lk = (-silence_ratio * 0.5).exp().max(0.01);
                    let flaky_lk = (-silence_ratio * 0.1).exp().max(0.05);
                    let dead_lk = 1.0 - alive_lk;

                    entry.core.update_posterior(&[
                        (LivenessState::Alive, alive_lk),
                        (LivenessState::Flaky, flaky_lk),
                        (LivenessState::Dead, dead_lk),
                    ]);

                    let (incumbent_action, incumbent_loss, runner_up_loss) = if incumbent_suspicious
                    {
                        incumbent_policy.choose_action(entry.core.posterior(), fallback_active)
                    } else {
                        (
                            LivenessAction::DeclareAlive,
                            incumbent_policy.expected_loss(
                                LivenessAction::DeclareAlive,
                                entry.core.posterior(),
                            ),
                            incumbent_policy
                                .expected_loss(LivenessAction::Suspect, entry.core.posterior()),
                        )
                    };
                    let (candidate_action, candidate_loss) = candidate_policy.as_ref().map_or(
                        (LivenessAction::DeclareAlive, incumbent_loss),
                        |candidate| {
                            if candidate_suspicious {
                                let (action, loss, _) = candidate
                                    .choose_action(entry.core.posterior(), fallback_active);
                                (action, loss)
                            } else {
                                (
                                    LivenessAction::DeclareAlive,
                                    candidate.expected_loss(
                                        LivenessAction::DeclareAlive,
                                        entry.core.posterior(),
                                    ),
                                )
                            }
                        },
                    );
                    shadow_pair = Some((
                        incumbent_action,
                        candidate_action,
                        incumbent_loss,
                        candidate_loss,
                    ));

                    if incumbent_suspicious && incumbent_action != LivenessAction::DeclareAlive {
                        let evidence_summary = format!(
                            "silence {}s (avg {}s, {:.1}σ)",
                            entry.rhythm.silence_duration(now_micros) / 1_000_000,
                            micros_f64_to_i64(entry.rhythm.effective_avg()) / 1_000_000,
                            silence_ratio,
                        );
                        match incumbent_action {
                            LivenessAction::Suspect => {
                                if entry.state != LivenessState::Flaky {
                                    entry.state = LivenessState::Flaky;
                                    entry.suspect_since = now_micros;
                                }
                            }
                            LivenessAction::ReleaseReservations => {
                                entry.state = LivenessState::Dead;
                                newly_dead = true;
                            }
                            LivenessAction::DeclareAlive => {}
                        }
                        decision_log = Some((
                            entry.name.clone(),
                            incumbent_action,
                            incumbent_loss,
                            runner_up_loss,
                            evidence_summary,
                        ));
                        action_to_emit = Some((entry.name.clone(), incumbent_action));
                    }
                }
            }

            if let Some((incumbent_action, candidate_action, incumbent_loss, candidate_loss)) =
                shadow_pair
            {
                self.shadow_policy.record_decision_pair(
                    incumbent_action,
                    candidate_action,
                    incumbent_loss,
                    candidate_loss,
                );
                self.regret.record_outcome(
                    self.ledger.latest_id(),
                    &format!("{incumbent_action:?}"),
                    incumbent_loss,
                    &format!("{candidate_action:?}"),
                    candidate_loss.min(incumbent_loss),
                    now_micros,
                );
            }

            if let Some((subject, action, incumbent_loss, runner_up_loss, evidence_summary)) =
                decision_log
            {
                let Some(entry) = self.agents.get(&agent_name) else {
                    continue;
                };
                let decision_id = self.ledger.record(&DecisionBuilder {
                    subsystem: AtcSubsystem::Liveness,
                    decision_class: "liveness_transition",
                    subject: &subject,
                    core: &entry.core,
                    action,
                    expected_loss: incumbent_loss,
                    runner_up_loss,
                    evidence_summary: &evidence_summary,
                    calibration_healthy: !fallback_active,
                    safe_mode_active: fallback_active,
                    policy_id: Some(&incumbent_policy.policy_id),
                    fallback_reason: fallback_reason.as_deref(),
                    timestamp_micros: now_micros,
                });
                evaluation
                    .decision_metadata
                    .insert(subject.clone(), LivenessDecisionMetadata { decision_id });
                self.record_pending_liveness_feedback(&subject, action, incumbent_loss, now_micros);
            }

            if let Some(action) = action_to_emit {
                evaluation.actions.push(action);
            }

            if newly_dead {
                self.prune_agent_from_conflict_graphs(&agent_name);
            }

            self.reschedule_agent(&agent_name, now_micros, incumbent_k);
        }

        evaluation
    }

    /// Check for deadlock cycles in all project conflict graphs.
    #[must_use]
    pub fn detect_deadlocks(&mut self) -> Vec<(String, Vec<Vec<String>>)> {
        if self.deadlock_cache.is_empty() {
            self.dirty_projects
                .extend(self.conflict_graphs.keys().cloned());
        }
        let dirty_projects: Vec<String> = self.dirty_projects.drain().collect();
        // Count cache hits for non-dirty projects still served from cache.
        let reused = self
            .conflict_graphs
            .len()
            .saturating_sub(dirty_projects.len());
        self.deadlock_cache_hits = self.deadlock_cache_hits.saturating_add(reused as u64);
        for project in dirty_projects {
            self.update_deadlock_cache_for_project(&project);
        }
        self.recompute_deadlock_totals();

        let mut results = Vec::new();
        for (project, cached) in &self.deadlock_cache {
            if !cached.cycles.is_empty() {
                results.push((project.clone(), cached.cycles.clone()));
            }
        }
        results.sort_by(|left, right| left.0.cmp(&right.0));
        results
    }

    fn propagate_liveness_policy(&mut self) {
        for entry in self.agents.values_mut() {
            entry.core.sync_policy_from(&self.liveness_core);
        }
        self.refresh_incumbent_policy_from_core();
    }

    fn absorb_population_snapshot(&self, population: &mut HierarchicalAgentModel) {
        for entry in self.agents.values() {
            population.absorb_agent(&entry.program, &entry.rhythm);
        }
    }

    fn build_summary_snapshot_with(
        &mut self,
        now_micros: i64,
        stage_timings: &AtcStageTimings,
        kernel: &AtcKernelTelemetry,
        budget: &AtcBudgetTelemetry,
        policy: &AtcPolicyTelemetry,
    ) -> AtcSummarySnapshot {
        self.snapshot_seq = self.snapshot_seq.saturating_add(1);

        // ── agent snapshots (deterministic order) ────────────────────
        let mut agent_states = Vec::with_capacity(self.agents.len());
        let agent_names: Vec<String> = self.sorted_agent_names().to_vec();
        for name in &agent_names {
            let Some(entry) = self.agents.get(name) else {
                continue;
            };
            agent_states.push(AgentStateSnapshot {
                name: name.clone(),
                state: entry.state,
                silence_secs: entry.rhythm.silence_duration(now_micros) / 1_000_000,
                posterior_alive: entry.core.posterior_probability(LivenessState::Alive),
                intervention_count: self
                    .agent_intervention_counts
                    .get(name)
                    .copied()
                    .unwrap_or(0),
            });
        }

        // ── execution posture ────────────────────────────────────────
        let safe_mode = self.is_safe_mode();
        let execution_posture = if safe_mode {
            ExecutionPosture::SafeMode
        } else if self.eprocess.miscalibrated() || self.slow_controller.probe_budget_fraction < 0.5
        {
            ExecutionPosture::Cautious
        } else {
            ExecutionPosture::Normal
        };

        // ── regret trend ─────────────────────────────────────────────
        let regret_avg = self.regret.average_regret();
        let regret_recent_avg = self.regret.recent_average_regret();
        let regret_trend = if self.regret.outcome_count() < 10 {
            TrendDirection::Flat
        } else {
            let ratio = if regret_avg.abs() < 1e-12 {
                0.0
            } else {
                regret_recent_avg / regret_avg
            };
            if ratio > 1.15 {
                TrendDirection::Worsening
            } else if ratio < 0.85 {
                TrendDirection::Improving
            } else {
                TrendDirection::Flat
            }
        };

        // ── regime state ─────────────────────────────────────────────
        let (regime_state, regime_dwell_micros) =
            if let Some(last_change) = self.cusum.recent_changes(1).next() {
                let state = match last_change.direction {
                    ChangeDirection::Degradation => SummaryRegimeState::Degraded,
                    ChangeDirection::Improvement => SummaryRegimeState::Improved,
                };
                (state, now_micros.saturating_sub(last_change.timestamp))
            } else {
                (
                    SummaryRegimeState::Stable,
                    now_micros.saturating_sub(self.cusum.regime_start()),
                )
            };
        let regime_change_count = self.cusum.regime_change_count();

        // ── most-impacted agent ──────────────────────────────────────
        let most_impacted_agent = self
            .agent_intervention_counts
            .iter()
            .max_by_key(|(_, count)| *count)
            .and_then(|(name, count)| (*count > 0).then(|| name.clone()));

        // ── delta computation ────────────────────────────────────────
        let prev = &self.prev_snapshot_state;
        let delta = SummaryDelta {
            posture_changed: prev.posture.is_some_and(|p| p != execution_posture),
            regime_changed: prev.regime.is_some_and(|r| r != regime_state),
            policy_revision_changed: prev.policy_revision != self.policy_revision,
            agents_added: agent_states.len().saturating_sub(prev.agent_count),
            agents_removed: prev.agent_count.saturating_sub(agent_states.len()),
            safe_mode_transition: prev.safe_mode != safe_mode && self.snapshot_seq > 1,
        };

        // ── cache state for next delta ───────────────────────────────
        self.prev_snapshot_state = PrevSnapshotState {
            posture: Some(execution_posture),
            regime: Some(regime_state),
            policy_revision: self.policy_revision,
            agent_count: agent_states.len(),
            safe_mode,
        };

        AtcSummarySnapshot {
            snapshot_seq: self.snapshot_seq,
            generated_at_micros: now_micros,
            completeness: SnapshotCompleteness::Full,
            enabled: self.enabled(),
            safe_mode,
            execution_posture,
            tick_count: self.tick_count(),
            tracked_agents: agent_states,
            deadlock_cycles: self.deadlock_cycle_total,
            eprocess_value: self.eprocess.e_value(),
            eprocess_alert: self.eprocess.miscalibrated(),
            regret_avg,
            regret_recent_avg,
            regret_trend,
            decisions_total: self.ledger.latest_id(),
            experiences_open: self.pending_liveness_feedback.len(),
            experiences_resolved: self.resolved_experience_count,
            calibration_consecutive_correct: self.calibration.consecutive_correct(),
            calibration_recovery_target: self.calibration.recovery_count(),
            regime_state,
            regime_dwell_micros,
            regime_change_count,
            policy_revision: self.policy_revision,
            most_impacted_agent,
            delta,
            recent_decisions: self.ledger.recent(16).cloned().collect(),
            stage_timings: stage_timings.clone(),
            kernel: kernel.clone(),
            budget: budget.clone(),
            policy: policy.clone(),
        }
    }

    fn effect_plan_from_record(
        &self,
        record: &AtcDecisionRecord,
        timestamp_micros: i64,
        kind: &str,
        category: &str,
        family: &str,
        agent: String,
        project_key: Option<String>,
        message: Option<String>,
    ) -> AtcEffectPlan {
        let semantics =
            self.effect_semantics_for(record, family, kind, &agent, project_key.as_deref());
        let mut effect_seed = format!("{}:{kind}:{category}:{family}:{agent}", record.trace_id);
        if let Some(project_key) = project_key.as_deref() {
            effect_seed.push(':');
            effect_seed.push_str(project_key);
        }
        if let Some(policy_id) = record.policy_id.as_deref() {
            effect_seed.push(':');
            effect_seed.push_str(policy_id);
        }
        if let Some(message) = message.as_deref() {
            effect_seed.push(':');
            effect_seed.push_str(message);
        }
        AtcEffectPlan {
            decision_id: record.id,
            effect_id: format!("atc-effect-{:016x}", stable_fnv1a64(effect_seed.as_bytes())),
            experience_id: None,
            claim_id: record.claim_id.clone(),
            evidence_id: record.evidence_id.clone(),
            trace_id: record.trace_id.clone(),
            timestamp_micros,
            kind: kind.to_string(),
            category: category.to_string(),
            agent,
            project_key,
            policy_id: record.policy_id.clone(),
            policy_revision: self.policy_revision,
            message,
            expected_loss: record
                .loss_table
                .iter()
                .find_map(|entry| (entry.action == record.action).then_some(entry.expected_loss)),
            semantics,
        }
    }

    fn effect_semantics_for(
        &self,
        record: &AtcDecisionRecord,
        family: &str,
        kind: &str,
        agent: &str,
        project_key: Option<&str>,
    ) -> AtcEffectSemantics {
        let project_scope = project_key.unwrap_or("-");
        let cooldown_key = format!("{family}:{project_scope}:{agent}");
        let advisory_cooldown = self.config.advisory_cooldown_micros.max(0);
        let probe_cooldown = self.config.probe_interval_micros.max(0);
        let summary_cooldown = self.config.summary_interval_micros.max(0);
        let (
            risk_level,
            utility_model,
            operator_action,
            remediation,
            escalation_policy,
            cooldown_micros,
            requires_project,
            ack_required,
            high_risk_intervention,
            preconditions,
        ) = match family {
            "liveness_monitoring" => (
                "low",
                "nudge on suspicious inactivity while evidence is still below the release bar",
                "Reply or acknowledge if the session is still active; otherwise ATC will keep monitoring.",
                "A quick acknowledgment clears the low-confidence inactivity suspicion before probe or release escalation.",
                "escalate_to_probe_or_release_on_stronger_liveness_evidence",
                advisory_cooldown,
                true,
                false,
                false,
                vec![
                    "project context is available for direct ATC mail".to_string(),
                    "liveness evidence indicates suspicious inactivity but not a confirmed dead-agent release".to_string(),
                ],
            ),
            "deadlock_remediation" => (
                "medium",
                "surface deterministic deadlock cycles only when they point to a concrete reservation cleanup path",
                "Inspect the contested reservation cycle and release only the stale holder if the work is no longer active.",
                "Use the cycle evidence to clear the blocking reservation rather than sending repeated generic nudges.",
                "escalate_to_manual_conflict_resolution_if_cycle_persists",
                advisory_cooldown.max(summary_cooldown),
                true,
                false,
                false,
                vec![
                    "project context is available for direct ATC mail".to_string(),
                    "a deterministic deadlock cycle is still present at execution time".to_string(),
                ],
            ),
            "liveness_probe" => (
                "medium",
                "request a fast acknowledgment that separates stale sessions from active work before stronger intervention",
                "Reply or acknowledge promptly; lack of response becomes stronger release evidence.",
                "A probe is lower risk than release, but it should not repeat faster than the probe cadence.",
                "escalate_to_release_only_after_independent_dead_verdict",
                probe_cooldown,
                true,
                true,
                false,
                vec![
                    "project context is available for direct ATC mail".to_string(),
                    "the agent is not already marked for release in the same tick".to_string(),
                ],
            ),
            "reservation_release" => (
                "high",
                "clear stale reservations only after the dead-agent release policy has crossed its intervention threshold",
                "Verify the agent is actually inactive and re-reserve files immediately if the session revives.",
                "Automated release is intentionally narrow because it can disrupt active work.",
                "escalate_to_operator_review_if_agent_reappears",
                advisory_cooldown.max(summary_cooldown),
                true,
                false,
                true,
                vec![
                    "project context is available for reservation release".to_string(),
                    "the liveness decision still supports release at execution time".to_string(),
                    "calibration or safety gates have not withheld release".to_string(),
                ],
            ),
            "release_notice" => (
                "medium",
                "make high-risk automated release legible to the affected agent so recovery is explicit instead of silent",
                "Inspect the worktree now if the agent is still active and re-reserve any files that should remain held.",
                "This notice accompanies a release request and should not repeat like a generic inactivity advisory.",
                "no_further_automatic_escalation",
                advisory_cooldown.max(summary_cooldown),
                true,
                false,
                false,
                vec![
                    "project context is available for direct ATC mail".to_string(),
                    "a paired reservation-release effect was emitted in the same decision flow".to_string(),
                ],
            ),
            "withheld_release_notice" => (
                "low",
                "replace risky automated release with a softer, evidence-backed nudge when calibration is uncertain",
                "Inspect the session manually or ask the agent to acknowledge before any manual cleanup.",
                "When release is withheld, the operator should get evidence and a safer next step instead of a false intervention notice.",
                "retry_probe_before_any_release",
                advisory_cooldown.max(probe_cooldown),
                true,
                false,
                false,
                vec![
                    "project context is available for direct ATC mail".to_string(),
                    "release was withheld by an active calibration or safety gate".to_string(),
                ],
            ),
            _ => (
                "medium",
                "carry a concrete ATC effect through execution without silent reinterpretation",
                "Review the effect details and act according to the attached evidence.",
                "Unknown effect families should stay explicit rather than inheriting generic copy.",
                "manual_review_required",
                advisory_cooldown,
                true,
                kind == "probe_agent",
                kind == "release_reservations_requested",
                vec![format!("ATC effect family '{family}' must remain explicitly handled")],
            ),
        };

        AtcEffectSemantics {
            family: family.to_string(),
            risk_level: risk_level.to_string(),
            utility_model: utility_model.to_string(),
            operator_action: operator_action.to_string(),
            remediation: remediation.to_string(),
            escalation_policy: escalation_policy.to_string(),
            evidence_summary: record.evidence_summary.clone(),
            cooldown_key,
            cooldown_micros,
            requires_project,
            ack_required,
            high_risk_intervention,
            preconditions,
        }
    }

    fn effect_plan_for_decision_id(
        &self,
        decision_id: u64,
        timestamp_micros: i64,
        kind: &str,
        category: &str,
        family: &str,
        agent: String,
        project_key: Option<String>,
        message: Option<String>,
    ) -> Option<AtcEffectPlan> {
        self.ledger.get(decision_id).map(|record| {
            self.effect_plan_from_record(
                record,
                timestamp_micros,
                kind,
                category,
                family,
                agent,
                project_key,
                message,
            )
        })
    }

    #[allow(clippy::too_many_lines)]
    fn run_tick(&mut self, now_micros: i64) -> AtcTickReport {
        let total_started = Instant::now();
        self.tick_count = self.tick_count.saturating_add(1);
        self.resolve_stale_liveness_feedback(now_micros);
        self.prune_stale_conflicts(now_micros);

        let decision_fallback_reason = self.current_release_guard_reason();
        let decision_fallback_active = decision_fallback_reason.is_some();
        let decision_mode = self.budget_mode();
        let candidate_policy = self.shadow_policy.candidate.clone();

        let mut actions = Vec::new();
        let mut effects = Vec::new();
        let mut timings = AtcStageTimings::default();

        let liveness_started = Instant::now();
        let liveness = self.evaluate_liveness(now_micros);
        timings.liveness_micros = elapsed_micros(liveness_started);
        for (agent_name, action) in &liveness.actions {
            let decision_id = liveness
                .decision_metadata
                .get(agent_name)
                .map(|metadata| metadata.decision_id);
            match *action {
                LivenessAction::Suspect => {
                    let message = format!(
                        "[ATC] {agent_name} has been inactive beyond its normal rhythm. Reply or acknowledge if the session is still active; no release has been requested."
                    );
                    actions.push(AtcTickAction::SendAdvisory {
                        agent: agent_name.clone(),
                        message: message.clone(),
                    });
                    if let Some(decision_id) = decision_id
                        && let Some(effect) = self.effect_plan_for_decision_id(
                            decision_id,
                            now_micros,
                            "send_advisory",
                            "liveness",
                            "liveness_monitoring",
                            agent_name.clone(),
                            self.agent_project_key(agent_name),
                            Some(message),
                        )
                    {
                        effects.push(effect);
                    }
                }
                LivenessAction::ReleaseReservations => {
                    actions.push(AtcTickAction::ReleaseReservations {
                        agent: agent_name.clone(),
                    });
                    if let Some(decision_id) = decision_id
                        && let Some(effect) = self.effect_plan_for_decision_id(
                            decision_id,
                            now_micros,
                            "release_reservations_requested",
                            "liveness",
                            "reservation_release",
                            agent_name.clone(),
                            self.agent_project_key(agent_name),
                            None,
                        )
                    {
                        effects.push(effect);
                    }
                }
                LivenessAction::DeclareAlive => {}
            }
        }

        let deadlock_started = Instant::now();
        let deadlocks = self.detect_deadlocks();
        timings.deadlock_micros = elapsed_micros(deadlock_started);
        for (project, cycles) in &deadlocks {
            for cycle in cycles {
                let subject = cycle.join(" → ");
                let evidence_summary =
                    format!("deterministic deadlock cycle in {project}: {subject}");

                // Causal Bottleneck Analysis
                let mut best_target = cycle[0].clone();
                let mut max_score = -1.0;
                let graph = self.conflict_graphs.get(project);

                for agent in cycle {
                    let survival_rate = self
                        .agents
                        .get(agent)
                        .map(|e| e.core.posterior_probability(LivenessState::Alive))
                        .unwrap_or(1.0);

                    let vcg_priority = graph
                        .and_then(|g| g.hard_edges.get(agent))
                        .map(|edges| edges.len() as f64)
                        .unwrap_or(1.0);

                    // Score = Expected wait reduction weighted by probability they are already stuck/dead
                    let score = vcg_priority * (1.0 - survival_rate + 0.01);
                    if score > max_score {
                        max_score = score;
                        best_target = agent.clone();
                    }
                }

                let decision_id = self.ledger.record_event(&EventDecisionBuilder {
                    subsystem: AtcSubsystem::Conflict,
                    decision_class: "deadlock_cycle",
                    subject: &subject,
                    policy_id: None,
                    posterior: Vec::new(),
                    action: "AdvisoryMessage",
                    expected_loss: 0.0,
                    runner_up_loss: 0.0,
                    loss_table: Vec::new(),
                    evidence_summary: &evidence_summary,
                    calibration_healthy: !decision_fallback_active,
                    safe_mode_active: decision_fallback_active,
                    fallback_reason: decision_fallback_reason.as_deref(),
                    timestamp_micros: now_micros,
                });

                // Deadlock cycles remain advisory-only unless independent liveness evidence
                // supports a reservation release. The semantics and operator guidance for
                // deadlock remediation are explicitly manual/stale-holder review, so do not
                // auto-release a live agent merely because they participate in a cycle.
                let fallback_suffix = decision_fallback_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| {
                        format!(" Automated release remains disabled while {reason}.")
                    });
                let message = format!(
                    "[ATC] Deadlock in {project}: {subject}. {} is the likeliest stale-holder bottleneck; inspect the cycle and release only inactive work if safe.{}",
                    best_target, fallback_suffix
                );

                // Notify all agents in the cycle so they are aware of the bottleneck.
                for agent in cycle {
                    actions.push(AtcTickAction::SendAdvisory {
                        agent: agent.clone(),
                        message: message.clone(),
                    });
                    if let Some(effect) = self.effect_plan_for_decision_id(
                        decision_id,
                        now_micros,
                        "send_advisory",
                        "conflict",
                        "deadlock_remediation",
                        agent.clone(),
                        Some(project.clone()),
                        Some(message.clone()),
                    ) {
                        effects.push(effect);
                    }
                }
            }
        }

        let excluded_agents: HashSet<String> = liveness
            .actions
            .iter()
            .filter_map(|(agent, action)| {
                (*action == LivenessAction::ReleaseReservations).then_some(agent.clone())
            })
            .collect();
        let estimated_probe_cost_micros = self.cost_model.estimated_probe_cost_micros().max(1);
        let projected_non_probe_micros = self
            .cost_model
            .estimated_non_probe_micros()
            .saturating_add(estimated_probe_cost_micros);
        let available_budget_micros = self
            .config
            .tick_budget_micros
            .saturating_sub(projected_non_probe_micros);
        let probe_budget_micros = floor_f64_to_u64(
            u64_to_f64(available_budget_micros)
                * self.incumbent_policy.probe_budget_fraction(decision_mode),
        )
        .min(self.config.tick_budget_micros);
        let debt_based_probe_limit =
            if self.slow_controller.budget_debt_micros() > self.config.tick_budget_micros {
                1
            } else {
                usize::MAX
            };
        let effective_probe_limit = self
            .slow_controller
            .probe_limit
            .min(self.incumbent_policy.max_probes(decision_mode))
            .min(debt_based_probe_limit);

        let probe_started = Instant::now();
        let incumbent_probe_candidates = budgeted_probe_schedule(
            &self.agents,
            &excluded_agents,
            &self.incumbent_policy,
            decision_mode,
            effective_probe_limit,
            probe_budget_micros,
            estimated_probe_cost_micros,
            now_micros,
        );
        let incumbent_probe_agents: Vec<String> = incumbent_probe_candidates
            .iter()
            .map(|candidate| candidate.agent.clone())
            .collect();
        if let Some(candidate) = candidate_policy.as_ref() {
            let candidate_probe_budget_micros = floor_f64_to_u64(
                u64_to_f64(available_budget_micros)
                    * candidate.probe_budget_fraction(decision_mode),
            )
            .min(self.config.tick_budget_micros);
            let candidate_probe_agents: Vec<String> = budgeted_probe_schedule(
                &self.agents,
                &excluded_agents,
                candidate,
                decision_mode,
                effective_probe_limit,
                candidate_probe_budget_micros,
                estimated_probe_cost_micros,
                now_micros,
            )
            .into_iter()
            .map(|candidate| candidate.agent)
            .collect();
            self.shadow_policy
                .record_probe_disagreement(&incumbent_probe_agents, &candidate_probe_agents);
        }
        timings.probe_micros = elapsed_micros(probe_started);

        for candidate in incumbent_probe_candidates {
            if let Some(entry) = self.agents.get_mut(&candidate.agent) {
                entry.probe_sent_at = now_micros;
            }
            let incumbent_k = Self::effective_threshold(
                self.incumbent_policy.suspicion_k,
                current_adaptive_threshold_k(&candidate.agent),
                decision_fallback_active,
            );
            self.reschedule_agent(&candidate.agent, now_micros, incumbent_k);
            let posterior = self
                .agents
                .get(&candidate.agent)
                .map_or_else(Vec::new, |entry| entry.core.posterior_summary());
            let evidence_summary = format!(
                "selected for probing with gain_per_micro {:.4} in {} mode",
                candidate.gain_per_micro,
                decision_mode.as_str()
            );
            let probe_loss = 1.0 / candidate.gain_per_micro.max(1e-6);
            let decision_id = self.ledger.record_event(&EventDecisionBuilder {
                subsystem: AtcSubsystem::Synthesis,
                decision_class: "probe_schedule",
                subject: &candidate.agent,
                policy_id: Some(&self.incumbent_policy.policy_id),
                posterior,
                action: "ProbeAgent",
                expected_loss: probe_loss,
                runner_up_loss: probe_loss + 1.0,
                loss_table: vec![
                    AtcLossTableEntry {
                        action: "ProbeAgent".to_string(),
                        expected_loss: probe_loss,
                    },
                    AtcLossTableEntry {
                        action: "DeferProbe".to_string(),
                        expected_loss: probe_loss + 1.0,
                    },
                ],
                evidence_summary: &evidence_summary,
                calibration_healthy: !decision_fallback_active,
                safe_mode_active: decision_fallback_active,
                fallback_reason: decision_fallback_reason.as_deref(),
                timestamp_micros: now_micros,
            });
            actions.push(AtcTickAction::ProbeAgent {
                agent: candidate.agent.clone(),
            });
            let probe_project_key = self.agent_project_key(&candidate.agent);
            if let Some(effect) = self.effect_plan_for_decision_id(
                decision_id,
                now_micros,
                "probe_agent",
                "probe",
                "liveness_probe",
                candidate.agent,
                probe_project_key,
                None,
            ) {
                effects.push(effect);
            }
        }

        let gating_started = Instant::now();
        let mut withheld_releases = Vec::new();
        if let Some(conformal_lock) = ATC_CONFORMAL.get()
            && let Ok(conformal) = conformal_lock.lock()
            && conformal.is_uncertain(
                AtcSubsystem::Liveness,
                self.liveness_core.max_possible_loss(),
            )
        {
            actions.retain(|action| match action {
                AtcTickAction::ReleaseReservations { agent } => {
                    withheld_releases.push(agent.clone());
                    false
                }
                _ => true,
            });
            effects.retain(|effect| {
                if effect.kind == "release_reservations_requested" {
                    withheld_releases.push(effect.agent.clone());
                    false
                } else {
                    true
                }
            });
            withheld_releases.sort();
            withheld_releases.dedup();
        }

        for agent_name in withheld_releases {
            if let Some(entry) = self.agents.get_mut(&agent_name)
                && entry.state == LivenessState::Dead
            {
                entry.state = LivenessState::Flaky;
                if entry.suspect_since == 0 {
                    entry.suspect_since = now_micros;
                }
            }
            let incumbent_k = Self::effective_threshold(
                self.incumbent_policy.suspicion_k,
                current_adaptive_threshold_k(&agent_name),
                true,
            );
            self.reschedule_agent(&agent_name, now_micros, incumbent_k);
            let posterior = self
                .agents
                .get(&agent_name)
                .map_or_else(Vec::new, |entry| entry.core.posterior_summary());
            let evidence_summary =
                "conformal uncertainty forced deterministic withholding of release".to_string();
            let decision_id = self.ledger.record_event(&EventDecisionBuilder {
                subsystem: AtcSubsystem::Calibration,
                decision_class: "withheld_release",
                subject: &agent_name,
                policy_id: Some(&self.incumbent_policy.policy_id),
                posterior,
                action: "WithholdRelease",
                expected_loss: 0.0,
                runner_up_loss: self
                    .incumbent_policy
                    .expected_loss(LivenessAction::ReleaseReservations, &[]),
                loss_table: vec![
                    AtcLossTableEntry {
                        action: "WithholdRelease".to_string(),
                        expected_loss: 0.0,
                    },
                    AtcLossTableEntry {
                        action: "ReleaseReservations".to_string(),
                        expected_loss: 1.0,
                    },
                ],
                evidence_summary: &evidence_summary,
                calibration_healthy: false,
                safe_mode_active: true,
                fallback_reason: Some("conformal_uncertainty"),
                timestamp_micros: now_micros,
            });
            let message = format!(
                "[ATC] {agent_name} looks inactive, but ATC withheld automated release because the liveness evidence is still uncertain. Inspect the session or request an acknowledgment before cleanup."
            );
            actions.push(AtcTickAction::SendAdvisory {
                agent: agent_name.clone(),
                message: message.clone(),
            });
            if let Some(effect) = self.effect_plan_for_decision_id(
                decision_id,
                now_micros,
                "send_advisory",
                "calibration",
                "withheld_release_notice",
                agent_name.clone(),
                self.agent_project_key(&agent_name),
                Some(message),
            ) {
                effects.push(effect);
            }
        }

        let release_targets: Vec<String> = actions
            .iter()
            .filter_map(|action| match action {
                AtcTickAction::ReleaseReservations { agent } => Some(agent.clone()),
                _ => None,
            })
            .collect();
        for agent_name in release_targets {
            let message = format!(
                "[ATC] {agent_name} crossed the dead-agent release threshold. ATC requested reservation release; inspect the worktree if the agent is still active."
            );
            let project_key = self.agent_project_key(&agent_name);
            actions.push(AtcTickAction::SendAdvisory {
                agent: agent_name.clone(),
                message: message.clone(),
            });
            if let Some(metadata) = liveness.decision_metadata.get(&agent_name)
                && let Some(effect) = self.effect_plan_for_decision_id(
                    metadata.decision_id,
                    now_micros,
                    "send_advisory",
                    "liveness",
                    "release_notice",
                    agent_name,
                    project_key,
                    Some(message),
                )
            {
                effects.push(effect);
            }
        }
        timings.gating_micros = elapsed_micros(gating_started);

        let slow_started = Instant::now();
        if self.tick_count % 50 == 0
            && let Some(pop_lock) = ATC_POPULATION.get()
            && let Ok(mut pop) = pop_lock.lock()
        {
            self.absorb_population_snapshot(&mut pop);
        }
        let pre_summary_total_micros = elapsed_micros(total_started);
        let budget_exceeded = pre_summary_total_micros > self.config.tick_budget_micros;
        let utilization_ratio = u64_to_f64(pre_summary_total_micros)
            / u64_to_f64(self.config.tick_budget_micros.max(1));
        self.slow_controller.note_tick(
            pre_summary_total_micros,
            self.config.tick_budget_micros,
            utilization_ratio,
            budget_exceeded,
            self.incumbent_policy.max_probes_per_tick,
        );
        timings.slow_control_micros = elapsed_micros(slow_started);

        let reported_mode = self.budget_mode();
        let reported_fallback_reason = self.current_release_guard_reason();
        let next_due_micros = self.next_scheduled_review_micros();

        let kernel = AtcKernelTelemetry {
            due_agents: liveness.due_agents,
            scheduled_agents: self.scheduled_agent_count(),
            next_due_micros,
            dirty_agents: self.dirty_agents.len(),
            dirty_projects: self.dirty_projects.len(),
            pending_effects: effects.len(),
            lock_wait_micros: 0,
            deadlock_cache_hits: self.deadlock_cache_hits,
            deadlock_cache_misses: self.deadlock_cache_misses,
            deadlock_cache_hit_rate: if self.deadlock_cache_hits + self.deadlock_cache_misses == 0 {
                1.0
            } else {
                u64_to_f64(self.deadlock_cache_hits)
                    / u64_to_f64(self.deadlock_cache_hits + self.deadlock_cache_misses)
            },
        };
        let budget = AtcBudgetTelemetry {
            mode: reported_mode.as_str().to_string(),
            tick_budget_micros: self.config.tick_budget_micros,
            probe_budget_micros,
            estimated_probe_cost_micros,
            max_probes_this_tick: self
                .slow_controller
                .probe_limit
                .min(self.incumbent_policy.max_probes(reported_mode)),
            budget_debt_micros: self.slow_controller.budget_debt_micros(),
            utilization_ratio,
            slow_window_utilization: self.slow_controller.last_utilization_ratio,
            kernel_total_micros: pre_summary_total_micros,
        };
        let policy = AtcPolicyTelemetry {
            bundle_id: self.policy_bundle.bundle_id.clone(),
            bundle_hash: self.policy_bundle.bundle_hash.clone(),
            incumbent_policy_id: self.incumbent_policy.policy_id.clone(),
            incumbent_artifact_hash: self.incumbent_policy.artifact_hash.clone(),
            candidate_policy_id: candidate_policy
                .as_ref()
                .map(|candidate| candidate.policy_id.clone()),
            candidate_artifact_hash: candidate_policy
                .as_ref()
                .map(|candidate| candidate.artifact_hash.clone()),
            shadow_enabled: candidate_policy.is_some(),
            shadow_disagreements: self.shadow_policy.disagreements,
            shadow_regret_avg: self.shadow_policy.average_regret(),
            decision_mode: decision_mode.as_str().to_string(),
            fallback_active: reported_fallback_reason.is_some(),
            fallback_reason: reported_fallback_reason,
        };

        let summary_started = Instant::now();
        timings.total_micros = elapsed_micros(total_started);
        let mut summary =
            self.build_summary_snapshot_with(now_micros, &timings, &kernel, &budget, &policy);
        timings.summary_micros = elapsed_micros(summary_started);
        timings.total_micros = elapsed_micros(total_started);
        summary.stage_timings = timings.clone();

        self.last_stage_timings = timings;
        self.last_kernel_telemetry = kernel;
        self.last_budget_telemetry = budget;
        self.last_policy_telemetry = policy;

        self.cost_model
            .update(&self.last_stage_timings, incumbent_probe_agents.len());

        AtcTickReport {
            actions,
            effects,
            summary,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Track 5 + Track 8 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod calibration_tests {
    use super::*;

    fn make_miscalibrated_eprocess() -> EProcessMonitor {
        let mut monitor = EProcessMonitor::new(0.85, 20.0);
        for _ in 0..200 {
            monitor.update(false, AtcSubsystem::Liveness, None);
        }
        assert!(monitor.miscalibrated());
        monitor
    }

    fn make_healthy_eprocess() -> EProcessMonitor {
        EProcessMonitor::new(0.85, 20.0)
    }

    fn make_degraded_cusum() -> CusumDetector {
        let mut cusum = CusumDetector::new(0.15, 5.0, 0.1);
        for i in 0..100 {
            cusum.update(true, i * 1_000_000);
        }
        assert!(cusum.degradation_detected());
        cusum
    }

    fn make_clean_cusum() -> CusumDetector {
        CusumDetector::new(0.15, 5.0, 0.1)
    }

    #[test]
    fn starts_not_in_safe_mode() {
        let guard = CalibrationGuard::new(20);
        assert!(!guard.is_safe_mode());
    }

    #[test]
    fn enters_safe_mode_on_eprocess_alert() {
        let mut guard = CalibrationGuard::new(20);
        let eprocess = make_miscalibrated_eprocess();
        let cusum = make_clean_cusum();
        let changed = guard.update(&eprocess, &cusum, false, 1_000_000);
        assert!(changed, "should return true when state changes");
        assert!(guard.is_safe_mode());
    }

    #[test]
    fn enters_safe_mode_on_cusum_degradation() {
        let mut guard = CalibrationGuard::new(20);
        let eprocess = make_healthy_eprocess();
        let cusum = make_degraded_cusum();
        guard.update(&eprocess, &cusum, true, 1_000_000);
        assert!(guard.is_safe_mode());
    }

    #[test]
    fn exits_safe_mode_after_recovery() {
        let mut guard = CalibrationGuard::new(20);
        let eprocess = make_miscalibrated_eprocess();
        let cusum = make_clean_cusum();
        guard.update(&eprocess, &cusum, false, 1_000_000);
        assert!(guard.is_safe_mode());

        let healthy_ep = make_healthy_eprocess();
        for i in 0..20 {
            guard.update(&healthy_ep, &cusum, true, (i + 2) * 1_000_000);
        }
        assert!(!guard.is_safe_mode());
    }

    #[test]
    fn does_not_exit_if_eprocess_still_miscalibrated() {
        let mut guard = CalibrationGuard::new(20);
        let eprocess = make_miscalibrated_eprocess();
        let cusum = make_clean_cusum();
        guard.update(&eprocess, &cusum, false, 1_000_000);
        assert!(guard.is_safe_mode());

        for i in 0..20 {
            guard.update(&eprocess, &cusum, true, (i + 2) * 1_000_000);
        }
        assert!(
            guard.is_safe_mode(),
            "should NOT exit while eprocess is miscalibrated"
        );
    }

    #[test]
    fn operator_override_forces_safe_mode() {
        let mut guard = CalibrationGuard::new(20);
        guard.set_safe_mode(true, 1_000_000);
        assert!(guard.is_safe_mode());
        guard.set_safe_mode(false, 2_000_000);
        assert!(!guard.is_safe_mode());
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn engine_creates_with_defaults() {
        let engine = AtcEngine::new_for_testing();
        assert!(engine.enabled());
        assert!(!engine.registered());
        assert!(!engine.is_safe_mode());
        assert_eq!(engine.tick_count(), 0);
    }

    #[test]
    fn engine_registers_agent() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);
        assert!(engine.agent_liveness("BlueFox").is_some());
        assert_eq!(engine.agent_liveness("BlueFox"), Some(LivenessState::Alive));
    }

    #[test]
    fn engine_excludes_atc_from_registration() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent(ATC_AGENT_NAME, "mcp-agent-mail", None);
        assert!(engine.agent_liveness(ATC_AGENT_NAME).is_none());
    }

    #[test]
    fn engine_excludes_atc_from_activity() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);
        engine.observe_activity(ATC_AGENT_NAME, None, 1_000_000);
        // ATC activity should be silently ignored
        assert!(engine.agent_liveness(ATC_AGENT_NAME).is_none());
    }

    #[test]
    fn activity_resets_to_alive() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);

        // Manually set to Flaky
        engine.agents.get_mut("BlueFox").unwrap().state = LivenessState::Flaky;
        assert_eq!(engine.agent_liveness("BlueFox"), Some(LivenessState::Flaky));

        // Activity resets to Alive
        engine.observe_activity("BlueFox", None, 1_000_000);
        assert_eq!(engine.agent_liveness("BlueFox"), Some(LivenessState::Alive));
    }

    #[test]
    fn evaluate_liveness_detects_silent_agent() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);

        // Establish a rhythm (60s intervals, 10 observations)
        for i in 0..10 {
            engine.observe_activity("BlueFox", None, i * 60_000_000);
        }

        // 5 minutes of silence (5× the 60s avg).  The posterior update is
        // incremental — each evaluate call pushes the posterior further from
        // the strong alive prior.  Simulate multiple tick evaluations.
        let base = 9 * 60_000_000;
        let mut any_action = false;
        let mut last_evaluation = LivenessEvaluation::default();
        for tick in 1..=10 {
            let now = base + tick * 30_000_000; // every 30s
            last_evaluation = engine.evaluate_liveness(now);
            if !last_evaluation.actions.is_empty() {
                any_action = true;
                break;
            }
        }
        assert!(
            any_action,
            "should detect silent agent within 10 evaluation ticks"
        );

        // Verify the action targets BlueFox
        let (agent, action) = &last_evaluation.actions[0];
        assert_eq!(agent, "BlueFox");
        assert!(
            *action == LivenessAction::Suspect || *action == LivenessAction::ReleaseReservations,
            "action should be Suspect or Release, got {action:?}"
        );
    }

    #[test]
    fn evaluate_liveness_ignores_active_agent() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);

        // Agent is active right now
        for i in 0..10 {
            engine.observe_activity("BlueFox", None, i * 60_000_000);
        }
        let now = 9 * 60_000_000 + 30_000_000; // only 30s since last activity
        let evaluation = engine.evaluate_liveness(now);
        assert!(
            evaluation.actions.is_empty(),
            "active agent should not trigger any action"
        );
    }

    #[test]
    fn safe_mode_blocks_release() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);
        engine.set_safe_mode(true, 0);

        // Establish rhythm then go very silent
        for i in 0..10 {
            engine.observe_activity("BlueFox", None, i * 60_000_000);
        }
        // Force posterior toward Dead
        let entry = engine.agents.get_mut("BlueFox").unwrap();
        for _ in 0..30 {
            entry.core.update_posterior(&[
                (LivenessState::Alive, 0.01),
                (LivenessState::Flaky, 0.05),
                (LivenessState::Dead, 0.95),
            ]);
        }

        let now = 9 * 60_000_000 + 600_000_000; // 10 min silence
        let evaluation = engine.evaluate_liveness(now);

        // In safe mode, even if the core recommends Release, the state
        // transition is downgraded to Flaky (Suspect), never Dead.
        let state = engine.agent_liveness("BlueFox").unwrap();
        assert_ne!(
            state,
            LivenessState::Dead,
            "safe mode should prevent Dead state, got {state:?}"
        );
        assert!(
            evaluation
                .actions
                .iter()
                .any(|(agent, action)| agent == "BlueFox" && *action == LivenessAction::Suspect),
            "safe mode should downgrade release actions to Suspect"
        );
        assert!(
            !evaluation
                .actions
                .iter()
                .any(|(agent, action)| agent == "BlueFox"
                    && *action == LivenessAction::ReleaseReservations),
            "safe mode should not emit release actions"
        );
    }

    #[test]
    fn detect_deadlocks_empty_graphs() {
        let mut engine = AtcEngine::new_for_testing();
        assert!(engine.detect_deadlocks().is_empty());
    }

    #[test]
    fn detect_deadlocks_finds_cycle() {
        let mut engine = AtcEngine::new_for_testing();
        let mut graph = ProjectConflictGraph::default();
        graph.hard_edges.insert(
            "AgentA".to_string(),
            vec![HardEdge {
                holder: "AgentA".to_string(),
                blocked: "AgentB".to_string(),
                contested_patterns: vec!["src/lib.rs".to_string()],
                since: 1,
            }],
        );
        graph.hard_edges.insert(
            "AgentB".to_string(),
            vec![HardEdge {
                holder: "AgentB".to_string(),
                blocked: "AgentA".to_string(),
                contested_patterns: vec!["src/main.rs".to_string()],
                since: 2,
            }],
        );
        engine
            .conflict_graphs
            .insert("test-project".to_string(), graph);

        let deadlocks = engine.detect_deadlocks();
        assert_eq!(deadlocks.len(), 1);
        assert_eq!(deadlocks[0].0, "test-project");
        assert_eq!(deadlocks[0].1.len(), 1);
        assert_eq!(deadlocks[0].1[0].len(), 2);
    }

    #[test]
    fn is_self_event_filters_correctly() {
        assert!(AtcEngine::is_self_event(ATC_AGENT_NAME, &[]));
        assert!(AtcEngine::is_self_event(
            "other",
            &[ATC_AGENT_NAME.to_string()]
        ));
        assert!(!AtcEngine::is_self_event("other", &["another".to_string()]));
    }

    #[test]
    fn ledger_records_liveness_decisions() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code", None);

        // Establish rhythm then go silent
        for i in 0..10 {
            engine.observe_activity("BlueFox", None, i * 60_000_000);
        }
        let now = 9 * 60_000_000 + 300_000_000;
        let _actions = engine.evaluate_liveness(now);

        // Check that decisions were logged
        if !engine.ledger().is_empty() {
            let record = engine.ledger().recent(1).next().unwrap();
            assert_eq!(record.subsystem, AtcSubsystem::Liveness);
            assert_eq!(record.subject, "BlueFox");
        }
    }

    // ── Engine boundary condition tests (br-oco5x) ────────────────────

    #[test]
    fn dead_agent_resurrection_via_observe_activity() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("Phoenix", "claude-code", None);

        // Manually set to Dead
        if let Some(entry) = engine.agents.get_mut("Phoenix") {
            entry.state = LivenessState::Dead;
            entry.suspect_since = 1_000_000;
            entry.sprt_log_lr = 5.0;
        }
        assert_eq!(
            engine.agent_liveness("Phoenix"),
            Some(LivenessState::Dead),
            "agent should be Dead before resurrection"
        );

        // observe_activity should resurrect from Dead → Alive
        engine.observe_activity("Phoenix", None, 2_000_000);

        assert_eq!(
            engine.agent_liveness("Phoenix"),
            Some(LivenessState::Alive),
            "agent should be Alive after observe_activity resurrection"
        );
        // SPRT and suspect fields should be cleared
        let entry = engine.agents.get("Phoenix").unwrap();
        assert_eq!(entry.suspect_since, 0, "suspect_since should be cleared");
        assert!(
            entry.sprt_log_lr.abs() < f64::EPSILON,
            "sprt_log_lr should be cleared, got {}",
            entry.sprt_log_lr
        );
    }

    #[test]
    fn evaluate_liveness_skips_dead_agents() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("Zombie", "claude-code", None);

        // Establish rhythm then set to Dead
        for i in 0..10 {
            engine.observe_activity("Zombie", None, i * 60_000_000);
        }
        if let Some(entry) = engine.agents.get_mut("Zombie") {
            entry.state = LivenessState::Dead;
        }

        // Long silence — but Dead agents should be skipped
        let now = 9 * 60_000_000 + 600_000_000; // 10 min silence
        let evaluation = engine.evaluate_liveness(now);

        // No actions should be generated for Dead agents
        let zombie_actions: Vec<_> = evaluation
            .actions
            .iter()
            .filter(|(name, _)| name == "Zombie")
            .collect();
        assert!(
            zombie_actions.is_empty(),
            "evaluate_liveness should skip Dead agents, got {zombie_actions:?}"
        );
    }

    #[test]
    fn observe_activity_for_unregistered_agent() {
        let mut engine = AtcEngine::new_for_testing();
        let agents_before = engine.tracked_agents().len();

        // Call observe_activity for an agent that was never registered
        engine.observe_activity("GhostAgent", None, 1_000_000);

        // Should not panic, should not create a new entry
        assert_eq!(
            engine.tracked_agents().len(),
            agents_before,
            "unregistered agent should not be auto-created"
        );
        assert_eq!(
            engine.agent_liveness("GhostAgent"),
            None,
            "unregistered agent should return None"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Session Synthesizer (Track 7)
// ──────────────────────────────────────────────────────────────────────

/// Monotonically-growing session summary.
///
/// Every field only increases — counters increment, sets grow, timestamps
/// advance.  This guarantees two consecutive reads always see the same or
/// more information (no regressions).
///
/// NOT a pure join-semilattice (counters use increment, not max).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    // ── Monotone Counters (increment only) ──
    pub messages_sent: u64,
    pub messages_received: u64,
    pub reservations_granted: u64,
    pub reservations_released: u64,
    pub reservations_renewed: u64,
    pub reservations_force_released: u64,
    pub conflicts_detected: u64,
    pub conflicts_resolved: u64,
    pub atc_interventions: u64,

    // ── Sets (union only) ──
    pub active_agents: HashSet<String>,
    pub active_threads: HashSet<String>,

    // ── Timestamps (max only) ──
    /// First event ever absorbed (set once, never changes).
    pub first_event_ts: i64,
    /// Most recent event timestamp.
    pub last_event_ts: i64,

    // ── Per-entity counters ──
    pub agent_message_counts: HashMap<String, u64>,
    pub thread_message_counts: HashMap<String, u64>,

    // ── Incremental rendering ──
    input_generation: u64,
    rendered_generation: u64,
    cached_output: Option<String>,
}

/// Recognized event types for session synthesis.
#[derive(Debug, Clone)]
pub enum SynthesisEvent {
    MessageSent {
        from: String,
        to: Vec<String>,
        thread_id: Option<String>,
        timestamp_micros: i64,
    },
    MessageReceived {
        agent: String,
        timestamp_micros: i64,
    },
    ReservationGranted {
        agent: String,
        timestamp_micros: i64,
    },
    ReservationReleased {
        agent: String,
        timestamp_micros: i64,
    },
    ReservationRenewed {
        agent: String,
        timestamp_micros: i64,
    },
    ReservationForceReleased {
        agent: String,
        timestamp_micros: i64,
    },
    ConflictDetected {
        timestamp_micros: i64,
    },
    ConflictResolved {
        timestamp_micros: i64,
    },
    AtcIntervention {
        timestamp_micros: i64,
    },
}

impl SynthesisEvent {
    /// Get the timestamp of this event.
    #[must_use]
    pub const fn timestamp_micros(&self) -> i64 {
        match self {
            Self::MessageSent {
                timestamp_micros, ..
            }
            | Self::MessageReceived {
                timestamp_micros, ..
            }
            | Self::ReservationGranted {
                timestamp_micros, ..
            }
            | Self::ReservationReleased {
                timestamp_micros, ..
            }
            | Self::ReservationRenewed {
                timestamp_micros, ..
            }
            | Self::ReservationForceReleased {
                timestamp_micros, ..
            }
            | Self::ConflictDetected { timestamp_micros }
            | Self::ConflictResolved { timestamp_micros }
            | Self::AtcIntervention { timestamp_micros } => *timestamp_micros,
        }
    }
}

impl SessionSummary {
    /// Absorb a single event.  O(1) per event.
    pub fn absorb(&mut self, event: &SynthesisEvent) {
        let ts = event.timestamp_micros();
        self.last_event_ts = self.last_event_ts.max(ts);
        if self.first_event_ts == 0 {
            self.first_event_ts = ts;
        }
        self.input_generation += 1;

        match event {
            SynthesisEvent::MessageSent {
                from,
                to,
                thread_id,
                ..
            } => {
                self.messages_sent += 1;
                self.active_agents.insert(from.clone());
                for recipient in to {
                    self.active_agents.insert(recipient.clone());
                }
                *self.agent_message_counts.entry(from.clone()).or_insert(0) += 1;
                if let Some(tid) = thread_id {
                    self.active_threads.insert(tid.clone());
                    *self.thread_message_counts.entry(tid.clone()).or_insert(0) += 1;
                }
            }
            SynthesisEvent::MessageReceived { agent, .. } => {
                self.messages_received += 1;
                self.active_agents.insert(agent.clone());
            }
            SynthesisEvent::ReservationGranted { agent, .. } => {
                self.reservations_granted += 1;
                self.active_agents.insert(agent.clone());
            }
            SynthesisEvent::ReservationReleased { agent, .. } => {
                self.reservations_released += 1;
                self.active_agents.insert(agent.clone());
            }
            SynthesisEvent::ReservationRenewed { agent, .. } => {
                self.reservations_renewed += 1;
                self.active_agents.insert(agent.clone());
            }
            SynthesisEvent::ReservationForceReleased { agent, .. } => {
                self.reservations_force_released += 1;
                self.active_agents.insert(agent.clone());
            }
            SynthesisEvent::ConflictDetected { .. } => {
                self.conflicts_detected += 1;
            }
            SynthesisEvent::ConflictResolved { .. } => {
                self.conflicts_resolved += 1;
            }
            SynthesisEvent::AtcIntervention { .. } => {
                self.atc_interventions += 1;
            }
        }
    }

    /// Format a human-readable summary.  Uses incremental rendering —
    /// only regenerates the string when inputs have changed.
    pub fn formatted(&mut self) -> &str {
        if self.rendered_generation < self.input_generation {
            self.cached_output = Some(self.render());
            self.rendered_generation = self.input_generation;
        }
        self.cached_output.as_deref().unwrap_or("(no data)")
    }

    /// Force a fresh render (bypasses cache).
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines = Vec::new();

        lines.push(format!(
            "Messages: {} sent, {} received across {} threads",
            self.messages_sent,
            self.messages_received,
            self.active_threads.len(),
        ));

        // Top agents by message count
        let mut agent_counts: Vec<(&str, u64)> = self
            .agent_message_counts
            .iter()
            .map(|(a, c)| (a.as_str(), *c))
            .collect();
        agent_counts.sort_by_key(|entry| Reverse(entry.1));
        if !agent_counts.is_empty() {
            let top: Vec<String> = agent_counts
                .iter()
                .take(5)
                .map(|(a, c)| format!("{a}: {c}"))
                .collect();
            lines.push(format!("Top agents: {}", top.join(", ")));
        }

        // Top threads by message count
        let mut thread_counts: Vec<(&str, u64)> = self
            .thread_message_counts
            .iter()
            .map(|(t, c)| (t.as_str(), *c))
            .collect();
        thread_counts.sort_by_key(|entry| Reverse(entry.1));
        if !thread_counts.is_empty() {
            let top: Vec<String> = thread_counts
                .iter()
                .take(5)
                .map(|(t, c)| format!("{t}: {c}"))
                .collect();
            lines.push(format!("Hot threads: {}", top.join(", ")));
        }

        if self.reservations_granted > 0
            || self.reservations_released > 0
            || self.reservations_renewed > 0
            || self.reservations_force_released > 0
        {
            lines.push(format!(
                "Reservations: {} granted, {} released, {} renewed, {} force-released",
                self.reservations_granted,
                self.reservations_released,
                self.reservations_renewed,
                self.reservations_force_released,
            ));
        }

        if self.conflicts_detected > 0 {
            lines.push(format!(
                "Conflicts: {} detected, {} resolved",
                self.conflicts_detected, self.conflicts_resolved,
            ));
        }

        if self.atc_interventions > 0 {
            lines.push(format!("ATC interventions: {}", self.atc_interventions));
        }

        lines.push(format!("Active agents: {}", self.active_agents.len()));

        lines.join("\n")
    }

    /// Check monotonicity: every field in `self` is >= `previous`.
    #[cfg(test)]
    fn is_monotone_vs(&self, previous: &Self) -> bool {
        self.messages_sent >= previous.messages_sent
            && self.messages_received >= previous.messages_received
            && self.reservations_granted >= previous.reservations_granted
            && self.reservations_released >= previous.reservations_released
            && self.reservations_renewed >= previous.reservations_renewed
            && self.reservations_force_released >= previous.reservations_force_released
            && self.conflicts_detected >= previous.conflicts_detected
            && self.conflicts_resolved >= previous.conflicts_resolved
            && self.atc_interventions >= previous.atc_interventions
            && self.last_event_ts >= previous.last_event_ts
            && previous.active_agents.is_subset(&self.active_agents)
            && previous.active_threads.is_subset(&self.active_threads)
            // Per-entity counters must also be monotone
            && previous.agent_message_counts.iter().all(|(agent, &prev_count)| {
                self.agent_message_counts.get(agent).copied().unwrap_or(0) >= prev_count
            })
            && previous.thread_message_counts.iter().all(|(thread, &prev_count)| {
                self.thread_message_counts.get(thread).copied().unwrap_or(0) >= prev_count
            })
    }
}

// ──────────────────────────────────────────────────────────────────────
// Track 7 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod synthesis_tests {
    use super::*;

    fn msg_sent(from: &str, to: &[&str], thread: Option<&str>, ts: i64) -> SynthesisEvent {
        SynthesisEvent::MessageSent {
            from: from.to_string(),
            to: to.iter().map(ToString::to_string).collect(),
            thread_id: thread.map(str::to_string),
            timestamp_micros: ts,
        }
    }

    #[test]
    fn empty_summary() {
        let summary = SessionSummary::default();
        assert_eq!(summary.messages_sent, 0);
        assert_eq!(summary.first_event_ts, 0);
        assert!(summary.active_agents.is_empty());
    }

    #[test]
    fn absorb_message_sent() {
        let mut summary = SessionSummary::default();
        summary.absorb(&msg_sent("BlueFox", &["RedHawk"], Some("t1"), 1_000_000));

        assert_eq!(summary.messages_sent, 1);
        assert!(summary.active_agents.contains("BlueFox"));
        assert!(summary.active_agents.contains("RedHawk"));
        assert!(summary.active_threads.contains("t1"));
        assert_eq!(summary.agent_message_counts.get("BlueFox"), Some(&1));
        assert_eq!(summary.thread_message_counts.get("t1"), Some(&1));
        assert_eq!(summary.first_event_ts, 1_000_000);
        assert_eq!(summary.last_event_ts, 1_000_000);
    }

    #[test]
    fn absorb_multiple_events() {
        let mut summary = SessionSummary::default();
        summary.absorb(&msg_sent("A", &["B"], Some("t1"), 1_000_000));
        summary.absorb(&msg_sent("B", &["A"], Some("t1"), 2_000_000));
        summary.absorb(&msg_sent("C", &["A", "B"], Some("t2"), 3_000_000));

        assert_eq!(summary.messages_sent, 3);
        assert_eq!(summary.active_agents.len(), 3);
        assert_eq!(summary.active_threads.len(), 2);
        assert_eq!(summary.first_event_ts, 1_000_000);
        assert_eq!(summary.last_event_ts, 3_000_000);
    }

    #[test]
    fn absorb_reservation_events() {
        let mut summary = SessionSummary::default();
        summary.absorb(&SynthesisEvent::ReservationGranted {
            agent: "A".to_string(),
            timestamp_micros: 1,
        });
        summary.absorb(&SynthesisEvent::ReservationReleased {
            agent: "A".to_string(),
            timestamp_micros: 2,
        });

        assert_eq!(summary.reservations_granted, 1);
        assert_eq!(summary.reservations_released, 1);
    }

    #[test]
    fn absorb_reservation_renewed_and_force_released_events() {
        let mut summary = SessionSummary::default();
        summary.absorb(&SynthesisEvent::ReservationRenewed {
            agent: "A".to_string(),
            timestamp_micros: 1,
        });
        summary.absorb(&SynthesisEvent::ReservationForceReleased {
            agent: "B".to_string(),
            timestamp_micros: 2,
        });
        summary.absorb(&SynthesisEvent::ReservationRenewed {
            agent: "A".to_string(),
            timestamp_micros: 3,
        });

        assert_eq!(summary.reservations_renewed, 2);
        assert_eq!(summary.reservations_force_released, 1);
        assert!(summary.active_agents.contains("A"));
        assert!(summary.active_agents.contains("B"));
    }

    #[test]
    fn absorb_conflict_events() {
        let mut summary = SessionSummary::default();
        summary.absorb(&SynthesisEvent::ConflictDetected {
            timestamp_micros: 1,
        });
        summary.absorb(&SynthesisEvent::ConflictResolved {
            timestamp_micros: 2,
        });

        assert_eq!(summary.conflicts_detected, 1);
        assert_eq!(summary.conflicts_resolved, 1);
    }

    #[test]
    fn monotonicity_holds() {
        let mut summary = SessionSummary::default();

        for i in 0..20 {
            let prev = summary.clone();
            let from = format!("Agent{}", i % 3);
            let to_name = format!("Agent{}", (i + 1) % 3);
            let thread = format!("t{}", i % 5);
            summary.absorb(&msg_sent(&from, &[&to_name], Some(&thread), i * 1_000_000));
            assert!(
                summary.is_monotone_vs(&prev),
                "monotonicity violated at event {i}"
            );
        }
    }

    #[test]
    fn formatted_caches_output() {
        let mut summary = SessionSummary::default();
        summary.absorb(&msg_sent("A", &["B"], Some("t1"), 1));

        let out1 = summary.formatted().to_string();
        // Same generation → returns cached
        let out2 = summary.formatted().to_string();
        assert_eq!(out1, out2);

        // New event → re-renders
        summary.absorb(&msg_sent("C", &["D"], Some("t2"), 2));
        let out3 = summary.formatted().to_string();
        assert_ne!(out1, out3, "should re-render after new event");
    }

    #[test]
    fn formatted_includes_key_info() {
        let mut summary = SessionSummary::default();
        for i in 0..10 {
            summary.absorb(&msg_sent("BlueFox", &["RedHawk"], Some("feat-1"), i));
        }
        summary.absorb(&SynthesisEvent::ConflictDetected {
            timestamp_micros: 100,
        });
        summary.absorb(&SynthesisEvent::AtcIntervention {
            timestamp_micros: 101,
        });

        let output = summary.formatted().to_string();
        assert!(output.contains("10 sent"), "should show message count");
        assert!(output.contains("BlueFox"), "should show agent name");
        assert!(output.contains("feat-1"), "should show thread");
        assert!(output.contains("Conflicts: 1"), "should show conflicts");
        assert!(
            output.contains("ATC interventions: 1"),
            "should show interventions"
        );
    }

    #[test]
    fn determinism_same_events_same_output() {
        let events = vec![
            msg_sent("A", &["B"], Some("t1"), 1),
            msg_sent("B", &["C"], Some("t2"), 2),
            SynthesisEvent::ReservationGranted {
                agent: "A".to_string(),
                timestamp_micros: 3,
            },
        ];

        let mut s1 = SessionSummary::default();
        let mut s2 = SessionSummary::default();
        for e in &events {
            s1.absorb(e);
            s2.absorb(e);
        }

        assert_eq!(s1.messages_sent, s2.messages_sent);
        assert_eq!(s1.active_agents, s2.active_agents);
        assert_eq!(s1.reservations_granted, s2.reservations_granted);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Load Router (Track 6)
// ──────────────────────────────────────────────────────────────────────

/// Per-agent load model for capacity-based routing.
#[derive(Debug, Clone)]
pub struct AgentLoadModel {
    /// EWMA of messages processed per hour.
    throughput_ewma: f64,
    /// Current active reservation count.
    pub active_reservations: u64,
    /// Current unread inbox count.
    pub pending_inbox: u64,
    /// Predicted capacity [0.0 = fully loaded, 1.0 = idle].
    predicted_capacity: f64,
    /// EWMA of prediction accuracy |predicted - actual|.
    prediction_accuracy: f64,
    /// Number of routing observations.
    observation_count: u64,
}

impl AgentLoadModel {
    /// Create a new load model.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            throughput_ewma: 0.0,
            active_reservations: 0,
            pending_inbox: 0,
            predicted_capacity: 1.0,  // assume idle initially
            prediction_accuracy: 0.5, // neutral accuracy
            observation_count: 0,
        }
    }

    /// Update from observed agent activity.
    pub fn observe_activity(&mut self, messages_processed: u64, interval_secs: f64) {
        if interval_secs > 0.0 {
            let rate = u64_to_f64(messages_processed) / interval_secs * 3600.0;
            self.throughput_ewma = 0.8_f64.mul_add(self.throughput_ewma, 0.2 * rate);
        }
        self.recompute_capacity();
    }

    /// Update reservation and inbox counts.
    pub fn update_counts(&mut self, reservations: u64, inbox: u64) {
        self.active_reservations = reservations;
        self.pending_inbox = inbox;
        self.recompute_capacity();
    }

    fn recompute_capacity(&mut self) {
        // Capacity decreases with more reservations and pending messages
        let reservation_load = (u64_to_f64(self.active_reservations) * 0.15).min(1.0);
        let inbox_load = (u64_to_f64(self.pending_inbox) * 0.05).min(1.0);
        self.predicted_capacity = (1.0 - reservation_load - inbox_load).max(0.0);
    }

    /// Record whether a routing prediction was accurate.
    pub fn record_accuracy(&mut self, actual_response_secs: f64, expected_response_secs: f64) {
        let error = ((actual_response_secs - expected_response_secs)
            / expected_response_secs.max(1.0))
        .abs();
        let correct = if error < 0.5 { 1.0 } else { 0.0 };
        self.prediction_accuracy = 0.9_f64.mul_add(self.prediction_accuracy, 0.1 * correct);
        self.observation_count += 1;
    }

    /// Classical routing score (no prediction needed): 1/(1 + reservations).
    #[must_use]
    pub fn classical_score(&self) -> f64 {
        1.0 / (1.0 + u64_to_f64(self.active_reservations))
    }

    /// Blended routing score using consistency-robustness tradeoff.
    ///
    /// `lambda = prediction_accuracy` (trust the predictor when it's been right).
    /// `score = lambda * predicted_capacity + (1 - lambda) * classical_score()`.
    #[must_use]
    pub fn routing_score(&self) -> f64 {
        let lambda = self.prediction_accuracy;
        lambda.mul_add(
            self.predicted_capacity,
            (1.0 - lambda) * self.classical_score(),
        )
    }
}

impl Default for AgentLoadModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Select the best agent to route a message to.
///
/// Returns `None` if all agents are saturated (all scores below threshold).
#[must_use]
pub fn select_route_target<'a, S: BuildHasher>(
    agents: &'a HashMap<String, AgentLoadModel, S>,
    exclude: &str, // don't route to the requester
    min_score: f64,
) -> Option<&'a str> {
    agents
        .iter()
        .filter(|(name, _)| name.as_str() != exclude && name.as_str() != ATC_AGENT_NAME)
        .max_by(|(_, a), (_, b)| a.routing_score().total_cmp(&b.routing_score()))
        .filter(|(_, model)| model.routing_score() >= min_score)
        .map(|(name, _)| name.as_str())
}

// ──────────────────────────────────────────────────────────────────────
// Track 6 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod load_routing_tests {
    use super::*;

    #[test]
    fn new_agent_has_high_capacity() {
        let model = AgentLoadModel::new();
        assert!((model.predicted_capacity - 1.0).abs() < f64::EPSILON);
        assert!(model.routing_score() > 0.5);
    }

    #[test]
    fn reservations_reduce_capacity() {
        let mut model = AgentLoadModel::new();
        model.update_counts(5, 0);
        assert!(
            model.predicted_capacity < 0.5,
            "5 reservations should reduce capacity"
        );
        assert!(model.routing_score() < 0.5);
    }

    #[test]
    fn pending_inbox_reduces_capacity() {
        let mut model = AgentLoadModel::new();
        model.update_counts(0, 10);
        assert!(
            model.predicted_capacity < 0.8,
            "10 pending messages should reduce capacity"
        );
    }

    #[test]
    fn classical_score_inversely_proportional_to_reservations() {
        let mut m0 = AgentLoadModel::new();
        let mut m5 = AgentLoadModel::new();
        m0.update_counts(0, 0);
        m5.update_counts(5, 0);
        assert!(m0.classical_score() > m5.classical_score());
    }

    #[test]
    fn lambda_anneals_toward_classical_when_inaccurate() {
        let mut model = AgentLoadModel::new();
        // Record many inaccurate predictions
        for _ in 0..50 {
            model.record_accuracy(100.0, 10.0); // 10× off
        }
        // prediction_accuracy should be near 0 → routing_score ≈ classical
        let blended = model.routing_score();
        let classical = model.classical_score();
        assert!(
            (blended - classical).abs() < 0.15,
            "with low accuracy, blended should approach classical"
        );
    }

    #[test]
    fn select_route_picks_least_loaded() {
        let mut agents = HashMap::new();
        let mut heavy = AgentLoadModel::new();
        heavy.update_counts(8, 5);
        let idle = AgentLoadModel::new(); // 0 reservations, 0 inbox

        agents.insert("HeavyAgent".to_string(), heavy);
        agents.insert("IdleAgent".to_string(), idle);

        let target = select_route_target(&agents, "Requester", 0.1);
        assert_eq!(target, Some("IdleAgent"));
    }

    #[test]
    fn select_route_excludes_requester() {
        let mut agents = HashMap::new();
        agents.insert("OnlyAgent".to_string(), AgentLoadModel::new());

        let target = select_route_target(&agents, "OnlyAgent", 0.1);
        assert!(target.is_none(), "should not route to the requester");
    }

    #[test]
    fn select_route_excludes_atc() {
        let mut agents = HashMap::new();
        agents.insert(ATC_AGENT_NAME.to_string(), AgentLoadModel::new());
        agents.insert("RealAgent".to_string(), AgentLoadModel::new());

        let target = select_route_target(&agents, "Requester", 0.1);
        assert_eq!(target, Some("RealAgent"), "should not route to ATC");
    }

    #[test]
    fn select_route_none_when_all_saturated() {
        let mut agents = HashMap::new();
        let mut saturated = AgentLoadModel::new();
        saturated.update_counts(20, 30);
        agents.insert("SaturatedAgent".to_string(), saturated);

        let target = select_route_target(&agents, "Requester", 0.5);
        assert!(
            target.is_none(),
            "should return None when all below threshold"
        );
    }

    #[test]
    fn throughput_ewma_updates() {
        let mut model = AgentLoadModel::new();
        model.observe_activity(10, 60.0); // 10 msgs in 60s = 600/hr
        assert!(model.throughput_ewma > 0.0);
        let first = model.throughput_ewma;
        model.observe_activity(20, 60.0); // 20 msgs in 60s = 1200/hr
        assert!(model.throughput_ewma > first, "EWMA should increase");
    }
}

// ──────────────────────────────────────────────────────────────────────
// Predictive Coordination Intelligence (Track 12)
// ──────────────────────────────────────────────────────────────────────

/// Thread participation graph — tracks which agents are active in which
/// threads.  Agents sharing multiple threads are at higher conflict risk.
#[derive(Debug, Clone, Default)]
pub struct ThreadParticipationGraph {
    /// `thread_id` → set of participating agent names.
    thread_agents: HashMap<String, HashSet<String>>,
    /// `agent` → set of `thread_id` values they are active in.
    agent_threads: HashMap<String, HashSet<String>>,
}

impl ThreadParticipationGraph {
    /// Record an agent participating in a thread.
    pub fn record_participation(&mut self, agent: &str, thread_id: &str) {
        if agent == ATC_AGENT_NAME {
            return;
        }
        self.thread_agents
            .entry(thread_id.to_string())
            .or_default()
            .insert(agent.to_string());
        self.agent_threads
            .entry(agent.to_string())
            .or_default()
            .insert(thread_id.to_string());
    }

    /// Count how many threads two agents share.
    #[must_use]
    pub fn shared_thread_count(&self, agent_a: &str, agent_b: &str) -> usize {
        let Some(threads_a) = self.agent_threads.get(agent_a) else {
            return 0;
        };
        let Some(threads_b) = self.agent_threads.get(agent_b) else {
            return 0;
        };
        threads_a.intersection(threads_b).count()
    }

    /// Find all agent pairs that share >= `min_shared` threads.
    /// These pairs are at elevated conflict risk.
    #[must_use]
    pub fn high_risk_pairs(&self, min_shared: usize) -> Vec<(String, String, usize)> {
        let agents: Vec<&str> = self.agent_threads.keys().map(String::as_str).collect();
        let mut pairs = Vec::new();
        for i in 0..agents.len() {
            for j in (i + 1)..agents.len() {
                let count = self.shared_thread_count(agents[i], agents[j]);
                if count >= min_shared {
                    pairs.push((agents[i].to_string(), agents[j].to_string(), count));
                }
            }
        }
        pairs.sort_by_key(|pair| Reverse(pair.2));
        pairs
    }

    /// Number of tracked agents.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agent_threads.len()
    }

    /// Number of tracked threads.
    #[must_use]
    pub fn thread_count(&self) -> usize {
        self.thread_agents.len()
    }
}

/// Compute the expected information gain from probing an agent.
///
/// Based on the entropy of the posterior belief about their liveness.
/// High entropy = high uncertainty = high info gain from probing.
/// Near-certain agents (entropy ≈ 0) don't need probes.
#[must_use]
pub fn probe_information_gain(posterior: &[(LivenessState, f64)]) -> f64 {
    posterior
        .iter()
        .filter(|(_, p)| *p > 0.0)
        .map(|(_, p)| -p * p.ln())
        .sum()
}

/// Rank agents by probe priority (highest information gain first).
///
/// Only includes agents above the `min_gain` threshold to avoid
/// wasting probes on agents we're already confident about.
#[must_use]
pub fn rank_probe_targets<S: BuildHasher>(
    agents: &HashMap<String, AgentLivenessEntry, S>,
    min_gain: f64,
) -> Vec<(String, f64)> {
    let mut targets: Vec<(String, f64)> = agents
        .iter()
        .filter(|(name, _)| name.as_str() != ATC_AGENT_NAME)
        .map(|(name, entry)| {
            let gain = probe_information_gain(entry.core.posterior());
            (name.clone(), gain)
        })
        .filter(|(_, gain)| *gain >= min_gain)
        .collect();
    targets.sort_by(|a, b| b.1.total_cmp(&a.1));
    targets
}

// ──────────────────────────────────────────────────────────────────────
// Track 12 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod predictive_tests {
    use super::*;

    #[test]
    fn empty_graph() {
        let graph = ThreadParticipationGraph::default();
        assert_eq!(graph.agent_count(), 0);
        assert_eq!(graph.thread_count(), 0);
        assert_eq!(graph.shared_thread_count("A", "B"), 0);
    }

    #[test]
    fn record_participation() {
        let mut graph = ThreadParticipationGraph::default();
        graph.record_participation("BlueFox", "thread-1");
        graph.record_participation("RedHawk", "thread-1");
        graph.record_participation("BlueFox", "thread-2");

        assert_eq!(graph.agent_count(), 2);
        assert_eq!(graph.thread_count(), 2);
        assert_eq!(graph.shared_thread_count("BlueFox", "RedHawk"), 1);
    }

    #[test]
    fn shared_threads_symmetric() {
        let mut graph = ThreadParticipationGraph::default();
        graph.record_participation("A", "t1");
        graph.record_participation("B", "t1");
        graph.record_participation("A", "t2");
        graph.record_participation("B", "t2");

        assert_eq!(graph.shared_thread_count("A", "B"), 2);
        assert_eq!(graph.shared_thread_count("B", "A"), 2);
    }

    #[test]
    fn high_risk_pairs_filters_by_threshold() {
        let mut graph = ThreadParticipationGraph::default();
        // A and B share 3 threads
        for t in ["t1", "t2", "t3"] {
            graph.record_participation("A", t);
            graph.record_participation("B", t);
        }
        // A and C share 1 thread
        graph.record_participation("C", "t1");

        let pairs = graph.high_risk_pairs(2);
        assert_eq!(pairs.len(), 1, "only A-B should exceed threshold 2");
        assert_eq!(pairs[0].2, 3);

        let all_pairs = graph.high_risk_pairs(1);
        // A-B share 3, A-C share 1, B-C share 1 (both in t1) = 3 pairs
        assert_eq!(
            all_pairs.len(),
            3,
            "all three pairs should exceed threshold 1"
        );
    }

    #[test]
    fn atc_excluded_from_participation() {
        let mut graph = ThreadParticipationGraph::default();
        graph.record_participation(ATC_AGENT_NAME, "t1");
        graph.record_participation("BlueFox", "t1");
        assert_eq!(graph.agent_count(), 1, "ATC should be excluded");
        assert_eq!(graph.shared_thread_count(ATC_AGENT_NAME, "BlueFox"), 0);
    }

    #[test]
    fn probe_information_gain_high_for_uncertain() {
        // Uniform posterior = max entropy
        let uniform = vec![
            (LivenessState::Alive, 1.0 / 3.0),
            (LivenessState::Flaky, 1.0 / 3.0),
            (LivenessState::Dead, 1.0 / 3.0),
        ];
        let gain_uniform = probe_information_gain(&uniform);

        // Certain posterior = zero entropy
        let certain = vec![
            (LivenessState::Alive, 1.0),
            (LivenessState::Flaky, 0.0),
            (LivenessState::Dead, 0.0),
        ];
        let gain_certain = probe_information_gain(&certain);

        assert!(gain_uniform > gain_certain);
        assert!(
            gain_certain < 0.01,
            "certain posterior should have ~0 entropy"
        );
        assert!(gain_uniform > 1.0, "uniform should have high entropy");
    }

    #[test]
    fn rank_probe_targets_orders_by_gain() {
        let mut agents = HashMap::new();

        // Uncertain agent (uniform posterior)
        let mut uncertain = AgentLivenessEntry {
            name: "Uncertain".to_string(),
            project_key: None,
            program: "claude-code".to_string(),
            state: LivenessState::Alive,
            rhythm: AgentRhythm::new(60.0),
            suspect_since: 0,
            probe_sent_at: 0,
            sprt_log_lr: 0.0,
            core: default_liveness_core(),
            schedule_version: 0,
            next_review_micros: i64::MAX,
        };
        // Push posterior toward uncertainty by boosting Flaky/Dead
        // relative to the strong Alive prior (0.95)
        for _ in 0..20 {
            uncertain.core.update_posterior(&[
                (LivenessState::Alive, 0.3),
                (LivenessState::Flaky, 0.9),
                (LivenessState::Dead, 0.9),
            ]);
        }
        agents.insert("Uncertain".to_string(), uncertain);

        // Confident agent (strong alive posterior — default)
        let confident = AgentLivenessEntry {
            name: "Confident".to_string(),
            project_key: None,
            program: "claude-code".to_string(),
            state: LivenessState::Alive,
            rhythm: AgentRhythm::new(60.0),
            suspect_since: 0,
            probe_sent_at: 0,
            sprt_log_lr: 0.0,
            core: default_liveness_core(),
            schedule_version: 0,
            next_review_micros: i64::MAX,
        };
        agents.insert("Confident".to_string(), confident);

        let ranked = rank_probe_targets(&agents, 0.0);
        assert!(!ranked.is_empty());
        assert_eq!(
            ranked[0].0, "Uncertain",
            "most uncertain should be probed first"
        );
    }

    #[test]
    fn rank_probe_targets_excludes_atc() {
        let mut agents = HashMap::new();
        agents.insert(
            ATC_AGENT_NAME.to_string(),
            AgentLivenessEntry {
                name: ATC_AGENT_NAME.to_string(),
                project_key: None,
                program: "mcp-agent-mail".to_string(),
                state: LivenessState::Alive,
                rhythm: AgentRhythm::new(60.0),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: default_liveness_core(),
                schedule_version: 0,
                next_review_micros: i64::MAX,
            },
        );

        let ranked = rank_probe_targets(&agents, 0.0);
        assert!(
            ranked.is_empty(),
            "ATC should be excluded from probe targets"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Mechanism Design — VCG Priority (Track 13)
// ──────────────────────────────────────────────────────────────────────

/// Participant in a conflict resolution auction.
#[derive(Debug, Clone)]
pub struct ConflictParticipant {
    pub agent: String,
    pub remaining_tasks: u64,
    pub estimated_completion_mins: u64,
    pub reservation_age_micros: i64,
}

/// Compute VCG-inspired priority for conflict resolution.
///
/// Each agent's priority = the externality they impose on others by
/// holding the resource.  The agent with the HIGHEST externality
/// (most blocking impact) should yield first.
///
/// Under VCG, truthful reporting of `remaining_tasks` is the dominant
/// strategy: lying increases your expected cost.
#[must_use]
pub fn vcg_priority(participants: &[ConflictParticipant]) -> Vec<(String, f64)> {
    let mut priorities: Vec<(String, f64)> = participants
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let externality: f64 = participants
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, other)| {
                    u64_to_f64(other.remaining_tasks)
                        * u64_to_f64(other.estimated_completion_mins.max(1))
                        / 60.0
                })
                .sum();
            (agent.agent.clone(), externality)
        })
        .collect();
    // Highest externality first (should yield first)
    priorities.sort_by(|a, b| b.1.total_cmp(&a.1));
    priorities
}

// ──────────────────────────────────────────────────────────────────────
// Queueing-Theoretic Load Model (Track 14)
// ──────────────────────────────────────────────────────────────────────

/// Pollaczek-Khinchine mean wait time for M/G/1 queue.
///
/// `rho` = utilization (lambda/mu), `cv_service` = coefficient of variation
/// of service time.  Returns `f64::INFINITY` if the queue is unstable.
#[must_use]
pub fn pk_wait_time(rho: f64, mu: f64, cv_service: f64) -> f64 {
    if rho >= 1.0 || mu <= 0.0 {
        return f64::INFINITY;
    }
    let cs_sq = cv_service * cv_service;
    rho * (1.0 + cs_sq) / (2.0 * mu * (1.0 - rho))
}

/// Kingman's approximation for G/G/1 wait time.
///
/// More accurate than P-K when arrival process also has variance.
#[must_use]
pub fn kingman_wait_time(rho: f64, mu: f64, cv_arrival: f64, cv_service: f64) -> f64 {
    if rho >= 1.0 || mu <= 0.0 {
        return f64::INFINITY;
    }
    let arrival_cv_sq = cv_arrival * cv_arrival;
    let service_cv_sq = cv_service * cv_service;
    arrival_cv_sq.midpoint(service_cv_sq) * rho / (mu * (1.0 - rho))
}

/// Little's Law consistency check: |L - λW| / L < tolerance.
///
/// Returns `true` if the measured metrics are consistent.
#[must_use]
pub fn littles_law_consistent(
    avg_queue_depth: f64,
    arrival_rate: f64,
    avg_sojourn_time: f64,
    tolerance: f64,
) -> bool {
    if avg_queue_depth <= 0.0 {
        return true; // empty queue is trivially consistent
    }
    let predicted = arrival_rate * avg_sojourn_time;
    ((avg_queue_depth - predicted) / avg_queue_depth).abs() < tolerance
}

// ──────────────────────────────────────────────────────────────────────
// PID Regret Controller (Track 15)
// ──────────────────────────────────────────────────────────────────────

/// PID controller state for one loss matrix entry.
#[derive(Debug, Clone)]
pub struct PidState {
    /// Current adjusted loss value.
    pub current_value: f64,
    /// Original configured value (for reset).
    original_value: f64,
    /// PID gains.
    kp: f64,
    ki: f64,
    kd: f64,
    /// Integral accumulator.
    integral: f64,
    /// Previous error (for derivative term).
    prev_error: f64,
    /// Anti-windup clamp for integral.
    integral_max: f64,
    /// Safety bounds.
    min_value: f64,
    max_value: f64,
}

impl PidState {
    /// Create a new PID state for a loss matrix entry.
    #[must_use]
    pub fn new(original_value: f64) -> Self {
        Self {
            current_value: original_value,
            original_value,
            kp: 0.1,
            ki: 0.01,
            kd: 0.02,
            integral: 0.0,
            prev_error: 0.0,
            integral_max: original_value * 2.0,
            min_value: original_value * 0.1,
            max_value: original_value * 10.0,
        }
    }

    /// Update the loss value based on regret error signal.
    ///
    /// `error` = recent average regret for this (action, state) pair.
    /// Positive error → loss too low (action chosen too often).
    /// Negative error → loss too high (action avoided too much).
    pub fn update(&mut self, error: f64, dt: f64) -> f64 {
        let p = self.kp * error;
        self.integral = error
            .mul_add(dt, self.integral)
            .clamp(-self.integral_max, self.integral_max);
        let i = self.ki * self.integral;
        let d = if dt > 0.0 {
            self.kd * (error - self.prev_error) / dt
        } else {
            0.0
        };
        self.prev_error = error;

        let delta = p + i + d;
        self.current_value = (self.current_value + delta).clamp(self.min_value, self.max_value);
        self.current_value
    }

    /// Reset to original value (operator override).
    pub const fn reset(&mut self) {
        self.current_value = self.original_value;
        self.integral = 0.0;
        self.prev_error = 0.0;
    }

    /// Current deviation from original.
    #[must_use]
    pub fn deviation(&self) -> f64 {
        self.current_value - self.original_value
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tracks 13-15 Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod advanced_tests {
    use super::*;

    // ── VCG Priority (Track 13) ──────────────────────────────────────

    #[test]
    fn vcg_highest_externality_first() {
        let participants = vec![
            ConflictParticipant {
                agent: "FastAgent".to_string(),
                remaining_tasks: 1,
                estimated_completion_mins: 5,
                reservation_age_micros: 100,
            },
            ConflictParticipant {
                agent: "SlowAgent".to_string(),
                remaining_tasks: 10,
                estimated_completion_mins: 60,
                reservation_age_micros: 200,
            },
        ];
        let priorities = vcg_priority(&participants);
        // SlowAgent has more remaining work → holding the resource blocks
        // FastAgent more (FastAgent would finish quickly).
        // But externality = sum of OTHER agents' cost.
        // FastAgent's externality = SlowAgent's cost = 10*60/60 = 10 agent-hours
        // SlowAgent's externality = FastAgent's cost = 1*5/60 ≈ 0.08 agent-hours
        // FastAgent has HIGHER externality → should yield first
        assert_eq!(priorities[0].0, "FastAgent");
    }

    #[test]
    fn vcg_single_participant() {
        let participants = vec![ConflictParticipant {
            agent: "Only".to_string(),
            remaining_tasks: 5,
            estimated_completion_mins: 30,
            reservation_age_micros: 0,
        }];
        let priorities = vcg_priority(&participants);
        assert_eq!(priorities.len(), 1);
        assert!(
            (priorities[0].1 - 0.0).abs() < f64::EPSILON,
            "single agent has 0 externality"
        );
    }

    // ── Queueing Theory (Track 14) ───────────────────────────────────

    #[test]
    fn pk_returns_infinity_for_saturated() {
        assert!(pk_wait_time(1.0, 1.0, 1.0).is_infinite());
        assert!(pk_wait_time(1.5, 1.0, 1.0).is_infinite());
    }

    #[test]
    fn pk_wait_increases_with_utilization() {
        let w_low = pk_wait_time(0.3, 1.0, 1.0);
        let w_high = pk_wait_time(0.8, 1.0, 1.0);
        assert!(w_high > w_low, "higher utilization should mean longer wait");
    }

    #[test]
    fn kingman_matches_pk_for_poisson() {
        // When cv_arrival = 1 (Poisson), Kingman ≈ PK for M/M/1 (cv_service = 1)
        let pk = pk_wait_time(0.5, 1.0, 1.0);
        let kingman = kingman_wait_time(0.5, 1.0, 1.0, 1.0);
        assert!(
            (pk - kingman).abs() < 0.01,
            "should match for Poisson/Exponential case"
        );
    }

    #[test]
    fn littles_law_consistent_check() {
        assert!(littles_law_consistent(10.0, 2.0, 5.0, 0.01)); // L=10, λ=2, W=5 → λW=10 ✓
        assert!(!littles_law_consistent(10.0, 2.0, 3.0, 0.1)); // L=10, λ=2, W=3 → λW=6 ✗
    }

    // ── PID Controller (Track 15) ────────────────────────────────────

    #[test]
    fn pid_converges_under_constant_error() {
        let mut pid = PidState::new(10.0);
        // Constant positive error → value should increase
        for _ in 0..100 {
            pid.update(1.0, 1.0);
        }
        assert!(
            pid.current_value > 10.0,
            "positive error should increase value"
        );
    }

    #[test]
    fn pid_stays_within_bounds() {
        let mut pid = PidState::new(10.0);
        // Extreme positive error
        for _ in 0..1000 {
            pid.update(100.0, 1.0);
        }
        assert!(pid.current_value <= 100.0, "should not exceed 10× original");
        assert!(
            pid.current_value >= 1.0,
            "should not go below 0.1× original"
        );
    }

    #[test]
    fn pid_reset_restores_original() {
        let mut pid = PidState::new(10.0);
        pid.update(5.0, 1.0);
        assert!((pid.current_value - 10.0).abs() > f64::EPSILON);
        pid.reset();
        assert!((pid.current_value - 10.0).abs() < f64::EPSILON);
        assert!((pid.integral - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pid_anti_windup_clamps_integral() {
        let mut pid = PidState::new(10.0);
        // Drive integral to max
        for _ in 0..10000 {
            pid.update(100.0, 1.0);
        }
        assert!(
            pid.integral.abs() <= pid.integral_max,
            "integral should be clamped by anti-windup"
        );
    }

    #[test]
    fn pid_deviation_tracks_distance() {
        let mut pid = PidState::new(10.0);
        assert!((pid.deviation() - 0.0).abs() < f64::EPSILON);
        pid.update(1.0, 1.0);
        assert!(
            pid.deviation() > 0.0,
            "positive error should increase deviation"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// Edge Case & Boundary Condition Tests (Test Coverage Beads)
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod edge_case_tests {
    use super::*;

    // ── DecisionCore edge cases (br-1u8gy) ───────────────────────────

    #[test]
    fn partial_likelihoods_only_update_specified_states() {
        let mut core = default_liveness_core();
        let before = core.posterior().to_vec();
        // Only update Alive likelihood, leave Flaky and Dead at default 1.0
        core.update_posterior(&[(LivenessState::Alive, 0.01)]);
        let after = core.posterior().to_vec();
        // Alive probability should decrease (low likelihood)
        let alive_before = before
            .iter()
            .find(|(s, _)| *s == LivenessState::Alive)
            .unwrap()
            .1;
        let alive_after = after
            .iter()
            .find(|(s, _)| *s == LivenessState::Alive)
            .unwrap()
            .1;
        assert!(
            alive_after < alive_before,
            "alive should decrease: before={alive_before:.4}, after={alive_after:.4}"
        );
        // Posterior should still be normalized
        let total: f64 = after.iter().map(|(_, p)| *p).sum();
        assert!(
            (total - 1.0).abs() < 1e-10,
            "should stay normalized: {total}"
        );
    }

    #[test]
    fn two_action_core_runner_up_is_other_action() {
        // Create a core with only 2 actions
        let core = DecisionCore::new(
            &[(LivenessState::Alive, 0.9), (LivenessState::Dead, 0.1)],
            &[
                (LivenessAction::DeclareAlive, LivenessState::Alive, 0.0),
                (LivenessAction::DeclareAlive, LivenessState::Dead, 50.0),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Alive,
                    100.0,
                ),
                (
                    LivenessAction::ReleaseReservations,
                    LivenessState::Dead,
                    1.0,
                ),
            ],
            0.3,
        );
        let (_best, best_loss, runner_up) = core.choose_action();
        assert!(
            runner_up.is_finite(),
            "runner_up should not be INFINITY with 2 actions"
        );
        assert!(runner_up > best_loss);
    }

    #[test]
    fn uniform_posterior_picks_globally_cheapest() {
        let core = DecisionCore::new(
            &[
                (ConflictState::NoConflict, 1.0 / 3.0),
                (ConflictState::MildOverlap, 1.0 / 3.0),
                (ConflictState::SevereCollision, 1.0 / 3.0),
            ],
            &[
                (ConflictAction::Ignore, ConflictState::NoConflict, 0.0),
                (ConflictAction::Ignore, ConflictState::MildOverlap, 15.0),
                (
                    ConflictAction::Ignore,
                    ConflictState::SevereCollision,
                    100.0,
                ),
                (
                    ConflictAction::AdvisoryMessage,
                    ConflictState::NoConflict,
                    3.0,
                ),
                (
                    ConflictAction::AdvisoryMessage,
                    ConflictState::MildOverlap,
                    1.0,
                ),
                (
                    ConflictAction::AdvisoryMessage,
                    ConflictState::SevereCollision,
                    8.0,
                ),
                (
                    ConflictAction::ForceReservation,
                    ConflictState::NoConflict,
                    12.0,
                ),
                (
                    ConflictAction::ForceReservation,
                    ConflictState::MildOverlap,
                    4.0,
                ),
                (
                    ConflictAction::ForceReservation,
                    ConflictState::SevereCollision,
                    2.0,
                ),
            ],
            0.3,
        );
        let (action, _, _) = core.choose_action();
        // Under uniform: Advisory=(3+1+8)/3=4, Ignore=(0+15+100)/3=38.3, Force=(12+4+2)/3=6
        assert_eq!(
            action,
            ConflictAction::AdvisoryMessage,
            "advisory has lowest expected loss under uniform"
        );
    }

    #[test]
    fn alpha_clamped_to_minimum() {
        let core = DecisionCore::new(
            &[(LivenessState::Alive, 1.0)],
            &[(LivenessAction::DeclareAlive, LivenessState::Alive, 0.0)],
            -5.0, // negative alpha
        );
        assert!(
            (core.alpha - 0.01).abs() < f64::EPSILON,
            "alpha should be clamped to 0.01"
        );
    }

    #[test]
    fn empty_likelihoods_no_change() {
        let mut core = default_liveness_core();
        let before = core.posterior().to_vec();
        core.update_posterior(&[]);
        let after = core.posterior().to_vec();
        // With empty likelihoods, all states get likelihood 1.0.
        // After raising to alpha and normalizing, proportions are unchanged.
        for i in 0..before.len() {
            assert!(
                (before[i].1 - after[i].1).abs() < 1e-10,
                "state {:?} should be unchanged: {:.6} vs {:.6}",
                before[i].0,
                before[i].1,
                after[i].1,
            );
        }
    }

    // ── EvidenceLedger edge cases (br-w5phh) ─────────────────────────

    #[test]
    fn ledger_all_returns_oldest_first() {
        let mut ledger = EvidenceLedger::new(100);
        let core = default_liveness_core();
        for i in 0_i64..5 {
            let subject = format!("Agent{i}");
            ledger.record(&DecisionBuilder {
                subsystem: AtcSubsystem::Liveness,
                decision_class: "test",
                subject: &subject,
                core: &core,
                action: LivenessAction::DeclareAlive,
                expected_loss: 1.0,
                runner_up_loss: 2.0,
                evidence_summary: "test",
                calibration_healthy: true,
                safe_mode_active: false,
                policy_id: None,
                fallback_reason: None,
                timestamp_micros: i,
            });
        }
        let ids: Vec<u64> = ledger.all().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5], "all() should return oldest first");
    }

    #[test]
    fn ledger_latest_id_when_empty() {
        let ledger = EvidenceLedger::new(100);
        assert_eq!(ledger.latest_id(), 0, "empty ledger should return 0");
    }

    #[test]
    fn format_message_with_empty_posterior() {
        let record = AtcDecisionRecord {
            id: 1,
            claim_id: "atc-claim-1".to_string(),
            evidence_id: "atc-evidence-1".to_string(),
            trace_id: "atc-trace-1".to_string(),
            timestamp_micros: 0,
            subsystem: AtcSubsystem::Liveness,
            decision_class: "test".to_string(),
            subject: "Test".to_string(),
            policy_id: None,
            posterior: vec![],
            action: "Test".to_string(),
            expected_loss: 0.0,
            runner_up_loss: 0.0,
            loss_table: Vec::new(),
            evidence_summary: "test".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            fallback_reason: None,
        };
        let msg = record.format_message();
        assert!(
            msg.contains("Decision #1"),
            "should not panic with empty posterior"
        );
    }

    #[test]
    fn ledger_capacity_one() {
        let mut ledger = EvidenceLedger::new(1);
        let core = default_liveness_core();
        for i in 0_i64..3 {
            let subject = format!("Agent{i}");
            ledger.record(&DecisionBuilder {
                subsystem: AtcSubsystem::Liveness,
                decision_class: "test",
                subject: &subject,
                core: &core,
                action: LivenessAction::DeclareAlive,
                expected_loss: 1.0,
                runner_up_loss: 2.0,
                evidence_summary: "test",
                calibration_healthy: true,
                safe_mode_active: false,
                policy_id: None,
                fallback_reason: None,
                timestamp_micros: i,
            });
        }
        assert_eq!(
            ledger.len(),
            1,
            "capacity-1 ledger should hold only 1 record"
        );
        assert!(ledger.get(3).is_some(), "should hold the last record");
        assert!(ledger.get(1).is_none(), "first record should be evicted");
    }

    // ── AgentRhythm edge cases (br-oco5x) ────────────────────────────

    #[test]
    fn observe_out_of_order_timestamps() {
        let mut rhythm = AgentRhythm::new(60.0);
        rhythm.observe(100_000_000); // 100s
        rhythm.observe(50_000_000); // 50s (before previous!)
        // Delta should be clamped to 0 via .max(0)
        assert!(rhythm.avg_interval >= 0.0, "avg should not go negative");
        assert!(
            rhythm.var_interval >= 0.0,
            "variance should not go negative"
        );
    }

    #[test]
    fn observe_same_timestamp_twice() {
        let mut rhythm = AgentRhythm::new(60.0);
        rhythm.observe(100_000_000);
        rhythm.observe(100_000_000); // delta = 0
        assert!(rhythm.avg_interval.is_finite(), "should not produce NaN");
        assert!(rhythm.var_interval.is_finite(), "variance should be finite");
    }

    #[test]
    fn evaluate_liveness_skips_dead_agent() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("DeadAgent", "claude-code", None);
        // Force to Dead
        engine.agents.get_mut("DeadAgent").unwrap().state = LivenessState::Dead;
        for i in 0..10 {
            engine.observe_activity("DeadAgent", None, i * 60_000_000);
        }
        // Reset to Dead again (observe_activity resurrects)
        engine.agents.get_mut("DeadAgent").unwrap().state = LivenessState::Dead;

        let now = 9 * 60_000_000 + 600_000_000;
        let evaluation = engine.evaluate_liveness(now);
        assert!(
            !evaluation
                .actions
                .iter()
                .any(|(name, _)| name == "DeadAgent"),
            "should skip Dead agents"
        );
    }

    #[test]
    fn observe_activity_unregistered_agent_ignored() {
        let mut engine = AtcEngine::new_for_testing();
        // Don't register "Ghost" — just observe activity
        engine.observe_activity("Ghost", None, 1_000_000);
        assert!(
            engine.agent_liveness("Ghost").is_none(),
            "unregistered agent should not be auto-created"
        );
    }

    // ── Martingale edge cases (br-ems9z) ─────────────────────────────

    #[test]
    fn drift_sources_empty_when_well_calibrated() {
        let monitor = EProcessMonitor::new(0.85, 20.0);
        let sources = monitor.drift_sources();
        assert!(sources.is_empty(), "no observations → no drift sources");
    }

    #[test]
    fn cusum_recent_changes_ordering() {
        let mut cusum = CusumDetector::new(0.15, 3.0, 0.1);
        // Trigger 3 degradation detections by feeding errors in bursts
        for burst in 0..3 {
            for i in 0..50 {
                let ts = (burst * 100 + i) * 1_000_000;
                cusum.update(true, ts);
            }
        }
        let recent: Vec<_> = cusum.recent_changes(2).collect();
        assert!(recent.len() <= 2, "should return at most 2");
        if recent.len() == 2 {
            assert!(
                recent[0].timestamp >= recent[1].timestamp,
                "recent_changes should return newest first"
            );
        }
    }

    #[test]
    fn calibration_guard_dual_alert() {
        let mut guard = CalibrationGuard::new(20);
        // Both eprocess AND cusum in alert state
        let mut eprocess = EProcessMonitor::new(0.85, 20.0);
        for _ in 0..200 {
            eprocess.update(false, AtcSubsystem::Liveness, None);
        }
        let mut cusum = CusumDetector::new(0.15, 5.0, 0.1);
        for i in 0..100 {
            cusum.update(true, i * 1_000_000);
        }
        assert!(eprocess.miscalibrated());
        assert!(cusum.degradation_detected());

        guard.update(&eprocess, &cusum, false, 1_000_000);
        assert!(
            guard.is_safe_mode(),
            "both signals should trigger safe mode"
        );

        // Recovery requires BOTH to clear
        let healthy_ep = EProcessMonitor::new(0.85, 20.0);
        // cusum still degraded → should NOT exit
        for i in 0..25 {
            guard.update(&healthy_ep, &cusum, true, (i + 2) * 1_000_000);
        }
        assert!(
            guard.is_safe_mode(),
            "should stay in safe mode while cusum degraded"
        );
    }

    #[test]
    fn eprocess_target_coverage_one() {
        // alpha = 0.0 edge case
        let mut monitor = EProcessMonitor::new(1.0, 20.0);
        monitor.update(true, AtcSubsystem::Liveness, None);
        monitor.update(false, AtcSubsystem::Liveness, None);
        assert!(
            monitor.e_value().is_finite(),
            "should handle alpha=0 without panic"
        );
    }

    #[test]
    fn eprocess_target_coverage_zero() {
        // alpha = 1.0 edge case
        let mut monitor = EProcessMonitor::new(0.0, 20.0);
        monitor.update(true, AtcSubsystem::Liveness, None);
        monitor.update(false, AtcSubsystem::Liveness, None);
        assert!(
            monitor.e_value().is_finite(),
            "should handle alpha=1 without panic"
        );
    }

    #[test]
    fn regret_tracker_zero_capacity() {
        let mut tracker = RegretTracker::new(0);
        // Should not panic
        tracker.record_outcome(1, "A", 5.0, "B", 0.0, 1);
        assert_eq!(tracker.outcome_count(), 1);
        assert!((tracker.average_regret() - 5.0).abs() < f64::EPSILON);
    }

    // ── SessionSummary + LoadRouter edge cases (br-kgew5) ────────────

    #[test]
    fn absorb_message_received() {
        let mut summary = SessionSummary::default();
        summary.absorb(&SynthesisEvent::MessageReceived {
            agent: "BlueFox".to_string(),
            timestamp_micros: 1_000_000,
        });
        assert_eq!(summary.messages_received, 1);
        assert!(summary.active_agents.contains("BlueFox"));
    }

    #[test]
    fn absorb_with_timestamp_zero() {
        let mut summary = SessionSummary::default();
        summary.absorb(&SynthesisEvent::MessageSent {
            from: "A".to_string(),
            to: vec!["B".to_string()],
            thread_id: None,
            timestamp_micros: 0,
        });
        assert_eq!(summary.first_event_ts, 0, "timestamp 0 should be accepted");
        assert_eq!(summary.last_event_ts, 0);
    }

    #[test]
    fn select_route_empty_agents() {
        let agents: HashMap<String, AgentLoadModel> = HashMap::new();
        let target = select_route_target(&agents, "Requester", 0.1);
        assert!(target.is_none(), "empty map should return None");
    }

    #[test]
    fn load_model_zero_interval() {
        let mut model = AgentLoadModel::new();
        model.observe_activity(10, 0.0); // zero interval
        // Should not panic or produce NaN
        assert!(model.throughput_ewma.is_finite());
    }

    #[test]
    fn pid_negative_error_decreases_value() {
        let mut pid = PidState::new(10.0);
        for _ in 0..50 {
            pid.update(-1.0, 1.0);
        }
        assert!(
            pid.current_value < 10.0,
            "negative error should decrease value"
        );
    }

    #[test]
    fn pid_zero_dt_no_panic() {
        let mut pid = PidState::new(10.0);
        let val = pid.update(1.0, 0.0);
        assert!(val.is_finite(), "dt=0 should not cause panic or NaN");
    }

    #[test]
    fn vcg_three_participants() {
        let participants = vec![
            ConflictParticipant {
                agent: "A".to_string(),
                remaining_tasks: 1,
                estimated_completion_mins: 10,
                reservation_age_micros: 0,
            },
            ConflictParticipant {
                agent: "B".to_string(),
                remaining_tasks: 5,
                estimated_completion_mins: 30,
                reservation_age_micros: 0,
            },
            ConflictParticipant {
                agent: "C".to_string(),
                remaining_tasks: 10,
                estimated_completion_mins: 60,
                reservation_age_micros: 0,
            },
        ];
        let priorities = vcg_priority(&participants);
        assert_eq!(priorities.len(), 3);
        // A's externality = B_cost + C_cost = (5*30/60) + (10*60/60) = 2.5 + 10 = 12.5
        // B's externality = A_cost + C_cost = (1*10/60) + (10*60/60) = 0.167 + 10 = 10.167
        // C's externality = A_cost + B_cost = (1*10/60) + (5*30/60) = 0.167 + 2.5 = 2.667
        // A has highest externality → should yield first
        assert_eq!(
            priorities[0].0, "A",
            "A should yield first (highest externality)"
        );
    }

    #[test]
    fn probe_gain_binary_posterior() {
        let binary = vec![(LivenessState::Alive, 0.5), (LivenessState::Dead, 0.5)];
        let gain = probe_information_gain(&binary);
        // Binary entropy at p=0.5 = ln(2) ≈ 0.693
        assert!(
            (gain - 2.0_f64.ln()).abs() < 0.01,
            "should be ln(2) for binary uniform"
        );
    }

    #[test]
    fn littles_law_zero_arrival_nonzero_queue() {
        assert!(
            !littles_law_consistent(10.0, 0.0, 5.0, 0.2),
            "nonzero queue with zero arrival should be inconsistent"
        );
    }

    #[test]
    fn kingman_lower_cv_means_shorter_wait() {
        let det = kingman_wait_time(0.5, 1.0, 0.0, 0.0); // deterministic
        let var = kingman_wait_time(0.5, 1.0, 1.0, 1.0); // variable
        assert!(
            det < var,
            "deterministic service should have shorter wait: {det} vs {var}"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════
// ALIEN-ARTIFACT ENHANCEMENTS — Tracks 16–21
// ══════════════════════════════════════════════════════════════════════

// ── Track 16: Regret → PID → Loss Matrix Feedback Loop ──────────────

/// Maintains a PID controller per (action, state) pair in a loss matrix.
#[derive(Debug)]
pub struct LossMatrixTuner<A: AtcAction, S: AtcState> {
    pids: HashMap<(A, S), PidState>,
    regret_accum: HashMap<(A, S), (f64, u64)>,
    update_interval: u64,
    decisions_since_update: u64,
}

impl<A: AtcAction, S: AtcState> LossMatrixTuner<A, S> {
    #[must_use]
    pub fn from_core(core: &DecisionCore<S, A>, update_interval: u64) -> Self {
        let mut pids = HashMap::new();
        for &action in &core.actions {
            for &(state, _) in core.posterior() {
                let loss = core.loss_entry(action, state);
                pids.insert((action, state), PidState::new(loss));
            }
        }
        Self {
            pids,
            regret_accum: HashMap::new(),
            update_interval: update_interval.max(1),
            decisions_since_update: 0,
        }
    }

    pub fn record_outcome(&mut self, action: A, true_state: S, regret: f64) {
        let entry = self
            .regret_accum
            .entry((action, true_state))
            .or_insert((0.0, 0));
        entry.0 += regret;
        entry.1 += 1;
        self.decisions_since_update += 1;
    }

    pub fn maybe_update(&mut self, core: &mut DecisionCore<S, A>) -> bool {
        if self.decisions_since_update < self.update_interval {
            return false;
        }
        self.decisions_since_update = 0;
        let dt = 1.0;
        let mut any_changed = false;
        for ((action, state), pid) in &mut self.pids {
            let (total_regret, count) = self
                .regret_accum
                .get(&(*action, *state))
                .copied()
                .unwrap_or((0.0, 0));
            if count == 0 {
                continue;
            }
            let avg_regret = total_regret / u64_to_f64(count);
            let new_loss = pid.update(avg_regret, dt);
            core.set_loss_entry(*action, *state, new_loss);
            any_changed = true;
        }
        self.regret_accum.clear();
        any_changed
    }
}

// ── Track 17: Conformal Prediction Sets for ATC Decisions ───────────

#[derive(Debug, Clone)]
pub struct AtcConformalSet {
    pub sets: HashMap<AtcSubsystem, SubsystemConformal>,
}

#[derive(Debug, Clone)]
pub struct SubsystemConformal {
    scores: VecDeque<f64>,
    capacity: usize,
    coverage: f64,
}

impl SubsystemConformal {
    fn new(capacity: usize, coverage: f64) -> Self {
        Self {
            scores: VecDeque::with_capacity(capacity),
            capacity,
            coverage,
        }
    }

    pub fn observe(&mut self, predicted_loss: f64, actual_loss: f64) {
        let score = (predicted_loss - actual_loss).abs();
        if self.scores.len() >= self.capacity {
            self.scores.pop_front();
        }
        self.scores.push_back(score);
    }

    #[must_use]
    pub fn interval_width(&self) -> Option<f64> {
        if self.scores.len() < 5 {
            return None;
        }
        let mut sorted: Vec<f64> = self.scores.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        // Quantile index at configured coverage level
        let raw_idx = (self.coverage * usize_to_f64(sorted.len())).ceil();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (raw_idx as usize).min(sorted.len().saturating_sub(1));
        Some(sorted[idx])
    }

    #[must_use]
    pub fn is_uncertain(&self, max_possible_loss: f64) -> bool {
        // Uncertainty threshold is 20% of the maximum possible loss
        self.interval_width()
            .is_some_and(|w| w > max_possible_loss * 0.20)
    }
}

impl AtcConformalSet {
    #[must_use]
    pub fn new(capacity: usize, coverage: f64) -> Self {
        let mut sets = HashMap::new();
        for sub in [
            AtcSubsystem::Liveness,
            AtcSubsystem::Conflict,
            AtcSubsystem::LoadRouting,
            AtcSubsystem::Synthesis,
            AtcSubsystem::Calibration,
        ] {
            sets.insert(sub, SubsystemConformal::new(capacity, coverage));
        }
        Self { sets }
    }

    pub fn observe(&mut self, subsystem: AtcSubsystem, predicted_loss: f64, actual_loss: f64) {
        if let Some(sc) = self.sets.get_mut(&subsystem) {
            sc.observe(predicted_loss, actual_loss);
        }
    }

    #[must_use]
    pub fn is_uncertain(&self, subsystem: AtcSubsystem, max_possible_loss: f64) -> bool {
        self.sets
            .get(&subsystem)
            .is_some_and(|sc| sc.is_uncertain(max_possible_loss))
    }
}

// ── Track 18: Hierarchical Bayesian Agent Population Model ──────────

#[derive(Debug, Clone)]
pub struct HierarchicalAgentModel {
    populations: HashMap<String, PopulationStats>,
}

#[derive(Debug, Clone)]
pub struct PopulationStats {
    pub mean: f64,
    pub variance: f64,
    pub n: f64,
    pub agent_count: u64,
}

impl PopulationStats {
    fn new(prior_mean_secs: f64) -> Self {
        let prior_mean = prior_mean_secs * 1_000_000.0;
        Self {
            mean: prior_mean,
            variance: (prior_mean * 0.5).powi(2),
            n: 3.0,
            agent_count: 0,
        }
    }

    fn absorb_agent(&mut self, agent_avg: f64, agent_var: f64, agent_n: u64) {
        if agent_n == 0 {
            return;
        }
        let n_agent = u64_to_f64(agent_n);
        let total_n = self.n + n_agent;
        let delta = agent_avg - self.mean;
        self.mean += delta * n_agent / total_n;
        self.variance = (self.variance.mul_add(self.n, agent_var * n_agent)
            + (delta * delta * self.n * n_agent / total_n))
            / total_n;
        self.n = total_n;
        self.agent_count += 1;
    }

    #[must_use]
    const fn derive_prior(&self) -> (f64, f64) {
        // Use manual max instead of f64::max() for const context.
        let v = if self.variance > 1.0 {
            self.variance
        } else {
            1.0
        };
        (self.mean, v)
    }
}

impl HierarchicalAgentModel {
    #[must_use]
    pub fn new() -> Self {
        let mut populations = HashMap::new();
        for (program, secs) in [
            ("claude-code", 60.0),
            ("codex-cli", 120.0),
            ("gemini-cli", 120.0),
            ("copilot-cli", 120.0),
            ("unknown", 300.0),
        ] {
            populations.insert(program.to_string(), PopulationStats::new(secs));
        }
        Self { populations }
    }

    fn canonical_program(program: &str) -> &'static str {
        match program.to_ascii_lowercase().as_str() {
            "claude-code" | "claude_code" => "claude-code",
            "codex-cli" | "codex_cli" | "codex" => "codex-cli",
            "gemini-cli" | "gemini_cli" | "gemini" => "gemini-cli",
            "copilot-cli" | "copilot_cli" | "copilot" => "copilot-cli",
            _ => "unknown",
        }
    }

    #[must_use]
    pub fn prior_for(&self, program: &str) -> (f64, f64) {
        let key = Self::canonical_program(program);
        self.populations.get(key).map_or_else(
            || {
                let default_mean = 300.0 * 1_000_000.0;
                (default_mean, (default_mean * 0.5).powi(2))
            },
            PopulationStats::derive_prior,
        )
    }

    pub fn absorb_agent(&mut self, program: &str, rhythm: &AgentRhythm) {
        let key = Self::canonical_program(program).to_string();
        let stats = self
            .populations
            .entry(key)
            .or_insert_with(|| PopulationStats::new(300.0));
        stats.absorb_agent(
            rhythm.avg_interval,
            rhythm.var_interval,
            rhythm.observation_count,
        );
    }

    #[must_use]
    pub fn create_rhythm(&self, program: &str) -> AgentRhythm {
        let (mean, var) = self.prior_for(program);
        let mut rhythm = AgentRhythm::new(mean / 1_000_000.0);
        rhythm.var_interval = var;
        rhythm
    }
}

impl Default for HierarchicalAgentModel {
    fn default() -> Self {
        Self::new()
    }
}

// ── Track 19: Contextual Bandits for Adaptive Thresholds ────────────

/// Thompson-sampling-inspired adaptive suspicion threshold per agent.
/// Maintains a Beta(α, β) posterior for the true-positive rate.
#[derive(Debug, Clone)]
pub struct AdaptiveThreshold {
    alpha: f64,
    beta_param: f64,
    base_k: f64,
    min_k: f64,
    max_k: f64,
}

impl AdaptiveThreshold {
    #[must_use]
    pub const fn new(base_k: f64) -> Self {
        Self {
            alpha: 2.0,
            beta_param: 2.0,
            base_k,
            min_k: 1.5,
            max_k: 5.0,
        }
    }

    pub fn record_outcome(&mut self, true_positive: bool) {
        if true_positive {
            self.alpha += 1.0;
        } else {
            self.beta_param += 1.0;
        }
    }

    #[must_use]
    pub fn effective_k(&self) -> f64 {
        let precision = self.alpha / (self.alpha + self.beta_param);
        (0.5 - precision)
            .mul_add(self.max_k - self.min_k, self.base_k)
            .clamp(self.min_k, self.max_k)
    }

    #[must_use]
    pub fn precision_estimate(&self) -> f64 {
        self.alpha / (self.alpha + self.beta_param)
    }

    #[must_use]
    pub fn observation_count(&self) -> u64 {
        // Subtract the 4 pseudo-counts from the prior
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (self.alpha + self.beta_param - 4.0).max(0.0) as u64
        }
    }
}

// ── Track 20: Submodular Probe Scheduling ───────────────────────────

#[must_use]
pub fn submodular_probe_schedule<S: BuildHasher>(
    agents: &HashMap<String, AgentLivenessEntry, S>,
    max_probes: usize,
    recency_decay: f64,
    now_micros: i64,
) -> Vec<(String, f64)> {
    if agents.is_empty() || max_probes == 0 {
        return Vec::new();
    }

    let mut candidates: Vec<(String, f64)> = Vec::with_capacity(max_probes);
    let mut agent_names: Vec<&String> = agents.keys().collect();
    agent_names.sort();
    for name in agent_names {
        let Some(entry) = agents.get(name) else {
            continue;
        };
        if name.as_str() == ATC_AGENT_NAME || entry.state == LivenessState::Dead {
            continue;
        }
        let gain = {
            let base_gain = probe_information_gain(entry.core.posterior());
            let time_since_probe = if entry.probe_sent_at > 0 {
                nonnegative_i64_to_f64(now_micros.saturating_sub(entry.probe_sent_at).max(0))
            } else {
                1_000_000_000.0
            };
            let recency = 1.0 - (-time_since_probe / (recency_decay * 1_000_000.0)).exp();
            base_gain * recency.max(0.01)
        };
        if gain <= 0.001 {
            continue;
        }
        let insert_at = candidates.partition_point(|(existing_name, existing_gain)| {
            match existing_gain.total_cmp(&gain) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => existing_name.as_str() <= name.as_str(),
                std::cmp::Ordering::Less => false,
            }
        });
        if insert_at >= max_probes {
            continue;
        }
        candidates.insert(insert_at, (name.clone(), gain));
        if candidates.len() > max_probes {
            candidates.pop();
        }
    }

    candidates
}

#[derive(Debug, Clone)]
struct AtcProbeCandidate {
    pub agent: String,
    pub gain_per_micro: f64,
}

#[must_use]
fn budgeted_probe_schedule<S: BuildHasher>(
    agents: &HashMap<String, AgentLivenessEntry, S>,
    excluded_agents: &HashSet<String>,
    policy: &AtcLivenessPolicyArtifact,
    mode: AtcBudgetMode,
    hard_probe_limit: usize,
    max_budget_micros: u64,
    estimated_probe_cost_micros: u64,
    now_micros: i64,
) -> Vec<AtcProbeCandidate> {
    if agents.is_empty() || max_budget_micros == 0 {
        return Vec::new();
    }

    let max_probes = policy.max_probes(mode).min(hard_probe_limit);
    if max_probes == 0 {
        return Vec::new();
    }

    let schedule = submodular_probe_schedule(
        agents,
        max_probes,
        policy.probe_recency_decay_secs,
        now_micros,
    );
    let probe_cost = estimated_probe_cost_micros.max(1);
    let budgeted_probe_count =
        usize::try_from((max_budget_micros / probe_cost).max(1)).unwrap_or(max_probes);
    let mut candidates = Vec::new();
    let mut spent_budget = 0_u64;

    for (agent, estimated_gain) in schedule {
        if excluded_agents.contains(&agent) || estimated_gain < policy.probe_gain_floor {
            continue;
        }
        if candidates.len() >= max_probes || candidates.len() >= budgeted_probe_count {
            break;
        }
        if spent_budget.saturating_add(probe_cost) > max_budget_micros {
            break;
        }
        spent_budget = spent_budget.saturating_add(probe_cost);
        candidates.push(AtcProbeCandidate {
            agent,
            gain_per_micro: estimated_gain / u64_to_f64(probe_cost),
        });
    }
    candidates
}

// ── Track 21: Survival Analysis for Agent Liveness ──────────────────

/// Kaplan-Meier survival estimator for agent silence durations.
#[derive(Debug, Clone)]
pub struct KaplanMeierEstimator {
    observations: VecDeque<(i64, bool)>,
    capacity: usize,
}

impl KaplanMeierEstimator {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            observations: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
        }
    }

    pub fn observe(&mut self, duration_micros: i64, is_death: bool) {
        if self.observations.len() >= self.capacity {
            self.observations.pop_front();
        }
        self.observations.push_back((duration_micros, is_death));
    }

    #[must_use]
    pub fn survival_probability(&self, t_micros: i64) -> f64 {
        if self.observations.is_empty() {
            return 1.0;
        }
        let mut sorted: Vec<(i64, bool)> = self.observations.iter().copied().collect();
        sorted.sort_by_key(|(d, _)| *d);

        let mut s = 1.0;
        let mut at_risk = usize_to_f64(sorted.len());

        for &(duration, is_death) in &sorted {
            if duration > t_micros {
                break;
            }
            if is_death && at_risk > 0.0 {
                s *= 1.0 - (1.0 / at_risk);
            }
            at_risk -= 1.0;
        }
        s.max(0.0)
    }

    #[must_use]
    pub fn hazard_rate(&self, t_micros: i64, window_micros: i64) -> f64 {
        let s_t = self.survival_probability(t_micros);
        let s_t_plus = self.survival_probability(t_micros + window_micros);
        if s_t <= 0.0 {
            return 1.0;
        }
        let window_secs = nonnegative_i64_to_f64(window_micros) / 1_000_000.0;
        if window_secs <= 0.0 {
            return 0.0;
        }
        (s_t - s_t_plus) / (s_t * window_secs)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.observations.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }
}

// ── Global ATC Engine Singleton ─────────────────────────────────────

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};

static ATC_ENGINE: OnceLock<Mutex<AtcEngine>> = OnceLock::new();
static ATC_WRITE_MODE: OnceLock<mcp_agent_mail_core::AtcWriteMode> = OnceLock::new();
static ATC_KILL_SWITCH: AtomicBool = AtomicBool::new(false);
static ATC_KILL_SWITCH_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
static ATC_POPULATION: OnceLock<Mutex<HierarchicalAgentModel>> = OnceLock::new();
static ATC_CONFORMAL: OnceLock<Mutex<AtcConformalSet>> = OnceLock::new();
static ATC_THRESHOLDS: OnceLock<Mutex<HashMap<String, AdaptiveThreshold>>> = OnceLock::new();
static ATC_SURVIVAL: OnceLock<Mutex<HashMap<String, KaplanMeierEstimator>>> = OnceLock::new();
static ATC_LIVENESS_TUNER: OnceLock<Mutex<LossMatrixTuner<LivenessAction, LivenessState>>> =
    OnceLock::new();

fn current_adaptive_threshold_k(agent_name: &str) -> Option<f64> {
    ATC_THRESHOLDS
        .get()
        .and_then(|lock| lock.lock().ok())
        .and_then(|thresholds| {
            thresholds
                .get(agent_name)
                .map(AdaptiveThreshold::effective_k)
        })
}

#[derive(Debug, Clone)]
pub enum ReplayEvent {
    Decision(AtcDecisionRecord),
    Feedback {
        decision_id: u64,
        actual_loss: f64,
    },
    Activity(SynthesisEvent),
    Tick {
        total_micros: u64,
        tick_budget_micros: u64,
        utilization_ratio: f64,
        budget_exceeded: bool,
        baseline_probe_limit: usize,
    },
}

impl AtcEngine {
    /// Replay historical events to seamlessly reconstruct Bayesian posteriors, Conformal bounds,
    /// and PI Control Theory states without amnesia after a server restart.
    pub fn replay_from_ledger(&mut self, events: impl IntoIterator<Item = ReplayEvent>) {
        for event in events {
            match event {
                ReplayEvent::Decision(record) => {
                    if record.subsystem == AtcSubsystem::Liveness {
                        if let Some(entry) = self.agents.get_mut(&record.subject) {
                            let parsed_posterior: Vec<(LivenessState, f64)> = record
                                .posterior
                                .iter()
                                .filter_map(|(s, p)| {
                                    let state = match s.as_str() {
                                        "Alive" => LivenessState::Alive,
                                        "Flaky" => LivenessState::Flaky,
                                        "Dead" => LivenessState::Dead,
                                        _ => return None,
                                    };
                                    Some((state, *p))
                                })
                                .collect();
                            entry.core.posterior = parsed_posterior;
                        }
                    }
                    self.ledger.insert_raw(record);
                }
                ReplayEvent::Feedback {
                    decision_id,
                    actual_loss,
                } => {
                    if let Some(record) = self.ledger.get(decision_id).cloned() {
                        if let Some(conformal_lock) = ATC_CONFORMAL.get() {
                            if let Ok(mut conformal) = conformal_lock.lock() {
                                conformal.observe(
                                    record.subsystem,
                                    record.expected_loss,
                                    actual_loss,
                                );
                            }
                        }
                    }
                }
                ReplayEvent::Activity(synthesis_event) => {
                    self.session_summary.absorb(&synthesis_event);
                    match synthesis_event {
                        SynthesisEvent::MessageSent {
                            ref from,
                            timestamp_micros,
                            ..
                        } => {
                            self.observe_activity(from, None, timestamp_micros);
                        }
                        SynthesisEvent::MessageReceived {
                            ref agent,
                            timestamp_micros,
                        } => {
                            self.observe_activity(agent, None, timestamp_micros);
                        }
                        SynthesisEvent::ReservationGranted {
                            ref agent,
                            timestamp_micros,
                        } => {
                            self.observe_activity(agent, None, timestamp_micros);
                        }
                        SynthesisEvent::ReservationReleased {
                            ref agent,
                            timestamp_micros,
                        } => {
                            self.observe_activity(agent, None, timestamp_micros);
                        }
                        SynthesisEvent::ReservationRenewed {
                            ref agent,
                            timestamp_micros,
                        } => {
                            self.observe_activity(agent, None, timestamp_micros);
                        }
                        SynthesisEvent::ReservationForceReleased {
                            ref agent,
                            timestamp_micros,
                        } => {
                            self.observe_activity(agent, None, timestamp_micros);
                        }
                        _ => {}
                    }
                }
                ReplayEvent::Tick {
                    total_micros,
                    tick_budget_micros,
                    utilization_ratio,
                    budget_exceeded,
                    baseline_probe_limit,
                } => {
                    self.slow_controller.note_tick(
                        total_micros,
                        tick_budget_micros,
                        utilization_ratio,
                        budget_exceeded,
                        baseline_probe_limit,
                    );
                }
            }
        }
    }

    /// Build an `AtcConfig` from the core `Config` env vars.
    #[must_use]
    pub fn config_from_env(config: &mcp_agent_mail_core::Config) -> AtcConfig {
        AtcConfig {
            enabled: config.atc_enabled,
            policy_bundle_path: std::env::var("AM_ATC_POLICY_BUNDLE_PATH")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            probe_interval_micros: i64::try_from(config.atc_probe_interval_secs)
                .unwrap_or(120)
                .saturating_mul(1_000_000),
            advisory_cooldown_micros: i64::try_from(config.atc_advisory_cooldown_secs)
                .unwrap_or(300)
                .saturating_mul(1_000_000),
            summary_interval_micros: i64::try_from(config.atc_summary_interval_secs)
                .unwrap_or(300)
                .saturating_mul(1_000_000),
            safe_mode_recovery_count: config.atc_safe_mode_recovery_count,
            eprocess_alert_threshold: config.atc_eprocess_threshold,
            cusum_threshold: config.atc_cusum_threshold,
            cusum_delta: config.atc_cusum_delta,
            ledger_capacity: config.atc_ledger_capacity,
            tick_budget_micros: 5_000,
            suspicion_k: config.atc_suspicion_k,
        }
    }
}

/// Initialize the global ATC engine. Call once at server startup.
pub fn init_global_atc(config: &mcp_agent_mail_core::Config) {
    let _ = ATC_WRITE_MODE.set(config.atc_write_mode);
    let kill_switch_path = config.storage_root.join(".atc_kill_switch");
    let _ = ATC_KILL_SWITCH_PATH.set(kill_switch_path);
    refresh_kill_switch();
    let atc_config = AtcEngine::config_from_env(config);
    let engine_lock = ATC_ENGINE.get_or_init(|| Mutex::new(AtcEngine::new(atc_config.clone())));
    *engine_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = AtcEngine::new(atc_config);

    let population_lock = ATC_POPULATION.get_or_init(|| Mutex::new(HierarchicalAgentModel::new()));
    *population_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = HierarchicalAgentModel::new();

    let conformal_lock = ATC_CONFORMAL.get_or_init(|| Mutex::new(AtcConformalSet::new(200, 0.90)));
    *conformal_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = AtcConformalSet::new(200, 0.90);

    let thresholds_lock = ATC_THRESHOLDS.get_or_init(|| Mutex::new(HashMap::new()));
    thresholds_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    let survival_lock = ATC_SURVIVAL.get_or_init(|| Mutex::new(HashMap::new()));
    survival_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    let tuner_lock = ATC_LIVENESS_TUNER
        .get_or_init(|| Mutex::new(LossMatrixTuner::from_core(&default_liveness_core(), 50)));
    *tuner_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        LossMatrixTuner::from_core(&default_liveness_core(), 50);
}

/// Whether the global ATC is initialized and enabled.
#[must_use]
pub fn atc_enabled() -> bool {
    ATC_ENGINE
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|e| e.enabled())
}

/// Record an agent registration in the ATC.
pub fn atc_register_agent(name: &str, program: &str) {
    atc_register_agent_with_project(name, program, None);
}

/// Record an agent registration in the ATC with optional project routing metadata.
pub fn atc_register_agent_with_project(name: &str, program: &str, project_key: Option<&str>) {
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    let base_k = e.config.suspicion_k;
    e.register_agent(name, program, project_key);
    drop(e);

    if let Some(thresholds) = ATC_THRESHOLDS.get()
        && let Ok(mut t) = thresholds.lock()
    {
        t.entry(name.to_string())
            .or_insert_with(|| AdaptiveThreshold::new(base_k));
    }
}

/// Record agent activity (tool call, message, etc.) in the ATC.
pub fn atc_observe_activity(agent: &str, timestamp_micros: i64) {
    atc_observe_activity_with_project(agent, None, timestamp_micros);
}

/// Record agent activity (tool call, message, etc.) in the ATC with optional project metadata.
pub fn atc_observe_activity_with_project(
    agent: &str,
    project_key: Option<&str>,
    timestamp_micros: i64,
) {
    if agent.trim().is_empty() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    let base_k = e.config.suspicion_k;
    if !e.agents.contains_key(agent) {
        // Tool-call activity frequently arrives after ATC startup for agents
        // that were registered in a previous process lifetime. Auto-register
        // unknown agents so liveness tracking can start from the first signal.
        e.register_agent(agent, "unknown-tool", project_key);
    }
    drop(e);
    if let Some(thresholds) = ATC_THRESHOLDS.get()
        && let Ok(mut t) = thresholds.lock()
    {
        t.entry(agent.to_string())
            .or_insert_with(|| AdaptiveThreshold::new(base_k));
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    e.observe_activity(agent, project_key, timestamp_micros);
}

/// Merge an authoritative agent snapshot into the ATC engine.
///
/// This is intentionally monotonic for `last_activity_ts`: repeated syncs from
/// the durable DB/archive must never resurrect an agent based on stale data.
pub fn atc_sync_agent_snapshot(
    name: &str,
    program: &str,
    project_key: Option<&str>,
    last_activity_ts: i64,
) {
    if name.trim().is_empty() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    let base_k = e.config.suspicion_k;
    let program = if program.trim().is_empty() {
        "unknown-tool"
    } else {
        program
    };

    e.register_agent(name, program, project_key);
    let prior_last_activity = e
        .agents
        .get(name)
        .map_or(0, |entry| entry.rhythm.last_activity_ts);
    drop(e);
    if let Some(thresholds) = ATC_THRESHOLDS.get()
        && let Ok(mut t) = thresholds.lock()
    {
        t.entry(name.to_string())
            .or_insert_with(|| AdaptiveThreshold::new(base_k));
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if last_activity_ts > prior_last_activity {
        e.observe_activity(name, project_key, last_activity_ts);
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AtcPopulationSyncStats {
    pub projects: usize,
    pub agents: usize,
    pub active_agents: usize,
}

/// Hydrate the ATC engine from the durable DB snapshot.
///
/// This seeds pre-existing agents on cold start and refreshes their latest
/// known activity monotonically so periodic syncs cannot regress liveness.
///
/// Only agents whose `last_active_ts` falls within the recency window are
/// hydrated. Agents that have been completely silent for longer than the
/// window would immediately appear Dead to the liveness evaluator anyway,
/// so loading them generates an O(agents) burst of tick effects on every
/// cold start without any coordination value.
pub fn atc_sync_population_from_db(
    pool: &mcp_agent_mail_db::DbPool,
) -> Result<AtcPopulationSyncStats, String> {
    /// Default recency window: 7 days in microseconds.  Agents silent longer
    /// than this are already effectively Dead; loading them generates a
    /// cold-start effect burst without any coordination value.
    const DEFAULT_RECENCY_MICROS: i64 = 7 * 24 * 3600 * 1_000_000;

    let recency_micros =
        mcp_agent_mail_core::config::full_env_value("AM_ATC_POPULATION_RECENCY_SECS")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|secs| *secs >= 0)
            .map(|secs| secs.saturating_mul(1_000_000))
            .unwrap_or(DEFAULT_RECENCY_MICROS);

    let now = mcp_agent_mail_core::timestamps::now_micros();
    let recency_cutoff = now.saturating_sub(recency_micros);

    let cx = asupersync::Cx::for_request_with_budget(asupersync::Budget::INFINITE);
    let projects =
        match fastmcp_core::block_on(mcp_agent_mail_db::queries::list_projects(&cx, pool)) {
            asupersync::Outcome::Ok(rows) => rows,
            asupersync::Outcome::Err(error) => return Err(error.to_string()),
            asupersync::Outcome::Cancelled(reason) => {
                return Err(format!("cancelled: {reason:?}"));
            }
            asupersync::Outcome::Panicked(payload) => {
                return Err(format!("panicked: {}", payload.message()));
            }
        };

    let mut stats = AtcPopulationSyncStats::default();
    for project in projects {
        let Some(project_id) = project.id else {
            continue;
        };
        stats.projects = stats.projects.saturating_add(1);
        let agents = match fastmcp_core::block_on(mcp_agent_mail_db::queries::list_agents(
            &cx, pool, project_id,
        )) {
            asupersync::Outcome::Ok(rows) => rows,
            asupersync::Outcome::Err(error) => return Err(error.to_string()),
            asupersync::Outcome::Cancelled(reason) => {
                return Err(format!("cancelled: {reason:?}"));
            }
            asupersync::Outcome::Panicked(payload) => {
                return Err(format!("panicked: {}", payload.message()));
            }
        };
        let project_key = project.human_key.trim();
        let project_key = (!project_key.is_empty()).then_some(project_key);
        for agent in agents {
            stats.agents = stats.agents.saturating_add(1);
            if agent.last_active_ts > 0 {
                stats.active_agents = stats.active_agents.saturating_add(1);
            }
            // Skip agents that have been silent beyond the recency window.
            // They are already Dead from the liveness evaluator's perspective;
            // hydrating them only floods the effect queue on cold start.
            if agent.last_active_ts > 0 && agent.last_active_ts < recency_cutoff {
                continue;
            }
            atc_sync_agent_snapshot(
                &agent.name,
                &agent.program,
                project_key,
                agent.last_active_ts,
            );
        }
    }
    Ok(stats)
}

#[derive(Debug, Clone)]
pub struct AtcConflictObservation {
    pub holder: String,
    pub requested_path: String,
    pub holder_path_pattern: String,
}

fn atc_write_mode() -> mcp_agent_mail_core::AtcWriteMode {
    if ATC_KILL_SWITCH.load(AtomicOrdering::Relaxed) {
        return mcp_agent_mail_core::AtcWriteMode::Off;
    }
    ATC_WRITE_MODE
        .get()
        .copied()
        .unwrap_or(mcp_agent_mail_core::AtcWriteMode::Off)
}

pub fn refresh_kill_switch() {
    let active = ATC_KILL_SWITCH_PATH.get().is_some_and(|p| p.exists());
    let was_active = ATC_KILL_SWITCH.swap(active, AtomicOrdering::Relaxed);
    mcp_agent_mail_core::global_metrics()
        .atc
        .set_kill_switch_enabled(active);
    if active != was_active {
        let reason = if active {
            ATC_KILL_SWITCH_PATH
                .get()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default()
        } else {
            String::new()
        };
        if active {
            tracing::info!(
                event = "atc.kill_switch.change",
                from_state = "disabled",
                to_state = "enabled",
                reason,
                path = ?ATC_KILL_SWITCH_PATH.get(),
                "ATC kill switch ACTIVATED — all experience writes suppressed"
            );
        } else {
            tracing::info!(
                event = "atc.kill_switch.change",
                from_state = "enabled",
                to_state = "disabled",
                reason = "",
                path = ?ATC_KILL_SWITCH_PATH.get(),
                "ATC kill switch DEACTIVATED — experience writes resume per AM_ATC_WRITE_MODE"
            );
        }
    }
}

pub fn atc_kill_switch_active() -> bool {
    ATC_KILL_SWITCH.load(AtomicOrdering::Relaxed)
}

pub fn activate_kill_switch(reason: &str) -> std::io::Result<()> {
    let Some(path) = ATC_KILL_SWITCH_PATH.get() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "kill switch path not initialized",
        ));
    };
    std::fs::write(path, reason)?;
    tracing::info!(reason, path = ?path, "ATC kill switch file written");
    refresh_kill_switch();
    Ok(())
}

pub fn deactivate_kill_switch() -> std::io::Result<()> {
    let Some(path) = ATC_KILL_SWITCH_PATH.get() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "kill switch path not initialized",
        ));
    };
    if path.exists() {
        std::fs::remove_file(path)?;
        tracing::info!(path = ?path, "ATC kill switch file removed");
    }
    refresh_kill_switch();
    Ok(())
}

pub fn kill_switch_status() -> serde_json::Value {
    let active = atc_kill_switch_active();
    let reason = ATC_KILL_SWITCH_PATH
        .get()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    serde_json::json!({
        "active": active,
        "reason": if active { reason.as_str() } else { "" },
        "path": ATC_KILL_SWITCH_PATH.get().map(|p| p.display().to_string()),
        "write_mode": ATC_WRITE_MODE.get().copied().unwrap_or(mcp_agent_mail_core::AtcWriteMode::Off).to_string(),
        "effective_mode": atc_write_mode().to_string(),
    })
}

pub fn atc_note_message_sent(
    from: &str,
    to: &[String],
    thread_id: Option<&str>,
    timestamp_micros: i64,
) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("message_sent");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "message_sent",
            from,
            ?to,
            ?thread_id,
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_message_sent(from, to, thread_id, timestamp_micros);
}

pub fn atc_note_message_received(agent: &str, thread_id: Option<&str>, timestamp_micros: i64) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("message_received");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "message_received",
            agent,
            ?thread_id,
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_message_received(agent, thread_id, timestamp_micros);
}

pub fn atc_note_reservation_granted(
    agent: &str,
    paths: &[String],
    exclusive: bool,
    project: &str,
    timestamp_micros: i64,
) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("reservation_granted");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "reservation_granted",
            agent,
            ?paths,
            exclusive,
            project,
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_reservation_granted(agent, paths, exclusive, project, timestamp_micros);
}

pub fn atc_note_reservation_released(
    agent: &str,
    paths: &[String],
    project: &str,
    timestamp_micros: i64,
) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("reservation_released");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "reservation_released",
            agent,
            ?paths,
            project,
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_reservation_released(agent, paths, project, timestamp_micros);
}

pub fn atc_note_reservation_conflicts(
    requester: &str,
    project: &str,
    conflicts: &[AtcConflictObservation],
    timestamp_micros: i64,
) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    let conflicts: Vec<(String, String, String)> = conflicts
        .iter()
        .map(|conflict| {
            (
                conflict.holder.clone(),
                conflict.requested_path.clone(),
                conflict.holder_path_pattern.clone(),
            )
        })
        .collect();
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("reservation_conflicts");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "reservation_conflicts",
            requester,
            project,
            conflict_count = conflicts.len(),
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_reservation_conflicts(requester, project, &conflicts, timestamp_micros);
}

pub fn atc_note_reservation_renewed(
    agent: &str,
    paths: &[String],
    project: &str,
    timestamp_micros: i64,
) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("reservation_renewed");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "reservation_renewed",
            agent,
            ?paths,
            project,
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_reservation_renewed(agent, paths, project, timestamp_micros);
}

pub fn atc_note_reservation_force_released(
    agent: &str,
    paths: &[String],
    project: &str,
    timestamp_micros: i64,
) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("reservation_force_released");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "reservation_force_released",
            agent,
            ?paths,
            project,
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_reservation_force_released(agent, paths, project, timestamp_micros);
}

pub fn atc_note_intervention(timestamp_micros: i64) {
    let mode = atc_write_mode();
    if mode.is_off() {
        return;
    }
    let Some(engine) = ATC_ENGINE.get() else {
        return;
    };
    let Ok(mut e) = engine.lock() else {
        return;
    };
    if !e.enabled() {
        return;
    }
    if mode.is_shadow() {
        mcp_agent_mail_core::global_metrics()
            .atc
            .record_shadow_event("intervention");
        tracing::trace!(
            event = "atc.shadow.would_insert",
            family = "intervention",
            timestamp_micros,
            "shadow: would insert experience row"
        );
        return;
    }
    e.note_atc_intervention(timestamp_micros);
}

/// Actionable outputs from an ATC tick.
#[derive(Debug, Clone)]
pub enum AtcTickAction {
    SendAdvisory { agent: String, message: String },
    ReleaseReservations { agent: String },
    ProbeAgent { agent: String },
}

/// Run one ATC tick: evaluate liveness, detect deadlocks, update calibration.
#[must_use]
pub fn atc_tick(now_micros: i64) -> Vec<AtcTickAction> {
    atc_tick_report(now_micros).map_or_else(Vec::new, |report| report.actions)
}

/// Run one ATC tick and return both actions and the fully-populated summary snapshot.
#[must_use]
pub fn atc_tick_report(now_micros: i64) -> Option<AtcTickReport> {
    let lock_started = Instant::now();
    let engine_lock = ATC_ENGINE.get()?;
    // Recover from poisoned mutex: if a previous tick panicked, clear the
    // poison and continue. A permanently disabled ATC engine is worse than
    // operating with potentially inconsistent state (which the calibration
    // guard and safe mode are designed to handle).
    let mut engine = match engine_lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("ATC engine mutex was poisoned (a previous tick panicked); recovering");
            poisoned.into_inner()
        }
    };
    let lock_wait_micros = elapsed_micros(lock_started);
    if !engine.enabled() {
        return None;
    }
    let mut report = engine.run_tick(now_micros);
    report.summary.kernel.lock_wait_micros = lock_wait_micros;
    engine.last_kernel_telemetry.lock_wait_micros = lock_wait_micros;
    Some(report)
}

/// Fetch a cloned ATC decision record by ID from the in-memory evidence ledger.
#[must_use]
pub fn atc_decision_record(decision_id: u64) -> Option<AtcDecisionRecord> {
    let engine_lock = ATC_ENGINE.get()?;
    let engine = engine_lock.lock().unwrap_or_else(|p| p.into_inner());
    engine.ledger.get(decision_id).cloned()
}

/// Query the last observed activity timestamp for an agent.
///
/// Returns `Some(timestamp_micros)` if the agent has been observed,
/// or `None` if the agent is unknown to the ATC engine.
#[must_use]
pub fn atc_agent_last_activity(agent: &str) -> Option<i64> {
    let engine_lock = ATC_ENGINE.get()?;
    let engine = engine_lock.lock().unwrap_or_else(|p| p.into_inner());
    engine
        .agents
        .get(agent)
        .map(|entry| entry.rhythm.last_activity_ts)
        .filter(|ts| *ts > 0)
}

/// Record the outcome of an ATC decision for calibration feedback.
pub fn atc_record_outcome(
    subsystem: AtcSubsystem,
    agent: Option<&str>,
    predicted_loss: f64,
    actual_loss: f64,
    correct: bool,
) {
    if !atc_enabled() {
        return;
    }

    // Feed e-process, CUSUM, and calibration guard
    if let Some(engine_lock) = ATC_ENGINE.get()
        && let Ok(mut engine) = engine_lock.lock()
    {
        engine.eprocess.update(correct, subsystem, agent);
        let now = mcp_agent_mail_core::timestamps::now_micros();
        engine.cusum.update(!correct, now);

        // Clone to satisfy borrow checker (calibration borrows eprocess/cusum)
        let ep_snapshot = engine.eprocess.clone();
        let cusum_snapshot = engine.cusum.clone();
        let was_safe_mode = engine.calibration.is_safe_mode();
        let safe_mode_since = engine.calibration.safe_mode_since();
        let changed = engine
            .calibration
            .update(&ep_snapshot, &cusum_snapshot, correct, now);
        if changed {
            let safe_mode_active = engine.calibration.is_safe_mode();
            mcp_agent_mail_core::global_metrics()
                .atc
                .record_safe_mode_transition(safe_mode_active);
            if safe_mode_active {
                tracing::warn!(
                    event = "atc.safe_mode.enter",
                    reason = format!(
                        "eprocess={} cusum={}",
                        ep_snapshot.miscalibrated(),
                        cusum_snapshot.degradation_detected()
                    ),
                    triggered_by = ?subsystem,
                    "ATC safe mode entered"
                );
            } else if was_safe_mode {
                tracing::info!(
                    event = "atc.safe_mode.exit",
                    duration = now.saturating_sub(safe_mode_since),
                    remediation = "consecutive_correct_recovery",
                    triggered_by = ?subsystem,
                    "ATC safe mode exited"
                );
            }
            engine.mark_agents_dirty();
        }
    }

    // Feed conformal predictor
    if let Some(conformal_lock) = ATC_CONFORMAL.get()
        && let Ok(mut conformal) = conformal_lock.lock()
    {
        conformal.observe(subsystem, predicted_loss, actual_loss);
    }

    // Feed adaptive threshold (liveness only)
    if subsystem == AtcSubsystem::Liveness
        && let Some(agent_name) = agent
        && let Some(thresholds_lock) = ATC_THRESHOLDS.get()
        && let Ok(mut thresholds) = thresholds_lock.lock()
        && let Some(adaptive) = thresholds.get_mut(agent_name)
    {
        adaptive.record_outcome(correct);
    }

    // Feed liveness tuner (regret → PID → loss matrix feedback loop)
    if subsystem == AtcSubsystem::Liveness
        && let Some(tuner_lock) = ATC_LIVENESS_TUNER.get()
        && let Ok(mut tuner) = tuner_lock.lock()
    {
        let regret = (predicted_loss - actual_loss).abs();
        let action = if correct {
            LivenessAction::DeclareAlive
        } else {
            LivenessAction::Suspect
        };
        let state = if correct {
            LivenessState::Alive
        } else {
            LivenessState::Dead
        };
        tuner.record_outcome(action, state, regret);

        if let Some(engine_lock) = ATC_ENGINE.get()
            && let Ok(mut engine) = engine_lock.lock()
            && tuner.maybe_update(&mut engine.liveness_core)
        {
            engine.propagate_liveness_policy();
        }
    }

    // Feed survival estimator.
    // Extract silence duration from engine first, then lock survival separately
    // to avoid holding two locks (SURVIVAL + ENGINE) simultaneously.
    if subsystem == AtcSubsystem::Liveness
        && let Some(agent_name) = agent
    {
        let silence = ATC_ENGINE
            .get()
            .and_then(|l| l.lock().ok())
            .and_then(|engine| {
                engine.agents.get(agent_name).map(|entry| {
                    entry
                        .rhythm
                        .silence_duration(mcp_agent_mail_core::timestamps::now_micros())
                })
            });
        if let Some(silence) = silence
            && let Some(survival_lock) = ATC_SURVIVAL.get()
            && let Ok(mut survival) = survival_lock.lock()
        {
            let estimator = survival
                .entry("all".to_string())
                .or_insert_with(|| KaplanMeierEstimator::new(1000));
            estimator.observe(silence, !correct);
        }
    }
}

/// Get ATC summary for robot mode / TUI display.
#[must_use]
pub fn atc_summary() -> Option<AtcSummarySnapshot> {
    let engine_lock = ATC_ENGINE.get()?;
    let mut engine = engine_lock.lock().ok()?;
    let timings = engine.last_stage_timings.clone();
    let kernel = engine.last_kernel_telemetry.clone();
    let budget = engine.last_budget_telemetry.clone();
    let policy = engine.last_policy_telemetry.clone();
    Some(engine.build_summary_snapshot_with(
        mcp_agent_mail_core::timestamps::now_micros(),
        &timings,
        &kernel,
        &budget,
        &policy,
    ))
}

/// Whether the ATC system is exercising normal authority, operating
/// cautiously, or fully locked down. Derived from safe-mode, budget
/// pressure, and eprocess state — not stored separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPosture {
    /// All subsystems nominal; full intervention authority.
    Normal,
    /// Budget is under pressure or eprocess is elevated; interventions
    /// proceed but with reduced probe frequency.
    Cautious,
    /// Calibration guard is active (safe mode); only advisory actions
    /// are permitted.
    SafeMode,
}

impl ExecutionPosture {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Cautious => "cautious",
            Self::SafeMode => "safe_mode",
        }
    }
}

/// How complete the snapshot is. A `Full` snapshot reads all maintained
/// statistics; `Partial` indicates some subsystem locks were contended;
/// `Fallback` means only cached/stale data was available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCompleteness {
    /// All subsystem data was freshly read.
    Full,
    /// Some subsystem data was unavailable; snapshot is best-effort.
    Partial,
    /// Only cached/stale data was used (lock contention or startup).
    Fallback,
}

/// Concise regime state for the summary: is the system stable, changing,
/// or recovering from a detected change?
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryRegimeState {
    /// No regime change detected; system is stationary.
    Stable,
    /// A degradation was detected and has not recovered.
    Degraded,
    /// An improvement was detected; system has shifted for the better.
    Improved,
}

/// Direction indicator for a trend (computed from recent vs. lifetime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    /// Recent values are lower than lifetime average.
    Improving,
    /// Recent values are close to lifetime average.
    Flat,
    /// Recent values are higher than lifetime average.
    Worsening,
}

/// Stable delta hint: tells consumers what materially changed since the
/// last snapshot without requiring a full diff.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SummaryDelta {
    /// Whether the execution posture changed.
    pub posture_changed: bool,
    /// Whether the regime state changed.
    pub regime_changed: bool,
    /// Whether the policy revision changed.
    pub policy_revision_changed: bool,
    /// Number of agents added since the prior snapshot.
    pub agents_added: usize,
    /// Number of agents removed since the prior snapshot.
    pub agents_removed: usize,
    /// Whether safe mode was entered or exited.
    pub safe_mode_transition: bool,
}

#[derive(Debug, Clone)]
pub struct AtcSummarySnapshot {
    // ── identity & freshness (machine-facing) ────────────────────────
    /// Monotonically increasing snapshot sequence for change detection.
    pub snapshot_seq: u64,
    /// Timestamp when this snapshot was generated (micros since epoch).
    pub generated_at_micros: i64,
    /// How complete this snapshot is.
    pub completeness: SnapshotCompleteness,

    // ── top-level state (operator-facing) ────────────────────────────
    pub enabled: bool,
    pub safe_mode: bool,
    /// Derived posture: normal, cautious, or safe-mode.
    pub execution_posture: ExecutionPosture,
    pub tick_count: u64,

    // ── agent liveness (operator-facing, deterministic order) ────────
    pub tracked_agents: Vec<AgentStateSnapshot>,
    pub deadlock_cycles: usize,

    // ── learning state (machine-facing) ──────────────────────────────
    /// E-process martingale value (>threshold = miscalibrated).
    pub eprocess_value: f64,
    /// Whether the eprocess has crossed the alert threshold.
    pub eprocess_alert: bool,
    /// Lifetime average regret per decision.
    pub regret_avg: f64,
    /// Recent-window average regret.
    pub regret_recent_avg: f64,
    /// Trend direction for regret (recent vs. lifetime).
    pub regret_trend: TrendDirection,
    /// Total decisions recorded in the evidence ledger.
    pub decisions_total: u64,
    /// Open experiences (decisions awaiting ground-truth feedback).
    pub experiences_open: usize,
    /// Resolved experiences (decisions with feedback received).
    pub experiences_resolved: u64,

    // ── calibration & regime (operator-facing) ───────────────────────
    /// Calibration guard state: consecutive correct predictions toward
    /// recovery (0 if not in safe mode).
    pub calibration_consecutive_correct: u64,
    /// Required consecutive correct predictions to exit safe mode.
    pub calibration_recovery_target: u64,
    /// Regime state: stable, degraded, or improved.
    pub regime_state: SummaryRegimeState,
    /// How long the current regime has been active (microseconds).
    pub regime_dwell_micros: i64,
    /// Number of regime changes detected in history.
    pub regime_change_count: usize,

    // ── policy (machine-facing) ─────────────────────────────────────
    /// Monotonic policy revision counter.
    pub policy_revision: u64,

    // ── fairness / concentration (operator-facing) ──────────────────
    /// Name of the agent bearing the most interventions, if any.
    /// `None` if no agents have been intervened on or all share equal load.
    pub most_impacted_agent: Option<String>,

    // ── delta hints (machine-facing) ────────────────────────────────
    /// What materially changed vs. the previous snapshot.
    pub delta: SummaryDelta,

    // ── detailed telemetry (machine-facing) ─────────────────────────
    pub recent_decisions: Vec<AtcDecisionRecord>,
    pub stage_timings: AtcStageTimings,
    pub kernel: AtcKernelTelemetry,
    pub budget: AtcBudgetTelemetry,
    pub policy: AtcPolicyTelemetry,
}

#[derive(Debug, Clone)]
pub struct AgentStateSnapshot {
    pub name: String,
    pub state: LivenessState,
    pub silence_secs: i64,
    pub posterior_alive: f64,
    /// Number of decisions targeting this agent (for concentration detection).
    pub intervention_count: u64,
}

// ── Shared test helpers (accessible from sibling crate modules) ─────

/// Shared lock for tests that mutate global ATC state.
#[cfg(test)]
pub(crate) static GLOBAL_ATC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reset all global ATC state for a fresh test run.
#[cfg(test)]
pub(crate) fn reset_global_atc_state_for_test(config: &mcp_agent_mail_core::Config) {
    let atc_config = AtcEngine::config_from_env(config);
    let fresh_engine = AtcEngine::new(atc_config);

    let engine_lock = ATC_ENGINE.get_or_init(|| Mutex::new(AtcEngine::new_for_testing()));
    *engine_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh_engine;

    let population_lock = ATC_POPULATION.get_or_init(|| Mutex::new(HierarchicalAgentModel::new()));
    *population_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = HierarchicalAgentModel::new();

    let conformal_lock = ATC_CONFORMAL.get_or_init(|| Mutex::new(AtcConformalSet::new(200, 0.90)));
    *conformal_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = AtcConformalSet::new(200, 0.90);

    let thresholds_lock = ATC_THRESHOLDS.get_or_init(|| Mutex::new(HashMap::new()));
    thresholds_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    let survival_lock = ATC_SURVIVAL.get_or_init(|| Mutex::new(HashMap::new()));
    survival_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();

    let tuner_lock = ATC_LIVENESS_TUNER
        .get_or_init(|| Mutex::new(LossMatrixTuner::from_core(&default_liveness_core(), 10)));
    *tuner_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        LossMatrixTuner::from_core(&default_liveness_core(), 10);
}

// ── Tests for alien-artifact enhancements ───────────────────────────

#[cfg(test)]
mod alien_enhancement_tests {
    use super::*;

    // Use the crate-level shared lock and reset function.
    #[allow(unused_imports)]
    use super::{GLOBAL_ATC_TEST_LOCK, reset_global_atc_state_for_test};

    #[allow(dead_code)]
    fn reset_global_atc_state_for_test_local(config: &mcp_agent_mail_core::Config) {
        let atc_config = AtcEngine::config_from_env(config);
        let fresh_engine = AtcEngine::new(atc_config);

        let engine_lock = ATC_ENGINE.get_or_init(|| Mutex::new(AtcEngine::new_for_testing()));
        *engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh_engine;

        let population_lock =
            ATC_POPULATION.get_or_init(|| Mutex::new(HierarchicalAgentModel::new()));
        *population_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = HierarchicalAgentModel::new();

        let conformal_lock =
            ATC_CONFORMAL.get_or_init(|| Mutex::new(AtcConformalSet::new(200, 0.90)));
        *conformal_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = AtcConformalSet::new(200, 0.90);

        let thresholds_lock = ATC_THRESHOLDS.get_or_init(|| Mutex::new(HashMap::new()));
        thresholds_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let survival_lock = ATC_SURVIVAL.get_or_init(|| Mutex::new(HashMap::new()));
        survival_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let tuner_lock = ATC_LIVENESS_TUNER
            .get_or_init(|| Mutex::new(LossMatrixTuner::from_core(&default_liveness_core(), 10)));
        *tuner_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            LossMatrixTuner::from_core(&default_liveness_core(), 10);
    }

    #[test]
    fn tuner_from_core_creates_pids() {
        let core = default_liveness_core();
        let tuner = LossMatrixTuner::from_core(&core, 10);
        assert_eq!(tuner.pids.len(), 9);
    }

    #[test]
    fn tuner_update_interval_respected() {
        let mut core = default_liveness_core();
        let mut tuner = LossMatrixTuner::from_core(&core, 5);
        for _ in 0..4 {
            tuner.record_outcome(LivenessAction::DeclareAlive, LivenessState::Alive, 1.0);
        }
        assert!(!tuner.maybe_update(&mut core));
        tuner.record_outcome(LivenessAction::DeclareAlive, LivenessState::Alive, 1.0);
        assert!(tuner.maybe_update(&mut core));
    }

    #[test]
    fn tuner_adjusts_loss_values() {
        let mut core = default_liveness_core();
        let original_loss = core.loss_entry(LivenessAction::Suspect, LivenessState::Alive);
        let mut tuner = LossMatrixTuner::from_core(&core, 3);
        for _ in 0..3 {
            tuner.record_outcome(LivenessAction::Suspect, LivenessState::Alive, 20.0);
        }
        tuner.maybe_update(&mut core);
        let new_loss = core.loss_entry(LivenessAction::Suspect, LivenessState::Alive);
        assert!(
            (new_loss - original_loss).abs() > 0.001,
            "PID should adjust loss: original={original_loss}, new={new_loss}"
        );
    }

    #[test]
    fn tuned_liveness_policy_propagates_to_existing_and_new_agents() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("Existing", "claude-code", None);

        let original_loss = engine
            .agents
            .get("Existing")
            .expect("existing agent")
            .core
            .loss_entry(LivenessAction::Suspect, LivenessState::Alive);

        let mut tuner = LossMatrixTuner::from_core(&engine.liveness_core, 1);
        tuner.record_outcome(LivenessAction::Suspect, LivenessState::Alive, 20.0);
        assert!(tuner.maybe_update(&mut engine.liveness_core));
        engine.propagate_liveness_policy();

        let tuned_loss = engine
            .liveness_core
            .loss_entry(LivenessAction::Suspect, LivenessState::Alive);
        let existing_loss = engine
            .agents
            .get("Existing")
            .expect("existing agent after propagate")
            .core
            .loss_entry(LivenessAction::Suspect, LivenessState::Alive);

        assert!(
            (tuned_loss - original_loss).abs() > 0.001,
            "tuned template should change: original={original_loss}, tuned={tuned_loss}"
        );
        assert!(
            (existing_loss - tuned_loss).abs() < 1e-9,
            "existing agent should receive tuned policy: existing={existing_loss}, tuned={tuned_loss}"
        );

        engine.register_agent("NewAgent", "claude-code", None);
        let new_loss = engine
            .agents
            .get("NewAgent")
            .expect("new agent")
            .core
            .loss_entry(LivenessAction::Suspect, LivenessState::Alive);
        assert!(
            (new_loss - tuned_loss).abs() < 1e-9,
            "new agent should inherit tuned policy: new={new_loss}, tuned={tuned_loss}"
        );
    }

    #[test]
    fn conformal_set_has_all_subsystems() {
        let cs = AtcConformalSet::new(100, 0.90);
        assert!(cs.sets.contains_key(&AtcSubsystem::Liveness));
        assert!(cs.sets.contains_key(&AtcSubsystem::Conflict));
    }

    #[test]
    fn conformal_interval_widens_with_variance() {
        let mut sc = SubsystemConformal::new(100, 0.90);
        for _ in 0..20 {
            sc.observe(5.0, 5.1);
        }
        let tight = sc.interval_width().unwrap();

        let mut sc2 = SubsystemConformal::new(100, 0.90);
        for i in 0..20 {
            sc2.observe(5.0, f64::from(i) * 2.0);
        }
        let wide = sc2.interval_width().unwrap();
        assert!(wide > tight, "tight={tight}, wide={wide}");
    }

    #[test]
    fn conformal_needs_minimum_data() {
        let sc = SubsystemConformal::new(100, 0.90);
        assert!(sc.interval_width().is_none());
    }

    #[test]
    fn hierarchical_model_default_priors() {
        let model = HierarchicalAgentModel::new();
        let (mean, _) = model.prior_for("claude-code");
        assert!((mean - 60_000_000.0).abs() < 1.0);
    }

    #[test]
    fn hierarchical_model_absorbs_agents() {
        let mut model = HierarchicalAgentModel::new();
        let mut rhythm = AgentRhythm::new(60.0);
        for i in 1..=10 {
            rhythm.observe(i * 30_000_000);
        }
        model.absorb_agent("claude-code", &rhythm);
        let (mean, _) = model.prior_for("claude-code");
        assert!(mean < 60_000_000.0, "mean should shift: {mean}");
    }

    #[test]
    fn engine_population_snapshot_uses_agent_programs() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("Claude", "claude-code", None);
        engine.register_agent("Codex", "codex-cli", None);

        let claude = engine.agents.get_mut("Claude").expect("claude entry");
        for i in 1..=10 {
            claude.rhythm.observe(i * 30_000_000);
        }

        let codex = engine.agents.get_mut("Codex").expect("codex entry");
        for i in 1..=10 {
            codex.rhythm.observe(i * 120_000_000);
        }

        let mut population = HierarchicalAgentModel::new();
        engine.absorb_population_snapshot(&mut population);

        let (claude_mean, _) = population.prior_for("claude-code");
        let (codex_mean, _) = population.prior_for("codex-cli");
        assert!(
            claude_mean < 60_000_000.0,
            "claude prior should shift: {claude_mean}"
        );
        assert!(
            (codex_mean - 120_000_000.0).abs() < 1.0,
            "codex prior should stay near baseline when cadence matches it: {codex_mean}"
        );
    }

    #[test]
    fn hierarchical_unknown_uses_default() {
        let model = HierarchicalAgentModel::new();
        let (mean, _) = model.prior_for("totally-new");
        assert!((mean - 300_000_000.0).abs() < 1.0);
    }

    #[test]
    fn adaptive_threshold_starts_near_base() {
        let at = AdaptiveThreshold::new(3.0);
        assert!((at.effective_k() - 3.0).abs() < 0.5);
    }

    #[test]
    fn adaptive_threshold_decreases_with_tp() {
        let mut at = AdaptiveThreshold::new(3.0);
        let before = at.effective_k();
        for _ in 0..20 {
            at.record_outcome(true);
        }
        assert!(at.effective_k() < before);
    }

    #[test]
    fn adaptive_threshold_increases_with_fp() {
        let mut at = AdaptiveThreshold::new(3.0);
        let before = at.effective_k();
        for _ in 0..20 {
            at.record_outcome(false);
        }
        assert!(at.effective_k() > before);
    }

    #[test]
    fn adaptive_threshold_k_bounded() {
        let mut at = AdaptiveThreshold::new(3.0);
        for _ in 0..1000 {
            at.record_outcome(true);
        }
        assert!(at.effective_k() >= at.min_k);
        let mut at2 = AdaptiveThreshold::new(3.0);
        for _ in 0..1000 {
            at2.record_outcome(false);
        }
        assert!(at2.effective_k() <= at2.max_k);
    }

    #[test]
    fn submodular_schedule_empty() {
        let agents: HashMap<String, AgentLivenessEntry> = HashMap::new();
        assert!(submodular_probe_schedule(&agents, 5, 60.0, 1_000_000).is_empty());
    }

    #[test]
    fn submodular_schedule_respects_max() {
        let mut agents = HashMap::new();
        for i in 0..10 {
            let name = format!("Agent{i}");
            agents.insert(
                name.clone(),
                AgentLivenessEntry {
                    name,
                    project_key: None,
                    program: "claude-code".to_string(),
                    state: LivenessState::Alive,
                    rhythm: AgentRhythm::new(60.0),
                    suspect_since: 0,
                    probe_sent_at: 0,
                    sprt_log_lr: 0.0,
                    core: default_liveness_core(),
                    schedule_version: 0,
                    next_review_micros: i64::MAX,
                },
            );
        }
        let schedule = submodular_probe_schedule(&agents, 3, 60.0, 1_000_000);
        assert!(schedule.len() <= 3);
    }

    #[test]
    fn submodular_schedule_skips_dead_agents() {
        let mut agents = HashMap::new();
        let mut dead_core = default_liveness_core();
        for _ in 0..30 {
            dead_core.update_posterior(&[
                (LivenessState::Alive, 0.01),
                (LivenessState::Flaky, 0.05),
                (LivenessState::Dead, 0.95),
            ]);
        }
        agents.insert(
            "DeadProbe".to_string(),
            AgentLivenessEntry {
                name: "DeadProbe".to_string(),
                project_key: None,
                program: "claude-code".to_string(),
                state: LivenessState::Dead,
                rhythm: AgentRhythm::new(60.0),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: dead_core,
                schedule_version: 0,
                next_review_micros: i64::MAX,
            },
        );
        agents.insert(
            "LiveProbe".to_string(),
            AgentLivenessEntry {
                name: "LiveProbe".to_string(),
                project_key: None,
                program: "claude-code".to_string(),
                state: LivenessState::Alive,
                rhythm: AgentRhythm::new(60.0),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: default_liveness_core(),
                schedule_version: 0,
                next_review_micros: i64::MAX,
            },
        );

        let schedule = submodular_probe_schedule(&agents, 2, 60.0, 1_000_000);
        assert!(
            !schedule.iter().any(|(name, _)| name == "DeadProbe"),
            "dead agents should not consume probe budget"
        );
    }

    #[test]
    fn kaplan_meier_no_data() {
        let km = KaplanMeierEstimator::new(100);
        assert!((km.survival_probability(1_000_000) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn kaplan_meier_survival_decreases() {
        let mut km = KaplanMeierEstimator::new(100);
        for i in 1..=10 {
            km.observe(i * 60_000_000, true);
        }
        let s_early = km.survival_probability(60_000_000);
        let s_late = km.survival_probability(600_000_000);
        assert!(s_early > s_late, "S(1m)={s_early}, S(10m)={s_late}");
    }

    #[test]
    fn kaplan_meier_censored() {
        let mut km = KaplanMeierEstimator::new(100);
        for i in 1..=5 {
            km.observe(i * 60_000_000, true);
            km.observe(i * 60_000_000, false);
        }
        let s = km.survival_probability(300_000_000);
        assert!(s > 0.0 && s < 1.0, "s={s}");
    }

    #[test]
    fn kaplan_meier_hazard_nonneg() {
        let mut km = KaplanMeierEstimator::new(100);
        for i in 1..=10 {
            km.observe(i * 60_000_000, true);
        }
        assert!(km.hazard_rate(300_000_000, 60_000_000) >= 0.0);
    }

    #[test]
    fn hazard_rate_zero_window() {
        let mut km = KaplanMeierEstimator::new(100);
        km.observe(120_000_000, true);
        // Zero window at a point where survival > 0 should return 0.0
        // (no time passes, so no additional risk).
        let h = km.hazard_rate(30_000_000, 0);
        assert!(
            h.abs() < f64::EPSILON,
            "zero window should yield zero hazard, got {h}"
        );
    }

    #[test]
    fn atc_config_from_env_defaults() {
        let config = mcp_agent_mail_core::Config::default();
        let atc_config = AtcEngine::config_from_env(&config);
        assert!(atc_config.enabled);
        assert_eq!(atc_config.probe_interval_micros, 120_000_000);
    }

    #[test]
    fn global_atc_init_and_query() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);
        assert!(atc_enabled());
        atc_register_agent("TestAlpha", "claude-code");
        atc_observe_activity("TestAlpha", 1_000_000);
        assert!(atc_summary().is_some());
    }

    #[test]
    fn atc_summary_sorts_tracked_agents_by_name() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        atc_register_agent("Zulu", "claude-code");
        atc_register_agent("Alpha", "claude-code");
        atc_register_agent("Mike", "claude-code");

        let summary = atc_summary().expect("summary available");
        let names: Vec<&str> = summary
            .tracked_agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        assert_eq!(names, vec!["Alpha", "Mike", "Zulu"]);

        // Verify new learning-state fields have sane defaults.
        assert_eq!(summary.execution_posture, ExecutionPosture::Normal);
        assert_eq!(summary.completeness, SnapshotCompleteness::Full);
        assert_eq!(summary.regime_state, SummaryRegimeState::Stable);
        assert_eq!(summary.regret_trend, TrendDirection::Flat);
        assert!(!summary.eprocess_alert);
        assert_eq!(summary.experiences_open, 0);
        assert_eq!(summary.experiences_resolved, 0);
        assert_eq!(summary.regime_change_count, 0);
        assert!(summary.snapshot_seq > 0);
        assert!(summary.generated_at_micros > 0);
        assert!(summary.most_impacted_agent.is_none());
        // All agents should have zero interventions initially.
        for agent in &summary.tracked_agents {
            assert_eq!(agent.intervention_count, 0);
        }
    }

    #[test]
    fn disabled_global_atc_ignores_registration_and_activity() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: false,
            ..Default::default()
        };
        reset_global_atc_state_for_test(&config);

        atc_register_agent("IgnoredAgent", "claude-code");
        atc_observe_activity("IgnoredAgent", 1_000_000);

        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        let engine = engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!engine.enabled(), "test setup should keep ATC disabled");
        assert!(
            engine.agents.is_empty(),
            "disabled ATC should not accumulate tracked agents"
        );
    }

    #[test]
    fn observe_activity_auto_registers_unknown_agent() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        atc_observe_activity_with_project("DriftAgent", Some("/tmp/project"), 1_000_000);

        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        let engine = engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = engine.agents.get("DriftAgent").expect("agent tracked");
        assert_eq!(entry.program, "unknown-tool");
        assert_eq!(entry.project_key.as_deref(), Some("/tmp/project"));
        assert_eq!(entry.rhythm.last_activity_ts, 1_000_000);
    }

    #[test]
    fn sync_agent_snapshot_does_not_resurrect_from_stale_activity() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        atc_sync_agent_snapshot(
            "BootstrapAgent",
            "claude-code",
            Some("/tmp/project"),
            5_000_000,
        );

        {
            let engine_lock = ATC_ENGINE.get().expect("engine initialized");
            let mut engine = engine_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = engine
                .agents
                .get_mut("BootstrapAgent")
                .expect("agent tracked");
            entry.state = LivenessState::Dead;
            entry.suspect_since = 4_000_000;
        }

        atc_sync_agent_snapshot(
            "BootstrapAgent",
            "codex-cli",
            Some("/tmp/project"),
            5_000_000,
        );

        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        let engine = engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = engine.agents.get("BootstrapAgent").expect("agent tracked");
        assert_eq!(entry.program, "codex-cli");
        assert_eq!(entry.state, LivenessState::Dead);
        assert_eq!(entry.rhythm.last_activity_ts, 5_000_000);
    }

    #[test]
    fn sync_population_from_db_seeds_existing_agents() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        let cx = asupersync::Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("atc-sync-population.db");

        let init_conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");
        init_conn
            .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init pragmas");
        init_conn
            .execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("initialize base schema");
        match fastmcp_core::block_on(mcp_agent_mail_db::schema::migrate_to_latest_base(
            &cx, &init_conn,
        )) {
            asupersync::Outcome::Ok(_) => {}
            asupersync::Outcome::Err(error) => panic!("apply migrations: {error}"),
            other => panic!("unexpected migration outcome: {other:?}"),
        }
        drop(init_conn);

        let pool =
            mcp_agent_mail_db::create_pool_without_startup_init(&mcp_agent_mail_db::DbPoolConfig {
                database_url: format!("sqlite:///{}", db_path.display()),
                min_connections: 1,
                max_connections: 1,
                run_migrations: false,
                warmup_connections: 0,
                ..Default::default()
            })
            .expect("create pool");

        let project = match fastmcp_core::block_on(mcp_agent_mail_db::queries::ensure_project(
            &cx,
            &pool,
            "/tmp/atc-sync-population",
        )) {
            asupersync::Outcome::Ok(project) => project,
            other => panic!("ensure project: {other:?}"),
        };
        let project_id = project.id.expect("project id");

        let agent = match fastmcp_core::block_on(mcp_agent_mail_db::queries::register_agent(
            &cx,
            &pool,
            project_id,
            "BlueLake",
            "claude-code",
            "gpt5",
            Some("ATC hydration test"),
            None,
            None,
        )) {
            asupersync::Outcome::Ok(agent) => agent,
            other => panic!("register agent: {other:?}"),
        };

        let stats = atc_sync_population_from_db(&pool).expect("sync population");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.agents, 1);
        assert_eq!(stats.active_agents, 1);
        assert_eq!(
            atc_agent_last_activity("BlueLake"),
            Some(agent.last_active_ts)
        );

        let summary = atc_summary().expect("summary");
        assert!(
            summary
                .tracked_agents
                .iter()
                .any(|tracked| tracked.name == "BlueLake"),
            "BlueLake should be present in hydrated ATC population"
        );
    }

    #[test]
    fn sync_population_from_db_clamps_negative_recency_override() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ATC_POPULATION_RECENCY_SECS", "-1")],
            || {
                let config = mcp_agent_mail_core::Config::default();
                reset_global_atc_state_for_test(&config);

                let cx = asupersync::Cx::for_testing();
                let dir = tempfile::tempdir().expect("tempdir");
                let db_path = dir.path().join("atc-sync-population-negative-recency.db");

                let init_conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
                    .expect("open db");
                init_conn
                    .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
                    .expect("apply init pragmas");
                init_conn
                    .execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                    .expect("initialize base schema");
                match fastmcp_core::block_on(mcp_agent_mail_db::schema::migrate_to_latest_base(
                    &cx, &init_conn,
                )) {
                    asupersync::Outcome::Ok(_) => {}
                    asupersync::Outcome::Err(error) => panic!("apply migrations: {error}"),
                    other => panic!("unexpected migration outcome: {other:?}"),
                }
                drop(init_conn);

                let pool = mcp_agent_mail_db::create_pool_without_startup_init(
                    &mcp_agent_mail_db::DbPoolConfig {
                        database_url: format!("sqlite:///{}", db_path.display()),
                        min_connections: 1,
                        max_connections: 1,
                        run_migrations: false,
                        warmup_connections: 0,
                        ..Default::default()
                    },
                )
                .expect("create pool");

                let project =
                    match fastmcp_core::block_on(mcp_agent_mail_db::queries::ensure_project(
                        &cx,
                        &pool,
                        "/tmp/atc-sync-population-negative-recency",
                    )) {
                        asupersync::Outcome::Ok(project) => project,
                        other => panic!("ensure project: {other:?}"),
                    };
                let project_id = project.id.expect("project id");

                let agent =
                    match fastmcp_core::block_on(mcp_agent_mail_db::queries::register_agent(
                        &cx,
                        &pool,
                        project_id,
                        "AmberLake",
                        "claude-code",
                        "gpt5",
                        Some("ATC negative recency test"),
                        None,
                        None,
                    )) {
                        asupersync::Outcome::Ok(agent) => agent,
                        other => panic!("register agent: {other:?}"),
                    };

                let stats = atc_sync_population_from_db(&pool).expect("sync population");
                assert_eq!(stats.projects, 1);
                assert_eq!(stats.agents, 1);
                assert_eq!(stats.active_agents, 1);
                assert_eq!(
                    atc_agent_last_activity("AmberLake"),
                    Some(agent.last_active_ts)
                );

                let summary = atc_summary().expect("summary");
                assert!(
                    summary
                        .tracked_agents
                        .iter()
                        .any(|tracked| tracked.name == "AmberLake"),
                    "AmberLake should remain hydrated even when the recency override is negative"
                );
            },
        );
    }

    #[test]
    fn disabled_global_atc_ignores_recorded_outcomes() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: false,
            ..Default::default()
        };
        reset_global_atc_state_for_test(&config);

        atc_record_outcome(
            AtcSubsystem::Liveness,
            Some("IgnoredAgent"),
            10.0,
            90.0,
            false,
        );

        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        let engine = engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!engine.enabled(), "test setup should keep ATC disabled");
        assert!(
            (engine.eprocess().e_value() - 1.0).abs() < f64::EPSILON,
            "disabled ATC should not update calibration state"
        );
    }

    #[test]
    fn init_global_atc_reinitializes_engine_and_clears_stale_agents() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let seeded_config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            atc_probe_interval_secs: 7,
            ..Default::default()
        };
        init_global_atc(&seeded_config);
        atc_register_agent("StickyAgent", "claude-code");
        atc_observe_activity("StickyAgent", 1_000_000);

        {
            let engine_lock = ATC_ENGINE.get().expect("engine initialized");
            let engine = engine_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                engine.enabled(),
                "seed config should allow tracked agents before reinit"
            );
            assert_eq!(engine.config.probe_interval_micros, 7_000_000);
            assert!(
                engine.agents.contains_key("StickyAgent"),
                "setup should register a tracked agent before reinit"
            );
        }

        let enabled_config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            atc_probe_interval_secs: 13,
            ..Default::default()
        };
        init_global_atc(&enabled_config);

        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        let engine = engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(engine.enabled(), "reinit should refresh enabled state");
        assert_eq!(engine.config.probe_interval_micros, 13_000_000);
        assert!(
            engine.agents.is_empty(),
            "reinit should discard stale tracked agents from prior runtime"
        );
    }

    #[test]
    fn atc_tick_no_actions_for_active_agent() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);
        let now = mcp_agent_mail_core::timestamps::now_micros();
        atc_register_agent("TickTest", "claude-code");
        atc_observe_activity("TickTest", now);
        let actions = atc_tick(now + 1_000_000);
        assert!(!actions.iter().any(
            |a| matches!(a, AtcTickAction::ReleaseReservations { agent } if agent == "TickTest")
        ));
    }

    #[test]
    fn atc_tick_release_candidate_is_not_probed_in_same_tick() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        let now = 9 * 60_000_000 + 600_000_000;
        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        {
            let mut engine = engine_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            engine.register_agent("ReleaseNoProbe", "claude-code", None);
            for i in 0..10 {
                engine.observe_activity("ReleaseNoProbe", None, i * 60_000_000);
            }
            let entry = engine.agents.get_mut("ReleaseNoProbe").expect("agent");
            for _ in 0..30 {
                entry.core.update_posterior(&[
                    (LivenessState::Alive, 0.01),
                    (LivenessState::Flaky, 0.05),
                    (LivenessState::Dead, 0.95),
                ]);
            }
        }

        let actions = atc_tick(now);
        assert!(
            actions.iter().any(|action| matches!(
                action,
                AtcTickAction::ReleaseReservations { agent } if agent == "ReleaseNoProbe"
            )),
            "severely silent agent should still surface a release recommendation"
        );
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                AtcTickAction::ProbeAgent { agent } if agent == "ReleaseNoProbe"
            )),
            "same tick should not both release and probe the same agent"
        );
    }

    #[test]
    fn atc_tick_uncertainty_withholds_release_without_false_release_notice() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        let now = 9 * 60_000_000 + 600_000_000;
        let engine_lock = ATC_ENGINE.get().expect("engine initialized");
        {
            let mut engine = engine_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            engine.register_agent("ConformalTick", "claude-code", None);
            for i in 0..10 {
                engine.observe_activity("ConformalTick", None, i * 60_000_000);
            }
            let entry = engine.agents.get_mut("ConformalTick").expect("agent");
            for _ in 0..30 {
                entry.core.update_posterior(&[
                    (LivenessState::Alive, 0.01),
                    (LivenessState::Flaky, 0.05),
                    (LivenessState::Dead, 0.95),
                ]);
            }
        }

        let conformal_lock = ATC_CONFORMAL.get().expect("conformal initialized");
        {
            let mut conformal = conformal_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for _ in 0..6 {
                conformal.observe(AtcSubsystem::Liveness, 100.0, 0.0);
            }
        }

        let actions = atc_tick(now);
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                AtcTickAction::ReleaseReservations { agent } if agent == "ConformalTick"
            )),
            "uncertain liveness should suppress automated release"
        );
        assert!(
            actions.iter().any(|action| matches!(
                action,
                AtcTickAction::SendAdvisory { agent, message }
                    if agent == "ConformalTick"
                        && message.contains("withheld automated release")
            )),
            "suppressed release should emit a softer advisory"
        );
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                AtcTickAction::SendAdvisory { agent, message }
                    if agent == "ConformalTick"
                        && message.contains("requested reservation release")
            )),
            "suppressed release should not claim a release request was made"
        );
        let engine = engine_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_ne!(
            engine.agent_liveness("ConformalTick"),
            Some(LivenessState::Dead),
            "suppressed release should not leave the agent in Dead state"
        );
    }

    #[test]
    fn deadlock_cycles_remain_advisory_without_independent_dead_agent_evidence() {
        let mut engine = AtcEngine::new_for_testing();
        let mut graph = ProjectConflictGraph::default();
        graph.generation = 1;
        // `HardEdge::since` must be within `conflict_edge_ttl_micros()` of
        // `now_micros`; anything older gets pruned by
        // `prune_stale_conflicts` at the top of `run_tick`, which would
        // erase the cycle before `detect_deadlocks` ever ran and leave
        // `actions`/`effects` empty. Anchor both edges slightly before
        // `now_micros` so the cycle survives into the deadlock stage.
        let now_micros = 1_700_000_000_000_000_i64;
        let fresh_since = now_micros - 1_000_000; // 1s old → well inside TTL
        graph.hard_edges.insert(
            "AgentA".to_string(),
            vec![HardEdge {
                holder: "AgentA".to_string(),
                blocked: "AgentB".to_string(),
                contested_patterns: vec!["src/lib.rs".to_string()],
                since: fresh_since,
            }],
        );
        graph.hard_edges.insert(
            "AgentB".to_string(),
            vec![HardEdge {
                holder: "AgentB".to_string(),
                blocked: "AgentA".to_string(),
                contested_patterns: vec!["src/main.rs".to_string()],
                since: fresh_since,
            }],
        );
        engine
            .conflict_graphs
            .insert("deadlock-project".to_string(), graph);
        engine.dirty_projects.insert("deadlock-project".to_string());

        let report = engine.run_tick(now_micros);
        assert!(
            !report
                .actions
                .iter()
                .any(|action| matches!(action, AtcTickAction::ReleaseReservations { .. })),
            "deadlock remediation should not auto-release reservations without separate liveness evidence"
        );
        assert!(
            report.actions.iter().any(|action| matches!(
                action,
                AtcTickAction::SendAdvisory { agent, message }
                    if (agent == "AgentA" || agent == "AgentB")
                        && message.contains("Deadlock in deadlock-project")
                        && message.contains("release only inactive work")
            )),
            "deadlock remediation should emit a targeted advisory instead"
        );
        assert!(
            !report
                .effects
                .iter()
                .any(|effect| effect.kind == "release_reservations_requested"),
            "deadlock remediation effects must stay advisory-only"
        );
        assert!(
            report.effects.iter().any(|effect| {
                effect.kind == "send_advisory"
                    && effect.category == "conflict"
                    && effect.semantics.family == "deadlock_remediation"
            }),
            "deadlock remediation should still publish a durable advisory effect"
        );
    }

    #[test]
    fn detect_deadlocks_reuses_cache_until_generation_changes() {
        let mut engine = AtcEngine::new_for_testing();
        let mut graph = ProjectConflictGraph::default();
        graph.generation = 1;
        graph.hard_edges.insert(
            "AgentA".to_string(),
            vec![HardEdge {
                holder: "AgentA".to_string(),
                blocked: "AgentB".to_string(),
                contested_patterns: vec!["src/lib.rs".to_string()],
                since: 1,
            }],
        );
        graph.hard_edges.insert(
            "AgentB".to_string(),
            vec![HardEdge {
                holder: "AgentB".to_string(),
                blocked: "AgentA".to_string(),
                contested_patterns: vec!["src/main.rs".to_string()],
                since: 2,
            }],
        );
        engine
            .conflict_graphs
            .insert("cache-project".to_string(), graph.clone());
        engine.dirty_projects.insert("cache-project".to_string());

        let first = engine.detect_deadlocks();
        assert_eq!(first.len(), 1);
        assert_eq!(engine.deadlock_cache_misses, 1);
        assert_eq!(engine.deadlock_cache_hits, 0);

        let second = engine.detect_deadlocks();
        assert_eq!(second.len(), 1);
        assert_eq!(engine.deadlock_cache_hits, 1);

        engine
            .conflict_graphs
            .get_mut("cache-project")
            .expect("graph")
            .generation += 1;
        engine.dirty_projects.insert("cache-project".to_string());
        let third = engine.detect_deadlocks();
        assert_eq!(third.len(), 1);
        assert_eq!(engine.deadlock_cache_misses, 2);
    }

    #[test]
    fn budgeted_probe_schedule_respects_budget_and_exclusions() {
        let mut agents = HashMap::new();
        for name in ["Alpha", "Beta", "Gamma"] {
            let mut entry = AgentLivenessEntry {
                name: name.to_string(),
                project_key: None,
                program: "claude-code".to_string(),
                state: LivenessState::Alive,
                rhythm: AgentRhythm::new(60.0),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: default_liveness_core(),
                schedule_version: 0,
                next_review_micros: i64::MAX,
            };
            for _ in 0..20 {
                entry.core.update_posterior(&[
                    (LivenessState::Alive, 0.3),
                    (LivenessState::Flaky, 0.9),
                    (LivenessState::Dead, 0.9),
                ]);
            }
            agents.insert(name.to_string(), entry);
        }

        let policy = AtcLivenessPolicyArtifact::from_core(
            "probe-test".to_string(),
            &default_liveness_core(),
            3.0,
        );
        let mut excluded = HashSet::new();
        excluded.insert("Alpha".to_string());

        let selected = budgeted_probe_schedule(
            &agents,
            &excluded,
            &policy,
            AtcBudgetMode::Nominal,
            3,
            10,
            10,
            1_000_000,
        );
        assert_eq!(selected.len(), 1, "probe budget should cap the schedule");
        assert_ne!(
            selected[0].agent, "Alpha",
            "excluded agents must be skipped"
        );
    }

    #[test]
    fn budgeted_probe_schedule_respects_slow_controller_limit() {
        let mut agents = HashMap::new();
        for name in ["Alpha", "Beta", "Gamma"] {
            let mut entry = AgentLivenessEntry {
                name: name.to_string(),
                project_key: None,
                program: "claude-code".to_string(),
                state: LivenessState::Alive,
                rhythm: AgentRhythm::new(60.0),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: default_liveness_core(),
                schedule_version: 0,
                next_review_micros: i64::MAX,
            };
            for _ in 0..20 {
                entry.core.update_posterior(&[
                    (LivenessState::Alive, 0.3),
                    (LivenessState::Flaky, 0.9),
                    (LivenessState::Dead, 0.9),
                ]);
            }
            agents.insert(name.to_string(), entry);
        }

        let policy = AtcLivenessPolicyArtifact::from_core(
            "probe-test".to_string(),
            &default_liveness_core(),
            3.0,
        );

        let selected = budgeted_probe_schedule(
            &agents,
            &HashSet::new(),
            &policy,
            AtcBudgetMode::Nominal,
            1,
            1_000_000,
            10,
            1_000_000,
        );
        assert_eq!(
            selected.len(),
            1,
            "slow controller probe limit should cap probe selection"
        );
    }

    #[test]
    fn shadow_policy_probe_disagreement_does_not_dilute_regret_average() {
        let mut shadow = AtcShadowPolicyState::default();
        shadow.record_decision_pair(
            LivenessAction::DeclareAlive,
            LivenessAction::Suspect,
            10.0,
            2.0,
        );
        assert!((shadow.average_regret() - 8.0).abs() < f64::EPSILON);

        shadow.record_probe_disagreement(&["Alpha".to_string()], &["Beta".to_string()]);
        assert!(
            (shadow.average_regret() - 8.0).abs() < f64::EPSILON,
            "probe disagreements should not dilute regret averages"
        );
    }

    #[test]
    fn schedule_compaction_discards_stale_heap_entries() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("HeapAgent", "claude-code", None);

        for tick in 1..=65 {
            engine.schedule_entry_for_push("HeapAgent", tick);
        }

        assert_eq!(
            engine.liveness_schedule.len(),
            1,
            "compaction should keep only the latest schedule entry per agent"
        );
        assert_eq!(engine.scheduled_agent_count(), 1);
    }

    #[test]
    fn observe_activity_reschedules_with_single_agent_adaptive_threshold() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("Adaptive", "claude-code", None);

        let thresholds_lock = ATC_THRESHOLDS.get_or_init(|| Mutex::new(HashMap::new()));
        {
            let mut thresholds = thresholds_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let adaptive = thresholds
                .entry("Adaptive".to_string())
                .or_insert_with(|| AdaptiveThreshold::new(engine.config.suspicion_k));
            for _ in 0..8 {
                adaptive.record_outcome(true);
            }
        }

        let observed_at = 60_000_000;
        engine.observe_activity("Adaptive", None, observed_at);

        let expected_k = AtcEngine::effective_threshold(
            engine.incumbent_policy.suspicion_k,
            current_adaptive_threshold_k("Adaptive"),
            false,
        );
        let entry = engine.agents.get("Adaptive").expect("adaptive agent");
        let expected_review = engine.next_review_time_for_policy(entry, expected_k, observed_at);
        assert_eq!(entry.next_review_micros, expected_review);
    }

    #[test]
    fn atc_tick_report_surfaces_budget_policy_and_kernel_telemetry() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        reset_global_atc_state_for_test(&config);

        let now = 10 * 60_000_000;
        atc_register_agent("TelemetryAgent", "claude-code");
        for i in 0..5 {
            atc_observe_activity("TelemetryAgent", i * 60_000_000);
        }

        let report = atc_tick_report(now).expect("tick report");
        assert!(report.summary.budget.tick_budget_micros > 0);
        assert!(!report.summary.policy.incumbent_policy_id.is_empty());
        assert!(report.summary.policy.shadow_enabled);
        assert!(
            report.summary.stage_timings.total_micros
                >= report.summary.stage_timings.liveness_micros
        );
        assert!(report.summary.kernel.scheduled_agents >= report.summary.tracked_agents.len());
    }

    #[test]
    fn effect_plan_carries_registered_project_key() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("ProjectAgent", "claude-code", Some("/tmp/project-agent"));

        let core = default_liveness_core();
        let decision_id = engine.ledger.record(&DecisionBuilder {
            subsystem: AtcSubsystem::Liveness,
            decision_class: "liveness_transition",
            subject: "ProjectAgent",
            core: &core,
            action: LivenessAction::Suspect,
            expected_loss: 1.0,
            runner_up_loss: 2.0,
            evidence_summary: "silence threshold exceeded",
            calibration_healthy: true,
            safe_mode_active: false,
            policy_id: Some("policy-test"),
            fallback_reason: None,
            timestamp_micros: 1_000_000,
        });

        let effect = engine
            .effect_plan_for_decision_id(
                decision_id,
                1_000_000,
                "send_advisory",
                "liveness",
                "liveness_monitoring",
                "ProjectAgent".to_string(),
                engine.agent_project_key("ProjectAgent"),
                Some("project-aware advisory".to_string()),
            )
            .expect("effect plan");

        assert_eq!(effect.project_key.as_deref(), Some("/tmp/project-agent"));
        assert_eq!(effect.semantics.family, "liveness_monitoring");
        assert!(effect.semantics.requires_project);
        assert!(!effect.semantics.ack_required);
        assert_eq!(
            effect.semantics.cooldown_key,
            "liveness_monitoring:/tmp/project-agent:ProjectAgent"
        );
    }

    #[test]
    fn effect_ids_are_unique_per_effect_variant() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("EffectAgent", "claude-code", Some("/tmp/effect-agent"));

        let core = default_liveness_core();
        let decision_id = engine.ledger.record(&DecisionBuilder {
            subsystem: AtcSubsystem::Liveness,
            decision_class: "liveness_transition",
            subject: "EffectAgent",
            core: &core,
            action: LivenessAction::ReleaseReservations,
            expected_loss: 0.5,
            runner_up_loss: 1.5,
            evidence_summary: "posterior dead state crossed release threshold",
            calibration_healthy: true,
            safe_mode_active: false,
            policy_id: Some("policy-test"),
            fallback_reason: None,
            timestamp_micros: 1_000_000,
        });

        let release = engine
            .effect_plan_for_decision_id(
                decision_id,
                1_000_000,
                "release_reservations_requested",
                "liveness",
                "reservation_release",
                "EffectAgent".to_string(),
                engine.agent_project_key("EffectAgent"),
                None,
            )
            .expect("release effect");
        let advisory = engine
            .effect_plan_for_decision_id(
                decision_id,
                1_000_000,
                "send_advisory",
                "liveness",
                "release_notice",
                "EffectAgent".to_string(),
                engine.agent_project_key("EffectAgent"),
                Some("release requested".to_string()),
            )
            .expect("advisory effect");

        assert_ne!(release.effect_id, advisory.effect_id);
        assert!(release.semantics.high_risk_intervention);
        assert_eq!(release.semantics.family, "reservation_release");
        assert_eq!(advisory.semantics.family, "release_notice");
    }

    #[test]
    fn engine_loads_policy_bundle_from_disk() {
        let incumbent = AtcLivenessPolicyArtifact::from_core(
            "liveness-incumbent-r99".to_string(),
            &default_liveness_core(),
            4.25,
        );
        let bundle = AtcLivenessPolicyBundle::from_live_policies(
            &incumbent,
            Some(&AtcLivenessPolicyArtifact::candidate_from_incumbent(
                &incumbent,
            )),
            99,
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_path = dir.path().join("atc-policy-bundle.json");
        std::fs::write(
            &bundle_path,
            serde_json::to_vec_pretty(&bundle).expect("serialize bundle"),
        )
        .expect("write bundle");

        let engine = AtcEngine::new(AtcConfig {
            policy_bundle_path: Some(bundle_path.display().to_string()),
            ..AtcConfig::default()
        });

        assert_eq!(engine.policy_bundle.bundle_hash, bundle.bundle_hash);
        assert_eq!(engine.incumbent_policy.policy_id, "liveness-incumbent-r99");
        assert!((engine.incumbent_policy.suspicion_k - 4.25).abs() < f64::EPSILON);
    }
}
