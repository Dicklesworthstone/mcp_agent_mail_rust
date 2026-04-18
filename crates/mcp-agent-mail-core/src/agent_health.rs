//! Per-agent health scoring for operator-facing dashboards.
//!
//! The scorecard is intentionally transparent: every composite score carries
//! the sub-metric evidence needed for an operator to understand why an agent
//! was graded highly or flagged for attention.

use serde::{Deserialize, Serialize};

const ACK_DISCIPLINE_WEIGHT_BP: u16 = 3_000;
const RESERVATION_DISCIPLINE_WEIGHT_BP: u16 = 2_500;
const CONTACT_POLICY_WEIGHT_BP: u16 = 1_500;
const RESPONSE_TIME_WEIGHT_BP: u16 = 1_500;
const ACTIVITY_RECENCY_WEIGHT_BP: u16 = 1_500;

const MINUTE_MICROS: u64 = 60 * 1_000_000;
const HOUR_MICROS: u64 = 60 * MINUTE_MICROS;
const DAY_MICROS: u64 = 24 * HOUR_MICROS;

/// Weighted dimensions that feed the composite agent health grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHealthMetricKind {
    AckDiscipline,
    ReservationDiscipline,
    ContactPolicyCompliance,
    ResponseTime,
    ActivityRecency,
}

impl AgentHealthMetricKind {
    /// Human-friendly label for dashboards and drill-down surfaces.
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

    /// Fixed configured weight for the metric, in basis points.
    #[must_use]
    pub const fn weight_bp(self) -> u16 {
        match self {
            Self::AckDiscipline => ACK_DISCIPLINE_WEIGHT_BP,
            Self::ReservationDiscipline => RESERVATION_DISCIPLINE_WEIGHT_BP,
            Self::ContactPolicyCompliance => CONTACT_POLICY_WEIGHT_BP,
            Self::ResponseTime => RESPONSE_TIME_WEIGHT_BP,
            Self::ActivityRecency => ACTIVITY_RECENCY_WEIGHT_BP,
        }
    }
}

/// Letter-grade band for the composite score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHealthGrade {
    A,
    B,
    C,
    D,
    F,
}

impl AgentHealthGrade {
    /// Compute the grade band for a composite 0-100 score.
    #[must_use]
    pub const fn from_score(score: u8) -> Self {
        match score {
            90..=100 => Self::A,
            75..=89 => Self::B,
            60..=74 => Self::C,
            40..=59 => Self::D,
            _ => Self::F,
        }
    }

    /// Short label used in table badges.
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

/// Raw measurements gathered from the database for one agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHealthInputs {
    pub ack_on_time_count: u64,
    pub ack_late_count: u64,
    pub ack_pending_count: u64,
    pub ack_p50_latency_micros: Option<u64>,
    pub reservation_clean_count: u64,
    pub reservation_late_release_count: u64,
    pub reservation_expired_count: u64,
    pub reservation_active_count: u64,
    pub contact_policy_respected_count: Option<u64>,
    pub contact_policy_violation_count: Option<u64>,
    pub last_active_age_micros: Option<u64>,
    pub decision_count: u64,
}

/// One scored metric, including evidence surfaced to operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHealthMetric {
    pub kind: AgentHealthMetricKind,
    pub available: bool,
    pub raw_score: u8,
    pub weight_bp: u16,
    pub evidence: String,
}

impl AgentHealthMetric {
    /// Metric label for dashboards.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.kind.label()
    }

    /// Stable value string for detail panes.
    #[must_use]
    pub fn value_label(&self) -> String {
        if self.available {
            format!("{}/100", self.raw_score)
        } else {
            "n/a".to_string()
        }
    }
}

/// Composite scorecard shown in agent dashboards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHealthScorecard {
    pub score: u8,
    pub grade: AgentHealthGrade,
    pub observed_weight_bp: u16,
    pub decision_count: u64,
    pub metrics: Vec<AgentHealthMetric>,
}

impl AgentHealthScorecard {
    /// Compact badge such as `B 81`.
    #[must_use]
    pub fn badge(&self) -> String {
        format!("{} {}", self.grade.label(), self.score)
    }

    /// Whether the operator should review this agent soon.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        matches!(
            self.grade,
            AgentHealthGrade::C | AgentHealthGrade::D | AgentHealthGrade::F
        )
    }
}

/// Compute the full scorecard from observed inputs.
#[must_use]
pub fn compute_agent_health(inputs: &AgentHealthInputs) -> AgentHealthScorecard {
    let metrics = vec![
        ack_discipline_metric(inputs),
        reservation_discipline_metric(inputs),
        contact_policy_metric(inputs),
        response_time_metric(inputs),
        activity_recency_metric(inputs),
    ];
    let observed_weight_bp = metrics
        .iter()
        .filter(|metric| metric.available)
        .map(|metric| metric.weight_bp)
        .sum::<u16>();
    let weighted_points = metrics
        .iter()
        .filter(|metric| metric.available)
        .map(|metric| u32::from(metric.raw_score) * u32::from(metric.weight_bp))
        .sum::<u32>();
    let score = if observed_weight_bp == 0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            ((weighted_points + u32::from(observed_weight_bp) / 2) / u32::from(observed_weight_bp))
                as u8
        }
    };

    AgentHealthScorecard {
        score,
        grade: AgentHealthGrade::from_score(score),
        observed_weight_bp,
        decision_count: inputs.decision_count,
        metrics,
    }
}

fn ack_discipline_metric(inputs: &AgentHealthInputs) -> AgentHealthMetric {
    let total = inputs
        .ack_on_time_count
        .saturating_add(inputs.ack_late_count)
        .saturating_add(inputs.ack_pending_count);
    let raw_score = ratio_score(inputs.ack_on_time_count, total);
    let evidence = if total == 0 {
        "no ack-required deliveries in the scoring window".to_string()
    } else {
        format!(
            "{} on-time, {} late, {} pending",
            inputs.ack_on_time_count, inputs.ack_late_count, inputs.ack_pending_count
        )
    };
    metric(AgentHealthMetricKind::AckDiscipline, raw_score, evidence)
}

fn reservation_discipline_metric(inputs: &AgentHealthInputs) -> AgentHealthMetric {
    let bad_count = inputs
        .reservation_late_release_count
        .saturating_add(inputs.reservation_expired_count);
    let total = inputs.reservation_clean_count.saturating_add(bad_count);
    let raw_score = ratio_score(inputs.reservation_clean_count, total);
    let evidence = if total == 0 && inputs.reservation_active_count == 0 {
        "no completed reservations in the scoring window".to_string()
    } else {
        format!(
            "{} clean, {} late, {} expired, {} active",
            inputs.reservation_clean_count,
            inputs.reservation_late_release_count,
            inputs.reservation_expired_count,
            inputs.reservation_active_count
        )
    };
    metric(
        AgentHealthMetricKind::ReservationDiscipline,
        raw_score,
        evidence,
    )
}

fn contact_policy_metric(inputs: &AgentHealthInputs) -> AgentHealthMetric {
    let respected = inputs.contact_policy_respected_count.unwrap_or(0);
    let violations = inputs.contact_policy_violation_count.unwrap_or(0);
    let total = respected.saturating_add(violations);
    let raw_score = if inputs.contact_policy_respected_count.is_none()
        || inputs.contact_policy_violation_count.is_none()
    {
        None
    } else {
        ratio_score(respected, total)
    };
    let evidence = if raw_score.is_none() {
        "no inbound contact-policy evaluations in the scoring window".to_string()
    } else {
        format!("{respected} compliant deliveries, {violations} violations")
    };
    metric(
        AgentHealthMetricKind::ContactPolicyCompliance,
        raw_score,
        evidence,
    )
}

fn response_time_metric(inputs: &AgentHealthInputs) -> AgentHealthMetric {
    let raw_score = inputs.ack_p50_latency_micros.map(latency_score);
    let evidence = inputs.ack_p50_latency_micros.map_or_else(
        || "no acknowledged deliveries in the scoring window".to_string(),
        |latency| format!("p50 ack latency {}", format_duration(latency)),
    );
    metric(AgentHealthMetricKind::ResponseTime, raw_score, evidence)
}

fn activity_recency_metric(inputs: &AgentHealthInputs) -> AgentHealthMetric {
    let (raw_score, evidence) = inputs.last_active_age_micros.map_or_else(
        || (Some(0), "never active".to_string()),
        |age| {
            (
                Some(recency_score(age)),
                format!("last active {}", format_duration(age)),
            )
        },
    );
    metric(AgentHealthMetricKind::ActivityRecency, raw_score, evidence)
}

fn metric(
    kind: AgentHealthMetricKind,
    raw_score: Option<u8>,
    evidence: String,
) -> AgentHealthMetric {
    AgentHealthMetric {
        kind,
        available: raw_score.is_some(),
        raw_score: raw_score.unwrap_or(0),
        weight_bp: kind.weight_bp(),
        evidence,
    }
}

const fn ratio_score(good: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        Some(((good.saturating_mul(100) + total / 2) / total) as u8)
    }
}

const fn latency_score(latency_micros: u64) -> u8 {
    if latency_micros <= 5 * MINUTE_MICROS {
        100
    } else if latency_micros <= 15 * MINUTE_MICROS {
        92
    } else if latency_micros <= 30 * MINUTE_MICROS {
        84
    } else if latency_micros <= HOUR_MICROS {
        72
    } else if latency_micros <= 4 * HOUR_MICROS {
        56
    } else if latency_micros <= DAY_MICROS {
        32
    } else if latency_micros <= 3 * DAY_MICROS {
        16
    } else {
        0
    }
}

const fn recency_score(age_micros: u64) -> u8 {
    if age_micros <= 7 * DAY_MICROS {
        100
    } else if age_micros <= 14 * DAY_MICROS {
        80
    } else if age_micros <= 21 * DAY_MICROS {
        60
    } else if age_micros <= 30 * DAY_MICROS {
        40
    } else {
        0
    }
}

fn format_duration(duration_micros: u64) -> String {
    if duration_micros < MINUTE_MICROS {
        format!("{}s", duration_micros / 1_000_000)
    } else if duration_micros < HOUR_MICROS {
        format!("{}m", duration_micros / MINUTE_MICROS)
    } else if duration_micros < DAY_MICROS {
        format!("{}h", duration_micros / HOUR_MICROS)
    } else {
        format!("{}d", duration_micros / DAY_MICROS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric<'a>(
        scorecard: &'a AgentHealthScorecard,
        kind: AgentHealthMetricKind,
    ) -> &'a AgentHealthMetric {
        scorecard
            .metrics
            .iter()
            .find(|metric| metric.kind == kind)
            .expect("metric present")
    }

    #[test]
    fn grade_bands_match_bead_contract() {
        assert_eq!(AgentHealthGrade::from_score(90), AgentHealthGrade::A);
        assert_eq!(AgentHealthGrade::from_score(75), AgentHealthGrade::B);
        assert_eq!(AgentHealthGrade::from_score(60), AgentHealthGrade::C);
        assert_eq!(AgentHealthGrade::from_score(40), AgentHealthGrade::D);
        assert_eq!(AgentHealthGrade::from_score(39), AgentHealthGrade::F);
    }

    #[test]
    fn renormalizes_when_contact_metric_is_unavailable() {
        let scorecard = compute_agent_health(&AgentHealthInputs {
            ack_on_time_count: 9,
            ack_late_count: 1,
            ack_p50_latency_micros: Some(4 * MINUTE_MICROS),
            reservation_clean_count: 8,
            reservation_late_release_count: 2,
            last_active_age_micros: Some(2 * MINUTE_MICROS),
            decision_count: 12,
            ..AgentHealthInputs::default()
        });

        assert_eq!(scorecard.observed_weight_bp, 8_500);
        assert!(!metric(&scorecard, AgentHealthMetricKind::ContactPolicyCompliance).available);
        assert!(scorecard.score > 0);
        assert_eq!(scorecard.decision_count, 12);
    }

    #[test]
    fn response_time_score_is_monotonic() {
        assert!(latency_score(2 * MINUTE_MICROS) > latency_score(20 * MINUTE_MICROS));
        assert!(latency_score(20 * MINUTE_MICROS) > latency_score(2 * HOUR_MICROS));
        assert!(latency_score(2 * HOUR_MICROS) > latency_score(2 * DAY_MICROS));
    }

    #[test]
    fn activity_recency_score_is_monotonic() {
        assert_eq!(recency_score(2 * DAY_MICROS), 100);
        assert_eq!(recency_score(7 * DAY_MICROS), 100);
        assert!(recency_score(10 * DAY_MICROS) < recency_score(7 * DAY_MICROS));
        assert!(recency_score(20 * DAY_MICROS) < recency_score(10 * DAY_MICROS));
        assert!(recency_score(40 * DAY_MICROS) < recency_score(20 * DAY_MICROS));
    }

    #[test]
    fn better_ack_discipline_never_reduces_composite_score() {
        let worse = compute_agent_health(&AgentHealthInputs {
            ack_on_time_count: 6,
            ack_late_count: 2,
            ack_pending_count: 2,
            ack_p50_latency_micros: Some(10 * MINUTE_MICROS),
            reservation_clean_count: 4,
            reservation_active_count: 0,
            reservation_late_release_count: 1,
            reservation_expired_count: 0,
            contact_policy_respected_count: Some(8),
            contact_policy_violation_count: Some(2),
            last_active_age_micros: Some(15 * MINUTE_MICROS),
            decision_count: 8,
        });
        let better = compute_agent_health(&AgentHealthInputs {
            ack_on_time_count: 8,
            ack_late_count: 1,
            ack_pending_count: 1,
            ..AgentHealthInputs {
                ack_p50_latency_micros: Some(10 * MINUTE_MICROS),
                reservation_clean_count: 4,
                reservation_late_release_count: 1,
                reservation_expired_count: 0,
                contact_policy_respected_count: Some(8),
                contact_policy_violation_count: Some(2),
                last_active_age_micros: Some(15 * MINUTE_MICROS),
                decision_count: 8,
                ..AgentHealthInputs::default()
            }
        });

        assert!(better.score >= worse.score);
        assert!(
            metric(&better, AgentHealthMetricKind::AckDiscipline).raw_score
                >= metric(&worse, AgentHealthMetricKind::AckDiscipline).raw_score
        );
    }

    #[test]
    fn response_time_evidence_uses_p50_language() {
        let scorecard = compute_agent_health(&AgentHealthInputs {
            ack_p50_latency_micros: Some(15 * MINUTE_MICROS),
            ..AgentHealthInputs::default()
        });

        assert_eq!(
            metric(&scorecard, AgentHealthMetricKind::ResponseTime).evidence,
            "p50 ack latency 15m"
        );
    }
}
