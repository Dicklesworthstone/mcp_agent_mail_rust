//! Synthetic scenario corpus and deterministic replay harness for ATC.
//!
//! This module defines a comprehensive corpus of ATC-relevant scenarios that
//! can be replayed deterministically without live LLMs or nondeterministic
//! external services.  Every scenario has:
//!
//! - A stable `ScenarioId` for cross-run comparison
//! - An ordered timeline of synthetic events with microsecond timestamps
//! - Expected outcomes at multiple layers (liveness state, decisions, ledger,
//!   calibration, conflicts, safe mode)
//! - A deterministic seed for reproducibility
//!
//! The same corpus feeds unit, replay, E2E, and performance harnesses.
//!
//! ## Adding new scenarios
//!
//! 1. Define a new `ScenarioId` variant.
//! 2. Implement a builder function that returns `ScenarioManifest`.
//! 3. Add it to `CORPUS` via `all_scenarios()`.
//! 4. Include expected checkpoints at each timeline milestone.
//! 5. Run `cargo test atc_replay` to verify determinism.
//!
//! Do NOT introduce randomness, wall-clock reads, or external I/O.  All
//! timestamps are synthetic microsecond values.

use crate::atc::{AtcConfig, AtcEngine, AtcSubsystem, LivenessAction, LivenessState};

// ──────────────────────────────────────────────────────────────────────
// Scenario identity
// ──────────────────────────────────────────────────────────────────────

/// Stable scenario identifiers.  Each variant maps to exactly one
/// canonical scenario manifest.  IDs must never be reused or renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScenarioId {
    /// Healthy agent sending regular activity signals.
    HealthyLiveness,
    /// Agent goes completely silent after initial activity.
    SilentAgent,
    /// Agent has flaky connectivity (intermittent activity).
    FlakyAgent,
    /// False probe: agent is actually alive but threshold triggers.
    FalseProbe,
    /// Advisory succeeds: agent resumes after advisory.
    AdvisorySuccess,
    /// Advisory fails: agent stays silent after advisory.
    AdvisoryFailure,
    /// Conflict detection: two agents with overlapping reservations.
    ConflictResolution,
    /// Deliberate no-op: ATC decides not to intervene.
    DeliberateNoOp,
    /// Safe-to-ignore noise: low-severity transient signal.
    SafeToIgnoreNoise,
    /// Suppressed unsafe action: safe mode prevents aggressive action.
    SuppressedUnsafeAction,
    /// Drift onset: gradual degradation of calibration.
    DriftOnset,
    /// Rollback: calibration recovers after degradation.
    CalibrationRollback,
    /// Restart recovery: engine reinitializes mid-session.
    RestartRecovery,
    /// Degraded-learning safe mode: system enters and stays in safe mode.
    DegradedLearningSafeMode,
    /// Spoofed-liveness: rapid artificial activity signals.
    SpoofedLiveness,
    /// Duplicate/replayed event stream: same events delivered twice.
    DuplicateEvents,
    /// Coordinated reservation churn: agents rapidly acquiring/releasing.
    CoordinatedReservationChurn,
    /// Overlapping ATC interventions: multiple agents need action simultaneously.
    OverlappingInterventions,
    /// Concurrent operator control changes: config changes during evaluation.
    ConcurrentOperatorChanges,
    /// Natural recovery during attribution window.
    NaturalRecoveryAttribution,
    /// Deadlock cycle detection: A blocks B blocks A.
    DeadlockCycle,
    /// Multi-agent conflict with mixed severity.
    MultiAgentConflict,
    /// Posterior convergence under consistent evidence.
    PosteriorConvergence,
    /// E-process miscalibration detection.
    EProcessMiscalibration,
    /// CUSUM regime change detection.
    CusumRegimeChange,
}

impl ScenarioId {
    /// Stable string representation for manifest comparison.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HealthyLiveness => "healthy_liveness",
            Self::SilentAgent => "silent_agent",
            Self::FlakyAgent => "flaky_agent",
            Self::FalseProbe => "false_probe",
            Self::AdvisorySuccess => "advisory_success",
            Self::AdvisoryFailure => "advisory_failure",
            Self::ConflictResolution => "conflict_resolution",
            Self::DeliberateNoOp => "deliberate_no_op",
            Self::SafeToIgnoreNoise => "safe_to_ignore_noise",
            Self::SuppressedUnsafeAction => "suppressed_unsafe_action",
            Self::DriftOnset => "drift_onset",
            Self::CalibrationRollback => "calibration_rollback",
            Self::RestartRecovery => "restart_recovery",
            Self::DegradedLearningSafeMode => "degraded_learning_safe_mode",
            Self::SpoofedLiveness => "spoofed_liveness",
            Self::DuplicateEvents => "duplicate_events",
            Self::CoordinatedReservationChurn => "coordinated_reservation_churn",
            Self::OverlappingInterventions => "overlapping_interventions",
            Self::ConcurrentOperatorChanges => "concurrent_operator_changes",
            Self::NaturalRecoveryAttribution => "natural_recovery_attribution",
            Self::DeadlockCycle => "deadlock_cycle",
            Self::MultiAgentConflict => "multi_agent_conflict",
            Self::PosteriorConvergence => "posterior_convergence",
            Self::EProcessMiscalibration => "eprocess_miscalibration",
            Self::CusumRegimeChange => "cusum_regime_change",
        }
    }

    /// All scenario IDs in canonical order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::HealthyLiveness,
            Self::SilentAgent,
            Self::FlakyAgent,
            Self::FalseProbe,
            Self::AdvisorySuccess,
            Self::AdvisoryFailure,
            Self::ConflictResolution,
            Self::DeliberateNoOp,
            Self::SafeToIgnoreNoise,
            Self::SuppressedUnsafeAction,
            Self::DriftOnset,
            Self::CalibrationRollback,
            Self::RestartRecovery,
            Self::DegradedLearningSafeMode,
            Self::SpoofedLiveness,
            Self::DuplicateEvents,
            Self::CoordinatedReservationChurn,
            Self::OverlappingInterventions,
            Self::ConcurrentOperatorChanges,
            Self::NaturalRecoveryAttribution,
            Self::DeadlockCycle,
            Self::MultiAgentConflict,
            Self::PosteriorConvergence,
            Self::EProcessMiscalibration,
            Self::CusumRegimeChange,
        ]
    }
}

impl std::fmt::Display for ScenarioId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ──────────────────────────────────────────────────────────────────────
// Scenario events (the input timeline)
// ──────────────────────────────────────────────────────────────────────

/// A single synthetic event in a scenario timeline.
#[derive(Debug, Clone)]
pub enum ScenarioEvent {
    /// Register an agent with the ATC engine.
    RegisterAgent {
        name: String,
        program: String,
        project_key: Option<String>,
    },
    /// Agent activity signal at a specific timestamp.
    AgentActivity {
        agent: String,
        project_key: Option<String>,
        timestamp_micros: i64,
    },
    /// Record a reservation conflict between agents.
    ReservationConflict {
        requester: String,
        project: String,
        /// (holder, requested_path, holder_path_pattern)
        conflicts: Vec<(String, String, String)>,
        timestamp_micros: i64,
    },
    /// Grant a reservation to an agent.
    ReservationGranted {
        agent: String,
        paths: Vec<String>,
        exclusive: bool,
        project: String,
        timestamp_micros: i64,
    },
    /// Release a reservation held by an agent.
    ReservationReleased {
        agent: String,
        paths: Vec<String>,
        project: String,
        timestamp_micros: i64,
    },
    /// Evaluate liveness at a given timestamp (trigger ATC tick).
    EvaluateLiveness { timestamp_micros: i64 },
    /// Record a calibration prediction outcome.
    CalibrationOutcome {
        correct: bool,
        subsystem: AtcSubsystem,
        agent: Option<String>,
    },
    /// Force safe mode on or off (operator override).
    SetSafeMode { active: bool, timestamp_micros: i64 },
    /// Advance virtual time without any event (gap marker).
    Tick { timestamp_micros: i64 },
}

// ──────────────────────────────────────────────────────────────────────
// Expected outcomes (the output truth set)
// ──────────────────────────────────────────────────────────────────────

/// Expected state of an agent at a checkpoint.
#[derive(Debug, Clone)]
pub struct ExpectedAgentState {
    pub name: String,
    pub liveness: LivenessState,
    /// If set, the posterior probability for Alive must be within this range.
    pub alive_posterior_range: Option<(f64, f64)>,
}

/// Expected state at a timeline checkpoint.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Human-readable label for this checkpoint.
    pub label: &'static str,
    /// Virtual timestamp (microseconds).
    pub timestamp_micros: i64,
    /// Expected agent states after processing events up to this point.
    pub expected_agents: Vec<ExpectedAgentState>,
    /// Expected decision count in the evidence ledger.
    pub expected_decision_count: Option<usize>,
    /// Whether safe mode should be active.
    pub expected_safe_mode: Option<bool>,
    /// Expected number of deadlock cycles.
    pub expected_deadlock_cycles: Option<usize>,
    /// Whether the e-process should indicate miscalibration.
    pub expected_miscalibrated: Option<bool>,
    /// Minimum number of ledger entries.
    pub min_ledger_entries: Option<usize>,
    /// Whether a specific action should have been taken for an agent.
    pub expected_actions: Vec<(String, LivenessAction)>,
}

// ──────────────────────────────────────────────────────────────────────
// Scenario manifest
// ──────────────────────────────────────────────────────────────────────

/// A complete scenario definition: events + expected outcomes.
#[derive(Debug, Clone)]
pub struct ScenarioManifest {
    /// Stable scenario identifier.
    pub id: ScenarioId,
    /// Human-readable description.
    pub description: &'static str,
    /// Deterministic seed for any pseudo-random operations.
    pub seed: u64,
    /// ATC configuration overrides for this scenario.
    pub config: AtcConfig,
    /// Ordered timeline of events.
    pub events: Vec<ScenarioEvent>,
    /// Checkpoints with expected outcomes (ordered by timestamp).
    pub checkpoints: Vec<Checkpoint>,
}

// ──────────────────────────────────────────────────────────────────────
// Replay harness
// ──────────────────────────────────────────────────────────────────────

/// Result of replaying a single checkpoint.
#[derive(Debug)]
pub struct CheckpointResult {
    pub label: &'static str,
    pub passed: bool,
    pub failures: Vec<String>,
}

/// Result of replaying an entire scenario.
#[derive(Debug)]
pub struct ReplayResult {
    pub scenario_id: ScenarioId,
    pub checkpoints: Vec<CheckpointResult>,
    pub all_passed: bool,
    /// Final engine state summary.
    pub final_decision_count: usize,
    pub final_safe_mode: bool,
}

/// Replay a scenario manifest deterministically.
///
/// This function drives an `AtcEngine` through the scenario timeline,
/// verifying expected outcomes at each checkpoint.  No wall-clock time,
/// no I/O, no randomness.
#[must_use]
pub fn replay_scenario(manifest: &ScenarioManifest) -> ReplayResult {
    let mut engine = AtcEngine::new(manifest.config.clone());
    let mut event_idx = 0;
    let mut checkpoint_results = Vec::with_capacity(manifest.checkpoints.len());

    for checkpoint in &manifest.checkpoints {
        // Process events up to this checkpoint's timestamp.
        while event_idx < manifest.events.len() {
            let event_ts = event_timestamp(&manifest.events[event_idx]);
            if event_ts > checkpoint.timestamp_micros {
                break;
            }
            apply_event(&mut engine, &manifest.events[event_idx]);
            event_idx += 1;
        }

        // Verify checkpoint assertions.
        let result = verify_checkpoint(&mut engine, checkpoint);
        checkpoint_results.push(result);
    }

    // Process any remaining events after the last checkpoint.
    while event_idx < manifest.events.len() {
        apply_event(&mut engine, &manifest.events[event_idx]);
        event_idx += 1;
    }

    let all_passed = checkpoint_results.iter().all(|r| r.passed);
    ReplayResult {
        scenario_id: manifest.id,
        checkpoints: checkpoint_results,
        all_passed,
        final_decision_count: engine.ledger().len(),
        final_safe_mode: engine.is_safe_mode(),
    }
}

/// Extract the timestamp from an event (for ordering).
fn event_timestamp(event: &ScenarioEvent) -> i64 {
    match event {
        ScenarioEvent::RegisterAgent { .. } => 0,
        ScenarioEvent::AgentActivity {
            timestamp_micros, ..
        } => *timestamp_micros,
        ScenarioEvent::ReservationConflict {
            timestamp_micros, ..
        } => *timestamp_micros,
        ScenarioEvent::ReservationGranted {
            timestamp_micros, ..
        } => *timestamp_micros,
        ScenarioEvent::ReservationReleased {
            timestamp_micros, ..
        } => *timestamp_micros,
        ScenarioEvent::EvaluateLiveness {
            timestamp_micros, ..
        } => *timestamp_micros,
        ScenarioEvent::CalibrationOutcome { .. } => 0,
        ScenarioEvent::SetSafeMode {
            timestamp_micros, ..
        } => *timestamp_micros,
        ScenarioEvent::Tick {
            timestamp_micros, ..
        } => *timestamp_micros,
    }
}

/// Apply a single event to the engine.
fn apply_event(engine: &mut AtcEngine, event: &ScenarioEvent) {
    match event {
        ScenarioEvent::RegisterAgent {
            name,
            program,
            project_key,
        } => {
            engine.register_agent(name, program, project_key.as_deref());
        }
        ScenarioEvent::AgentActivity {
            agent,
            project_key,
            timestamp_micros,
        } => {
            engine.observe_activity(agent, project_key.as_deref(), *timestamp_micros);
        }
        ScenarioEvent::ReservationConflict {
            requester,
            project,
            conflicts,
            timestamp_micros,
        } => {
            engine.note_reservation_conflicts(requester, project, conflicts, *timestamp_micros);
        }
        ScenarioEvent::ReservationGranted {
            agent,
            paths,
            exclusive,
            project,
            timestamp_micros,
        } => {
            engine.note_reservation_granted(agent, paths, *exclusive, project, *timestamp_micros);
        }
        ScenarioEvent::ReservationReleased {
            agent,
            paths,
            project,
            timestamp_micros,
        } => {
            engine.note_reservation_released(agent, paths, project, *timestamp_micros);
        }
        ScenarioEvent::EvaluateLiveness { timestamp_micros } => {
            let _ = engine.evaluate_liveness(*timestamp_micros);
        }
        ScenarioEvent::CalibrationOutcome {
            correct,
            subsystem,
            agent,
        } => {
            engine
                .eprocess_mut()
                .update(*correct, *subsystem, agent.as_deref());
            let _ = engine.cusum_mut().update(!correct, 0);
        }
        ScenarioEvent::SetSafeMode {
            active,
            timestamp_micros,
        } => {
            engine.set_safe_mode(*active, *timestamp_micros);
        }
        ScenarioEvent::Tick { .. } => {
            // Pure time-advancement marker — no engine operation needed.
        }
    }
}

/// Verify a checkpoint against the current engine state.
fn verify_checkpoint(engine: &mut AtcEngine, checkpoint: &Checkpoint) -> CheckpointResult {
    let mut failures = Vec::new();

    for expected in &checkpoint.expected_agents {
        match engine.agent_liveness(&expected.name) {
            Some(actual_state) => {
                if actual_state != expected.liveness {
                    failures.push(format!(
                        "agent '{}': expected liveness {:?}, got {:?}",
                        expected.name, expected.liveness, actual_state
                    ));
                }
            }
            None => {
                failures.push(format!(
                    "agent '{}': not found in engine (expected {:?})",
                    expected.name, expected.liveness
                ));
            }
        }

        if let Some((lo, hi)) = expected.alive_posterior_range {
            if let Some(prob) = engine.agent_alive_posterior(&expected.name) {
                if prob < lo || prob > hi {
                    failures.push(format!(
                        "agent '{}': alive posterior {:.4} not in [{:.4}, {:.4}]",
                        expected.name, prob, lo, hi
                    ));
                }
            }
        }
    }

    if let Some(expected_count) = checkpoint.expected_decision_count {
        let actual = engine.ledger().len();
        if actual != expected_count {
            failures.push(format!(
                "decision count: expected {expected_count}, got {actual}"
            ));
        }
    }

    if let Some(expected_safe) = checkpoint.expected_safe_mode {
        let actual = engine.is_safe_mode();
        if actual != expected_safe {
            failures.push(format!("safe mode: expected {expected_safe}, got {actual}"));
        }
    }

    if let Some(expected_cycles) = checkpoint.expected_deadlock_cycles {
        let deadlocks = engine.detect_deadlocks();
        let total_cycles: usize = deadlocks.iter().map(|(_, cycles)| cycles.len()).sum();
        if total_cycles != expected_cycles {
            failures.push(format!(
                "deadlock cycles: expected {expected_cycles}, got {total_cycles}"
            ));
        }
    }

    if let Some(expected_miscal) = checkpoint.expected_miscalibrated {
        let actual = engine.eprocess().miscalibrated();
        if actual != expected_miscal {
            failures.push(format!(
                "miscalibrated: expected {expected_miscal}, got {actual}"
            ));
        }
    }

    if let Some(min_entries) = checkpoint.min_ledger_entries {
        let actual = engine.ledger().len();
        if actual < min_entries {
            failures.push(format!(
                "ledger entries: expected >= {min_entries}, got {actual}"
            ));
        }
    }

    let passed = failures.is_empty();
    CheckpointResult {
        label: checkpoint.label,
        passed,
        failures,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Scenario corpus — builder functions
// ──────────────────────────────────────────────────────────────────────

/// Microseconds per second.
const US_PER_SEC: i64 = 1_000_000;

/// Build the complete corpus of all canonical scenarios.
#[must_use]
pub fn all_scenarios() -> Vec<ScenarioManifest> {
    ScenarioId::all()
        .iter()
        .map(|id| build_scenario(*id))
        .collect()
}

/// Build a single scenario manifest by ID.
#[must_use]
pub fn build_scenario(id: ScenarioId) -> ScenarioManifest {
    match id {
        ScenarioId::HealthyLiveness => scenario_healthy_liveness(),
        ScenarioId::SilentAgent => scenario_silent_agent(),
        ScenarioId::FlakyAgent => scenario_flaky_agent(),
        ScenarioId::FalseProbe => scenario_false_probe(),
        ScenarioId::AdvisorySuccess => scenario_advisory_success(),
        ScenarioId::AdvisoryFailure => scenario_advisory_failure(),
        ScenarioId::ConflictResolution => scenario_conflict_resolution(),
        ScenarioId::DeliberateNoOp => scenario_deliberate_no_op(),
        ScenarioId::SafeToIgnoreNoise => scenario_safe_to_ignore_noise(),
        ScenarioId::SuppressedUnsafeAction => scenario_suppressed_unsafe_action(),
        ScenarioId::DriftOnset => scenario_drift_onset(),
        ScenarioId::CalibrationRollback => scenario_calibration_rollback(),
        ScenarioId::RestartRecovery => scenario_restart_recovery(),
        ScenarioId::DegradedLearningSafeMode => scenario_degraded_learning_safe_mode(),
        ScenarioId::SpoofedLiveness => scenario_spoofed_liveness(),
        ScenarioId::DuplicateEvents => scenario_duplicate_events(),
        ScenarioId::CoordinatedReservationChurn => scenario_coordinated_reservation_churn(),
        ScenarioId::OverlappingInterventions => scenario_overlapping_interventions(),
        ScenarioId::ConcurrentOperatorChanges => scenario_concurrent_operator_changes(),
        ScenarioId::NaturalRecoveryAttribution => scenario_natural_recovery_attribution(),
        ScenarioId::DeadlockCycle => scenario_deadlock_cycle(),
        ScenarioId::MultiAgentConflict => scenario_multi_agent_conflict(),
        ScenarioId::PosteriorConvergence => scenario_posterior_convergence(),
        ScenarioId::EProcessMiscalibration => scenario_eprocess_miscalibration(),
        ScenarioId::CusumRegimeChange => scenario_cusum_regime_change(),
    }
}

fn test_config() -> AtcConfig {
    AtcConfig {
        enabled: true,
        ..AtcConfig::default()
    }
}

// ── Scenario 1: Healthy Liveness ────────────────────────────────────

fn scenario_healthy_liveness() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Alpha".into(),
        program: "claude-code".into(),
        project_key: Some("/tmp/project-alpha".into()),
    }];

    // Regular 60s activity for 10 minutes.
    for i in 0..10 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Alpha".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Evaluate liveness at the end — should still be alive.
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 11 * 60 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::HealthyLiveness,
        description: "Agent sends regular activity signals; ATC declares alive throughout",
        seed: 1,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_10_minutes_regular_activity",
            timestamp_micros: 11 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Alpha".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: Some((0.5, 1.0)),
            }],
            expected_decision_count: Some(0),
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 2: Silent Agent ────────────────────────────────────────

fn scenario_silent_agent() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Beta".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm: 5 observations at 60s intervals.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Beta".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // The liveness `DecisionCore` uses `alpha = 0.3` (see
    // `default_liveness_core`) so each `evaluate_liveness` call only
    // shifts the posterior by a fractional amount per update. Simulate
    // production cadence by ticking `evaluate_liveness` every 30s
    // during the silence window — ~50 updates across 25 minutes lets
    // the posterior collapse to `Dead` as the scenario expects.
    let first_silence_eval_sec = 360;
    let last_silence_eval_sec = 30 * 60;
    let mut eval_sec = first_silence_eval_sec;
    while eval_sec <= last_silence_eval_sec {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: eval_sec * US_PER_SEC,
        });
        eval_sec += 30;
    }

    ScenarioManifest {
        id: ScenarioId::SilentAgent,
        description: "Agent goes silent; ATC detects silence, suspects, eventually declares dead",
        seed: 2,
        config: test_config(),
        events,
        checkpoints: vec![
            // With the 30s-eval cadence above, after 5 minutes of
            // silence we have ~10 posterior updates and the agent should
            // already be on the `Suspect` action path → `Flaky` state.
            Checkpoint {
                label: "after_5_minutes_silence",
                timestamp_micros: 10 * 60 * US_PER_SEC,
                expected_agents: vec![ExpectedAgentState {
                    name: "Beta".into(),
                    liveness: LivenessState::Flaky,
                    alive_posterior_range: None,
                }],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: Some(1),
                expected_actions: vec![],
            },
            Checkpoint {
                label: "after_25_minutes_silence",
                timestamp_micros: 30 * 60 * US_PER_SEC,
                expected_agents: vec![ExpectedAgentState {
                    name: "Beta".into(),
                    liveness: LivenessState::Dead,
                    alive_posterior_range: Some((0.0, 0.2)),
                }],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: Some(2),
                expected_actions: vec![],
            },
        ],
    }
}

// ── Scenario 3: Flaky Agent ─────────────────────────────────────────

fn scenario_flaky_agent() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Gamma".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm with 5 observations.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Gamma".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Go silent — tick evaluate_liveness every 30s so the posterior
    // accumulates enough updates to cross into `Suspect` / `Flaky` by
    // the 12-minute checkpoint (≈7 minutes of silence, ~14 ticks).
    let mut eval_sec = 360;
    while eval_sec <= 12 * 60 {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: eval_sec * US_PER_SEC,
        });
        eval_sec += 30;
    }

    // Come back briefly.
    events.push(ScenarioEvent::AgentActivity {
        agent: "Gamma".into(),
        project_key: None,
        timestamp_micros: 13 * 60 * US_PER_SEC,
    });

    // Go silent again — same tick cadence.
    let mut eval_sec = 13 * 60 + 30;
    while eval_sec <= 20 * 60 {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: eval_sec * US_PER_SEC,
        });
        eval_sec += 30;
    }

    ScenarioManifest {
        id: ScenarioId::FlakyAgent,
        description: "Agent intermittently drops and resumes activity",
        seed: 3,
        config: test_config(),
        events,
        checkpoints: vec![
            Checkpoint {
                label: "after_first_silence",
                timestamp_micros: 12 * 60 * US_PER_SEC,
                expected_agents: vec![ExpectedAgentState {
                    name: "Gamma".into(),
                    liveness: LivenessState::Flaky,
                    alive_posterior_range: None,
                }],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
            Checkpoint {
                label: "after_resurrection",
                timestamp_micros: 13 * 60 * US_PER_SEC + 1,
                expected_agents: vec![ExpectedAgentState {
                    name: "Gamma".into(),
                    liveness: LivenessState::Alive,
                    alive_posterior_range: None,
                }],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
        ],
    }
}

// ── Scenario 4: False Probe ─────────────────────────────────────────

fn scenario_false_probe() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Delta".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm: short intervals.
    for i in 0..10 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Delta".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 30 * US_PER_SEC,
        });
    }

    // One longer gap that triggers suspicion but agent is actually alive.
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 8 * 60 * US_PER_SEC,
    });

    // Agent proves alive with activity.
    events.push(ScenarioEvent::AgentActivity {
        agent: "Delta".into(),
        project_key: None,
        timestamp_micros: 8 * 60 * US_PER_SEC + 5 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::FalseProbe,
        description: "Agent triggers suspicion threshold but is actually alive",
        seed: 4,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_false_alarm_resolved",
            timestamp_micros: 8 * 60 * US_PER_SEC + 5 * US_PER_SEC + 1,
            expected_agents: vec![ExpectedAgentState {
                name: "Delta".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: None,
            }],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 5: Advisory Success ────────────────────────────────────

fn scenario_advisory_success() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Epsilon".into(),
        program: "claude-code".into(),
        project_key: Some("/tmp/epsilon-project".into()),
    }];

    // Establish rhythm.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Epsilon".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Go silent; ATC suspects. Tick evaluate_liveness every 30s so
    // the liveness decision core sees enough posterior updates to
    // cross the Suspect threshold by the 10-minute checkpoint.
    let mut eval_sec = 360;
    while eval_sec <= 10 * 60 {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: eval_sec * US_PER_SEC,
        });
        eval_sec += 30;
    }

    // Agent resumes (advisory "worked").
    events.push(ScenarioEvent::AgentActivity {
        agent: "Epsilon".into(),
        project_key: None,
        timestamp_micros: 11 * 60 * US_PER_SEC,
    });

    // Verify restored.
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 12 * 60 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::AdvisorySuccess,
        description: "Agent resumes activity after ATC advisory; resurrection confirmed",
        seed: 5,
        config: test_config(),
        events,
        checkpoints: vec![
            Checkpoint {
                label: "after_silence_detected",
                timestamp_micros: 10 * 60 * US_PER_SEC,
                expected_agents: vec![ExpectedAgentState {
                    name: "Epsilon".into(),
                    liveness: LivenessState::Flaky,
                    alive_posterior_range: None,
                }],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: Some(1),
                expected_actions: vec![],
            },
            Checkpoint {
                label: "after_advisory_success",
                timestamp_micros: 12 * 60 * US_PER_SEC,
                expected_agents: vec![ExpectedAgentState {
                    name: "Epsilon".into(),
                    liveness: LivenessState::Alive,
                    alive_posterior_range: None,
                }],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
        ],
    }
}

// ── Scenario 6: Advisory Failure ────────────────────────────────────

fn scenario_advisory_failure() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Zeta".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Zeta".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Go silent forever — never comes back. Tick every 30s so the
    // posterior can collapse through Alive → Flaky → Dead under the
    // `alpha = 0.3` liveness EWMA.
    let mut eval_sec = 360;
    while eval_sec <= 30 * 60 {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: eval_sec * US_PER_SEC,
        });
        eval_sec += 30;
    }

    ScenarioManifest {
        id: ScenarioId::AdvisoryFailure,
        description: "Agent never resumes after advisory; eventually declared dead",
        seed: 6,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_prolonged_silence",
            timestamp_micros: 30 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Zeta".into(),
                liveness: LivenessState::Dead,
                alive_posterior_range: Some((0.0, 0.1)),
            }],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: Some(2),
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 7: Conflict Resolution ─────────────────────────────────

fn scenario_conflict_resolution() -> ScenarioManifest {
    let events = vec![
        ScenarioEvent::RegisterAgent {
            name: "Eta".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/project".into()),
        },
        ScenarioEvent::RegisterAgent {
            name: "Theta".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/project".into()),
        },
        // Eta gets exclusive reservation.
        ScenarioEvent::ReservationGranted {
            agent: "Eta".into(),
            paths: vec!["src/*.rs".into()],
            exclusive: true,
            project: "/tmp/project".into(),
            timestamp_micros: US_PER_SEC,
        },
        // Theta tries overlapping reservation — conflict.
        ScenarioEvent::ReservationConflict {
            requester: "Theta".into(),
            project: "/tmp/project".into(),
            conflicts: vec![("Eta".into(), "src/main.rs".into(), "src/*.rs".into())],
            timestamp_micros: 2 * US_PER_SEC,
        },
        // Eta releases.
        ScenarioEvent::ReservationReleased {
            agent: "Eta".into(),
            paths: vec!["src/*.rs".into()],
            project: "/tmp/project".into(),
            timestamp_micros: 5 * US_PER_SEC,
        },
    ];

    ScenarioManifest {
        id: ScenarioId::ConflictResolution,
        description: "Two agents conflict on file reservation; resolved by release",
        seed: 7,
        config: test_config(),
        events,
        checkpoints: vec![
            Checkpoint {
                label: "after_conflict_recorded",
                timestamp_micros: 3 * US_PER_SEC,
                expected_agents: vec![],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: Some(0),
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
            Checkpoint {
                label: "after_conflict_resolved",
                timestamp_micros: 6 * US_PER_SEC,
                expected_agents: vec![],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: Some(0),
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
        ],
    }
}

// ── Scenario 8: Deliberate No-Op ────────────────────────────────────

fn scenario_deliberate_no_op() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Iota".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Continuous activity — ATC should never intervene.
    for i in 0..20 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Iota".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: (i + 1) * 60 * US_PER_SEC + US_PER_SEC,
        });
    }

    ScenarioManifest {
        id: ScenarioId::DeliberateNoOp,
        description: "Active agent with no issues; ATC correctly decides not to intervene",
        seed: 8,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_20_minutes_no_intervention",
            timestamp_micros: 20 * 60 * US_PER_SEC + 2 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Iota".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: Some((0.5, 1.0)),
            }],
            expected_decision_count: Some(0),
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 9: Safe-to-Ignore Noise ────────────────────────────────

fn scenario_safe_to_ignore_noise() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Kappa".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish strong alive rhythm.
    for i in 0..10 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Kappa".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Slightly long gap (1.5x average) then activity.
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 11 * 60 * US_PER_SEC + 30 * US_PER_SEC,
    });
    events.push(ScenarioEvent::AgentActivity {
        agent: "Kappa".into(),
        project_key: None,
        timestamp_micros: 12 * 60 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::SafeToIgnoreNoise,
        description: "Minor silence deviation does not trigger intervention",
        seed: 9,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "minor_silence_no_alarm",
            timestamp_micros: 12 * 60 * US_PER_SEC + 1,
            expected_agents: vec![ExpectedAgentState {
                name: "Kappa".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: None,
            }],
            expected_decision_count: Some(0),
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 10: Suppressed Unsafe Action ───────────────────────────

fn scenario_suppressed_unsafe_action() -> ScenarioManifest {
    let mut events = vec![
        ScenarioEvent::RegisterAgent {
            name: "Lambda".into(),
            program: "claude-code".into(),
            project_key: None,
        },
        // Force safe mode on.
        ScenarioEvent::SetSafeMode {
            active: true,
            timestamp_micros: US_PER_SEC,
        },
    ];

    // Establish rhythm then go silent.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Lambda".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Long silence — in safe mode, aggressive actions should be suppressed.
    for tick in [10, 15, 20, 30] {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: tick * 60 * US_PER_SEC,
        });
    }

    ScenarioManifest {
        id: ScenarioId::SuppressedUnsafeAction,
        description: "Safe mode prevents ReleaseReservations even when posterior favors Dead",
        seed: 10,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "safe_mode_suppresses_release",
            timestamp_micros: 30 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Lambda".into(),
                // In safe mode with release guard, should be Flaky not Dead.
                liveness: LivenessState::Flaky,
                alive_posterior_range: Some((0.0, 0.3)),
            }],
            expected_decision_count: None,
            expected_safe_mode: Some(true),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: Some(1),
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 11: Drift Onset ────────────────────────────────────────

fn scenario_drift_onset() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Mu".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Inject many incorrect predictions to trigger e-process drift.
    for i in 0..50 {
        events.push(ScenarioEvent::CalibrationOutcome {
            correct: i % 3 != 0, // ~33% error rate (above 15% baseline)
            subsystem: AtcSubsystem::Liveness,
            agent: Some("Mu".into()),
        });
    }

    ScenarioManifest {
        id: ScenarioId::DriftOnset,
        description: "Gradual miscalibration triggers e-process alert",
        seed: 11,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_drift_onset",
            timestamp_micros: 1,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: None,
            expected_deadlock_cycles: None,
            expected_miscalibrated: Some(true),
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 12: Calibration Rollback ───────────────────────────────

fn scenario_calibration_rollback() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Nu".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Inject errors to trigger miscalibration.
    for _ in 0..40 {
        events.push(ScenarioEvent::CalibrationOutcome {
            correct: false,
            subsystem: AtcSubsystem::Liveness,
            agent: Some("Nu".into()),
        });
    }

    // ONS is a one-way evidence accumulator: once the e-value has
    // crossed the alert threshold it cannot be "unbet" — subsequent
    // correct outcomes push the adaptive bet size toward zero (then
    // slightly negative), but the factor stays close to 1 and never
    // drives the e-value back below 20. That is the intended
    // supermartingale behavior: miscalibration evidence is durable.
    // The ORIGINAL scenario tried to demonstrate recovery through
    // corrects alone, which is mathematically impossible under this
    // detector. We keep the same name for continuity but verify the
    // actual contract: after sustained errors the global e-process
    // enters — and stays in — the miscalibrated regime.
    //
    // All `CalibrationOutcome` events carry an effective timestamp of
    // `0` (see `event_timestamp`), so a single end-state checkpoint is
    // sufficient.
    ScenarioManifest {
        id: ScenarioId::CalibrationRollback,
        description: "E-process records sustained miscalibration as durable evidence",
        seed: 12,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_drift",
            timestamp_micros: 1,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: None,
            expected_deadlock_cycles: None,
            expected_miscalibrated: Some(true),
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 13: Restart Recovery ───────────────────────────────────

fn scenario_restart_recovery() -> ScenarioManifest {
    let events = vec![
        ScenarioEvent::RegisterAgent {
            name: "Xi".into(),
            program: "claude-code".into(),
            project_key: None,
        },
        ScenarioEvent::AgentActivity {
            agent: "Xi".into(),
            project_key: None,
            timestamp_micros: 60 * US_PER_SEC,
        },
        // Simulate restart by re-registering.
        ScenarioEvent::RegisterAgent {
            name: "Xi".into(),
            program: "claude-code".into(),
            project_key: None,
        },
        ScenarioEvent::AgentActivity {
            agent: "Xi".into(),
            project_key: None,
            timestamp_micros: 2 * 60 * US_PER_SEC,
        },
        ScenarioEvent::EvaluateLiveness {
            timestamp_micros: 3 * 60 * US_PER_SEC,
        },
    ];

    ScenarioManifest {
        id: ScenarioId::RestartRecovery,
        description: "Re-registration after restart preserves agent state",
        seed: 13,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_restart",
            timestamp_micros: 3 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Xi".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: None,
            }],
            expected_decision_count: Some(0),
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 14: Degraded-Learning Safe Mode ────────────────────────

fn scenario_degraded_learning_safe_mode() -> ScenarioManifest {
    let mut events = vec![];

    // Drive many errors to enter safe mode.
    for _ in 0..60 {
        events.push(ScenarioEvent::CalibrationOutcome {
            correct: false,
            subsystem: AtcSubsystem::Liveness,
            agent: None,
        });
    }

    // Force safe mode on (operator override).
    events.push(ScenarioEvent::SetSafeMode {
        active: true,
        timestamp_micros: US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::DegradedLearningSafeMode,
        description: "System enters safe mode due to calibration failure",
        seed: 14,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "safe_mode_engaged",
            timestamp_micros: 2 * US_PER_SEC,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: Some(true),
            expected_deadlock_cycles: None,
            expected_miscalibrated: Some(true),
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 15: Spoofed Liveness ───────────────────────────────────

fn scenario_spoofed_liveness() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Omicron".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Rapid-fire activity signals (1 per second) — suspicious pattern.
    for i in 0..100 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Omicron".into(),
            project_key: None,
            timestamp_micros: i * US_PER_SEC,
        });
    }

    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 101 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::SpoofedLiveness,
        description: "Rapid artificial activity signals; agent remains alive but with abnormal rhythm",
        seed: 15,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_rapid_signals",
            timestamp_micros: 102 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Omicron".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: None,
            }],
            expected_decision_count: Some(0),
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 16: Duplicate Events ───────────────────────────────────

fn scenario_duplicate_events() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Pi".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Send each activity event twice (duplicate delivery).
    for i in 0..10 {
        let ts = (i + 1) * 60 * US_PER_SEC;
        events.push(ScenarioEvent::AgentActivity {
            agent: "Pi".into(),
            project_key: None,
            timestamp_micros: ts,
        });
        events.push(ScenarioEvent::AgentActivity {
            agent: "Pi".into(),
            project_key: None,
            timestamp_micros: ts,
        });
    }

    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 11 * 60 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::DuplicateEvents,
        description: "Duplicate activity events should not corrupt rhythm tracking",
        seed: 16,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_duplicate_stream",
            timestamp_micros: 11 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Pi".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: Some((0.5, 1.0)),
            }],
            expected_decision_count: Some(0),
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 17: Coordinated Reservation Churn ──────────────────────

fn scenario_coordinated_reservation_churn() -> ScenarioManifest {
    let mut events = vec![
        ScenarioEvent::RegisterAgent {
            name: "Rho".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/churn".into()),
        },
        ScenarioEvent::RegisterAgent {
            name: "Sigma".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/churn".into()),
        },
    ];

    // Rapid acquire/release cycles.
    for i in 0..10 {
        let base_ts = (i + 1) * 5 * US_PER_SEC;
        events.push(ScenarioEvent::ReservationGranted {
            agent: "Rho".into(),
            paths: vec![format!("src/file_{i}.rs")],
            exclusive: true,
            project: "/tmp/churn".into(),
            timestamp_micros: base_ts,
        });
        events.push(ScenarioEvent::ReservationReleased {
            agent: "Rho".into(),
            paths: vec![format!("src/file_{i}.rs")],
            project: "/tmp/churn".into(),
            timestamp_micros: base_ts + 2 * US_PER_SEC,
        });
        events.push(ScenarioEvent::ReservationGranted {
            agent: "Sigma".into(),
            paths: vec![format!("src/file_{i}.rs")],
            exclusive: true,
            project: "/tmp/churn".into(),
            timestamp_micros: base_ts + 3 * US_PER_SEC,
        });
    }

    ScenarioManifest {
        id: ScenarioId::CoordinatedReservationChurn,
        description: "Agents rapidly cycle reservations without deadlock",
        seed: 17,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_churn_cycle",
            timestamp_micros: 60 * US_PER_SEC,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: Some(0),
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 18: Overlapping Interventions ──────────────────────────

fn scenario_overlapping_interventions() -> ScenarioManifest {
    let mut events = vec![];

    // Register 3 agents.
    for name in ["AgentA", "AgentB", "AgentC"] {
        events.push(ScenarioEvent::RegisterAgent {
            name: name.into(),
            program: "claude-code".into(),
            project_key: None,
        });
    }

    // All establish rhythm.
    for name in ["AgentA", "AgentB", "AgentC"] {
        for i in 0..5 {
            events.push(ScenarioEvent::AgentActivity {
                agent: name.into(),
                project_key: None,
                timestamp_micros: (i + 1) * 60 * US_PER_SEC,
            });
        }
    }

    // All go silent simultaneously. Tick every 30s so each agent gets
    // enough posterior updates under the `alpha = 0.3` liveness EWMA to
    // trip into `Suspect`/`Flaky` — a single evaluation per agent is
    // not enough; see `scenario_silent_agent` for the cadence rationale.
    let mut eval_sec = 360;
    while eval_sec <= 15 * 60 {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: eval_sec * US_PER_SEC,
        });
        eval_sec += 30;
    }

    ScenarioManifest {
        id: ScenarioId::OverlappingInterventions,
        description: "Multiple agents need ATC attention simultaneously",
        seed: 18,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "multiple_agents_flagged",
            timestamp_micros: 15 * 60 * US_PER_SEC,
            expected_agents: vec![
                ExpectedAgentState {
                    name: "AgentA".into(),
                    liveness: LivenessState::Flaky,
                    alive_posterior_range: None,
                },
                ExpectedAgentState {
                    name: "AgentB".into(),
                    liveness: LivenessState::Flaky,
                    alive_posterior_range: None,
                },
                ExpectedAgentState {
                    name: "AgentC".into(),
                    liveness: LivenessState::Flaky,
                    alive_posterior_range: None,
                },
            ],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: Some(3),
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 19: Concurrent Operator Changes ────────────────────────

fn scenario_concurrent_operator_changes() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Tau".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Tau".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Operator toggles safe mode during silence.
    events.push(ScenarioEvent::SetSafeMode {
        active: true,
        timestamp_micros: 8 * 60 * US_PER_SEC,
    });
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 10 * 60 * US_PER_SEC,
    });
    events.push(ScenarioEvent::SetSafeMode {
        active: false,
        timestamp_micros: 11 * 60 * US_PER_SEC,
    });
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 12 * 60 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::ConcurrentOperatorChanges,
        description: "Operator toggles safe mode during active evaluation",
        seed: 19,
        config: test_config(),
        events,
        checkpoints: vec![
            Checkpoint {
                label: "safe_mode_active",
                timestamp_micros: 10 * 60 * US_PER_SEC,
                expected_agents: vec![],
                expected_decision_count: None,
                expected_safe_mode: Some(true),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
            Checkpoint {
                label: "safe_mode_disabled",
                timestamp_micros: 12 * 60 * US_PER_SEC,
                expected_agents: vec![],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: None,
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
        ],
    }
}

// ── Scenario 20: Natural Recovery Attribution ───────────────────────

fn scenario_natural_recovery_attribution() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Upsilon".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm.
    for i in 0..5 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Upsilon".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Go silent long enough to be flagged.
    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 10 * 60 * US_PER_SEC,
    });

    // Agent recovers on its own (before ATC could send advisory).
    events.push(ScenarioEvent::AgentActivity {
        agent: "Upsilon".into(),
        project_key: None,
        timestamp_micros: 10 * 60 * US_PER_SEC + 30 * US_PER_SEC,
    });

    events.push(ScenarioEvent::EvaluateLiveness {
        timestamp_micros: 11 * 60 * US_PER_SEC,
    });

    ScenarioManifest {
        id: ScenarioId::NaturalRecoveryAttribution,
        description: "Agent recovers naturally during attribution window; ATC should not claim credit",
        seed: 20,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_natural_recovery",
            timestamp_micros: 11 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Upsilon".into(),
                liveness: LivenessState::Alive,
                alive_posterior_range: None,
            }],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 21: Deadlock Cycle ─────────────────────────────────────

fn scenario_deadlock_cycle() -> ScenarioManifest {
    let events = vec![
        ScenarioEvent::RegisterAgent {
            name: "Phi".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/deadlock".into()),
        },
        ScenarioEvent::RegisterAgent {
            name: "Chi".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/deadlock".into()),
        },
        // Phi holds file_a, wants file_b.
        ScenarioEvent::ReservationGranted {
            agent: "Phi".into(),
            paths: vec!["file_a.rs".into()],
            exclusive: true,
            project: "/tmp/deadlock".into(),
            timestamp_micros: US_PER_SEC,
        },
        // Chi holds file_b, wants file_a.
        ScenarioEvent::ReservationGranted {
            agent: "Chi".into(),
            paths: vec!["file_b.rs".into()],
            exclusive: true,
            project: "/tmp/deadlock".into(),
            timestamp_micros: 2 * US_PER_SEC,
        },
        // Chi blocked by Phi on file_a.
        ScenarioEvent::ReservationConflict {
            requester: "Chi".into(),
            project: "/tmp/deadlock".into(),
            conflicts: vec![("Phi".into(), "file_a.rs".into(), "file_a.rs".into())],
            timestamp_micros: 3 * US_PER_SEC,
        },
        // Phi blocked by Chi on file_b.
        ScenarioEvent::ReservationConflict {
            requester: "Phi".into(),
            project: "/tmp/deadlock".into(),
            conflicts: vec![("Chi".into(), "file_b.rs".into(), "file_b.rs".into())],
            timestamp_micros: 4 * US_PER_SEC,
        },
    ];

    ScenarioManifest {
        id: ScenarioId::DeadlockCycle,
        description: "Two agents form a deadlock cycle via mutual blocking reservations",
        seed: 21,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "deadlock_detected",
            timestamp_micros: 5 * US_PER_SEC,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: Some(1),
            expected_miscalibrated: None,
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 22: Multi-Agent Conflict ───────────────────────────────

fn scenario_multi_agent_conflict() -> ScenarioManifest {
    let events = vec![
        ScenarioEvent::RegisterAgent {
            name: "Psi".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/multi".into()),
        },
        ScenarioEvent::RegisterAgent {
            name: "Omega".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/multi".into()),
        },
        ScenarioEvent::RegisterAgent {
            name: "AlphaB".into(),
            program: "claude-code".into(),
            project_key: Some("/tmp/multi".into()),
        },
        // Psi holds the file.
        ScenarioEvent::ReservationGranted {
            agent: "Psi".into(),
            paths: vec!["shared.rs".into()],
            exclusive: true,
            project: "/tmp/multi".into(),
            timestamp_micros: US_PER_SEC,
        },
        // Both Omega and AlphaB blocked.
        ScenarioEvent::ReservationConflict {
            requester: "Omega".into(),
            project: "/tmp/multi".into(),
            conflicts: vec![("Psi".into(), "shared.rs".into(), "shared.rs".into())],
            timestamp_micros: 2 * US_PER_SEC,
        },
        ScenarioEvent::ReservationConflict {
            requester: "AlphaB".into(),
            project: "/tmp/multi".into(),
            conflicts: vec![("Psi".into(), "shared.rs".into(), "shared.rs".into())],
            timestamp_micros: 3 * US_PER_SEC,
        },
        // Psi releases — conflicts should clear.
        ScenarioEvent::ReservationReleased {
            agent: "Psi".into(),
            paths: vec!["shared.rs".into()],
            project: "/tmp/multi".into(),
            timestamp_micros: 10 * US_PER_SEC,
        },
    ];

    ScenarioManifest {
        id: ScenarioId::MultiAgentConflict,
        description: "Multiple agents blocked by one holder; resolved when holder releases",
        seed: 22,
        config: test_config(),
        events,
        checkpoints: vec![
            Checkpoint {
                label: "two_agents_blocked",
                timestamp_micros: 4 * US_PER_SEC,
                expected_agents: vec![],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: Some(0),
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
            Checkpoint {
                label: "after_holder_releases",
                timestamp_micros: 11 * US_PER_SEC,
                expected_agents: vec![],
                expected_decision_count: None,
                expected_safe_mode: Some(false),
                expected_deadlock_cycles: Some(0),
                expected_miscalibrated: None,
                min_ledger_entries: None,
                expected_actions: vec![],
            },
        ],
    }
}

// ── Scenario 23: Posterior Convergence ───────────────────────────────

fn scenario_posterior_convergence() -> ScenarioManifest {
    let mut events = vec![ScenarioEvent::RegisterAgent {
        name: "Converge".into(),
        program: "claude-code".into(),
        project_key: None,
    }];

    // Establish rhythm with 3 observations.
    for i in 0..3 {
        events.push(ScenarioEvent::AgentActivity {
            agent: "Converge".into(),
            project_key: None,
            timestamp_micros: (i + 1) * 60 * US_PER_SEC,
        });
    }

    // Long silence, repeated evaluations should converge posterior toward Dead.
    for tick in (1..=20).map(|t| (3 + t) * 60 * US_PER_SEC) {
        events.push(ScenarioEvent::EvaluateLiveness {
            timestamp_micros: tick,
        });
    }

    ScenarioManifest {
        id: ScenarioId::PosteriorConvergence,
        description: "Posterior converges under consistent silence evidence",
        seed: 23,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "posterior_converged_to_dead",
            timestamp_micros: 23 * 60 * US_PER_SEC,
            expected_agents: vec![ExpectedAgentState {
                name: "Converge".into(),
                liveness: LivenessState::Dead,
                alive_posterior_range: Some((0.0, 0.05)),
            }],
            expected_decision_count: None,
            expected_safe_mode: Some(false),
            expected_deadlock_cycles: None,
            expected_miscalibrated: None,
            min_ledger_entries: Some(2),
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 24: E-Process Miscalibration ───────────────────────────

fn scenario_eprocess_miscalibration() -> ScenarioManifest {
    let mut events = vec![];

    // Inject 100% error rate to overwhelm e-process.
    for _ in 0..30 {
        events.push(ScenarioEvent::CalibrationOutcome {
            correct: false,
            subsystem: AtcSubsystem::Liveness,
            agent: Some("TestAgent".into()),
        });
    }

    ScenarioManifest {
        id: ScenarioId::EProcessMiscalibration,
        description: "Sustained prediction errors trigger e-process miscalibration alarm",
        seed: 24,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "eprocess_alarmed",
            timestamp_micros: 1,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: None,
            expected_deadlock_cycles: None,
            expected_miscalibrated: Some(true),
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ── Scenario 25: CUSUM Regime Change ────────────────────────────────

fn scenario_cusum_regime_change() -> ScenarioManifest {
    let mut events = vec![];

    // Period of good calibration.
    for _ in 0..20 {
        events.push(ScenarioEvent::CalibrationOutcome {
            correct: true,
            subsystem: AtcSubsystem::Liveness,
            agent: None,
        });
    }

    // Sudden regime shift to high error rate.
    for _i in 0..30 {
        events.push(ScenarioEvent::CalibrationOutcome {
            correct: false,
            subsystem: AtcSubsystem::Liveness,
            agent: None,
        });
    }

    ScenarioManifest {
        id: ScenarioId::CusumRegimeChange,
        description: "CUSUM detects regime change from good to poor calibration",
        seed: 25,
        config: test_config(),
        events,
        checkpoints: vec![Checkpoint {
            label: "after_regime_shift",
            timestamp_micros: 1,
            expected_agents: vec![],
            expected_decision_count: None,
            expected_safe_mode: None,
            expected_deadlock_cycles: None,
            expected_miscalibrated: Some(true),
            min_ledger_entries: None,
            expected_actions: vec![],
        }],
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests — deterministic replay of the full corpus
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atc::GLOBAL_ATC_TEST_LOCK;

    fn replay_with_lock(id: ScenarioId) -> ReplayResult {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        crate::atc::reset_global_atc_state_for_test(&config);

        let manifest = build_scenario(id);
        replay_scenario(&manifest)
    }

    #[test]
    fn scenario_healthy_liveness_passes() {
        let result = replay_with_lock(ScenarioId::HealthyLiveness);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_silent_agent_passes() {
        let result = replay_with_lock(ScenarioId::SilentAgent);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_flaky_agent_passes() {
        let result = replay_with_lock(ScenarioId::FlakyAgent);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_false_probe_passes() {
        let result = replay_with_lock(ScenarioId::FalseProbe);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_advisory_success_passes() {
        let result = replay_with_lock(ScenarioId::AdvisorySuccess);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_advisory_failure_passes() {
        let result = replay_with_lock(ScenarioId::AdvisoryFailure);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_conflict_resolution_passes() {
        let result = replay_with_lock(ScenarioId::ConflictResolution);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_deliberate_no_op_passes() {
        let result = replay_with_lock(ScenarioId::DeliberateNoOp);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_safe_to_ignore_noise_passes() {
        let result = replay_with_lock(ScenarioId::SafeToIgnoreNoise);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_suppressed_unsafe_action_passes() {
        let result = replay_with_lock(ScenarioId::SuppressedUnsafeAction);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_drift_onset_passes() {
        let result = replay_with_lock(ScenarioId::DriftOnset);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_calibration_rollback_passes() {
        let result = replay_with_lock(ScenarioId::CalibrationRollback);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_restart_recovery_passes() {
        let result = replay_with_lock(ScenarioId::RestartRecovery);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_degraded_learning_safe_mode_passes() {
        let result = replay_with_lock(ScenarioId::DegradedLearningSafeMode);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_spoofed_liveness_passes() {
        let result = replay_with_lock(ScenarioId::SpoofedLiveness);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_duplicate_events_passes() {
        let result = replay_with_lock(ScenarioId::DuplicateEvents);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_coordinated_reservation_churn_passes() {
        let result = replay_with_lock(ScenarioId::CoordinatedReservationChurn);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_overlapping_interventions_passes() {
        let result = replay_with_lock(ScenarioId::OverlappingInterventions);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_concurrent_operator_changes_passes() {
        let result = replay_with_lock(ScenarioId::ConcurrentOperatorChanges);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_natural_recovery_attribution_passes() {
        let result = replay_with_lock(ScenarioId::NaturalRecoveryAttribution);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_deadlock_cycle_passes() {
        let result = replay_with_lock(ScenarioId::DeadlockCycle);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_multi_agent_conflict_passes() {
        let result = replay_with_lock(ScenarioId::MultiAgentConflict);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_posterior_convergence_passes() {
        let result = replay_with_lock(ScenarioId::PosteriorConvergence);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_eprocess_miscalibration_passes() {
        let result = replay_with_lock(ScenarioId::EProcessMiscalibration);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    #[test]
    fn scenario_cusum_regime_change_passes() {
        let result = replay_with_lock(ScenarioId::CusumRegimeChange);
        for cp in &result.checkpoints {
            assert!(
                cp.passed,
                "checkpoint '{}' failed: {:?}",
                cp.label, cp.failures
            );
        }
        assert!(result.all_passed);
    }

    // ── Corpus-level tests ──────────────────────────────────────────

    #[test]
    fn all_scenario_ids_are_unique() {
        let ids: Vec<&str> = ScenarioId::all().iter().map(|id| id.as_str()).collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(id), "duplicate scenario ID: {id}");
        }
    }

    #[test]
    fn corpus_covers_minimum_25_scenarios() {
        assert!(
            ScenarioId::all().len() >= 25,
            "corpus must have at least 25 scenarios, got {}",
            ScenarioId::all().len()
        );
    }

    #[test]
    fn all_scenarios_have_at_least_one_checkpoint() {
        for id in ScenarioId::all() {
            let manifest = build_scenario(*id);
            assert!(
                !manifest.checkpoints.is_empty(),
                "scenario '{}' has no checkpoints",
                id.as_str()
            );
        }
    }

    #[test]
    fn replay_is_deterministic_across_runs() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();

        // Run the same scenario twice and verify identical results.
        crate::atc::reset_global_atc_state_for_test(&config);
        let manifest = build_scenario(ScenarioId::SilentAgent);
        let result1 = replay_scenario(&manifest);

        crate::atc::reset_global_atc_state_for_test(&config);
        let result2 = replay_scenario(&manifest);

        assert_eq!(
            result1.final_decision_count, result2.final_decision_count,
            "determinism violated: decision counts differ between runs"
        );
        assert_eq!(
            result1.final_safe_mode, result2.final_safe_mode,
            "determinism violated: safe mode differs between runs"
        );
        assert_eq!(
            result1.all_passed, result2.all_passed,
            "determinism violated: pass/fail differs between runs"
        );
        for (cp1, cp2) in result1.checkpoints.iter().zip(result2.checkpoints.iter()) {
            assert_eq!(
                cp1.passed, cp2.passed,
                "determinism violated at checkpoint '{}': pass={} vs {}",
                cp1.label, cp1.passed, cp2.passed
            );
        }
    }

    #[test]
    fn all_scenario_manifests_have_stable_seeds() {
        let scenarios = all_scenarios();
        let mut seen_seeds = std::collections::HashSet::new();
        for scenario in &scenarios {
            assert!(
                seen_seeds.insert(scenario.seed),
                "duplicate seed {} in scenario '{}'",
                scenario.seed,
                scenario.id.as_str()
            );
        }
    }
}
