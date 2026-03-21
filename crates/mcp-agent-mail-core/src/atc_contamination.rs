#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]
//! Feedback-loop contamination detection and anti-gaming safeguards
//! (br-0qt6e.3.12).
//!
//! Defines how ATC detects and handles evidence contamination from
//! strategic agent behavior, replayed events, intervention-caused
//! observability shifts, and other feedback-loop artifacts.
//!
//! # Threat Model
//!
//! | Threat                        | Observable Signal                          |
//! |-------------------------------|--------------------------------------------|
//! | Replayed/duplicated events    | Same trace_id appearing multiple times     |
//! | Spoofed liveness              | Agent sends keep-alive with no real work   |
//! | Performative acknowledgements | Ack without corresponding activity         |
//! | Message floods                | Burst rate exceeding normal cadence        |
//! | Reservation churn             | Rapid reserve-release cycles               |
//! | Operator selective labeling   | Manual resolution of specific outcomes     |
//! | Coordinated agent behavior    | Correlated activity across agents          |
//! | Intervention observability    | Outcome only visible because we probed     |
//!
//! # Evidence Quality States
//!
//! Each piece of evidence (experience row) carries a quality assessment:
//!
//! | Quality    | Meaning                                        |
//! |------------|------------------------------------------------|
//! | `Trusted`  | No contamination signals detected              |
//! | `Suspect`  | One or more signals triggered, under review    |
//! | `Quarantined` | Evidence excluded from learning, retained for audit |
//! | `Capped`   | Evidence influence limited (weight reduced)    |
//!
//! # Influence Limiting
//!
//! To prevent a small number of contaminated traces from dominating
//! policy updates, the system applies:
//! - **Per-agent influence cap**: No single agent contributes >20% of
//!   a stratum's total evidence weight.
//! - **Per-trace deduplication**: Same trace_id counted only once.
//! - **Burst detection**: Rapid event sequences (>10 events/minute from
//!   one agent) trigger automatic downweighting.

use serde::{Deserialize, Serialize};

/// Maximum fraction of stratum evidence from any single agent.
pub const MAX_AGENT_INFLUENCE_FRACTION: f64 = 0.20;

/// Events per minute threshold for burst detection.
pub const BURST_RATE_THRESHOLD: u32 = 10;

/// Minimum interval between events from same agent (microseconds)
/// below which duplication is suspected.
pub const MIN_EVENT_INTERVAL_MICROS: i64 = 100_000; // 100ms

/// Evidence quality assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceQuality {
    /// No contamination signals detected. Full learning weight.
    #[default]
    Trusted,
    /// One or more signals triggered. Learning proceeds with caution.
    Suspect,
    /// Evidence excluded from learning, retained for audit only.
    Quarantined,
    /// Evidence influence is limited (weight reduced).
    Capped,
}

impl EvidenceQuality {
    /// Weight multiplier for this quality level.
    #[must_use]
    pub const fn weight_multiplier(self) -> f64 {
        match self {
            Self::Trusted => 1.0,
            Self::Suspect => 0.5,
            Self::Quarantined => 0.0,
            Self::Capped => 0.3,
        }
    }

    /// Whether this quality level allows policy promotion evidence.
    #[must_use]
    pub const fn allows_promotion(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

impl std::fmt::Display for EvidenceQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trusted => write!(f, "trusted"),
            Self::Suspect => write!(f, "suspect"),
            Self::Quarantined => write!(f, "quarantined"),
            Self::Capped => write!(f, "capped"),
        }
    }
}

/// A contamination signal detected in the evidence stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContaminationSignal {
    /// Which type of contamination was detected.
    pub signal_type: ContaminationKind,
    /// Severity (0.0 = informational, 1.0 = critical).
    pub severity: f64,
    /// Which agent or trace triggered the signal.
    pub source: String,
    /// Human-readable description.
    pub description: String,
    /// When the signal was detected (microseconds).
    pub detected_ts_micros: i64,
}

/// Types of contamination signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContaminationKind {
    /// Same trace_id appeared multiple times.
    DuplicateTrace,
    /// Events arriving faster than physically plausible.
    EventBurst,
    /// Agent sending keepalive without real work.
    SpoofedLiveness,
    /// Ack without subsequent real activity.
    PerformativeAck,
    /// Rapid reserve-release cycles.
    ReservationChurn,
    /// Single agent dominating stratum evidence.
    InfluenceDomination,
    /// Outcome only observable because of ATC intervention.
    ObservabilityArtifact,
    /// Manual operator intervention during attribution window.
    OperatorIntervention,
    /// Correlated behavior across multiple agents.
    CoordinatedBehavior,
}

/// Per-agent contamination tracker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentContaminationTracker {
    /// Number of events from this agent in the current window.
    pub event_count_in_window: u32,
    /// Window start timestamp (microseconds).
    pub window_start_micros: i64,
    /// Last event timestamp from this agent.
    pub last_event_micros: i64,
    /// Duplicate trace IDs detected.
    pub duplicate_traces: u32,
    /// Total contamination signals for this agent.
    pub total_signals: u32,
    /// Current quality assessment for evidence from this agent.
    pub quality: EvidenceQuality,
    /// Fraction of stratum evidence attributed to this agent.
    pub influence_fraction: f64,
}

impl AgentContaminationTracker {
    /// Record a new event from this agent and check for contamination.
    pub fn record_event(
        &mut self,
        ts_micros: i64,
        window_duration_micros: i64,
    ) -> Vec<ContaminationSignal> {
        let mut signals = Vec::new();

        // Check for burst. Use saturating_sub to handle clock skew safely.
        if ts_micros.saturating_sub(self.window_start_micros) > window_duration_micros {
            // Reset window.
            self.event_count_in_window = 0;
            self.window_start_micros = ts_micros;
        }
        self.event_count_in_window += 1;

        // Only compute burst rate after enough events AND enough elapsed time.
        // This prevents false burst signals when a few events arrive
        // near-simultaneously after a window reset.
        // With min 3 events and min 1 second observation, the worst-case
        // rate from near-instant events is 3/min × 60 = 180/min — still a
        // valid signal. True bursts (11 events in 10s = 66/min) are detected
        // reliably once the 1-second floor is passed.
        const MIN_BURST_OBSERVATION_MICROS: i64 = 1_000_000; // 1 second
        const MIN_BURST_EVENT_COUNT: u32 = 3;
        let events_per_minute =
            if self.event_count_in_window >= MIN_BURST_EVENT_COUNT && window_duration_micros > 0 {
                let actual_elapsed = ts_micros.saturating_sub(self.window_start_micros);
                let elapsed_micros = actual_elapsed.max(MIN_BURST_OBSERVATION_MICROS);
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let rate = {
                    let elapsed_minutes = elapsed_micros as f64 / 60_000_000.0;
                    (f64::from(self.event_count_in_window) / elapsed_minutes) as u32
                };
                rate
            } else {
                0 // not enough events to compute a rate
            };

        if events_per_minute > BURST_RATE_THRESHOLD {
            signals.push(ContaminationSignal {
                signal_type: ContaminationKind::EventBurst,
                severity: 0.6,
                source: String::new(),
                description: format!("{events_per_minute} events/min exceeds threshold"),
                detected_ts_micros: ts_micros,
            });
        }

        // Check for suspiciously rapid events.
        if self.last_event_micros > 0 {
            let interval = ts_micros.saturating_sub(self.last_event_micros);
            if interval > 0 && interval < MIN_EVENT_INTERVAL_MICROS {
                signals.push(ContaminationSignal {
                    signal_type: ContaminationKind::DuplicateTrace,
                    severity: 0.4,
                    source: String::new(),
                    description: format!(
                        "events {interval}μs apart (< {MIN_EVENT_INTERVAL_MICROS}μs threshold)"
                    ),
                    detected_ts_micros: ts_micros,
                });
            }
        }

        self.last_event_micros = ts_micros;
        self.total_signals = self
            .total_signals
            .saturating_add(u32::try_from(signals.len()).unwrap_or(u32::MAX));

        // Update quality based on signal count.
        self.quality = if self.total_signals == 0 {
            EvidenceQuality::Trusted
        } else if self.total_signals <= 3 {
            EvidenceQuality::Suspect
        } else if self.total_signals <= 10 {
            EvidenceQuality::Capped
        } else {
            EvidenceQuality::Quarantined
        };

        signals
    }

    /// Check if this agent's influence fraction exceeds the cap.
    #[must_use]
    pub fn is_influence_capped(&self) -> bool {
        self.influence_fraction > MAX_AGENT_INFLUENCE_FRACTION
    }

    /// Reset the tracker (e.g., on regime change).
    pub const fn reset(&mut self) {
        self.event_count_in_window = 0;
        self.duplicate_traces = 0;
        self.total_signals = 0;
        self.quality = EvidenceQuality::Trusted;
    }
}

/// Decay rule for contamination state.
///
/// Contamination signals decay over time so that past issues don't
/// permanently poison the evidence stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DecayRule {
    /// How many clean observations to see before quality upgrades.
    pub clean_observations_for_upgrade: u32,
    /// Maximum age (seconds) before signals are forgotten.
    pub max_signal_age_secs: u64,
    /// Whether re-entry from Quarantined requires explicit approval.
    pub quarantine_requires_approval: bool,
}

impl Default for DecayRule {
    fn default() -> Self {
        Self {
            clean_observations_for_upgrade: 50,
            max_signal_age_secs: 3600, // 1 hour
            quarantine_requires_approval: true,
        }
    }
}

/// Compute the effective learning weight for evidence from an agent
/// given its contamination state and attribution confidence.
#[must_use]
pub fn effective_weight(
    quality: EvidenceQuality,
    attribution_confidence: f64,
    influence_fraction: f64,
) -> f64 {
    let base = quality.weight_multiplier() * attribution_confidence;

    // Apply influence cap: if agent dominates the stratum, reduce weight.
    if influence_fraction > MAX_AGENT_INFLUENCE_FRACTION {
        let cap_factor = MAX_AGENT_INFLUENCE_FRACTION / influence_fraction.max(1e-10);
        base * cap_factor
    } else {
        base
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_evidence_has_full_weight() {
        assert!((EvidenceQuality::Trusted.weight_multiplier() - 1.0).abs() < 1e-10);
        assert!(EvidenceQuality::Trusted.allows_promotion());
    }

    #[test]
    fn quarantined_evidence_has_zero_weight() {
        assert!((EvidenceQuality::Quarantined.weight_multiplier()).abs() < 1e-10);
        assert!(!EvidenceQuality::Quarantined.allows_promotion());
    }

    #[test]
    fn suspect_evidence_half_weight() {
        assert!((EvidenceQuality::Suspect.weight_multiplier() - 0.5).abs() < 1e-10);
        assert!(!EvidenceQuality::Suspect.allows_promotion());
    }

    #[test]
    fn new_tracker_is_trusted() {
        let tracker = AgentContaminationTracker::default();
        assert_eq!(tracker.quality, EvidenceQuality::Trusted);
        assert_eq!(tracker.total_signals, 0);
    }

    #[test]
    fn rapid_events_trigger_burst_signal() {
        let mut tracker = AgentContaminationTracker::default();
        // Send 15 events in 1 second.
        for i in 0..15 {
            tracker.record_event(i * 50_000, 60_000_000); // 50ms apart, 1-min window
        }
        assert!(tracker.total_signals > 0);
        assert_ne!(tracker.quality, EvidenceQuality::Trusted);
    }

    #[test]
    fn normal_rate_stays_trusted() {
        let mut tracker = AgentContaminationTracker::default();
        // Send events well apart (5 minutes between each, 10-minute window).
        // This is well below the 10 events/min burst threshold.
        let window = 600_000_000_i64; // 10 minutes
        let interval = 300_000_000_i64; // 5 minutes between events

        // First event resets the window.
        tracker.window_start_micros = window; // start window at t=10min
        let signals = tracker.record_event(window + interval, window);
        assert!(signals.is_empty(), "event 0: {signals:?}");

        let signals = tracker.record_event(window + interval * 2, window);
        assert!(signals.is_empty(), "event 1: {signals:?}");

        assert_eq!(tracker.quality, EvidenceQuality::Trusted);
    }

    #[test]
    fn burst_detection_uses_actual_elapsed_time_not_full_window_average() {
        let mut tracker = AgentContaminationTracker::default();
        let window = 600_000_000_i64; // 10 minutes

        for i in 0..11 {
            let ts = 1_000_000 + i * 1_000_000; // 11 events in 10 seconds
            let _ = tracker.record_event(ts, window);
        }

        assert!(
            tracker.total_signals > 0,
            "true burst should not be hidden by averaging over the full window"
        );
    }

    #[test]
    fn influence_cap_reduces_weight() {
        let w = effective_weight(EvidenceQuality::Trusted, 1.0, 0.5);
        // Agent has 50% influence, cap is 20%. Weight should be reduced.
        assert!(w < 1.0, "weight={w} should be < 1.0");
        assert!((w - 0.4).abs() < 1e-10); // 0.20/0.50 = 0.4
    }

    #[test]
    fn below_influence_cap_no_reduction() {
        let w = effective_weight(EvidenceQuality::Trusted, 1.0, 0.15);
        assert!((w - 1.0).abs() < 1e-10);
    }

    #[test]
    fn reset_restores_trusted() {
        let mut tracker = AgentContaminationTracker {
            total_signals: 5,
            quality: EvidenceQuality::Capped,
            ..Default::default()
        };
        tracker.reset();
        assert_eq!(tracker.quality, EvidenceQuality::Trusted);
        assert_eq!(tracker.total_signals, 0);
    }

    #[test]
    fn decay_rule_defaults() {
        let rule = DecayRule::default();
        assert_eq!(rule.clean_observations_for_upgrade, 50);
        assert_eq!(rule.max_signal_age_secs, 3600);
        assert!(rule.quarantine_requires_approval);
    }

    #[test]
    fn serde_roundtrip() {
        let signal = ContaminationSignal {
            signal_type: ContaminationKind::EventBurst,
            severity: 0.6,
            source: "TestAgent".to_string(),
            description: "burst detected".to_string(),
            detected_ts_micros: 1_000_000,
        };
        let json = serde_json::to_string(&signal).unwrap();
        let decoded: ContaminationSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.signal_type, ContaminationKind::EventBurst);
    }
}
