//! Outcome labeling automata and censoring rules for ATC learning (br-0qt6e.3.1).
//!
//! This module defines deterministic, machine-checkable rules for what counts
//! as success, failure, suppression, or censoring for each action family.
//! Without these rules the learning loop would drift into ad-hoc label noise.
//!
//! # Design Principles
//!
//! 1. **Execution truth, not intent**: Labels consume actual execution results
//!    and delayed evidence, not what ATC intended to do.
//!
//! 2. **Deterministic**: Given the same inputs, the automaton always produces
//!    the same label. No randomization, no fuzzy heuristics.
//!
//! 3. **Censoring over false precision**: When attribution is ambiguous
//!    (overlapping interventions, exogenous recovery, concurrent operator
//!    changes), the automaton censors rather than inventing a confident label.
//!
//! 4. **All paths labeled**: Acted, deliberately non-acted, safety-suppressed,
//!    throttled, and causally confounded paths all have explicit treatment.
//!
//! # Action Families
//!
//! Each [`EffectKind`] has its own labeling rules because the observable
//! outcomes differ:
//!
//! | Family             | Success Signal          | Failure Signal            |
//! |--------------------|-------------------------|---------------------------|
//! | Probe              | Agent responds          | No response within window |
//! | Advisory           | Agent behavior changes  | No behavior change        |
//! | Release            | Released agent was dead  | Released agent was alive  |
//! | `ForceReservation` | Conflict resolved       | New conflict created      |
//! | `RoutingSuggestion`| Load rebalanced         | Load worsened             |
//! | Backpressure       | Queue depth drops       | Queue depth unchanged     |
//! | `NoAction`         | Situation stable        | Situation worsened        |
//!
//! # Attribution Windows
//!
//! Each action family defines how long to wait for outcome evidence before
//! censoring. These windows are tuned to balance learning speed against
//! attribution confidence.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

use crate::experience::{EffectKind, ExperienceOutcome, ExperienceState, NonExecutionReason};

// ──────────────────────────────────────────────────────────────────────
// Outcome label taxonomy
// ──────────────────────────────────────────────────────────────────────

/// The resolved label for an ATC experience.
///
/// This is the learning signal. Each variant carries enough information
/// for correctness and realized-loss tracking downstream. Regret remains
/// optional and depends on caller-supplied optimal-loss context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeLabel {
    /// The action achieved its intended effect.
    Success {
        /// The observed loss (lower is better).
        realized_loss: f64,
        /// Confidence in this label (0.0–1.0). 1.0 = unambiguous.
        confidence: f64,
    },
    /// The action failed to achieve its intended effect.
    Failure {
        /// The observed loss.
        realized_loss: f64,
        /// Confidence in this label.
        confidence: f64,
        /// Why the action failed (human-readable).
        reason: String,
    },
    /// The action was correct (true positive or true negative).
    Correct {
        /// The observed loss (should be low).
        realized_loss: f64,
    },
    /// The action was incorrect (false positive or false negative).
    Incorrect {
        /// The observed loss (should be high).
        realized_loss: f64,
        /// Whether this was a false positive (acted when shouldn't have).
        false_positive: bool,
    },
    /// The outcome is ambiguous — attribution cannot be determined.
    Censored {
        /// Why attribution failed.
        reason: CensorReason,
    },
    /// The experience was a deliberate non-execution with observable outcome.
    NonExecution {
        /// Whether inaction was correct (situation stayed stable or resolved).
        inaction_correct: bool,
        /// The realized cost of inaction (0 if correct, positive if wrong).
        realized_loss: f64,
    },
}

impl OutcomeLabel {
    /// Whether this label carries a usable learning signal.
    ///
    /// Censored labels do NOT carry learning signal — they should be
    /// excluded from loss computations and regret tracking.
    #[must_use]
    pub const fn has_learning_signal(&self) -> bool {
        !matches!(self, Self::Censored { .. })
    }

    /// Extract the realized loss if available.
    #[must_use]
    pub const fn realized_loss(&self) -> Option<f64> {
        match self {
            Self::Success { realized_loss, .. }
            | Self::Failure { realized_loss, .. }
            | Self::Correct { realized_loss }
            | Self::Incorrect { realized_loss, .. }
            | Self::NonExecution { realized_loss, .. } => Some(*realized_loss),
            Self::Censored { .. } => None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Censoring reasons
// ──────────────────────────────────────────────────────────────────────

/// Why an experience was censored (outcome unattributable).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CensorReason {
    /// Attribution window expired before outcome was observed.
    WindowExpired,
    /// The subject (agent/thread) departed before outcome could be observed.
    SubjectDeparted,
    /// Multiple ATC interventions overlapped during the attribution window,
    /// making it impossible to attribute the outcome to a single action.
    OverlappingInterventions {
        /// Number of concurrent interventions.
        intervention_count: u32,
    },
    /// The outcome appears to be an exogenous recovery — the situation
    /// resolved without ATC intervention (e.g., agent returned on its own).
    ExogenousRecovery,
    /// An operator made a concurrent change that confounds attribution.
    ConcurrentOperatorChange {
        /// Description of the operator action.
        change_description: String,
    },
    /// The execution result was ambiguous (neither clear success nor failure).
    AmbiguousResult {
        /// Description of the ambiguity.
        description: String,
    },
    /// The project was closed or removed during the attribution window.
    ProjectClosed,
    /// Insufficient evidence to assign a confident label.
    InsufficientEvidence {
        /// How many evidence signals were expected.
        expected: u32,
        /// How many were actually observed.
        observed: u32,
    },
}

impl std::fmt::Display for CensorReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WindowExpired => write!(f, "attribution window expired"),
            Self::SubjectDeparted => write!(f, "subject departed"),
            Self::OverlappingInterventions { intervention_count } => {
                write!(f, "{intervention_count} overlapping interventions")
            }
            Self::ExogenousRecovery => write!(f, "exogenous recovery"),
            Self::ConcurrentOperatorChange { change_description } => {
                write!(f, "concurrent operator change: {change_description}")
            }
            Self::AmbiguousResult { description } => {
                write!(f, "ambiguous: {description}")
            }
            Self::ProjectClosed => write!(f, "project closed"),
            Self::InsufficientEvidence { expected, observed } => {
                write!(f, "insufficient evidence ({observed}/{expected})")
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Attribution windows (per action family)
// ──────────────────────────────────────────────────────────────────────

/// Attribution window configuration for an action family.
///
/// Defines how long to wait for outcome evidence before censoring,
/// and the causal eligibility rules for that family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionWindow {
    /// Maximum time to wait for outcome evidence (microseconds).
    pub max_window_micros: i64,
    /// Minimum time before an outcome is considered attributable
    /// (to filter out coincidental timing).
    pub min_delay_micros: i64,
    /// Maximum number of concurrent interventions before censoring.
    /// 0 means any overlap forces censoring.
    pub max_concurrent_interventions: u32,
    /// Whether exogenous recovery should censor or count as success.
    pub censor_on_exogenous_recovery: bool,
}

/// Attribution windows for each effect kind.
///
/// These are calibrated to balance learning speed against attribution
/// confidence. Shorter windows learn faster but risk false attribution.
#[must_use]
pub const fn attribution_window(kind: EffectKind) -> AttributionWindow {
    match kind {
        // Probes: response expected within 30s, allow up to 60s
        EffectKind::Probe => AttributionWindow {
            max_window_micros: 60_000_000,       // 60s
            min_delay_micros: 0,                 // immediate response is valid
            max_concurrent_interventions: 0,     // any overlap censors
            censor_on_exogenous_recovery: false, // probe response IS the outcome
        },
        // Advisories: behavior change expected within 5 minutes
        EffectKind::Advisory => AttributionWindow {
            max_window_micros: 300_000_000,     // 5 min
            min_delay_micros: 5_000_000,        // 5s minimum (filter noise)
            max_concurrent_interventions: 1,    // one other intervention OK
            censor_on_exogenous_recovery: true, // hard to distinguish advisory effect from natural
        },
        // Release / Force reservation: outcome visible quickly, no overlap tolerated
        EffectKind::Release | EffectKind::ForceReservation => AttributionWindow {
            max_window_micros: 120_000_000, // 2 min
            min_delay_micros: 0,
            max_concurrent_interventions: 0, // no overlap for high-force
            censor_on_exogenous_recovery: false, // these are definitive actions
        },
        // Routing suggestions: load change within 5 minutes
        EffectKind::RoutingSuggestion => AttributionWindow {
            max_window_micros: 300_000_000, // 5 min
            min_delay_micros: 10_000_000,   // 10s minimum
            max_concurrent_interventions: 1,
            censor_on_exogenous_recovery: true,
        },
        // Backpressure: queue depth change within 2 minutes
        EffectKind::Backpressure => AttributionWindow {
            max_window_micros: 120_000_000, // 2 min
            min_delay_micros: 5_000_000,    // 5s minimum
            max_concurrent_interventions: 1,
            censor_on_exogenous_recovery: true,
        },
        // No-action: check situation after full probe interval
        EffectKind::NoAction => AttributionWindow {
            max_window_micros: 300_000_000,      // 5 min (same as advisory)
            min_delay_micros: 30_000_000,        // 30s minimum (let situation develop)
            max_concurrent_interventions: 0,     // if someone else acted, can't evaluate inaction
            censor_on_exogenous_recovery: false, // stable situation = inaction was correct
        },
    }
}

// ──────────────────────────────────────────────────────────────────────
// Labeling automaton input
// ──────────────────────────────────────────────────────────────────────

/// Input to the labeling automaton for resolving an experience.
///
/// Aggregates all evidence needed to determine the outcome label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelingInput {
    /// The experience's current lifecycle state.
    pub state: ExperienceState,
    /// The effect kind that was planned/executed.
    pub effect_kind: EffectKind,
    /// When the experience was created (microseconds).
    pub created_ts_micros: i64,
    /// When the experience was executed (if executed).
    pub executed_ts_micros: Option<i64>,
    /// Current time (microseconds).
    pub now_micros: i64,
    /// Non-execution reason (for Throttled/Suppressed/Skipped states).
    pub non_execution_reason: Option<NonExecutionReason>,
    /// How many concurrent interventions are active on the same subject.
    pub concurrent_intervention_count: u32,
    /// Whether the subject has departed (agent deregistered, project closed).
    pub subject_departed: bool,
    /// Whether an operator made a concurrent change.
    pub operator_change: Option<String>,
    /// The execution outcome evidence (if any was observed).
    pub execution_evidence: Option<ExecutionEvidence>,
}

/// Observed evidence about the execution outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEvidence {
    /// When the evidence was observed (microseconds).
    pub observed_ts_micros: i64,
    /// Whether the intended effect was achieved.
    pub effect_achieved: bool,
    /// Whether the situation is now better, same, or worse.
    pub situation_change: SituationChange,
    /// Whether this appears to be an exogenous recovery.
    pub exogenous_recovery: bool,
    /// Realized loss from the actual outcome.
    pub realized_loss: f64,
    /// The best-possible loss (for regret computation).
    pub best_possible_loss: f64,
}

/// How the situation changed after the intervention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SituationChange {
    /// Situation improved (intended effect achieved or situation resolved).
    Improved,
    /// Situation unchanged (no observable effect).
    Unchanged,
    /// Situation worsened (unintended consequences).
    Worsened,
}

// ──────────────────────────────────────────────────────────────────────
// Labeling automaton
// ──────────────────────────────────────────────────────────────────────

/// The labeling automaton result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelingResult {
    /// The resolved label.
    pub label: OutcomeLabel,
    /// The new experience state after labeling.
    pub new_state: ExperienceState,
    /// Audit trail: which rule produced this label.
    pub rule_id: &'static str,
}

/// Run the labeling automaton on an experience.
///
/// This is the core deterministic function that transforms an experience's
/// current state plus observed evidence into a resolved outcome label.
///
/// # Determinism Guarantee
///
/// Given identical `LabelingInput`, this function always produces the
/// same `LabelingResult`. No randomization, no external state.
#[must_use]
pub fn label_experience(input: &LabelingInput) -> LabelingResult {
    // 1. Handle non-execution states first (Throttled, Suppressed, Skipped)
    if input.state.is_non_execution() {
        return label_non_execution(input);
    }

    // 2. Handle pre-execution states — labeling doesn't apply yet
    if input.state == ExperienceState::Planned || input.state == ExperienceState::Dispatched {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::InsufficientEvidence {
                    expected: 1,
                    observed: 0,
                },
            },
            new_state: input.state, // stay in current pre-execution state
            rule_id: "pre_execution",
        };
    }

    // 3. Handle terminal dispatch failures
    if input.state == ExperienceState::Failed {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::AmbiguousResult {
                    description: "dispatch or execution failed (infra error)".to_string(),
                },
            },
            new_state: ExperienceState::Failed,
            rule_id: "failed_dispatch",
        };
    }

    // 3. Handle unresolved states — check if window expired
    if input.state == ExperienceState::Executed || input.state == ExperienceState::Open {
        return label_open_or_executed(input);
    }

    // 4. Already resolved — idempotent
    LabelingResult {
        label: OutcomeLabel::Censored {
            reason: CensorReason::AmbiguousResult {
                description: format!("already in terminal state: {}", input.state),
            },
        },
        new_state: input.state,
        rule_id: "already_terminal",
    }
}

/// Label an experience in Executed or Open state with attribution window checks.
fn label_open_or_executed(input: &LabelingInput) -> LabelingResult {
    let window = attribution_window(input.effect_kind);
    let anchor = input.executed_ts_micros.unwrap_or(input.created_ts_micros);

    // Check for subject departure
    if input.subject_departed {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::SubjectDeparted,
            },
            new_state: ExperienceState::Censored,
            rule_id: "subject_departed",
        };
    }

    // Check for concurrent operator change
    if let Some(ref change) = input.operator_change {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::ConcurrentOperatorChange {
                    change_description: change.clone(),
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "operator_change",
        };
    }

    // Check for too many concurrent interventions
    if input.concurrent_intervention_count > window.max_concurrent_interventions {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::OverlappingInterventions {
                    intervention_count: input.concurrent_intervention_count,
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "overlapping_interventions",
        };
    }

    // Check if we have evidence. Window enforcement is based on when evidence
    // arrived relative to the attribution anchor, not when this function
    // happens to run.
    if let Some(ref evidence) = input.execution_evidence {
        if let Some(result) = validate_evidence_timing(anchor, window, evidence) {
            return result;
        }

        if let Some(result) = censor_exogenous_recovery(window, evidence) {
            return result;
        }

        return label_from_evidence(input.effect_kind, evidence);
    }

    // No evidence yet, so expiry depends on how long we've waited by "now".
    // Saturating subtraction prevents wrapping on clock skew (anchor > now).
    let elapsed = input.now_micros.saturating_sub(anchor);
    if elapsed > window.max_window_micros {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::WindowExpired,
            },
            new_state: ExperienceState::Expired,
            rule_id: "window_expired",
        };
    }

    // No evidence yet, still within window
    LabelingResult {
        label: OutcomeLabel::Censored {
            reason: CensorReason::InsufficientEvidence {
                expected: 1,
                observed: 0,
            },
        },
        new_state: input.state,
        rule_id: "awaiting_evidence",
    }
}

/// Label a non-execution experience (Throttled, Suppressed, Skipped).
#[must_use]
#[allow(clippy::too_many_lines)]
fn label_non_execution(input: &LabelingInput) -> LabelingResult {
    let window = attribution_window(input.effect_kind);
    let anchor = input.created_ts_micros;

    if input.subject_departed {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::SubjectDeparted,
            },
            new_state: ExperienceState::Censored,
            rule_id: "subject_departed",
        };
    }

    if let Some(ref change) = input.operator_change {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::ConcurrentOperatorChange {
                    change_description: change.clone(),
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "operator_change",
        };
    }

    if input.concurrent_intervention_count > window.max_concurrent_interventions {
        return LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::OverlappingInterventions {
                    intervention_count: input.concurrent_intervention_count,
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "overlapping_interventions",
        };
    }

    match (&input.non_execution_reason, &input.execution_evidence) {
        // Deliberate inaction with observed outcome
        (Some(NonExecutionReason::DeliberateInaction { .. }), Some(evidence)) => {
            if let Some(result) = validate_evidence_timing(anchor, window, evidence) {
                return result;
            }
            if let Some(result) = censor_exogenous_recovery(window, evidence) {
                return result;
            }
            let inaction_correct = matches!(
                evidence.situation_change,
                SituationChange::Improved | SituationChange::Unchanged
            );
            LabelingResult {
                label: OutcomeLabel::NonExecution {
                    inaction_correct,
                    realized_loss: evidence.realized_loss,
                },
                new_state: ExperienceState::Resolved,
                rule_id: "deliberate_inaction_resolved",
            }
        }
        // Safety gate suppression with observed outcome
        (Some(NonExecutionReason::SafetyGate { .. }), Some(evidence)) => {
            if let Some(result) = validate_evidence_timing(anchor, window, evidence) {
                return result;
            }
            if let Some(result) = censor_exogenous_recovery(window, evidence) {
                return result;
            }
            // If the safety gate was correct (situation didn't worsen), it was right to suppress
            let inaction_correct = !matches!(evidence.situation_change, SituationChange::Worsened);
            LabelingResult {
                label: OutcomeLabel::NonExecution {
                    inaction_correct,
                    realized_loss: evidence.realized_loss,
                },
                new_state: ExperienceState::Resolved,
                rule_id: "safety_gate_resolved",
            }
        }
        // Budget exhaustion with observed outcome
        (Some(NonExecutionReason::BudgetExhausted { .. }), Some(evidence)) => {
            if let Some(result) = validate_evidence_timing(anchor, window, evidence) {
                return result;
            }
            if let Some(result) = censor_exogenous_recovery(window, evidence) {
                return result;
            }
            let inaction_correct = !matches!(evidence.situation_change, SituationChange::Worsened);
            LabelingResult {
                label: OutcomeLabel::NonExecution {
                    inaction_correct,
                    realized_loss: evidence.realized_loss,
                },
                new_state: ExperienceState::Resolved,
                rule_id: "budget_exhausted_resolved",
            }
        }
        // Calibration fallback with observed outcome
        (Some(NonExecutionReason::CalibrationFallback { .. }), Some(evidence)) => {
            if let Some(result) = validate_evidence_timing(anchor, window, evidence) {
                return result;
            }
            if let Some(result) = censor_exogenous_recovery(window, evidence) {
                return result;
            }
            let inaction_correct = !matches!(evidence.situation_change, SituationChange::Worsened);
            LabelingResult {
                label: OutcomeLabel::NonExecution {
                    inaction_correct,
                    realized_loss: evidence.realized_loss,
                },
                new_state: ExperienceState::Resolved,
                rule_id: "calibration_fallback_resolved",
            }
        }
        // Non-execution reason missing — ambiguous regardless of evidence.
        (None, _) => LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::AmbiguousResult {
                    description: "non-execution state without reason".to_string(),
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "missing_non_execution_reason",
        },
        // Non-execution without outcome evidence — check window
        (_, None) => {
            let elapsed = input.now_micros.saturating_sub(anchor);
            if elapsed > window.max_window_micros {
                LabelingResult {
                    label: OutcomeLabel::Censored {
                        reason: CensorReason::WindowExpired,
                    },
                    new_state: ExperienceState::Expired,
                    rule_id: "non_execution_window_expired",
                }
            } else {
                LabelingResult {
                    label: OutcomeLabel::Censored {
                        reason: CensorReason::InsufficientEvidence {
                            expected: 1,
                            observed: 0,
                        },
                    },
                    new_state: input.state, // stay in non-execution state
                    rule_id: "non_execution_awaiting_evidence",
                }
            }
        }
    }
}

fn validate_evidence_timing(
    anchor_micros: i64,
    window: AttributionWindow,
    evidence: &ExecutionEvidence,
) -> Option<LabelingResult> {
    if let Some(result) = censor_pre_anchor_evidence(anchor_micros, evidence) {
        return Some(result);
    }

    let evidence_delay = evidence.observed_ts_micros.saturating_sub(anchor_micros);
    if evidence_delay > window.max_window_micros {
        return Some(LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::WindowExpired,
            },
            new_state: ExperienceState::Expired,
            rule_id: "evidence_after_window",
        });
    }
    if evidence_delay < window.min_delay_micros {
        return Some(LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::AmbiguousResult {
                    description: "evidence arrived before minimum delay".to_string(),
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "evidence_too_early",
        });
    }

    None
}

const fn censor_exogenous_recovery(
    window: AttributionWindow,
    evidence: &ExecutionEvidence,
) -> Option<LabelingResult> {
    if evidence.exogenous_recovery && window.censor_on_exogenous_recovery {
        return Some(LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::ExogenousRecovery,
            },
            new_state: ExperienceState::Censored,
            rule_id: "exogenous_recovery",
        });
    }

    None
}

fn censor_pre_anchor_evidence(
    anchor_micros: i64,
    evidence: &ExecutionEvidence,
) -> Option<LabelingResult> {
    if evidence.observed_ts_micros < anchor_micros {
        return Some(LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::AmbiguousResult {
                    description: "evidence observed before attribution anchor".to_string(),
                },
            },
            new_state: ExperienceState::Censored,
            rule_id: "evidence_before_anchor",
        });
    }

    None
}

/// Label an experience from execution evidence using action-family-specific rules.
fn label_from_evidence(effect_kind: EffectKind, evidence: &ExecutionEvidence) -> LabelingResult {
    match effect_kind {
        EffectKind::Probe => label_probe(evidence),
        EffectKind::Advisory => label_advisory(evidence),
        EffectKind::Release => label_release(evidence),
        EffectKind::ForceReservation => label_force_reservation(evidence),
        EffectKind::RoutingSuggestion => label_routing(evidence),
        EffectKind::Backpressure => label_backpressure(evidence),
        EffectKind::NoAction => label_no_action(evidence),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Per-action-family labeling rules
// ──────────────────────────────────────────────────────────────────────

/// Probe labeling: both outcomes are Correct because the decision to probe
/// was always valid (entropy scheduling only probes uncertain agents). The
/// realized_loss differs between response (low) and no-response (high).
const fn label_probe(evidence: &ExecutionEvidence) -> LabelingResult {
    if evidence.effect_achieved {
        // Agent responded to probe
        LabelingResult {
            label: OutcomeLabel::Correct {
                realized_loss: evidence.realized_loss,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "probe_responded",
        }
    } else {
        // No response — probe revealed genuinely silent agent
        LabelingResult {
            label: OutcomeLabel::Correct {
                realized_loss: evidence.realized_loss,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "probe_no_response",
        }
    }
}

/// Advisory labeling: behavior change = success, no change = possible failure.
fn label_advisory(evidence: &ExecutionEvidence) -> LabelingResult {
    match evidence.situation_change {
        SituationChange::Improved => LabelingResult {
            label: OutcomeLabel::Success {
                realized_loss: evidence.realized_loss,
                confidence: 0.8, // advisories have indirect effect
            },
            new_state: ExperienceState::Resolved,
            rule_id: "advisory_improved",
        },
        SituationChange::Unchanged => LabelingResult {
            label: OutcomeLabel::Failure {
                realized_loss: evidence.realized_loss,
                confidence: 0.6, // no change might mean agent ignored or was already fine
                reason: "no observable behavior change after advisory".to_string(),
            },
            new_state: ExperienceState::Resolved,
            rule_id: "advisory_unchanged",
        },
        SituationChange::Worsened => LabelingResult {
            label: OutcomeLabel::Failure {
                realized_loss: evidence.realized_loss,
                confidence: 0.7,
                reason: "situation worsened after advisory".to_string(),
            },
            new_state: ExperienceState::Resolved,
            rule_id: "advisory_worsened",
        },
    }
}

/// Release labeling: binary — released agent was dead (correct) or alive (incorrect).
const fn label_release(evidence: &ExecutionEvidence) -> LabelingResult {
    if evidence.effect_achieved {
        // Released a genuinely inactive/dead agent
        LabelingResult {
            label: OutcomeLabel::Correct {
                realized_loss: evidence.realized_loss,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "release_correct",
        }
    } else {
        // FALSE POSITIVE: released an agent that was actually alive
        LabelingResult {
            label: OutcomeLabel::Incorrect {
                realized_loss: evidence.realized_loss,
                false_positive: true,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "release_false_positive",
        }
    }
}

/// Force reservation labeling.
const fn label_force_reservation(evidence: &ExecutionEvidence) -> LabelingResult {
    match evidence.situation_change {
        SituationChange::Improved => LabelingResult {
            label: OutcomeLabel::Correct {
                realized_loss: evidence.realized_loss,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "force_reservation_resolved",
        },
        SituationChange::Unchanged => LabelingResult {
            label: OutcomeLabel::Incorrect {
                realized_loss: evidence.realized_loss,
                false_positive: true,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "force_reservation_unnecessary",
        },
        SituationChange::Worsened => LabelingResult {
            label: OutcomeLabel::Incorrect {
                realized_loss: evidence.realized_loss,
                false_positive: true,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "force_reservation_harmful",
        },
    }
}

/// Routing suggestion labeling.
fn label_routing(evidence: &ExecutionEvidence) -> LabelingResult {
    match evidence.situation_change {
        SituationChange::Improved => LabelingResult {
            label: OutcomeLabel::Success {
                realized_loss: evidence.realized_loss,
                confidence: 0.7, // routing effects are indirect
            },
            new_state: ExperienceState::Resolved,
            rule_id: "routing_improved",
        },
        SituationChange::Unchanged => LabelingResult {
            label: OutcomeLabel::Failure {
                realized_loss: evidence.realized_loss,
                confidence: 0.5, // no change could mean suggestion was ignored
                reason: "load distribution unchanged after routing suggestion".to_string(),
            },
            new_state: ExperienceState::Resolved,
            rule_id: "routing_unchanged",
        },
        SituationChange::Worsened => LabelingResult {
            label: OutcomeLabel::Failure {
                realized_loss: evidence.realized_loss,
                confidence: 0.8,
                reason: "load worsened after routing suggestion".to_string(),
            },
            new_state: ExperienceState::Resolved,
            rule_id: "routing_worsened",
        },
    }
}

/// Backpressure labeling.
fn label_backpressure(evidence: &ExecutionEvidence) -> LabelingResult {
    match evidence.situation_change {
        SituationChange::Improved => LabelingResult {
            label: OutcomeLabel::Success {
                realized_loss: evidence.realized_loss,
                confidence: 0.8,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "backpressure_improved",
        },
        SituationChange::Unchanged => LabelingResult {
            label: OutcomeLabel::Failure {
                realized_loss: evidence.realized_loss,
                confidence: 0.6,
                reason: "queue depth unchanged after backpressure".to_string(),
            },
            new_state: ExperienceState::Resolved,
            rule_id: "backpressure_unchanged",
        },
        SituationChange::Worsened => LabelingResult {
            label: OutcomeLabel::Failure {
                realized_loss: evidence.realized_loss,
                confidence: 0.9,
                reason: "queue depth worsened after backpressure signal".to_string(),
            },
            new_state: ExperienceState::Resolved,
            rule_id: "backpressure_worsened",
        },
    }
}

/// No-action labeling: stable = correct inaction, worsened = missed opportunity.
const fn label_no_action(evidence: &ExecutionEvidence) -> LabelingResult {
    match evidence.situation_change {
        SituationChange::Improved | SituationChange::Unchanged => LabelingResult {
            label: OutcomeLabel::NonExecution {
                inaction_correct: true,
                realized_loss: evidence.realized_loss,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "no_action_stable",
        },
        SituationChange::Worsened => LabelingResult {
            label: OutcomeLabel::NonExecution {
                inaction_correct: false,
                realized_loss: evidence.realized_loss,
            },
            new_state: ExperienceState::Resolved,
            rule_id: "no_action_worsened",
        },
    }
}

// ──────────────────────────────────────────────────────────────────────
// Convenience: build ExperienceOutcome from label
// ──────────────────────────────────────────────────────────────────────

/// Convert a labeling result into an [`ExperienceOutcome`] for storage.
///
/// `observed_ts_micros` must be the actual timestamp when the outcome evidence
/// was observed, not when labeling happened to run.
#[must_use]
pub fn label_to_outcome(
    result: &LabelingResult,
    observed_ts_micros: i64,
) -> Option<ExperienceOutcome> {
    match &result.label {
        OutcomeLabel::Success {
            realized_loss,
            confidence,
        } => Some(ExperienceOutcome {
            observed_ts_micros,
            label: format!(
                "success (confidence={confidence:.2}, rule={})",
                result.rule_id
            ),
            correct: true,
            actual_loss: Some(*realized_loss),
            regret: None,
            evidence: None,
        }),
        OutcomeLabel::Failure {
            realized_loss,
            reason,
            confidence,
        } => Some(ExperienceOutcome {
            observed_ts_micros,
            label: format!(
                "failure: {reason} (confidence={confidence:.2}, rule={})",
                result.rule_id
            ),
            correct: false,
            actual_loss: Some(*realized_loss),
            regret: None,
            evidence: None,
        }),
        OutcomeLabel::Correct { realized_loss } => Some(ExperienceOutcome {
            observed_ts_micros,
            label: format!("correct (rule={})", result.rule_id),
            correct: true,
            actual_loss: Some(*realized_loss),
            regret: Some(0.0), // correct = zero regret
            evidence: None,
        }),
        OutcomeLabel::Incorrect {
            realized_loss,
            false_positive,
        } => {
            let fp_label = if *false_positive {
                "false_positive"
            } else {
                "false_negative"
            };
            Some(ExperienceOutcome {
                observed_ts_micros,
                label: format!("{fp_label} (rule={})", result.rule_id),
                correct: false,
                actual_loss: Some(*realized_loss),
                regret: None,
                evidence: None,
            })
        }
        OutcomeLabel::NonExecution {
            inaction_correct,
            realized_loss,
        } => Some(ExperienceOutcome {
            observed_ts_micros,
            label: format!(
                "non_execution_{} (rule={})",
                if *inaction_correct {
                    "correct"
                } else {
                    "incorrect"
                },
                result.rule_id,
            ),
            correct: *inaction_correct,
            actual_loss: Some(*realized_loss),
            regret: None,
            evidence: None,
        }),
        OutcomeLabel::Censored { .. } => {
            // Censored labels have no learning signal — return None so the
            // caller doesn't store a misleading outcome record.
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

    fn base_input() -> LabelingInput {
        LabelingInput {
            state: ExperienceState::Open,
            effect_kind: EffectKind::Probe,
            created_ts_micros: 1_000_000,
            executed_ts_micros: Some(1_500_000),
            now_micros: 10_000_000,
            non_execution_reason: None,
            concurrent_intervention_count: 0,
            subject_departed: false,
            operator_change: None,
            execution_evidence: None,
        }
    }

    fn success_evidence() -> ExecutionEvidence {
        ExecutionEvidence {
            observed_ts_micros: 5_000_000,
            effect_achieved: true,
            situation_change: SituationChange::Improved,
            exogenous_recovery: false,
            realized_loss: 0.5,
            best_possible_loss: 0.0,
        }
    }

    #[test]
    fn probe_success() {
        let mut input = base_input();
        input.execution_evidence = Some(success_evidence());
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert!(result.label.has_learning_signal());
        assert_eq!(result.rule_id, "probe_responded");
    }

    #[test]
    fn probe_no_response() {
        let mut input = base_input();
        let mut evidence = success_evidence();
        evidence.effect_achieved = false;
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert_eq!(result.rule_id, "probe_no_response");
    }

    #[test]
    fn evidence_within_window_still_labels_after_late_evaluation() {
        let mut input = base_input();
        input.now_micros = 90_000_000;
        input.execution_evidence = Some(success_evidence());

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert_eq!(result.rule_id, "probe_responded");
    }

    #[test]
    fn evidence_after_window_expires_even_if_present() {
        let mut input = base_input();
        input.execution_evidence = Some(ExecutionEvidence {
            observed_ts_micros: 70_000_000,
            effect_achieved: true,
            situation_change: SituationChange::Improved,
            exogenous_recovery: false,
            realized_loss: 0.5,
            best_possible_loss: 0.0,
        });

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Expired);
        assert_eq!(result.rule_id, "evidence_after_window");
        assert!(!result.label.has_learning_signal());
    }

    #[test]
    fn probe_evidence_before_execution_anchor_censors() {
        let mut input = base_input();
        let mut evidence = success_evidence();
        evidence.observed_ts_micros = 1_400_000;
        input.execution_evidence = Some(evidence);

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "evidence_before_anchor");
    }

    #[test]
    fn window_expired_censors() {
        let mut input = base_input();
        input.now_micros = 200_000_000; // well past 60s window
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Expired);
        assert!(!result.label.has_learning_signal());
        assert_eq!(result.rule_id, "window_expired");
    }

    #[test]
    fn subject_departed_censors() {
        let mut input = base_input();
        input.subject_departed = true;
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "subject_departed");
    }

    #[test]
    fn overlapping_interventions_censor() {
        let mut input = base_input();
        input.concurrent_intervention_count = 1; // probe has max=0
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "overlapping_interventions");
    }

    #[test]
    fn operator_change_censors() {
        let mut input = base_input();
        input.operator_change = Some("manual release override".to_string());
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "operator_change");
    }

    #[test]
    fn advisory_exogenous_recovery_censors() {
        let mut input = base_input();
        input.effect_kind = EffectKind::Advisory;
        input.executed_ts_micros = Some(1_000_000);
        let mut evidence = success_evidence();
        evidence.exogenous_recovery = true;
        evidence.observed_ts_micros = 10_000_000; // past min delay
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "exogenous_recovery");
    }

    #[test]
    fn release_false_positive() {
        let mut input = base_input();
        input.effect_kind = EffectKind::Release;
        let mut evidence = success_evidence();
        evidence.effect_achieved = false; // released alive agent
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert!(matches!(
            result.label,
            OutcomeLabel::Incorrect {
                false_positive: true,
                ..
            }
        ));
        assert_eq!(result.rule_id, "release_false_positive");
    }

    #[test]
    fn release_correct() {
        let mut input = base_input();
        input.effect_kind = EffectKind::Release;
        let mut evidence = success_evidence();
        evidence.effect_achieved = true; // released genuinely dead agent
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert!(matches!(result.label, OutcomeLabel::Correct { .. }));
    }

    #[test]
    fn deliberate_inaction_correct() {
        let mut input = base_input();
        input.state = ExperienceState::Skipped;
        input.effect_kind = EffectKind::NoAction;
        input.now_micros = 100_000_000; // 100s — well within 5min window
        input.non_execution_reason = Some(NonExecutionReason::DeliberateInaction {
            no_action_loss: 0.5,
            best_action_loss: 1.0,
        });
        let mut evidence = success_evidence();
        evidence.situation_change = SituationChange::Unchanged;
        evidence.observed_ts_micros = 50_000_000; // 50s — past 30s min delay
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert!(matches!(
            result.label,
            OutcomeLabel::NonExecution {
                inaction_correct: true,
                ..
            }
        ));
    }

    #[test]
    fn safety_gate_suppression_worsened() {
        let mut input = base_input();
        input.state = ExperienceState::Suppressed;
        input.effect_kind = EffectKind::Release;
        input.non_execution_reason = Some(NonExecutionReason::SafetyGate {
            gate_name: "conformal_uncertainty".to_string(),
            risk_score: 0.9,
            gate_threshold: 0.7,
        });
        let mut evidence = success_evidence();
        evidence.situation_change = SituationChange::Worsened;
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert!(matches!(
            result.label,
            OutcomeLabel::NonExecution {
                inaction_correct: false,
                ..
            }
        ));
    }

    #[test]
    fn non_execution_evidence_after_window_expires_even_if_present() {
        let mut input = base_input();
        input.state = ExperienceState::Suppressed;
        input.effect_kind = EffectKind::Release;
        input.non_execution_reason = Some(NonExecutionReason::SafetyGate {
            gate_name: "conformal_uncertainty".to_string(),
            risk_score: 0.9,
            gate_threshold: 0.7,
        });
        input.execution_evidence = Some(ExecutionEvidence {
            observed_ts_micros: 130_000_000,
            effect_achieved: false,
            situation_change: SituationChange::Unchanged,
            exogenous_recovery: false,
            realized_loss: 0.5,
            best_possible_loss: 0.0,
        });

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Expired);
        assert_eq!(result.rule_id, "evidence_after_window");
    }

    #[test]
    fn non_execution_evidence_too_early_censors() {
        let mut input = base_input();
        input.state = ExperienceState::Skipped;
        input.effect_kind = EffectKind::NoAction;
        input.non_execution_reason = Some(NonExecutionReason::DeliberateInaction {
            no_action_loss: 0.5,
            best_action_loss: 1.0,
        });
        input.execution_evidence = Some(ExecutionEvidence {
            observed_ts_micros: 5_000_000,
            effect_achieved: false,
            situation_change: SituationChange::Unchanged,
            exogenous_recovery: false,
            realized_loss: 0.2,
            best_possible_loss: 0.0,
        });

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "evidence_too_early");
    }

    #[test]
    fn non_execution_evidence_before_created_anchor_censors() {
        let mut input = base_input();
        input.state = ExperienceState::Suppressed;
        input.effect_kind = EffectKind::Release;
        input.non_execution_reason = Some(NonExecutionReason::SafetyGate {
            gate_name: "conformal_uncertainty".to_string(),
            risk_score: 0.9,
            gate_threshold: 0.7,
        });
        let mut evidence = success_evidence();
        evidence.observed_ts_micros = 900_000;
        input.execution_evidence = Some(evidence);

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "evidence_before_anchor");
    }

    #[test]
    fn non_execution_subject_departed_censors() {
        let mut input = base_input();
        input.state = ExperienceState::Skipped;
        input.effect_kind = EffectKind::NoAction;
        input.non_execution_reason = Some(NonExecutionReason::DeliberateInaction {
            no_action_loss: 0.5,
            best_action_loss: 1.0,
        });
        input.subject_departed = true;

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "subject_departed");
    }

    #[test]
    fn non_execution_missing_reason_censors_even_without_evidence() {
        let mut input = base_input();
        input.state = ExperienceState::Suppressed;
        input.effect_kind = EffectKind::Release;
        input.non_execution_reason = None;
        input.execution_evidence = None;
        input.now_micros = 10_000_000;

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "missing_non_execution_reason");
    }

    #[test]
    fn non_execution_exogenous_recovery_censors_when_family_requires_it() {
        let mut input = base_input();
        input.state = ExperienceState::Suppressed;
        input.effect_kind = EffectKind::Advisory;
        input.non_execution_reason = Some(NonExecutionReason::SafetyGate {
            gate_name: "calibration_guard".to_string(),
            risk_score: 0.9,
            gate_threshold: 0.7,
        });
        let mut evidence = success_evidence();
        evidence.exogenous_recovery = true;
        evidence.observed_ts_micros = 10_000_000;
        input.execution_evidence = Some(evidence);

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert!(matches!(
            result.label,
            OutcomeLabel::Censored {
                reason: CensorReason::ExogenousRecovery,
            }
        ));
        assert_eq!(result.rule_id, "exogenous_recovery");
    }

    #[test]
    fn deliberate_inaction_exogenous_recovery_still_resolves_for_no_action() {
        let mut input = base_input();
        input.state = ExperienceState::Skipped;
        input.effect_kind = EffectKind::NoAction;
        input.now_micros = 100_000_000;
        input.non_execution_reason = Some(NonExecutionReason::DeliberateInaction {
            no_action_loss: 0.5,
            best_action_loss: 1.0,
        });
        let mut evidence = success_evidence();
        evidence.exogenous_recovery = true;
        evidence.situation_change = SituationChange::Unchanged;
        evidence.observed_ts_micros = 50_000_000;
        input.execution_evidence = Some(evidence);

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Resolved);
        assert!(matches!(
            result.label,
            OutcomeLabel::NonExecution {
                inaction_correct: true,
                ..
            }
        ));
    }

    #[test]
    fn non_execution_operator_change_censors() {
        let mut input = base_input();
        input.state = ExperienceState::Suppressed;
        input.effect_kind = EffectKind::Release;
        input.non_execution_reason = Some(NonExecutionReason::SafetyGate {
            gate_name: "conformal_uncertainty".to_string(),
            risk_score: 0.9,
            gate_threshold: 0.7,
        });
        input.operator_change = Some("manual override".to_string());

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "operator_change");
    }

    #[test]
    fn non_execution_overlapping_interventions_censor() {
        let mut input = base_input();
        input.state = ExperienceState::Skipped;
        input.effect_kind = EffectKind::NoAction;
        input.non_execution_reason = Some(NonExecutionReason::DeliberateInaction {
            no_action_loss: 0.5,
            best_action_loss: 1.0,
        });
        input.concurrent_intervention_count = 1;

        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Censored);
        assert_eq!(result.rule_id, "overlapping_interventions");
    }

    #[test]
    fn failed_dispatch_censored() {
        let mut input = base_input();
        input.state = ExperienceState::Failed;
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Failed);
        assert!(!result.label.has_learning_signal());
    }

    #[test]
    fn no_action_worsened() {
        let mut input = base_input();
        input.effect_kind = EffectKind::NoAction;
        input.now_micros = 100_000_000; // 100s — well within 5min window
        let mut evidence = success_evidence();
        evidence.situation_change = SituationChange::Worsened;
        evidence.observed_ts_micros = 50_000_000; // 50s — past 30s min delay
        input.execution_evidence = Some(evidence);
        let result = label_experience(&input);
        assert!(matches!(
            result.label,
            OutcomeLabel::NonExecution {
                inaction_correct: false,
                ..
            }
        ));
    }

    #[test]
    fn attribution_windows_are_positive() {
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
            let window = attribution_window(kind);
            assert!(
                window.max_window_micros > 0,
                "attribution window for {kind} must be positive"
            );
            assert!(
                window.min_delay_micros >= 0,
                "min delay for {kind} must be non-negative"
            );
            assert!(
                window.max_window_micros > window.min_delay_micros,
                "max window must exceed min delay for {kind}"
            );
        }
    }

    #[test]
    fn high_force_effects_have_zero_overlap_tolerance() {
        for kind in [EffectKind::Release, EffectKind::ForceReservation] {
            let window = attribution_window(kind);
            assert_eq!(
                window.max_concurrent_interventions, 0,
                "high-force effect {kind} must have zero overlap tolerance"
            );
        }
    }

    #[test]
    fn label_to_outcome_roundtrip() {
        let result = LabelingResult {
            label: OutcomeLabel::Correct { realized_loss: 1.5 },
            new_state: ExperienceState::Resolved,
            rule_id: "test_rule",
        };
        let outcome = label_to_outcome(&result, 42_000_000);
        assert!(outcome.is_some());
        let outcome = outcome.unwrap();
        assert_eq!(outcome.observed_ts_micros, 42_000_000);
        assert!(outcome.correct);
        assert_eq!(outcome.actual_loss, Some(1.5));
        assert_eq!(outcome.regret, Some(0.0));
    }

    #[test]
    fn censored_label_no_learning_signal() {
        let label = OutcomeLabel::Censored {
            reason: CensorReason::WindowExpired,
        };
        assert!(!label.has_learning_signal());
        assert!(label.realized_loss().is_none());
    }

    #[test]
    fn outcome_label_realized_loss_extraction() {
        assert_eq!(
            OutcomeLabel::Success {
                realized_loss: 1.0,
                confidence: 0.9
            }
            .realized_loss(),
            Some(1.0)
        );
        assert_eq!(
            OutcomeLabel::Failure {
                realized_loss: 2.0,
                confidence: 0.8,
                reason: "test".to_string()
            }
            .realized_loss(),
            Some(2.0)
        );
        assert_eq!(
            OutcomeLabel::Correct { realized_loss: 0.5 }.realized_loss(),
            Some(0.5)
        );
    }

    #[test]
    fn determinism_same_input_same_output() {
        let input = base_input();
        let r1 = label_experience(&input);
        let r2 = label_experience(&input);
        assert_eq!(r1.label, r2.label);
        assert_eq!(r1.new_state, r2.new_state);
        assert_eq!(r1.rule_id, r2.rule_id);
    }

    #[test]
    fn planned_state_stays_planned() {
        let mut input = base_input();
        input.state = ExperienceState::Planned;
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Planned);
        assert_eq!(result.rule_id, "pre_execution");
        assert!(!result.label.has_learning_signal());
    }

    #[test]
    fn dispatched_state_stays_dispatched() {
        let mut input = base_input();
        input.state = ExperienceState::Dispatched;
        let result = label_experience(&input);
        assert_eq!(result.new_state, ExperienceState::Dispatched);
        assert_eq!(result.rule_id, "pre_execution");
    }

    #[test]
    fn censored_label_to_outcome_returns_none() {
        let result = LabelingResult {
            label: OutcomeLabel::Censored {
                reason: CensorReason::WindowExpired,
            },
            new_state: ExperienceState::Expired,
            rule_id: "window_expired",
        };
        assert!(label_to_outcome(&result, 42_000_000).is_none());
    }

    #[test]
    fn censor_reason_display() {
        assert_eq!(
            CensorReason::WindowExpired.to_string(),
            "attribution window expired"
        );
        assert_eq!(
            CensorReason::OverlappingInterventions {
                intervention_count: 3
            }
            .to_string(),
            "3 overlapping interventions"
        );
    }
}
