//! User-facing utility, remediation guidance, noise control, and safe
//! defaults for ATC surfaces (br-0qt6e.4.6).
//!
//! Defines how ATC communicates with operators and agents: what to show,
//! when to show it, how to frame actions vs. non-actions, and how to
//! keep the surface calm, fair, and trustworthy.
//!
//! # Design Principle
//!
//! Even a mathematically strong ATC system will feel bad if it is noisy,
//! opaque, hard to act on, or unable to say when the evidence is not
//! trustworthy. This module optimizes for real operators and agents.
//!
//! # Surface State Taxonomy
//!
//! Every ATC surface element must communicate one of these states:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │  Observation        — "ATC noticed X. No action needed."    │
//! │  Recommendation     — "ATC suggests Y. Here's the evidence."│
//! │  Executed           — "ATC did Z. Here's why and what next."│
//! │  Suppressed         — "ATC held back. Here's why."          │
//! │  Fairness Throttle  — "Paused to avoid over-burdening."     │
//! │  Deliberate No-Op   — "ATC chose inaction. Here's why."     │
//! │  Distrust           — "Evidence quality too low to act on."  │
//! │  Confounded         — "Outcome is real but credit is unclear"│
//! │  Safe-to-Ignore     — "Nothing requires attention right now."│
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Noise Control Philosophy
//!
//! The system should be informative without becoming spammy:
//! - Default surfaces show only actionable items
//! - Repeated identical messages are suppressed after the first
//! - Safe-no-action states are silent unless the operator drills down
//! - Escalation is visible but not alarming until action is needed
//! - Evidence-trust problems are surfaced honestly, not hidden

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// Surface state taxonomy
// ──────────────────────────────────────────────────────────────────────

/// The state of an ATC surface element as presented to the user.
///
/// Every ATC output must be classifiable into exactly one of these states.
/// This prevents unexplained silence and makes the system's reasoning
/// legible to operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceState {
    /// ATC observed something noteworthy but no action is needed.
    /// Example: "Agent X has been idle for 3 minutes (normal for this agent)."
    Observation,
    /// ATC recommends an action but has not taken it.
    /// Example: "Consider probing Agent X — idle for 8 minutes."
    Recommendation,
    /// ATC executed an intervention.
    /// Example: "Released reservations for Agent X (confirmed inactive)."
    ExecutedIntervention,
    /// ATC considered an action but held back due to safety/calibration.
    /// Example: "Release withheld — calibration uncertain."
    SuppressedIntervention,
    /// ATC throttled action to avoid over-burdening a target.
    /// Example: "Advisory suppressed — Agent X received 3 advisories this hour."
    FairnessThrottle,
    /// ATC deliberately chose inaction as the optimal response.
    /// Example: "No action needed — Agent X's behavior is within normal bounds."
    DeliberateNoOp,
    /// ATC cannot trust the evidence enough to act on it.
    /// Example: "Evidence quarantined — possible contamination detected."
    EvidenceDistrust,
    /// The outcome is real but the causal story is not clean.
    /// Example: "Agent recovered, but 3 overlapping interventions make credit unclear."
    AttributionConfounded,
    /// Nothing requires attention. This is the default calm state.
    /// Example: "All agents healthy. No actions pending."
    SafeToIgnore,
}

impl SurfaceState {
    /// Whether this state is actionable (requires operator attention).
    #[must_use]
    pub const fn is_actionable(self) -> bool {
        matches!(
            self,
            Self::Recommendation | Self::ExecutedIntervention | Self::EvidenceDistrust
        )
    }

    /// Whether this state should appear in default (non-verbose) views.
    #[must_use]
    pub const fn visible_by_default(self) -> bool {
        matches!(
            self,
            Self::Recommendation
                | Self::ExecutedIntervention
                | Self::EvidenceDistrust
                | Self::AttributionConfounded
        )
    }

    /// Whether this state should be silent unless the operator drills down.
    #[must_use]
    pub const fn silent_by_default(self) -> bool {
        matches!(
            self,
            Self::Observation | Self::DeliberateNoOp | Self::SafeToIgnore
        )
    }

    /// Human-readable label for UI display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Observation => "Observed",
            Self::Recommendation => "Recommended",
            Self::ExecutedIntervention => "Executed",
            Self::SuppressedIntervention => "Suppressed",
            Self::FairnessThrottle => "Throttled",
            Self::DeliberateNoOp => "No Action",
            Self::EvidenceDistrust => "Low Trust",
            Self::AttributionConfounded => "Confounded",
            Self::SafeToIgnore => "All Clear",
        }
    }

    /// Icon hint for TUI/robot rendering.
    #[must_use]
    pub const fn icon_hint(self) -> &'static str {
        match self {
            Self::Observation => "eye",
            Self::Recommendation => "lightbulb",
            Self::ExecutedIntervention => "bolt",
            Self::SuppressedIntervention => "pause",
            Self::FairnessThrottle => "scale",
            Self::DeliberateNoOp => "check",
            Self::EvidenceDistrust => "shield_alert",
            Self::AttributionConfounded => "help",
            Self::SafeToIgnore => "check_circle",
        }
    }

    /// Default evidence-trust metadata implied by this surface state.
    #[must_use]
    pub const fn default_evidence_trust(self) -> EvidenceTrustLevel {
        match self {
            Self::EvidenceDistrust => EvidenceTrustLevel::Quarantined,
            _ => EvidenceTrustLevel::High,
        }
    }

    /// Default attribution-clarity metadata implied by this surface state.
    #[must_use]
    pub const fn default_attribution_clarity(self) -> AttributionClarity {
        match self {
            Self::AttributionConfounded => AttributionClarity::Confounded,
            _ => AttributionClarity::Clean,
        }
    }

    /// Whether the operator can safely dismiss this state after reading it.
    ///
    /// This is intentionally different from `silent_by_default()`: some states
    /// are worth showing by default for transparency, even though they do not
    /// require follow-up action.
    #[must_use]
    pub const fn safe_to_ignore(self) -> bool {
        !self.is_actionable()
    }
}

impl std::fmt::Display for SurfaceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ──────────────────────────────────────────────────────────────────────
// Surface card: the atomic unit of user-facing ATC output
// ──────────────────────────────────────────────────────────────────────

/// A surface card: the atomic unit of ATC output for operators.
///
/// Every ATC communication to the user (TUI toast, robot status line,
/// audit log entry, system health indicator) should be derivable from
/// a `SurfaceCard`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceCard {
    /// The state classification.
    pub state: SurfaceState,
    /// One-line headline (max 80 chars).
    pub headline: String,
    /// What happened (evidence summary).
    pub what_happened: String,
    /// Why it happened (reasoning chain).
    pub why: String,
    /// How risky the situation or action is.
    pub risk_assessment: RiskAssessment,
    /// What the operator should do next.
    pub next_action: NextAction,
    /// Whether this can be safely ignored.
    pub safe_to_ignore: bool,
    /// Evidence trust level for this card.
    pub evidence_trust: EvidenceTrustLevel,
    /// Attribution clarity for this card.
    pub attribution_clarity: AttributionClarity,
    /// Target agent or cohort (if applicable).
    pub target: Option<String>,
    /// Severity for display ordering (0.0 = informational, 1.0 = critical).
    pub severity: f64,
    /// When this card was generated (microseconds).
    pub generated_ts_micros: i64,
    /// Time-to-live before the card auto-expires (microseconds).
    pub ttl_micros: i64,
}

/// Risk assessment for a surface card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskAssessment {
    /// No risk to operators or agents.
    None,
    /// Low risk: informational only.
    Low,
    /// Medium risk: monitoring recommended.
    Medium,
    /// High risk: action may be needed.
    High,
    /// Critical: immediate attention required.
    Critical,
}

impl std::fmt::Display for RiskAssessment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// What the operator should do next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextAction {
    /// Concise instruction (max 120 chars).
    pub instruction: String,
    /// Whether action is required or optional.
    pub required: bool,
    /// Suggested command or path (if applicable).
    pub command_hint: Option<String>,
}

/// Evidence trust level communicated to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTrustLevel {
    /// Evidence is clean and trustworthy.
    High,
    /// Evidence has some uncertainty but is usable.
    Moderate,
    /// Evidence quality is low — decisions should be cautious.
    Low,
    /// Evidence is quarantined — do not trust this yet.
    Quarantined,
}

impl EvidenceTrustLevel {
    /// User-facing explanation of the trust level.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::High => "Evidence is clean and reliable.",
            Self::Moderate => "Some uncertainty in the evidence, but sufficient for action.",
            Self::Low => "Evidence quality is low. Exercise caution with automated decisions.",
            Self::Quarantined => {
                "Evidence has been quarantined due to suspected contamination. Do not act on this alone."
            }
        }
    }
}

impl std::fmt::Display for EvidenceTrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::High => write!(f, "high"),
            Self::Moderate => write!(f, "moderate"),
            Self::Low => write!(f, "low"),
            Self::Quarantined => write!(f, "quarantined"),
        }
    }
}

/// Attribution clarity communicated to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionClarity {
    /// Single clear cause. Full confidence in the causal story.
    Clean,
    /// Multiple causes but one is dominant. Mostly clear.
    Dominant,
    /// Multiple overlapping causes. Credit story is not clean.
    Confounded,
    /// Cannot determine cause. Outcome is real but attribution is unknown.
    Unknown,
}

impl AttributionClarity {
    /// User-facing explanation.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Clean => "Clear single cause identified.",
            Self::Dominant => "Multiple factors, but one primary cause identified.",
            Self::Confounded => {
                "Multiple overlapping interventions make the credit story unclear. The outcome is real, but ATC cannot confidently assign credit."
            }
            Self::Unknown => "Cannot determine what caused this outcome.",
        }
    }
}

impl std::fmt::Display for AttributionClarity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => write!(f, "clean"),
            Self::Dominant => write!(f, "dominant"),
            Self::Confounded => write!(f, "confounded"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Noise control policy
// ──────────────────────────────────────────────────────────────────────

/// Noise control policy for a specific surface type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoisePolicy {
    /// Surface type this policy applies to.
    pub surface: &'static str,
    /// Maximum emissions per agent per hour.
    pub max_per_agent_per_hour: u32,
    /// Maximum emissions globally per minute.
    pub max_global_per_minute: u32,
    /// Cooldown between identical messages (microseconds).
    pub dedup_cooldown_micros: i64,
    /// Whether to auto-suppress after repeated occurrences.
    pub auto_suppress_after: u32,
    /// Escalation threshold (after N suppressions, escalate).
    pub escalation_threshold: u32,
    /// Whether this surface type is interruptive (toast, alert).
    pub is_interruptive: bool,
}

/// Canonical noise policies for all ATC surface types.
pub const NOISE_POLICIES: &[NoisePolicy] = &[
    NoisePolicy {
        surface: "advisory",
        max_per_agent_per_hour: 10,
        max_global_per_minute: 3,
        dedup_cooldown_micros: 300_000_000, // 5 minutes
        auto_suppress_after: 3,
        escalation_threshold: 5,
        is_interruptive: false,
    },
    NoisePolicy {
        surface: "probe",
        max_per_agent_per_hour: 5,
        max_global_per_minute: 2,
        dedup_cooldown_micros: 600_000_000, // 10 minutes
        auto_suppress_after: 2,
        escalation_threshold: 3,
        is_interruptive: true,
    },
    NoisePolicy {
        surface: "release_warning",
        max_per_agent_per_hour: 2,
        max_global_per_minute: 1,
        dedup_cooldown_micros: 1_800_000_000, // 30 minutes
        auto_suppress_after: 1,
        escalation_threshold: 2,
        is_interruptive: true,
    },
    NoisePolicy {
        surface: "degraded_learning_alert",
        max_per_agent_per_hour: 3,
        max_global_per_minute: 1,
        dedup_cooldown_micros: 600_000_000, // 10 minutes
        auto_suppress_after: 2,
        escalation_threshold: 3,
        is_interruptive: false,
    },
    NoisePolicy {
        surface: "suspicious_evidence_alert",
        max_per_agent_per_hour: 2,
        max_global_per_minute: 1,
        dedup_cooldown_micros: 900_000_000, // 15 minutes
        auto_suppress_after: 1,
        escalation_threshold: 2,
        is_interruptive: true,
    },
    NoisePolicy {
        surface: "confounded_outcome_alert",
        max_per_agent_per_hour: 3,
        max_global_per_minute: 2,
        dedup_cooldown_micros: 300_000_000, // 5 minutes
        auto_suppress_after: 2,
        escalation_threshold: 4,
        is_interruptive: false,
    },
    NoisePolicy {
        surface: "safe_no_action",
        max_per_agent_per_hour: 0, // never emitted proactively
        max_global_per_minute: 0,
        dedup_cooldown_micros: 0,
        auto_suppress_after: 0,
        escalation_threshold: 0,
        is_interruptive: false,
    },
    NoisePolicy {
        surface: "fairness_throttle",
        max_per_agent_per_hour: 3,
        max_global_per_minute: 2,
        dedup_cooldown_micros: 600_000_000, // 10 minutes
        auto_suppress_after: 2,
        escalation_threshold: 4,
        is_interruptive: false,
    },
];

// ──────────────────────────────────────────────────────────────────────
// Safe defaults
// ──────────────────────────────────────────────────────────────────────

/// Safe defaults for surface behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SafeDefaults {
    /// Default view mode (summary vs. drill-down).
    pub default_view: ViewMode,
    /// Whether interruptive surfaces (toasts, alerts) are enabled.
    pub interruptive_enabled: bool,
    /// Minimum severity for interruptive surfaces (0.0-1.0).
    pub interruptive_min_severity: f64,
    /// Whether to show attribution-confounded outcomes.
    pub show_confounded: bool,
    /// Whether to show evidence-distrust indicators.
    pub show_distrust: bool,
    /// Whether to show fairness throttle indicators.
    pub show_fairness_throttle: bool,
    /// Whether to show deliberate no-ops in the timeline.
    pub show_no_ops: bool,
    /// Default auto-dismiss time for info toasts (seconds).
    pub toast_info_dismiss_secs: u32,
    /// Default auto-dismiss time for warning toasts (seconds).
    pub toast_warn_dismiss_secs: u32,
    /// Default auto-dismiss time for error toasts (seconds).
    pub toast_error_dismiss_secs: u32,
}

/// View mode for surface rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// Summary view: only actionable items, concise format.
    Summary,
    /// Detailed view: includes observations, no-ops, full evidence.
    Detailed,
    /// Forensic view: everything including raw metrics and internals.
    Forensic,
}

impl std::fmt::Display for ViewMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Summary => write!(f, "summary"),
            Self::Detailed => write!(f, "detailed"),
            Self::Forensic => write!(f, "forensic"),
        }
    }
}

/// The canonical safe defaults for ATC surfaces.
pub const SAFE_DEFAULTS: SafeDefaults = SafeDefaults {
    default_view: ViewMode::Summary,
    interruptive_enabled: true,
    interruptive_min_severity: 0.50, // only medium+ severity interrupts
    show_confounded: true,           // operators should know when credit is unclear
    show_distrust: true,             // operators should know when evidence is bad
    show_fairness_throttle: true,    // operators should know when throttling happens
    show_no_ops: false,              // no-ops are silent by default
    toast_info_dismiss_secs: 5,
    toast_warn_dismiss_secs: 8,
    toast_error_dismiss_secs: 15,
};

// ──────────────────────────────────────────────────────────────────────
// Golden workflows
// ──────────────────────────────────────────────────────────────────────

/// A golden user workflow that defines the expected operator experience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenWorkflow {
    /// Workflow name.
    pub name: &'static str,
    /// When this workflow applies.
    pub trigger: &'static str,
    /// What the operator should see.
    pub expected_surface: &'static str,
    /// What the operator should do.
    pub expected_action: &'static str,
    /// What should NOT happen.
    pub anti_pattern: &'static str,
}

/// Canonical golden workflows for ATC surfaces.
pub const GOLDEN_WORKFLOWS: &[GoldenWorkflow] = &[
    GoldenWorkflow {
        name: "agent_goes_idle",
        trigger: "An agent stops responding for longer than its normal cadence",
        expected_surface: "Summary shows Recommendation card with probe suggestion. Evidence shows silence duration vs. agent's normal pattern. Risk: Low→Medium as duration increases.",
        expected_action: "If agent is truly idle, acknowledge the advisory. If agent is working, reply to clear suspicion.",
        anti_pattern: "Do NOT show a toast for every agent that goes quiet for 2 minutes. Most pauses are normal.",
    },
    GoldenWorkflow {
        name: "reservation_deadlock",
        trigger: "Two or more agents hold reservations that block each other",
        expected_surface: "Summary shows Recommendation card with the specific cycle. Evidence shows which files are contested. Risk: Medium.",
        expected_action: "Inspect which agent's work is less active and release their reservation.",
        anti_pattern: "Do NOT send repeated deadlock alerts for the same cycle. One notification is enough.",
    },
    GoldenWorkflow {
        name: "automated_release",
        trigger: "ATC releases reservations for a confirmed-dead agent",
        expected_surface: "Summary shows Executed card with the release details. Evidence shows the liveness verdict. Risk: High. Next action: verify the agent is truly inactive.",
        expected_action: "Check if the agent's session is truly dead. Re-reserve if the agent comes back.",
        anti_pattern: "Do NOT release without showing exactly which files were released and why.",
    },
    GoldenWorkflow {
        name: "withheld_release",
        trigger: "ATC wanted to release but calibration/safety gates blocked it",
        expected_surface: "Summary shows Suppressed card explaining why release was withheld. Evidence shows the gate that blocked it. Risk: Low.",
        expected_action: "No immediate action needed. The system is being cautious. Consider manual inspection if the situation persists.",
        anti_pattern: "Do NOT silently swallow the withheld release. Operators should know the system considered releasing.",
    },
    GoldenWorkflow {
        name: "evidence_contamination",
        trigger: "ATC detects contaminated or suspicious evidence",
        expected_surface: "Summary shows Distrust card with the contamination type. Evidence shows which signals triggered. Risk: Medium.",
        expected_action: "Investigate the source of contamination. Consider whether the affected agent is gaming the system.",
        anti_pattern: "Do NOT continue making decisions based on quarantined evidence without telling the operator.",
    },
    GoldenWorkflow {
        name: "confounded_attribution",
        trigger: "An outcome occurred but multiple overlapping interventions make credit unclear",
        expected_surface: "Summary shows Confounded card. Evidence shows the overlapping interventions. Attribution: confounded.",
        expected_action: "No action needed. ATC will learn with reduced weight from this outcome. The outcome is real even if credit is unclear.",
        anti_pattern: "Do NOT pretend the credit is clean when it isn't. Honesty about uncertainty builds trust.",
    },
    GoldenWorkflow {
        name: "all_quiet",
        trigger: "All agents are healthy, no actions are pending, no alerts",
        expected_surface: "Summary shows SafeToIgnore state. No toasts or alerts. Timeline shows green status.",
        expected_action: "Nothing. This is the desired state.",
        anti_pattern: "Do NOT fill the screen with 'everything is OK' messages. Silence IS the message.",
    },
    GoldenWorkflow {
        name: "fairness_throttle",
        trigger: "ATC suppresses an action to avoid over-burdening a specific agent",
        expected_surface: "Detailed view shows FairnessThrottle card explaining which agent was spared and why.",
        expected_action: "Review if the affected agent is receiving disproportionate attention. Consider manual investigation.",
        anti_pattern: "Do NOT hide fairness throttling. Operators should know when the system is protecting specific targets.",
    },
];

// ──────────────────────────────────────────────────────────────────────
// Surface card builder
// ──────────────────────────────────────────────────────────────────────

/// Build a summary card for a surface state.
///
/// This consolidates the state into a quick-glance representation.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_surface_card(
    state: SurfaceState,
    headline: String,
    what_happened: String,
    why: String,
    risk: RiskAssessment,
    next_action_text: &str,
    target: Option<String>,
    now_micros: i64,
) -> SurfaceCard {
    let severity = match risk {
        RiskAssessment::None => 0.0,
        RiskAssessment::Low => 0.25,
        RiskAssessment::Medium => 0.50,
        RiskAssessment::High => 0.75,
        RiskAssessment::Critical => 1.0,
    };

    let ttl = match state {
        SurfaceState::SafeToIgnore | SurfaceState::DeliberateNoOp => 60_000_000, // 1 min
        SurfaceState::Observation => 300_000_000,                                // 5 min
        SurfaceState::FairnessThrottle => 600_000_000,                           // 10 min
        _ => 1_800_000_000, // 30 min for actionable items
    };

    SurfaceCard {
        state,
        headline,
        what_happened,
        why,
        risk_assessment: risk,
        next_action: NextAction {
            instruction: next_action_text.to_string(),
            required: state.is_actionable(),
            command_hint: None,
        },
        safe_to_ignore: state.safe_to_ignore(),
        evidence_trust: state.default_evidence_trust(),
        attribution_clarity: state.default_attribution_clarity(),
        target,
        severity,
        generated_ts_micros: now_micros,
        ttl_micros: ttl,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::assertions_on_constants)]
    use super::*;

    // ── Surface state ──

    #[test]
    fn test_actionable_states() {
        assert!(SurfaceState::Recommendation.is_actionable());
        assert!(SurfaceState::ExecutedIntervention.is_actionable());
        assert!(SurfaceState::EvidenceDistrust.is_actionable());
        assert!(!SurfaceState::Observation.is_actionable());
        assert!(!SurfaceState::SafeToIgnore.is_actionable());
        assert!(!SurfaceState::DeliberateNoOp.is_actionable());
    }

    #[test]
    fn test_visible_by_default() {
        assert!(SurfaceState::Recommendation.visible_by_default());
        assert!(SurfaceState::ExecutedIntervention.visible_by_default());
        assert!(SurfaceState::EvidenceDistrust.visible_by_default());
        assert!(SurfaceState::AttributionConfounded.visible_by_default());
        assert!(!SurfaceState::Observation.visible_by_default());
        assert!(!SurfaceState::SafeToIgnore.visible_by_default());
    }

    #[test]
    fn test_silent_by_default() {
        assert!(SurfaceState::Observation.silent_by_default());
        assert!(SurfaceState::DeliberateNoOp.silent_by_default());
        assert!(SurfaceState::SafeToIgnore.silent_by_default());
        assert!(!SurfaceState::Recommendation.silent_by_default());
        assert!(!SurfaceState::ExecutedIntervention.silent_by_default());
    }

    #[test]
    fn test_surface_state_labels() {
        assert_eq!(SurfaceState::Observation.label(), "Observed");
        assert_eq!(SurfaceState::ExecutedIntervention.label(), "Executed");
        assert_eq!(SurfaceState::EvidenceDistrust.label(), "Low Trust");
        assert_eq!(SurfaceState::SafeToIgnore.label(), "All Clear");
    }

    #[test]
    fn test_surface_state_display() {
        assert_eq!(SurfaceState::Recommendation.to_string(), "Recommended");
        assert_eq!(SurfaceState::FairnessThrottle.to_string(), "Throttled");
    }

    #[test]
    fn test_all_states_have_icon_hints() {
        let states = [
            SurfaceState::Observation,
            SurfaceState::Recommendation,
            SurfaceState::ExecutedIntervention,
            SurfaceState::SuppressedIntervention,
            SurfaceState::FairnessThrottle,
            SurfaceState::DeliberateNoOp,
            SurfaceState::EvidenceDistrust,
            SurfaceState::AttributionConfounded,
            SurfaceState::SafeToIgnore,
        ];
        for state in states {
            assert!(!state.icon_hint().is_empty(), "Missing icon for {state:?}");
        }
    }

    // ── Risk assessment ──

    #[test]
    fn test_risk_assessment_display() {
        assert_eq!(RiskAssessment::None.to_string(), "none");
        assert_eq!(RiskAssessment::Critical.to_string(), "critical");
    }

    // ── Evidence trust ──

    #[test]
    fn test_evidence_trust_explanations() {
        assert!(!EvidenceTrustLevel::High.explanation().is_empty());
        assert!(!EvidenceTrustLevel::Quarantined.explanation().is_empty());
        assert!(
            EvidenceTrustLevel::Quarantined
                .explanation()
                .contains("quarantined")
        );
    }

    // ── Attribution clarity ──

    #[test]
    fn test_attribution_clarity_explanations() {
        assert!(AttributionClarity::Clean.explanation().contains("single"));
        assert!(
            AttributionClarity::Confounded
                .explanation()
                .contains("unclear")
        );
    }

    // ── Noise policies ──

    #[test]
    fn test_noise_policies_complete() {
        let surfaces = [
            "advisory",
            "probe",
            "release_warning",
            "degraded_learning_alert",
            "suspicious_evidence_alert",
            "confounded_outcome_alert",
            "safe_no_action",
            "fairness_throttle",
        ];
        for surface in surfaces {
            let policy = NOISE_POLICIES.iter().find(|p| p.surface == surface);
            assert!(policy.is_some(), "Missing noise policy for {surface}");
        }
    }

    #[test]
    fn test_safe_no_action_is_never_proactive() {
        let policy = NOISE_POLICIES
            .iter()
            .find(|p| p.surface == "safe_no_action")
            .unwrap();
        assert_eq!(policy.max_per_agent_per_hour, 0);
        assert_eq!(policy.max_global_per_minute, 0);
        assert!(!policy.is_interruptive);
    }

    #[test]
    fn test_probe_is_interruptive() {
        let policy = NOISE_POLICIES
            .iter()
            .find(|p| p.surface == "probe")
            .unwrap();
        assert!(policy.is_interruptive);
    }

    // ── Safe defaults ──

    #[test]
    fn test_safe_defaults_are_conservative() {
        assert_eq!(SAFE_DEFAULTS.default_view, ViewMode::Summary);
        assert!(SAFE_DEFAULTS.interruptive_min_severity >= 0.50);
        assert!(!SAFE_DEFAULTS.show_no_ops);
        assert!(SAFE_DEFAULTS.show_distrust);
        assert!(SAFE_DEFAULTS.show_confounded);
    }

    // ── Golden workflows ──

    #[test]
    fn test_golden_workflows_complete() {
        assert!(GOLDEN_WORKFLOWS.len() >= 7);
        for wf in GOLDEN_WORKFLOWS {
            assert!(!wf.name.is_empty());
            assert!(!wf.trigger.is_empty());
            assert!(!wf.expected_surface.is_empty());
            assert!(!wf.expected_action.is_empty());
            assert!(!wf.anti_pattern.is_empty());
        }
    }

    #[test]
    fn test_all_quiet_workflow_expects_silence() {
        let wf = GOLDEN_WORKFLOWS
            .iter()
            .find(|w| w.name == "all_quiet")
            .unwrap();
        assert!(wf.anti_pattern.contains("NOT"));
        assert!(wf.expected_action.contains("Nothing"));
    }

    // ── Surface card builder ──

    #[test]
    fn test_build_surface_card_recommendation() {
        let card = build_surface_card(
            SurfaceState::Recommendation,
            "Test headline".into(),
            "Something happened".into(),
            "Because of reasons".into(),
            RiskAssessment::Medium,
            "Do this thing",
            Some("AgentX".into()),
            1_000_000,
        );
        assert!(card.next_action.required);
        assert!(!card.safe_to_ignore);
        assert_eq!(card.severity, 0.50);
        assert_eq!(card.target.as_deref(), Some("AgentX"));
    }

    #[test]
    fn test_build_surface_card_safe_to_ignore() {
        let card = build_surface_card(
            SurfaceState::SafeToIgnore,
            "All clear".into(),
            "Nothing happened".into(),
            "Everything is fine".into(),
            RiskAssessment::None,
            "Nothing to do",
            None,
            1_000_000,
        );
        assert!(card.safe_to_ignore);
        assert!(!card.next_action.required);
        assert_eq!(card.severity, 0.0);
    }

    #[test]
    fn test_build_surface_card_evidence_distrust_defaults_to_quarantined_trust() {
        let card = build_surface_card(
            SurfaceState::EvidenceDistrust,
            "Low-trust evidence".into(),
            "ATC detected contaminated evidence".into(),
            "Trace replay made this outcome unsafe to trust".into(),
            RiskAssessment::High,
            "Do not act automatically",
            Some("AgentX".into()),
            1_000_000,
        );

        assert_eq!(card.evidence_trust, EvidenceTrustLevel::Quarantined);
        assert_eq!(card.attribution_clarity, AttributionClarity::Clean);
    }

    #[test]
    fn test_build_surface_card_confounded_defaults_to_confounded_attribution() {
        let card = build_surface_card(
            SurfaceState::AttributionConfounded,
            "Confounded outcome".into(),
            "The target recovered".into(),
            "Multiple overlapping interventions make credit unclear".into(),
            RiskAssessment::Medium,
            "Inspect overlapping actions",
            Some("AgentY".into()),
            1_000_000,
        );

        assert_eq!(card.evidence_trust, EvidenceTrustLevel::High);
        assert_eq!(card.attribution_clarity, AttributionClarity::Confounded);
        assert!(card.safe_to_ignore);
        assert!(!card.state.silent_by_default());
    }

    #[test]
    fn test_surface_card_ttl_varies_by_state() {
        let actionable = build_surface_card(
            SurfaceState::ExecutedIntervention,
            "h".into(),
            "w".into(),
            "y".into(),
            RiskAssessment::High,
            "act",
            None,
            0,
        );
        let passive = build_surface_card(
            SurfaceState::SafeToIgnore,
            "h".into(),
            "w".into(),
            "y".into(),
            RiskAssessment::None,
            "nothing",
            None,
            0,
        );
        assert!(actionable.ttl_micros > passive.ttl_micros);
    }

    // ── View mode ──

    #[test]
    fn test_view_mode_display() {
        assert_eq!(ViewMode::Summary.to_string(), "summary");
        assert_eq!(ViewMode::Detailed.to_string(), "detailed");
        assert_eq!(ViewMode::Forensic.to_string(), "forensic");
    }
}
