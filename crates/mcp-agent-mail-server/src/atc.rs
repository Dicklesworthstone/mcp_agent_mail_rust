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

use std::collections::HashMap;
use std::collections::VecDeque;
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

/// E-process martingale monitor for anytime-valid miscalibration detection.
///
/// Maintains a non-negative supermartingale E_t starting at 1.0 under H₀
/// ("predictions are well-calibrated").  If E_t >= threshold at ANY time,
/// we have statistically valid evidence of miscalibration — no multiple-
/// testing correction needed.
///
/// Also maintains per-subsystem and per-agent e-processes to PINPOINT
/// which component is drifting.
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
        // factor is guaranteed non-negative by the lambda bounds
        self.e_value = (self.e_value * factor).max(1e-30);
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
        // Force a degradation
        for i in 0..100 {
            cusum.update(true, i * 1_000_000); // 100% errors
        }
        assert!(cusum.degradation_detected());
        assert!(!cusum.regime_changes.is_empty());

        // After reset, s_pos should be 0
        assert!(
            cusum.s_pos.abs() < f64::EPSILON,
            "CUSUM should reset after detection"
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
