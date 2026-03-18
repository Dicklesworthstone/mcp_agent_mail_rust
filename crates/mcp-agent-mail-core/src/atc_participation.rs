//! Thread participation graph and session-synthesis features for ATC learning
//! (br-0qt6e.2.8).
//!
//! This module defines the participation types and derived features that the
//! learning pipeline consumes. The actual graph lives in the server's ATC
//! kernel (`atc.rs`), but the **types** and **feature extraction** live here
//! so the core crate's learning pipeline can consume them without depending
//! on the server crate.
//!
//! # How Thread Participation Influences ATC Decisions
//!
//! Thread participation is not an ornamental metric. It feeds into ATC
//! decision-making at three levels:
//!
//! 1. **Conflict risk assessment**: Agents sharing multiple threads are at
//!    elevated conflict risk because they're likely working on related tasks.
//!    The participation graph's [`high_risk_pairs`] computation feeds into
//!    the conflict subsystem's posterior update, increasing the prior
//!    probability of `MildOverlap` or `SevereCollision` for those pairs.
//!
//! 2. **Advisory targeting**: When ATC detects a liveness issue, the
//!    participation graph identifies which other agents are coordinating
//!    with the suspect agent. Advisories are sent to those co-participants
//!    first, not broadcast to all agents.
//!
//! 3. **Probe prioritization**: Agents with high participation count
//!    (many active threads) are probed more urgently when suspected
//!    because their death would affect more coordination flows.
//!
//! # Event Flow
//!
//! ```text
//! Message sent/received ──► note_message_sent/received() ──► ThreadParticipationGraph
//!                                                                │
//!                              ┌────────────────────────────────┘
//!                              ▼
//!                    ParticipationSnapshot
//!                              │
//!                    ┌─────────┼─────────┐
//!                    ▼         ▼         ▼
//!              Conflict   Advisory   Probe
//!              risk       targeting  priority
//!              prior      selection  scoring
//! ```
//!
//! # Determinism Contract
//!
//! Participation features are computed from the graph at snapshot time.
//! Given the same sequence of `record_participation` calls, the same
//! snapshot is produced. No randomization.

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// Participation snapshot (learning-pipeline input)
// ──────────────────────────────────────────────────────────────────────

/// A point-in-time snapshot of participation state for a single agent.
///
/// This is the type that the learning feature pipeline consumes.
/// It is extracted from the live [`ThreadParticipationGraph`] in the
/// server's ATC kernel and passed to the experience feature vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipationSnapshot {
    /// Agent name.
    pub agent: String,
    /// Number of threads this agent is currently participating in.
    pub thread_count: u32,
    /// Number of distinct co-participants across all threads.
    pub co_participant_count: u32,
    /// Number of high-risk pairs this agent is part of (shared >= 2 threads).
    pub high_risk_pair_count: u32,
    /// Maximum number of threads shared with any single other agent.
    pub max_shared_threads: u32,
    /// Agent with whom this agent shares the most threads (if any).
    pub most_connected_agent: Option<String>,
}

/// Features derived from participation state for the learning pipeline.
///
/// These map into the [`FeatureVector`] and [`FeatureExtension`] fields
/// defined in `experience.rs`. They are quantized to basis points or
/// small integers for compact storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipationFeatures {
    /// Thread count (capped at u8::MAX = 255).
    pub thread_count: u8,
    /// Co-participant count (capped at u8::MAX).
    pub co_participant_count: u8,
    /// High-risk pair count (capped at u8::MAX).
    pub high_risk_pair_count: u8,
    /// Max shared threads with any single agent (capped at u8::MAX).
    pub max_shared_threads: u8,
    /// Coordination pressure score (0–10000 basis points).
    ///
    /// Higher values indicate the agent is deeply embedded in
    /// coordination flows and its disruption would cascade.
    pub coordination_pressure_bp: u16,
}

impl ParticipationFeatures {
    /// Compute features from a participation snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: &ParticipationSnapshot) -> Self {
        let thread_count = u8::try_from(snapshot.thread_count).unwrap_or(u8::MAX);
        let co_participant_count = u8::try_from(snapshot.co_participant_count).unwrap_or(u8::MAX);
        let high_risk_pair_count = u8::try_from(snapshot.high_risk_pair_count).unwrap_or(u8::MAX);
        let max_shared_threads = u8::try_from(snapshot.max_shared_threads).unwrap_or(u8::MAX);

        // Coordination pressure: geometric mean of thread count and co-participants,
        // scaled to basis points. An agent in 10 threads with 10 co-participants
        // would score ~10000 bp. An agent in 1 thread with 1 co-participant scores ~100 bp.
        let pressure_raw =
            f64::from(snapshot.thread_count) * f64::from(snapshot.co_participant_count);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pressure_scaled = if pressure_raw > 0.0 {
            (pressure_raw.sqrt() * 1000.0).min(10000.0) as u16
        } else {
            0
        };

        Self {
            thread_count,
            co_participant_count,
            high_risk_pair_count,
            max_shared_threads,
            coordination_pressure_bp: pressure_scaled,
        }
    }

    /// Zero features (agent with no participation).
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            thread_count: 0,
            co_participant_count: 0,
            high_risk_pair_count: 0,
            max_shared_threads: 0,
            coordination_pressure_bp: 0,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Session synthesis event types
// ──────────────────────────────────────────────────────────────────────

/// Events that the session synthesis absorbs to build coordination context.
///
/// These are the canonical event types that feed into both the
/// `ThreadParticipationGraph` and the session summary. The same
/// production event stream drives both — no separate reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipationEvent {
    /// An agent sent a message in a thread.
    MessageSent {
        agent: String,
        thread_id: String,
        recipients: Vec<String>,
        timestamp_micros: i64,
    },
    /// An agent received a message in a thread.
    MessageReceived {
        agent: String,
        thread_id: String,
        timestamp_micros: i64,
    },
    /// An agent acknowledged a message.
    MessageAcknowledged {
        agent: String,
        thread_id: String,
        timestamp_micros: i64,
    },
    /// An agent was mentioned in an advisory sent by ATC.
    AdvisorySent {
        target_agent: String,
        thread_id: Option<String>,
        timestamp_micros: i64,
    },
    /// An agent was probed by ATC.
    ProbeSent {
        target_agent: String,
        timestamp_micros: i64,
    },
}

impl ParticipationEvent {
    /// Extract the primary agent name from this event.
    #[must_use]
    pub fn agent(&self) -> &str {
        match self {
            Self::MessageSent { agent, .. }
            | Self::MessageReceived { agent, .. }
            | Self::MessageAcknowledged { agent, .. } => agent,
            Self::AdvisorySent { target_agent, .. }
            | Self::ProbeSent { target_agent, .. } => target_agent,
        }
    }

    /// Extract the thread ID if this event is thread-scoped.
    #[must_use]
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::MessageSent { thread_id, .. }
            | Self::MessageReceived { thread_id, .. }
            | Self::MessageAcknowledged { thread_id, .. } => Some(thread_id),
            Self::AdvisorySent { thread_id, .. } => thread_id.as_deref(),
            Self::ProbeSent { .. } => None,
        }
    }

    /// Extract the timestamp.
    #[must_use]
    pub const fn timestamp_micros(&self) -> i64 {
        match self {
            Self::MessageSent { timestamp_micros, .. }
            | Self::MessageReceived { timestamp_micros, .. }
            | Self::MessageAcknowledged { timestamp_micros, .. }
            | Self::AdvisorySent { timestamp_micros, .. }
            | Self::ProbeSent { timestamp_micros, .. } => *timestamp_micros,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Participation risk signals
// ──────────────────────────────────────────────────────────────────────

/// Risk signals derived from participation patterns.
///
/// These are consumed by the conflict subsystem to adjust its posterior
/// priors and by the probe scheduler to prioritize probes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipationRiskSignals {
    /// Agent pairs with elevated conflict risk (shared >= 2 threads).
    pub high_risk_pairs: Vec<RiskPair>,
    /// Agents with high coordination pressure (top quartile).
    pub high_pressure_agents: Vec<String>,
    /// Total number of active threads across all agents.
    pub total_active_threads: u32,
    /// Total number of participating agents.
    pub total_participating_agents: u32,
}

/// A pair of agents with elevated conflict risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskPair {
    /// First agent in the pair.
    pub agent_a: String,
    /// Second agent in the pair.
    pub agent_b: String,
    /// Number of threads they share.
    pub shared_thread_count: u32,
}

// ──────────────────────────────────────────────────────────────────────
// Feature pipeline integration
// ──────────────────────────────────────────────────────────────────────

/// How participation features influence ATC decision features.
///
/// This is the documented mapping from participation state to the
/// learning feature pipeline. Each field describes what the feature
/// means for downstream decisions.
pub const PARTICIPATION_FEATURE_MAPPING: &[FeatureMapping] = &[
    FeatureMapping {
        feature_name: "thread_count",
        source: "ParticipationSnapshot.thread_count",
        influences: "Probe priority: agents in more threads probed more urgently. \
                     Advisory scope: agents in many threads receive broader advisories.",
        quantization: "Capped at u8::MAX (255). Typical range 0–20.",
    },
    FeatureMapping {
        feature_name: "co_participant_count",
        source: "ParticipationSnapshot.co_participant_count",
        influences: "Conflict risk prior: more co-participants = higher overlap probability. \
                     Advisory targeting: advisories sent to co-participants first.",
        quantization: "Capped at u8::MAX. Typical range 0–30.",
    },
    FeatureMapping {
        feature_name: "high_risk_pair_count",
        source: "ParticipationSnapshot.high_risk_pair_count",
        influences: "Conflict subsystem posterior: elevates prior for MildOverlap/SevereCollision. \
                     Deadlock detection: high-risk pairs checked first for circular dependencies.",
        quantization: "Capped at u8::MAX. Typical range 0–5.",
    },
    FeatureMapping {
        feature_name: "coordination_pressure_bp",
        source: "sqrt(thread_count * co_participant_count) * 1000",
        influences: "Overall risk tier assignment: high pressure → higher risk tier → \
                     more conservative loss matrices. Probe budget allocation: high-pressure \
                     agents get larger share of probe budget.",
        quantization: "0–10000 basis points. 0 = isolated, 10000 = deeply embedded.",
    },
];

/// Mapping from a participation feature to ATC decision influence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeatureMapping {
    /// Feature name in the learning pipeline.
    pub feature_name: &'static str,
    /// Where this feature comes from.
    pub source: &'static str,
    /// How this feature influences ATC decisions (human-readable).
    pub influences: &'static str,
    /// How the raw value is quantized for storage.
    pub quantization: &'static str,
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_features() {
        let f = ParticipationFeatures::zero();
        assert_eq!(f.thread_count, 0);
        assert_eq!(f.co_participant_count, 0);
        assert_eq!(f.coordination_pressure_bp, 0);
    }

    #[test]
    fn features_from_active_agent() {
        let snapshot = ParticipationSnapshot {
            agent: "GreenCastle".to_string(),
            thread_count: 5,
            co_participant_count: 8,
            high_risk_pair_count: 2,
            max_shared_threads: 3,
            most_connected_agent: Some("BlueLake".to_string()),
        };
        let features = ParticipationFeatures::from_snapshot(&snapshot);
        assert_eq!(features.thread_count, 5);
        assert_eq!(features.co_participant_count, 8);
        assert_eq!(features.high_risk_pair_count, 2);
        assert_eq!(features.max_shared_threads, 3);
        // sqrt(5 * 8) * 1000 = sqrt(40) * 1000 ≈ 6324
        assert!(features.coordination_pressure_bp > 6000);
        assert!(features.coordination_pressure_bp < 6500);
    }

    #[test]
    fn features_capped_at_limits() {
        let snapshot = ParticipationSnapshot {
            agent: "TestAgent".to_string(),
            thread_count: 1000,
            co_participant_count: 500,
            high_risk_pair_count: 300,
            max_shared_threads: 400,
            most_connected_agent: None,
        };
        let features = ParticipationFeatures::from_snapshot(&snapshot);
        assert_eq!(features.thread_count, 255);
        assert_eq!(features.co_participant_count, 255);
        assert_eq!(features.high_risk_pair_count, 255);
        assert_eq!(features.max_shared_threads, 255);
        assert_eq!(features.coordination_pressure_bp, 10000); // capped
    }

    #[test]
    fn participation_event_accessors() {
        let event = ParticipationEvent::MessageSent {
            agent: "GreenCastle".to_string(),
            thread_id: "FEAT-123".to_string(),
            recipients: vec!["BlueLake".to_string()],
            timestamp_micros: 42_000_000,
        };
        assert_eq!(event.agent(), "GreenCastle");
        assert_eq!(event.thread_id(), Some("FEAT-123"));
        assert_eq!(event.timestamp_micros(), 42_000_000);
    }

    #[test]
    fn probe_event_has_no_thread() {
        let event = ParticipationEvent::ProbeSent {
            target_agent: "BlueLake".to_string(),
            timestamp_micros: 50_000_000,
        };
        assert_eq!(event.agent(), "BlueLake");
        assert!(event.thread_id().is_none());
    }

    #[test]
    fn risk_pair_serde_roundtrip() {
        let pair = RiskPair {
            agent_a: "GreenCastle".to_string(),
            agent_b: "BlueLake".to_string(),
            shared_thread_count: 3,
        };
        let json = serde_json::to_string(&pair).unwrap();
        let decoded: RiskPair = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, pair);
    }

    #[test]
    fn feature_mapping_documentation_complete() {
        assert_eq!(PARTICIPATION_FEATURE_MAPPING.len(), 4);
        for mapping in PARTICIPATION_FEATURE_MAPPING {
            assert!(!mapping.feature_name.is_empty());
            assert!(!mapping.influences.is_empty());
            assert!(!mapping.quantization.is_empty());
        }
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snapshot = ParticipationSnapshot {
            agent: "TestAgent".to_string(),
            thread_count: 5,
            co_participant_count: 3,
            high_risk_pair_count: 1,
            max_shared_threads: 2,
            most_connected_agent: Some("Partner".to_string()),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let decoded: ParticipationSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn isolated_agent_zero_pressure() {
        let snapshot = ParticipationSnapshot {
            agent: "LoneWolf".to_string(),
            thread_count: 0,
            co_participant_count: 0,
            high_risk_pair_count: 0,
            max_shared_threads: 0,
            most_connected_agent: None,
        };
        let features = ParticipationFeatures::from_snapshot(&snapshot);
        assert_eq!(features.coordination_pressure_bp, 0);
    }

    #[test]
    fn single_thread_low_pressure() {
        let snapshot = ParticipationSnapshot {
            agent: "Newbie".to_string(),
            thread_count: 1,
            co_participant_count: 1,
            high_risk_pair_count: 0,
            max_shared_threads: 1,
            most_connected_agent: Some("Mentor".to_string()),
        };
        let features = ParticipationFeatures::from_snapshot(&snapshot);
        // sqrt(1 * 1) * 1000 = 1000
        assert_eq!(features.coordination_pressure_bp, 1000);
    }
}
