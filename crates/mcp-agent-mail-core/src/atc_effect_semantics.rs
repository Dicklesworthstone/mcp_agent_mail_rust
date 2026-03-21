//! Effect semantics enforcement: preconditions, cooldown, escalation, and
//! semantic message rendering for ATC effects (br-0qt6e.2.7).
//!
//! This module tightens the ATC effect layer so that every effect family
//! has enforceable safety rules, suppression for repeated low-value effects,
//! and operator-facing content that is concrete rather than generic.
//!
//! # Design Principle
//!
//! A bad action family with a perfect learner is still a bad system.
//! This module makes the actions themselves worth learning over by
//! enforcing their safety rules and utility contracts.
//!
//! # Effect Classification
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────────┐
//!  │  Low-Risk Nudge                                          │
//!  │  - Advisory, withheld_release_notice                     │
//!  │  - Can repeat (with cooldown)                           │
//!  │  - No ack required                                      │
//!  │  - Learning: optimize for relevance, not intervention   │
//!  ├─────────────────────────────────────────────────────────┤
//!  │  Medium-Risk Check                                       │
//!  │  - Probe, deadlock_remediation, release_notice          │
//!  │  - Cooldown enforced, ack may be required               │
//!  │  - Suppressed if same family fired recently             │
//!  │  - Learning: optimize for accuracy and timeliness       │
//!  ├─────────────────────────────────────────────────────────┤
//!  │  High-Risk Intervention                                  │
//!  │  - reservation_release                                   │
//!  │  - No repetition (per-agent per-regime)                 │
//!  │  - Requires multiple independent evidence sources       │
//!  │  - Learning: optimize for zero false positives          │
//!  └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Escalation Ladder
//!
//! ```text
//!  advisory → probe → release
//!     │          │         │
//!     │          │         └─ Terminal: no further automatic escalation
//!     │          └─ After unanswered probe, consider release
//!     └─ After repeated advisory, escalate to probe
//! ```

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::experience::EffectKind;

// ──────────────────────────────────────────────────────────────────────
// Effect risk classification
// ──────────────────────────────────────────────────────────────────────

/// Risk classification for an effect family.
///
/// Determines what safety rules, suppression policies, and learning
/// objectives apply to effects in this class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectRiskClass {
    /// Low-risk nudge: advisory messages, withheld-release notices.
    /// Can repeat with cooldown. Optimized for relevance.
    LowRiskNudge,
    /// Medium-risk check: probes, deadlock remediation, release notices.
    /// Cooldown enforced. May require ack. Optimized for accuracy.
    MediumRiskCheck,
    /// High-risk intervention: reservation releases.
    /// No repetition. Requires independent evidence. Zero false positives.
    HighRiskIntervention,
}

impl EffectRiskClass {
    /// Classify an effect family by name.
    #[must_use]
    pub fn for_family(family: &str) -> Self {
        match family {
            "liveness_monitoring" | "withheld_release_notice" => Self::LowRiskNudge,
            "reservation_release" => Self::HighRiskIntervention,
            _ => Self::MediumRiskCheck, // unknown defaults to medium
        }
    }

    /// Classify an `EffectKind` into a risk class.
    #[must_use]
    pub const fn for_effect_kind(kind: EffectKind) -> Self {
        match kind {
            EffectKind::Advisory | EffectKind::RoutingSuggestion | EffectKind::NoAction => {
                Self::LowRiskNudge
            }
            EffectKind::Probe | EffectKind::Backpressure => Self::MediumRiskCheck,
            EffectKind::Release | EffectKind::ForceReservation => Self::HighRiskIntervention,
        }
    }

    /// Maximum allowed repetitions within a cooldown window.
    #[must_use]
    pub const fn max_repetitions_in_window(self) -> u32 {
        match self {
            Self::LowRiskNudge => 3,         // can repeat up to 3 times
            Self::MediumRiskCheck => 1,      // once per cooldown window
            Self::HighRiskIntervention => 0, // never repeat (per agent per regime)
        }
    }

    /// Whether this class requires all preconditions to be verified.
    #[must_use]
    pub const fn requires_precondition_check(self) -> bool {
        true // all classes require precondition checks
    }

    /// Whether this class should be suppressed when safe mode is active.
    #[must_use]
    pub const fn suppressed_in_safe_mode(self) -> bool {
        matches!(self, Self::HighRiskIntervention)
    }
}

impl std::fmt::Display for EffectRiskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LowRiskNudge => write!(f, "low_risk_nudge"),
            Self::MediumRiskCheck => write!(f, "medium_risk_check"),
            Self::HighRiskIntervention => write!(f, "high_risk_intervention"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Precondition checking
// ──────────────────────────────────────────────────────────────────────

/// Runtime context for precondition checking.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct PreconditionContext {
    /// Whether the project context is available.
    pub has_project_context: bool,
    /// Whether the agent has active reservations.
    pub agent_has_reservations: bool,
    /// Whether a deadlock cycle was detected.
    pub deadlock_cycle_present: bool,
    /// Whether the agent is marked for release in this tick.
    pub release_in_this_tick: bool,
    /// Whether a paired reservation-release was emitted.
    pub paired_release_emitted: bool,
    /// Whether release was withheld by a safety gate.
    pub release_withheld_by_gate: bool,
    /// Whether safe mode is active.
    pub safe_mode_active: bool,
    /// Whether the liveness evidence supports this action.
    pub liveness_evidence_supports: bool,
    /// Whether calibration gates allow the action.
    pub calibration_gates_allow: bool,
}

/// Result of precondition validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreconditionResult {
    /// Whether all preconditions passed.
    pub passed: bool,
    /// Total preconditions checked.
    pub total_checked: u32,
    /// Number that passed.
    pub passed_count: u32,
    /// Failures with descriptions.
    pub failures: Vec<PreconditionFailure>,
}

/// A single precondition failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreconditionFailure {
    /// Which precondition failed.
    pub precondition: String,
    /// Why it failed.
    pub reason: String,
}

/// Validate preconditions for an effect family.
///
/// Returns a `PreconditionResult` documenting which checks passed
/// and which failed. The effect should only proceed if `result.passed`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn validate_preconditions(family: &str, context: &PreconditionContext) -> PreconditionResult {
    let mut failures = Vec::new();
    let mut total = 0u32;
    let mut passed = 0u32;

    // Gate: safe mode blocks high-risk interventions.
    let risk_class = EffectRiskClass::for_family(family);
    total += 1;
    if risk_class.suppressed_in_safe_mode() && context.safe_mode_active {
        failures.push(PreconditionFailure {
            precondition: "safe_mode_inactive".to_string(),
            reason: "safe mode blocks high-risk interventions".to_string(),
        });
    } else {
        passed += 1;
    }

    match family {
        "liveness_monitoring" => {
            // Project context required.
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: "no project context for ATC mail delivery".to_string(),
                });
            }
            // Must not be a confirmed dead-agent release.
            total += 1;
            if context.release_in_this_tick {
                failures.push(PreconditionFailure {
                    precondition: "not_confirmed_release".to_string(),
                    reason: "agent already marked for release; advisory is redundant".to_string(),
                });
            } else {
                passed += 1;
            }
            // Liveness evidence must support inactivity suspicion.
            total += 1;
            if context.liveness_evidence_supports {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "liveness_evidence_supports".to_string(),
                    reason: "no liveness evidence supporting inactivity suspicion".to_string(),
                });
            }
        }
        "deadlock_remediation" => {
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: "no project context for deadlock remediation".to_string(),
                });
            }
            total += 1;
            if context.deadlock_cycle_present {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "deadlock_cycle_present".to_string(),
                    reason: "no active deadlock cycle; remediation message would be noise"
                        .to_string(),
                });
            }
        }
        "liveness_probe" => {
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: "no project context for probe delivery".to_string(),
                });
            }
            total += 1;
            if context.release_in_this_tick {
                failures.push(PreconditionFailure {
                    precondition: "not_releasing_this_tick".to_string(),
                    reason: "agent marked for release; probing is redundant".to_string(),
                });
            } else {
                passed += 1;
            }
        }
        "reservation_release" => {
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: "no project context for reservation release".to_string(),
                });
            }
            total += 1;
            if context.liveness_evidence_supports {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "liveness_supports_release".to_string(),
                    reason: "liveness evidence does not support release".to_string(),
                });
            }
            total += 1;
            if context.calibration_gates_allow {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "calibration_gates_allow".to_string(),
                    reason: "calibration or safety gates have withheld release".to_string(),
                });
            }
            total += 1;
            if context.agent_has_reservations {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "agent_has_reservations".to_string(),
                    reason: "agent holds no reservations; release is a no-op".to_string(),
                });
            }
        }
        "release_notice" => {
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: "no project context for release notice".to_string(),
                });
            }
            total += 1;
            if context.paired_release_emitted {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "paired_release_emitted".to_string(),
                    reason: "release notice without paired release effect".to_string(),
                });
            }
        }
        "withheld_release_notice" => {
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: "no project context for withheld notice".to_string(),
                });
            }
            total += 1;
            if context.release_withheld_by_gate {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "release_withheld_by_gate".to_string(),
                    reason: "withheld notice without an active gate withholding release"
                        .to_string(),
                });
            }
        }
        _ => {
            // Unknown families: require project context only.
            total += 1;
            if context.has_project_context {
                passed += 1;
            } else {
                failures.push(PreconditionFailure {
                    precondition: "project_context_available".to_string(),
                    reason: format!("unknown family '{family}' requires project context"),
                });
            }
        }
    }

    PreconditionResult {
        passed: failures.is_empty(),
        total_checked: total,
        passed_count: passed,
        failures,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cooldown tracker
// ──────────────────────────────────────────────────────────────────────

/// Entry in the cooldown tracker.
#[derive(Debug, Clone)]
struct CooldownEntry {
    /// Last emission timestamp (microseconds since epoch).
    last_ts_micros: i64,
    /// Number of emissions in the current cooldown window.
    emissions_in_window: u32,
}

/// Tracks cooldown state for effect families, preventing repeated
/// low-value effects from becoming spam.
#[derive(Debug, Clone, Default)]
pub struct CooldownTracker {
    entries: HashMap<String, CooldownEntry>,
}

/// Result of a cooldown check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooldownVerdict {
    /// Effect is allowed to proceed.
    Allowed,
    /// Effect is suppressed due to cooldown.
    Suppressed {
        /// How long until the cooldown expires (microseconds).
        remaining_micros: i64,
        /// How many emissions have occurred in this window.
        emissions_in_window: u32,
        /// Maximum allowed in this window.
        max_in_window: u32,
    },
}

impl CooldownTracker {
    /// Create a new empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether an effect is allowed given its cooldown key and current time.
    ///
    /// This does NOT record the emission — call `record_emission` if the
    /// effect actually executes.
    #[must_use]
    pub fn check(
        &self,
        cooldown_key: &str,
        now_micros: i64,
        cooldown_micros: i64,
        family: &str,
    ) -> CooldownVerdict {
        let risk_class = EffectRiskClass::for_family(family);
        let max_reps = risk_class.max_repetitions_in_window();
        let active_cooldown_micros = cooldown_micros.max(0);

        if let Some(entry) = self.entries.get(cooldown_key) {
            // For effects with max_reps == 0 (e.g., HighRiskIntervention),
            // ANY prior emission means suppression, regardless of window.
            if max_reps == 0 && entry.emissions_in_window > 0 {
                // Structural suppression: max_reps == 0 means "never repeat",
                // so remaining_micros is 0 (suppression is permanent, not
                // time-based). Callers should check max_in_window == 0 to
                // distinguish this from a time-based cooldown that just expired.
                return CooldownVerdict::Suppressed {
                    remaining_micros: 0,
                    emissions_in_window: entry.emissions_in_window,
                    max_in_window: 0,
                };
            }

            let elapsed = now_micros.saturating_sub(entry.last_ts_micros);
            if elapsed < active_cooldown_micros {
                // Still within cooldown window.
                if entry.emissions_in_window >= max_reps {
                    return CooldownVerdict::Suppressed {
                        remaining_micros: active_cooldown_micros - elapsed,
                        emissions_in_window: entry.emissions_in_window,
                        max_in_window: max_reps,
                    };
                }
            }
            // Window has expired — will reset on record.
        }

        CooldownVerdict::Allowed
    }

    /// Record that an effect was emitted.
    pub fn record_emission(
        &mut self,
        cooldown_key: &str,
        now_micros: i64,
        cooldown_micros: i64,
        _family: &str,
    ) {
        let active_cooldown_micros = cooldown_micros.max(0);
        let entry = self
            .entries
            .entry(cooldown_key.to_string())
            .or_insert(CooldownEntry {
                last_ts_micros: 0,
                emissions_in_window: 0,
            });

        let elapsed = now_micros.saturating_sub(entry.last_ts_micros);
        if elapsed >= active_cooldown_micros {
            // Window expired — reset counter.
            entry.emissions_in_window = 1;
        } else {
            entry.emissions_in_window += 1;
        }
        entry.last_ts_micros = now_micros;
    }

    /// Number of tracked cooldown keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tracker is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict entries older than the given threshold (microseconds).
    pub fn evict_older_than(&mut self, threshold_micros: i64) {
        self.entries
            .retain(|_, e| e.last_ts_micros >= threshold_micros);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Escalation state machine
// ──────────────────────────────────────────────────────────────────────

/// The current escalation level for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationLevel {
    /// No escalation active. Normal monitoring.
    None,
    /// Advisory level: low-risk nudges have been sent.
    Advisory,
    /// Probe level: acknowledgment-requiring probes have been sent.
    Probe,
    /// Release level: reservation release has been requested.
    Release,
}

impl EscalationLevel {
    /// The next escalation level in the ladder.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::None => Self::Advisory,
            Self::Advisory => Self::Probe,
            Self::Probe | Self::Release => Self::Release, // terminal
        }
    }

    /// Whether this is the terminal escalation level.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Release)
    }

    /// Number of unanswered effects at the current level before
    /// escalation to the next level.
    #[must_use]
    pub const fn escalation_threshold(self) -> u32 {
        match self {
            Self::Advisory => 3, // 3 unanswered advisories → escalate to probe
            Self::Probe => 2,    // 2 unanswered probes → escalate to release
            Self::None | Self::Release => 0,  // first advisory is free, terminal has no escalation
        }
    }
}

impl std::fmt::Display for EscalationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Advisory => write!(f, "advisory"),
            Self::Probe => write!(f, "probe"),
            Self::Release => write!(f, "release"),
        }
    }
}

/// Per-agent escalation state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationState {
    /// Current escalation level.
    pub level: EscalationLevel,
    /// How many effects at the current level went unanswered.
    pub unanswered_at_level: u32,
    /// When the current level was entered (microseconds).
    pub level_entered_ts_micros: i64,
    /// When the last effect was emitted (microseconds).
    pub last_effect_ts_micros: i64,
    /// Total escalations for this agent since inception.
    pub total_escalations: u32,
}

impl EscalationState {
    /// Create a fresh escalation state for a new agent.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            level: EscalationLevel::None,
            unanswered_at_level: 0,
            level_entered_ts_micros: 0,
            last_effect_ts_micros: 0,
            total_escalations: 0,
        }
    }

    /// Record that an effect was emitted at the current level.
    pub const fn record_effect(&mut self, now_micros: i64) {
        self.unanswered_at_level += 1;
        self.last_effect_ts_micros = now_micros;
    }

    /// Record that the agent acknowledged/responded (resets escalation).
    pub const fn record_acknowledgment(&mut self) {
        self.level = EscalationLevel::None;
        self.unanswered_at_level = 0;
    }

    /// Check whether escalation is warranted and advance if so.
    /// Returns the new level if escalation occurred.
    pub const fn maybe_escalate(&mut self, now_micros: i64) -> Option<EscalationLevel> {
        if self.level.is_terminal() {
            return None;
        }

        let threshold = self.level.escalation_threshold();
        if self.unanswered_at_level >= threshold && threshold > 0 {
            let new_level = self.level.next();
            self.level = new_level;
            self.unanswered_at_level = 0;
            self.level_entered_ts_micros = now_micros;
            self.total_escalations += 1;
            Some(new_level)
        } else {
            None
        }
    }

    /// The recommended effect family for the current escalation level.
    #[must_use]
    pub const fn recommended_family(&self) -> &'static str {
        match self.level {
            EscalationLevel::None | EscalationLevel::Advisory => "liveness_monitoring",
            EscalationLevel::Probe => "liveness_probe",
            EscalationLevel::Release => "reservation_release",
        }
    }
}

impl Default for EscalationState {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Semantic message builder
// ──────────────────────────────────────────────────────────────────────

/// Structured components of an effect message.
///
/// Instead of a single generic message string, effect messages are
/// built from semantic components so operators get concrete, actionable
/// content with evidence pointers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMessage {
    /// Concise subject line (max 80 chars).
    pub subject: String,
    /// Evidence summary: what signal triggered this effect.
    pub evidence: String,
    /// Operator action: what the recipient should do.
    pub action: String,
    /// Remediation: how to resolve the underlying issue.
    pub remediation: String,
    /// Escalation notice: what happens if this is ignored.
    pub escalation: String,
    /// Rendered body (all components combined).
    pub rendered_body: String,
}

/// Build a semantic message from effect components.
#[must_use]
pub fn build_semantic_message(
    family: &str,
    agent: &str,
    evidence_summary: &str,
    operator_action: &str,
    remediation: &str,
    escalation_policy: &str,
    escalation_state: &EscalationState,
) -> SemanticMessage {
    let subject = match family {
        "liveness_monitoring" => format!("[ATC advisory] {agent}: inactivity detected"),
        "deadlock_remediation" => format!("[ATC deadlock] {agent}: reservation cycle detected"),
        "liveness_probe" => format!("[ATC probe] {agent}: acknowledgment requested"),
        "reservation_release" => format!("[ATC release] {agent}: reservations released"),
        "release_notice" => format!("[ATC notice] {agent}: reservations were released by ATC"),
        "withheld_release_notice" => {
            format!("[ATC withheld] {agent}: release deferred, manual check suggested")
        }
        _ => format!("[ATC] {agent}: {family}"),
    };

    let escalation_notice = if escalation_state.level.is_terminal() {
        "No further automatic escalation. Manual review recommended.".to_string()
    } else {
        let next = escalation_state.level.next();
        let threshold = escalation_state.level.escalation_threshold();
        let remaining = threshold.saturating_sub(escalation_state.unanswered_at_level);
        format!(
            "If unresolved, ATC will escalate to {next} after {remaining} more unanswered effect(s). Policy: {escalation_policy}",
        )
    };

    let rendered_body = format!(
        "**Evidence:** {evidence_summary}\n\n\
         **Action:** {operator_action}\n\n\
         **Remediation:** {remediation}\n\n\
         **Escalation:** {escalation_notice}"
    );

    SemanticMessage {
        subject,
        evidence: evidence_summary.to_string(),
        action: operator_action.to_string(),
        remediation: remediation.to_string(),
        escalation: escalation_notice,
        rendered_body,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Effect family utility model
// ──────────────────────────────────────────────────────────────────────

/// The intended utility model for each effect family.
///
/// This makes the learning objective explicit so the outcome resolver
/// and adaptation engine can optimize the right metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectUtilityModel {
    /// Effect family name.
    pub family: &'static str,
    /// What benefit this effect provides to operators/agents.
    pub utility: &'static str,
    /// What constitutes a successful outcome for this effect.
    pub success_criterion: &'static str,
    /// What constitutes a harmful outcome for this effect.
    pub harm_criterion: &'static str,
    /// Whether this family is worth optimizing (vs. constraining).
    pub optimization_target: OptimizationTarget,
}

/// Whether the learning system should optimize or constrain this family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizationTarget {
    /// Optimize for relevance: maximize true-positive rate.
    Relevance,
    /// Optimize for accuracy: minimize both false positives and negatives.
    Accuracy,
    /// Constrain: minimize false positives (even at cost of false negatives).
    ZeroFalsePositive,
}

impl std::fmt::Display for OptimizationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relevance => write!(f, "relevance"),
            Self::Accuracy => write!(f, "accuracy"),
            Self::ZeroFalsePositive => write!(f, "zero_false_positive"),
        }
    }
}

/// Canonical utility models for all known effect families.
pub const EFFECT_UTILITY_MODELS: &[EffectUtilityModel] = &[
    EffectUtilityModel {
        family: "liveness_monitoring",
        utility: "Alert operators to suspicious inactivity before it becomes a blocking issue",
        success_criterion: "Advisory sent to an agent that was genuinely inactive and the operator benefited from the alert",
        harm_criterion: "Advisory sent to an actively working agent, creating noise and eroding trust",
        optimization_target: OptimizationTarget::Relevance,
    },
    EffectUtilityModel {
        family: "deadlock_remediation",
        utility: "Surface deterministic reservation deadlock cycles with concrete cleanup paths",
        success_criterion: "Deadlock cycle correctly identified and the operator resolved it using the provided evidence",
        harm_criterion: "False deadlock report or report for a cycle that resolved itself before the operator saw it",
        optimization_target: OptimizationTarget::Accuracy,
    },
    EffectUtilityModel {
        family: "liveness_probe",
        utility: "Separate stale sessions from active work before resorting to reservation release",
        success_criterion: "Probe sent to an inactive agent, eliciting either a response (alive) or silence (confirming inactivity)",
        harm_criterion: "Probe sent to an actively working agent, interrupting their flow",
        optimization_target: OptimizationTarget::Accuracy,
    },
    EffectUtilityModel {
        family: "reservation_release",
        utility: "Clear stale reservations held by dead agents to unblock other agents",
        success_criterion: "Reservations released for a genuinely inactive agent, unblocking waiting agents",
        harm_criterion: "Reservations released for an active agent, destroying their work in progress",
        optimization_target: OptimizationTarget::ZeroFalsePositive,
    },
    EffectUtilityModel {
        family: "release_notice",
        utility: "Make automated release visible so affected agents can recover explicitly",
        success_criterion: "Notice delivered to an agent whose reservations were released, enabling re-reservation if needed",
        harm_criterion: "Notice delivered without a paired release (confusing) or to an agent that already departed",
        optimization_target: OptimizationTarget::Accuracy,
    },
    EffectUtilityModel {
        family: "withheld_release_notice",
        utility: "Explain why release was deferred and what softer step was taken instead",
        success_criterion: "Operator understands why ATC chose not to release and can take manual action if warranted",
        harm_criterion: "Notice creates confusion or suggests action when no intervention is needed",
        optimization_target: OptimizationTarget::Relevance,
    },
];

// ──────────────────────────────────────────────────────────────────────
// Noise suppression rules
// ──────────────────────────────────────────────────────────────────────

/// Per-agent, per-hour noise limits.
pub const MAX_ADVISORIES_PER_AGENT_PER_HOUR: u32 = 10;

/// Maximum toasts per minute (across all agents).
pub const MAX_TOASTS_PER_MINUTE: u32 = 3;

/// Escalated cooldown (microseconds) when noise budget is exceeded.
pub const NOISE_ESCALATION_COOLDOWN_MICROS: i64 = 600_000_000; // 10 minutes

/// Check whether the per-agent advisory noise budget is exceeded.
#[must_use]
pub const fn is_advisory_noise_exceeded(advisories_in_last_hour: u32) -> bool {
    advisories_in_last_hour >= MAX_ADVISORIES_PER_AGENT_PER_HOUR
}

/// Check whether the global toast rate is exceeded.
#[must_use]
pub const fn is_toast_rate_exceeded(toasts_in_last_minute: u32) -> bool {
    toasts_in_last_minute >= MAX_TOASTS_PER_MINUTE
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Risk classification ──

    #[test]
    fn test_risk_class_for_known_families() {
        assert_eq!(
            EffectRiskClass::for_family("liveness_monitoring"),
            EffectRiskClass::LowRiskNudge
        );
        assert_eq!(
            EffectRiskClass::for_family("withheld_release_notice"),
            EffectRiskClass::LowRiskNudge
        );
        assert_eq!(
            EffectRiskClass::for_family("liveness_probe"),
            EffectRiskClass::MediumRiskCheck
        );
        assert_eq!(
            EffectRiskClass::for_family("deadlock_remediation"),
            EffectRiskClass::MediumRiskCheck
        );
        assert_eq!(
            EffectRiskClass::for_family("reservation_release"),
            EffectRiskClass::HighRiskIntervention
        );
    }

    #[test]
    fn test_risk_class_for_effect_kind() {
        assert_eq!(
            EffectRiskClass::for_effect_kind(EffectKind::Advisory),
            EffectRiskClass::LowRiskNudge
        );
        assert_eq!(
            EffectRiskClass::for_effect_kind(EffectKind::Probe),
            EffectRiskClass::MediumRiskCheck
        );
        assert_eq!(
            EffectRiskClass::for_effect_kind(EffectKind::Release),
            EffectRiskClass::HighRiskIntervention
        );
    }

    #[test]
    fn test_risk_class_unknown_defaults_medium() {
        assert_eq!(
            EffectRiskClass::for_family("unknown_family"),
            EffectRiskClass::MediumRiskCheck
        );
    }

    #[test]
    fn test_safe_mode_suppresses_high_risk_only() {
        assert!(EffectRiskClass::HighRiskIntervention.suppressed_in_safe_mode());
        assert!(!EffectRiskClass::MediumRiskCheck.suppressed_in_safe_mode());
        assert!(!EffectRiskClass::LowRiskNudge.suppressed_in_safe_mode());
    }

    // ── Precondition checking ──

    fn default_context() -> PreconditionContext {
        PreconditionContext {
            has_project_context: true,
            agent_has_reservations: true,
            deadlock_cycle_present: false,
            release_in_this_tick: false,
            paired_release_emitted: false,
            release_withheld_by_gate: false,
            safe_mode_active: false,
            liveness_evidence_supports: true,
            calibration_gates_allow: true,
        }
    }

    #[test]
    fn test_preconditions_pass_for_advisory() {
        let ctx = default_context();
        let result = validate_preconditions("liveness_monitoring", &ctx);
        assert!(result.passed, "Expected pass: {:?}", result.failures);
    }

    #[test]
    fn test_preconditions_fail_advisory_no_project() {
        let mut ctx = default_context();
        ctx.has_project_context = false;
        let result = validate_preconditions("liveness_monitoring", &ctx);
        assert!(!result.passed);
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.precondition == "project_context_available")
        );
    }

    #[test]
    fn test_preconditions_fail_advisory_release_in_tick() {
        let mut ctx = default_context();
        ctx.release_in_this_tick = true;
        let result = validate_preconditions("liveness_monitoring", &ctx);
        assert!(!result.passed);
    }

    #[test]
    fn test_preconditions_fail_deadlock_no_cycle() {
        let ctx = default_context();
        let result = validate_preconditions("deadlock_remediation", &ctx);
        assert!(!result.passed);
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.precondition == "deadlock_cycle_present")
        );
    }

    #[test]
    fn test_preconditions_pass_deadlock_with_cycle() {
        let mut ctx = default_context();
        ctx.deadlock_cycle_present = true;
        let result = validate_preconditions("deadlock_remediation", &ctx);
        assert!(result.passed);
    }

    #[test]
    fn test_preconditions_fail_release_safe_mode() {
        let mut ctx = default_context();
        ctx.safe_mode_active = true;
        let result = validate_preconditions("reservation_release", &ctx);
        assert!(!result.passed);
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.precondition == "safe_mode_inactive")
        );
    }

    #[test]
    fn test_preconditions_fail_release_no_reservations() {
        let mut ctx = default_context();
        ctx.agent_has_reservations = false;
        let result = validate_preconditions("reservation_release", &ctx);
        assert!(!result.passed);
    }

    #[test]
    fn test_preconditions_fail_release_notice_no_paired() {
        let ctx = default_context();
        let result = validate_preconditions("release_notice", &ctx);
        assert!(!result.passed);
    }

    #[test]
    fn test_preconditions_pass_release_notice_with_paired() {
        let mut ctx = default_context();
        ctx.paired_release_emitted = true;
        let result = validate_preconditions("release_notice", &ctx);
        assert!(result.passed);
    }

    #[test]
    fn test_preconditions_fail_withheld_no_gate() {
        let ctx = default_context();
        let result = validate_preconditions("withheld_release_notice", &ctx);
        assert!(!result.passed);
    }

    #[test]
    fn test_preconditions_pass_withheld_with_gate() {
        let mut ctx = default_context();
        ctx.release_withheld_by_gate = true;
        let result = validate_preconditions("withheld_release_notice", &ctx);
        assert!(result.passed);
    }

    // ── Cooldown tracker ──

    #[test]
    fn test_cooldown_allows_first_emission() {
        let tracker = CooldownTracker::new();
        let verdict = tracker.check(
            "liveness_monitoring:proj:Agent",
            1_000_000,
            60_000_000,
            "liveness_monitoring",
        );
        assert_eq!(verdict, CooldownVerdict::Allowed);
    }

    #[test]
    fn test_cooldown_suppresses_within_window() {
        let mut tracker = CooldownTracker::new();
        let key = "liveness_probe:proj:Agent";
        // Record first emission.
        tracker.record_emission(key, 1_000_000, 60_000_000, "liveness_probe");
        // Check again within window — probe is medium risk (max 1 in window).
        let verdict = tracker.check(key, 2_000_000, 60_000_000, "liveness_probe");
        assert!(matches!(verdict, CooldownVerdict::Suppressed { .. }));
    }

    #[test]
    fn test_cooldown_allows_after_window_expires() {
        let mut tracker = CooldownTracker::new();
        let key = "liveness_probe:proj:Agent";
        tracker.record_emission(key, 1_000_000, 60_000_000, "liveness_probe");
        // Check after window expires.
        let verdict = tracker.check(key, 70_000_000, 60_000_000, "liveness_probe");
        assert_eq!(verdict, CooldownVerdict::Allowed);
    }

    #[test]
    fn test_cooldown_low_risk_allows_multiple() {
        let mut tracker = CooldownTracker::new();
        let key = "liveness_monitoring:proj:Agent";
        // Low risk nudges allow up to 3 in window.
        tracker.record_emission(key, 1_000_000, 60_000_000, "liveness_monitoring");
        let verdict = tracker.check(key, 2_000_000, 60_000_000, "liveness_monitoring");
        assert_eq!(verdict, CooldownVerdict::Allowed);
        tracker.record_emission(key, 2_000_000, 60_000_000, "liveness_monitoring");
        let verdict = tracker.check(key, 3_000_000, 60_000_000, "liveness_monitoring");
        assert_eq!(verdict, CooldownVerdict::Allowed);
        tracker.record_emission(key, 3_000_000, 60_000_000, "liveness_monitoring");
        // 4th should be suppressed.
        let verdict = tracker.check(key, 4_000_000, 60_000_000, "liveness_monitoring");
        assert!(matches!(verdict, CooldownVerdict::Suppressed { .. }));
    }

    #[test]
    fn test_cooldown_high_risk_always_suppresses_repeat() {
        let mut tracker = CooldownTracker::new();
        let key = "reservation_release:proj:Agent";
        tracker.record_emission(key, 1_000_000, 60_000_000, "reservation_release");
        // High risk: max 0 repetitions in window.
        let verdict = tracker.check(key, 2_000_000, 60_000_000, "reservation_release");
        assert!(matches!(verdict, CooldownVerdict::Suppressed { .. }));
    }

    #[test]
    fn test_cooldown_high_risk_suppresses_even_with_zero_cooldown() {
        let mut tracker = CooldownTracker::new();
        let key = "reservation_release:proj:Agent";
        // Record emission with zero cooldown window.
        tracker.record_emission(key, 1_000_000, 0, "reservation_release");
        // High risk with zero cooldown should STILL suppress repeats.
        let verdict = tracker.check(key, 2_000_000, 0, "reservation_release");
        assert!(
            matches!(verdict, CooldownVerdict::Suppressed { .. }),
            "High-risk effects must be suppressed even with zero cooldown window"
        );
    }

    #[test]
    fn test_cooldown_check_uses_current_window_argument() {
        let mut tracker = CooldownTracker::new();
        let key = "liveness_probe:proj:Agent";
        tracker.record_emission(key, 1_000_000, 60_000_000, "liveness_probe");

        let verdict = tracker.check(key, 31_000_000, 30_000_000, "liveness_probe");
        assert_eq!(verdict, CooldownVerdict::Allowed);
    }

    #[test]
    fn test_record_emission_uses_current_window_argument() {
        let mut tracker = CooldownTracker::new();
        let key = "liveness_monitoring:proj:Agent";

        tracker.record_emission(key, 1_000_000, 60_000_000, "liveness_monitoring");
        tracker.record_emission(key, 31_000_000, 30_000_000, "liveness_monitoring");
        tracker.record_emission(key, 32_000_000, 30_000_000, "liveness_monitoring");

        let verdict = tracker.check(key, 33_000_000, 30_000_000, "liveness_monitoring");
        assert_eq!(verdict, CooldownVerdict::Allowed);
    }

    #[test]
    fn test_cooldown_eviction() {
        let mut tracker = CooldownTracker::new();
        tracker.record_emission("a", 1_000_000, 60_000_000, "liveness_monitoring");
        tracker.record_emission("b", 100_000_000, 60_000_000, "liveness_monitoring");
        assert_eq!(tracker.len(), 2);
        tracker.evict_older_than(50_000_000);
        assert_eq!(tracker.len(), 1);
    }

    // ── Escalation state machine ──

    #[test]
    fn test_escalation_starts_at_none() {
        let state = EscalationState::new();
        assert_eq!(state.level, EscalationLevel::None);
    }

    #[test]
    fn test_escalation_advisory_to_probe() {
        let mut state = EscalationState::new();
        state.level = EscalationLevel::Advisory;
        // Record 3 unanswered effects.
        state.record_effect(1_000_000);
        state.record_effect(2_000_000);
        state.record_effect(3_000_000);
        let escalated = state.maybe_escalate(4_000_000);
        assert_eq!(escalated, Some(EscalationLevel::Probe));
        assert_eq!(state.level, EscalationLevel::Probe);
    }

    #[test]
    fn test_escalation_probe_to_release() {
        let mut state = EscalationState::new();
        state.level = EscalationLevel::Probe;
        state.record_effect(1_000_000);
        state.record_effect(2_000_000);
        let escalated = state.maybe_escalate(3_000_000);
        assert_eq!(escalated, Some(EscalationLevel::Release));
        assert_eq!(state.level, EscalationLevel::Release);
    }

    #[test]
    fn test_escalation_release_is_terminal() {
        let mut state = EscalationState::new();
        state.level = EscalationLevel::Release;
        state.record_effect(1_000_000);
        let escalated = state.maybe_escalate(2_000_000);
        assert_eq!(escalated, None);
    }

    #[test]
    fn test_acknowledgment_resets_escalation() {
        let mut state = EscalationState::new();
        state.level = EscalationLevel::Probe;
        state.unanswered_at_level = 2;
        state.record_acknowledgment();
        assert_eq!(state.level, EscalationLevel::None);
        assert_eq!(state.unanswered_at_level, 0);
    }

    #[test]
    fn test_recommended_family_per_level() {
        let mut state = EscalationState::new();
        assert_eq!(state.recommended_family(), "liveness_monitoring");
        state.level = EscalationLevel::Advisory;
        assert_eq!(state.recommended_family(), "liveness_monitoring");
        state.level = EscalationLevel::Probe;
        assert_eq!(state.recommended_family(), "liveness_probe");
        state.level = EscalationLevel::Release;
        assert_eq!(state.recommended_family(), "reservation_release");
    }

    // ── Semantic message builder ──

    #[test]
    fn test_semantic_message_has_all_sections() {
        let state = EscalationState::new();
        let msg = build_semantic_message(
            "liveness_monitoring",
            "TestAgent",
            "No activity for 5 minutes",
            "Reply or acknowledge",
            "Quick ack clears suspicion",
            "escalate_to_probe",
            &state,
        );
        assert!(msg.rendered_body.contains("**Evidence:**"));
        assert!(msg.rendered_body.contains("**Action:**"));
        assert!(msg.rendered_body.contains("**Remediation:**"));
        assert!(msg.rendered_body.contains("**Escalation:**"));
    }

    #[test]
    fn test_semantic_message_subject_format() {
        let state = EscalationState::new();
        let msg = build_semantic_message(
            "liveness_probe",
            "Eagle",
            "evidence",
            "action",
            "fix",
            "policy",
            &state,
        );
        assert!(msg.subject.contains("[ATC probe]"));
        assert!(msg.subject.contains("Eagle"));
    }

    #[test]
    fn test_semantic_message_terminal_escalation() {
        let mut state = EscalationState::new();
        state.level = EscalationLevel::Release;
        let msg = build_semantic_message(
            "reservation_release",
            "Agent",
            "evidence",
            "action",
            "fix",
            "policy",
            &state,
        );
        assert!(msg.escalation.contains("No further automatic escalation"));
    }

    // ── Utility models ──

    #[test]
    fn test_all_families_have_utility_models() {
        let families = [
            "liveness_monitoring",
            "deadlock_remediation",
            "liveness_probe",
            "reservation_release",
            "release_notice",
            "withheld_release_notice",
        ];
        for family in families {
            let model = EFFECT_UTILITY_MODELS.iter().find(|m| m.family == family);
            assert!(model.is_some(), "Missing utility model for {family}");
            let m = model.unwrap();
            assert!(!m.utility.is_empty());
            assert!(!m.success_criterion.is_empty());
            assert!(!m.harm_criterion.is_empty());
        }
    }

    // ── Noise suppression ──

    #[test]
    fn test_advisory_noise_under_limit() {
        assert!(!is_advisory_noise_exceeded(5));
    }

    #[test]
    fn test_advisory_noise_at_limit() {
        assert!(is_advisory_noise_exceeded(10));
    }

    #[test]
    fn test_toast_rate_under_limit() {
        assert!(!is_toast_rate_exceeded(2));
    }

    #[test]
    fn test_toast_rate_at_limit() {
        assert!(is_toast_rate_exceeded(3));
    }

    // ── Display impls ──

    #[test]
    fn test_risk_class_display() {
        assert_eq!(EffectRiskClass::LowRiskNudge.to_string(), "low_risk_nudge");
        assert_eq!(
            EffectRiskClass::MediumRiskCheck.to_string(),
            "medium_risk_check"
        );
        assert_eq!(
            EffectRiskClass::HighRiskIntervention.to_string(),
            "high_risk_intervention"
        );
    }

    #[test]
    fn test_escalation_level_display() {
        assert_eq!(EscalationLevel::None.to_string(), "none");
        assert_eq!(EscalationLevel::Advisory.to_string(), "advisory");
        assert_eq!(EscalationLevel::Probe.to_string(), "probe");
        assert_eq!(EscalationLevel::Release.to_string(), "release");
    }

    #[test]
    fn test_optimization_target_display() {
        assert_eq!(OptimizationTarget::Relevance.to_string(), "relevance");
        assert_eq!(OptimizationTarget::Accuracy.to_string(), "accuracy");
        assert_eq!(
            OptimizationTarget::ZeroFalsePositive.to_string(),
            "zero_false_positive"
        );
    }
}
