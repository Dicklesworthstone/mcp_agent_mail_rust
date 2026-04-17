//! Interference-aware credit assignment and causal eligibility windows
//! (br-0qt6e.3.13).
//!
//! Defines how ATC attributes outcomes to specific decisions when multiple
//! interventions overlap in time, when operators change controls, or when
//! agents recover naturally.
//!
//! # Problem
//!
//! ATC does not operate in a clean single-cause world:
//! - Multiple probes/advisories may be active for the same agent
//! - An operator may manually intervene during an attribution window
//! - An agent may recover on its own (natural recovery, not ATC action)
//! - One ATC action may change the observability of another
//!
//! # Attribution Model
//!
//! Each outcome is attributed with a confidence level:
//!
//! | Confidence | Meaning                                     |
//! |------------|---------------------------------------------|
//! | `Clean`    | Single cause, no interference               |
//! | `Primary`  | Multiple causes, but this one is dominant   |
//! | `Shared`   | Multiple plausible causes, credit split     |
//! | `Ambiguous`| Cannot determine cause — do not learn        |
//! | `Censored` | Outcome unobservable (agent gone, etc.)     |
//!
//! # Eligibility Windows
//!
//! An experience is eligible for attribution to an outcome only if:
//! 1. The experience's subject matches the outcome's subject (same agent)
//! 2. The experience was created BEFORE the outcome was observed
//! 3. The experience's effect kind is causally plausible for this outcome
//! 4. The time between effect and outcome is within the eligibility window

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

/// Default eligibility window: 10 minutes in microseconds.
pub const DEFAULT_ELIGIBILITY_WINDOW_MICROS: i64 = 600_000_000;

/// Maximum candidate causes to consider for a single outcome.
pub const MAX_CANDIDATE_CAUSES: usize = 10;

/// Attribution confidence level for an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionConfidence {
    /// Single cause, no interference. Safe to learn from.
    Clean,
    /// Multiple causes exist, but this one is the dominant contributor.
    /// Learning is allowed but with reduced weight.
    Primary,
    /// Multiple plausible causes. Credit is split proportionally.
    /// Learning is allowed with proportional weight.
    Shared,
    /// Cannot determine cause. Do not learn from this outcome.
    Ambiguous,
    /// Outcome is unobservable (agent departed, project closed).
    Censored,
}

impl AttributionConfidence {
    /// Whether this confidence level allows learning.
    #[must_use]
    pub const fn allows_learning(self) -> bool {
        matches!(self, Self::Clean | Self::Primary | Self::Shared)
    }

    /// Weight to apply to the learning signal (0.0 to 1.0).
    #[must_use]
    pub const fn learning_weight(self) -> f64 {
        match self {
            Self::Clean => 1.0,
            Self::Primary => 0.8,
            Self::Shared => 0.5,
            Self::Ambiguous | Self::Censored => 0.0,
        }
    }
}

impl std::fmt::Display for AttributionConfidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => write!(f, "clean"),
            Self::Primary => write!(f, "primary"),
            Self::Shared => write!(f, "shared"),
            Self::Ambiguous => write!(f, "ambiguous"),
            Self::Censored => write!(f, "censored"),
        }
    }
}

/// A candidate cause for an observed outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateCause {
    /// Experience ID of the candidate.
    pub experience_id: u64,
    /// Decision ID that generated this experience.
    pub decision_id: u64,
    /// Effect kind (advisory, probe, release, etc.).
    pub effect_kind: String,
    /// When the effect was created.
    pub created_ts_micros: i64,
    /// Time between effect and outcome observation.
    pub delay_micros: i64,
    /// Whether this candidate was the most recent before the outcome.
    pub is_most_recent: bool,
    /// Subsystem that originated the decision.
    pub subsystem: String,
}

/// Result of attributing an outcome to candidate causes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributionResult {
    /// The confidence level of the attribution.
    pub confidence: AttributionConfidence,
    /// The primary cause (if identifiable).
    pub primary_cause: Option<u64>,
    /// All candidate causes considered.
    pub candidate_count: usize,
    /// Whether interference was detected.
    pub interference_detected: bool,
    /// Reason for the attribution decision (human-readable).
    pub reason: String,
    /// Learning weight to apply to the primary cause.
    pub learning_weight: f64,
    /// Operator-facing reason code.
    pub reason_code: AttributionReasonCode,
}

/// Machine-readable reason code for attribution decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionReasonCode {
    /// Single eligible cause — clean attribution.
    SingleCause,
    /// Most recent cause selected as primary.
    MostRecentCause,
    /// Causes from different subsystems — no interference.
    DisjointSubsystems,
    /// Multiple overlapping causes of same type.
    OverlappingCauses,
    /// Natural recovery (no ATC action was the cause).
    NaturalRecovery,
    /// Operator intervention confounds attribution.
    OperatorIntervention,
    /// Insufficient causal isolation.
    InsufficientIsolation,
    /// Subject departed or project closed.
    SubjectDeparted,
    /// No eligible candidates.
    NoCandidates,
}

/// Determine attribution for an observed outcome given candidate causes.
///
/// # Algorithm
///
/// 1. Filter candidates to those within the eligibility window.
/// 2. If no candidates → NaturalRecovery (no ATC action caused this).
/// 3. If one candidate → Clean attribution.
/// 4. If multiple candidates from different subsystems → Clean (disjoint).
/// 5. If multiple candidates from the same subsystem:
///    a. If one is clearly most recent (> 2× closer than next) → Primary.
///    b. Otherwise → Shared credit proportional to recency.
#[must_use]
pub fn attribute_outcome(
    candidates: &[CandidateCause],
    outcome_ts_micros: i64,
    eligibility_window_micros: i64,
) -> AttributionResult {
    // Filter to eligible candidates.
    let eligible: Vec<&CandidateCause> = candidates
        .iter()
        .filter(|c| {
            c.delay_micros >= 0
                && c.delay_micros <= eligibility_window_micros
                && c.created_ts_micros < outcome_ts_micros
        })
        .take(MAX_CANDIDATE_CAUSES)
        .collect();

    if eligible.is_empty() {
        return AttributionResult {
            confidence: AttributionConfidence::Ambiguous,
            primary_cause: None,
            candidate_count: 0,
            interference_detected: false,
            reason: "no eligible candidate causes within attribution window".to_string(),
            learning_weight: 0.0,
            reason_code: AttributionReasonCode::NoCandidates,
        };
    }

    if eligible.len() == 1 {
        let cause = eligible[0];
        return AttributionResult {
            confidence: AttributionConfidence::Clean,
            primary_cause: Some(cause.experience_id),
            candidate_count: 1,
            interference_detected: false,
            reason: format!(
                "single eligible cause: {} (delay {}ms)",
                cause.effect_kind,
                cause.delay_micros / 1000
            ),
            learning_weight: 1.0,
            reason_code: AttributionReasonCode::SingleCause,
        };
    }

    // Multiple candidates — check subsystem overlap.
    let subsystems: std::collections::HashSet<&str> =
        eligible.iter().map(|c| c.subsystem.as_str()).collect();

    if subsystems.len() == eligible.len() {
        // All from different subsystems — disjoint, no interference.
        let most_recent = eligible.iter().min_by_key(|c| c.delay_micros).expect("eligible is not empty");
        return AttributionResult {
            confidence: AttributionConfidence::Clean,
            primary_cause: Some(most_recent.experience_id),
            candidate_count: eligible.len(),
            interference_detected: false,
            reason: format!(
                "{} candidates from disjoint subsystems, most recent: {}",
                eligible.len(),
                most_recent.effect_kind
            ),
            learning_weight: 1.0,
            reason_code: AttributionReasonCode::DisjointSubsystems,
        };
    }

    // Same-subsystem overlap — check recency dominance.
    let mut sorted = eligible.clone();
    sorted.sort_by_key(|c| c.delay_micros);

    let closest = sorted[0];
    let next_closest = sorted[1];

    // A zero-delay cause is the strongest possible attribution signal:
    // the effect and outcome were observed at the same instant.
    // Use delay + 1 to handle the zero case (0*2=0 would fail the > check).
    if next_closest.delay_micros > closest.delay_micros.saturating_mul(2).saturating_add(1) {
        // Most recent is clearly dominant (> 2× closer).
        return AttributionResult {
            confidence: AttributionConfidence::Primary,
            primary_cause: Some(closest.experience_id),
            candidate_count: eligible.len(),
            interference_detected: true,
            reason: format!(
                "most recent cause dominant: {} (delay {}ms), next at {}ms",
                closest.effect_kind,
                closest.delay_micros / 1000,
                next_closest.delay_micros / 1000
            ),
            learning_weight: AttributionConfidence::Primary.learning_weight(),
            reason_code: AttributionReasonCode::MostRecentCause,
        };
    }

    // Close temporal overlap — shared credit.
    AttributionResult {
        confidence: AttributionConfidence::Shared,
        primary_cause: Some(closest.experience_id),
        candidate_count: eligible.len(),
        interference_detected: true,
        reason: format!(
            "{} overlapping causes within {}ms, credit shared",
            eligible.len(),
            eligibility_window_micros / 1000
        ),
        learning_weight: AttributionConfidence::Shared.learning_weight(),
        reason_code: AttributionReasonCode::OverlappingCauses,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    fn make_cause(id: u64, kind: &str, subsystem: &str, delay: i64) -> CandidateCause {
        CandidateCause {
            experience_id: id,
            decision_id: id * 10,
            effect_kind: kind.to_string(),
            created_ts_micros: 1_000_000 - delay,
            delay_micros: delay,
            is_most_recent: false,
            subsystem: subsystem.to_string(),
        }
    }

    #[test]
    fn no_candidates_returns_ambiguous() {
        let result = attribute_outcome(&[], 1_000_000, DEFAULT_ELIGIBILITY_WINDOW_MICROS);
        assert_eq!(result.confidence, AttributionConfidence::Ambiguous);
        assert_eq!(result.reason_code, AttributionReasonCode::NoCandidates);
        assert!(!result.confidence.allows_learning());
    }

    #[test]
    fn single_candidate_is_clean() {
        let causes = vec![make_cause(1, "advisory", "liveness", 30_000)];
        let result = attribute_outcome(&causes, 1_000_000, DEFAULT_ELIGIBILITY_WINDOW_MICROS);
        assert_eq!(result.confidence, AttributionConfidence::Clean);
        assert_eq!(result.primary_cause, Some(1));
        assert_eq!(result.learning_weight, 1.0);
    }

    #[test]
    fn disjoint_subsystems_is_clean() {
        let causes = vec![
            make_cause(1, "advisory", "liveness", 30_000),
            make_cause(2, "force_reservation", "conflict", 50_000),
        ];
        let result = attribute_outcome(&causes, 1_000_000, DEFAULT_ELIGIBILITY_WINDOW_MICROS);
        assert_eq!(result.confidence, AttributionConfidence::Clean);
        assert_eq!(
            result.reason_code,
            AttributionReasonCode::DisjointSubsystems
        );
        assert!(!result.interference_detected);
    }

    #[test]
    fn dominant_recency_is_primary() {
        let causes = vec![
            make_cause(1, "probe", "liveness", 10_000),     // closest
            make_cause(2, "advisory", "liveness", 100_000), // 10× farther
        ];
        let result = attribute_outcome(&causes, 1_000_000, DEFAULT_ELIGIBILITY_WINDOW_MICROS);
        assert_eq!(result.confidence, AttributionConfidence::Primary);
        assert_eq!(result.primary_cause, Some(1));
        assert!(result.interference_detected);
        assert!((result.learning_weight - 0.8).abs() < 1e-10);
    }

    #[test]
    fn close_overlap_is_shared() {
        let causes = vec![
            make_cause(1, "probe", "liveness", 10_000),
            make_cause(2, "advisory", "liveness", 15_000), // < 2× farther
        ];
        let result = attribute_outcome(&causes, 1_000_000, DEFAULT_ELIGIBILITY_WINDOW_MICROS);
        assert_eq!(result.confidence, AttributionConfidence::Shared);
        assert!(result.interference_detected);
        assert!((result.learning_weight - 0.5).abs() < 1e-10);
    }

    #[test]
    fn outside_window_filtered() {
        let causes = vec![make_cause(
            1,
            "advisory",
            "liveness",
            DEFAULT_ELIGIBILITY_WINDOW_MICROS + 1,
        )];
        let result = attribute_outcome(&causes, 1_000_000, DEFAULT_ELIGIBILITY_WINDOW_MICROS);
        assert_eq!(result.confidence, AttributionConfidence::Ambiguous);
    }

    #[test]
    fn confidence_learning_weights() {
        assert_eq!(AttributionConfidence::Clean.learning_weight(), 1.0);
        assert_eq!(AttributionConfidence::Primary.learning_weight(), 0.8);
        assert_eq!(AttributionConfidence::Shared.learning_weight(), 0.5);
        assert_eq!(AttributionConfidence::Ambiguous.learning_weight(), 0.0);
        assert_eq!(AttributionConfidence::Censored.learning_weight(), 0.0);
    }

    #[test]
    fn allows_learning_for_clean_primary_shared() {
        assert!(AttributionConfidence::Clean.allows_learning());
        assert!(AttributionConfidence::Primary.allows_learning());
        assert!(AttributionConfidence::Shared.allows_learning());
        assert!(!AttributionConfidence::Ambiguous.allows_learning());
        assert!(!AttributionConfidence::Censored.allows_learning());
    }

    #[test]
    fn serde_roundtrip() {
        let result = AttributionResult {
            confidence: AttributionConfidence::Primary,
            primary_cause: Some(42),
            candidate_count: 3,
            interference_detected: true,
            reason: "test attribution".to_string(),
            learning_weight: 0.8,
            reason_code: AttributionReasonCode::MostRecentCause,
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: AttributionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.confidence, AttributionConfidence::Primary);
        assert_eq!(decoded.primary_cause, Some(42));
    }
}
