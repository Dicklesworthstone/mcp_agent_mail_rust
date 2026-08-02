//! Serialized operator-facing DTOs required by the shared dashboard modules.
//!
//! These intentionally contain no configuration, filesystem, database, or
//! process behavior. Their JSON shape matches the native Agent Mail core DTOs.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHealthMetricKind {
    AckDiscipline,
    ReservationDiscipline,
    ContactPolicyCompliance,
    ResponseTime,
    ActivityRecency,
}

impl AgentHealthMetricKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AckDiscipline => "Ack discipline",
            Self::ReservationDiscipline => "Reservation discipline",
            Self::ContactPolicyCompliance => "Contact policy",
            Self::ResponseTime => "Response time",
            Self::ActivityRecency => "Activity recency",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentHealthGrade {
    A,
    B,
    C,
    D,
    F,
}

impl AgentHealthGrade {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
            Self::F => "F",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentHealthMetric {
    pub kind: AgentHealthMetricKind,
    pub available: bool,
    pub raw_score: u8,
    pub weight_bp: u16,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentHealthScorecard {
    pub score: u8,
    pub grade: AgentHealthGrade,
    pub observed_weight_bp: u16,
    pub decision_count: u64,
    pub metrics: Vec<AgentHealthMetric>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalySeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EvidenceLedgerEntry {
    #[serde(default)]
    pub seq: u64,
    pub ts_micros: i64,
    pub decision_id: String,
    pub decision_point: String,
    pub action: String,
    pub confidence: f64,
    pub evidence: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_loss: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correct: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::{AgentHealthGrade, AgentHealthMetric, AgentHealthMetricKind, AgentHealthScorecard};

    #[test]
    fn health_scorecard_contract_round_trips() {
        let scorecard = AgentHealthScorecard {
            score: 81,
            grade: AgentHealthGrade::B,
            observed_weight_bp: 10_000,
            decision_count: 42,
            metrics: vec![AgentHealthMetric {
                kind: AgentHealthMetricKind::AckDiscipline,
                available: true,
                raw_score: 90,
                weight_bp: 3_000,
                evidence: "public aggregate".to_string(),
            }],
        };
        let json = serde_json::to_string(&scorecard).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentHealthScorecard>(&json).unwrap(),
            scorecard
        );
    }
}
