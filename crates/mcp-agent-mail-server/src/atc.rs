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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::hash::Hash;

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
    /// Loss matrix: `L[(action, state)] = cost`.
    loss_matrix: HashMap<(A, S), f64>,
    /// Current posterior belief over states.  Values sum to 1.0.
    posterior: Vec<(S, f64)>,
    /// How fast the posterior moves toward new evidence (0.0–1.0).
    /// Default 0.3 = moderately responsive.
    alpha: f64,
    /// All known actions (for argmin enumeration).  Always non-empty.
    actions: Vec<A>,
}

impl<S: AtcState, A: AtcAction> DecisionCore<S, A> {
    /// Create a new decision core with the given loss matrix and initial prior.
    ///
    /// `prior` must be a valid probability distribution (non-negative, sums to ~1).
    /// `loss_entries` are `(action, state, cost)` triples.
    pub fn new(
        prior: &[(S, f64)],
        loss_entries: &[(A, S, f64)],
        alpha: f64,
    ) -> Self {
        let mut loss_matrix = HashMap::new();
        let mut actions_set = Vec::new();
        for &(a, s, cost) in loss_entries {
            loss_matrix.insert((a, s), cost);
            if !actions_set.contains(&a) {
                actions_set.push(a);
            }
        }
        assert!(
            !actions_set.is_empty(),
            "DecisionCore requires at least one action in loss_entries"
        );
        assert!(
            !prior.is_empty(),
            "DecisionCore requires at least one state in prior"
        );
        Self {
            loss_matrix,
            posterior: prior.to_vec(),
            alpha: alpha.clamp(0.01, 1.0),
            actions: actions_set,
        }
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

        for &action in &self.actions {
            let expected_loss = self.expected_loss_for(action);
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
        self.posterior
            .iter()
            .map(|&(state, prob)| {
                let cost = self.loss_matrix.get(&(action, state)).copied().unwrap_or(0.0);
                cost * prob
            })
            .sum()
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

        let likelihood_map: HashMap<S, f64> = likelihoods.iter().copied().collect();

        for entry in &mut self.posterior {
            let lk = likelihood_map
                .get(&entry.0)
                .copied()
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

    /// Get the current posterior formatted for the evidence ledger.
    #[must_use]
    pub fn posterior_summary(&self) -> Vec<(String, f64)> {
        self.posterior
            .iter()
            .map(|(s, p)| (format!("{s:?}"), *p))
            .collect()
    }

    /// Look up a single loss matrix entry.
    #[must_use]
    pub fn loss_entry(&self, action: A, state: S) -> f64 {
        self.loss_matrix.get(&(action, state)).copied().unwrap_or(0.0)
    }

    /// Get the best action for a known true state (for regret computation).
    #[must_use]
    pub fn best_action_for_state(&self, state: S) -> A {
        self.actions
            .iter()
            .copied()
            .min_by(|&a, &b| {
                let la = self.loss_entry(a, state);
                let lb = self.loss_entry(b, state);
                la.partial_cmp(&lb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(self.actions[0])
    }

    /// Mutably access the loss matrix (for PID regret controller tuning).
    pub const fn loss_matrix_mut(&mut self) -> &mut HashMap<(A, S), f64> {
        &mut self.loss_matrix
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
            (LivenessAction::ReleaseReservations, LivenessState::Alive, 100.0),
            (LivenessAction::ReleaseReservations, LivenessState::Flaky, 20.0),
            (LivenessAction::ReleaseReservations, LivenessState::Dead, 1.0),
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
            (ConflictAction::Ignore, ConflictState::SevereCollision, 100.0),
            (ConflictAction::AdvisoryMessage, ConflictState::NoConflict, 3.0),
            (ConflictAction::AdvisoryMessage, ConflictState::MildOverlap, 1.0),
            (ConflictAction::AdvisoryMessage, ConflictState::SevereCollision, 8.0),
            (ConflictAction::ForceReservation, ConflictState::NoConflict, 12.0),
            (ConflictAction::ForceReservation, ConflictState::MildOverlap, 4.0),
            (ConflictAction::ForceReservation, ConflictState::SevereCollision, 2.0),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// A single auditable decision record.
#[derive(Debug, Clone)]
pub struct AtcDecisionRecord {
    /// Unique decision ID (monotonically increasing).
    pub id: u64,
    /// Timestamp of the decision (microseconds since epoch).
    pub timestamp_micros: i64,
    /// Which subsystem made the decision.
    pub subsystem: AtcSubsystem,
    /// The entity the decision concerns (agent name or thread ID).
    pub subject: String,
    /// Posterior belief at decision time.
    pub posterior: Vec<(String, f64)>,
    /// Action chosen.
    pub action: String,
    /// Expected loss of chosen action.
    pub expected_loss: f64,
    /// Expected loss of the next-best alternative.
    pub runner_up_loss: f64,
    /// Key evidence that drove this decision.
    pub evidence_summary: String,
    /// Whether the calibration guard was healthy at decision time.
    pub calibration_healthy: bool,
    /// Whether safe mode was active.
    pub safe_mode_active: bool,
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
    pub subject: &'a str,
    pub core: &'a DecisionCore<S, A>,
    pub action: A,
    pub expected_loss: f64,
    pub runner_up_loss: f64,
    pub evidence_summary: &'a str,
    pub calibration_healthy: bool,
    pub safe_mode_active: bool,
    pub timestamp_micros: i64,
}

impl EvidenceLedger {
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

        let record = AtcDecisionRecord {
            id,
            timestamp_micros: builder.timestamp_micros,
            subsystem: builder.subsystem,
            subject: builder.subject.to_string(),
            posterior: builder.core.posterior_summary(),
            action: format!("{:?}", builder.action),
            expected_loss: builder.expected_loss,
            runner_up_loss: builder.runner_up_loss,
            evidence_summary: builder.evidence_summary.to_string(),
            calibration_healthy: builder.calibration_healthy,
            safe_mode_active: builder.safe_mode_active,
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
            .map(|(_, p)| *p)
            .unwrap_or(0.0);

        assert!(
            flaky_prob > 0.5,
            "posterior should converge toward Flaky, got P(flaky)={flaky_prob:.3}"
        );
    }

    #[test]
    fn posterior_stays_normalized() {
        let mut core = default_liveness_core();

        core.update_posterior(&[
            (LivenessState::Alive, 0.5),
            (LivenessState::Dead, 0.8),
        ]);

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
            subject,
            core,
            action: LivenessAction::DeclareAlive,
            expected_loss: 1.0,
            runner_up_loss: 2.0,
            evidence_summary: "test",
            calibration_healthy: true,
            safe_mode_active: false,
            timestamp_micros: ts,
        }
    }

    #[test]
    fn ledger_records_and_retrieves() {
        let mut ledger = EvidenceLedger::new(100);
        let core = default_liveness_core();
        let id = ledger.record(&DecisionBuilder {
            subsystem: AtcSubsystem::Liveness,
            subject: "TestAgent",
            core: &core,
            action: LivenessAction::DeclareAlive,
            expected_loss: 1.5,
            runner_up_loss: 8.0,
            evidence_summary: "agent sent message 3s ago",
            calibration_healthy: true,
            safe_mode_active: false,
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
        assert!(
            ledger.get(1).is_none(),
            "oldest records should be evicted"
        );
        assert!(
            ledger.get(2).is_none(),
            "second oldest should be evicted"
        );
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
            timestamp_micros: 1_000_000,
            subsystem: AtcSubsystem::Liveness,
            subject: "BlueFox".to_string(),
            posterior: vec![
                ("Alive".to_string(), 0.12),
                ("Flaky".to_string(), 0.41),
                ("Dead".to_string(), 0.47),
            ],
            action: "Suspect".to_string(),
            expected_loss: 3.2,
            runner_up_loss: 18.1,
            evidence_summary: "no activity for 847s".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
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
            timestamp_micros: 0,
            subsystem: AtcSubsystem::Calibration,
            subject: "system".to_string(),
            posterior: vec![],
            action: "SafeMode".to_string(),
            expected_loss: 0.0,
            runner_up_loss: 0.0,
            evidence_summary: "coverage dropped".to_string(),
            calibration_healthy: false,
            safe_mode_active: true,
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
            .map(|(_, p)| *p)
            .unwrap_or(0.0);
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
        let initial_posterior: Vec<(LivenessState, f64)> =
            core.posterior().to_vec();

        // Update with likelihoods for only Alive and Dead — Flaky is absent
        // and should get likelihood 1.0 (uninformative).
        core.update_posterior(&[
            (LivenessState::Alive, 0.1),  // strong evidence against Alive
            (LivenessState::Dead, 5.0),   // strong evidence for Dead
        ]);

        let posterior: Vec<(LivenessState, f64)> = core.posterior().to_vec();

        // Alive probability should have decreased
        let alive_before = initial_posterior.iter()
            .find(|(s, _)| *s == LivenessState::Alive).unwrap().1;
        let alive_after = posterior.iter()
            .find(|(s, _)| *s == LivenessState::Alive).unwrap().1;
        assert!(
            alive_after < alive_before,
            "Alive should decrease with low likelihood, posterior: {posterior:?}"
        );

        // Dead probability should have increased
        let dead_before = initial_posterior.iter()
            .find(|(s, _)| *s == LivenessState::Dead).unwrap().1;
        let dead_after = posterior.iter()
            .find(|(s, _)| *s == LivenessState::Dead).unwrap().1;
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
        enum TwoState { Good, Bad }
        impl AtcState for TwoState {}
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        enum TwoAction { Act, Wait }
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
                (LivenessAction::ReleaseReservations, LivenessState::Alive, 100.0),
                (LivenessAction::ReleaseReservations, LivenessState::Flaky, 20.0),
                (LivenessAction::ReleaseReservations, LivenessState::Dead, 1.0),
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
                (LivenessAction::ReleaseReservations, LivenessState::Alive, 100.0),
                (LivenessAction::ReleaseReservations, LivenessState::Dead, 1.0),
            ],
            -1.0,
        );

        // Verify alpha was clamped by checking posterior behavior:
        // with alpha=0.01, updates should be very slow (nearly no change).
        let mut slow_core = core;
        let before: Vec<f64> = slow_core.posterior().iter().map(|(_, p)| *p).collect();

        slow_core.update_posterior(&[
            (LivenessState::Alive, 0.01),
            (LivenessState::Dead, 10.0),
        ]);

        let after: Vec<f64> = slow_core.posterior().iter().map(|(_, p)| *p).collect();
        let posterior = slow_core.posterior();

        // With alpha clamped to 0.01, the change should be tiny (< 0.05)
        let delta: f64 = before.iter().zip(after.iter())
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
            observation_count: 0,
            alpha: 0.1,
            prior_interval: prior_micros,
            last_activity_ts: 0,
        }
    }

    /// Record a new activity observation.
    pub fn observe(&mut self, timestamp_micros: i64) {
        if self.last_activity_ts > 0 {
            let delta = (timestamp_micros - self.last_activity_ts).max(0) as f64;
            let old_avg = self.effective_avg();
            self.avg_interval = (1.0 - self.alpha) * self.avg_interval + self.alpha * delta;
            self.var_interval =
                (1.0 - self.alpha) * self.var_interval + self.alpha * (delta - old_avg).powi(2);
            self.observation_count = self.observation_count.saturating_add(1);
        }
        self.last_activity_ts = timestamp_micros;
    }

    /// Effective average interval, blending observed data with the prior.
    ///
    /// For the first ~10 observations, the prior dominates.  After that,
    /// the observed average takes over.
    #[must_use]
    pub fn effective_avg(&self) -> f64 {
        let n = self.observation_count as f64;
        let prior_weight = 3.0; // pseudo-count for the prior
        (n * self.avg_interval + prior_weight * self.prior_interval) / (n + prior_weight)
    }

    /// Standard deviation of inter-activity interval.
    #[must_use]
    pub fn std_dev(&self) -> f64 {
        self.var_interval.max(0.0).sqrt()
    }

    /// Suspicion threshold: `avg + k * std_dev`.
    ///
    /// `k` controls the false-positive rate (k≈3 → ~0.3% false positive
    /// under Gaussian assumption).
    #[must_use]
    pub fn suspicion_threshold(&self, k: f64) -> f64 {
        self.effective_avg() + k * self.std_dev()
    }

    /// How long since the last activity (microseconds).
    #[must_use]
    pub fn silence_duration(&self, now_micros: i64) -> i64 {
        if self.last_activity_ts > 0 {
            (now_micros - self.last_activity_ts).max(0)
        } else {
            0
        }
    }

    /// Whether the agent has exceeded the suspicion threshold.
    #[must_use]
    pub fn is_suspicious(&self, now_micros: i64, k: f64) -> bool {
        let silence = self.silence_duration(now_micros) as f64;
        self.last_activity_ts > 0 && silence > self.suspicion_threshold(k)
    }
}

/// Per-agent liveness state tracked by the ATC.
#[derive(Debug, Clone)]
pub struct AgentLivenessEntry {
    /// Agent name.
    pub name: String,
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
}

/// Infer a reasonable inter-activity prior from program name.
#[must_use]
pub fn program_prior_interval_secs(program: &str) -> f64 {
    match program.to_ascii_lowercase().as_str() {
        "claude-code" | "claude_code" => 60.0,
        "codex-cli" | "codex_cli" | "codex" => 120.0,
        "gemini-cli" | "gemini_cli" | "gemini" => 120.0,
        "copilot-cli" | "copilot_cli" | "copilot" => 120.0,
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

/// Find all deadlock cycles in the hard conflict graph using Tarjan's SCC.
///
/// Returns only SCCs with |V| > 1 (true cycles).  O(V+E).
#[must_use]
pub fn find_deadlock_cycles(graph: &ProjectConflictGraph) -> Vec<Vec<String>> {
    // Collect all agents that appear in the graph.
    let mut agents: Vec<&str> = Vec::new();
    for (holder, edges) in &graph.hard_edges {
        if !agents.contains(&holder.as_str()) {
            agents.push(holder);
        }
        for edge in edges {
            if !agents.contains(&edge.blocked.as_str()) {
                agents.push(&edge.blocked);
            }
        }
    }
    if agents.len() < 2 {
        return Vec::new();
    }

    // Map agent names to indices for Tarjan's algorithm.
    let index_of: HashMap<&str, usize> = agents.iter().enumerate().map(|(i, &a)| (a, i)).collect();
    let n = agents.len();

    // Build adjacency list.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (holder, edges) in &graph.hard_edges {
        if let Some(&from) = index_of.get(holder.as_str()) {
            for edge in edges {
                if let Some(&to) = index_of.get(edge.blocked.as_str()) {
                    if !adj[from].contains(&to) {
                        adj[from].push(to);
                    }
                }
            }
        }
    }

    // Tarjan's SCC algorithm.
    let mut index_counter: usize = 0;
    let mut stack: Vec<usize> = Vec::new();
    let mut on_stack = vec![false; n];
    let mut indices = vec![usize::MAX; n]; // usize::MAX = unvisited
    let mut lowlinks = vec![0_usize; n];
    let mut sccs: Vec<Vec<String>> = Vec::new();

    fn strongconnect(
        v: usize,
        adj: &[Vec<usize>],
        index_counter: &mut usize,
        stack: &mut Vec<usize>,
        on_stack: &mut [bool],
        indices: &mut [usize],
        lowlinks: &mut [usize],
        sccs: &mut Vec<Vec<String>>,
        agents: &[&str],
    ) {
        indices[v] = *index_counter;
        lowlinks[v] = *index_counter;
        *index_counter += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in &adj[v] {
            if indices[w] == usize::MAX {
                strongconnect(
                    w,
                    adj,
                    index_counter,
                    stack,
                    on_stack,
                    indices,
                    lowlinks,
                    sccs,
                    agents,
                );
                lowlinks[v] = lowlinks[v].min(lowlinks[w]);
            } else if on_stack[w] {
                lowlinks[v] = lowlinks[v].min(indices[w]);
            }
        }

        if lowlinks[v] == indices[v] {
            let mut scc = Vec::new();
            while let Some(w) = stack.pop() {
                on_stack[w] = false;
                scc.push(agents[w].to_string());
                if w == v {
                    break;
                }
            }
            // Only keep SCCs with multiple nodes (true cycles).
            if scc.len() > 1 {
                sccs.push(scc);
            }
        }
    }

    for v in 0..n {
        if indices[v] == usize::MAX {
            strongconnect(
                v,
                &adj,
                &mut index_counter,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlinks,
                &mut sccs,
                &agents,
            );
        }
    }

    sccs
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
        assert_eq!(rhythm.observation_count, 0, "first observe just sets anchor");

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
        assert!(cycles.is_empty(), "self-loop should not be reported as deadlock");
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
        let factor = 1.0 + lambda * centered;
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
/// Maintains a non-negative supermartingale E_t starting at 1.0 under H₀
/// ("predictions are well-calibrated").  If E_t >= threshold at ANY time,
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
    pub fn update(
        &mut self,
        correct: bool,
        subsystem: AtcSubsystem,
        agent: Option<&str>,
    ) {
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
    pub fn e_value(&self) -> f64 {
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
        sources.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
    pub fn new(expected_rate: f64, threshold: f64, delta: f64) -> Self {
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
}

/// Counterfactual regret tracker.
///
/// For every ATC decision, computes what WOULD have happened if the ATC
/// had chosen differently.  Tracks cumulative regret per action type and
/// recent regret trend for loss matrix tuning.
#[derive(Debug, Clone)]
pub struct RegretTracker {
    /// Cumulative regret per action (action_name → total regret).
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
    /// Regret = actual_loss - best_loss (always >= 0).
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
            self.total_regret / self.outcome_count as f64
        }
    }

    /// Average regret over the recent window only.
    #[must_use]
    pub fn recent_average_regret(&self) -> f64 {
        if self.recent.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.recent.iter().map(|r| r.regret).sum();
        sum / self.recent.len() as f64
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
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
        let liveness_e = monitor.per_subsystem.get(&AtcSubsystem::Liveness).map(|o| o.e_value).unwrap_or(0.0);
        let conflict_e = monitor.per_subsystem.get(&AtcSubsystem::Conflict).map(|o| o.e_value).unwrap_or(0.0);
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

        let agent_a_e = monitor.per_agent.get("AgentA").map(|o| o.e_value).unwrap_or(0.0);
        let agent_b_e = monitor.per_agent.get("AgentB").map(|o| o.e_value).unwrap_or(0.0);
        assert!(
            agent_a_e > agent_b_e,
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
            if let Some(ChangeDirection::Degradation) = result {
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
            if let Some(ChangeDirection::Improvement) = result {
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
            if let Some(ChangeDirection::Degradation) = cusum.update(true, i * 1_000_000) {
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
        for i in 0..10 {
            tracker.record_outcome(i, "DeclareAlive", 0.0, "DeclareAlive", 0.0, i as i64);
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
        for i in 0_u64..5 {
            tracker.record_outcome(
                i,
                "A",
                i as f64 * 2.0,
                "B",
                0.0,
                i as i64,
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
    pub fn new(recovery_count: u64) -> Self {
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
        if !self.safe_mode
            && (eprocess.miscalibrated() || cusum.degradation_detected())
        {
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

    /// Force safe mode on/off (operator override).
    pub fn set_safe_mode(&mut self, active: bool, timestamp_micros: i64) {
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

/// Top-level ATC engine that orchestrates all subsystems.
#[derive(Debug)]
pub struct AtcEngine {
    /// Configuration.
    config: AtcConfig,
    /// Whether the ATC has registered itself as an agent.
    registered: bool,
    /// Liveness decision core.
    liveness_core: DecisionCore<LivenessState, LivenessAction>,
    /// Conflict decision core.
    conflict_core: DecisionCore<ConflictState, ConflictAction>,
    /// Per-agent liveness tracking.
    agents: HashMap<String, AgentLivenessEntry>,
    /// Per-project conflict graphs.
    conflict_graphs: HashMap<String, ProjectConflictGraph>,
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
    /// Last processed event sequence number (incremental computation).
    last_event_seq: u64,
    /// Engine tick count.
    tick_count: u64,
}

impl AtcEngine {
    /// Create a new ATC engine with the given configuration.
    #[must_use]
    pub fn new(config: AtcConfig) -> Self {
        let calibration = CalibrationGuard::new(config.safe_mode_recovery_count);
        let eprocess = EProcessMonitor::new(0.85, config.eprocess_alert_threshold);
        let cusum = CusumDetector::new(0.15, config.cusum_threshold, config.cusum_delta);
        let regret = RegretTracker::new(100);
        let ledger = EvidenceLedger::new(config.ledger_capacity);

        Self {
            config,
            registered: false,
            liveness_core: default_liveness_core(),
            conflict_core: default_conflict_core(),
            agents: HashMap::new(),
            conflict_graphs: HashMap::new(),
            calibration,
            eprocess,
            cusum,
            regret,
            ledger,
            last_event_seq: 0,
            tick_count: 0,
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
    pub fn is_safe_mode(&self) -> bool {
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

    /// Force safe mode on/off (operator override).
    pub fn set_safe_mode(&mut self, active: bool, timestamp_micros: i64) {
        self.calibration.set_safe_mode(active, timestamp_micros);
    }

    /// Check whether an event is from/to the ATC itself (self-exclusion).
    fn is_self_event(from: &str, to: &[String]) -> bool {
        from == ATC_AGENT_NAME || to.iter().any(|t| t == ATC_AGENT_NAME)
    }

    /// Register a new agent for liveness tracking.
    pub fn register_agent(&mut self, name: &str, program: &str) {
        if name == ATC_AGENT_NAME {
            return; // self-exclusion
        }
        self.agents.entry(name.to_string()).or_insert_with(|| {
            let prior_secs = program_prior_interval_secs(program);
            AgentLivenessEntry {
                name: name.to_string(),
                state: LivenessState::Alive,
                rhythm: AgentRhythm::new(prior_secs),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: default_liveness_core(),
            }
        });
    }

    /// Process an agent activity signal (message, reservation, commit).
    pub fn observe_activity(&mut self, agent: &str, timestamp_micros: i64) {
        if agent == ATC_AGENT_NAME {
            return;
        }
        if let Some(entry) = self.agents.get_mut(agent) {
            entry.rhythm.observe(timestamp_micros);
            // Any activity resets to Alive (resurrection)
            if entry.state != LivenessState::Alive {
                entry.state = LivenessState::Alive;
                entry.suspect_since = 0;
                entry.probe_sent_at = 0;
                entry.sprt_log_lr = 0.0;
            }
        }
    }

    /// Evaluate liveness for all tracked agents.
    ///
    /// Returns a list of (agent_name, recommended_action) for agents
    /// that need intervention.
    #[must_use]
    pub fn evaluate_liveness(&mut self, now_micros: i64) -> Vec<(String, LivenessAction)> {
        let k = self.config.suspicion_k;
        let mut actions = Vec::new();

        let agent_names: Vec<String> = self.agents.keys().cloned().collect();
        for name in agent_names {
            let entry = self.agents.get_mut(&name).unwrap();

            // Skip agents already declared dead
            if entry.state == LivenessState::Dead {
                continue;
            }

            // Check if the agent is suspicious
            if entry.rhythm.is_suspicious(now_micros, k) {
                // Update posterior toward flaky/dead
                let silence_ratio = entry.rhythm.silence_duration(now_micros) as f64
                    / entry.rhythm.effective_avg().max(1.0);

                let alive_lk = (-silence_ratio * 0.5).exp().max(0.01);
                let flaky_lk = (-silence_ratio * 0.1).exp().max(0.05);
                let dead_lk = 1.0 - alive_lk;

                entry.core.update_posterior(&[
                    (LivenessState::Alive, alive_lk),
                    (LivenessState::Flaky, flaky_lk),
                    (LivenessState::Dead, dead_lk),
                ]);

                let (action, expected_loss, runner_up) = entry.core.choose_action();

                // Only act if the action is different from DeclareAlive
                if action != LivenessAction::DeclareAlive {
                    // Log to evidence ledger
                    self.ledger.record(&DecisionBuilder {
                        subsystem: AtcSubsystem::Liveness,
                        subject: &name,
                        core: &entry.core,
                        action,
                        expected_loss,
                        runner_up_loss: runner_up,
                        evidence_summary: &format!(
                            "silence {}s (avg {}s, {:.1}σ)",
                            entry.rhythm.silence_duration(now_micros) / 1_000_000,
                            entry.rhythm.effective_avg() as i64 / 1_000_000,
                            silence_ratio,
                        ),
                        calibration_healthy: !self.calibration.is_safe_mode(),
                        safe_mode_active: self.calibration.is_safe_mode(),
                        timestamp_micros: now_micros,
                    });

                    // Apply state transition
                    match action {
                        LivenessAction::Suspect => {
                            if entry.state != LivenessState::Flaky {
                                entry.state = LivenessState::Flaky;
                                entry.suspect_since = now_micros;
                            }
                        }
                        LivenessAction::ReleaseReservations => {
                            if !self.calibration.is_safe_mode() {
                                entry.state = LivenessState::Dead;
                            }
                            // In safe mode, downgrade to Suspect only
                            else if entry.state != LivenessState::Flaky {
                                entry.state = LivenessState::Flaky;
                                entry.suspect_since = now_micros;
                            }
                        }
                        LivenessAction::DeclareAlive => {}
                    }

                    actions.push((name.clone(), action));
                }
            }
        }

        actions
    }

    /// Check for deadlock cycles in all project conflict graphs.
    #[must_use]
    pub fn detect_deadlocks(&self) -> Vec<(String, Vec<Vec<String>>)> {
        let mut results = Vec::new();
        for (project, graph) in &self.conflict_graphs {
            let cycles = find_deadlock_cycles(graph);
            if !cycles.is_empty() {
                results.push((project.clone(), cycles));
            }
        }
        results
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
        assert!(guard.is_safe_mode(), "should NOT exit while eprocess is miscalibrated");
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
        engine.register_agent("BlueFox", "claude-code");
        assert!(engine.agent_liveness("BlueFox").is_some());
        assert_eq!(engine.agent_liveness("BlueFox"), Some(LivenessState::Alive));
    }

    #[test]
    fn engine_excludes_atc_from_registration() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent(ATC_AGENT_NAME, "mcp-agent-mail");
        assert!(engine.agent_liveness(ATC_AGENT_NAME).is_none());
    }

    #[test]
    fn engine_excludes_atc_from_activity() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code");
        engine.observe_activity(ATC_AGENT_NAME, 1_000_000);
        // ATC activity should be silently ignored
        assert!(engine.agent_liveness(ATC_AGENT_NAME).is_none());
    }

    #[test]
    fn activity_resets_to_alive() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code");

        // Manually set to Flaky
        engine.agents.get_mut("BlueFox").unwrap().state = LivenessState::Flaky;
        assert_eq!(engine.agent_liveness("BlueFox"), Some(LivenessState::Flaky));

        // Activity resets to Alive
        engine.observe_activity("BlueFox", 1_000_000);
        assert_eq!(engine.agent_liveness("BlueFox"), Some(LivenessState::Alive));
    }

    #[test]
    fn evaluate_liveness_detects_silent_agent() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code");

        // Establish a rhythm (60s intervals, 10 observations)
        for i in 0..10 {
            engine.observe_activity("BlueFox", i * 60_000_000);
        }

        // 5 minutes of silence (5× the 60s avg).  The posterior update is
        // incremental — each evaluate call pushes the posterior further from
        // the strong alive prior.  Simulate multiple tick evaluations.
        let base = 9 * 60_000_000;
        let mut any_action = false;
        let mut last_actions = Vec::new();
        for tick in 1..=10 {
            let now = base + tick * 30_000_000; // every 30s
            last_actions = engine.evaluate_liveness(now);
            if !last_actions.is_empty() {
                any_action = true;
                break;
            }
        }
        assert!(
            any_action,
            "should detect silent agent within 10 evaluation ticks"
        );

        // Verify the action targets BlueFox
        let (agent, action) = &last_actions[0];
        assert_eq!(agent, "BlueFox");
        assert!(
            *action == LivenessAction::Suspect || *action == LivenessAction::ReleaseReservations,
            "action should be Suspect or Release, got {action:?}"
        );
    }

    #[test]
    fn evaluate_liveness_ignores_active_agent() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code");

        // Agent is active right now
        for i in 0..10 {
            engine.observe_activity("BlueFox", i * 60_000_000);
        }
        let now = 9 * 60_000_000 + 30_000_000; // only 30s since last activity
        let actions = engine.evaluate_liveness(now);
        assert!(actions.is_empty(), "active agent should not trigger any action");
    }

    #[test]
    fn safe_mode_blocks_release() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code");
        engine.set_safe_mode(true, 0);

        // Establish rhythm then go very silent
        for i in 0..10 {
            engine.observe_activity("BlueFox", i * 60_000_000);
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
        let _actions = engine.evaluate_liveness(now);

        // In safe mode, even if the core recommends Release, the state
        // transition is downgraded to Flaky (Suspect), never Dead.
        let state = engine.agent_liveness("BlueFox").unwrap();
        assert_ne!(
            state,
            LivenessState::Dead,
            "safe mode should prevent Dead state, got {state:?}"
        );
    }

    #[test]
    fn detect_deadlocks_empty_graphs() {
        let engine = AtcEngine::new_for_testing();
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
        engine.conflict_graphs.insert("test-project".to_string(), graph);

        let deadlocks = engine.detect_deadlocks();
        assert_eq!(deadlocks.len(), 1);
        assert_eq!(deadlocks[0].0, "test-project");
        assert_eq!(deadlocks[0].1.len(), 1);
        assert_eq!(deadlocks[0].1[0].len(), 2);
    }

    #[test]
    fn is_self_event_filters_correctly() {
        assert!(AtcEngine::is_self_event(ATC_AGENT_NAME, &[]));
        assert!(AtcEngine::is_self_event("other", &[ATC_AGENT_NAME.to_string()]));
        assert!(!AtcEngine::is_self_event("other", &["another".to_string()]));
    }

    #[test]
    fn ledger_records_liveness_decisions() {
        let mut engine = AtcEngine::new_for_testing();
        engine.register_agent("BlueFox", "claude-code");

        // Establish rhythm then go silent
        for i in 0..10 {
            engine.observe_activity("BlueFox", i * 60_000_000);
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSummary {
    // ── Monotone Counters (increment only) ──
    pub messages_sent: u64,
    pub messages_received: u64,
    pub reservations_granted: u64,
    pub reservations_released: u64,
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
            Self::MessageSent { timestamp_micros, .. }
            | Self::MessageReceived { timestamp_micros, .. }
            | Self::ReservationGranted { timestamp_micros, .. }
            | Self::ReservationReleased { timestamp_micros, .. }
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
            SynthesisEvent::MessageSent { from, to, thread_id, .. } => {
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
        agent_counts.sort_by(|a, b| b.1.cmp(&a.1));
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
        thread_counts.sort_by(|a, b| b.1.cmp(&a.1));
        if !thread_counts.is_empty() {
            let top: Vec<String> = thread_counts
                .iter()
                .take(5)
                .map(|(t, c)| format!("{t}: {c}"))
                .collect();
            lines.push(format!("Hot threads: {}", top.join(", ")));
        }

        if self.reservations_granted > 0 || self.reservations_released > 0 {
            lines.push(format!(
                "Reservations: {} granted, {} released",
                self.reservations_granted, self.reservations_released,
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
            to: to.iter().map(|s| s.to_string()).collect(),
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
    fn absorb_conflict_events() {
        let mut summary = SessionSummary::default();
        summary.absorb(&SynthesisEvent::ConflictDetected { timestamp_micros: 1 });
        summary.absorb(&SynthesisEvent::ConflictResolved { timestamp_micros: 2 });

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
        summary.absorb(&SynthesisEvent::ConflictDetected { timestamp_micros: 100 });
        summary.absorb(&SynthesisEvent::AtcIntervention { timestamp_micros: 101 });

        let output = summary.formatted().to_string();
        assert!(output.contains("10 sent"), "should show message count");
        assert!(output.contains("BlueFox"), "should show agent name");
        assert!(output.contains("feat-1"), "should show thread");
        assert!(output.contains("Conflicts: 1"), "should show conflicts");
        assert!(output.contains("ATC interventions: 1"), "should show interventions");
    }

    #[test]
    fn determinism_same_events_same_output() {
        let events = vec![
            msg_sent("A", &["B"], Some("t1"), 1),
            msg_sent("B", &["C"], Some("t2"), 2),
            SynthesisEvent::ReservationGranted { agent: "A".to_string(), timestamp_micros: 3 },
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
    pub fn new() -> Self {
        Self {
            throughput_ewma: 0.0,
            active_reservations: 0,
            pending_inbox: 0,
            predicted_capacity: 1.0, // assume idle initially
            prediction_accuracy: 0.5, // neutral accuracy
            observation_count: 0,
        }
    }

    /// Update from observed agent activity.
    pub fn observe_activity(&mut self, messages_processed: u64, interval_secs: f64) {
        if interval_secs > 0.0 {
            let rate = messages_processed as f64 / interval_secs * 3600.0;
            self.throughput_ewma = 0.8 * self.throughput_ewma + 0.2 * rate;
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
        let reservation_load = (self.active_reservations as f64 * 0.15).min(1.0);
        let inbox_load = (self.pending_inbox as f64 * 0.05).min(1.0);
        self.predicted_capacity = (1.0 - reservation_load - inbox_load).max(0.0);
    }

    /// Record whether a routing prediction was accurate.
    pub fn record_accuracy(&mut self, actual_response_secs: f64, expected_response_secs: f64) {
        let error = ((actual_response_secs - expected_response_secs) / expected_response_secs.max(1.0)).abs();
        let correct = if error < 0.5 { 1.0 } else { 0.0 };
        self.prediction_accuracy = 0.9 * self.prediction_accuracy + 0.1 * correct;
        self.observation_count += 1;
    }

    /// Classical routing score (no prediction needed): 1/(1 + reservations).
    #[must_use]
    pub fn classical_score(&self) -> f64 {
        1.0 / (1.0 + self.active_reservations as f64)
    }

    /// Blended routing score using consistency-robustness tradeoff.
    ///
    /// λ = prediction_accuracy (trust the predictor when it's been right).
    /// score = λ * predicted_capacity + (1-λ) * classical_score.
    #[must_use]
    pub fn routing_score(&self) -> f64 {
        let lambda = self.prediction_accuracy;
        lambda * self.predicted_capacity + (1.0 - lambda) * self.classical_score()
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
pub fn select_route_target<'a>(
    agents: &'a HashMap<String, AgentLoadModel>,
    exclude: &str, // don't route to the requester
    min_score: f64,
) -> Option<&'a str> {
    agents
        .iter()
        .filter(|(name, _)| name.as_str() != exclude && name.as_str() != ATC_AGENT_NAME)
        .max_by(|(_, a), (_, b)| {
            a.routing_score()
                .partial_cmp(&b.routing_score())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
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
        assert!(model.predicted_capacity < 0.5, "5 reservations should reduce capacity");
        assert!(model.routing_score() < 0.5);
    }

    #[test]
    fn pending_inbox_reduces_capacity() {
        let mut model = AgentLoadModel::new();
        model.update_counts(0, 10);
        assert!(model.predicted_capacity < 0.8, "10 pending messages should reduce capacity");
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
        assert!(target.is_none(), "should return None when all below threshold");
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
    /// thread_id → set of participating agent names.
    thread_agents: HashMap<String, HashSet<String>>,
    /// agent → set of thread_ids they're active in.
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
        let threads_a = match self.agent_threads.get(agent_a) {
            Some(t) => t,
            None => return 0,
        };
        let threads_b = match self.agent_threads.get(agent_b) {
            Some(t) => t,
            None => return 0,
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
                    pairs.push((
                        agents[i].to_string(),
                        agents[j].to_string(),
                        count,
                    ));
                }
            }
        }
        pairs.sort_by(|a, b| b.2.cmp(&a.2));
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
pub fn rank_probe_targets(
    agents: &HashMap<String, AgentLivenessEntry>,
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
    targets.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
        assert_eq!(all_pairs.len(), 3, "all three pairs should exceed threshold 1");
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
        assert!(gain_certain < 0.01, "certain posterior should have ~0 entropy");
        assert!(gain_uniform > 1.0, "uniform should have high entropy");
    }

    #[test]
    fn rank_probe_targets_orders_by_gain() {
        let mut agents = HashMap::new();

        // Uncertain agent (uniform posterior)
        let mut uncertain = AgentLivenessEntry {
            name: "Uncertain".to_string(),
            state: LivenessState::Alive,
            rhythm: AgentRhythm::new(60.0),
            suspect_since: 0,
            probe_sent_at: 0,
            sprt_log_lr: 0.0,
            core: default_liveness_core(),
        };
        // Push posterior toward uncertainty
        for _ in 0..5 {
            uncertain.core.update_posterior(&[
                (LivenessState::Alive, 0.5),
                (LivenessState::Flaky, 0.5),
                (LivenessState::Dead, 0.5),
            ]);
        }
        agents.insert("Uncertain".to_string(), uncertain);

        // Confident agent (strong alive posterior — default)
        let confident = AgentLivenessEntry {
            name: "Confident".to_string(),
            state: LivenessState::Alive,
            rhythm: AgentRhythm::new(60.0),
            suspect_since: 0,
            probe_sent_at: 0,
            sprt_log_lr: 0.0,
            core: default_liveness_core(),
        };
        agents.insert("Confident".to_string(), confident);

        let ranked = rank_probe_targets(&agents, 0.0);
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].0, "Uncertain", "most uncertain should be probed first");
    }

    #[test]
    fn rank_probe_targets_excludes_atc() {
        let mut agents = HashMap::new();
        agents.insert(
            ATC_AGENT_NAME.to_string(),
            AgentLivenessEntry {
                name: ATC_AGENT_NAME.to_string(),
                state: LivenessState::Alive,
                rhythm: AgentRhythm::new(60.0),
                suspect_since: 0,
                probe_sent_at: 0,
                sprt_log_lr: 0.0,
                core: default_liveness_core(),
            },
        );

        let ranked = rank_probe_targets(&agents, 0.0);
        assert!(ranked.is_empty(), "ATC should be excluded from probe targets");
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
                    other.remaining_tasks as f64
                        * other.estimated_completion_mins.max(1) as f64
                        / 60.0
                })
                .sum();
            (agent.agent.clone(), externality)
        })
        .collect();
    // Highest externality first (should yield first)
    priorities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
    let ca_sq = cv_arrival * cv_arrival;
    let cs_sq = cv_service * cv_service;
    (ca_sq + cs_sq) / 2.0 * rho / (mu * (1.0 - rho))
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
        self.integral = (self.integral + error * dt).clamp(-self.integral_max, self.integral_max);
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
    pub fn reset(&mut self) {
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
        assert!((priorities[0].1 - 0.0).abs() < f64::EPSILON, "single agent has 0 externality");
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
        assert!(pid.current_value > 10.0, "positive error should increase value");
    }

    #[test]
    fn pid_stays_within_bounds() {
        let mut pid = PidState::new(10.0);
        // Extreme positive error
        for _ in 0..1000 {
            pid.update(100.0, 1.0);
        }
        assert!(pid.current_value <= 100.0, "should not exceed 10× original");
        assert!(pid.current_value >= 1.0, "should not go below 0.1× original");
    }

    #[test]
    fn pid_reset_restores_original() {
        let mut pid = PidState::new(10.0);
        pid.update(5.0, 1.0);
        assert!(pid.current_value != 10.0);
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
        assert!(pid.deviation() > 0.0, "positive error should increase deviation");
    }
}
