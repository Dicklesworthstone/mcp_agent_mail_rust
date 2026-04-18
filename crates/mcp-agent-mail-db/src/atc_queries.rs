//! ATC policy engine read path — rate-limited queries for rollups and open
//! experiences.  Designed to avoid contending with the write hot path.

use asupersync::{Cx, Outcome};
use sqlmodel_core::{Row, Value};

use crate::error::DbError;
use crate::pool::DbPool;

// ── Rollup read path ────────────────────────────────────────────────

/// Typed rollup row for policy engine consumption.
#[derive(Debug, Clone)]
pub struct RollupEntry {
    pub stratum_key: String,
    pub subsystem: String,
    pub effect_kind: String,
    pub risk_tier: i32,
    pub total_count: i64,
    pub resolved_count: i64,
    pub censored_count: i64,
    pub expired_count: i64,
    pub correct_count: i64,
    pub incorrect_count: i64,
    pub total_regret: f64,
    pub total_loss: f64,
    pub ewma_loss: f64,
    pub ewma_weight: f64,
    pub delay_sum_micros: i64,
    pub delay_count: i64,
    pub delay_max_micros: i64,
    pub last_updated_ts: i64,
}

const ROLLUP_SELECT_SQL: &str = "\
    SELECT stratum_key, subsystem, effect_kind, risk_tier, \
           total_count, resolved_count, censored_count, expired_count, \
           correct_count, incorrect_count, total_regret, total_loss, \
           ewma_loss, ewma_weight, delay_sum_micros, delay_count, \
           delay_max_micros, last_updated_ts \
    FROM atc_experience_rollups";

fn row_text_idx(row: &Row, idx: usize) -> String {
    row.get(idx)
        .and_then(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn row_i64_idx(row: &Row, idx: usize) -> i64 {
    row.get(idx)
        .and_then(|v| match v {
            Value::BigInt(n) => Some(*n),
            Value::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0)
}

fn row_f64_idx(row: &Row, idx: usize) -> f64 {
    row.get(idx)
        .and_then(|v| match v {
            Value::Double(n) => Some(*n),
            Value::Float(n) => Some(f64::from(*n)),
            Value::BigInt(n) => Some(*n as f64),
            Value::Int(n) => Some(f64::from(*n)),
            _ => None,
        })
        .unwrap_or(0.0)
}

fn decode_rollup_row(row: &Row) -> RollupEntry {
    #[allow(clippy::cast_possible_truncation)]
    RollupEntry {
        stratum_key: row_text_idx(row, 0),
        subsystem: row_text_idx(row, 1),
        effect_kind: row_text_idx(row, 2),
        risk_tier: row_i64_idx(row, 3) as i32,
        total_count: row_i64_idx(row, 4),
        resolved_count: row_i64_idx(row, 5),
        censored_count: row_i64_idx(row, 6),
        expired_count: row_i64_idx(row, 7),
        correct_count: row_i64_idx(row, 8),
        incorrect_count: row_i64_idx(row, 9),
        total_regret: row_f64_idx(row, 10),
        total_loss: row_f64_idx(row, 11),
        ewma_loss: row_f64_idx(row, 12),
        ewma_weight: row_f64_idx(row, 13),
        delay_sum_micros: row_i64_idx(row, 14),
        delay_count: row_i64_idx(row, 15),
        delay_max_micros: row_i64_idx(row, 16),
        last_updated_ts: row_i64_idx(row, 17),
    }
}

fn query_rollups_canonical(pool: &DbPool) -> Result<Vec<RollupEntry>, DbError> {
    let conn = crate::queries::open_canonical_atc_conn(pool, "query_rollups")?;
    let result = (|| {
        let rows = crate::queries::canonical_query_atc_rows(
            &conn,
            ROLLUP_SELECT_SQL,
            &[],
            "query_rollups",
        )?;
        Ok(rows.iter().map(|r| decode_rollup_row(r)).collect())
    })();
    crate::queries::close_canonical_db_conn(conn, "query_rollups connection");
    result
}

/// Read all rollup rows for the policy engine tick.
///
/// Uses a dedicated canonical connection for file-backed DBs to avoid
/// contending with the write hot path.
pub async fn query_rollups(cx: &Cx, pool: &DbPool) -> Outcome<Vec<RollupEntry>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        return match query_rollups_canonical(pool) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        };
    }
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(DbError::Sqlite(e.to_string())),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    match conn.query_sync(ROLLUP_SELECT_SQL, &[]) {
        Ok(rows) => Outcome::Ok(rows.iter().map(|r| decode_rollup_row(r)).collect()),
        Err(error) => Outcome::Err(DbError::Sqlite(format!("query_rollups: {error}"))),
    }
}

// ── Open experience read path ───────────────────────────────────────

/// Filter criteria for open experiences.
#[derive(Debug, Clone, Default)]
pub struct OpenExperienceFilter {
    /// Only return experiences for this subsystem.
    pub subsystem: Option<String>,
    /// Only return experiences for this subject (agent or thread).
    pub subject: Option<String>,
    /// Only return experiences for this project.
    pub project_key: Option<String>,
    /// Maximum number of rows to return.
    pub limit: Option<u32>,
}

const OPEN_EXPERIENCE_BASE_SQL: &str = "\
    SELECT experience_id, decision_id, effect_id, trace_id, claim_id, \
           evidence_id, state, subsystem, decision_class, subject, \
           project_key, policy_id, effect_kind, action, \
           expected_loss, created_ts, dispatched_ts, executed_ts \
    FROM atc_experiences \
    WHERE state IN ('open', 'dispatched', 'executed', 'planned')";

/// Lightweight open-experience summary for the policy engine.
#[derive(Debug, Clone)]
pub struct OpenExperienceSummary {
    pub experience_id: i64,
    pub decision_id: i64,
    pub effect_id: i64,
    pub trace_id: String,
    pub state: String,
    pub subsystem: String,
    pub decision_class: String,
    pub subject: String,
    pub project_key: Option<String>,
    pub effect_kind: String,
    pub action: String,
    pub expected_loss: f64,
    pub created_ts_micros: i64,
    pub dispatched_ts_micros: Option<i64>,
    pub executed_ts_micros: Option<i64>,
}

fn opt_i64(row: &Row, idx: usize) -> Option<i64> {
    row.get(idx).and_then(|v| match v {
        Value::BigInt(n) => Some(*n),
        Value::Int(n) => Some(i64::from(*n)),
        Value::Null => None,
        _ => None,
    })
}

fn decode_open_experience(row: &Row) -> OpenExperienceSummary {
    OpenExperienceSummary {
        experience_id: row_i64_idx(row, 0),
        decision_id: row_i64_idx(row, 1),
        effect_id: row_i64_idx(row, 2),
        trace_id: row_text_idx(row, 3),
        // skip claim_id (4), evidence_id (5)
        state: row_text_idx(row, 6),
        subsystem: row_text_idx(row, 7),
        decision_class: row_text_idx(row, 8),
        subject: row_text_idx(row, 9),
        project_key: row.get(10).and_then(|v| match v {
            Value::Text(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }),
        // skip policy_id (11)
        effect_kind: row_text_idx(row, 12),
        action: row_text_idx(row, 13),
        expected_loss: row_f64_idx(row, 14),
        created_ts_micros: row_i64_idx(row, 15),
        dispatched_ts_micros: opt_i64(row, 16),
        executed_ts_micros: opt_i64(row, 17),
    }
}

fn build_open_experience_query(
    filter: &OpenExperienceFilter,
) -> (String, Vec<Value>) {
    let mut sql = String::from(OPEN_EXPERIENCE_BASE_SQL);
    let mut params = Vec::new();
    if let Some(ref subsystem) = filter.subsystem {
        sql.push_str(" AND subsystem = ?");
        params.push(Value::Text(subsystem.clone()));
    }
    if let Some(ref subject) = filter.subject {
        sql.push_str(" AND subject = ?");
        params.push(Value::Text(subject.clone()));
    }
    if let Some(ref project_key) = filter.project_key {
        sql.push_str(" AND project_key = ?");
        params.push(Value::Text(project_key.clone()));
    }
    sql.push_str(" ORDER BY created_ts DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    (sql, params)
}

fn query_open_experiences_canonical(
    pool: &DbPool,
    filter: &OpenExperienceFilter,
) -> Result<Vec<OpenExperienceSummary>, DbError> {
    let conn = crate::queries::open_canonical_atc_conn(pool, "query_open_experiences")?;
    let result = (|| {
        let (sql, params) = build_open_experience_query(filter);
        let rows = crate::queries::canonical_query_atc_rows(
            &conn,
            &sql,
            &params,
            "query_open_experiences",
        )?;
        Ok(rows.iter().map(|r| decode_open_experience(r)).collect())
    })();
    crate::queries::close_canonical_db_conn(conn, "query_open_experiences connection");
    result
}

/// Read open/non-terminal experiences matching optional filters.
///
/// Returns lightweight summaries — the policy engine doesn't need the
/// full feature vectors or outcome payloads for state estimation.
pub async fn query_open_experiences(
    cx: &Cx,
    pool: &DbPool,
    filter: &OpenExperienceFilter,
) -> Outcome<Vec<OpenExperienceSummary>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        return match query_open_experiences_canonical(pool, filter) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        };
    }
    let conn = match pool.acquire(cx).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(DbError::Sqlite(e.to_string())),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let (sql, params) = build_open_experience_query(filter);
    match conn.query_sync(&sql, &params) {
        Ok(rows) => Outcome::Ok(rows.iter().map(|r| decode_open_experience(r)).collect()),
        Err(error) => {
            Outcome::Err(DbError::Sqlite(format!("query_open_experiences: {error}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(names: Vec<&str>, values: Vec<Value>) -> Row {
        Row::new(names.into_iter().map(String::from).collect(), values)
    }

    #[test]
    fn decode_rollup_row_handles_all_fields() {
        let row = make_row(
            vec![
                "stratum_key", "subsystem", "effect_kind", "risk_tier",
                "total_count", "resolved_count", "censored_count", "expired_count",
                "correct_count", "incorrect_count", "total_regret", "total_loss",
                "ewma_loss", "ewma_weight", "delay_sum_micros", "delay_count",
                "delay_max_micros", "last_updated_ts",
            ],
            vec![
                Value::Text("liveness|probe|0".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe".to_string()),
                Value::BigInt(0),
                Value::BigInt(100),
                Value::BigInt(90),
                Value::BigInt(3),
                Value::BigInt(7),
                Value::BigInt(85),
                Value::BigInt(5),
                Value::Double(1.5),
                Value::Double(2.0),
                Value::Double(0.02),
                Value::Double(0.95),
                Value::BigInt(5_000_000),
                Value::BigInt(90),
                Value::BigInt(120_000),
                Value::BigInt(1_713_000_000_000_000),
            ],
        );
        let entry = decode_rollup_row(&row);
        assert_eq!(entry.stratum_key, "liveness|probe|0");
        assert_eq!(entry.subsystem, "liveness");
        assert_eq!(entry.effect_kind, "probe");
        assert_eq!(entry.risk_tier, 0);
        assert_eq!(entry.total_count, 100);
        assert_eq!(entry.resolved_count, 90);
        assert_eq!(entry.censored_count, 3);
        assert_eq!(entry.expired_count, 7);
        assert_eq!(entry.correct_count, 85);
        assert_eq!(entry.incorrect_count, 5);
        assert!((entry.total_regret - 1.5).abs() < f64::EPSILON);
        assert!((entry.total_loss - 2.0).abs() < f64::EPSILON);
        assert!((entry.ewma_loss - 0.02).abs() < f64::EPSILON);
        assert!((entry.ewma_weight - 0.95).abs() < f64::EPSILON);
        assert_eq!(entry.delay_sum_micros, 5_000_000);
        assert_eq!(entry.delay_count, 90);
        assert_eq!(entry.delay_max_micros, 120_000);
    }

    #[test]
    fn decode_open_experience_handles_nulls() {
        let row = make_row(
            vec![
                "experience_id", "decision_id", "effect_id", "trace_id",
                "claim_id", "evidence_id", "state", "subsystem",
                "decision_class", "subject", "project_key", "policy_id",
                "effect_kind", "action", "expected_loss", "created_ts",
                "dispatched_ts", "executed_ts",
            ],
            vec![
                Value::BigInt(42),
                Value::BigInt(7),
                Value::BigInt(1),
                Value::Text("trc-abc".to_string()),
                Value::Text("clm-1".to_string()),
                Value::Text("evi-1".to_string()),
                Value::Text("open".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe_check".to_string()),
                Value::Text("GreenCastle".to_string()),
                Value::Null, // project_key
                Value::Null, // policy_id
                Value::Text("probe".to_string()),
                Value::Text("DeclareAlive".to_string()),
                Value::Double(0.15),
                Value::BigInt(1_000_000),
                Value::Null, // dispatched_ts
                Value::Null, // executed_ts
            ],
        );
        let entry = decode_open_experience(&row);
        assert_eq!(entry.experience_id, 42);
        assert_eq!(entry.state, "open");
        assert_eq!(entry.subject, "GreenCastle");
        assert!(entry.project_key.is_none());
        assert!(entry.dispatched_ts_micros.is_none());
        assert!(entry.executed_ts_micros.is_none());
    }

    #[test]
    fn build_query_with_no_filter() {
        let filter = OpenExperienceFilter::default();
        let (sql, params) = build_open_experience_query(&filter);
        assert!(sql.contains("WHERE state IN"));
        assert!(sql.contains("ORDER BY created_ts DESC"));
        assert!(!sql.contains("LIMIT"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_query_with_all_filters() {
        let filter = OpenExperienceFilter {
            subsystem: Some("liveness".to_string()),
            subject: Some("GreenCastle".to_string()),
            project_key: Some("proj-a".to_string()),
            limit: Some(50),
        };
        let (sql, params) = build_open_experience_query(&filter);
        assert!(sql.contains("AND subsystem = ?"));
        assert!(sql.contains("AND subject = ?"));
        assert!(sql.contains("AND project_key = ?"));
        assert!(sql.contains("LIMIT 50"));
        assert_eq!(params.len(), 3);
    }
}
