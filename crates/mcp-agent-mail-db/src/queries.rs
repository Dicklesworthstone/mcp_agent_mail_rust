//! Database query operations
//!
//! CRUD operations for all models using `sqlmodel` with frankensqlite backend.
//!
//! These functions are the "DB truth" for the rest of the application: tools and
//! resources should rely on these helpers rather than embedding raw SQL.

#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::explicit_auto_deref)]

use crate::error::DbError;
use crate::models::{
    AgentLinkRow, AgentRow, FileReservationRow, InboxStatsRow, MessageRecipientRow, MessageRow,
    ProductRow, ProjectRow,
};
use crate::pool::DbPool;
use crate::timestamps::now_micros;
use asupersync::{CancelReason, Outcome};
use mcp_agent_mail_core::pattern_overlap::CompiledPattern;
use mcp_agent_mail_core::{
    ExperienceOutcome, ExperienceRow, ExperienceState, FEATURE_SCHEMA_VERSION, FeatureExtension,
    FeatureVector, NonExecutionReason, ResolutionKind, infer_feature_schema_version,
    migrate_feature_payload, validate_transition,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlmodel::prelude::*;
use sqlmodel_core::{Connection, Dialect, Error as SqlError, IsolationLevel, PreparedStatement};
use sqlmodel_core::{Row as SqlRow, TransactionOps, Value};
use sqlmodel_query::{raw_execute, raw_query};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

fn cache_scope_for_pool(pool: &DbPool) -> String {
    pool.sqlite_identity_key()
}

static MESSAGE_WRITE_SERIALIZER: LazyLock<asupersync::sync::Mutex<()>> =
    LazyLock::new(|| asupersync::sync::Mutex::new(()));

// =============================================================================
// ATC Leader Lease types
// =============================================================================

/// Result of attempting to acquire the ATC leader lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// This instance now holds the leader lease.
    Acquired,
    /// Another instance holds a non-expired lease.
    NotLeader {
        /// Instance ID of the current leader.
        holder: String,
        /// When the current lease expires (microseconds since epoch).
        expires_at_micros: i64,
    },
}

impl LeaseOutcome {
    /// Whether this instance acquired or retained the lease.
    #[must_use]
    pub const fn is_leader(&self) -> bool {
        matches!(self, Self::Acquired)
    }
}

// =============================================================================
// ATC Rollup Snapshot types
// =============================================================================

/// Result of taking an ATC rollup snapshot for backup/restore.
#[derive(Debug, Clone)]
pub struct RollupSnapshot {
    pub captured_ts_micros: i64,
    pub rollup_rows: i64,
    pub payload_sha256: String,
    pub payload: String,
}

fn sha256_hex(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let result = hasher.finalize();
    result.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

// =============================================================================
// Tracked query wrappers
// =============================================================================

struct TrackedConnection<'conn> {
    inner: &'conn crate::DbConn,
}

impl<'conn> TrackedConnection<'conn> {
    fn new(inner: &'conn crate::DbConn) -> Self {
        Self { inner }
    }
}

struct TrackedTransaction<'conn> {
    inner: <crate::DbConn as Connection>::Tx<'conn>,
}

impl TransactionOps for TrackedTransaction<'_> {
    fn query(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<Vec<SqlRow>, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.query(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn query_one(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<Option<SqlRow>, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.query_one(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn execute(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<u64, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.execute(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn savepoint(&self, cx: &Cx, name: &str) -> impl Future<Output = Outcome<(), SqlError>> + Send {
        self.inner.savepoint(cx, name)
    }

    fn rollback_to(
        &self,
        cx: &Cx,
        name: &str,
    ) -> impl Future<Output = Outcome<(), SqlError>> + Send {
        self.inner.rollback_to(cx, name)
    }

    fn release(&self, cx: &Cx, name: &str) -> impl Future<Output = Outcome<(), SqlError>> + Send {
        self.inner.release(cx, name)
    }

    fn commit(self, cx: &Cx) -> impl Future<Output = Outcome<(), SqlError>> + Send {
        self.inner.commit(cx)
    }

    fn rollback(self, cx: &Cx) -> impl Future<Output = Outcome<(), SqlError>> + Send {
        self.inner.rollback(cx)
    }
}

impl Connection for TrackedConnection<'_> {
    type Tx<'conn>
        = TrackedTransaction<'conn>
    where
        Self: 'conn;

    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn query(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<Vec<SqlRow>, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.query(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn query_one(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<Option<SqlRow>, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.query_one(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn execute(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<u64, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.execute(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn insert(
        &self,
        cx: &Cx,
        sql: &str,
        params: &[Value],
    ) -> impl Future<Output = Outcome<i64, SqlError>> + Send {
        let start = crate::tracking::query_timer();
        let fut = self.inner.insert(cx, sql, params);
        async move {
            let result = fut.await;
            let elapsed = crate::tracking::elapsed_us(start);
            crate::tracking::record_query(sql, elapsed);
            result
        }
    }

    fn batch(
        &self,
        cx: &Cx,
        statements: &[(String, Vec<Value>)],
    ) -> impl Future<Output = Outcome<Vec<u64>, SqlError>> + Send {
        let statements = statements.to_vec();
        async move {
            let mut results = Vec::with_capacity(statements.len());
            for (sql, params) in statements {
                let start = crate::tracking::query_timer();
                let out = self.inner.execute(cx, &sql, &params).await;
                let elapsed = crate::tracking::elapsed_us(start);
                crate::tracking::record_query(&sql, elapsed);
                match out {
                    Outcome::Ok(n) => results.push(n),
                    Outcome::Err(e) => return Outcome::Err(e),
                    Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                    Outcome::Panicked(p) => return Outcome::Panicked(p),
                }
            }
            Outcome::Ok(results)
        }
    }

    fn begin(&self, cx: &Cx) -> impl Future<Output = Outcome<Self::Tx<'_>, SqlError>> + Send {
        self.begin_with(cx, IsolationLevel::default())
    }

    fn begin_with(
        &self,
        cx: &Cx,
        isolation: IsolationLevel,
    ) -> impl Future<Output = Outcome<Self::Tx<'_>, SqlError>> + Send {
        let fut = self.inner.begin_with(cx, isolation);
        async move {
            match fut.await {
                Outcome::Ok(tx) => Outcome::Ok(TrackedTransaction { inner: tx }),
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
    }

    fn prepare(
        &self,
        cx: &Cx,
        sql: &str,
    ) -> impl Future<Output = Outcome<PreparedStatement, SqlError>> + Send {
        self.inner.prepare(cx, sql)
    }

    fn query_prepared(
        &self,
        cx: &Cx,
        stmt: &PreparedStatement,
        params: &[Value],
    ) -> impl Future<Output = Outcome<Vec<SqlRow>, SqlError>> + Send {
        self.query(cx, stmt.sql(), params)
    }

    fn execute_prepared(
        &self,
        cx: &Cx,
        stmt: &PreparedStatement,
        params: &[Value],
    ) -> impl Future<Output = Outcome<u64, SqlError>> + Send {
        self.execute(cx, stmt.sql(), params)
    }

    fn ping(&self, cx: &Cx) -> impl Future<Output = Outcome<(), SqlError>> + Send {
        self.inner.ping(cx)
    }

    async fn close(self, _cx: &Cx) -> sqlmodel_core::Result<()> {
        // TrackedConnection borrows the underlying connection; closing is a
        // no-op because we don't own the connection.
        Ok(())
    }
}

/// Execute a raw query using the tracked connection.
async fn traw_query(
    cx: &Cx,
    conn: &TrackedConnection<'_>,
    sql: &str,
    params: &[Value],
) -> Outcome<Vec<SqlRow>, SqlError> {
    raw_query(cx, conn, sql, params).await
}

/// Execute a raw statement using the tracked connection.
async fn traw_execute(
    cx: &Cx,
    conn: &TrackedConnection<'_>,
    sql: &str,
    params: &[Value],
) -> Outcome<u64, SqlError> {
    raw_execute(cx, conn, sql, params).await
}

// =============================================================================
// Project Queries
// =============================================================================

/// Generate a URL-safe slug from a human key (path).
#[must_use]
pub fn generate_slug(human_key: &str) -> String {
    // Directory-mode slugs are based on the resolved real path when one exists,
    // so symlink spellings for the same workspace converge on one project.
    mcp_agent_mail_core::compute_project_slug(human_key)
}

fn canonical_absolute_path_string(value: &str) -> Option<String> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return None;
    }
    std::fs::canonicalize(path)
        .ok()
        .map(|path| path.to_string_lossy().to_string())
}

fn human_keys_equivalent(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let Some(left_canonical) = canonical_absolute_path_string(left) else {
        return false;
    };
    let Some(right_canonical) = canonical_absolute_path_string(right) else {
        return false;
    };
    left_canonical == right_canonical
}

fn project_matches_human_key_alias(
    project: &ProjectRow,
    raw_human_key: &str,
    resolved_human_key: &str,
) -> bool {
    project.human_key == raw_human_key
        || project.human_key == resolved_human_key
        || human_keys_equivalent(&project.human_key, raw_human_key)
        || human_keys_equivalent(&project.human_key, resolved_human_key)
}

fn map_sql_error(e: &SqlError) -> DbError {
    let message = e.to_string();
    if crate::error::is_lock_error(&message) {
        DbError::ResourceBusy(message)
    } else {
        DbError::Sqlite(message)
    }
}

fn map_sql_outcome<T>(out: Outcome<T, SqlError>) -> Outcome<T, DbError> {
    match out {
        Outcome::Ok(v) => Outcome::Ok(v),
        Outcome::Err(e) => Outcome::Err(map_sql_error(&e)),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

fn decode_project_row(row: &SqlRow) -> std::result::Result<ProjectRow, DbError> {
    let id = row.get(0).and_then(value_as_i64);
    let slug = row
        .get(1)
        .and_then(|value| match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        })
        .ok_or_else(|| DbError::Internal("missing slug in project row".to_string()))?;
    let human_key = row
        .get(2)
        .and_then(|value| match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        })
        .ok_or_else(|| DbError::Internal("missing human_key in project row".to_string()))?;
    let created_at = row.get(3).and_then(value_as_i64).unwrap_or(0);

    Ok(ProjectRow {
        id,
        slug,
        human_key,
        created_at,
    })
}

fn orphaned_project_placeholder(project_id: i64, created_at: i64) -> ProjectRow {
    let placeholder = format!("[unknown-project-{project_id}]");
    ProjectRow {
        id: Some(project_id),
        slug: placeholder.clone(),
        human_key: placeholder,
        created_at,
    }
}

fn decode_file_reservation_row(row: &SqlRow) -> std::result::Result<FileReservationRow, DbError> {
    FileReservationRow::from_row(row).map_err(|e| map_sql_error(&e))
}

fn decode_agent_link_row(row: &SqlRow) -> std::result::Result<AgentLinkRow, DbError> {
    AgentLinkRow::from_row(row).map_err(|e| map_sql_error(&e))
}

fn value_as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::BigInt(n) => Some(*n != 0),
        Value::Int(n) => Some(*n != 0),
        Value::SmallInt(n) => Some(*n != 0),
        Value::TinyInt(n) => Some(*n != 0),
        Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn row_text(row: &SqlRow, idx: usize, field: &str) -> std::result::Result<String, DbError> {
    row.get(idx)
        .and_then(|value| match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        })
        .ok_or_else(|| DbError::Internal(format!("missing {field} in atc_experience row")))
}

fn row_opt_text(row: &SqlRow, idx: usize) -> Option<String> {
    row.get(idx).and_then(|value| match value {
        Value::Text(text) => Some(text.clone()),
        _ => None,
    })
}

fn parse_enum<T>(raw: String, field: &str) -> std::result::Result<T, DbError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::String(raw.clone()))
        .map_err(|error| DbError::Internal(format!("invalid {field} '{raw}': {error}")))
}

fn parse_json_required<T>(raw: String, field: &str) -> std::result::Result<T, DbError>
where
    T: DeserializeOwned,
{
    serde_json::from_str(&raw)
        .map_err(|error| DbError::Internal(format!("invalid {field} JSON: {error}")))
}

fn parse_json_optional<T>(
    raw: Option<String>,
    field: &str,
) -> std::result::Result<Option<T>, DbError>
where
    T: DeserializeOwned,
{
    raw.map(|text| parse_json_required(text, field)).transpose()
}

fn encode_json<T: serde::Serialize>(
    value: &T,
    field: &str,
) -> std::result::Result<String, DbError> {
    serde_json::to_string(value)
        .map_err(|error| DbError::Internal(format!("failed to serialize {field}: {error}")))
}

fn encode_json_optional<T: serde::Serialize>(
    value: &Option<T>,
    field: &str,
) -> std::result::Result<Option<String>, DbError> {
    value
        .as_ref()
        .map(|entry| encode_json(entry, field))
        .transpose()
}

fn merge_json_object(
    existing: &mut serde_json::Map<String, serde_json::Value>,
    patch: &serde_json::Map<String, serde_json::Value>,
) {
    for (key, patch_value) in patch {
        match (existing.get_mut(key), patch_value) {
            (
                Some(serde_json::Value::Object(existing_map)),
                serde_json::Value::Object(patch_map),
            ) => {
                merge_json_object(existing_map, patch_map);
            }
            _ => {
                existing.insert(key.clone(), patch_value.clone());
            }
        }
    }
}

fn merge_context_patch(
    existing: Option<serde_json::Value>,
    patch: Option<&serde_json::Value>,
) -> std::result::Result<Option<serde_json::Value>, DbError> {
    let Some(patch) = patch else {
        return Ok(existing);
    };
    let serde_json::Value::Object(patch_map) = patch else {
        return Err(DbError::invalid(
            "context_patch",
            "execution context patch must be a JSON object",
        ));
    };

    let mut merged = match existing {
        Some(serde_json::Value::Object(map)) => map,
        Some(other) => {
            let mut map = serde_json::Map::new();
            map.insert("base_context".to_string(), other);
            map
        }
        None => serde_json::Map::new(),
    };
    merge_json_object(&mut merged, patch_map);
    Ok(Some(serde_json::Value::Object(merged)))
}

pub(crate) fn decode_atc_experience_row(
    row: &SqlRow,
) -> std::result::Result<ExperienceRow, DbError> {
    let experience_id = row
        .get(0)
        .and_then(value_as_i64)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| {
            DbError::Internal("missing experience_id in atc_experience row".to_string())
        })?;
    let decision_id = row
        .get(1)
        .and_then(value_as_i64)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| {
            DbError::Internal("missing decision_id in atc_experience row".to_string())
        })?;
    let effect_id = row
        .get(2)
        .and_then(value_as_i64)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| DbError::Internal("missing effect_id in atc_experience row".to_string()))?;
    let trace_id = row_text(row, 3, "trace_id")?;
    let claim_id = row_text(row, 4, "claim_id")?;
    let evidence_id = row_text(row, 5, "evidence_id")?;
    let state = parse_enum(row_text(row, 6, "state")?, "state")?;
    let subsystem = parse_enum(row_text(row, 7, "subsystem")?, "subsystem")?;
    let decision_class = row_text(row, 8, "decision_class")?;
    let subject = row_text(row, 9, "subject")?;
    let project_key = row_opt_text(row, 10);
    let policy_id = row_opt_text(row, 11);
    let effect_kind = parse_enum(row_text(row, 12, "effect_kind")?, "effect_kind")?;
    let action = row_text(row, 13, "action")?;
    let posterior = parse_json_required(row_text(row, 14, "posterior_json")?, "posterior_json")?;
    let expected_loss = row
        .get(15)
        .and_then(|value| match value {
            Value::Float(number) => Some(f64::from(*number)),
            Value::Double(number) => Some(*number),
            Value::BigInt(number) => Some(*number as f64),
            Value::Int(number) => Some(f64::from(*number)),
            _ => None,
        })
        .unwrap_or(0.0);
    let runner_up_action = row_opt_text(row, 16);
    let runner_up_loss = row.get(17).and_then(|value| match value {
        Value::Float(number) => Some(f64::from(*number)),
        Value::Double(number) => Some(*number),
        Value::BigInt(number) => Some(*number as f64),
        Value::Int(number) => Some(f64::from(*number)),
        _ => None,
    });
    let evidence_summary = row_text(row, 18, "evidence_summary")?;
    let calibration_healthy = row.get(19).and_then(value_as_bool).unwrap_or(true);
    let safe_mode_active = row.get(20).and_then(value_as_bool).unwrap_or(false);
    let non_execution_reason =
        parse_json_optional::<NonExecutionReason>(row_opt_text(row, 21), "non_execution_json")?;
    let outcome = parse_json_optional::<ExperienceOutcome>(row_opt_text(row, 22), "outcome_json")?;
    let features = parse_json_optional::<FeatureVector>(row_opt_text(row, 23), "features_json")?;
    let feature_ext =
        parse_json_optional::<FeatureExtension>(row_opt_text(row, 24), "feature_ext_json")?;
    let feature_schema_version = row
        .get(25)
        .and_then(value_as_i64)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(FEATURE_SCHEMA_VERSION);
    let feature_payload = migrate_feature_payload(feature_schema_version, features, feature_ext)
        .map_err(|error| DbError::invalid("feature_schema_version", error.to_string()))?;
    let created_ts_micros = row
        .get(26)
        .and_then(value_as_i64)
        .ok_or_else(|| DbError::Internal("missing created_ts in atc_experience row".to_string()))?;
    let dispatched_ts_micros = row.get(27).and_then(value_as_i64);
    let executed_ts_micros = row.get(28).and_then(value_as_i64);
    let resolved_ts_micros = row.get(29).and_then(value_as_i64);
    let context = parse_json_optional::<serde_json::Value>(row_opt_text(row, 30), "context_json")?;

    Ok(ExperienceRow {
        experience_id,
        decision_id,
        effect_id,
        trace_id,
        claim_id,
        evidence_id,
        state,
        subsystem,
        decision_class,
        subject,
        project_key,
        policy_id,
        effect_kind,
        action,
        posterior,
        expected_loss,
        runner_up_action,
        runner_up_loss,
        evidence_summary,
        calibration_healthy,
        safe_mode_active,
        non_execution_reason,
        outcome,
        created_ts_micros,
        dispatched_ts_micros,
        executed_ts_micros,
        resolved_ts_micros,
        features: feature_payload.features,
        feature_ext: feature_payload.feature_ext,
        context,
    })
}

const PROJECT_SELECT_ALL_SQL: &str =
    "SELECT id, slug, human_key, created_at FROM projects ORDER BY id ASC";
pub type AtcRollupRow = (String, i64, i64, i64, i64, i64, i64, f64, f64, f64, f64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AtcRollupSnapshotRow {
    stratum_key: String,
    subsystem: String,
    effect_kind: String,
    risk_tier: i64,
    total_count: i64,
    resolved_count: i64,
    censored_count: i64,
    expired_count: i64,
    correct_count: i64,
    incorrect_count: i64,
    total_regret: f64,
    total_loss: f64,
    ewma_loss: f64,
    ewma_weight: f64,
    delay_sum_micros: i64,
    delay_count: i64,
    delay_max_micros: i64,
    last_updated_ts: i64,
    compacted_total_count: i64,
    compacted_resolved_count: i64,
    compacted_censored_count: i64,
    compacted_expired_count: i64,
    compacted_correct_count: i64,
    compacted_incorrect_count: i64,
    compacted_total_regret: f64,
    compacted_total_loss: f64,
    compacted_ewma_loss: f64,
    compacted_ewma_weight: f64,
    compacted_delay_sum_micros: i64,
    compacted_delay_count: i64,
    compacted_delay_max_micros: i64,
    compacted_last_updated_ts: i64,
}

const ATC_ROLLUP_SNAPSHOT_SELECT_SQL: &str = "\
    SELECT stratum_key, subsystem, effect_kind, risk_tier, \
           total_count, resolved_count, censored_count, expired_count, \
           correct_count, incorrect_count, total_regret, total_loss, \
           ewma_loss, ewma_weight, delay_sum_micros, delay_count, \
           delay_max_micros, last_updated_ts, \
           compacted_total_count, compacted_resolved_count, \
           compacted_censored_count, compacted_expired_count, \
           compacted_correct_count, compacted_incorrect_count, \
           compacted_total_regret, compacted_total_loss, \
           compacted_ewma_loss, compacted_ewma_weight, \
           compacted_delay_sum_micros, compacted_delay_count, \
           compacted_delay_max_micros, compacted_last_updated_ts \
    FROM atc_experience_rollups ORDER BY stratum_key";

pub(crate) const ATC_EXPERIENCE_SELECT_COLUMNS_SQL: &str = "SELECT experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem, decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json, expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy, safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json, feature_schema_version, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json FROM atc_experiences";
const FILE_RESERVATION_SELECT_COLUMNS_SQL: &str = "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts \
     FROM file_reservations";
const AGENT_LINK_SELECT_COLUMNS_SQL: &str = "SELECT id, a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts \
     FROM agent_links";

/// `SQLite` predicate for active reservations across legacy sentinel values.
pub const ACTIVE_RESERVATION_LEGACY_PREDICATE: &str = "released_ts IS NULL \
    OR (typeof(released_ts) IN ('integer', 'real') AND released_ts <= 0) \
    OR (typeof(released_ts) = 'text' AND lower(trim(released_ts)) IN ('', '0', 'null', 'none')) \
    OR (typeof(released_ts) = 'text' \
      AND length(trim(released_ts)) > 0 \
      AND trim(released_ts) GLOB '*[0-9]*' \
      AND REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(\
            trim(released_ts),\
            '0',''),'1',''),'2',''),'3',''),'4',''),'5',''),'6',''),'7',''),'8',''),'9',''),'.',''),'+',''),'-','') = '' \
      AND CAST(trim(released_ts) AS REAL) <= 0)";

/// Active-reservation predicate with sidecar release ledger exclusion.
pub const ACTIVE_RESERVATION_PREDICATE: &str = "(
    (file_reservations.released_ts IS NULL \
      OR (typeof(file_reservations.released_ts) IN ('integer', 'real') AND file_reservations.released_ts <= 0) \
      OR (typeof(file_reservations.released_ts) = 'text' AND lower(trim(file_reservations.released_ts)) IN ('', '0', 'null', 'none')) \
      OR (typeof(file_reservations.released_ts) = 'text' \
        AND length(trim(file_reservations.released_ts)) > 0 \
        AND trim(file_reservations.released_ts) GLOB '*[0-9]*' \
        AND REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(\
              trim(file_reservations.released_ts),\
              '0',''),'1',''),'2',''),'3',''),'4',''),'5',''),'6',''),'7',''),'8',''),'9',''),'.',''),'+',''),'-','') = '' \
        AND CAST(trim(file_reservations.released_ts) AS REAL) <= 0)
    ) \
    AND file_reservations.id NOT IN (
        SELECT reservation_id FROM file_reservation_releases
    )
)";

/// Return [`ACTIVE_RESERVATION_PREDICATE`] adjusted for a table reference.
///
/// This exists for queries that alias `file_reservations` (for example `fr`,
/// `fr1`, or `fr2`) but still need the canonical active-reservation semantics,
/// including the sidecar release ledger exclusion.
#[must_use]
pub fn active_reservation_predicate_for(table_ref: &str) -> String {
    let table_ref = table_ref.trim().trim_end_matches('.');
    if table_ref.is_empty() || table_ref == "file_reservations" {
        return ACTIVE_RESERVATION_PREDICATE.to_string();
    }
    ACTIVE_RESERVATION_PREDICATE.replace("file_reservations.", &format!("{table_ref}."))
}

/// The active-reservation predicate WITHOUT the sidecar release-ledger
/// exclusion, adjusted for a table reference.
///
/// GH#180: [`ACTIVE_RESERVATION_PREDICATE`] ends in an uncorrelated
/// `id NOT IN (SELECT reservation_id FROM file_reservation_releases)` anti-join.
/// Under sqlmodel-frankensqlite's join execution that subquery is re-scanned per
/// join row rather than materialized once, so an active-view query with a
/// `LEFT JOIN agents` degrades to O(N·M) — ~25s on a 30k-row store (canonical
/// SQLite materializes it and runs the same query in ~0.01s). The no-join paths
/// are fine, so this cheap `released_ts`-only predicate is used by the join
/// sites to fetch *candidate* rows; callers then subtract the released
/// reservation IDs (see [`RELEASED_RESERVATION_IDS_SQL`]) in Rust for identical
/// results without the O(N·M) blowup.
#[must_use]
pub fn active_reservation_candidate_predicate_for(table_ref: &str) -> String {
    let table_ref = table_ref.trim().trim_end_matches('.');
    let qualifier = if table_ref.is_empty() || table_ref == "file_reservations" {
        "file_reservations.".to_string()
    } else {
        format!("{table_ref}.")
    };
    // ACTIVE_RESERVATION_LEGACY_PREDICATE references the bare `released_ts`
    // column; qualify every occurrence with the requested table reference.
    format!(
        "({})",
        ACTIVE_RESERVATION_LEGACY_PREDICATE
            .replace("released_ts", &format!("{qualifier}released_ts"))
    )
}

/// SQL that returns the reservation IDs recorded in the sidecar release ledger.
///
/// GH#180: fetch this set separately (a single-column scan, cheap in any engine)
/// and subtract it from the candidate rows in Rust — this reproduces canonical
/// SQLite's materialized `NOT IN` semantics without the per-join-row rescan.
/// Reservation IDs are globally unique primary keys, so the unscoped ledger set
/// is safe to subtract from project-scoped candidates.
pub const RELEASED_RESERVATION_IDS_SQL: &str =
    "SELECT reservation_id FROM file_reservation_releases";

/// Decode `ProductRow` from raw SQL query result using positional (indexed) column access.
/// Expected column order: `id`, `product_uid`, `name`, `created_at`.
fn decode_product_row_indexed(row: &SqlRow) -> std::result::Result<ProductRow, DbError> {
    let id = row.get(0).and_then(value_as_i64);
    let product_uid = row
        .get(1)
        .and_then(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| DbError::Internal("missing product_uid in product row".to_string()))?;
    let name = row
        .get(2)
        .and_then(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| DbError::Internal("missing name in product row".to_string()))?;
    let created_at = row.get(3).and_then(value_as_i64).unwrap_or(0);

    Ok(ProductRow {
        id,
        product_uid,
        name,
        created_at,
    })
}

/// Decode `AgentRow` from raw SQL query result using positional (indexed) column access.
/// Expected column order: `id`, `project_id`, `name`, `program`, `model`, `task_description`,
/// `inception_ts`, `last_active_ts`, `attachments_policy`, `contact_policy`, `reaper_exempt`,
/// `registration_token`.
fn decode_agent_row_indexed(row: &SqlRow) -> AgentRow {
    fn get_i64(row: &SqlRow, idx: usize) -> i64 {
        row.get(idx).and_then(value_as_i64).unwrap_or(0)
    }
    fn get_string(row: &SqlRow, idx: usize) -> String {
        row.get(idx)
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
    fn get_opt_i64(row: &SqlRow, idx: usize) -> Option<i64> {
        row.get(idx).and_then(value_as_i64)
    }
    fn get_opt_string(row: &SqlRow, idx: usize) -> Option<String> {
        row.get(idx).and_then(|v| match v {
            Value::Text(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
    }

    AgentRow {
        id: get_opt_i64(row, 0),
        project_id: get_i64(row, 1),
        name: get_string(row, 2),
        program: get_string(row, 3),
        model: get_string(row, 4),
        task_description: get_string(row, 5),
        inception_ts: get_i64(row, 6),
        last_active_ts: get_i64(row, 7),
        attachments_policy: {
            let s = get_string(row, 8);
            if s.is_empty() { "auto".to_string() } else { s }
        },
        contact_policy: {
            let s = get_string(row, 9);
            if s.is_empty() { "auto".to_string() } else { s }
        },
        reaper_exempt: get_opt_i64(row, 10).unwrap_or(0),
        registration_token: get_opt_string(row, 11),
    }
}

#[allow(clippy::cast_possible_truncation)]
pub(crate) fn value_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::BigInt(n) => Some(*n),
        Value::Int(n) => Some(i64::from(*n)),
        Value::SmallInt(n) => Some(i64::from(*n)),
        Value::TinyInt(n) => Some(i64::from(*n)),
        Value::Float(f) if f.is_finite() => Some(*f as i64),
        Value::Double(d) if d.is_finite() => Some(*d as i64),
        Value::Text(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Double(n) => Some(*n),
        Value::Float(n) => Some(f64::from(*n)),
        Value::BigInt(n) => Some(*n as f64),
        Value::Int(n) => Some(f64::from(*n)),
        Value::SmallInt(n) => Some(f64::from(*n)),
        Value::TinyInt(n) => Some(f64::from(*n)),
        Value::Text(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

pub(crate) fn row_first_i64(row: &SqlRow) -> Option<i64> {
    row.get(0).and_then(value_as_i64)
}

/// `SQLite` default `SQLITE_MAX_VARIABLE_NUMBER` is 999 (32766 in newer builds).
/// We cap IN-clause item counts well below that to prevent excessively large
/// SQL strings and parameter arrays from untrusted input.
const SQLITE_MAX_BIND_PARAMS: usize = 999;
const MAX_IN_CLAUSE_ITEMS: usize = 500;
// FrankenSQLite currently degrades and can surface malformed-page errors under
// very large IN-clause updates on file_reservations. Keep release-path chunks
// conservative until the engine-side planner/executor bug is fixed.
const MAX_RELEASE_RESERVATION_CHUNK_ITEMS: usize = 128;
// release_reservations executes both:
// - SELECT ... WHERE project_id, agent_id, filters...
// - UPDATE ... SET released_ts = ? WHERE project_id, agent_id, filters...
// The UPDATE has one extra bind (released_ts), so total binds are:
// 3 + reservation_ids.len() + paths.len()
const RELEASE_RESERVATION_BASE_BIND_PARAMS: usize = 3;
const MAX_RELEASE_RESERVATION_FILTER_ITEMS: usize =
    SQLITE_MAX_BIND_PARAMS - RELEASE_RESERVATION_BASE_BIND_PARAMS;

static PLACEHOLDER_CACHE: OnceLock<Vec<String>> = OnceLock::new();
static APPROVED_CONTACT_SQL_CACHE: OnceLock<Vec<String>> = OnceLock::new();
static RECENT_CONTACT_SQL_CACHE: OnceLock<Vec<String>> = OnceLock::new();

fn build_placeholders(capped: usize) -> String {
    std::iter::repeat_n("?", capped)
        .collect::<Vec<_>>()
        .join(", ")
}

fn placeholders(count: usize) -> String {
    let capped = count.min(MAX_IN_CLAUSE_ITEMS);
    if capped == 0 {
        return String::new();
    }

    let cache = PLACEHOLDER_CACHE.get_or_init(|| {
        (1..=MAX_IN_CLAUSE_ITEMS)
            .map(build_placeholders)
            .collect::<Vec<_>>()
    });
    cache[capped - 1].clone()
}

fn build_approved_contact_sql_with_placeholders(placeholders: &str) -> String {
    format!(
        "SELECT b_agent_id FROM agent_links \
         WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? \
           AND status = 'approved' AND (expires_ts IS NULL OR expires_ts > ?) \
           AND b_agent_id IN ({placeholders})"
    )
}

fn approved_contact_sql(item_count: usize) -> &'static str {
    let capped = item_count.min(MAX_IN_CLAUSE_ITEMS);
    let cache = APPROVED_CONTACT_SQL_CACHE.get_or_init(|| {
        (0..=MAX_IN_CLAUSE_ITEMS)
            .map(|count| build_approved_contact_sql_with_placeholders(&placeholders(count)))
            .collect::<Vec<_>>()
    });
    &cache[capped]
}

const REQUIRED_ATC_EXPERIENCE_COLUMNS: &[&str] = &[
    "experience_id",
    "decision_id",
    "effect_id",
    "trace_id",
    "claim_id",
    "evidence_id",
    "state",
    "subsystem",
    "decision_class",
    "subject",
    "project_key",
    "policy_id",
    "effect_kind",
    "action",
    "posterior_json",
    "expected_loss",
    "runner_up_action",
    "runner_up_loss",
    "evidence_summary",
    "calibration_healthy",
    "safe_mode_active",
    "non_execution_json",
    "outcome_json",
    "features_json",
    "feature_ext_json",
    "feature_schema_version",
    "created_ts",
    "dispatched_ts",
    "executed_ts",
    "resolved_ts",
    "context_json",
    "contained_suspected_secret",
    "privacy_classification",
];

const REQUIRED_ATC_ROLLUP_COLUMNS: &[&str] = &[
    "stratum_key",
    "subsystem",
    "effect_kind",
    "risk_tier",
    "total_count",
    "resolved_count",
    "censored_count",
    "expired_count",
    "correct_count",
    "incorrect_count",
    "total_regret",
    "total_loss",
    "ewma_loss",
    "ewma_weight",
    "delay_sum_micros",
    "delay_count",
    "delay_max_micros",
    "last_updated_ts",
    "compacted_total_count",
    "compacted_resolved_count",
    "compacted_censored_count",
    "compacted_expired_count",
    "compacted_correct_count",
    "compacted_incorrect_count",
    "compacted_total_regret",
    "compacted_total_loss",
    "compacted_ewma_loss",
    "compacted_ewma_weight",
    "compacted_delay_sum_micros",
    "compacted_delay_count",
    "compacted_delay_max_micros",
    "compacted_last_updated_ts",
];

const REQUIRED_ATC_AUXILIARY_TABLES: &[(&str, &[&str])] = &[
    (
        "atc_leader_lease",
        &[
            "lease_slot",
            "instance_id",
            "acquired_at",
            "renewed_at",
            "ttl_micros",
        ],
    ),
    (
        "atc_rollup_snapshots",
        &[
            "snapshot_id",
            "captured_ts",
            "archive_relpath",
            "rollup_rows",
            "payload_sha256",
            "restored_ts",
        ],
    ),
];

fn build_recent_contact_union_sql_with_placeholders(placeholders: &str) -> String {
    format!(
        "SELECT agent_id FROM ( \
           SELECT r.agent_id AS agent_id \
           FROM message_recipients r \
           JOIN messages m ON m.id = r.message_id \
           WHERE m.project_id = ? AND m.sender_id = ? AND m.created_ts > ? \
             AND r.agent_id IN ({placeholders}) \
           UNION \
           SELECT m.sender_id AS agent_id \
           FROM messages m \
           JOIN message_recipients r ON r.message_id = m.id \
           WHERE m.project_id = ? AND r.agent_id = ? AND m.created_ts > ? \
             AND m.sender_id IN ({placeholders}) \
        ) ORDER BY agent_id"
    )
}

fn recent_contact_union_sql(item_count: usize) -> &'static str {
    let capped = item_count.min(MAX_IN_CLAUSE_ITEMS);
    let cache = RECENT_CONTACT_SQL_CACHE.get_or_init(|| {
        (0..=MAX_IN_CLAUSE_ITEMS)
            .map(|count| build_recent_contact_union_sql_with_placeholders(&placeholders(count)))
            .collect::<Vec<_>>()
    });
    &cache[capped]
}

async fn acquire_conn(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<sqlmodel_pool::PooledConnection<crate::DbConn>, DbError> {
    map_sql_outcome(pool.acquire(cx).await)
}

fn canonical_table_columns(
    conn: &crate::CanonicalDbConn,
    table_name: &'static str,
    purpose: &'static str,
) -> std::result::Result<HashSet<String>, DbError> {
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table_name})"), &[])
        .map_err(|error| DbError::Sqlite(format!("{purpose}: {error}")))?;
    let mut columns = HashSet::with_capacity(rows.len());
    for row in &rows {
        if let Ok(name) = row.get_named::<String>("name") {
            columns.insert(name);
        }
    }
    Ok(columns)
}

fn inspect_canonical_atc_schema(pool: &DbPool) -> std::result::Result<Vec<String>, DbError> {
    let conn = open_canonical_atc_conn(pool, "inspect canonical ATC schema")?;
    let result = (|| {
        let mut missing = Vec::new();

        let experience_columns = canonical_table_columns(
            &conn,
            "atc_experiences",
            "inspect canonical ATC experience columns",
        )?;
        if experience_columns.is_empty() {
            missing.push("table atc_experiences".to_string());
        } else {
            for column in REQUIRED_ATC_EXPERIENCE_COLUMNS {
                if !experience_columns.contains(*column) {
                    missing.push(format!("atc_experiences.{column}"));
                }
            }
        }

        let rollup_columns = canonical_table_columns(
            &conn,
            "atc_experience_rollups",
            "inspect canonical ATC rollup columns",
        )?;
        if rollup_columns.is_empty() {
            missing.push("table atc_experience_rollups".to_string());
        } else {
            for column in REQUIRED_ATC_ROLLUP_COLUMNS {
                if !rollup_columns.contains(*column) {
                    missing.push(format!("atc_experience_rollups.{column}"));
                }
            }
        }

        for (table_name, required_columns) in REQUIRED_ATC_AUXILIARY_TABLES {
            let columns = canonical_table_columns(
                &conn,
                table_name,
                "inspect canonical ATC auxiliary columns",
            )?;
            if columns.is_empty() {
                missing.push(format!("table {table_name}"));
                continue;
            }
            for column in *required_columns {
                if !columns.contains(*column) {
                    missing.push(format!("{table_name}.{column}"));
                }
            }
        }

        Ok(missing)
    })();
    close_canonical_db_conn(conn, "canonical ATC schema inspection connection");
    result
}

/// Summary returned by ATC feature-schema reprocessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AtcFeatureSchemaReprocessSummary {
    /// Rows scanned after filters were applied.
    pub scanned: usize,
    /// Rows whose payload needed rewriting.
    pub updated: usize,
    /// Rows already matching the current schema.
    pub unchanged: usize,
    /// Current supported feature schema version.
    pub current_schema_version: u16,
    /// Whether the command ran in preview mode.
    pub dry_run: bool,
}

/// Reprocess persisted ATC feature payloads to the current schema contract.
// ATC experiences live in the dedicated sidecar DB (br-bvq1x.11.7); the
// reprocess operates on a canonical connection to that sidecar.
pub fn reprocess_atc_feature_schema(
    conn: &crate::CanonicalDbConn,
    project_key: Option<&str>,
    subject: Option<&str>,
    limit: usize,
    dry_run: bool,
) -> std::result::Result<AtcFeatureSchemaReprocessSummary, DbError> {
    if limit == 0 {
        return Err(DbError::invalid("limit", "must be greater than zero"));
    }

    let limit = i64::try_from(limit)
        .map_err(|_| DbError::invalid("limit", "exceeds SQLite INTEGER range"))?;
    let mut sql = String::from(
        "SELECT experience_id, feature_schema_version, features_json, feature_ext_json \
         FROM atc_experiences",
    );
    let mut predicates = Vec::new();
    let mut params = Vec::new();
    if let Some(value) = project_key {
        predicates.push("project_key = ?");
        params.push(Value::Text(value.to_string()));
    }
    if let Some(value) = subject {
        predicates.push("subject = ?");
        params.push(Value::Text(value.to_string()));
    }
    if !predicates.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&predicates.join(" AND "));
    }
    sql.push_str(" ORDER BY experience_id ASC LIMIT ?");
    params.push(Value::BigInt(limit));

    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|error| DbError::Sqlite(format!("reprocess ATC feature schema query: {error}")))?;

    if !dry_run {
        conn.execute_raw("BEGIN IMMEDIATE TRANSACTION")
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "begin ATC feature schema reprocess transaction: {error}"
                ))
            })?;
    }

    let result = (|| -> std::result::Result<AtcFeatureSchemaReprocessSummary, DbError> {
        let mut updated = 0_usize;
        for row in &rows {
            let experience_id = row.get(0).and_then(value_as_i64).ok_or_else(|| {
                DbError::Internal(
                    "missing experience_id in ATC feature schema reprocess row".to_string(),
                )
            })?;
            let stored_schema_version = row
                .get(1)
                .and_then(value_as_i64)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(FEATURE_SCHEMA_VERSION);
            let features =
                parse_json_optional::<FeatureVector>(row_opt_text(row, 2), "features_json")?;
            let feature_ext =
                parse_json_optional::<FeatureExtension>(row_opt_text(row, 3), "feature_ext_json")?;
            let migrated =
                migrate_feature_payload(stored_schema_version, features, feature_ext.clone())
                    .map_err(|error| {
                        DbError::invalid("feature_schema_version", error.to_string())
                    })?;
            let needs_update = stored_schema_version != migrated.schema_version
                || features != migrated.features
                || feature_ext != migrated.feature_ext;
            if !needs_update {
                continue;
            }

            updated += 1;
            if dry_run {
                continue;
            }

            let features_json = encode_json_optional(&migrated.features, "features_json")?;
            let feature_ext_json = encode_json_optional(&migrated.feature_ext, "feature_ext_json")?;
            conn.execute_sync(
                "UPDATE atc_experiences \
                 SET feature_schema_version = ?, features_json = ?, feature_ext_json = ? \
                 WHERE experience_id = ?",
                &[
                    Value::BigInt(i64::from(migrated.schema_version)),
                    features_json.map_or(Value::Null, Value::Text),
                    feature_ext_json.map_or(Value::Null, Value::Text),
                    Value::BigInt(experience_id),
                ],
            )
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "update ATC feature payload for experience_id={experience_id}: {error}"
                ))
            })?;
        }

        Ok(AtcFeatureSchemaReprocessSummary {
            scanned: rows.len(),
            updated,
            unchanged: rows.len().saturating_sub(updated),
            current_schema_version: FEATURE_SCHEMA_VERSION,
            dry_run,
        })
    })();

    match (&result, dry_run) {
        (Ok(_), false) => {
            conn.execute_raw("COMMIT").map_err(|error| {
                DbError::Sqlite(format!(
                    "commit ATC feature schema reprocess transaction: {error}"
                ))
            })?;
        }
        (Err(_), false) => {
            let _ = conn.execute_raw("ROLLBACK");
        }
        _ => {}
    }

    result
}

pub(crate) async fn ensure_file_backed_atc_pool_initialized(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<(), DbError> {
    if pool.sqlite_path() == ":memory:" {
        return Outcome::Ok(());
    }
    let missing_before = match inspect_canonical_atc_schema(pool) {
        Ok(missing) => {
            if missing.is_empty() {
                return Outcome::Ok(());
            }
            missing
        }
        Err(error) => return Outcome::Err(error),
    };

    let conn = match open_canonical_atc_conn(pool, "initialize canonical ATC schema") {
        Ok(conn) => conn,
        Err(error) => return Outcome::Err(error),
    };
    // On first init the sidecar is a brand-new on-disk file; place it in WAL
    // mode to match the mail DB's durability/concurrency posture. journal_mode
    // is DB-wide and is intentionally omitted from PRAGMA_CONN_SETTINGS_SQL, so
    // it must be applied here once during sidecar creation.
    if let Err(error) = conn.execute_raw(crate::schema::PRAGMA_DB_INIT_SQL) {
        close_canonical_db_conn(conn, "initialize canonical ATC schema connection");
        return Outcome::Err(DbError::Sqlite(format!(
            "initialize canonical ATC schema: set sidecar db pragmas failed: {error}"
        )));
    }
    // Keep the sidecar private (0600): like storage.sqlite3 it carries project
    // keys, subjects, and evidence summaries (br-bvq1x.11.7). Best-effort — a
    // chmod failure must not block ATC telemetry.
    #[cfg(unix)]
    if let Some(atc_path) = pool.atc_sqlite_path() {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            std::fs::set_permissions(&atc_path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!(
                path = %atc_path,
                error = %error,
                "failed to restrict ATC sidecar permissions to 0600"
            );
        }
    }
    let migration_result = crate::schema::migrate_atc_runtime_canonical_followup(cx, &conn).await;
    close_canonical_db_conn(conn, "initialize canonical ATC schema connection");

    match migration_result {
        Outcome::Ok(_) => match inspect_canonical_atc_schema(pool) {
            Ok(missing_after) if missing_after.is_empty() => Outcome::Ok(()),
            Ok(missing_after) => Outcome::Err(DbError::Internal(format!(
                "ATC schema initialization did not converge; missing before init: {}; still missing after init: {}",
                missing_before.join(", "),
                missing_after.join(", "),
            ))),
            Err(error) => Outcome::Err(error),
        },
        Outcome::Err(error) => Outcome::Err(DbError::Sqlite(format!(
            "initialize canonical ATC schema: {error}"
        ))),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

fn tracked(conn: &crate::DbConn) -> TrackedConnection<'_> {
    TrackedConnection::new(conn)
}

// =============================================================================
// Transaction helpers
// =============================================================================

/// Whether `BEGIN CONCURRENT` is enabled (MVCC page-level writes).
///
/// Read once from `FSQLITE_CONCURRENT_MODE` env var; defaults to `false`.
/// When `false`, all transactions use `BEGIN IMMEDIATE` (single-writer).
/// Set `FSQLITE_CONCURRENT_MODE=true` to opt in to `BEGIN CONCURRENT`.
///
/// **Warning:** Concurrent mode is known to cause snapshot drift
/// (`fcw_base_drift`) under sustained write load, where `snapshot_high`
/// lags behind `commit_seq` and all writes eventually fail.  See
/// <https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/65>.
/// The underlying frankensqlite MVCC snapshot-advance bug has not yet
/// been fixed.  Single-writer mode (`BEGIN IMMEDIATE`) is recommended
/// for all deployments.
static CONCURRENT_MODE_ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    let enabled = std::env::var("FSQLITE_CONCURRENT_MODE")
        .ok()
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    if enabled {
        tracing::warn!(
            "FSQLITE_CONCURRENT_MODE=true: BEGIN CONCURRENT is enabled. \
             This mode has a known snapshot-drift bug (GH#65) that causes \
             all writes to fail after ~10-20 operations. Single-writer mode \
             (FSQLITE_CONCURRENT_MODE=false) is strongly recommended."
        );
    }
    enabled
});

fn should_fallback_begin_concurrent(err_msg: &str) -> bool {
    let lower = err_msg.to_ascii_lowercase();
    lower.contains("near \"concurrent\"") || crate::pool::is_sqlite_recovery_error_message(&lower)
}

/// Begin a concurrent write transaction (MVCC page-level concurrent writes).
///
/// Falls back to `BEGIN IMMEDIATE` on backends that do not support
/// `BEGIN CONCURRENT`, or when `FSQLITE_CONCURRENT_MODE=false`.
async fn begin_concurrent_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    if !*CONCURRENT_MODE_ENABLED {
        return begin_immediate_tx(cx, tracked).await;
    }
    match map_sql_outcome(tracked.execute(cx, "BEGIN CONCURRENT", &[]).await).map(|_| ()) {
        Outcome::Err(DbError::Sqlite(msg)) if should_fallback_begin_concurrent(&msg) => {
            begin_immediate_tx(cx, tracked).await
        }
        out => out,
    }
}

/// Commit the current transaction and publish file-backed writes to fresh handles.
async fn commit_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    match map_sql_outcome(tracked.execute(cx, "COMMIT", &[]).await) {
        Outcome::Ok(_) => {
            if tracked.inner.path() != ":memory:" {
                // FrankenSQLite can otherwise keep a successful COMMIT private
                // to the pooled connection until a later close. The checkpoint
                // gives post-commit probes and sibling processes the same view
                // immediately after the write path returns.
                if let Err(error) = tracked
                    .inner
                    .query_sync("PRAGMA wal_checkpoint(PASSIVE)", &[])
                {
                    tracing::warn!(
                        db_path = %tracked.inner.path(),
                        error = %error,
                        "post_commit_checkpoint_failed_after_successful_commit"
                    );
                }
            }
            Outcome::Ok(())
        }
        Outcome::Err(error) => Outcome::Err(error),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

/// Rebuild indexes via `REINDEX`.
///
/// Only needed for explicit repair/recovery paths (e.g. `am doctor repair`).
/// Regular writes do not need this — `SQLite` maintains indexes automatically.
/// Calling `REINDEX` after every write is expensive and can trigger UNIQUE
/// constraint failures from unrelated tables if data inconsistencies exist.
#[allow(dead_code)]
async fn rebuild_indexes(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    map_sql_outcome(traw_execute(cx, tracked, "REINDEX", &[]).await).map(|_| ())
}

/// Begin an immediate write transaction (single-writer semantics).
///
/// Used for write paths that are sensitive to `BEGIN CONCURRENT` backend quirks.
async fn begin_immediate_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    map_sql_outcome(tracked.execute(cx, "BEGIN IMMEDIATE", &[]).await).map(|_| ())
}

/// Rollback the current transaction (best-effort, errors ignored).
async fn rollback_tx(cx: &Cx, tracked: &TrackedConnection<'_>) {
    let _ = tracked.execute(cx, "ROLLBACK", &[]).await;
}

/// Unwrap an `Outcome` inside a transaction: on non-`Ok`, rollback and return early.
///
/// Usage: `let val = try_in_tx!(cx, tracked, some_outcome_expr);`
macro_rules! try_in_tx {
    ($cx:expr, $tracked:expr, $out:expr) => {
        match $out {
            Outcome::Ok(v) => v,
            Outcome::Err(e) => {
                rollback_tx($cx, $tracked).await;
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                rollback_tx($cx, $tracked).await;
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                rollback_tx($cx, $tracked).await;
                return Outcome::Panicked(p);
            }
        }
    };
}

/// Maximum retry attempts for durability probe transient busy errors.
///
/// Durability probes are read-only SELECTs on a fresh connection; they should
/// not be blocked for long.  3 retries with 25–200 ms backoff (~0.4 s total)
/// are enough to outlast transient WAL checkpoint stalls without hiding real
/// failures.
const DURABILITY_PROBE_MAX_RETRIES: u32 = 3;

/// Execute a durability probe query from a fresh connection when file-backed.
///
/// This avoids false positives where the writer connection can still observe
/// transient state that is not yet durable/visible from independent handles.
///
/// Transient `SQLITE_BUSY` / `database is locked` errors are retried up to
/// [`DURABILITY_PROBE_MAX_RETRIES`] times with exponential backoff so that a
/// concurrent WAL checkpoint does not cause the caller to see a spurious
/// `DATABASE_ERROR busy` for a mutation that already committed successfully.
async fn durability_probe_query(
    cx: &Cx,
    pool: &DbPool,
    sql: &str,
    params: &[Value],
) -> Outcome<Vec<SqlRow>, DbError> {
    if pool.sqlite_path() == ":memory:" {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        let tracked = tracked(&*conn);
        return map_sql_outcome(traw_query(cx, &tracked, sql, params).await);
    }

    for attempt in 0..=DURABILITY_PROBE_MAX_RETRIES {
        // Use a plain open — no recovery, no fallback paths. Durability probes
        // must be side-effect-free so they never trigger REINDEX or open a
        // fallback database, which could make committed rows appear to vanish.
        let probe_conn = match crate::DbConn::open_file(pool.sqlite_path()) {
            Ok(conn) => conn,
            Err(e) => return Outcome::Err(DbError::Sqlite(e.to_string())),
        };
        if let Err(e) = probe_conn.execute_raw(crate::schema::PRAGMA_CONN_SETTINGS_SQL) {
            return Outcome::Err(DbError::Sqlite(format!(
                "durability probe connection init failed: {e}"
            )));
        }
        let probe_tracked = tracked(&probe_conn);
        let out = map_sql_outcome(traw_query(cx, &probe_tracked, sql, params).await);
        crate::close_db_conn(probe_conn, "durability_probe_query connection");

        match &out {
            Outcome::Err(e)
                if is_probe_transient_busy(e) && attempt < DURABILITY_PROBE_MAX_RETRIES =>
            {
                tracing::warn!(
                    attempt,
                    max_retries = DURABILITY_PROBE_MAX_RETRIES,
                    error = %e,
                    "durability probe hit transient busy, retrying"
                );
                durability_probe_backoff(attempt);
            }
            _ => return out,
        }
    }

    // Unreachable: the loop always returns on the final attempt.
    Outcome::Err(DbError::Internal(
        "durability probe retry loop fell through".to_string(),
    ))
}

/// Check if a durability-probe error is a transient busy/locked condition
/// that is safe to retry (the probe is a read-only SELECT).
fn is_probe_transient_busy(e: &DbError) -> bool {
    is_plain_write_contention_error(e) || is_mvcc_error(e)
}

/// Exponential backoff for durability probe retries.
///
/// Base: 25 ms, max: 200 ms (lightweight — probes should unblock quickly).
fn durability_probe_backoff(attempt: u32) {
    use crate::retry::RetryConfig;
    let config = RetryConfig {
        base_delay: std::time::Duration::from_millis(25),
        max_delay: std::time::Duration::from_millis(200),
        use_circuit_breaker: false,
        ..Default::default()
    };
    std::thread::sleep(config.delay_for_attempt(attempt));
}

/// Fetch an agent row directly from `SQLite` after commit to verify durability.
///
/// This bypasses cache and probes from a fresh connection for file-backed DBs
/// so callers only return success when the row is query-visible post-commit.
async fn verify_agent_visible_after_commit(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
) -> Outcome<AgentRow, DbError> {
    // GH#169: resolve the canonical (lowest-id) variant deterministically, the
    // same row get_agent / register_agent reuse resolves.
    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
               registration_token \
               FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
               ORDER BY id ASC LIMIT 1";
    let params = [Value::BigInt(project_id), Value::Text(name.to_string())];
    let fresh_result = match durability_probe_query(cx, pool, sql, &params).await {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || {
                Outcome::Err(DbError::Internal(format!(
                    "agent row not visible after commit for {project_id}:{name}"
                )))
            },
            |row| Outcome::Ok(decode_agent_row_indexed(row)),
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    };
    let Outcome::Err(fresh_error) = &fresh_result else {
        return fresh_result;
    };
    if pool.sqlite_path() == ":memory:" || !is_agent_visibility_probe_consistency_error(fresh_error)
    {
        return fresh_result;
    }
    tracing::warn!(
        project_id,
        agent = %name,
        fresh_error = %fresh_error,
        "fresh durability probe missed committed agent visibility; refusing pooled-only confirmation"
    );
    fresh_result
}

fn is_agent_visibility_probe_consistency_error(error: &DbError) -> bool {
    matches!(error, DbError::Internal(message) if message.contains("agent row not visible after commit"))
}

fn normalize_expected_recipients(recipients: &[(i64, &str)]) -> Vec<(i64, String)> {
    let mut pairs: Vec<(i64, String)> = recipients
        .iter()
        .map(|(agent_id, kind)| (*agent_id, (*kind).to_string()))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageVisibilityProbeMode {
    FreshHandle,
    #[cfg(test)]
    PooledHandle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WriterPostCommitCounts {
    message_count: Option<i64>,
    recipient_count: Option<i64>,
}

async fn message_visibility_probe_query(
    cx: &Cx,
    pool: &DbPool,
    sql: &str,
    params: &[Value],
    mode: MessageVisibilityProbeMode,
) -> Outcome<Vec<SqlRow>, DbError> {
    match mode {
        MessageVisibilityProbeMode::FreshHandle => {
            durability_probe_query(cx, pool, sql, params).await
        }
        #[cfg(test)]
        MessageVisibilityProbeMode::PooledHandle => {
            let conn = match acquire_conn(cx, pool).await {
                Outcome::Ok(c) => c,
                Outcome::Err(e) => return Outcome::Err(e),
                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                Outcome::Panicked(p) => return Outcome::Panicked(p),
            };
            let tracked = tracked(&*conn);
            map_sql_outcome(traw_query(cx, &tracked, sql, params).await)
        }
    }
}

fn decode_message_recipient_pairs(
    message_id: i64,
    recipient_rows: &[SqlRow],
) -> std::result::Result<Vec<(i64, String)>, DbError> {
    let mut actual: Vec<(i64, String)> = Vec::with_capacity(recipient_rows.len());
    for row in recipient_rows {
        let Some(agent_id) = row.get(0).and_then(value_as_i64) else {
            return Err(DbError::Internal(format!(
                "message recipient durability check failed: missing agent_id for message_id={message_id}"
            )));
        };
        let Some(kind) = row.get(1).and_then(|value| match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        }) else {
            return Err(DbError::Internal(format!(
                "message recipient durability check failed: missing kind for message_id={message_id}"
            )));
        };
        actual.push((agent_id, kind));
    }
    actual.sort_unstable();
    actual.dedup();
    Ok(actual)
}

fn is_message_visibility_probe_consistency_error(error: &DbError) -> bool {
    match error {
        DbError::Internal(message) => {
            message.contains("message row not visible after commit")
                || message.contains("message recipient rows not visible after commit")
        }
        _ => false,
    }
}

/// Is this ghost-success verdict specifically about the RECIPIENT rows (rather
/// than the message row itself)?
///
/// GH#179: the writer's own post-commit sample can only refute the probe class
/// it actually contradicts. A present message row refutes a message-row ghost,
/// but says nothing about a recipient-rows ghost — recipient rows missing means
/// the message is delivered to nobody, and reporting success there is the exact
/// silent-loss class the probe exists to catch.
fn is_recipient_visibility_probe_consistency_error(error: &DbError) -> bool {
    matches!(
        error,
        DbError::Internal(message)
            if message.contains("message recipient rows not visible after commit")
    )
}

/// Does the writer's own post-commit sample independently refute this specific
/// ghost-success verdict?
///
/// - Message-row ghost: refuted when the writer handle saw the message row.
/// - Recipient-rows ghost: refuted only when the writer handle saw the message
///   row AND the full expected recipient-row count.
fn writer_sample_refutes_probe_verdict(
    error: &DbError,
    writer_counts: WriterPostCommitCounts,
    expected_recipients: i64,
) -> bool {
    let message_confirmed = writer_counts.message_count == Some(1);
    if is_recipient_visibility_probe_consistency_error(error) {
        message_confirmed && writer_counts.recipient_count == Some(expected_recipients)
    } else {
        message_confirmed
    }
}

fn annotate_message_visibility_error_with_writer_counts(
    error: DbError,
    writer_counts: WriterPostCommitCounts,
) -> DbError {
    if !is_message_visibility_probe_consistency_error(&error) {
        return error;
    }
    match error {
        DbError::Internal(message) => DbError::Internal(format!(
            "{message}; writer_handle_message_count={:?}; writer_handle_recipient_count={:?}",
            writer_counts.message_count, writer_counts.recipient_count
        )),
        other => other,
    }
}

async fn verify_message_recipients_visible_with_probe_mode(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    message_id: i64,
    expected_recipients: &[(i64, &str)],
    probe_mode: MessageVisibilityProbeMode,
) -> Outcome<(), DbError> {
    let message_count_sql = "SELECT COUNT(*) FROM messages WHERE id = ? AND project_id = ?";
    let message_count_params = [Value::BigInt(message_id), Value::BigInt(project_id)];
    let message_count_rows = match message_visibility_probe_query(
        cx,
        pool,
        message_count_sql,
        &message_count_params,
        probe_mode,
    )
    .await
    {
        Outcome::Ok(rows) => rows,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let message_count = message_count_rows
        .first()
        .and_then(row_first_i64)
        .unwrap_or_default();
    if message_count != 1 {
        return Outcome::Err(DbError::Internal(format!(
            "message row not visible after commit for message_id={message_id} project_id={project_id}"
        )));
    }

    let recipient_sql = "SELECT agent_id, kind FROM message_recipients WHERE message_id = ? ORDER BY agent_id, kind";
    let recipient_params = [Value::BigInt(message_id)];
    let recipient_rows = match message_visibility_probe_query(
        cx,
        pool,
        recipient_sql,
        &recipient_params,
        probe_mode,
    )
    .await
    {
        Outcome::Ok(rows) => rows,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let actual = match decode_message_recipient_pairs(message_id, &recipient_rows) {
        Ok(actual) => actual,
        Err(error) => return Outcome::Err(error),
    };
    let expected = normalize_expected_recipients(expected_recipients);
    if actual != expected {
        return Outcome::Err(DbError::Internal(format!(
            "message recipient rows not visible after commit for message_id={message_id}: expected={} actual={}",
            expected.len(),
            actual.len()
        )));
    }

    Outcome::Ok(())
}

/// Verify message + recipient rows are query-visible after commit.
///
/// This guards against ghost success where the API returns success but
/// `message_recipients` rows are missing.
async fn verify_message_recipients_visible_after_commit(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    message_id: i64,
    expected_recipients: &[(i64, &str)],
) -> Outcome<(), DbError> {
    let fresh_result = verify_message_recipients_visible_with_probe_mode(
        cx,
        pool,
        project_id,
        message_id,
        expected_recipients,
        MessageVisibilityProbeMode::FreshHandle,
    )
    .await;
    let Outcome::Err(fresh_error) = &fresh_result else {
        return fresh_result;
    };
    if pool.sqlite_path() == ":memory:"
        || !is_message_visibility_probe_consistency_error(fresh_error)
    {
        return fresh_result;
    }
    tracing::warn!(
        project_id,
        message_id,
        fresh_error = %fresh_error,
        "fresh durability probe missed committed message visibility; refusing pooled-only confirmation"
    );
    fresh_result
}

async fn fetch_durable_atc_experience_by_decision_effect(
    cx: &Cx,
    pool: &DbPool,
    decision_id: i64,
    effect_id: i64,
) -> Outcome<ExperienceRow, DbError> {
    if pool.sqlite_path() != ":memory:" {
        return match canonical_fetch_atc_experience_by_decision_effect(pool, decision_id, effect_id)
        {
            Ok(Some(row)) => Outcome::Ok(row),
            Ok(None) => Outcome::Err(DbError::Internal(format!(
                "ATC experience row not visible after commit for decision_id={decision_id} effect_id={effect_id}"
            ))),
            Err(error) => Outcome::Err(error),
        };
    }

    let sql = format!(
        "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE decision_id = ? AND effect_id = ? LIMIT 1"
    );
    let params = [Value::BigInt(decision_id), Value::BigInt(effect_id)];
    match durability_probe_query(cx, pool, &sql, &params).await {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || {
                Outcome::Err(DbError::Internal(format!(
                    "ATC experience row not visible after commit for decision_id={decision_id} effect_id={effect_id}"
                )))
            },
            |row| match decode_atc_experience_row(row) {
                Ok(decoded) => Outcome::Ok(decoded),
                Err(error) => Outcome::Err(error),
            },
        ),
        Outcome::Err(error) => Outcome::Err(error),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

async fn fetch_durable_atc_experience_by_id(
    cx: &Cx,
    pool: &DbPool,
    experience_id: i64,
) -> Outcome<Option<ExperienceRow>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        return match canonical_fetch_atc_experience_by_id(pool, experience_id) {
            Ok(row) => Outcome::Ok(row),
            Err(error) => Outcome::Err(error),
        };
    }

    let sql = format!("{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE experience_id = ? LIMIT 1");
    let params = [Value::BigInt(experience_id)];
    match durability_probe_query(cx, pool, &sql, &params).await {
        Outcome::Ok(rows) => rows.first().map_or(Outcome::Ok(None), |row| {
            match decode_atc_experience_row(row) {
                Ok(decoded) => Outcome::Ok(Some(decoded)),
                Err(error) => Outcome::Err(error),
            }
        }),
        Outcome::Err(error) => Outcome::Err(error),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

fn open_fresh_file_backed_conn(pool: &DbPool) -> std::result::Result<crate::DbConn, DbError> {
    let conn = crate::DbConn::open_file(pool.sqlite_path())
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    conn.execute_raw(crate::schema::PRAGMA_CONN_SETTINGS_SQL)
        .map_err(|error| DbError::Sqlite(format!("fresh connection init failed: {error}")))?;
    Ok(conn)
}

// ATC raw experience rows are the only runtime table family still observed
// corrupting file-backed mailboxes under FrankenConnection writes. Route
// file-backed ATC experience IO through canonical SQLite until the upstream
// page-ordering/runtime bug is fixed.
pub(crate) fn close_canonical_db_conn(conn: crate::CanonicalDbConn, context: &'static str) {
    let _ = context;
    drop(conn);
}

pub(crate) fn open_canonical_atc_conn(
    pool: &DbPool,
    purpose: &'static str,
) -> std::result::Result<crate::CanonicalDbConn, DbError> {
    // ATC telemetry tables are isolated into a sidecar DB (atc.sqlite3), a
    // sibling of the mailbox DB, so ATC churn/bloat/corruption can never reach
    // storage.sqlite3's VACUUM/integrity/backup/size (br-bvq1x.11.7). The
    // sidecar is opened only through canonical SQLite, which sidesteps the
    // FrankenConnection page-corruption bug (br-q37ep) entirely.
    let atc_path = pool.atc_sqlite_path().ok_or_else(|| {
        DbError::Internal(format!(
            "{purpose}: canonical ATC connection requested for an in-memory pool"
        ))
    })?;
    let conn = crate::CanonicalDbConn::open_file(atc_path.as_str())
        .map_err(|error| DbError::Sqlite(format!("{purpose}: open failed: {error}")))?;
    conn.execute_raw(crate::schema::PRAGMA_CONN_SETTINGS_SQL)
        .map_err(|error| DbError::Sqlite(format!("{purpose}: init pragmas failed: {error}")))?;
    Ok(conn)
}

pub(crate) fn begin_canonical_atc_write_tx(
    conn: &crate::CanonicalDbConn,
) -> std::result::Result<(), DbError> {
    conn.execute_sync("BEGIN IMMEDIATE", &[])
        .map(|_| ())
        .map_err(|error| DbError::Sqlite(error.to_string()))
}

pub(crate) fn commit_canonical_atc_write_tx(
    conn: &crate::CanonicalDbConn,
) -> std::result::Result<(), DbError> {
    conn.execute_sync("COMMIT", &[])
        .map(|_| ())
        .map_err(|error| DbError::Sqlite(error.to_string()))
}

pub(crate) fn rollback_canonical_atc_write_tx(conn: &crate::CanonicalDbConn) {
    let _ = conn.execute_sync("ROLLBACK", &[]);
}

pub(crate) fn canonical_query_atc_rows(
    conn: &crate::CanonicalDbConn,
    sql: &str,
    params: &[Value],
    purpose: &'static str,
) -> std::result::Result<Vec<SqlRow>, DbError> {
    conn.query_sync(sql, params)
        .map_err(|error| DbError::Sqlite(format!("{purpose}: {error}")))
}

pub(crate) fn canonical_execute_atc(
    conn: &crate::CanonicalDbConn,
    sql: &str,
    params: &[Value],
    purpose: &'static str,
) -> std::result::Result<u64, DbError> {
    conn.execute_sync(sql, params)
        .map_err(|error| DbError::Sqlite(format!("{purpose}: {error}")))
}

fn canonical_fetch_atc_experience_by_decision_effect(
    pool: &DbPool,
    decision_id: i64,
    effect_id: i64,
) -> std::result::Result<Option<ExperienceRow>, DbError> {
    let conn = open_canonical_atc_conn(pool, "fetch durable ATC experience by decision/effect")?;
    let result = (|| {
        let sql = format!(
            "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE decision_id = ? AND effect_id = ? LIMIT 1"
        );
        let params = [Value::BigInt(decision_id), Value::BigInt(effect_id)];
        let rows = canonical_query_atc_rows(
            &conn,
            &sql,
            &params,
            "fetch durable ATC experience by decision/effect",
        )?;
        rows.first().map(decode_atc_experience_row).transpose()
    })();
    close_canonical_db_conn(conn, "canonical ATC decision/effect fetch connection");
    result
}

fn canonical_fetch_atc_experience_by_id(
    pool: &DbPool,
    experience_id: i64,
) -> std::result::Result<Option<ExperienceRow>, DbError> {
    let conn = open_canonical_atc_conn(pool, "fetch durable ATC experience by id")?;
    let result = (|| {
        let sql = format!("{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE experience_id = ? LIMIT 1");
        let params = [Value::BigInt(experience_id)];
        let rows =
            canonical_query_atc_rows(&conn, &sql, &params, "fetch durable ATC experience by id")?;
        rows.first().map(decode_atc_experience_row).transpose()
    })();
    close_canonical_db_conn(conn, "canonical ATC id fetch connection");
    result
}

fn append_atc_experience_file_backed(
    pool: &DbPool,
    row: &ExperienceRow,
) -> std::result::Result<ExperienceRow, DbError> {
    fn sql_i64_id(field: &str, value: u64) -> std::result::Result<i64, DbError> {
        i64::try_from(value).map_err(|_| {
            DbError::Internal(format!("{field} exceeds SQLite INTEGER range: {value}"))
        })
    }

    let decision_id = sql_i64_id("decision_id", row.decision_id)?;
    let effect_id = sql_i64_id("effect_id", row.effect_id)?;
    let feature_payload = migrate_feature_payload(
        infer_feature_schema_version(row.features.as_ref(), row.feature_ext.as_ref()),
        row.features,
        row.feature_ext.clone(),
    )
    .map_err(|error| DbError::invalid("feature_schema_version", error.to_string()))?;
    let posterior_json = encode_json(&row.posterior, "posterior_json")?;
    let non_execution_json = encode_json_optional(&row.non_execution_reason, "non_execution_json")?;
    let outcome_json = encode_json_optional(&row.outcome, "outcome_json")?;
    let features_json = encode_json_optional(&feature_payload.features, "features_json")?;
    let feature_ext_json = encode_json_optional(&feature_payload.feature_ext, "feature_ext_json")?;
    let context_json = encode_json_optional(&row.context, "context_json")?;

    let conn = open_canonical_atc_conn(pool, "append_atc_experience")?;
    let write_result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let insert_sql = "INSERT INTO atc_experiences \
            (decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem, decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json, expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy, safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json, feature_schema_version, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json) \
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
            ON CONFLICT(decision_id, effect_id) DO NOTHING";
        let insert_params = [
            Value::BigInt(decision_id),
            Value::BigInt(effect_id),
            Value::Text(row.trace_id.clone()),
            Value::Text(row.claim_id.clone()),
            Value::Text(row.evidence_id.clone()),
            Value::Text(row.state.to_string()),
            Value::Text(row.subsystem.to_string()),
            Value::Text(row.decision_class.clone()),
            Value::Text(row.subject.clone()),
            row.project_key
                .as_ref()
                .map_or(Value::Null, |value| Value::Text(value.clone())),
            row.policy_id
                .as_ref()
                .map_or(Value::Null, |value| Value::Text(value.clone())),
            Value::Text(row.effect_kind.to_string()),
            Value::Text(row.action.clone()),
            Value::Text(posterior_json),
            Value::Double(row.expected_loss),
            row.runner_up_action
                .as_ref()
                .map_or(Value::Null, |value| Value::Text(value.clone())),
            row.runner_up_loss.map_or(Value::Null, Value::Double),
            Value::Text(row.evidence_summary.clone()),
            Value::BigInt(if row.calibration_healthy { 1 } else { 0 }),
            Value::BigInt(if row.safe_mode_active { 1 } else { 0 }),
            non_execution_json.map_or(Value::Null, Value::Text),
            outcome_json.map_or(Value::Null, Value::Text),
            features_json.map_or(Value::Null, Value::Text),
            feature_ext_json.map_or(Value::Null, Value::Text),
            Value::BigInt(i64::from(feature_payload.schema_version)),
            Value::BigInt(row.created_ts_micros),
            row.dispatched_ts_micros.map_or(Value::Null, Value::BigInt),
            row.executed_ts_micros.map_or(Value::Null, Value::BigInt),
            row.resolved_ts_micros.map_or(Value::Null, Value::BigInt),
            context_json.map_or(Value::Null, Value::Text),
        ];
        canonical_execute_atc(
            &conn,
            insert_sql,
            &insert_params,
            "append_atc_experience insert",
        )?;
        commit_canonical_atc_write_tx(&conn)
    })();
    if write_result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC append connection");
    write_result?;

    canonical_fetch_atc_experience_by_decision_effect(pool, decision_id, effect_id)?.ok_or_else(
        || {
            DbError::Internal(format!(
                "ATC experience append succeeded but re-select failed for decision_id={decision_id} effect_id={effect_id}"
            ))
        },
    )
}

fn transition_atc_experience_file_backed(
    pool: &DbPool,
    experience_id: u64,
    new_state: ExperienceState,
    ts_micros: i64,
    non_execution_reason: Option<&NonExecutionReason>,
    context_patch: Option<&serde_json::Value>,
) -> std::result::Result<(), DbError> {
    let sql_experience_id = i64::try_from(experience_id).map_err(|_| {
        DbError::Internal(format!(
            "experience_id exceeds SQLite INTEGER range: {experience_id}"
        ))
    })?;

    let conn = open_canonical_atc_conn(pool, "transition_atc_experience")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let select_sql =
            format!("{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE experience_id = ? LIMIT 1");
        let rows = canonical_query_atc_rows(
            &conn,
            &select_sql,
            &[Value::BigInt(sql_experience_id)],
            "transition_atc_experience select",
        )?;
        let Some(stored) = rows.first() else {
            return Err(DbError::not_found(
                "AtcExperience",
                experience_id.to_string(),
            ));
        };
        let mut experience = decode_atc_experience_row(stored)?;

        if experience.state != new_state
            && let Err(reason) = validate_transition(experience.state, new_state)
        {
            return Err(DbError::invalid(
                "state",
                format!(
                    "experience {experience_id} cannot transition from {} to {}: {reason}",
                    experience.state, new_state
                ),
            ));
        }

        if non_execution_reason.is_some() && !new_state.is_non_execution() {
            return Err(DbError::invalid(
                "non_execution_reason",
                format!(
                    "non_execution_reason is only valid for throttled/suppressed/skipped states, got {new_state}"
                ),
            ));
        }
        if new_state.is_non_execution()
            && experience.state != new_state
            && non_execution_reason.is_none()
        {
            return Err(DbError::invalid(
                "non_execution_reason",
                format!("non_execution_reason is required when transitioning to {new_state}"),
            ));
        }

        if let Err(reason) = experience.transition_to(new_state) {
            return Err(DbError::invalid(
                "state",
                format!("experience {experience_id} transition rejected: {reason}"),
            ));
        }

        match new_state {
            ExperienceState::Planned => {}
            ExperienceState::Dispatched => {
                if experience.dispatched_ts_micros.is_none() {
                    experience.dispatched_ts_micros = Some(ts_micros);
                }
            }
            ExperienceState::Executed
            | ExperienceState::Failed
            | ExperienceState::Throttled
            | ExperienceState::Suppressed
            | ExperienceState::Skipped => {
                if experience.executed_ts_micros.is_none() {
                    experience.executed_ts_micros = Some(ts_micros);
                }
            }
            ExperienceState::Open => {}
            ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired => {
                if experience.resolved_ts_micros.is_none() {
                    experience.resolved_ts_micros = Some(ts_micros);
                }
            }
        }

        if new_state.is_non_execution() {
            if let Some(reason) = non_execution_reason {
                experience.non_execution_reason = Some(reason.clone());
            }
        } else {
            experience.non_execution_reason = None;
        }
        experience.context = merge_context_patch(experience.context.take(), context_patch)?;

        let non_exec_json =
            encode_json_optional(&experience.non_execution_reason, "non_execution_json")?;
        let context_json = encode_json_optional(&experience.context, "context_json")?;
        let update_sql = "UPDATE atc_experiences SET state = ?, non_execution_json = ?, dispatched_ts = ?, \
             executed_ts = ?, resolved_ts = ?, context_json = ? WHERE experience_id = ?";
        let update_params = [
            Value::Text(experience.state.to_string()),
            non_exec_json.map_or(Value::Null, Value::Text),
            experience
                .dispatched_ts_micros
                .map_or(Value::Null, Value::BigInt),
            experience
                .executed_ts_micros
                .map_or(Value::Null, Value::BigInt),
            experience
                .resolved_ts_micros
                .map_or(Value::Null, Value::BigInt),
            context_json.map_or(Value::Null, Value::Text),
            Value::BigInt(sql_experience_id),
        ];
        canonical_execute_atc(
            &conn,
            update_sql,
            &update_params,
            "transition_atc_experience update",
        )?;
        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC transition connection");
    result
}

fn fetch_open_atc_experiences_file_backed(
    pool: &DbPool,
    subject: Option<&str>,
    limit: u32,
) -> std::result::Result<Vec<ExperienceRow>, DbError> {
    let conn = open_canonical_atc_conn(pool, "fetch_open_atc_experiences")?;
    let result = (|| {
        let (sql, params) = if let Some(agent) = subject {
            (
                format!(
                    "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} \
                     WHERE state IN ('executed', 'open') AND subject = ? COLLATE NOCASE \
                     ORDER BY created_ts ASC LIMIT ?"
                ),
                vec![
                    Value::Text(agent.to_string()),
                    Value::BigInt(i64::from(limit)),
                ],
            )
        } else {
            (
                format!(
                    "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} \
                     WHERE state IN ('executed', 'open') \
                     ORDER BY created_ts ASC LIMIT ?"
                ),
                vec![Value::BigInt(i64::from(limit))],
            )
        };
        let rows =
            canonical_query_atc_rows(&conn, &sql, &params, "fetch_open_atc_experiences query")?;
        let mut experiences = Vec::with_capacity(rows.len());
        for row in &rows {
            match decode_atc_experience_row(row) {
                Ok(experience) => experiences.push(experience),
                Err(error) => {
                    tracing::warn!(%error, "skipping malformed atc_experience row");
                }
            }
        }
        Ok(experiences)
    })();
    close_canonical_db_conn(conn, "canonical ATC open-experiences connection");
    result
}

fn fetch_message_sent_atc_experience_file_backed(
    pool: &DbPool,
    message_id: i64,
) -> std::result::Result<Option<ExperienceRow>, DbError> {
    let conn = open_canonical_atc_conn(pool, "fetch_message_sent_atc_experience")?;
    let result = (|| {
        let sql = format!(
            "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} \
             WHERE decision_class = 'message_sent' \
               AND CAST(json_extract(context_json, '$.message_id') AS INTEGER) = ? \
             ORDER BY experience_id DESC LIMIT 1"
        );
        let params = [Value::BigInt(message_id)];
        let rows = canonical_query_atc_rows(
            &conn,
            &sql,
            &params,
            "fetch_message_sent_atc_experience query",
        )?;
        rows.first().map(decode_atc_experience_row).transpose()
    })();
    close_canonical_db_conn(conn, "canonical ATC message lookup connection");
    result
}

fn resolve_atc_experience_file_backed(
    pool: &DbPool,
    experience_id: u64,
    outcome: &ExperienceOutcome,
) -> std::result::Result<(), DbError> {
    let id = i64::try_from(experience_id).map_err(|_| {
        DbError::Internal(format!(
            "experience_id exceeds SQLite INTEGER range: {experience_id}"
        ))
    })?;
    let outcome_json = encode_json(outcome, "outcome_json")?;

    let conn = open_canonical_atc_conn(pool, "resolve_atc_experience")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let sql = "UPDATE atc_experiences \
                   SET state = 'resolved', resolved_ts = ?, outcome_json = ? \
                   WHERE experience_id = ? AND state = 'open'";
        let params = [
            Value::BigInt(outcome.observed_ts_micros),
            Value::Text(outcome_json),
            Value::BigInt(id),
        ];
        let rows_affected =
            canonical_execute_atc(&conn, sql, &params, "resolve_atc_experience update")?;
        if rows_affected == 0 {
            let rows = canonical_query_atc_rows(
                &conn,
                "SELECT state FROM atc_experiences WHERE experience_id = ? LIMIT 1",
                &[Value::BigInt(id)],
                "resolve_atc_experience select",
            )?;
            let state = rows
                .first()
                .map(|row| row_text(row, 0, "state").and_then(|raw| parse_enum(raw, "state")))
                .transpose()?;
            resolve_atc_experience_noop_result(state, experience_id)?;
        }
        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC resolve connection");
    result
}

fn overwrite_resolved_atc_experience_outcome_file_backed(
    pool: &DbPool,
    experience_id: u64,
    outcome: &ExperienceOutcome,
) -> std::result::Result<(), DbError> {
    let id = i64::try_from(experience_id).map_err(|_| {
        DbError::Internal(format!(
            "experience_id exceeds SQLite INTEGER range: {experience_id}"
        ))
    })?;
    let outcome_json = encode_json(outcome, "outcome_json")?;

    let conn = open_canonical_atc_conn(pool, "overwrite_resolved_atc_experience_outcome")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let rows = canonical_query_atc_rows(
            &conn,
            "SELECT state FROM atc_experiences WHERE experience_id = ? LIMIT 1",
            &[Value::BigInt(id)],
            "overwrite_resolved_atc_experience_outcome select",
        )?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::not_found("experience", experience_id.to_string()))?;
        let current_state: ExperienceState = parse_enum(row_text(row, 0, "state")?, "state")?;
        if current_state != ExperienceState::Resolved {
            return Err(DbError::invalid(
                "state",
                format!(
                    "experience {experience_id} is {current_state}, must be resolved to overwrite outcome"
                ),
            ));
        }
        canonical_execute_atc(
            &conn,
            "UPDATE atc_experiences \
             SET resolved_ts = ?, outcome_json = ? \
             WHERE experience_id = ? AND state = 'resolved'",
            &[
                Value::BigInt(outcome.observed_ts_micros),
                Value::Text(outcome_json),
                Value::BigInt(id),
            ],
            "overwrite_resolved_atc_experience_outcome update",
        )?;
        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC overwrite-resolved connection");
    result
}

fn resolve_atc_experience_noop_result(
    current_state: Option<ExperienceState>,
    experience_id: u64,
) -> std::result::Result<(), DbError> {
    match current_state {
        Some(ExperienceState::Resolved) => Ok(()),
        Some(current_state) => Err(DbError::invalid(
            "state",
            format!("experience {experience_id} is {current_state}, must be open to resolve"),
        )),
        None => Err(DbError::not_found("experience", experience_id.to_string())),
    }
}

fn resolve_experience_file_backed(
    pool: &DbPool,
    experience_id: u64,
    resolution: &ResolutionKind,
) -> std::result::Result<(), DbError> {
    let id = i64::try_from(experience_id).map_err(|_| {
        DbError::Internal(format!(
            "experience_id exceeds SQLite INTEGER range: {experience_id}"
        ))
    })?;

    let conn = open_canonical_atc_conn(pool, "resolve_experience")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;

        let select_sql = "SELECT state FROM atc_experiences WHERE experience_id = ? LIMIT 1";
        let rows = canonical_query_atc_rows(
            &conn,
            select_sql,
            &[Value::BigInt(id)],
            "resolve_experience select",
        )?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::not_found("experience", experience_id.to_string()))?;
        let current_state: ExperienceState = parse_enum(row_text(row, 0, "state")?, "state")?;
        let target = resolution.target_state();

        if current_state == target {
            return Ok(());
        }
        if current_state.is_terminal() {
            return Err(DbError::invalid(
                "state",
                format!(
                    "experience {experience_id} already terminal as {current_state}, \
                     cannot re-resolve as {target}"
                ),
            ));
        }
        if current_state != ExperienceState::Open {
            return Err(DbError::invalid(
                "state",
                format!("experience {experience_id} is {current_state}, must be open to resolve"),
            ));
        }

        match resolution {
            ResolutionKind::Resolved(outcome) => {
                let outcome_json = encode_json(outcome, "outcome_json")?;
                let sql = "UPDATE atc_experiences \
                           SET state = 'resolved', resolved_ts = ?, outcome_json = ? \
                           WHERE experience_id = ?";
                canonical_execute_atc(
                    &conn,
                    sql,
                    &[
                        Value::BigInt(outcome.observed_ts_micros),
                        Value::Text(outcome_json),
                        Value::BigInt(id),
                    ],
                    "resolve_experience resolved",
                )?;
            }
            ResolutionKind::Censored { ts_micros } => {
                let sql = "UPDATE atc_experiences \
                           SET state = 'censored', resolved_ts = ? \
                           WHERE experience_id = ?";
                canonical_execute_atc(
                    &conn,
                    sql,
                    &[Value::BigInt(*ts_micros), Value::BigInt(id)],
                    "resolve_experience censored",
                )?;
            }
            ResolutionKind::Expired { ts_micros } => {
                let sql = "UPDATE atc_experiences \
                           SET state = 'expired', resolved_ts = ? \
                           WHERE experience_id = ?";
                canonical_execute_atc(
                    &conn,
                    sql,
                    &[Value::BigInt(*ts_micros), Value::BigInt(id)],
                    "resolve_experience expired",
                )?;
            }
        }

        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC resolve_experience connection");
    result
}

fn update_atc_experience_rollup_file_backed(
    pool: &DbPool,
    stratum_key: &str,
    subsystem: &str,
    effect_kind: &str,
    risk_tier: i32,
    resolution_state: ExperienceState,
    correct: bool,
    actual_loss: f64,
    regret: f64,
    delay_micros: i64,
    ts_micros: i64,
) -> std::result::Result<(), DbError> {
    const EWMA_LAMBDA: f64 = 0.95;

    let (resolved_inc, censored_inc, expired_inc) = match resolution_state {
        ExperienceState::Resolved => (1, 0, 0),
        ExperienceState::Censored => (0, 1, 0),
        ExperienceState::Expired => (0, 0, 1),
        _ => (0, 0, 0),
    };

    let correct_inc: i64 = if correct { 1 } else { 0 };
    let incorrect_inc: i64 = if correct { 0 } else { 1 };
    let one_minus_lambda = 1.0 - EWMA_LAMBDA;

    let conn = open_canonical_atc_conn(pool, "update_atc_experience_rollup")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let sql = "\
            INSERT INTO atc_experience_rollups \
                (stratum_key, subsystem, effect_kind, risk_tier, \
                 total_count, resolved_count, censored_count, expired_count, \
                 correct_count, incorrect_count, total_regret, total_loss, \
                 ewma_loss, ewma_weight, delay_sum_micros, delay_count, delay_max_micros, \
                 last_updated_ts) \
            VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, 1.0, ?, 1, ?, ?) \
            ON CONFLICT(stratum_key) DO UPDATE SET \
                total_count = total_count + 1, \
                resolved_count = resolved_count + ?, \
                censored_count = censored_count + ?, \
                expired_count = expired_count + ?, \
                correct_count = correct_count + ?, \
                incorrect_count = incorrect_count + ?, \
                total_regret = total_regret + ?, \
                total_loss = total_loss + ?, \
                ewma_loss = ? * ewma_loss + ? * ?, \
                ewma_weight = ? * ewma_weight + 1.0, \
                delay_sum_micros = delay_sum_micros + ?, \
                delay_count = delay_count + 1, \
                delay_max_micros = MAX(delay_max_micros, ?), \
                last_updated_ts = ?";
        let params = vec![
            Value::Text(stratum_key.to_string()),
            Value::Text(subsystem.to_string()),
            Value::Text(effect_kind.to_string()),
            Value::Int(risk_tier),
            Value::BigInt(resolved_inc),
            Value::BigInt(censored_inc),
            Value::BigInt(expired_inc),
            Value::BigInt(correct_inc),
            Value::BigInt(incorrect_inc),
            Value::Double(regret),
            Value::Double(actual_loss),
            Value::Double(actual_loss),
            Value::BigInt(delay_micros),
            Value::BigInt(delay_micros),
            Value::BigInt(ts_micros),
            Value::BigInt(resolved_inc),
            Value::BigInt(censored_inc),
            Value::BigInt(expired_inc),
            Value::BigInt(correct_inc),
            Value::BigInt(incorrect_inc),
            Value::Double(regret),
            Value::Double(actual_loss),
            Value::Double(EWMA_LAMBDA),
            Value::Double(one_minus_lambda),
            Value::Double(actual_loss),
            Value::Double(EWMA_LAMBDA),
            Value::BigInt(delay_micros),
            Value::BigInt(delay_micros),
            Value::BigInt(ts_micros),
        ];
        canonical_execute_atc(&conn, sql, &params, "update_atc_experience_rollup upsert")?;
        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC rollup update connection");
    result
}

fn fetch_atc_rollups_file_backed(pool: &DbPool) -> std::result::Result<Vec<AtcRollupRow>, DbError> {
    let conn = open_canonical_atc_conn(pool, "fetch_atc_rollups")?;
    let result = (|| {
        let sql = "SELECT stratum_key, total_count, resolved_count, censored_count, \
                   expired_count, correct_count, incorrect_count, total_regret, total_loss, \
                   ewma_loss, ewma_weight \
                   FROM atc_experience_rollups ORDER BY stratum_key";
        let rows = canonical_query_atc_rows(&conn, sql, &[], "fetch_atc_rollups query")?;
        let mut results = Vec::with_capacity(rows.len());
        for row in &rows {
            let stratum_key = row
                .get(0)
                .and_then(|v| match v {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let total = row.get(1).and_then(value_as_i64).unwrap_or(0);
            let resolved = row.get(2).and_then(value_as_i64).unwrap_or(0);
            let censored = row.get(3).and_then(value_as_i64).unwrap_or(0);
            let expired = row.get(4).and_then(value_as_i64).unwrap_or(0);
            let correct = row.get(5).and_then(value_as_i64).unwrap_or(0);
            let incorrect = row.get(6).and_then(value_as_i64).unwrap_or(0);
            let regret = row
                .get(7)
                .and_then(|v| match v {
                    Value::Double(f) => Some(*f),
                    Value::Float(f) => Some(f64::from(*f)),
                    _ => None,
                })
                .unwrap_or(0.0);
            let loss = row
                .get(8)
                .and_then(|v| match v {
                    Value::Double(f) => Some(*f),
                    Value::Float(f) => Some(f64::from(*f)),
                    _ => None,
                })
                .unwrap_or(0.0);
            let ewma = row
                .get(9)
                .and_then(|v| match v {
                    Value::Double(f) => Some(*f),
                    Value::Float(f) => Some(f64::from(*f)),
                    _ => None,
                })
                .unwrap_or(0.0);
            let weight = row
                .get(10)
                .and_then(|v| match v {
                    Value::Double(f) => Some(*f),
                    Value::Float(f) => Some(f64::from(*f)),
                    _ => None,
                })
                .unwrap_or(0.0);
            results.push((
                stratum_key,
                total,
                resolved,
                censored,
                expired,
                correct,
                incorrect,
                regret,
                loss,
                ewma,
                weight,
            ));
        }
        Ok(results)
    })();
    close_canonical_db_conn(conn, "canonical ATC rollups fetch connection");
    result
}

fn row_text_or_default(row: &SqlRow, idx: usize) -> String {
    row.get(idx)
        .and_then(|value| match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn row_i64_or_default(row: &SqlRow, idx: usize) -> i64 {
    row.get(idx).and_then(value_as_i64).unwrap_or(0)
}

fn row_f64_or_default(row: &SqlRow, idx: usize) -> f64 {
    row.get(idx).and_then(value_as_f64).unwrap_or(0.0)
}

fn decode_atc_rollup_snapshot_row(row: &SqlRow) -> AtcRollupSnapshotRow {
    AtcRollupSnapshotRow {
        stratum_key: row_text_or_default(row, 0),
        subsystem: row_text_or_default(row, 1),
        effect_kind: row_text_or_default(row, 2),
        risk_tier: row_i64_or_default(row, 3),
        total_count: row_i64_or_default(row, 4),
        resolved_count: row_i64_or_default(row, 5),
        censored_count: row_i64_or_default(row, 6),
        expired_count: row_i64_or_default(row, 7),
        correct_count: row_i64_or_default(row, 8),
        incorrect_count: row_i64_or_default(row, 9),
        total_regret: row_f64_or_default(row, 10),
        total_loss: row_f64_or_default(row, 11),
        ewma_loss: row_f64_or_default(row, 12),
        ewma_weight: row_f64_or_default(row, 13),
        delay_sum_micros: row_i64_or_default(row, 14),
        delay_count: row_i64_or_default(row, 15),
        delay_max_micros: row_i64_or_default(row, 16),
        last_updated_ts: row_i64_or_default(row, 17),
        compacted_total_count: row_i64_or_default(row, 18),
        compacted_resolved_count: row_i64_or_default(row, 19),
        compacted_censored_count: row_i64_or_default(row, 20),
        compacted_expired_count: row_i64_or_default(row, 21),
        compacted_correct_count: row_i64_or_default(row, 22),
        compacted_incorrect_count: row_i64_or_default(row, 23),
        compacted_total_regret: row_f64_or_default(row, 24),
        compacted_total_loss: row_f64_or_default(row, 25),
        compacted_ewma_loss: row_f64_or_default(row, 26),
        compacted_ewma_weight: row_f64_or_default(row, 27),
        compacted_delay_sum_micros: row_i64_or_default(row, 28),
        compacted_delay_count: row_i64_or_default(row, 29),
        compacted_delay_max_micros: row_i64_or_default(row, 30),
        compacted_last_updated_ts: row_i64_or_default(row, 31),
    }
}

fn fetch_atc_rollup_snapshot_rows_file_backed(
    pool: &DbPool,
) -> std::result::Result<Vec<AtcRollupSnapshotRow>, DbError> {
    let conn = open_canonical_atc_conn(pool, "snapshot_atc_rollups fetch")?;
    let result = (|| {
        let rows = canonical_query_atc_rows(
            &conn,
            ATC_ROLLUP_SNAPSHOT_SELECT_SQL,
            &[],
            "snapshot_atc_rollups fetch",
        )?;
        Ok(rows.iter().map(decode_atc_rollup_snapshot_row).collect())
    })();
    close_canonical_db_conn(conn, "canonical ATC rollup snapshot fetch connection");
    result
}

// ── File-backed ATC rollup snapshot ─────────────────────────────────────

fn snapshot_atc_rollups_file_backed(
    pool: &DbPool,
    now_micros: i64,
) -> std::result::Result<RollupSnapshot, DbError> {
    let rollups = fetch_atc_rollup_snapshot_rows_file_backed(pool)?;
    let payload = serde_json::to_string(&rollups)
        .map_err(|e| DbError::Internal(format!("snapshot rollups serialize: {e}")))?;
    let sha256 = sha256_hex(&payload);
    #[allow(clippy::cast_possible_wrap)]
    let row_count = rollups.len() as i64;

    let conn = open_canonical_atc_conn(pool, "snapshot_atc_rollups")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        canonical_execute_atc(
            &conn,
            "INSERT INTO atc_rollup_snapshots (captured_ts, rollup_rows, payload_sha256) \
             VALUES (?, ?, ?)",
            &[
                Value::BigInt(now_micros),
                Value::BigInt(row_count),
                Value::Text(sha256.clone()),
            ],
            "snapshot_rollups insert",
        )?;
        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC snapshot connection");
    result?;
    Ok(RollupSnapshot {
        captured_ts_micros: now_micros,
        rollup_rows: row_count,
        payload_sha256: sha256,
        payload,
    })
}

const RESTORE_ROLLUP_UPSERT_SQL: &str = "\
    INSERT INTO atc_experience_rollups \
    (stratum_key, subsystem, effect_kind, risk_tier, \
     total_count, resolved_count, censored_count, expired_count, \
     correct_count, incorrect_count, total_regret, total_loss, \
     ewma_loss, ewma_weight, delay_sum_micros, delay_count, delay_max_micros, \
     last_updated_ts, compacted_total_count, compacted_resolved_count, \
     compacted_censored_count, compacted_expired_count, compacted_correct_count, \
     compacted_incorrect_count, compacted_total_regret, compacted_total_loss, \
     compacted_ewma_loss, compacted_ewma_weight, compacted_delay_sum_micros, \
     compacted_delay_count, compacted_delay_max_micros, compacted_last_updated_ts) \
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
    ON CONFLICT(stratum_key) DO UPDATE SET \
     subsystem = excluded.subsystem, \
     effect_kind = excluded.effect_kind, \
     risk_tier = excluded.risk_tier, \
     total_count = excluded.total_count, \
     resolved_count = excluded.resolved_count, \
     censored_count = excluded.censored_count, \
     expired_count = excluded.expired_count, \
     correct_count = excluded.correct_count, \
     incorrect_count = excluded.incorrect_count, \
     total_regret = excluded.total_regret, \
     total_loss = excluded.total_loss, \
     ewma_loss = excluded.ewma_loss, \
     ewma_weight = excluded.ewma_weight, \
     delay_sum_micros = excluded.delay_sum_micros, \
     delay_count = excluded.delay_count, \
     delay_max_micros = excluded.delay_max_micros, \
     last_updated_ts = excluded.last_updated_ts, \
     compacted_total_count = excluded.compacted_total_count, \
     compacted_resolved_count = excluded.compacted_resolved_count, \
     compacted_censored_count = excluded.compacted_censored_count, \
     compacted_expired_count = excluded.compacted_expired_count, \
     compacted_correct_count = excluded.compacted_correct_count, \
     compacted_incorrect_count = excluded.compacted_incorrect_count, \
     compacted_total_regret = excluded.compacted_total_regret, \
     compacted_total_loss = excluded.compacted_total_loss, \
     compacted_ewma_loss = excluded.compacted_ewma_loss, \
     compacted_ewma_weight = excluded.compacted_ewma_weight, \
     compacted_delay_sum_micros = excluded.compacted_delay_sum_micros, \
     compacted_delay_count = excluded.compacted_delay_count, \
     compacted_delay_max_micros = excluded.compacted_delay_max_micros, \
     compacted_last_updated_ts = excluded.compacted_last_updated_ts";

fn restore_rollup_params(row: &AtcRollupSnapshotRow) -> Vec<Value> {
    vec![
        Value::Text(row.stratum_key.clone()),
        Value::Text(row.subsystem.clone()),
        Value::Text(row.effect_kind.clone()),
        Value::BigInt(row.risk_tier),
        Value::BigInt(row.total_count),
        Value::BigInt(row.resolved_count),
        Value::BigInt(row.censored_count),
        Value::BigInt(row.expired_count),
        Value::BigInt(row.correct_count),
        Value::BigInt(row.incorrect_count),
        Value::Double(row.total_regret),
        Value::Double(row.total_loss),
        Value::Double(row.ewma_loss),
        Value::Double(row.ewma_weight),
        Value::BigInt(row.delay_sum_micros),
        Value::BigInt(row.delay_count),
        Value::BigInt(row.delay_max_micros),
        Value::BigInt(row.last_updated_ts),
        Value::BigInt(row.compacted_total_count),
        Value::BigInt(row.compacted_resolved_count),
        Value::BigInt(row.compacted_censored_count),
        Value::BigInt(row.compacted_expired_count),
        Value::BigInt(row.compacted_correct_count),
        Value::BigInt(row.compacted_incorrect_count),
        Value::Double(row.compacted_total_regret),
        Value::Double(row.compacted_total_loss),
        Value::Double(row.compacted_ewma_loss),
        Value::Double(row.compacted_ewma_weight),
        Value::BigInt(row.compacted_delay_sum_micros),
        Value::BigInt(row.compacted_delay_count),
        Value::BigInt(row.compacted_delay_max_micros),
        Value::BigInt(row.compacted_last_updated_ts),
    ]
}

fn parse_rollup_payload(payload: &str) -> std::result::Result<Vec<AtcRollupSnapshotRow>, DbError> {
    serde_json::from_str(payload)
        .map_err(|e| DbError::Internal(format!("restore rollups deserialize: {e}")))
}

fn restore_atc_rollups_from_payload(
    conn: &crate::CanonicalDbConn,
    payload: &str,
    _now_micros: i64,
) -> std::result::Result<usize, DbError> {
    let rollups = parse_rollup_payload(payload)?;
    let mut restored = 0usize;
    for row in &rollups {
        let params = restore_rollup_params(row);
        conn.execute_sync(RESTORE_ROLLUP_UPSERT_SQL, &params)
            .map_err(|e| DbError::Sqlite(format!("restore rollup upsert: {e}")))?;
        restored += 1;
    }
    Ok(restored)
}

fn restore_atc_rollups_from_payload_pooled(
    conn: &crate::DbConn,
    payload: &str,
    _now_micros: i64,
) -> std::result::Result<usize, DbError> {
    let rollups = parse_rollup_payload(payload)?;
    let mut restored = 0usize;
    for row in &rollups {
        let params = restore_rollup_params(row);
        conn.execute_sync(RESTORE_ROLLUP_UPSERT_SQL, &params)
            .map_err(|e| DbError::Sqlite(format!("restore rollup upsert: {e}")))?;
        restored += 1;
    }
    Ok(restored)
}

// ── File-backed ATC leader lease ────────────────────────────────────────

fn try_acquire_atc_leader_lease_file_backed(
    pool: &DbPool,
    instance_id: &str,
    now_micros: i64,
    ttl_micros: i64,
) -> std::result::Result<LeaseOutcome, DbError> {
    let conn = open_canonical_atc_conn(pool, "try_acquire_atc_leader_lease")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;

        let rows = canonical_query_atc_rows(
            &conn,
            "SELECT instance_id, renewed_at, ttl_micros FROM atc_leader_lease WHERE lease_slot = 1",
            &[],
            "leader_lease select",
        )?;

        if let Some(row) = rows.first() {
            let holder = row_text(row, 0, "instance_id")?;
            let renewed = row.get(1).and_then(value_as_i64).unwrap_or(0);
            let ttl = row.get(2).and_then(value_as_i64).unwrap_or(0);

            if holder == instance_id {
                canonical_execute_atc(
                    &conn,
                    "UPDATE atc_leader_lease SET renewed_at = ?, ttl_micros = ? WHERE lease_slot = 1",
                    &[Value::BigInt(now_micros), Value::BigInt(ttl_micros)],
                    "leader_lease renew own",
                )?;
                commit_canonical_atc_write_tx(&conn)?;
                return Ok(LeaseOutcome::Acquired);
            }

            let expires_at = renewed.saturating_add(ttl);
            if now_micros >= expires_at {
                canonical_execute_atc(
                    &conn,
                    "UPDATE atc_leader_lease SET instance_id = ?, acquired_at = ?, renewed_at = ?, ttl_micros = ? WHERE lease_slot = 1",
                    &[
                        Value::Text(instance_id.to_string()),
                        Value::BigInt(now_micros),
                        Value::BigInt(now_micros),
                        Value::BigInt(ttl_micros),
                    ],
                    "leader_lease steal expired",
                )?;
                commit_canonical_atc_write_tx(&conn)?;
                return Ok(LeaseOutcome::Acquired);
            }

            commit_canonical_atc_write_tx(&conn)?;
            return Ok(LeaseOutcome::NotLeader {
                holder,
                expires_at_micros: expires_at,
            });
        }

        canonical_execute_atc(
            &conn,
            "INSERT INTO atc_leader_lease (lease_slot, instance_id, acquired_at, renewed_at, ttl_micros) \
             VALUES (1, ?, ?, ?, ?)",
            &[
                Value::Text(instance_id.to_string()),
                Value::BigInt(now_micros),
                Value::BigInt(now_micros),
                Value::BigInt(ttl_micros),
            ],
            "leader_lease insert",
        )?;
        commit_canonical_atc_write_tx(&conn)?;
        Ok(LeaseOutcome::Acquired)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC leader lease connection");
    result
}

fn renew_atc_leader_lease_file_backed(
    pool: &DbPool,
    instance_id: &str,
    now_micros: i64,
    ttl_micros: i64,
) -> std::result::Result<bool, DbError> {
    let conn = open_canonical_atc_conn(pool, "renew_atc_leader_lease")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let rows = canonical_query_atc_rows(
            &conn,
            "SELECT instance_id FROM atc_leader_lease WHERE lease_slot = 1",
            &[],
            "leader_lease renew select",
        )?;
        let is_ours = rows
            .first()
            .and_then(|r| r.get(0))
            .is_some_and(|v| matches!(v, Value::Text(s) if s == instance_id));
        if !is_ours {
            commit_canonical_atc_write_tx(&conn)?;
            return Ok(false);
        }
        canonical_execute_atc(
            &conn,
            "UPDATE atc_leader_lease SET renewed_at = ?, ttl_micros = ? WHERE lease_slot = 1 AND instance_id = ?",
            &[
                Value::BigInt(now_micros),
                Value::BigInt(ttl_micros),
                Value::Text(instance_id.to_string()),
            ],
            "leader_lease renew update",
        )?;
        commit_canonical_atc_write_tx(&conn)?;
        Ok(true)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC leader lease renew connection");
    result
}

fn release_atc_leader_lease_file_backed(
    pool: &DbPool,
    instance_id: &str,
) -> std::result::Result<bool, DbError> {
    let conn = open_canonical_atc_conn(pool, "release_atc_leader_lease")?;
    let result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        canonical_execute_atc(
            &conn,
            "DELETE FROM atc_leader_lease WHERE lease_slot = 1 AND instance_id = ?",
            &[Value::Text(instance_id.to_string())],
            "leader_lease release",
        )?;
        commit_canonical_atc_write_tx(&conn)?;
        Ok(true)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC leader lease release connection");
    result
}

fn is_hard_post_commit_probe_error(error: &DbError) -> bool {
    // GH#179: a post-commit durability probe exists to prove a committed write is
    // query-visible from an independent handle before we report success. A probe
    // failure is only "hard" — i.e. positive proof the committed state is
    // genuinely inconsistent and the row must be rolled back — when the probe
    // actually RAN and observed the message row (or its recipient rows) to be
    // absent/mismatched (a "ghost success").
    //
    // Every OTHER probe outcome — `SQLITE_BUSY` / "database is locked" / MVCC
    // snapshot conflict / pool exhaustion / disk IO error / cancel / panic — is a
    // probe *execution* failure. It says nothing about whether the committed row
    // is durable; it only means we could not re-prove visibility from a fresh
    // handle right now. Treating those as hard and deleting the already-committed
    // `messages` / `message_recipients` rows is data loss under concurrent write
    // pressure. Such errors are advisory: the committed row stays durable.
    is_message_visibility_probe_consistency_error(error)
}

/// Decide whether a post-commit visibility-probe failure justifies destructive
/// cleanup of the already-committed message.
///
/// GH#179: cleanup is only justified when the probe positively observed the
/// committed state to be inconsistent (a ghost success, per
/// [`is_hard_post_commit_probe_error`]) AND the writer's own post-commit sample
/// did not independently confirm the message row landed. A transient/execution
/// probe failure — or a probe failure contradicted by the writer's own
/// confirmation that the row is present — must never delete committed data.
fn post_commit_probe_requires_cleanup(error: &DbError, writer_confirms_durable: bool) -> bool {
    is_hard_post_commit_probe_error(error) && !writer_confirms_durable
}

fn post_commit_probe_cancelled_error(operation: &'static str, detail: &str) -> DbError {
    DbError::ResourceBusy(format!(
        "{operation} durability probe cancelled after commit for {detail}"
    ))
}

fn post_commit_probe_panicked_error(
    operation: &'static str,
    detail: &str,
    panic_message: &str,
) -> DbError {
    DbError::Internal(format!(
        "{operation} durability probe panicked after commit for {detail}: {panic_message}"
    ))
}

fn log_advisory_post_commit_probe_error(operation: &'static str, detail: &str, error: &str) {
    tracing::warn!(
        operation,
        detail,
        error,
        "post-commit durability probe failed after commit; returning committed result"
    );
}

async fn cleanup_created_agent_after_post_commit_probe_failure(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    agent_name: &str,
    error: DbError,
) -> DbError {
    let error_text = error.to_string();
    match cleanup_committed_agent_after_consistency_failure(
        cx, pool, project_id, agent_id, agent_name,
    )
    .await
    {
        Outcome::Ok(()) => error,
        Outcome::Err(cleanup_err) => DbError::Internal(format!(
            "post-commit agent visibility failed for project_id={project_id} name={agent_name}: {error_text}; cleanup failed: {cleanup_err}"
        )),
        Outcome::Cancelled(_) => DbError::Internal(format!(
            "post-commit agent visibility failed for project_id={project_id} name={agent_name}: {error_text}; cleanup was cancelled"
        )),
        Outcome::Panicked(p) => DbError::Internal(format!(
            "post-commit agent visibility failed for project_id={project_id} name={agent_name}: {error_text}; cleanup panicked: {}",
            p.message()
        )),
    }
}

async fn finalize_register_agent_post_commit_probe(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
    provisional: &AgentRow,
    inserted_new: bool,
    probe_result: Outcome<AgentRow, DbError>,
) -> Outcome<Option<AgentRow>, DbError> {
    let probe_detail = format!("{project_id}:{name}");
    let provisional_id = provisional.id.unwrap_or(0);
    match probe_result {
        Outcome::Ok(agent) => Outcome::Ok(Some(agent)),
        Outcome::Err(error) => {
            if inserted_new {
                Outcome::Err(
                    cleanup_created_agent_after_post_commit_probe_failure(
                        cx,
                        pool,
                        project_id,
                        provisional_id,
                        name,
                        error,
                    )
                    .await,
                )
            } else {
                log_advisory_post_commit_probe_error(
                    "register_agent",
                    &probe_detail,
                    &error.to_string(),
                );
                Outcome::Ok(None)
            }
        }
        Outcome::Cancelled(_) => {
            if inserted_new {
                Outcome::Err(
                    cleanup_created_agent_after_post_commit_probe_failure(
                        cx,
                        pool,
                        project_id,
                        provisional_id,
                        name,
                        post_commit_probe_cancelled_error("register_agent", &probe_detail),
                    )
                    .await,
                )
            } else {
                tracing::warn!(
                    project_id,
                    agent = %name,
                    "register_agent durability probe cancelled after commit; returning committed result"
                );
                Outcome::Ok(None)
            }
        }
        Outcome::Panicked(panic) => {
            if inserted_new {
                Outcome::Err(
                    cleanup_created_agent_after_post_commit_probe_failure(
                        cx,
                        pool,
                        project_id,
                        provisional_id,
                        name,
                        post_commit_probe_panicked_error(
                            "register_agent",
                            &probe_detail,
                            panic.message(),
                        ),
                    )
                    .await,
                )
            } else {
                tracing::error!(
                    project_id,
                    agent = %name,
                    panic = %panic.message(),
                    "register_agent durability probe panicked after commit; returning committed result"
                );
                Outcome::Ok(None)
            }
        }
    }
}

async fn cleanup_message_after_post_commit_probe_failure(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    message_id: i64,
    recipient_agent_ids: &[i64],
    error: DbError,
) -> DbError {
    let error_text = error.to_string();
    match cleanup_committed_message_after_consistency_failure(
        cx,
        pool,
        project_id,
        message_id,
        recipient_agent_ids,
    )
    .await
    {
        Outcome::Ok(()) => error,
        Outcome::Err(cleanup_err) => DbError::Internal(format!(
            "post-commit recipient visibility failed for message_id={message_id}: {error_text}; cleanup failed: {cleanup_err}"
        )),
        Outcome::Cancelled(_) => DbError::Internal(format!(
            "post-commit recipient visibility failed for message_id={message_id}: {error_text}; cleanup was cancelled"
        )),
        Outcome::Panicked(p) => DbError::Internal(format!(
            "post-commit recipient visibility failed for message_id={message_id}: {error_text}; cleanup panicked: {}",
            p.message()
        )),
    }
}

async fn cleanup_committed_agent_after_consistency_failure(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    agent_name: &str,
) -> Outcome<(), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(
            traw_execute(
                cx,
                &tracked,
                "DELETE FROM agents WHERE id = ? AND project_id = ?",
                &[Value::BigInt(agent_id), Value::BigInt(project_id)],
            )
            .await,
        )
    );
    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    drop(conn);
    crate::cache::read_cache().invalidate_agent_scoped(
        &cache_scope_for_pool(pool),
        project_id,
        agent_name,
        Some(agent_id),
    );
    Outcome::Ok(())
}

async fn cleanup_committed_message_after_consistency_failure(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    message_id: i64,
    recipient_agent_ids: &[i64],
) -> Outcome<(), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    let recipient_rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(
            traw_query(
                cx,
                &tracked,
                "SELECT DISTINCT agent_id FROM message_recipients WHERE message_id = ?",
                &[Value::BigInt(message_id)],
            )
            .await,
        )
    );

    // Use a HashSet for O(1) dedup instead of O(n) Vec::contains per insertion.
    let mut seen =
        std::collections::HashSet::with_capacity(recipient_agent_ids.len() + recipient_rows.len());
    let mut affected_agent_ids =
        Vec::with_capacity(recipient_agent_ids.len() + recipient_rows.len());
    for &agent_id in recipient_agent_ids {
        if seen.insert(agent_id) {
            affected_agent_ids.push(agent_id);
        }
    }
    for row in &recipient_rows {
        let agent_id: i64 = match row.get_as(0) {
            Ok(value) => value,
            Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(map_sql_error(&e));
            }
        };
        if seen.insert(agent_id) {
            affected_agent_ids.push(agent_id);
        }
    }

    let delete_recipients_sql = "DELETE FROM message_recipients WHERE message_id = ?";
    let delete_recipients_params = [Value::BigInt(message_id)];
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(
            traw_execute(
                cx,
                &tracked,
                delete_recipients_sql,
                &delete_recipients_params
            )
            .await
        )
    );

    let delete_message_sql = "DELETE FROM messages WHERE id = ? AND project_id = ?";
    let delete_message_params = [Value::BigInt(message_id), Value::BigInt(project_id)];
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(
            traw_execute(cx, &tracked, delete_message_sql, &delete_message_params).await
        )
    );

    try_in_tx!(
        cx,
        &tracked,
        rebuild_agents_inbox_stats_in_tx(cx, &tracked, &affected_agent_ids).await
    );

    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    drop(conn);

    let cache = crate::cache::read_cache();
    let cache_scope = cache_scope_for_pool(pool);
    for agent_id in affected_agent_ids {
        cache.invalidate_inbox_stats_scoped(&cache_scope, agent_id);
    }
    Outcome::Ok(())
}

// =============================================================================
// MVCC conflict retry helpers
// =============================================================================

/// Maximum retry attempts for MVCC write conflicts (`BEGIN CONCURRENT`
/// page-level collisions) and plain `SQLite` write contention.
///
/// Read once from `FSQLITE_CONCURRENT_RETRIES` env var; default 16.
///
/// History:
///   - 5  → 8: exponential backoff (25ms..2s) gets ~5s total budget to
///     outlast transient WAL checkpoint stalls under sustained writes.
///   - 8  → 16: #98 reported 6 concurrent `register_agent` callers with
///     distinct `project_key`s producing a ~33 % failure rate — the 8-retry
///     budget exhausts before all writers settle. 16 retries with the wider
///     max-delay ceiling below give ~20s cumulative budget while staying
///     under the 60s SQLite `busy_timeout`, so the last-in-queue writer
///     still makes it through rather than surfacing RESOURCE_BUSY to
///     every multi-agent swarm entry-tool caller.
static MVCC_MAX_RETRIES: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
    std::env::var("FSQLITE_CONCURRENT_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16)
});

/// Global counter: total MVCC retries performed.
static MVCC_RETRIES_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Global counter: MVCC conflicts that exhausted all retries.
static MVCC_EXHAUSTED_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Check if a [`DbError`] is an MVCC write conflict.
///
/// `BusySnapshot` arrives from frankensqlite as a lock-classified
/// `ResourceBusy("database is busy (snapshot conflict on pages: ...)")`,
/// so we must recognize both variants to restart the whole transaction.
fn is_mvcc_error(e: &DbError) -> bool {
    matches!(
        e,
        DbError::Sqlite(msg) | DbError::ResourceBusy(msg) if crate::error::is_mvcc_conflict(msg)
    )
}

/// Check if a [`DbError`] is a plain `SQLite` write-contention failure.
///
/// This intentionally stays narrower than [`crate::error::is_lock_error`]:
/// we retry lock/busy contention, but we do not loop on broader open/I/O
/// failures that share the same high-level `ResourceBusy` classification.
fn is_plain_write_contention_error(e: &DbError) -> bool {
    matches!(
        e,
        DbError::Sqlite(msg) | DbError::ResourceBusy(msg) if {
            let lower = msg.to_ascii_lowercase();
            lower.contains("database is locked")
                || lower.contains("database table is locked")
                || lower.contains("database schema is locked")
                || lower.contains("database is busy")
                || lower.contains("locked by another process")
        }
    )
}

/// Run a whole transaction body with whole-transaction retry semantics.
///
/// `BEGIN CONCURRENT` conflicts cannot be retried in-place at the failed
/// statement or `COMMIT`; the entire transaction body must restart from the
/// beginning so reads are re-bound against the latest snapshot. The helper also
/// retries plain `SQLite` busy/locked contention for the same reason: once a
/// write transaction has failed mid-flight, retrying a single statement is not
/// sufficient to guarantee a coherent outcome.
///
/// **IMPORTANT (GH#65):** Each retry *must* issue a fresh `BEGIN CONCURRENT`
/// (or `BEGIN IMMEDIATE`) so that the new transaction obtains a current
/// snapshot from `load_consistent_snapshot()`.  Reusing a connection's
/// stale snapshot across retries causes `fcw_base_drift` rejection where
/// `snapshot_high` permanently lags behind `commit_seq`.  Callers achieve
/// this by placing `begin_concurrent_tx` inside the closure passed here.
async fn run_with_mvcc_retry<T, F, Fut>(
    cx: &Cx,
    operation: &'static str,
    op: F,
) -> Outcome<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Outcome<T, DbError>>,
{
    run_with_mvcc_retry_with_budget(cx, operation, *MVCC_MAX_RETRIES, op).await
}

/// [`run_with_mvcc_retry`] with an explicit retry budget (testable core).
///
/// On budget exhaustion the final busy/MVCC error is wrapped in
/// [`DbError::RetryBudgetExhausted`] so downstream envelopes can report the
/// attempts made, the wall-clock time spent, and honest fallback guidance
/// instead of advising yet another blind retry (br-bvq1x.4.3 / D3).
async fn run_with_mvcc_retry_with_budget<T, F, Fut>(
    _cx: &Cx,
    operation: &'static str,
    max: u32,
    mut op: F,
) -> Outcome<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Outcome<T, DbError>>,
{
    // K3 (br-bvq1x.11.3): if the corruption circuit breaker is open, refuse the
    // write immediately — without touching the database again — so agents stop
    // hammering a corrupt store. Reads do not go through this wrapper, and the
    // CLI/doctor sync path runs in a separate process, so recovery is never
    // gated.
    if let Some(refusal) = crate::corruption_circuit_breaker().refusal_error() {
        return Outcome::Err(refusal);
    }

    let started = std::time::Instant::now();
    let exhausted = |e: DbError| DbError::RetryBudgetExhausted {
        operation,
        attempts: max + 1,
        budget: max + 1,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        inner: Box::new(e),
    };
    for attempt in 0..=max {
        match op().await {
            Outcome::Err(e) if is_mvcc_error(&e) && attempt < max => {
                MVCC_RETRIES_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    attempt,
                    max_retries = max,
                    error = %e,
                    operation,
                    "MVCC write conflict, retrying whole transaction"
                );
                mvcc_backoff(attempt);
            }
            Outcome::Err(e) if is_plain_write_contention_error(&e) && attempt < max => {
                tracing::warn!(
                    attempt,
                    max_retries = max,
                    error = %e,
                    operation,
                    "SQLite write contention, retrying whole transaction"
                );
                mvcc_backoff(attempt);
            }
            Outcome::Err(e) if is_mvcc_error(&e) => {
                MVCC_EXHAUSTED_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    attempts = max + 1,
                    error = %e,
                    operation,
                    "MVCC retries exhausted"
                );
                return Outcome::Err(exhausted(e));
            }
            Outcome::Err(e) if is_plain_write_contention_error(&e) => {
                tracing::error!(
                    attempts = max + 1,
                    error = %e,
                    operation,
                    "SQLite write-contention retries exhausted"
                );
                return Outcome::Err(exhausted(e));
            }
            other => {
                // K3: a hard, edit-blocking corruption surfaced on the write
                // path — trip the breaker so subsequent writes are refused
                // until the database is verified healthy again.
                if let Outcome::Err(ref e) = other {
                    crate::corruption_circuit_breaker().observe_error(e);
                }
                return other;
            }
        }
    }

    Outcome::Err(DbError::Internal(format!(
        "MVCC retry loop fell through in {operation}"
    )))
}

/// Sleep with exponential backoff for MVCC retry.
///
/// Base: 25 ms, max: 3000 ms, ±25 % jitter (via existing LCG in `retry` module).
///
/// Budget summary (before jitter):
///   - attempts 0..8 : 25+50+100+200+400+800+1600+2000 ≈ 5.2s
///   - attempts 8..16: 3000 × 8 ≈ 24s (max-delay ceiling reached)
///
/// Cumulative cap across 16 retries ≈ 29s, which stays comfortably under
/// the 60s SQLite `busy_timeout` while giving enough runway for 6+
/// concurrent writers with ~4s each to all settle (see #98). The 3s
/// ceiling (raised from 2s) lets later retries outlast the tail of WAL
/// checkpoint stalls that occasionally stretch to 1-2s on busy disks.
fn mvcc_backoff(attempt: u32) {
    use crate::retry::RetryConfig;
    let config = RetryConfig {
        base_delay: std::time::Duration::from_millis(25),
        max_delay: std::time::Duration::from_secs(3),
        use_circuit_breaker: false,
        ..Default::default()
    };
    std::thread::sleep(config.delay_for_attempt(attempt));
}

/// Snapshot of MVCC retry metrics for health/diagnostics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MvccRetryMetrics {
    pub max_retries: u32,
    pub retries_total: u64,
    pub exhausted_total: u64,
}

/// Get current MVCC retry metrics.
#[must_use]
pub fn mvcc_retry_metrics() -> MvccRetryMetrics {
    use std::sync::atomic::Ordering;
    MvccRetryMetrics {
        max_retries: *MVCC_MAX_RETRIES,
        retries_total: MVCC_RETRIES_TOTAL.load(Ordering::Relaxed),
        exhausted_total: MVCC_EXHAUSTED_TOTAL.load(Ordering::Relaxed),
    }
}

/// Ensure a project exists, creating if necessary.
///
/// Returns the project row (existing or newly created).
/// Uses the in-memory cache to avoid DB round-trips on repeated calls.
#[allow(clippy::too_many_lines)]
pub async fn ensure_project(
    cx: &Cx,
    pool: &DbPool,
    human_key: &str,
) -> Outcome<ProjectRow, DbError> {
    // Validate absolute path
    if !Path::new(human_key).is_absolute() {
        return Outcome::Err(DbError::invalid(
            "human_key",
            "Must be an absolute path (e.g., /data/projects/backend)",
        ));
    }

    let identity = mcp_agent_mail_core::resolve_project_identity(human_key);
    let slug = identity.slug;
    let resolved_human_key = identity.human_key;
    let cache_scope = cache_scope_for_pool(pool);

    // Fast path: check cache first
    if let Some(cached) = crate::cache::read_cache().get_project_scoped(&cache_scope, &slug) {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Match legacy semantics: slug is the stable identity; `human_key` is informative.
    let select_sql = "SELECT id, slug, human_key, created_at FROM projects WHERE slug = ? LIMIT 1";
    let select_params = [Value::Text(slug.clone())];

    match map_sql_outcome(traw_query(cx, &tracked, select_sql, &select_params).await) {
        Outcome::Ok(rows) => {
            if let Some(r) = rows.first() {
                match decode_project_row(r) {
                    Ok(row) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        return Outcome::Ok(row);
                    }
                    Err(e) => return Outcome::Err(e),
                }
            }
        }
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    match find_project_in_inventory(cx, &tracked, |project| {
        project_matches_human_key_alias(project, human_key, &resolved_human_key)
    })
    .await
    {
        Outcome::Ok(Some(row)) => {
            crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
            return Outcome::Ok(row);
        }
        Outcome::Ok(None) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // The row doesn't exist yet — we're about to INSERT. This is the only
    // path where the ephemeral-fixture guard should fire. Lookups of
    // already-existing rows (from the SELECT above or the cache) pass
    // through unchanged so operators can still interact with pre-existing
    // data, even if it accumulated before this guard landed.
    //
    // Refuse to register project paths whose path segments match known
    // test-fixture naming patterns (tempfile.mkdtemp, t.TempDir(), .tmpXXXXXX).
    // These historically leaked into the real user archive whenever tests
    // bypassed the ephemeral-storage reroute (e.g., by calling this function
    // directly with a real DB pool). Set AM_ALLOW_EPHEMERAL_PROJECT_ROOTS=1
    // to opt out (real integration tests that need to register such paths).
    let raw_is_ephemeral = mcp_agent_mail_core::ephemeral::is_ephemeral_project_path(human_key);
    let resolved_is_ephemeral =
        mcp_agent_mail_core::ephemeral::is_ephemeral_project_path(&resolved_human_key);
    if (raw_is_ephemeral || resolved_is_ephemeral)
        && !mcp_agent_mail_core::ephemeral::ephemeral_project_roots_allowed()
    {
        return Outcome::Err(DbError::invalid(
            "human_key",
            format!(
                "Refusing to register project path `{human_key}` — one of its segments matches \
                 a test-fixture naming pattern (tempfile.mkdtemp / Go TestXxx / .tmpXXXXXX). \
                 This guard prevents test tempdirs from polluting the real mailbox archive. \
                 Set AM_ALLOW_EPHEMERAL_PROJECT_ROOTS=1 if this registration is intentional."
            ),
        ));
    }

    // Use an explicit write transaction and conflict-safe insert so project creation
    // participates in concurrent writer mode.
    let fresh = match run_with_mvcc_retry(cx, "ensure_project", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let row = ProjectRow::new(slug.clone(), resolved_human_key.clone());
        let insert_sql = "INSERT INTO projects (slug, human_key, created_at) \
                          VALUES (?, ?, ?) ON CONFLICT(slug) DO NOTHING";
        let insert_params = [
            Value::Text(row.slug.clone()),
            Value::Text(row.human_key.clone()),
            Value::BigInt(row.created_at),
        ];
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
        );

        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, select_sql, &select_params).await)
        );
        let Some(found) = rows.first() else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::Internal(format!(
                "project insert/upsert succeeded but re-select failed for slug={slug}"
            )));
        };
        let fresh = match decode_project_row(found) {
            Ok(row) => row,
            Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
        };

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(fresh)
    })
    .await
    {
        Outcome::Ok(fresh) => fresh,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    crate::cache::read_cache().put_project_scoped(&cache_scope, &fresh);
    Outcome::Ok(fresh)
}

/// Get project by slug (cache-first)
pub async fn get_project_by_slug(
    cx: &Cx,
    pool: &DbPool,
    slug: &str,
) -> Outcome<ProjectRow, DbError> {
    let cache_scope = cache_scope_for_pool(pool);
    if let Some(cached) = crate::cache::read_cache().get_project_scoped(&cache_scope, slug) {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT id, slug, human_key, created_at FROM projects WHERE slug = ? LIMIT 1";
    let params = [Value::Text(slug.to_string())];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            if let Some(row) = rows.first() {
                match decode_project_row(row) {
                    Ok(row) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        Outcome::Ok(row)
                    }
                    Err(e) => Outcome::Err(e),
                }
            } else {
                match find_project_in_inventory(cx, &tracked, |project| {
                    project.slug.eq_ignore_ascii_case(slug)
                })
                .await
                {
                    Outcome::Ok(Some(row)) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        Outcome::Ok(row)
                    }
                    Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", slug)),
                    Outcome::Err(e) => Outcome::Err(e),
                    Outcome::Cancelled(r) => Outcome::Cancelled(r),
                    Outcome::Panicked(p) => Outcome::Panicked(p),
                }
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get project by `human_key` (cache-first)
pub async fn get_project_by_human_key(
    cx: &Cx,
    pool: &DbPool,
    human_key: &str,
) -> Outcome<ProjectRow, DbError> {
    let cache_scope = cache_scope_for_pool(pool);
    if let Some(cached) =
        crate::cache::read_cache().get_project_by_human_key_scoped(&cache_scope, human_key)
    {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT id, slug, human_key, created_at FROM projects WHERE human_key = ? LIMIT 1";
    let params = [Value::Text(human_key.to_string())];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            if let Some(row) = rows.first() {
                match decode_project_row(row) {
                    Ok(row) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        Outcome::Ok(row)
                    }
                    Err(e) => Outcome::Err(e),
                }
            } else {
                match find_project_in_inventory(cx, &tracked, |project| {
                    project_matches_human_key_alias(project, human_key, human_key)
                })
                .await
                {
                    Outcome::Ok(Some(row)) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        Outcome::Ok(row)
                    }
                    Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", human_key)),
                    Outcome::Err(e) => Outcome::Err(e),
                    Outcome::Cancelled(r) => Outcome::Cancelled(r),
                    Outcome::Panicked(p) => Outcome::Panicked(p),
                }
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Look up a project by its primary key.
pub async fn get_project_by_id(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<ProjectRow, DbError> {
    let cache_scope = cache_scope_for_pool(pool);
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT id, slug, human_key, created_at FROM projects WHERE id = ? LIMIT 1";
    let params = [Value::BigInt(project_id)];
    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            if let Some(row) = rows.first() {
                match decode_project_row(row) {
                    Ok(row) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        Outcome::Ok(row)
                    }
                    Err(e) => Outcome::Err(e),
                }
            } else {
                match find_project_in_inventory(cx, &tracked, |project| {
                    project.id == Some(project_id)
                })
                .await
                {
                    Outcome::Ok(Some(row)) => {
                        crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                        Outcome::Ok(row)
                    }
                    Outcome::Ok(None) => {
                        Outcome::Err(DbError::not_found("Project", project_id.to_string()))
                    }
                    Outcome::Err(e) => Outcome::Err(e),
                    Outcome::Cancelled(r) => Outcome::Cancelled(r),
                    Outcome::Panicked(p) => Outcome::Panicked(p),
                }
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

async fn find_project_in_inventory<F>(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
    mut predicate: F,
) -> Outcome<Option<ProjectRow>, DbError>
where
    F: FnMut(&ProjectRow) -> bool,
{
    match list_projects_with_tracked(cx, tracked).await {
        Outcome::Ok(projects) => {
            Outcome::Ok(projects.into_iter().find(|project| predicate(project)))
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

async fn list_projects_with_tracked(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
) -> Outcome<Vec<ProjectRow>, DbError> {
    let now_us = now_micros();
    match map_sql_outcome(traw_query(cx, tracked, PROJECT_SELECT_ALL_SQL, &[]).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                match decode_project_row(r) {
                    Ok(row) => out.push(row),
                    Err(e) => return Outcome::Err(e),
                }
            }

            let mut orphan_project_ids = std::collections::BTreeSet::new();
            let orphan_sources: Vec<(String, Vec<Value>)> = vec![
                (
                    "SELECT DISTINCT m.project_id AS project_id \
                     FROM messages m \
                     LEFT JOIN projects p ON p.id = m.project_id \
                     WHERE p.id IS NULL"
                        .to_string(),
                    vec![],
                ),
                (
                    "SELECT DISTINCT a.project_id AS project_id \
                     FROM agents a \
                     LEFT JOIN projects p ON p.id = a.project_id \
                     WHERE p.id IS NULL"
                        .to_string(),
                    vec![],
                ),
                (
                    format!(
                        "SELECT DISTINCT file_reservations.project_id AS project_id \
                         FROM file_reservations \
                         LEFT JOIN projects p ON p.id = file_reservations.project_id \
                         WHERE p.id IS NULL \
                           AND ({ACTIVE_RESERVATION_PREDICATE}) \
                           AND file_reservations.expires_ts > ?"
                    ),
                    vec![Value::BigInt(now_us)],
                ),
                (
                    "SELECT DISTINCT ppl.project_id AS project_id \
                     FROM product_project_links ppl \
                     LEFT JOIN projects p ON p.id = ppl.project_id \
                     WHERE p.id IS NULL"
                        .to_string(),
                    vec![],
                ),
            ];
            for (sql, params) in &orphan_sources {
                let orphan_rows = match map_sql_outcome(traw_query(cx, tracked, sql, params).await)
                {
                    Outcome::Ok(rows) => rows,
                    Outcome::Err(e) => return Outcome::Err(e),
                    Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                    Outcome::Panicked(p) => return Outcome::Panicked(p),
                };
                for row in &orphan_rows {
                    if let Some(project_id) = row.get(0).and_then(value_as_i64) {
                        orphan_project_ids.insert(project_id);
                    }
                }
            }

            let orphan_created_at_sql = format!(
                "SELECT COALESCE( \
                     (SELECT MIN(m.created_ts) FROM messages m WHERE m.project_id = ?), \
                     (SELECT MIN(a.inception_ts) FROM agents a WHERE a.project_id = ?), \
                     (SELECT MIN(fr.created_ts) FROM file_reservations fr \
                      WHERE fr.project_id = ? \
                        AND ({}) \
                        AND fr.expires_ts > ?), \
                     (SELECT MIN(ppl.created_at) FROM product_project_links ppl WHERE ppl.project_id = ?), \
                     0 \
                 ) AS created_at",
                active_reservation_predicate_for("fr")
            );

            for project_id in orphan_project_ids {
                let created_params = [
                    Value::BigInt(project_id),
                    Value::BigInt(project_id),
                    Value::BigInt(project_id),
                    Value::BigInt(now_us),
                    Value::BigInt(project_id),
                ];
                let created_at_rows = match map_sql_outcome(
                    traw_query(cx, tracked, &orphan_created_at_sql, &created_params).await,
                ) {
                    Outcome::Ok(rows) => rows,
                    Outcome::Err(e) => return Outcome::Err(e),
                    Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                    Outcome::Panicked(p) => return Outcome::Panicked(p),
                };
                let created_at = created_at_rows
                    .first()
                    .and_then(|row| row.get(0).and_then(value_as_i64))
                    .unwrap_or(0);
                out.push(orphaned_project_placeholder(project_id, created_at));
            }
            out.sort_by_key(|project| (project.created_at, project.id.unwrap_or_default()));
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// List all projects
pub async fn list_projects(cx: &Cx, pool: &DbPool) -> Outcome<Vec<ProjectRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    list_projects_with_tracked(cx, &tracked(&*conn)).await
}

// =============================================================================
// Agent Queries
// =============================================================================

/// Register or update an agent
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn register_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
    program: &str,
    model: &str,
    task_description: Option<&str>,
    attachments_policy: Option<&str>,
    reaper_exempt: Option<bool>,
) -> Outcome<AgentRow, DbError> {
    // Validate agent name
    if !mcp_agent_mail_core::models::is_valid_agent_name(name) {
        return Outcome::Err(DbError::invalid(
            "name",
            format!("Invalid agent name '{name}'. Must be adjective+noun format"),
        ));
    }
    let now = now_micros();
    let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                     inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                     registration_token \
                     FROM agents \
                     WHERE project_id = ? AND name = ? COLLATE NOCASE \
                     ORDER BY id ASC LIMIT 1";
    let fetch_params = [Value::BigInt(project_id), Value::Text(name.to_string())];
    let existing_before = match durability_probe_query(cx, pool, fetch_sql, &fetch_params).await {
        Outcome::Ok(rows) => rows.first().map(decode_agent_row_indexed),
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let (provisional, durable) = {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let (provisional, inserted_new) = {
            let tracked = tracked(&*conn);
            match run_with_mvcc_retry(cx, "register_agent", || async {
                try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

                let program_s = program.to_string();
                let model_s = model.to_string();
                let name_s = name.to_string();
                let insert_task_desc = task_description.unwrap_or_default().to_string();
                let attach_pol = attachments_policy.map_or_else(
                    || "auto".to_string(),
                    std::string::ToString::to_string,
                );
                let insert_sql = "INSERT INTO agents \
                    (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt) \
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                    ON CONFLICT(project_id, name) DO NOTHING";
                let insert_params = [
                    Value::BigInt(project_id),
                    Value::Text(name_s.clone()),
                    Value::Text(program_s.clone()),
                    Value::Text(model_s.clone()),
                    Value::Text(insert_task_desc.clone()),
                    Value::BigInt(now),
                    Value::BigInt(now),
                    Value::Text(attach_pol.clone()),
                    Value::Text("auto".to_string()),
                    Value::BigInt(i64::from(reaper_exempt.unwrap_or(false))),
                ];
                let mut inserted_new = false;
                let mut inserted_id = None;
                if existing_before.is_none() {
                    let inserted_rows = try_in_tx!(
                        cx,
                        &tracked,
                        map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
                    );
                    inserted_new = inserted_rows > 0;
                    if inserted_new {
                        let id_rows = try_in_tx!(
                            cx,
                            &tracked,
                            map_sql_outcome(
                                traw_query(cx, &tracked, "SELECT last_insert_rowid() AS id", &[]).await
                            )
                        );
                        let Some(id) = id_rows.first().and_then(row_first_i64) else {
                            rollback_tx(cx, &tracked).await;
                            return Outcome::Err(DbError::Internal(format!(
                                "agent insert succeeded but last_insert_rowid() returned no row for {project_id}:{name}"
                            )));
                        };
                        inserted_id = Some(id);
                    }
                }

                if !inserted_new {
                    // Keep behavior consistent with insert path: omitted task_description clears
                    // to empty string instead of preserving stale content.
                    let mut normalize_sets = vec!["program = ?", "model = ?", "last_active_ts = ?"];
                    let mut normalize_params = vec![
                        Value::Text(program_s.clone()),
                        Value::Text(model_s.clone()),
                        Value::BigInt(now),
                        Value::Text(insert_task_desc.clone()),
                    ];
                    normalize_sets.push("task_description = ?");
                    if let Some(ap) = attachments_policy {
                        normalize_sets.push("attachments_policy = ?");
                        normalize_params.push(Value::Text(ap.to_string()));
                    }
                    if let Some(exempt) = reaper_exempt {
                        normalize_sets.push("reaper_exempt = ?");
                        normalize_params.push(Value::BigInt(i64::from(exempt)));
                    }
                    let normalize_sql = format!(
                        "UPDATE agents SET {} WHERE project_id = ? AND name = ? COLLATE NOCASE",
                        normalize_sets.join(", ")
                    );
                    normalize_params.push(Value::BigInt(project_id));
                    normalize_params.push(Value::Text(name_s.clone()));
                    let updated_rows = try_in_tx!(
                        cx,
                        &tracked,
                        map_sql_outcome(
                            traw_execute(cx, &tracked, &normalize_sql, &normalize_params).await
                        )
                    );
                    if updated_rows == 0 {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(DbError::Internal(format!(
                            "agent upsert affected zero rows for {project_id}:{name}"
                        )));
                    }
                }

                try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);

                let build_inserted_agent = || AgentRow {
                    id: inserted_id,
                    project_id,
                    name: name_s.clone(),
                    program: program_s.clone(),
                    model: model_s.clone(),
                    task_description: insert_task_desc.clone(),
                    inception_ts: now,
                    last_active_ts: now,
                    attachments_policy: attach_pol.clone(),
                    contact_policy: "auto".to_string(),
                    reaper_exempt: i64::from(reaper_exempt.unwrap_or(false)),
                    registration_token: None,
                };
                let mut provisional = if inserted_new {
                    build_inserted_agent()
                } else if let Some(existing) = existing_before.clone() {
                    existing
                } else {
                    build_inserted_agent()
                };
                provisional.program = program_s;
                provisional.model = model_s;
                provisional.task_description = insert_task_desc;
                provisional.last_active_ts = now;
                if let Some(ap) = attachments_policy {
                    provisional.attachments_policy = ap.to_string();
                }
                if let Some(exempt) = reaper_exempt {
                    provisional.reaper_exempt = i64::from(exempt);
                }

                Outcome::Ok((provisional, inserted_new))
            })
            .await
            {
                Outcome::Ok(result) => result,
                Outcome::Err(e) => return Outcome::Err(e),
                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                Outcome::Panicked(p) => return Outcome::Panicked(p),
            }
        };
        drop(conn);
        let durable = match finalize_register_agent_post_commit_probe(
            cx,
            pool,
            project_id,
            name,
            &provisional,
            inserted_new,
            verify_agent_visible_after_commit(cx, pool, project_id, name).await,
        )
        .await
        {
            Outcome::Ok(agent) => agent,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        (provisional, durable)
    };

    let final_agent = durable.unwrap_or_else(|| provisional.clone());
    if final_agent.id != provisional.id {
        tracing::warn!(
            project_id,
            agent = %name,
            provisional_id = ?provisional.id,
            durable_id = ?final_agent.id,
            "agent id changed between commit and durability check"
        );
    }

    crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &final_agent);
    Outcome::Ok(final_agent)
}

/// Update an agent's registration token for sender identity verification.
///
/// Sets the `registration_token` column to the provided token value.
/// Called after agent registration to persist the newly generated token.
pub async fn update_agent_registration_token(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    token: &str,
) -> Outcome<(), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);
    let sql = "UPDATE agents SET registration_token = ? WHERE id = ?";
    let params = [Value::Text(token.to_string()), Value::BigInt(agent_id)];
    match map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await) {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Create a new agent identity, failing if the name is already taken.
///
/// Unlike `register_agent` (which does an upsert), this function enforces
/// strict uniqueness and returns `DbError::Duplicate` when the identity exists.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn create_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
    program: &str,
    model: &str,
    task_description: Option<&str>,
    attachments_policy: Option<&str>,
) -> Outcome<AgentRow, DbError> {
    // Validate agent name
    if !mcp_agent_mail_core::models::is_valid_agent_name(name) {
        return Outcome::Err(DbError::invalid(
            "name",
            format!("Invalid agent name '{name}'. Must be adjective+noun format"),
        ));
    }
    let now = now_micros();
    let (provisional, durable) = {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let provisional = {
            let tracked = tracked(&*conn);
            try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

            let task_desc = task_description.unwrap_or_default();
            let attach_pol = attachments_policy.unwrap_or("auto");
            // GH#169: resolve the canonical (first-registered / lowest-id) row
            // deterministically so the duplicate check matches what get_agent
            // resolves; see the get_agent note for the full rationale.
            let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                             inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                             registration_token \
                             FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
                             ORDER BY id ASC LIMIT 1";
            let fetch_params = [Value::BigInt(project_id), Value::Text(name.to_string())];

            // Fast duplicate check before insert.
            let existing_rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, fetch_sql, &fetch_params).await)
            );
            if !existing_rows.is_empty() {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::duplicate(
                    "agent",
                    format!("{name} (project {project_id})"),
                ));
            }

            let insert_sql = "INSERT INTO agents \
                (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
            let insert_params = [
                Value::BigInt(project_id),
                Value::Text(name.to_string()),
                Value::Text(program.to_string()),
                Value::Text(model.to_string()),
                Value::Text(task_desc.to_string()),
                Value::BigInt(now),
                Value::BigInt(now),
                Value::Text(attach_pol.to_string()),
                Value::Text("auto".to_string()),
                Value::BigInt(0),
            ];
            match map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await) {
                Outcome::Ok(_) => {}
                Outcome::Err(e) => {
                    let is_unique_violation = match &e {
                        DbError::Sqlite(msg) => {
                            let msg = msg.to_ascii_lowercase();
                            msg.contains("unique constraint failed")
                                && (msg.contains("agents.project_id")
                                    || msg.contains("agents.name"))
                        }
                        _ => false,
                    };

                    rollback_tx(cx, &tracked).await;
                    if is_unique_violation {
                        return Outcome::Err(DbError::duplicate(
                            "agent",
                            format!("{name} (project {project_id})"),
                        ));
                    }
                    return Outcome::Err(e);
                }
                Outcome::Cancelled(r) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Cancelled(r);
                }
                Outcome::Panicked(p) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Panicked(p);
                }
            }

            // Read back the inserted row so callers never see a synthetic id=0.
            let rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, fetch_sql, &fetch_params).await)
            );
            let Some(found) = rows.first().map(decode_agent_row_indexed) else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(format!(
                    "agent insert succeeded but re-select failed for {project_id}:{name}"
                )));
            };
            try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
            found
        };
        drop(conn);
        let probe_detail = format!("{project_id}:{name}");
        let provisional_id = provisional.id.unwrap_or(0);
        let durable = match verify_agent_visible_after_commit(cx, pool, project_id, name).await {
            Outcome::Ok(agent) => Some(agent),
            Outcome::Err(e) => {
                return Outcome::Err(
                    cleanup_created_agent_after_post_commit_probe_failure(
                        cx,
                        pool,
                        project_id,
                        provisional_id,
                        name,
                        e,
                    )
                    .await,
                );
            }
            Outcome::Cancelled(_) => {
                return Outcome::Err(
                    cleanup_created_agent_after_post_commit_probe_failure(
                        cx,
                        pool,
                        project_id,
                        provisional_id,
                        name,
                        post_commit_probe_cancelled_error("create_agent", &probe_detail),
                    )
                    .await,
                );
            }
            Outcome::Panicked(p) => {
                return Outcome::Err(
                    cleanup_created_agent_after_post_commit_probe_failure(
                        cx,
                        pool,
                        project_id,
                        provisional_id,
                        name,
                        post_commit_probe_panicked_error(
                            "create_agent",
                            &probe_detail,
                            p.message(),
                        ),
                    )
                    .await,
                );
            }
        };
        (provisional, durable)
    };

    let final_agent = durable.unwrap_or_else(|| provisional.clone());
    if final_agent.id != provisional.id {
        tracing::warn!(
            project_id,
            agent = %name,
            provisional_id = ?provisional.id,
            durable_id = ?final_agent.id,
            "agent id changed between commit and durability check"
        );
    }

    crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &final_agent);
    Outcome::Ok(final_agent)
}

/// Get agent by project and name (cache-first)
pub async fn get_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
) -> Outcome<AgentRow, DbError> {
    if let Some(cached) =
        crate::cache::read_cache().get_agent_scoped(&cache_scope_for_pool(pool), project_id, name)
    {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Optimized: filter by name directly in SQL (case-insensitive).
    //
    // GH#169: a case-insensitive lookup against a BINARY-unique `agents` table
    // can match several case-variant rows (e.g. `BlueLake` and `bluelake`,
    // created by a concurrent multi-pane registration race). `LIMIT 1` with no
    // `ORDER BY` picks a row non-deterministically, so the same `agent_name`
    // could resolve to a different `agent_id` over time — leaking file
    // reservations (release resolves to a different id than grant did). Pin a
    // stable canonical row: the first-registered (lowest `id`) variant. Grant
    // and release then always resolve to the same row.
    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
               registration_token \
               FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
               ORDER BY id ASC LIMIT 1";
    let params = [Value::BigInt(project_id), Value::Text(name.to_string())];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Agent", format!("{project_id}:{name}"))),
            |row| {
                let agent = decode_agent_row_indexed(row);
                crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &agent);
                Outcome::Ok(agent)
            },
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get agent by id (cache-first).
pub async fn get_agent_by_id(cx: &Cx, pool: &DbPool, agent_id: i64) -> Outcome<AgentRow, DbError> {
    if let Some(cached) =
        crate::cache::read_cache().get_agent_by_id_scoped(&cache_scope_for_pool(pool), agent_id)
    {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Use raw SQL with explicit column order to avoid ORM decoding issues
    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
               registration_token \
               FROM agents WHERE id = ? LIMIT 1";
    let params = [Value::BigInt(agent_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Agent", agent_id.to_string())),
            |row| {
                let agent = decode_agent_row_indexed(row);
                crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &agent);
                Outcome::Ok(agent)
            },
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Fetch a single agent by ID, bypassing the read cache.
///
/// Cleanup and integrity paths use this when they need authoritative current
/// database state rather than possibly stale cached rows.
pub async fn get_agent_by_id_fresh(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
) -> Outcome<AgentRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
               registration_token \
               FROM agents WHERE id = ? LIMIT 1";
    let params = [Value::BigInt(agent_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Agent", agent_id.to_string())),
            |row| Outcome::Ok(decode_agent_row_indexed(row)),
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// List agents for a project
pub async fn list_agents(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<Vec<AgentRow>, DbError> {
    list_agents_bounded(cx, pool, project_id, None, None).await
}

/// List a project's agents with an optional activity floor and result cap
/// (GH#154 item 3).
///
/// `min_last_active_ts`: when `Some(floor)`, only agents whose `last_active_ts`
/// is `>= floor` are returned (used to exclude agents idle past a retention
/// horizon). `None` returns agents regardless of idle time.
///
/// `limit`: when `Some(n)`, at most `n` rows are returned *after* the
/// case-insensitive name de-duplication, keeping the most-recently-active
/// agents (the result is ordered `last_active_ts DESC, id DESC`). This bounds
/// the tool response on long-lived projects that accumulate agents across many
/// short-lived swarms (one reported project reached 1,119 agents / ~199 KB,
/// enough to blow the calling agent's context window). `None` is unbounded
/// (preserves the historical [`list_agents`] contract).
///
/// The activity filter and cap are applied in Rust (not SQL) so this keeps the
/// "simple ordered scan + Rust-side de-dup" shape that deliberately avoids a
/// FrankenSQLite window-function dependency during mailbox recovery.
pub async fn list_agents_bounded(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    min_last_active_ts: Option<i64>,
    limit: Option<usize>,
) -> Outcome<Vec<AgentRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Use raw SQL with explicit column order to avoid ORM decoding issues.
    // Keep this as a simple ordered scan: startup/TUI paths call this often,
    // and case-insensitive de-duplication is cheap in Rust while avoiding a
    // FrankenSQLite window-function dependency during mailbox recovery.
    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
               registration_token \
               FROM agents \
               WHERE project_id = ? \
               ORDER BY last_active_ts DESC, id DESC";
    let params = [Value::BigInt(project_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut agents: Vec<AgentRow> = rows.iter().map(decode_agent_row_indexed).collect();
            agents.sort_by(|left, right| {
                right
                    .last_active_ts
                    .cmp(&left.last_active_ts)
                    .then_with(|| {
                        right
                            .id
                            .unwrap_or_default()
                            .cmp(&left.id.unwrap_or_default())
                    })
            });

            let mut seen_names = HashSet::new();
            agents.retain(|agent| seen_names.insert(agent.name.to_ascii_lowercase()));

            if let Some(floor) = min_last_active_ts {
                agents.retain(|agent| agent.last_active_ts >= floor);
            }
            if let Some(cap) = limit {
                agents.truncate(cap);
            }
            Outcome::Ok(agents)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get agents by ids (cache-first).
pub async fn get_agents_by_ids(
    cx: &Cx,
    pool: &DbPool,
    agent_ids: &[i64],
) -> Outcome<Vec<AgentRow>, DbError> {
    if agent_ids.is_empty() {
        return Outcome::Ok(vec![]);
    }

    // Try to serve from cache first
    let mut out = Vec::with_capacity(agent_ids.len());
    let mut missing_ids = Vec::with_capacity(agent_ids.len());

    let cache = crate::cache::read_cache();
    let cache_scope = cache_scope_for_pool(pool);
    for id in agent_ids {
        if let Some(cached) = cache.get_agent_by_id_scoped(&cache_scope, *id) {
            out.push(cached);
        } else {
            missing_ids.push(*id);
        }
    }

    if missing_ids.is_empty() {
        return Outcome::Ok(out);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    for chunk in missing_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT id, project_id, name, program, model, task_description, \
             inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
             registration_token \
             FROM agents WHERE id IN ({placeholders})"
        );

        let mut params: Vec<Value> = Vec::with_capacity(chunk.len());
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
                for row in rows {
                    let agent = decode_agent_row_indexed(&row);
                    crate::cache::read_cache().put_agent_scoped(&cache_scope, &agent);
                    out.push(agent);
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }
    Outcome::Ok(out)
}

/// Touch agent (deferred).
///
/// Enqueues a `last_active_ts` update into the in-memory batch queue.
/// The actual DB write happens when the flush interval elapses or when
/// `flush_deferred_touches` is called explicitly. This eliminates a DB
/// round-trip on every single tool invocation.
pub async fn touch_agent(cx: &Cx, pool: &DbPool, agent_id: i64) -> Outcome<(), DbError> {
    let now = now_micros();
    let cache_scope = cache_scope_for_pool(pool);
    let should_flush = crate::cache::read_cache().enqueue_touch_scoped(&cache_scope, agent_id, now);

    if should_flush {
        flush_deferred_touches(cx, pool).await
    } else {
        Outcome::Ok(())
    }
}

/// Immediately flush all pending deferred touch updates to the DB.
/// Call this on server shutdown or when precise `last_active_ts` is needed.
pub async fn flush_deferred_touches(cx: &Cx, pool: &DbPool) -> Outcome<(), DbError> {
    let read_cache = crate::cache::read_cache();
    let cache_scope = cache_scope_for_pool(pool);
    if !read_cache.has_pending_touches() {
        return Outcome::Ok(());
    }
    let pending = read_cache.drain_touches_scoped(&cache_scope);
    if pending.is_empty() {
        return Outcome::Ok(());
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => {
            re_enqueue_touches(&cache_scope, &pending);
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => {
            re_enqueue_touches(&cache_scope, &pending);
            return Outcome::Cancelled(r);
        }
        Outcome::Panicked(p) => {
            re_enqueue_touches(&cache_scope, &pending);
            return Outcome::Panicked(p);
        }
    };

    let tracked = tracked(&*conn);

    let flush_outcome = run_with_mvcc_retry(cx, "flush_deferred_touches", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Batch UPDATE using VALUES CTE without UPDATE ... FROM so it remains
        // compatible with FrankenSQLite's VDBE codegen path.
        // SQLite parameter limit is 999; 2 params per row → max 499 per chunk.
        let entries: Vec<_> = pending.iter().collect();

        for chunk in entries.chunks(400) {
            let placeholders = std::iter::repeat_n("(?,?)", chunk.len()).collect::<Vec<_>>();
            let sql = format!(
                "WITH batch(agent_id, new_ts) AS (VALUES {}) \
                 UPDATE agents \
                 SET last_active_ts = MAX(last_active_ts, ( \
                     SELECT b.new_ts FROM batch b WHERE b.agent_id = agents.id \
                 )) \
                 WHERE id IN (SELECT agent_id FROM batch)",
                placeholders.join(",")
            );
            let mut params = Vec::with_capacity(chunk.len() * 2);
            for &(&agent_id, &ts) in chunk {
                params.push(Value::BigInt(agent_id));
                params.push(Value::BigInt(ts));
            }

            match map_sql_outcome(traw_execute(cx, &tracked, &sql, &params).await) {
                Outcome::Ok(_) => {}
                Outcome::Err(e) => {
                    let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                    return Outcome::Err(e);
                }
                Outcome::Cancelled(r) => {
                    let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                    return Outcome::Cancelled(r);
                }
                Outcome::Panicked(p) => {
                    let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                    return Outcome::Panicked(p);
                }
            }
        }

        match commit_tx(cx, &tracked).await {
            Outcome::Ok(()) => Outcome::Ok(()),
            Outcome::Err(e) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                Outcome::Err(e)
            }
            Outcome::Cancelled(r) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                Outcome::Cancelled(r)
            }
            Outcome::Panicked(p) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                Outcome::Panicked(p)
            }
        }
    })
    .await;

    match flush_outcome {
        Outcome::Ok(()) => Outcome::Ok(()),
        Outcome::Err(e) => {
            re_enqueue_touches(&cache_scope, &pending);
            Outcome::Err(e)
        }
        Outcome::Cancelled(r) => {
            re_enqueue_touches(&cache_scope, &pending);
            Outcome::Cancelled(r)
        }
        Outcome::Panicked(p) => {
            re_enqueue_touches(&cache_scope, &pending);
            Outcome::Panicked(p)
        }
    }
}

/// Re-enqueue touches that failed to flush, so they aren't lost.
fn re_enqueue_touches(scope: &str, pending: &std::collections::HashMap<i64, i64>) {
    let cache = crate::cache::read_cache();
    for (&agent_id, &ts) in pending {
        cache.enqueue_touch_scoped(scope, agent_id, ts);
    }
}

/// Outcome of attempting to consume a registration-proof nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceOutcome {
    /// The nonce had not been seen before; it is now durably recorded.
    Fresh,
    /// The nonce was already consumed within its validity window (a replay).
    Replayed,
}

/// Durably record a consumed registration-proof nonce, atomically detecting
/// replays.
///
/// `issuer_key` is a stable string id for the trusted public key the proof was
/// signed under (base64 of the 32-byte Ed25519 key). The `(issuer_key, nonce)`
/// pair may be consumed at most once: the composite PRIMARY KEY on
/// `proof_gate_consumed_nonces` makes the INSERT itself the atomic check — a
/// fresh insert (rows-affected 1) is [`NonceOutcome::Fresh`], a conflict
/// (rows-affected 0) is [`NonceOutcome::Replayed`]. Two racing consumes of the
/// same nonce resolve correctly: one commits `Fresh`, the other's commit
/// conflicts, retries, then sees the row and returns `Replayed`. Because the
/// record lives in the shared database the guarantee survives process restarts
/// and holds across processes, unlike an in-memory store.
///
/// Expired rows (`retain_until < now`) are pruned in the same transaction so the
/// table stays bounded without a background sweeper.
///
/// The caller MUST fail closed on any non-`Ok` outcome: if the nonce cannot be
/// durably recorded we cannot prove the proof is not being replayed, so
/// registration must be refused.
pub async fn consume_proof_nonce(
    cx: &Cx,
    pool: &DbPool,
    issuer_key: &str,
    nonce: &str,
    retain_until: i64,
    now: i64,
) -> Outcome<NonceOutcome, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    run_with_mvcc_retry(cx, "consume_proof_nonce", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Prune expired nonces (housekeeping; keeps the table bounded).
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(
                traw_execute(
                    cx,
                    &tracked,
                    "DELETE FROM proof_gate_consumed_nonces WHERE retain_until < ?",
                    &[Value::BigInt(now)],
                )
                .await
            )
        );

        // Atomic replay check: INSERT the nonce; a conflict (already consumed)
        // affects 0 rows, a fresh insert affects 1.
        let affected = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(
                traw_execute(
                    cx,
                    &tracked,
                    "INSERT INTO proof_gate_consumed_nonces \
                     (issuer_key, nonce, retain_until, consumed_at) \
                     VALUES (?, ?, ?, ?) \
                     ON CONFLICT(issuer_key, nonce) DO NOTHING",
                    &[
                        Value::Text(issuer_key.to_string()),
                        Value::Text(nonce.to_string()),
                        Value::BigInt(retain_until),
                        Value::BigInt(now),
                    ],
                )
                .await
            )
        );

        let outcome = if affected >= 1 {
            NonceOutcome::Fresh
        } else {
            NonceOutcome::Replayed
        };

        match commit_tx(cx, &tracked).await {
            Outcome::Ok(()) => Outcome::Ok(outcome),
            Outcome::Err(e) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                Outcome::Err(e)
            }
            Outcome::Cancelled(r) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                Outcome::Cancelled(r)
            }
            Outcome::Panicked(p) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                Outcome::Panicked(p)
            }
        }
    })
    .await
}

/// Update agent's `contact_policy`
pub async fn set_agent_contact_policy(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    policy: &str,
) -> Outcome<AgentRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let agent = match run_with_mvcc_retry(cx, "set_agent_contact_policy", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let now = now_micros();
        let sql = "UPDATE agents SET contact_policy = ?, last_active_ts = ? WHERE id = ?";
        let params = [
            Value::Text(policy.to_string()),
            Value::BigInt(now),
            Value::BigInt(agent_id),
        ];

        let _rows_affected = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
        );

        // Fetch updated agent using raw SQL with explicit column order.
        let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                         inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                         registration_token \
                         FROM agents WHERE id = ? LIMIT 1";
        let fetch_params = [Value::BigInt(agent_id)];
        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, fetch_sql, &fetch_params).await)
        );
        let Some(row) = rows.first() else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::not_found("Agent", agent_id.to_string()));
        };
        let agent = decode_agent_row_indexed(row);
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(agent)
    })
    .await
    {
        Outcome::Ok(agent) => agent,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &agent);
    Outcome::Ok(agent)
}

/// Update agent's `contact_policy` by project and name (avoids ID lookup issues)
pub async fn set_agent_contact_policy_by_name(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
    policy: &str,
) -> Outcome<AgentRow, DbError> {
    let normalized_name = name.trim();
    if normalized_name.is_empty() {
        return Outcome::Err(DbError::invalid(
            "name",
            "agent name cannot be empty".to_string(),
        ));
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let agent = match run_with_mvcc_retry(cx, "set_agent_contact_policy_by_name", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);
        let now = now_micros();

        // Resolve row first so we can preserve attachments_policy explicitly.
        // GH#169: pin the same canonical (lowest-id) row get_agent resolves so a
        // policy set here actually applies to the row reads resolve back to when
        // case-variant duplicates exist.
        let current_sql = "SELECT id, project_id, name, program, model, task_description, \
                           inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                           registration_token \
                           FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
                           ORDER BY id ASC LIMIT 1";
        let current_params = [
            Value::BigInt(project_id),
            Value::Text(normalized_name.to_string()),
        ];
        let current_rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, current_sql, &current_params).await)
        );
        let Some(current_agent) = current_rows.first().map(decode_agent_row_indexed) else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::not_found(
                "Agent",
                format!("{project_id}:{normalized_name}"),
            ));
        };
        let Some(current_id) = current_agent.id else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::Internal(format!(
                "policy update lookup returned agent without id for {project_id}:{normalized_name}"
            )));
        };

        let sql = "UPDATE agents SET contact_policy = ?, last_active_ts = ? WHERE id = ?";
        let params = [
            Value::Text(policy.to_string()),
            Value::BigInt(now),
            Value::BigInt(current_id),
        ];

        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
        );

        let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                         inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                         registration_token \
                         FROM agents WHERE id = ? LIMIT 1";
        let fetch_params = [Value::BigInt(current_id)];
        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, fetch_sql, &fetch_params).await)
        );
        let Some(agent) = rows.first().map(decode_agent_row_indexed) else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::Internal(format!(
                "policy update succeeded but re-select failed for {project_id}:{normalized_name}"
            )));
        };
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(agent)
    })
    .await
    {
        Outcome::Ok(agent) => agent,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &agent);
    Outcome::Ok(agent)
}

// =============================================================================
// Message Queries
// =============================================================================

/// Thread message details (for `summarize_thread` / resources).
#[derive(Debug, Clone)]
pub struct ThreadMessageRow {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub body_md: String,
    pub importance: String,
    pub ack_required: i64,
    pub created_ts: i64,
    pub recipients: String,
    pub attachments: String,
    pub from: String,
}

/// Recipient details for a single message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecipientDetailRow {
    pub name: String,
    pub kind: String,
}

/// Atomically check for conflicts and create reservations.
///
/// Executes the read-check-write cycle within a `BEGIN IMMEDIATE` transaction
/// to prevent TOCTOU races where two agents reserve the same file simultaneously.
///
/// 1. Begins IMMEDIATE transaction (serializing reservations).
/// 2. Fetches active reservations for the project.
/// 3. Invokes `checker` with the list of active reservations.
/// 4. If `checker` returns `Ok(inserts)`, performs batch INSERT and commits.
/// 5. If `checker` returns `Err(msg)`, rolls back and returns `DbError::Conflict`.
#[allow(clippy::too_many_lines)]
pub async fn atomic_file_reservation_check_and_create<F>(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    checker: F,
) -> Outcome<Vec<FileReservationRow>, DbError>
where
    F: FnOnce(
            &[FileReservationRow],
        ) -> std::result::Result<Vec<(i64, String, i64, bool, String)>, String>
        + Send,
{
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Use IMMEDIATE transaction to serialize reservation checks.
    // This prevents other writers from starting, effectively locking for this operation.
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    // Fetch active reservations within the transaction snapshot.
    // We duplicate the logic of `get_active_reservations` here to use the transaction.
    let sql = format!(
        "{FILE_RESERVATION_SELECT_COLUMNS_SQL} WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE})"
    );
    let params = [Value::BigInt(project_id)];
    let rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await)
    );

    let mut active = Vec::with_capacity(rows.len());
    for r in &rows {
        match decode_file_reservation_row(r) {
            Ok(row) => active.push(row),
            Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
        }
    }

    // Invoke the caller-provided conflict checker logic.
    let inserts = match checker(&active) {
        Ok(i) => i,
        Err(msg) => {
            rollback_tx(cx, &tracked).await;
            // Map the conflict message to a generic error or specific type?
            // Since this is a check failure, we return it as a conflict/logic error.
            // Using DbError::Internal or custom wrapper?
            // DbError doesn't have a generic "LogicError" variant, but Sqlite variant works.
            return Outcome::Err(DbError::Sqlite(format!("Reservation conflict: {msg}")));
        }
    };

    if inserts.is_empty() {
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        return Outcome::Ok(Vec::new());
    }

    let now = now_micros();
    let mut created_rows = Vec::with_capacity(inserts.len());

    // Batch insert
    for chunk in inserts.chunks(50) {
        let mut query = String::from(
            "INSERT INTO file_reservations \
             (project_id, agent_id, path_pattern, created_ts, expires_ts, \"exclusive\", reason) \
             VALUES ",
        );
        let mut params = Vec::with_capacity(chunk.len() * 7);

        for (i, (agent_id, path, ttl, exclusive, reason)) in chunk.iter().enumerate() {
            if i > 0 {
                query.push_str(", ");
            }
            query.push_str("(?, ?, ?, ?, ?, ?, ?)");
            let expires = now.saturating_add(ttl.saturating_mul(1_000_000));
            params.push(Value::BigInt(project_id));
            params.push(Value::BigInt(*agent_id));
            params.push(Value::Text(path.clone()));
            params.push(Value::BigInt(now));
            params.push(Value::BigInt(expires));
            params.push(Value::Int(i32::from(*exclusive)));
            params.push(Value::Text(reason.clone()));

            created_rows.push(FileReservationRow {
                id: None,
                project_id,
                agent_id: *agent_id,
                path_pattern: path.clone(),
                exclusive: i64::from(*exclusive),
                reason: reason.clone(),
                created_ts: now,
                expires_ts: expires,
                released_ts: None,
            });
        }

        // Insert without RETURNING (frankensqlite compatibility).
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, &query, &params).await)
        );

        // Retrieve the inserted IDs via a deterministic query keyed on the
        // exact (project_id, created_ts, path_pattern) tuples we just inserted.
        // Within this IMMEDIATE transaction no concurrent inserts can interleave.
        let start_idx = created_rows.len() - chunk.len();
        for (j, (agent_id, path, _, _, _)) in chunk.iter().enumerate() {
            let id_sql = "SELECT id FROM file_reservations \
                          WHERE project_id = ? AND agent_id = ? AND path_pattern = ? AND created_ts = ? \
                          ORDER BY id DESC LIMIT 1";
            let id_params = [
                Value::BigInt(project_id),
                Value::BigInt(*agent_id),
                Value::Text(path.clone()),
                Value::BigInt(now),
            ];
            let id_rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, id_sql, &id_params).await)
            );
            let Some(id) = id_rows.first().and_then(row_first_i64) else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(
                    "file reservation insert: could not retrieve inserted row id".to_string(),
                ));
            };
            if let Some(cr) = created_rows.get_mut(start_idx + j) {
                cr.id = Some(id);
            }
        }
    }

    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    Outcome::Ok(created_rows)
}

/// Create a new message
#[allow(clippy::too_many_arguments)]
pub async fn create_message(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body_md: &str,
    thread_id: Option<&str>,
    importance: &str,
    ack_required: bool,
    attachments: &str,
) -> Outcome<MessageRow, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // mcp_agent_mail#176: allocate the canonical id from the process-wide
    // monotonic allocator (see `create_message_with_recipients` for the full
    // rationale) and insert it explicitly, so it can never be re-issued even
    // when the live SQLite's durable AUTOINCREMENT fails to advance.
    let id_allocator = pool.message_id_allocator();
    let archive_seed = if id_allocator.needs_archive_seed() {
        id_allocator.mark_archive_seeded();
        crate::id_floor::max_message_id_in_archive(pool.storage_root()).unwrap_or(0)
    } else {
        0
    };
    let db_floor = match read_messages_id_floor(cx, &tracked).await {
        Outcome::Ok(floor) => floor,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let message_id = id_allocator.allocate(db_floor, archive_seed);

    let row = match run_with_mvcc_retry(cx, "create_message", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Insert message with an explicit id (mcp_agent_mail#176).
        let sql = "INSERT INTO messages \
	               (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
	               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let params = [
            Value::BigInt(message_id),
            Value::BigInt(project_id),
            Value::BigInt(sender_id),
            thread_id.map_or_else(|| Value::Null, |t| Value::Text(t.to_string())),
            Value::Text(subject.to_string()),
            Value::Text(body_md.to_string()),
            Value::Text(importance.to_string()),
            Value::BigInt(i64::from(ack_required)),
            Value::BigInt(now),
            Value::Text(attachments.to_string()),
        ];

        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
        );

        let row = MessageRow {
            id: Some(message_id),
            project_id,
            sender_id,
            thread_id: thread_id.map(String::from),
            subject: subject.to_string(),
            body_md: body_md.to_string(),
            importance: importance.to_string(),
            ack_required: i64::from(ack_required),
            created_ts: now,
            recipients_json: "{}".to_string(),
            attachments: attachments.to_string(),
        };

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(row)
    })
    .await
    {
        Outcome::Ok(row) => row,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    if let Err(error) = index_created_message_best_effort(&conn, &row) {
        tracing::warn!(
            message_id = row.id.unwrap_or_default(),
            error = %error,
            "message committed but incremental search indexing failed"
        );
    }
    Outcome::Ok(row)
}

fn index_created_message_best_effort(
    conn: &crate::DbConn,
    row: &MessageRow,
) -> std::result::Result<bool, String> {
    let Some(message_id) = row.id else {
        return Ok(false);
    };
    let project_slug = conn
        .query_sync(
            "SELECT slug FROM projects WHERE id = ? LIMIT 1",
            &[Value::BigInt(row.project_id)],
        )
        .ok()
        .and_then(|rows| rows.first().and_then(|row| row.get_as::<String>(0).ok()))
        .unwrap_or_default();
    let sender_name = conn
        .query_sync(
            "SELECT name FROM agents WHERE id = ? LIMIT 1",
            &[Value::BigInt(row.sender_id)],
        )
        .ok()
        .and_then(|rows| rows.first().and_then(|row| row.get_as::<String>(0).ok()))
        .unwrap_or_else(|| UNKNOWN_SENDER_DISPLAY.to_string());

    let message = crate::search_v3::IndexableMessage {
        id: message_id,
        project_id: row.project_id,
        project_slug,
        sender_name,
        subject: row.subject.clone(),
        body_md: row.body_md.clone(),
        thread_id: row.thread_id.clone(),
        importance: row.importance.clone(),
        created_ts: row.created_ts,
    };
    crate::search_v3::index_message(&message)
}

/// Read the messages-table allocator floor: the larger of `MAX(id)` and the
/// `sqlite_sequence` row for `messages`.
///
/// Used to seed/advance the process-wide [`MessageIdAllocator`](crate::id_floor::MessageIdAllocator)
/// (mcp_agent_mail#176). A missing `sqlite_sequence` row (or table, on a
/// brand-new database) is treated as `0` so it can never block message
/// creation.
async fn read_messages_id_floor(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<i64, DbError> {
    let db_max = match map_sql_outcome(
        traw_query(
            cx,
            tracked,
            "SELECT COALESCE(MAX(id), 0) AS v FROM messages",
            &[],
        )
        .await,
    ) {
        Outcome::Ok(rows) => rows.first().and_then(row_first_i64).unwrap_or(0),
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let seq_val = match map_sql_outcome(
        traw_query(
            cx,
            tracked,
            "SELECT COALESCE(seq, 0) AS v FROM sqlite_sequence WHERE name = 'messages'",
            &[],
        )
        .await,
    ) {
        Outcome::Ok(rows) => rows.first().and_then(row_first_i64).unwrap_or(0),
        // sqlite_sequence may be absent on a fresh DB; not an error here.
        Outcome::Err(_) | Outcome::Cancelled(_) | Outcome::Panicked(_) => 0,
    };
    Outcome::Ok(db_max.max(seq_val))
}

/// Create a message AND insert all recipients in a single `SQLite` transaction.
///
/// This eliminates N+2 separate auto-commit writes (1 message INSERT + N
/// recipient INSERTs) into a single transaction with 1 fsync.
///
/// On MVCC write conflicts (`BEGIN CONCURRENT` page collision), the entire
/// transaction is retried up to `FSQLITE_CONCURRENT_RETRIES` times (default 5)
/// with exponential backoff (10–200 ms).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn create_message_with_recipients(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body_md: &str,
    thread_id: Option<&str>,
    importance: &str,
    ack_required: bool,
    attachments: &str,
    recipients: &[(i64, &str)], // (agent_id, kind)
) -> Outcome<MessageRow, DbError> {
    let _serializer_guard = match MESSAGE_WRITE_SERIALIZER.lock(cx).await {
        Ok(guard) => guard,
        Err(asupersync::sync::LockError::Cancelled) => {
            return Outcome::Cancelled(CancelReason::user(
                "create_message_with_recipients serializer lock cancelled",
            ));
        }
        Err(error) => {
            return Outcome::Err(DbError::Internal(format!(
                "create_message_with_recipients serializer lock failed: {error}"
            )));
        }
    };
    // De-duplicate resolved recipient ids before any insert. The
    // `message_recipients` primary key is `(message_id, agent_id)` — `kind` is
    // NOT part of it — so the same agent appearing twice in `recipients` (e.g.
    // once as `to` and once as `cc`, or a name that resolves to an already-listed
    // id) would otherwise fail the second insert with a UNIQUE-constraint error
    // and surface a *false* `isError` even though the message and every distinct
    // recipient were persisted, prompting clients to retry and create duplicate
    // messages (see #243 Bug 2). First occurrence wins, preserving order and the
    // most-prominent kind.
    let deduped_recipients: Vec<(i64, &str)> = {
        let mut seen = std::collections::HashSet::with_capacity(recipients.len());
        let mut out = Vec::with_capacity(recipients.len());
        for &(agent_id, kind) in recipients {
            if seen.insert(agent_id) {
                out.push((agent_id, kind));
            }
        }
        out
    };
    let recipients = deduped_recipients.as_slice();
    let now = now_micros();
    let (row, writer_post_commit_counts) = {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let tracked = tracked(&*conn);

        // mcp_agent_mail#176: allocate the canonical message id from the
        // process-wide monotonic allocator rather than relying on the live
        // SQLite's AUTOINCREMENT. While the database is held suspect
        // (canonical-fallback mode, the #151 NOCASE family), the durable
        // allocator can fail to advance per-write and re-issue an id the
        // archive already considers canonical — the duplicate-canonical-file
        // reject (#130) then trips a non-clearable durability latch. The
        // allocator derives the next id as
        // `max(in_memory_high_water, db_floor, archive_max) + 1` atomically,
        // so consecutive creations can never collide regardless of which
        // surface is authoritative. We compute it once here (under the global
        // MESSAGE_WRITE_SERIALIZER) so MVCC retries of the transaction reuse a
        // stable id.
        let id_allocator = pool.message_id_allocator();
        let archive_seed = if id_allocator.needs_archive_seed() {
            id_allocator.mark_archive_seeded();
            crate::id_floor::max_message_id_in_archive(pool.storage_root()).unwrap_or(0)
        } else {
            0
        };
        let db_floor = match read_messages_id_floor(cx, &tracked).await {
            Outcome::Ok(floor) => floor,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        let message_id = id_allocator.allocate(db_floor, archive_seed);

        let row = match run_with_mvcc_retry(cx, "create_message_with_recipients", || {
            create_message_with_recipients_tx(
                cx,
                &tracked,
                project_id,
                sender_id,
                subject,
                body_md,
                thread_id,
                importance,
                ack_required,
                attachments,
                recipients,
                now,
                message_id,
            )
        })
        .await
        {
            Outcome::Ok(created) => {
                let Some(_message_id) = created.id else {
                    return Outcome::Err(DbError::Internal(
                        "message commit succeeded but returned row has no id".to_string(),
                    ));
                };
                created
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let writer_post_commit_counts = if let Some(message_id) = row.id {
            let message_rows = match map_sql_outcome(
                traw_query(
                    cx,
                    &tracked,
                    "SELECT COUNT(*) FROM messages WHERE id = ? AND project_id = ?",
                    &[Value::BigInt(message_id), Value::BigInt(project_id)],
                )
                .await,
            ) {
                Outcome::Ok(rows) => rows,
                Outcome::Err(_) | Outcome::Cancelled(_) | Outcome::Panicked(_) => Vec::new(),
            };
            let recipient_rows = match map_sql_outcome(
                traw_query(
                    cx,
                    &tracked,
                    "SELECT COUNT(*) FROM message_recipients WHERE message_id = ?",
                    &[Value::BigInt(message_id)],
                )
                .await,
            ) {
                Outcome::Ok(rows) => rows,
                Outcome::Err(_) | Outcome::Cancelled(_) | Outcome::Panicked(_) => Vec::new(),
            };
            WriterPostCommitCounts {
                message_count: message_rows.first().and_then(row_first_i64),
                recipient_count: recipient_rows.first().and_then(row_first_i64),
            }
        } else {
            WriterPostCommitCounts::default()
        };

        drop(conn);
        (row, writer_post_commit_counts)
    };

    let Some(message_id) = row.id else {
        return Outcome::Err(DbError::Internal(
            "message commit succeeded but returned row has no id".to_string(),
        ));
    };
    let recipient_agent_ids: Vec<i64> = recipients
        .iter()
        .map(|(agent_id, _kind)| *agent_id)
        .collect();
    let post_commit_probe_error = match verify_message_recipients_visible_after_commit(
        cx, pool, project_id, message_id, recipients,
    )
    .await
    {
        Outcome::Ok(()) => None,
        Outcome::Err(e) => Some(e),
        Outcome::Cancelled(_) => Some(post_commit_probe_cancelled_error(
            "create_message_with_recipients",
            &format!("{project_id}:{message_id}"),
        )),
        Outcome::Panicked(p) => Some(post_commit_probe_panicked_error(
            "create_message_with_recipients",
            &format!("{project_id}:{message_id}"),
            p.message(),
        )),
    };
    if let Some(error) = post_commit_probe_error {
        // GH#179: NEVER delete an already-committed message over a *probe*
        // failure. The write transaction (guarded by `run_with_mvcc_retry`,
        // ~20s budget) already committed; this fresh-handle probe only re-proves
        // visibility. Destructive cleanup is justified ONLY when the probe
        // positively observed a genuine inconsistency (a "ghost success" — the
        // message/recipient rows absent, per `is_hard_post_commit_probe_error`)
        // AND the writer's own post-commit sample did not independently confirm
        // the message row landed. Transient busy/locked/MVCC/pool/IO failures —
        // and cancel/panic — are advisory: the committed row stays durable and
        // we return success, avoiding both the data loss and the compensating
        // DELETE transaction that amplifies contention under swarm load.
        //
        // The writer's own sample can only refute the ghost class it actually
        // contradicts: a present message row refutes a message-row ghost, but a
        // recipient-rows ghost is refuted only when the writer also saw the full
        // expected recipient-row count (otherwise "success" would deliver the
        // message to nobody).
        let expected_recipients =
            i64::try_from(normalize_expected_recipients(recipients).len()).unwrap_or(i64::MAX);
        let writer_confirms_durable = writer_sample_refutes_probe_verdict(
            &error,
            writer_post_commit_counts,
            expected_recipients,
        );
        if post_commit_probe_requires_cleanup(&error, writer_confirms_durable) {
            let error = annotate_message_visibility_error_with_writer_counts(
                error,
                writer_post_commit_counts,
            );
            return Outcome::Err(
                cleanup_message_after_post_commit_probe_failure(
                    cx,
                    pool,
                    project_id,
                    message_id,
                    &recipient_agent_ids,
                    error,
                )
                .await,
            );
        }
        log_advisory_post_commit_probe_error(
            "create_message_with_recipients",
            &format!("{project_id}:{message_id}"),
            &error.to_string(),
        );
    }

    // Invalidate cached inbox stats for all recipients.
    let cache = crate::cache::read_cache();
    let cache_scope = cache_scope_for_pool(pool);
    for agent_id in &recipient_agent_ids {
        cache.invalidate_inbox_stats_scoped(&cache_scope, *agent_id);
    }
    Outcome::Ok(row)
}

/// Inner transaction body for [`create_message_with_recipients`].
///
/// Runs BEGIN CONCURRENT → INSERT message → INSERT recipients → COMMIT.
/// On any failure the `try_in_tx!` macro rolls back before returning.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn create_message_with_recipients_tx(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
    project_id: i64,
    sender_id: i64,
    subject: &str,
    body_md: &str,
    thread_id: Option<&str>,
    importance: &str,
    ack_required: bool,
    attachments: &str,
    recipients: &[(i64, &str)],
    now: i64,
    message_id: i64,
) -> Outcome<MessageRow, DbError> {
    // Use MVCC concurrent transaction for page-level parallelism.
    try_in_tx!(cx, tracked, begin_concurrent_tx(cx, tracked).await);

    // Fetch recipient names to build recipients_json
    let mut to_names = Vec::new();
    let mut cc_names = Vec::new();
    let mut bcc_names = Vec::new();

    if !recipients.is_empty() {
        let mut name_map = std::collections::HashMap::new();
        for chunk in recipients.chunks(MAX_IN_CLAUSE_ITEMS) {
            let ph = placeholders(chunk.len());
            let lookup_sql = format!("SELECT id, name FROM agents WHERE id IN ({ph})");
            let params: Vec<Value> = chunk.iter().map(|(id, _)| Value::BigInt(*id)).collect();
            let agent_rows = try_in_tx!(
                cx,
                tracked,
                map_sql_outcome(traw_query(cx, tracked, &lookup_sql, &params).await)
            );
            for r in agent_rows {
                if let (Ok(id), Ok(name)) = (r.get_as::<i64>(0), r.get_as::<String>(1)) {
                    name_map.insert(id, name);
                }
            }
        }

        for (id, kind) in recipients {
            if let Some(name) = name_map.get(id) {
                match *kind {
                    "cc" => cc_names.push(name.clone()),
                    "bcc" => bcc_names.push(name.clone()),
                    _ => to_names.push(name.clone()),
                }
            }
        }
    }

    let recipients_json_val = serde_json::json!({
        "to": to_names,
        "cc": cc_names,
        "bcc": bcc_names,
    })
    .to_string();

    // Insert the message with an EXPLICIT id (mcp_agent_mail#176). We do not
    // rely on AUTOINCREMENT + a deterministic read-back here: the id was
    // allocated by the process-wide monotonic allocator in the caller, so it
    // is guaranteed unique-and-increasing even when the live SQLite's durable
    // allocator state fails to advance. Inserting an explicit rowid keeps the
    // DB row id and the canonical archive filename in lockstep regardless of
    // engine state. (Inserting an explicit id > the current sequence also
    // advances `sqlite_sequence`, keeping any non-explicit path consistent.)
    let sql = "INSERT INTO messages \
               (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    let params = [
        Value::BigInt(message_id),
        Value::BigInt(project_id),
        Value::BigInt(sender_id),
        thread_id.map_or_else(|| Value::Null, |t| Value::Text(t.to_string())),
        Value::Text(subject.to_string()),
        Value::Text(body_md.to_string()),
        Value::Text(importance.to_string()),
        Value::BigInt(i64::from(ack_required)),
        Value::BigInt(now),
        Value::Text(recipients_json_val.clone()),
        Value::Text(attachments.to_string()),
    ];

    try_in_tx!(
        cx,
        tracked,
        map_sql_outcome(traw_execute(cx, tracked, sql, &params).await)
    );

    let row = MessageRow {
        id: Some(message_id),
        project_id,
        sender_id,
        thread_id: thread_id.map(String::from),
        subject: subject.to_string(),
        body_md: body_md.to_string(),
        importance: importance.to_string(),
        ack_required: i64::from(ack_required),
        created_ts: now,
        recipients_json: recipients_json_val,
        attachments: attachments.to_string(),
    };

    // Insert recipients one row at a time inside the same transaction.
    // This avoids a known multi-row INSERT + trigger path that can surface
    // spurious PRIMARY KEY conflicts in the franken sqlite engine.
    //
    // `ON CONFLICT(message_id, agent_id) DO NOTHING` makes a re-driven insert
    // idempotent (see #243 Bug 2): under busy/MVCC retry a partial re-drive of
    // the recipient loop must not fail with a UNIQUE-constraint error that gets
    // surfaced as a false `isError` after the message + all distinct recipients
    // were already persisted. The caller de-duplicates recipient ids before this
    // runs, so a genuinely-distinct recipient is never silently dropped; the
    // post-commit visibility probe still verifies every distinct recipient is
    // present, so a swallowed engine quirk cannot hide a missing row.
    let insert_recipient_sql = "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (?, ?, ?, NULL, NULL) ON CONFLICT(message_id, agent_id) DO NOTHING";
    for (agent_id, kind) in recipients {
        let params = [
            Value::BigInt(message_id),
            Value::BigInt(*agent_id),
            Value::Text((*kind).to_string()),
        ];
        match map_sql_outcome(traw_execute(cx, tracked, insert_recipient_sql, &params).await) {
            Outcome::Ok(_) => {}
            Outcome::Err(error) => {
                let existing_rows = match map_sql_outcome(
                    traw_query(
                        cx,
                        tracked,
                        "SELECT agent_id, kind FROM message_recipients WHERE message_id = ? ORDER BY agent_id, kind",
                        &[Value::BigInt(message_id)],
                    )
                    .await,
                ) {
                    Outcome::Ok(rows) => rows,
                    Outcome::Err(_) | Outcome::Cancelled(_) | Outcome::Panicked(_) => Vec::new(),
                };
                rollback_tx(cx, tracked).await;
                return Outcome::Err(DbError::Internal(format!(
                    "message recipient insert failed for message_id={message_id} agent_id={agent_id} kind={kind}: {error}; existing_rows_after_failure={:?}; subject={subject:?} thread_id={thread_id:?}",
                    decode_message_recipient_pairs(message_id, &existing_rows)
                )));
            }
            Outcome::Cancelled(reason) => {
                rollback_tx(cx, tracked).await;
                return Outcome::Cancelled(reason);
            }
            Outcome::Panicked(payload) => {
                rollback_tx(cx, tracked).await;
                return Outcome::Panicked(payload);
            }
        }
    }

    let recipient_agent_ids: Vec<i64> = recipients.iter().map(|(id, _)| *id).collect();
    try_in_tx!(
        cx,
        tracked,
        rebuild_agents_inbox_stats_in_tx(cx, tracked, &recipient_agent_ids).await
    );

    // COMMIT (single fsync)
    try_in_tx!(cx, tracked, commit_tx(cx, tracked).await);

    Outcome::Ok(row)
}

/// Fetch detailed message information for a batch of message IDs.
///
/// Used for hydrating search results (e.g. from vector search) where
/// the index does not store full content.
/// Fetch full message details for a list of message IDs.
///
/// `project_id` is an optional project boundary filter.  When `Some`,
/// only messages belonging to that project are returned — preventing
/// cross-project data leakage.  When `None` (e.g., internal index
/// rebuilds), all matching messages are returned regardless of project.
#[allow(clippy::too_many_lines)]
pub async fn get_messages_details_by_ids(
    cx: &Cx,
    pool: &DbPool,
    message_ids: &[i64],
    project_id: Option<i64>,
) -> Outcome<Vec<ThreadMessageRow>, DbError> {
    if message_ids.is_empty() {
        return Outcome::Ok(Vec::new());
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut out = Vec::with_capacity(message_ids.len());

    for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let project_clause = if project_id.is_some() {
            " AND m.project_id = ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                    m.importance, m.ack_required, m.created_ts, m.recipients_json, \
                    m.attachments, COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') as from_name \
             FROM messages m \
             LEFT JOIN agents a ON a.id = m.sender_id \
             WHERE m.id IN ({placeholders}){project_clause}"
        );

        let mut params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();
        if let Some(pid) = project_id {
            params.push(Value::BigInt(pid));
        }

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
                for row in rows {
                    let get_i64 = |idx: usize| -> std::result::Result<i64, DbError> {
                        match row.get_as(idx) {
                            Ok(v) => Ok(v),
                            Err(e) => {
                                if let Some(v) = row.get(idx) {
                                    value_as_i64(v).ok_or_else(|| map_sql_error(&e))
                                } else {
                                    Err(map_sql_error(&e))
                                }
                            }
                        }
                    };

                    let id: i64 = match get_i64(0) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(e),
                    };
                    let project_id: i64 = match get_i64(1) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(e),
                    };
                    let sender_id: i64 = match get_i64(2) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(e),
                    };
                    let thread_id: Option<String> = match row.get_as(3) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let subject: String = match row.get_as(4) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let body_md: String = match row.get_as(5) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let importance: String = match row.get_as(6) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let ack_required: i64 = match get_i64(7) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(e),
                    };
                    let created_ts: i64 = match get_i64(8) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(e),
                    };
                    let recipients: String = match row.get_as::<Option<String>>(9) {
                        Ok(v) => v.unwrap_or_else(|| "{}".to_string()),
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let attachments: String = match row.get_as::<Option<String>>(10) {
                        Ok(v) => v.unwrap_or_else(|| "[]".to_string()),
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let from: String = match row.get_as::<Option<String>>(11) {
                        Ok(v) => v.unwrap_or_default(),
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(ThreadMessageRow {
                        id,
                        project_id,
                        sender_id,
                        thread_id,
                        subject,
                        body_md,
                        importance,
                        ack_required,
                        created_ts,
                        recipients,
                        attachments,
                        from,
                    });
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }
    Outcome::Ok(out)
}

/// List messages for a thread.
///
/// Thread semantics:
/// - If `thread_id` is a numeric string, it is treated as a root message id.
///   The thread includes the root message (`id = root`) and any replies (`thread_id = "{root}"`).
/// - Otherwise, the thread includes messages where `thread_id = thread_id`.
/// - If `limit` is set, the most recent `limit` messages are selected and returned in
///   chronological order (oldest-to-newest within that limited window).
#[allow(clippy::too_many_lines)]
pub async fn list_thread_messages(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    thread_id: &str,
    limit: Option<usize>,
) -> Outcome<Vec<ThreadMessageRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut params: Vec<Value> = vec![Value::BigInt(project_id)];

    let is_root = thread_id.parse::<i64>().is_ok();
    if let Ok(root_id) = thread_id.parse::<i64>() {
        params.push(Value::BigInt(root_id));
    }
    params.push(Value::Text(thread_id.to_string()));

    let (sql, reverse_to_chronological) = match (is_root, limit) {
        (true, Some(lim)) => {
            let Ok(limit_i64) = i64::try_from(lim) else {
                return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
            };
            params.push(Value::BigInt(limit_i64));
            (
                format!(
                    "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                            m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                            m.importance AS importance, m.ack_required AS ack_required, \
                            m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                            m.attachments AS attachments, \
                            COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') AS from_name \
                     FROM messages m \
                     LEFT JOIN agents a ON a.id = m.sender_id \
                     WHERE m.project_id = ? AND (m.id = ? OR m.thread_id = ?) \
                     ORDER BY created_ts DESC, id DESC \
                     LIMIT ?"
                ),
                true,
            )
        }
        (true, None) => (
            format!(
                "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                        m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                        m.importance AS importance, m.ack_required AS ack_required, \
                        m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                        m.attachments AS attachments, \
                        COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') AS from_name \
                 FROM messages m \
                 LEFT JOIN agents a ON a.id = m.sender_id \
                 WHERE m.project_id = ? AND (m.id = ? OR m.thread_id = ?) \
                 ORDER BY created_ts ASC, id ASC"
            ),
            false,
        ),
        (false, Some(lim)) => {
            let Ok(limit_i64) = i64::try_from(lim) else {
                return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
            };
            params.push(Value::BigInt(limit_i64));
            (
                format!(
                    "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                            m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                            m.importance AS importance, m.ack_required AS ack_required, \
                            m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                            m.attachments AS attachments, \
                            COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') AS from_name \
                     FROM messages m \
                     LEFT JOIN agents a ON a.id = m.sender_id \
                     WHERE m.project_id = ? AND m.thread_id = ? \
                     ORDER BY created_ts DESC, id DESC \
                     LIMIT ?"
                ),
                true,
            )
        }
        (false, None) => (
            format!(
                "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                        m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                        m.importance AS importance, m.ack_required AS ack_required, \
                        m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                        m.attachments AS attachments, \
                        COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') AS from_name \
                 FROM messages m \
                 LEFT JOIN agents a ON a.id = m.sender_id \
                 WHERE m.project_id = ? AND m.thread_id = ? \
                 ORDER BY created_ts ASC, id ASC"
            ),
            false,
        ),
    };

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_as(0) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let project_id: i64 = match row.get_as(1) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_id: i64 = match row.get_as(2) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let thread_id: Option<String> = match row.get_as(3) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let subject: String = match row.get_as(4) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let body_md: String = match row.get_as(5) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let importance: String = match row.get_as(6) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let ack_required: i64 = match row.get_as(7) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let created_ts: i64 = match row.get_as(8) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let recipients: String = match row.get_as(9) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let attachments: String = match row.get_as(10) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let from: String = match row.get_as(11) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                out.push(ThreadMessageRow {
                    id,
                    project_id,
                    sender_id,
                    thread_id,
                    subject,
                    body_md,
                    importance,
                    ack_required,
                    created_ts,
                    recipients,
                    attachments,
                    from,
                });
            }
            if reverse_to_chronological {
                out.reverse();
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

pub async fn list_numeric_thread_roots_with_replies(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    root_message_ids: &[i64],
) -> Outcome<Vec<i64>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut candidate_ids: Vec<i64> = root_message_ids
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect();
    if candidate_ids.is_empty() {
        return Outcome::Ok(Vec::new());
    }
    candidate_ids.sort_unstable();
    candidate_ids.dedup();

    let tracked = tracked(&*conn);
    let mut roots_with_replies = Vec::new();

    for chunk in candidate_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT DISTINCT m.thread_id \
             FROM messages m \
             WHERE m.project_id = ? AND m.thread_id IN ({placeholders})"
        );
        let mut params = Vec::with_capacity(chunk.len() + 1);
        params.push(Value::BigInt(project_id));
        for root_id in chunk {
            params.push(Value::Text(root_id.to_string()));
        }

        let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
        let rows = match rows_out {
            Outcome::Ok(rows) => rows,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        for row in rows {
            let thread_id: String = match row.get_as(0) {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            if let Ok(root_id) = thread_id.parse::<i64>() {
                roots_with_replies.push(root_id);
            }
        }
    }

    roots_with_replies.sort_unstable();
    roots_with_replies.dedup();
    Outcome::Ok(roots_with_replies)
}

/// List unique recipient agent names for a set of message ids.
#[allow(clippy::items_after_statements)]
pub async fn list_message_recipient_names_for_messages(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    message_ids: &[i64],
) -> Outcome<Vec<String>, DbError> {
    if message_ids.is_empty() {
        return Outcome::Ok(vec![]);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut out = Vec::new();

    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let get_sql = |count: usize| -> &'static str {
        let capped = count.min(MAX_IN_CLAUSE_ITEMS);
        let cache = CACHE.get_or_init(|| {
            (0..=MAX_IN_CLAUSE_ITEMS)
                .map(|c| {
                    format!(
                        "SELECT r.agent_id AS raw_agent_id, a.name AS name \
                         FROM message_recipients r \
                         LEFT JOIN agents a ON a.id = r.agent_id \
                         JOIN messages m ON m.id = r.message_id \
                         WHERE m.project_id = ? AND r.message_id IN ({})",
                        placeholders(c)
                    )
                })
                .collect::<Vec<_>>()
        });
        &cache[capped]
    };

    for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let sql = get_sql(chunk.len());

        let mut params: Vec<Value> = Vec::with_capacity(chunk.len() + 1);
        params.push(Value::BigInt(project_id));
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
            Outcome::Ok(rows) => {
                for row in rows {
                    let agent_id: i64 = match row.get_named("raw_agent_id") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let name: Option<String> = match row.get_named("name") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(resolved_agent_display(agent_id, name));
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

    out.sort();
    out.dedup();
    Outcome::Ok(out)
}

/// List recipients for a single message, preserving delivery kind ordering.
pub async fn list_message_recipients_by_message(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    message_id: i64,
) -> Outcome<Vec<MessageRecipientDetailRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let sql = "SELECT r.agent_id AS raw_agent_id, a.name, r.kind \
               FROM message_recipients r \
               LEFT JOIN agents a ON a.id = r.agent_id \
               JOIN messages m ON m.id = r.message_id \
               WHERE m.project_id = ? AND r.message_id = ?";
    let params = [Value::BigInt(project_id), Value::BigInt(message_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let agent_id: i64 = match row.get_as(0) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let name: Option<String> = match row.get_as(1) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let kind: String = match row.get_as(2) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                out.push(MessageRecipientDetailRow {
                    name: resolved_agent_display(agent_id, name),
                    kind,
                });
            }
            out.sort_by(|left, right| {
                let kind_rank = |kind: &str| {
                    if kind.eq_ignore_ascii_case("to") {
                        0
                    } else if kind.eq_ignore_ascii_case("cc") {
                        1
                    } else if kind.eq_ignore_ascii_case("bcc") {
                        2
                    } else {
                        3
                    }
                };
                kind_rank(&left.kind)
                    .cmp(&kind_rank(&right.kind))
                    .then_with(|| {
                        left.name
                            .bytes()
                            .map(|b| b.to_ascii_lowercase())
                            .cmp(right.name.bytes().map(|b| b.to_ascii_lowercase()))
                    })
                    .then_with(|| left.name.cmp(&right.name))
            });
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// List recipient agent names keyed by message id for a set of messages.
pub async fn list_message_recipient_names_by_message(
    cx: &Cx,
    pool: &DbPool,
    message_ids: &[i64],
) -> Outcome<HashMap<i64, Vec<String>>, DbError> {
    if message_ids.is_empty() {
        return Outcome::Ok(HashMap::new());
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();

    for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT r.message_id, r.agent_id AS raw_agent_id, a.name \
             FROM message_recipients r \
             LEFT JOIN agents a ON a.id = r.agent_id \
             WHERE r.message_id IN ({placeholders}) \
             ORDER BY r.message_id ASC, a.name COLLATE NOCASE ASC, r.agent_id ASC"
        );

        let params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
                for row in rows {
                    let message_id: i64 = match row.get_as(0) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let agent_id: i64 = match row.get_as(1) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let name: Option<String> = match row.get_as(2) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.entry(message_id)
                        .or_default()
                        .push(resolved_agent_display(agent_id, name));
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

    for names in out.values_mut() {
        names.sort_by(|left, right| {
            left.bytes()
                .map(|b| b.to_ascii_lowercase())
                .cmp(right.bytes().map(|b| b.to_ascii_lowercase()))
                .then_with(|| left.cmp(right))
        });
        names.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    }

    Outcome::Ok(out)
}

/// Get message by ID
pub async fn get_message(cx: &Cx, pool: &DbPool, message_id: i64) -> Outcome<MessageRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT id, project_id, sender_id, thread_id, subject, body_md, importance, \
                       ack_required, created_ts, recipients_json, attachments \
                FROM messages \
                WHERE id = ? \
                LIMIT 1";
    let params = [Value::BigInt(message_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            let Some(row) = rows.first() else {
                return Outcome::Err(DbError::not_found("Message", message_id.to_string()));
            };

            let id: i64 = match row.get_named("id") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let project_id: i64 = match row.get_named("project_id") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let sender_id: i64 = match row.get_named("sender_id") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let thread_id: Option<String> = match row.get_named("thread_id") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let subject: String = match row.get_named("subject") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let body_md: String = match row.get_named("body_md") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let importance: String = match row.get_named("importance") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let ack_required: i64 = match row.get_named("ack_required") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let created_ts: i64 = match row.get_named("created_ts") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let recipients_json: String = match row.get_named("recipients_json") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };
            let attachments: String = match row.get_named("attachments") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(map_sql_error(&e)),
            };

            Outcome::Ok(MessageRow {
                id: Some(id),
                project_id,
                sender_id,
                thread_id,
                subject,
                body_md,
                importance,
                ack_required,
                created_ts,
                recipients_json,
                attachments,
            })
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Fetch inbox for an agent
#[derive(Debug, Clone)]
pub struct InboxRow {
    pub message: MessageRow,
    pub kind: String,
    pub sender_name: String,
    pub read_ts: Option<i64>,
    pub ack_ts: Option<i64>,
}

#[allow(clippy::too_many_lines)]
pub async fn fetch_inbox(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxQueryOptions {
            urgent_only,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: None,
            body_policy: InboxBodyPolicy::Full,
        },
    )
    .await
}

pub async fn fetch_inbox_metadata(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxQueryOptions {
            urgent_only,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
    .await
}

pub async fn fetch_inbox_unread(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxQueryOptions {
            urgent_only,
            unread_only: true,
            ack_required_only: false,
            ack_overdue_before: None,
            body_policy: InboxBodyPolicy::Full,
        },
    )
    .await
}

pub async fn fetch_inbox_unread_metadata(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxQueryOptions {
            urgent_only,
            unread_only: true,
            ack_required_only: false,
            ack_overdue_before: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
    .await
}

pub async fn fetch_inbox_ack_overdue(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
    ack_overdue_before: i64,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxQueryOptions {
            urgent_only,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: Some(ack_overdue_before),
            body_policy: InboxBodyPolicy::Full,
        },
    )
    .await
}

pub async fn fetch_inbox_ack_overdue_metadata(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
    ack_overdue_before: i64,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        since_ts,
        limit,
        InboxQueryOptions {
            urgent_only,
            unread_only: false,
            ack_required_only: false,
            ack_overdue_before: Some(ack_overdue_before),
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
    .await
}

pub async fn fetch_inbox_ack_required(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        None,
        limit,
        InboxQueryOptions {
            urgent_only: false,
            unread_only: false,
            ack_required_only: true,
            ack_overdue_before: None,
            body_policy: InboxBodyPolicy::Full,
        },
    )
    .await
}

pub async fn fetch_inbox_ack_required_metadata(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_impl(
        cx,
        pool,
        project_id,
        agent_id,
        None,
        limit,
        InboxQueryOptions {
            urgent_only: false,
            unread_only: false,
            ack_required_only: true,
            ack_overdue_before: None,
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
    .await
}

#[derive(Clone, Copy)]
enum InboxBodyPolicy {
    Full,
    MetadataOnly,
}

#[derive(Clone, Copy)]
struct InboxQueryOptions {
    urgent_only: bool,
    unread_only: bool,
    ack_required_only: bool,
    ack_overdue_before: Option<i64>,
    body_policy: InboxBodyPolicy,
}

#[allow(clippy::too_many_lines)]
async fn fetch_inbox_impl(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    since_ts: Option<i64>,
    limit: usize,
    options: InboxQueryOptions,
) -> Outcome<Vec<InboxRow>, DbError> {
    let Ok(_limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Outcome::Err(error),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };

    let result = match (options.body_policy, options.ack_overdue_before) {
        (InboxBodyPolicy::Full, Some(threshold)) => {
            crate::sync::fetch_inbox_ack_overdue_rows_from_conn(
                &conn,
                project_id,
                agent_id,
                options.urgent_only,
                since_ts,
                limit,
                threshold,
            )
        }
        (InboxBodyPolicy::MetadataOnly, Some(threshold)) => {
            crate::sync::fetch_inbox_ack_overdue_metadata_rows_from_conn(
                &conn,
                project_id,
                agent_id,
                options.urgent_only,
                since_ts,
                limit,
                threshold,
            )
        }
        (InboxBodyPolicy::Full, None) => crate::sync::fetch_inbox_rows_from_conn(
            &conn,
            project_id,
            agent_id,
            options.urgent_only,
            options.unread_only,
            options.ack_required_only,
            since_ts,
            limit,
        ),
        (InboxBodyPolicy::MetadataOnly, None) => crate::sync::fetch_inbox_metadata_rows_from_conn(
            &conn,
            project_id,
            agent_id,
            options.urgent_only,
            options.unread_only,
            options.ack_required_only,
            since_ts,
            limit,
        ),
    };

    match result {
        Ok(rows) => Outcome::Ok(rows),
        Err(error) => Outcome::Err(error),
    }
}

fn decode_inbox_row_indexed(row: &SqlRow) -> std::result::Result<InboxRow, DbError> {
    let id: i64 = row.get_as(0).map_err(|e| map_sql_error(&e))?;
    let project_id: i64 = row.get_as(1).map_err(|e| map_sql_error(&e))?;
    let sender_id: i64 = row.get_as(2).map_err(|e| map_sql_error(&e))?;
    let thread_id: Option<String> = row.get_as(3).map_err(|e| map_sql_error(&e))?;
    let subject: String = row.get_as(4).map_err(|e| map_sql_error(&e))?;
    let body_md: String = row.get_as(5).map_err(|e| map_sql_error(&e))?;
    let importance: String = row.get_as(6).map_err(|e| map_sql_error(&e))?;
    let ack_required: i64 = row.get_as(7).map_err(|e| map_sql_error(&e))?;
    let created_ts: i64 = row.get_as(8).map_err(|e| map_sql_error(&e))?;
    let recipients_json: String = row.get_as(9).map_err(|e| map_sql_error(&e))?;
    let attachments: String = row.get_as(10).map_err(|e| map_sql_error(&e))?;
    let kind: String = row.get_as(11).map_err(|e| map_sql_error(&e))?;
    let sender_name: String = row.get_as(12).map_err(|e| map_sql_error(&e))?;
    let read_ts: Option<i64> = row.get_as(13).map_err(|e| map_sql_error(&e))?;
    let ack_ts: Option<i64> = row.get_as(14).map_err(|e| map_sql_error(&e))?;

    Ok(InboxRow {
        message: MessageRow {
            id: Some(id),
            project_id,
            sender_id,
            thread_id,
            subject,
            body_md,
            importance,
            ack_required,
            created_ts,
            recipients_json,
            attachments,
        },
        kind,
        sender_name,
        read_ts,
        ack_ts,
    })
}

#[derive(Clone, Copy)]
struct ProductInboxQueryOptions {
    urgent_only: bool,
    body_policy: InboxBodyPolicy,
}

pub async fn fetch_inbox_for_product_agent(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
    agent_name: &str,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_for_product_agent_impl(
        cx,
        pool,
        product_id,
        agent_name,
        since_ts,
        limit,
        ProductInboxQueryOptions {
            urgent_only,
            body_policy: InboxBodyPolicy::Full,
        },
    )
    .await
}

pub async fn fetch_inbox_for_product_agent_metadata(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
    agent_name: &str,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<InboxRow>, DbError> {
    fetch_inbox_for_product_agent_impl(
        cx,
        pool,
        product_id,
        agent_name,
        since_ts,
        limit,
        ProductInboxQueryOptions {
            urgent_only,
            body_policy: InboxBodyPolicy::MetadataOnly,
        },
    )
    .await
}

async fn fetch_inbox_for_product_agent_impl(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
    agent_name: &str,
    since_ts: Option<i64>,
    limit: usize,
    options: ProductInboxQueryOptions,
) -> Outcome<Vec<InboxRow>, DbError> {
    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(conn) => conn,
        Outcome::Err(error) => return Outcome::Err(error),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => return Outcome::Panicked(payload),
    };
    let tracked = tracked(&*conn);

    let body_select = match options.body_policy {
        InboxBodyPolicy::Full => "m.body_md",
        InboxBodyPolicy::MetadataOnly => "'' AS body_md",
    };
    let mut sql = format!(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, {body_select}, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                r.kind, COALESCE(s.name, ?) AS sender_name, r.read_ts, r.ack_ts \
         FROM product_project_links ppl \
         JOIN agents recipient ON recipient.project_id = ppl.project_id AND recipient.name = ? COLLATE NOCASE \
         JOIN message_recipients r ON r.agent_id = recipient.id \
         JOIN messages m ON m.id = r.message_id AND m.project_id = ppl.project_id \
         LEFT JOIN agents s ON s.id = m.sender_id \
         WHERE ppl.product_id = ?"
    );

    let mut params = vec![
        Value::Text(UNKNOWN_SENDER_DISPLAY.to_string()),
        Value::Text(agent_name.to_string()),
        Value::BigInt(product_id),
    ];
    if options.urgent_only {
        sql.push_str(" AND m.importance IN ('high', 'urgent')");
    }
    if let Some(ts) = since_ts {
        sql.push_str(" AND m.created_ts > ?");
        params.push(Value::BigInt(ts));
    }
    sql.push_str(" ORDER BY m.created_ts DESC, m.id DESC LIMIT ?");
    params.push(Value::BigInt(limit_i64));

    match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                match decode_inbox_row_indexed(&row) {
                    Ok(decoded) => out.push(decoded),
                    Err(error) => return Outcome::Err(error),
                }
            }
            Outcome::Ok(out)
        }
        Outcome::Err(error) => Outcome::Err(error),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

/// Search messages using FTS5
#[derive(Debug, Clone)]
pub struct SearchRow {
    pub id: i64,
    pub sender_id: i64,
    pub subject: String,
    pub importance: String,
    pub ack_required: i64,
    pub created_ts: i64,
    pub thread_id: Option<String>,
    pub from: String,
    pub body_md: String,
}

pub const UNKNOWN_SENDER_DISPLAY: &str = "[unknown sender]";

fn unknown_agent_display(agent_id: i64) -> String {
    format!("[unknown-agent-{agent_id}]")
}

fn resolved_agent_display(agent_id: i64, name: Option<String>) -> String {
    name.filter(|value| !value.is_empty())
        .unwrap_or_else(|| unknown_agent_display(agent_id))
}

/// Search result row that includes `project_id` for cross-project queries (e.g. product search).
#[derive(Debug, Clone)]
pub struct SearchRowWithProject {
    pub id: i64,
    pub sender_id: i64,
    pub subject: String,
    pub importance: String,
    pub ack_required: i64,
    pub created_ts: i64,
    pub thread_id: Option<String>,
    pub from: String,
    pub body_md: String,
    pub project_id: i64,
}

// FTS5 unsearchable patterns that cannot produce meaningful results.
const FTS5_UNSEARCHABLE: &[&str] = &["*", "**", "***", ".", "..", "...", "?", "??", "???"];

/// Sanitize an FTS5 query string, fixing common issues.
///
/// Returns `None` when the query cannot produce meaningful results (caller
/// should return an empty list). Ports Python `_sanitize_fts_query()`.
#[must_use]
pub fn sanitize_fts_query(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Bare unsearchable patterns
    if FTS5_UNSEARCHABLE.contains(&trimmed) {
        return None;
    }

    // Punctuation/emoji-only queries (no alphanumeric content) cannot yield meaningful matches.
    if !trimmed.chars().any(char::is_alphanumeric) {
        return None;
    }

    // Bare boolean operators without terms
    let upper = trimmed.to_ascii_uppercase();
    if matches!(upper.as_str(), "AND" | "OR" | "NOT") {
        return None;
    }

    // Multi-token boolean operator sequences without any terms.
    // Examples: "AND OR NOT", "(AND) OR" → None.
    let mut saw_operator = false;
    let mut saw_term = false;
    for raw_tok in trimmed.split_whitespace() {
        let tok = raw_tok.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.is_empty() {
            continue;
        }
        match tok.to_ascii_uppercase().as_str() {
            "AND" | "OR" | "NOT" | "NEAR" => saw_operator = true,
            _ => {
                saw_term = true;
                break;
            }
        }
    }
    if saw_operator && !saw_term {
        return None;
    }

    let mut result = trimmed.to_string();

    // FTS5 doesn't support leading wildcards (*foo); strip iteratively
    while result.starts_with('*') {
        result = result[1..].to_string();
    }
    let mut result = result.trim().to_string();
    if result.is_empty() || !result.chars().any(char::is_alphanumeric) {
        return None;
    }

    // Trailing lone asterisk: "foo *" → "foo"
    if result.ends_with(" *") {
        result.truncate(result.len() - 2);
        let trimmed_end = result.trim_end().to_string();
        if trimmed_end.is_empty() {
            return None;
        }
        result = trimmed_end;
    }

    // Strip SQL comment markers (-- and /*) that have no FTS5 meaning
    while result.contains("--") {
        result = result.replace("--", " ");
    }
    while result.contains("/*") {
        result = result.replace("/*", " ");
    }
    while result.contains("*/") {
        result = result.replace("*/", " ");
    }

    // Collapse multiple consecutive spaces
    while result.contains("  ") {
        result = result.replace("  ", " ");
    }
    let mut result = result.trim().to_string();

    // Strip FTS5 metacharacters (defense-in-depth against re-enablement)
    result.retain(|c| !matches!(c, '{' | '}' | '^' | '[' | ']' | '~' | '\\'));
    // Re-strip comment markers that may have formed after metachar removal (e.g. "/[*" → "/*")
    while result.contains("--") {
        result = result.replace("--", " ");
    }
    while result.contains("/*") {
        result = result.replace("/*", " ");
    }
    while result.contains("*/") {
        result = result.replace("*/", " ");
    }
    while result.contains("  ") {
        result = result.replace("  ", " ");
    }
    let mut result = result.trim().to_string();
    if result.is_empty() || !result.chars().any(char::is_alphanumeric) {
        return None;
    }

    // Quote hyphenated tokens to prevent FTS5 from interpreting hyphens as operators.
    // Match: POL-358, FEAT-123, foo-bar-baz (not already quoted)
    result = quote_hyphenated_tokens(&result);

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Width of a UTF-8 character based on its leading byte.
///
/// Returns 1 for ASCII (0x00–0x7F), 2–4 for multi-byte sequences.
/// Input must be valid UTF-8 (guaranteed since callers operate on `&str`).
const fn utf8_char_width(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte < 0xE0 {
        2
    } else if first_byte < 0xF0 {
        3
    } else {
        4
    }
}

/// Copy a single UTF-8 character from `src` at byte offset `i` into `out`,
/// returning the byte width so the caller can advance its index correctly.
///
/// This avoids the `bytes[i] as char` anti-pattern which re-encodes each
/// byte of a multi-byte character individually, corrupting non-ASCII text
/// (e.g. `é` (0xC3 0xA9) → `Ã©` (0xC3 0x83 0xC2 0xA9)).
fn push_utf8_char(out: &mut String, src: &str, i: usize) -> usize {
    let w = utf8_char_width(src.as_bytes()[i]);
    let end = (i + w).min(src.len());
    out.push_str(&src[i..end]);
    end - i
}

/// Quote hyphenated tokens (e.g. `POL-358` → `"POL-358"`) for FTS5.
fn quote_hyphenated_tokens(query: &str) -> String {
    if !query.contains('-') {
        return query.to_string();
    }
    // If the entire query is a single quoted string, leave it alone
    if query.starts_with('"')
        && query.ends_with('"')
        && query.chars().filter(|c| *c == '"').count() == 2
    {
        return query.to_string();
    }

    let mut out = String::with_capacity(query.len() + 8);
    let mut in_quote = false;
    let mut i = 0;
    let bytes = query.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_quote = !in_quote;
            out.push('"');
            i += 1;
            continue;
        }
        if in_quote {
            i += push_utf8_char(&mut out, query, i);
            continue;
        }
        // Try to match a hyphenated token: [A-Za-z0-9]+(-[A-Za-z0-9]+)+
        if bytes[i].is_ascii_alphanumeric() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'-' {
                // Potential hyphenated token – check for at least one more segment
                let mut has_hyphen_segment = false;
                let mut j = i;
                while j < bytes.len() && bytes[j] == b'-' {
                    j += 1;
                    let seg_start = j;
                    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
                        j += 1;
                    }
                    if j > seg_start {
                        has_hyphen_segment = true;
                    } else {
                        break;
                    }
                }
                if has_hyphen_segment {
                    out.push('"');
                    out.push_str(&query[start..j]);
                    out.push('"');
                    i = j;
                } else {
                    out.push_str(&query[start..i]);
                }
            } else {
                out.push_str(&query[start..i]);
            }
        } else {
            i += push_utf8_char(&mut out, query, i);
        }
    }
    out
}

/// Extract LIKE fallback terms from a raw search query.
///
/// Returns up to `max_terms` alphanumeric tokens (min 2 chars each),
/// excluding FTS boolean keywords.
#[must_use]
pub fn extract_like_terms(query: &str, max_terms: usize) -> Vec<String> {
    const STOPWORDS: &[&str] = &["AND", "OR", "NOT", "NEAR"];
    let mut terms: Vec<String> = Vec::new();
    for token in query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_' && c != '/' && c != '-')
    {
        if token.len() < 2 {
            continue;
        }
        if STOPWORDS.iter().any(|&sw| sw.eq_ignore_ascii_case(token)) {
            continue;
        }
        if !terms.iter().any(|t| t.eq_ignore_ascii_case(token)) {
            terms.push(token.to_string());
        }
        if terms.len() >= max_terms {
            break;
        }
    }
    terms
}

/// Escape LIKE wildcards for literal substring matching.
#[must_use]
pub fn like_escape(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// LIKE fallback when FTS5 fails (e.g. malformed query syntax).
/// Builds `subject LIKE '%term%' OR body_md LIKE '%term%'` for each term.
async fn run_like_fallback(
    cx: &Cx,
    conn: &TrackedConnection<'_>,
    project_id: i64,
    terms: &[String],
    limit: i64,
) -> Outcome<Vec<sqlmodel_core::Row>, DbError> {
    // params layout: [project_id, term1_like, term1_like, term2_like, term2_like, ..., limit]
    let mut params: Vec<Value> = Vec::with_capacity(2 + terms.len() * 2);
    params.push(Value::BigInt(project_id));

    let mut where_parts: Vec<&str> = Vec::with_capacity(terms.len());
    for term in terms {
        let escaped = format!("%{}%", like_escape(term));
        params.push(Value::Text(escaped.clone()));
        params.push(Value::Text(escaped));
        where_parts.push("(m.subject LIKE ? ESCAPE '\\' OR m.body_md LIKE ? ESCAPE '\\')");
    }
    // Fallback should stay permissive: match when any extracted term appears.
    let where_clause = where_parts.join(" OR ");
    params.push(Value::BigInt(limit));

    let sql = format!(
        "SELECT m.id, m.sender_id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, \
                COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') as from_name, m.body_md \
         FROM messages m \
         LEFT JOIN agents a ON a.id = m.sender_id \
         WHERE m.project_id = ? AND ({where_clause}) \
         ORDER BY m.id DESC \
         LIMIT ?"
    );
    map_sql_outcome(traw_query(cx, conn, &sql, &params).await)
}

/// LIKE fallback for cross-project/product search when FTS5 fails (e.g. malformed query syntax).
async fn run_like_fallback_product(
    cx: &Cx,
    conn: &TrackedConnection<'_>,
    product_id: i64,
    terms: &[String],
    limit: i64,
) -> Outcome<Vec<sqlmodel_core::Row>, DbError> {
    // params layout: [product_id, term1_like, term1_like, term2_like, term2_like, ..., limit]
    let mut params: Vec<Value> = Vec::with_capacity(2 + terms.len() * 2);
    params.push(Value::BigInt(product_id));

    let mut where_parts: Vec<&str> = Vec::with_capacity(terms.len());
    for term in terms {
        let escaped = format!("%{}%", like_escape(term));
        params.push(Value::Text(escaped.clone()));
        params.push(Value::Text(escaped));
        where_parts.push("(m.subject LIKE ? ESCAPE '\\' OR m.body_md LIKE ? ESCAPE '\\')");
    }
    // Fallback should stay permissive: match when any extracted term appears.
    let where_clause = where_parts.join(" OR ");
    params.push(Value::BigInt(limit));

    let sql = format!(
        "SELECT m.id, m.sender_id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, \
                COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') as from_name, m.body_md, m.project_id \
         FROM messages m \
         LEFT JOIN agents a ON a.id = m.sender_id \
         JOIN product_project_links ppl ON ppl.project_id = m.project_id \
         WHERE ppl.product_id = ? AND ({where_clause}) \
         ORDER BY m.id DESC \
         LIMIT ?"
    );
    map_sql_outcome(traw_query(cx, conn, &sql, &params).await)
}

pub async fn search_messages(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Outcome<Vec<SearchRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };

    // Sanitize the FTS query; None means "no meaningful results possible"
    let sanitized = sanitize_fts_query(query);

    let rows_out = if sanitized.is_some() {
        // FTS5-backed search was decommissioned (br-2tnl.8.4).
        // Fall back directly to LIKE with extracted terms for legacy/fallback path.
        let terms = extract_like_terms(query, 5);
        if terms.is_empty() {
            Outcome::Ok(Vec::new())
        } else {
            run_like_fallback(cx, &tracked, project_id, &terms, limit_i64).await
        }
    } else {
        // Empty/unsearchable query: return empty results
        Outcome::Ok(Vec::new())
    };
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_as(0) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_id: i64 = match row.get_as(1) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let subject: String = match row.get_as(2) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let importance: String = match row.get_as(3) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let ack_required: i64 = match row.get_as(4) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let created_ts: i64 = match row.get_as(5) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let thread_id: Option<String> = match row.get_as(6) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let from: String = match row.get_as(7) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let body_md: String = row.get_as(8).unwrap_or_default();

                out.push(SearchRow {
                    id,
                    sender_id,
                    subject,
                    importance,
                    ack_required,
                    created_ts,
                    thread_id,
                    from,
                    body_md,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Full-text search across all projects linked to a product.
///
/// This is the DB-side primitive used by the MCP `search_messages_product` tool to avoid
/// per-project loops and to ensure global ranking is correct.
#[allow(clippy::too_many_lines)]
pub async fn search_messages_for_product(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
    query: &str,
    limit: usize,
) -> Outcome<Vec<SearchRowWithProject>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };

    let sanitized = sanitize_fts_query(query);
    let rows_out = if sanitized.is_some() {
        // FTS5-backed search was decommissioned (br-2tnl.8.4).
        // Fall back directly to LIKE with extracted terms for legacy/fallback path.
        let terms = extract_like_terms(query, 5);
        if terms.is_empty() {
            Outcome::Ok(Vec::new())
        } else {
            run_like_fallback_product(cx, &tracked, product_id, &terms, limit_i64).await
        }
    } else {
        Outcome::Ok(Vec::new())
    };

    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                // Use positional access for aliased columns where ORM column name inference
                // incorrectly parses "a.name as from_name" as "name as" instead of "from_name".
                // Column order: id(0), sender_id(1), subject(2), importance(3), ack_required(4),
                // created_ts(5), thread_id(6), from_name(7), body_md(8), project_id(9)
                let id: i64 = match row.get_named("id") {
                    Ok(v) => v,
                    Err(_) => match row.get_as(0) {
                        Ok(v) => v,
                        Err(_) => match row.get(0) {
                            Some(v) => value_as_i64(v).unwrap_or(0),
                            None => 0,
                        },
                    },
                };
                let sender_id: i64 = match row.get_as(1) {
                    Ok(v) => v,
                    Err(_) => match row.get(1) {
                        Some(v) => value_as_i64(v).unwrap_or(0),
                        None => 0,
                    },
                };
                let subject: String = match row.get_named("subject") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let importance: String = match row.get_named("importance") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let ack_required: i64 = match row.get_named("ack_required") {
                    Ok(v) => v,
                    Err(_) => match row.get_by_name("ack_required") {
                        Some(v) => value_as_i64(v).unwrap_or(0),
                        None => 0,
                    },
                };
                let created_ts: i64 = match row.get_named("created_ts") {
                    Ok(v) => v,
                    Err(_) => match row.get_by_name("created_ts") {
                        Some(val) => value_as_i64(val).unwrap_or(0),
                        None => match row.get_as(5) {
                            Ok(v) => v,
                            Err(_) => match row.get(5) {
                                Some(val) => value_as_i64(val).unwrap_or(0),
                                None => 0,
                            },
                        },
                    },
                };
                let thread_id: Option<String> = match row.get_named("thread_id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let from: String = match row.get_as(7) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let body_md: String = row.get_as(8).unwrap_or_default();
                let project_id: i64 = match row.get_as(9) {
                    Ok(v) => v,
                    Err(_) => match row.get(9) {
                        Some(v) => value_as_i64(v).unwrap_or(0),
                        None => 0,
                    },
                };

                out.push(SearchRowWithProject {
                    id,
                    sender_id,
                    subject,
                    importance,
                    ack_required,
                    created_ts,
                    thread_id,
                    from,
                    body_md,
                    project_id,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// Global (Cross-Project) Queries — br-2bbt.14.1
// =============================================================================

/// Inbox row that includes project context for global inbox view.
#[derive(Debug, Clone)]
pub struct GlobalInboxRow {
    pub message: MessageRow,
    pub kind: String,
    pub sender_name: String,
    pub ack_ts: Option<i64>,
    pub project_id: i64,
    pub project_slug: String,
}

/// Fetch inbox across ALL projects for a given agent name.
///
/// Unlike `fetch_inbox` which is scoped to a single project, this returns
/// messages from all projects where the agent exists. The agent is matched
/// by name, not ID, since agent IDs are project-specific.
#[allow(clippy::too_many_lines)]
pub async fn fetch_inbox_global(
    cx: &Cx,
    pool: &DbPool,
    agent_name: &str,
    urgent_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Outcome<Vec<GlobalInboxRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut sql = format!(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, \
                m.attachments, \
                r.kind, COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') as sender_name, r.ack_ts, \
                COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || m.project_id || ']') as project_slug \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         LEFT JOIN agents s ON s.id = m.sender_id \
         LEFT JOIN projects p ON p.id = m.project_id \
         WHERE r.agent_id IN (SELECT id FROM agents WHERE name = ? COLLATE NOCASE)"
    );

    let mut params: Vec<Value> = vec![Value::Text(agent_name.to_string())];

    if urgent_only {
        sql.push_str(" AND m.importance IN ('high', 'urgent')");
    }
    if let Some(ts) = since_ts {
        sql.push_str(" AND m.created_ts > ?");
        params.push(Value::BigInt(ts));
    }

    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };
    sql.push_str(" ORDER BY m.created_ts DESC LIMIT ?");
    params.push(Value::BigInt(limit_i64));

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = row.get_as(0).unwrap_or(0);
                let project_id: i64 = row.get_as(1).unwrap_or(0);
                let sender_id: i64 = row.get_as(2).unwrap_or(0);
                let thread_id: Option<String> = row.get_as(3).unwrap_or(None);
                let subject: String = row.get_as(4).unwrap_or_default();
                let body_md: String = row.get_as(5).unwrap_or_default();
                let importance: String = row.get_as(6).unwrap_or_default();
                let ack_required: i64 = row.get_as(7).unwrap_or(0);
                let created_ts: i64 = row.get_as(8).unwrap_or(0);
                let recipients_json: String = row.get_as(9).unwrap_or_default();
                let attachments: String = row.get_as(10).unwrap_or_default();
                let kind: String = row.get_as(11).unwrap_or_default();
                let sender_name: String = row.get_as(12).unwrap_or_default();
                let ack_ts: Option<i64> = row.get_as(13).unwrap_or(None);
                let project_slug: String = row.get_as(14).unwrap_or_default();

                out.push(GlobalInboxRow {
                    message: MessageRow {
                        id: Some(id),
                        project_id,
                        sender_id,
                        thread_id,
                        subject,
                        body_md,
                        importance,
                        ack_required,
                        created_ts,
                        recipients_json,
                        attachments,
                    },
                    kind,
                    sender_name,
                    ack_ts,
                    project_id,
                    project_slug,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Per-project unread message counts for global inbox view.
#[derive(Debug, Clone)]
pub struct ProjectUnreadCount {
    pub project_id: i64,
    pub project_slug: String,
    pub unread_count: i64,
}

/// Count unread messages per project for a given agent name.
///
/// Returns a list of (`project_id`, `project_slug`, `unread_count`) for all projects
/// where the agent has unread messages.
pub async fn count_unread_global(
    cx: &Cx,
    pool: &DbPool,
    agent_name: &str,
) -> Outcome<Vec<ProjectUnreadCount>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT m.project_id as project_id, \
               COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || m.project_id || ']') as project_slug, \
               COUNT(*) as unread_count \
               FROM message_recipients r \
               JOIN messages m ON m.id = r.message_id \
               LEFT JOIN projects p ON p.id = m.project_id \
               WHERE r.agent_id IN (SELECT id FROM agents WHERE name = ? COLLATE NOCASE) \
               AND r.read_ts IS NULL \
               GROUP BY m.project_id, project_slug \
               ORDER BY unread_count DESC";

    let params = [Value::Text(agent_name.to_string())];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let project_id: i64 = row.get_named("project_id").unwrap_or(0);
                let project_slug: String = row.get_named("project_slug").unwrap_or_default();
                let unread_count: i64 = row.get_named("unread_count").unwrap_or(0);
                out.push(ProjectUnreadCount {
                    project_id,
                    project_slug,
                    unread_count,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Search result with project context for global search.
#[derive(Debug, Clone)]
pub struct GlobalSearchRow {
    pub id: i64,
    pub sender_id: i64,
    pub subject: String,
    pub importance: String,
    pub ack_required: i64,
    pub created_ts: i64,
    pub thread_id: Option<String>,
    pub from: String,
    pub body_md: String,
    pub project_id: i64,
    pub project_slug: String,
}

/// Full-text search across ALL projects.
///
/// Unlike `search_messages` which is scoped to a single project, this searches
/// across all messages in the database and includes project context in results.
#[allow(clippy::too_many_lines)]
pub async fn search_messages_global(
    cx: &Cx,
    pool: &DbPool,
    query: &str,
    limit: usize,
) -> Outcome<Vec<GlobalSearchRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };

    let sanitized = sanitize_fts_query(query);
    let rows_out = if sanitized.is_some() {
        // FTS5-backed search was decommissioned (br-2tnl.8.4).
        // Fall back directly to LIKE with extracted terms for legacy/fallback path.
        let terms = extract_like_terms(query, 5);
        if terms.is_empty() {
            Outcome::Ok(Vec::new())
        } else {
            run_like_fallback_global(cx, &tracked, &terms, limit_i64).await
        }
    } else {
        Outcome::Ok(Vec::new())
    };
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = row.get_as(0).unwrap_or_default();
                let sender_id: i64 = row.get_as(1).unwrap_or_default();
                let subject: String = row.get_as(2).unwrap_or_default();
                let importance: String = row.get_as(3).unwrap_or_default();
                let ack_required: i64 = row.get_as(4).unwrap_or_default();
                let created_ts: i64 = row.get_as(5).unwrap_or_default();
                let thread_id: Option<String> = row.get_as(6).ok();
                let from: String = row.get_as(7).unwrap_or_default();
                let body_md: String = row.get_as(8).unwrap_or_default();
                let project_id: i64 = row.get_as(9).unwrap_or_default();
                let project_slug: String = row.get_as(10).unwrap_or_default();

                out.push(GlobalSearchRow {
                    id,
                    sender_id,
                    subject,
                    importance,
                    ack_required,
                    created_ts,
                    thread_id,
                    from,
                    body_md,
                    project_id,
                    project_slug,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// LIKE fallback for global search when FTS5 fails.
async fn run_like_fallback_global(
    cx: &Cx,
    conn: &TrackedConnection<'_>,
    terms: &[String],
    limit: i64,
) -> Outcome<Vec<sqlmodel_core::Row>, DbError> {
    if terms.is_empty() {
        return Outcome::Ok(Vec::new());
    }

    let mut conditions = Vec::with_capacity(terms.len());
    let mut params: Vec<Value> = Vec::with_capacity(terms.len() * 2 + 1);

    for term in terms {
        conditions.push("(m.subject LIKE ? ESCAPE '\\' OR m.body_md LIKE ? ESCAPE '\\')");
        let pattern = format!("%{}%", like_escape(term));
        params.push(Value::Text(pattern.clone()));
        params.push(Value::Text(pattern));
    }

    let sql = format!(
        "SELECT m.id, m.sender_id, m.subject, m.importance, m.ack_required, m.created_ts, \
                m.thread_id, COALESCE(a.name, '{UNKNOWN_SENDER_DISPLAY}') as from_name, m.body_md, \
                m.project_id, COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || m.project_id || ']') as project_slug \
         FROM messages m \
         LEFT JOIN agents a ON a.id = m.sender_id \
         LEFT JOIN projects p ON p.id = m.project_id \
         WHERE {} \
         ORDER BY m.created_ts DESC \
         LIMIT ?",
        conditions.join(" OR ")
    );
    params.push(Value::BigInt(limit));

    map_sql_outcome(traw_query(cx, conn, &sql, &params).await)
}

// =============================================================================
// MessageRecipient Queries
// =============================================================================

/// Add recipients to a message
pub async fn add_recipients(
    cx: &Cx,
    pool: &DbPool,
    message_id: i64,
    recipients: &[(i64, &str)], // (agent_id, kind)
) -> Outcome<(), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Batch all recipient inserts in a single transaction (1 fsync instead of N).
    run_with_mvcc_retry(cx, "add_recipients", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        for (agent_id, kind) in recipients {
            let row = MessageRecipientRow {
                message_id,
                agent_id: *agent_id,
                kind: (*kind).to_string(),
                read_ts: None,
                ack_ts: None,
            };
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(insert!(&row).execute(cx, &tracked).await)
            );
        }

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(())
    })
    .await
}

/// Mark message as read
pub async fn mark_message_read(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    message_id: i64,
) -> Outcome<i64, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "mark_message_read", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Idempotent: set read_ts if NULL. Acknowledgements are intentionally
        // separate state; callers must use acknowledge_message to set ack_ts.
        let sql = "UPDATE message_recipients \
                   SET read_ts = COALESCE(read_ts, ?) \
                   WHERE agent_id = ? AND message_id = ?";
        let params = [
            Value::BigInt(now),
            Value::BigInt(agent_id),
            Value::BigInt(message_id),
        ];
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
        );

        // Rebuild inbox_stats from ground truth.
        try_in_tx!(
            cx,
            &tracked,
            rebuild_agents_inbox_stats_in_tx(cx, &tracked, &[agent_id]).await
        );

        // Invalidate cached inbox stats (unread/ack counts may have changed).
        crate::cache::read_cache()
            .invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

        // Read back the actual stored timestamp (may differ from `now` on
        // idempotent calls where COALESCE preserved the original value).
        //
        // We intentionally do not trust `rows_affected` from the UPDATE above:
        // under some backend/runtime combinations, updates that clearly match
        // a row can report 0. Existence is determined by this read-back query.
        let read_sql =
            "SELECT read_ts FROM message_recipients WHERE agent_id = ? AND message_id = ?";
        let read_params = [Value::BigInt(agent_id), Value::BigInt(message_id)];
        let ts = match map_sql_outcome(traw_query(cx, &tracked, read_sql, &read_params).await) {
            Outcome::Ok(rows) => {
                if rows.is_empty() {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(DbError::not_found(
                        "MessageRecipient",
                        format!("{agent_id}:{message_id}"),
                    ));
                }
                rows.first()
                    .and_then(|r| r.get(0))
                    .and_then(|v| match v {
                        Value::BigInt(n) => Some(*n),
                        Value::Int(n) => Some(i64::from(*n)),
                        _ => None,
                    })
                    .unwrap_or(now)
            }
            Outcome::Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Panicked(p);
            }
        };

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(ts)
    })
    .await
}

/// Batch-mark multiple messages as read for a single agent in one transaction.
///
/// This is the high-performance counterpart of [`mark_message_read`] for use
/// in `fetch_inbox` where 20+ messages need to be marked read at once.
/// Reduces N separate transactions to a single transaction with N UPDATE
/// statements, cutting `fetch_inbox` latency by ~80%.
pub async fn mark_messages_read_batch(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    message_ids: &[i64],
) -> Outcome<(), DbError> {
    if message_ids.is_empty() {
        return Outcome::Ok(());
    }

    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "mark_messages_read_batch", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Batch UPDATE: mark all messages read in one pass per chunk.
        for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
            let ph = placeholders(chunk.len());
            let sql = format!(
                "UPDATE message_recipients \
                 SET read_ts = COALESCE(read_ts, ?) \
                 WHERE agent_id = ? AND message_id IN ({ph})"
            );
            let mut params = Vec::with_capacity(2 + chunk.len());
            params.push(Value::BigInt(now));
            params.push(Value::BigInt(agent_id));
            for &mid in chunk {
                params.push(Value::BigInt(mid));
            }
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, &sql, &params).await)
            );
        }

        // Single inbox_stats rebuild (instead of N rebuilds).
        try_in_tx!(
            cx,
            &tracked,
            rebuild_agents_inbox_stats_in_tx(cx, &tracked, &[agent_id]).await
        );

        crate::cache::read_cache()
            .invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(())
    })
    .await
}

/// Mark every unread message in a project inbox as read for a specific agent.
pub async fn mark_all_messages_read_in_project(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
) -> Outcome<i64, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "mark_all_messages_read_in_project", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Identify which messages are actually unread for this agent in this project.
        // We do this explicitly to avoid trusting unreliable rows_affected from UPDATE.
        let find_sql = "SELECT m.id FROM message_recipients r \
                        JOIN messages m ON m.id = r.message_id \
                        WHERE r.agent_id = ? AND r.read_ts IS NULL \
                        AND m.project_id = ?";
        let find_params = [Value::BigInt(agent_id), Value::BigInt(project_id)];
        // Route through try_in_tx! so the open BEGIN CONCURRENT is rolled back on
        // cancel/panic too (the bare match previously only rolled back on Err,
        // leaking a half-open transaction back into the pool on cancellation).
        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, find_sql, &find_params).await)
        );

        let count = rows.len();
        if count > 0 {
            // Mark unread messages as read without acknowledging them.
            let sql = "UPDATE message_recipients \
                       SET read_ts = ? \
                       WHERE agent_id = ? AND read_ts IS NULL \
                       AND message_id IN (SELECT id FROM messages WHERE project_id = ?)";
            let params = [
                Value::BigInt(now),
                Value::BigInt(agent_id),
                Value::BigInt(project_id),
            ];
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
            );

            // Rebuild inbox_stats from ground truth.
            try_in_tx!(
                cx,
                &tracked,
                rebuild_agents_inbox_stats_in_tx(cx, &tracked, &[agent_id]).await
            );
        }

        crate::cache::read_cache()
            .invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);

        let count_i64 = i64::try_from(count).expect("message recipient count fits in i64");
        Outcome::Ok(count_i64)
    })
    .await
}

/// Acknowledge message
pub async fn acknowledge_message(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    message_id: i64,
) -> Outcome<(i64, i64), DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "acknowledge_message", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Idempotent: set read_ts if NULL; set ack_ts if NULL.
        let sql = "UPDATE message_recipients \
                   SET read_ts = COALESCE(read_ts, ?), ack_ts = COALESCE(ack_ts, ?) \
                   WHERE agent_id = ? AND message_id = ?";
        let params = [
            Value::BigInt(now),
            Value::BigInt(now),
            Value::BigInt(agent_id),
            Value::BigInt(message_id),
        ];
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
        );

        // Rebuild inbox_stats from ground truth.
        try_in_tx!(
            cx,
            &tracked,
            rebuild_agents_inbox_stats_in_tx(cx, &tracked, &[agent_id]).await
        );

        // Invalidate cached inbox stats (ack_pending_count may have changed).
        crate::cache::read_cache()
            .invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

        // Read back the actual stored timestamps (may differ from `now` on
        // idempotent calls where COALESCE preserved the original values).
        //
        // We intentionally do not trust `rows_affected` from the UPDATE above:
        // under some backend/runtime combinations, updates that clearly match
        // a row can report 0. Existence is determined by this read-back query.
        let read_sql =
            "SELECT read_ts, ack_ts FROM message_recipients WHERE agent_id = ? AND message_id = ?";
        let read_params = [Value::BigInt(agent_id), Value::BigInt(message_id)];
        let (read_ts, ack_ts) =
            match map_sql_outcome(traw_query(cx, &tracked, read_sql, &read_params).await) {
                Outcome::Ok(rows) => {
                    if rows.is_empty() {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(DbError::not_found(
                            "MessageRecipient",
                            format!("{agent_id}:{message_id}"),
                        ));
                    }
                    let row = rows.first();
                    let read_ts = row
                        .and_then(|r| r.get(0))
                        .and_then(|v| match v {
                            Value::BigInt(n) => Some(*n),
                            Value::Int(n) => Some(i64::from(*n)),
                            _ => None,
                        })
                        .unwrap_or(now);
                    let ack_ts = row
                        .and_then(|r| r.get(1))
                        .and_then(|v| match v {
                            Value::BigInt(n) => Some(*n),
                            Value::Int(n) => Some(i64::from(*n)),
                            _ => None,
                        })
                        .unwrap_or(now);
                    (read_ts, ack_ts)
                }
                Outcome::Err(e) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(e);
                }
                Outcome::Cancelled(r) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Cancelled(r);
                }
                Outcome::Panicked(p) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Panicked(p);
                }
            };

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok((read_ts, ack_ts))
    })
    .await
}

/// Per-message outcome for [`acknowledge_messages_batch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchAcknowledgeResult {
    pub message_id: i64,
    pub read_ts: Option<i64>,
    pub ack_ts: Option<i64>,
    pub found: bool,
}

/// Batch-acknowledge multiple messages for a single recipient agent.
///
/// This is the high-performance counterpart of [`acknowledge_message`] for
/// coordination bursts. Input IDs are deduplicated in first-seen order, existing
/// rows are updated in one transaction, and missing recipient rows are reported
/// per item instead of failing the whole batch.
///
/// Archive/event policy: this helper only mutates SQLite read/ack state. It
/// intentionally does not write per-message Git archive artifacts or emit
/// operator events; higher-level batch surfaces should use the returned per-item
/// statuses to emit one grouped event rather than recreating a per-message storm.
pub async fn acknowledge_messages_batch(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    message_ids: &[i64],
) -> Outcome<Vec<BatchAcknowledgeResult>, DbError> {
    if message_ids.is_empty() {
        return Outcome::Ok(Vec::new());
    }

    let mut seen = HashSet::with_capacity(message_ids.len());
    let mut unique_message_ids = Vec::with_capacity(message_ids.len());
    for &message_id in message_ids {
        if seen.insert(message_id) {
            unique_message_ids.push(message_id);
        }
    }

    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "acknowledge_messages_batch", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        for chunk in unique_message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
            let ph = placeholders(chunk.len());
            let sql = format!(
                "UPDATE message_recipients \
                 SET read_ts = COALESCE(read_ts, ?), ack_ts = COALESCE(ack_ts, ?) \
                 WHERE agent_id = ? AND message_id IN ({ph})"
            );
            let mut params = Vec::with_capacity(3 + chunk.len());
            params.push(Value::BigInt(now));
            params.push(Value::BigInt(now));
            params.push(Value::BigInt(agent_id));
            params.extend(chunk.iter().map(|&message_id| Value::BigInt(message_id)));

            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, &sql, &params).await)
            );
        }

        try_in_tx!(
            cx,
            &tracked,
            rebuild_agents_inbox_stats_in_tx(cx, &tracked, &[agent_id]).await
        );

        crate::cache::read_cache()
            .invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

        let mut stored_by_message_id = BTreeMap::new();
        for chunk in unique_message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
            let ph = placeholders(chunk.len());
            let read_sql = format!(
                "SELECT message_id, read_ts, ack_ts \
                 FROM message_recipients \
                 WHERE agent_id = ? AND message_id IN ({ph})"
            );
            let mut read_params = Vec::with_capacity(1 + chunk.len());
            read_params.push(Value::BigInt(agent_id));
            read_params.extend(chunk.iter().map(|&message_id| Value::BigInt(message_id)));

            let rows =
                match map_sql_outcome(traw_query(cx, &tracked, &read_sql, &read_params).await) {
                    Outcome::Ok(rows) => rows,
                    Outcome::Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(e);
                    }
                    Outcome::Cancelled(r) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Cancelled(r);
                    }
                    Outcome::Panicked(p) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Panicked(p);
                    }
                };

            for row in rows {
                let Some(message_id) = row.get(0).and_then(value_as_i64) else {
                    continue;
                };
                let read_ts = row.get(1).and_then(value_as_i64);
                let ack_ts = row.get(2).and_then(value_as_i64);
                stored_by_message_id.insert(message_id, (read_ts, ack_ts));
            }
        }

        let results = unique_message_ids
            .iter()
            .map(|&message_id| {
                stored_by_message_id.remove(&message_id).map_or(
                    BatchAcknowledgeResult {
                        message_id,
                        read_ts: None,
                        ack_ts: None,
                        found: false,
                    },
                    |(read_ts, ack_ts)| BatchAcknowledgeResult {
                        message_id,
                        read_ts,
                        ack_ts,
                        found: true,
                    },
                )
            })
            .collect();

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(results)
    })
    .await
}

// =============================================================================
// Inbox Stats Queries (materialized aggregate counters)
// =============================================================================

/// Fetch materialized inbox stats for an agent (O(1) primary key lookup).
///
/// Returns `None` if the agent has never received any messages (no row
/// in `inbox_stats`).
pub async fn get_inbox_stats(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
) -> Outcome<Option<InboxStatsRow>, DbError> {
    // Check cache first (30s TTL).
    let cache_scope = cache_scope_for_pool(pool);
    if let Some(cached) = crate::cache::read_cache().get_inbox_stats_scoped(&cache_scope, agent_id)
    {
        return Outcome::Ok(Some(cached));
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT agent_id, total_count, unread_count, ack_pending_count, last_message_ts \
               FROM inbox_stats WHERE agent_id = ?";
    let params = [Value::BigInt(agent_id)];

    let out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
    match out {
        Outcome::Ok(rows) => {
            if rows.is_empty() {
                Outcome::Ok(None)
            } else {
                let row = &rows[0];
                let stats = InboxStatsRow {
                    agent_id: match row.get_named("agent_id") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    },
                    total_count: match row.get_named("total_count") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    },
                    unread_count: match row.get_named("unread_count") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    },
                    ack_pending_count: match row.get_named("ack_pending_count") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    },
                    last_message_ts: match row.get_named("last_message_ts") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    },
                };
                // Populate cache for next lookup.
                crate::cache::read_cache().put_inbox_stats_scoped(&cache_scope, &stats);
                Outcome::Ok(Some(stats))
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Rebuild `inbox_stats` for a single agent from ground truth, inside an
/// already-open transaction.
///
/// Uses DELETE + per-agent aggregate reads + INSERT to recompute counters from
/// `message_recipients` and `messages`. This is the canonical way to keep
/// `inbox_stats` consistent regardless of whether SQLite triggers fire,
/// partially fire, or are absent.
fn is_tolerable_inbox_stats_rebuild_error(error: &DbError) -> bool {
    match error {
        DbError::Sqlite(message) => {
            let lowered = message.to_ascii_lowercase();
            (lowered.contains("no such table") && lowered.contains("inbox_stats"))
                || (lowered.contains("inbox_stats")
                    && lowered.contains("view")
                    && lowered.contains("cannot modify"))
        }
        _ => false,
    }
}

async fn rebuild_agents_inbox_stats_in_tx(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
    agent_ids: &[i64],
) -> Outcome<(), DbError> {
    if agent_ids.is_empty() {
        return Outcome::Ok(());
    }

    // Deduplicate and chunk to stay safely within SQLite's bind-parameter cap
    // even when a project has hundreds or thousands of recipients.
    let mut unique_agent_ids = agent_ids.to_vec();
    unique_agent_ids.sort_unstable();
    unique_agent_ids.dedup();

    for chunk in unique_agent_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let reset_sql = format!("DELETE FROM inbox_stats WHERE agent_id IN ({placeholders})");

        let params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();

        match map_sql_outcome(traw_execute(cx, tracked, &reset_sql, &params).await) {
            Outcome::Ok(_) => {}
            Outcome::Err(error) if is_tolerable_inbox_stats_rebuild_error(&error) => {
                return Outcome::Ok(());
            }
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(panic) => return Outcome::Panicked(panic),
        }

        for agent_id in chunk {
            let stats = match compute_agent_inbox_stats_in_tx(cx, tracked, *agent_id).await {
                Outcome::Ok(stats) => stats,
                Outcome::Err(error) => return Outcome::Err(error),
                Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
                Outcome::Panicked(panic) => return Outcome::Panicked(panic),
            };
            let Some(stats) = stats else {
                continue;
            };
            match insert_agent_inbox_stats_in_tx(cx, tracked, *agent_id, stats).await {
                Outcome::Ok(()) => {}
                Outcome::Err(error) if is_tolerable_inbox_stats_rebuild_error(&error) => {
                    return Outcome::Ok(());
                }
                Outcome::Err(error) => return Outcome::Err(error),
                Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
                Outcome::Panicked(panic) => return Outcome::Panicked(panic),
            }
        }
    }

    Outcome::Ok(())
}

#[derive(Debug, Clone, Copy)]
struct AgentInboxStatsRebuild {
    total_count: i64,
    unread_count: i64,
    ack_pending_count: i64,
    last_message_ts: Option<i64>,
}

async fn compute_agent_inbox_stats_in_tx(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
    agent_id: i64,
) -> Outcome<Option<AgentInboxStatsRebuild>, DbError> {
    let sql = "\
        SELECT \
            COUNT(*) AS total_count, \
            SUM(CASE WHEN read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
            SUM(CASE \
                WHEN ack_ts IS NULL \
                 AND message_id IN (SELECT id FROM messages WHERE ack_required = 1) \
                THEN 1 ELSE 0 END) AS ack_pending_count, \
            (SELECT MAX(created_ts) \
               FROM messages \
              WHERE id IN (SELECT message_id \
                             FROM message_recipients \
                            WHERE agent_id = ?)) AS last_message_ts \
        FROM message_recipients \
        WHERE agent_id = ? \
          AND message_id IN (SELECT id FROM messages)";
    let rows = match map_sql_outcome(
        traw_query(
            cx,
            tracked,
            sql,
            &[Value::BigInt(agent_id), Value::BigInt(agent_id)],
        )
        .await,
    ) {
        Outcome::Ok(rows) => rows,
        Outcome::Err(error) => return Outcome::Err(error),
        Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
        Outcome::Panicked(panic) => return Outcome::Panicked(panic),
    };
    let Some(row) = rows.first() else {
        return Outcome::Err(DbError::Internal(format!(
            "inbox_stats rebuild returned no aggregate row for agent_id={agent_id}"
        )));
    };
    let total_count = row.get(0).and_then(value_as_i64).unwrap_or(0);
    if total_count == 0 {
        return Outcome::Ok(None);
    }
    Outcome::Ok(Some(AgentInboxStatsRebuild {
        total_count,
        unread_count: row.get(1).and_then(value_as_i64).unwrap_or(0),
        ack_pending_count: row.get(2).and_then(value_as_i64).unwrap_or(0),
        last_message_ts: row.get(3).and_then(value_as_i64),
    }))
}

async fn insert_agent_inbox_stats_in_tx(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
    agent_id: i64,
    stats: AgentInboxStatsRebuild,
) -> Outcome<(), DbError> {
    let sql = "INSERT INTO inbox_stats \
         (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
         VALUES (?, ?, ?, ?, ?)";
    let last_message_ts = stats.last_message_ts.map_or(Value::Null, Value::BigInt);
    let params = [
        Value::BigInt(agent_id),
        Value::BigInt(stats.total_count),
        Value::BigInt(stats.unread_count),
        Value::BigInt(stats.ack_pending_count),
        last_message_ts,
    ];
    match map_sql_outcome(traw_execute(cx, tracked, sql, &params).await) {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(error) => Outcome::Err(error),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(panic) => Outcome::Panicked(panic),
    }
}

/// Rebuild **all** rows in `inbox_stats` from ground truth.
///
/// Typically called once at startup via the sync counterpart in pool.rs.
pub async fn rebuild_all_inbox_stats(cx: &Cx, pool: &DbPool) -> Outcome<(), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    let delete_sql = "DELETE FROM inbox_stats";
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, delete_sql, &[]).await)
    );

    let agent_rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(
            traw_query(
                cx,
                &tracked,
                "SELECT DISTINCT agent_id FROM message_recipients ORDER BY agent_id",
                &[],
            )
            .await
        )
    );
    let mut agent_ids = Vec::with_capacity(agent_rows.len());
    for row in agent_rows {
        if let Some(agent_id) = row.get(0).and_then(value_as_i64) {
            agent_ids.push(agent_id);
        }
    }
    try_in_tx!(
        cx,
        &tracked,
        rebuild_agents_inbox_stats_in_tx(cx, &tracked, &agent_ids).await
    );

    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);

    crate::cache::read_cache().invalidate_all_inbox_stats_scoped(&cache_scope_for_pool(pool));

    Outcome::Ok(())
}

// =============================================================================
// FileReservation Queries
// =============================================================================

struct ReservationConflictIndex {
    exact_by_path: BTreeMap<String, Vec<Arc<CompiledPattern>>>,
    globs_by_prefix: BTreeMap<String, Vec<Arc<CompiledPattern>>>,
    root_globs: Vec<Arc<CompiledPattern>>,
}

impl ReservationConflictIndex {
    fn build(patterns: impl IntoIterator<Item = String>) -> Self {
        let mut exact_by_path = BTreeMap::new();
        let mut globs_by_prefix = BTreeMap::new();
        let mut root_globs = Vec::new();

        for raw_pattern in patterns {
            let compiled = CompiledPattern::cached(&raw_pattern);
            if !compiled.is_glob() {
                exact_by_path
                    .entry(compiled.normalized().to_owned())
                    .or_insert_with(Vec::new)
                    .push(compiled);
            } else if let Some(prefix) = compiled.first_literal_segment() {
                globs_by_prefix
                    .entry(prefix.to_owned())
                    .or_insert_with(Vec::new)
                    .push(compiled);
            } else {
                root_globs.push(compiled);
            }
        }

        Self {
            exact_by_path,
            globs_by_prefix,
            root_globs,
        }
    }

    fn first_conflict<'a>(&'a self, request_pat: &CompiledPattern) -> Option<&'a CompiledPattern> {
        let req_norm = request_pat.normalized();
        let req_prefix = request_pat.first_literal_segment();

        let found = if request_pat.is_glob() {
            self.first_glob_request_conflict(request_pat, req_prefix)
        } else {
            self.first_exact_request_conflict(request_pat, req_norm, req_prefix)
        };

        found.or_else(|| {
            self.root_globs
                .iter()
                .find(|res_pat| res_pat.overlaps(request_pat))
                .map(Arc::as_ref)
        })
    }

    fn first_exact_request_conflict<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        req_norm: &str,
        req_prefix: Option<&str>,
    ) -> Option<&'a CompiledPattern> {
        if let Some(exact) = self
            .exact_by_path
            .get(req_norm)
            .and_then(|entries| entries.first())
        {
            return Some(exact.as_ref());
        }

        for slash_idx in req_norm.match_indices('/').map(|(idx, _)| idx) {
            let ancestor = &req_norm[..slash_idx];
            if let Some(exact) = self
                .exact_by_path
                .get(ancestor)
                .and_then(|entries| entries.first())
            {
                return Some(exact.as_ref());
            }
        }

        if let Some(descendant_prefix) = reservation_descendant_prefix(req_norm) {
            for (path, entries) in self.exact_by_path.range(descendant_prefix.clone()..) {
                if !path.starts_with(&descendant_prefix) {
                    break;
                }
                if let Some(exact) = entries.first() {
                    return Some(exact.as_ref());
                }
            }
        }

        if let Some(prefix) = req_prefix {
            return self
                .globs_by_prefix
                .get(prefix)
                .and_then(|entries| entries.iter().find(|pat| pat.overlaps(request_pat)))
                .map(Arc::as_ref);
        }

        self.globs_by_prefix
            .values()
            .flat_map(|entries| entries.iter())
            .find(|pat| pat.overlaps(request_pat))
            .map(Arc::as_ref)
    }

    fn first_glob_request_conflict<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        req_prefix: Option<&str>,
    ) -> Option<&'a CompiledPattern> {
        if let Some(prefix) = req_prefix {
            if let Some(exact) = self.exact_by_path.get(prefix).and_then(|entries| {
                entries.iter().find(|exact_pat| {
                    request_pat.matches(exact_pat.normalized()) || exact_pat.overlaps(request_pat)
                })
            }) {
                return Some(exact.as_ref());
            }

            if let Some(descendant_prefix) = reservation_descendant_prefix(prefix) {
                for (path, entries) in self.exact_by_path.range(descendant_prefix.clone()..) {
                    if !path.starts_with(&descendant_prefix) {
                        break;
                    }
                    if let Some(exact) = entries.iter().find(|exact_pat| {
                        request_pat.matches(exact_pat.normalized())
                            || exact_pat.overlaps(request_pat)
                    }) {
                        return Some(exact.as_ref());
                    }
                }
            }

            return self
                .globs_by_prefix
                .get(prefix)
                .and_then(|entries| entries.iter().find(|pat| pat.overlaps(request_pat)))
                .map(Arc::as_ref);
        }

        if let Some(exact) = self
            .exact_by_path
            .values()
            .flat_map(|entries| entries.iter())
            .find(|exact_pat| {
                request_pat.matches(exact_pat.normalized()) || exact_pat.overlaps(request_pat)
            })
        {
            return Some(exact.as_ref());
        }

        self.globs_by_prefix
            .values()
            .flat_map(|entries| entries.iter())
            .find(|pat| pat.overlaps(request_pat))
            .map(Arc::as_ref)
    }
}

fn reservation_descendant_prefix(norm: &str) -> Option<String> {
    if norm.is_empty() {
        None
    } else {
        Some(format!("{norm}/"))
    }
}

/// Create file reservations
#[allow(clippy::too_many_arguments)]
pub async fn create_file_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    paths: &[&str],
    ttl_seconds: i64,
    exclusive: bool,
    reason: &str,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    let now = now_micros();
    let expires = now.saturating_add(ttl_seconds.saturating_mul(1_000_000));

    run_with_mvcc_retry(cx, "create_file_reservations", || async {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let tracked = tracked(&*conn);

        // Batch all reservation inserts in a single transaction (1 fsync instead of N).
        // Use IMMEDIATE transaction to serialize reservation checks and prevent TOCTOU races.
        try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

        let exclusive_filter = if exclusive {
            ""
        } else {
            "AND \"exclusive\" = 1"
        };

        // Check for conflicting active reservations held by others to prevent TOCTOU races.
        let conflict_sql = format!(
            "SELECT path_pattern FROM file_reservations \
             WHERE project_id = ? AND agent_id != ? \
               AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts > ? \
               {exclusive_filter}"
        );
        let conflict_params = [
            Value::BigInt(project_id),
            Value::BigInt(agent_id),
            Value::BigInt(now),
        ];
        let active_rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, &conflict_sql, &conflict_params).await)
        );

        let active_index = ReservationConflictIndex::build(
            active_rows
                .into_iter()
                .filter_map(|row| row.get_named::<String>("path_pattern").ok()),
        );

        for path in paths {
            let req_pat = CompiledPattern::cached(path);
            if let Some(active_pat) = active_index.first_conflict(req_pat.as_ref()) {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::ResourceBusy(format!(
                    "Reservation conflict: '{}' overlaps with active exclusive reservation '{}'",
                    path,
                    active_pat.normalized()
                )));
            }
        }

        let mut out: Vec<FileReservationRow> = Vec::with_capacity(paths.len());
        for path in paths {
            let mut row = FileReservationRow {
                id: None,
                project_id,
                agent_id,
                path_pattern: (*path).to_string(),
                exclusive: i64::from(exclusive),
                reason: reason.to_string(),
                created_ts: now,
                expires_ts: expires,
                released_ts: None,
            };

            // Insert the row explicitly so this critical coordination path does not
            // depend on macro-generated SQL shape.
            let insert_sql = "INSERT INTO file_reservations \
                (project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) \
                VALUES (?, ?, ?, ?, ?, ?, ?, NULL)";
            let insert_params = [
                Value::BigInt(row.project_id),
                Value::BigInt(row.agent_id),
                Value::Text(row.path_pattern.clone()),
                Value::BigInt(row.exclusive),
                Value::Text(row.reason.clone()),
                Value::BigInt(row.created_ts),
                Value::BigInt(row.expires_ts),
            ];
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
            );

            // Use connection-local rowid state to retrieve the ID for this exact insert.
            // This avoids cross-transaction races that can happen with MAX(id).
            let lookup_sql = "SELECT last_insert_rowid() AS id";
            let rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, lookup_sql, &[]).await)
            );
            let Some(id_row) = rows.first() else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(format!(
                    "file reservation insert succeeded but last_insert_rowid() returned no row for project_id={project_id} agent_id={agent_id} path={path}"
                )));
            };
            let id: i64 = match id_row.get_named("id") {
                Ok(v) => v,
                Err(e) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(map_sql_error(&e));
                }
            };
            if id <= 0 {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(format!(
                    "file reservation insert succeeded but last_insert_rowid() returned invalid id={id} for project_id={project_id} agent_id={agent_id} path={path}"
                )));
            }
            row.id = Some(id);
            out.push(row);
        }

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(out)
    })
    .await
}

/// Get active file reservations for a project.
///
/// Uses `BEGIN IMMEDIATE` to acquire a fresh WAL snapshot, ensuring the
/// caller always sees the latest committed reservation state.  Without an
/// explicit transaction the pooled connection may re-use a stale read
/// snapshot which causes phantom conflicts after release (Bug #85) and
/// missed conflicts before insert (Bug #86).
pub async fn get_active_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    run_with_mvcc_retry(cx, "get_active_reservations", || {
        get_active_reservations_once(cx, pool, project_id)
    })
    .await
}

async fn get_active_reservations_once(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    let now = now_micros();
    let candidate_predicate = active_reservation_candidate_predicate_for("fr");

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Force a fresh WAL snapshot so we never read stale reservation state.
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    // GH#180: fetch candidate active rows with the cheap `released_ts` predicate
    // (no release-ledger join, no `NOT IN` subquery), then subtract the release
    // ledger in Rust. This avoids the O(N·M) per-join-row rescan of the
    // uncorrelated `NOT IN` subquery under sqlmodel-frankensqlite that made this
    // a ~24s daemon-dispatch query on a 30k-row store, while producing identical
    // results. Survivors are (by definition) absent from the release ledger, so
    // their authoritative `released_ts` is `fr.released_ts`.
    let candidate_sql = format!(
        "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                fr.created_ts, fr.expires_ts, fr.released_ts AS released_ts \
         FROM file_reservations fr \
         WHERE fr.project_id = ? AND {candidate_predicate} AND fr.expires_ts > ?"
    );
    let candidate_params = [Value::BigInt(project_id), Value::BigInt(now)];

    let mut candidates =
        match map_sql_outcome(traw_query(cx, &tracked, &candidate_sql, &candidate_params).await) {
            Outcome::Ok(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    match decode_file_reservation_row(&row) {
                        Ok(decoded) => out.push(decoded),
                        Err(e) => {
                            rollback_tx(cx, &tracked).await;
                            return Outcome::Err(e);
                        }
                    }
                }
                out
            }
            Outcome::Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Panicked(p);
            }
        };

    let released_ids =
        match map_sql_outcome(traw_query(cx, &tracked, RELEASED_RESERVATION_IDS_SQL, &[]).await) {
            Outcome::Ok(rows) => {
                let mut set = std::collections::HashSet::with_capacity(rows.len());
                for row in &rows {
                    if let Some(id) = row.get(0).and_then(value_as_i64) {
                        set.insert(id);
                    }
                }
                set
            }
            Outcome::Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Panicked(p);
            }
        };

    candidates.retain(|row| row.id.is_none_or(|id| !released_ids.contains(&id)));

    // Commit the read-only IMMEDIATE tx to release the write lock promptly.
    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    Outcome::Ok(candidates)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseReservationChunkTarget {
    ReservationIds,
    Paths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReleasedReservationMarker {
    id: i64,
    released_ts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseReservationExpiryConstraint {
    Any,
    OnOrBefore(i64),
    Exact(i64),
}

fn release_reservation_chunk_plan(
    path_count: usize,
    reservation_id_count: usize,
) -> Option<(ReleaseReservationChunkTarget, usize)> {
    let ids_limit = MAX_RELEASE_RESERVATION_CHUNK_ITEMS.min(
        MAX_RELEASE_RESERVATION_FILTER_ITEMS
            .saturating_sub(path_count)
            .max(1),
    );
    let paths_limit = MAX_RELEASE_RESERVATION_CHUNK_ITEMS.min(
        MAX_RELEASE_RESERVATION_FILTER_ITEMS
            .saturating_sub(reservation_id_count)
            .max(1),
    );

    let chunk_ids = reservation_id_count > ids_limit;
    let chunk_paths = path_count > paths_limit;
    match (chunk_ids, chunk_paths) {
        (false, false) => None,
        (true, false) => Some((ReleaseReservationChunkTarget::ReservationIds, ids_limit)),
        (false, true) => Some((ReleaseReservationChunkTarget::Paths, paths_limit)),
        (true, true) => {
            if reservation_id_count >= path_count {
                Some((ReleaseReservationChunkTarget::ReservationIds, ids_limit))
            } else {
                Some((ReleaseReservationChunkTarget::Paths, paths_limit))
            }
        }
    }
}

fn append_release_reservation_filters(
    sql: &mut String,
    params: &mut Vec<Value>,
    reservation_ids: Option<&[i64]>,
    paths: Option<&[&str]>,
) {
    if let Some(ids) = reservation_ids {
        if ids.is_empty() {
            sql.push_str(" AND 0");
        } else {
            sql.push_str(" AND id IN (");
            for (i, id) in ids.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push('?');
                params.push(Value::BigInt(*id));
            }
            sql.push(')');
        }
    }

    if let Some(pats) = paths {
        if pats.is_empty() {
            sql.push_str(" AND 0");
        } else {
            sql.push_str(" AND (");
            for (i, pat) in pats.iter().enumerate() {
                if i > 0 {
                    sql.push_str(" OR ");
                }
                sql.push_str("path_pattern = ?");
                params.push(Value::Text((*pat).to_string()));
            }
            sql.push(')');
        }
    }
}

fn apply_release_markers(
    mut reservations: Vec<FileReservationRow>,
    markers: &[ReleasedReservationMarker],
) -> Vec<FileReservationRow> {
    if reservations.is_empty() || markers.is_empty() {
        return Vec::new();
    }

    let released_ts_by_id: std::collections::HashMap<i64, i64> = markers
        .iter()
        .map(|marker| (marker.id, marker.released_ts))
        .collect();
    reservations.retain(|reservation| {
        reservation
            .id
            .is_some_and(|id| released_ts_by_id.contains_key(&id))
    });
    for reservation in &mut reservations {
        reservation.released_ts = reservation
            .id
            .and_then(|id| released_ts_by_id.get(&id).copied());
    }
    reservations
}

/// Release file reservations
#[allow(clippy::too_many_lines, clippy::must_use_candidate)]
pub fn release_reservations<'a>(
    cx: &'a Cx,
    pool: &'a DbPool,
    project_id: i64,
    agent_id: i64,
    paths: Option<&'a [&'a str]>,
    reservation_ids: Option<&'a [i64]>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Outcome<Vec<FileReservationRow>, DbError>> + Send + 'a>,
> {
    Box::pin(async move {
        // Avoid exceeding SQLite bind parameter limits by chunking very large filters.
        // Each chunk call uses the same logic below and commits independently.
        let path_count = paths.map_or(0, <[&str]>::len);
        let reservation_id_count = reservation_ids.map_or(0, <[i64]>::len);
        if let Some((target, chunk_size)) =
            release_reservation_chunk_plan(path_count, reservation_id_count)
        {
            let mut released = Vec::new();
            match target {
                ReleaseReservationChunkTarget::ReservationIds => {
                    if let Some(ids) = reservation_ids {
                        for chunk in ids.chunks(chunk_size) {
                            let rows = match release_reservations(
                                cx,
                                pool,
                                project_id,
                                agent_id,
                                paths,
                                Some(chunk),
                            )
                            .await
                            {
                                Outcome::Ok(rows) => rows,
                                Outcome::Err(e) => return Outcome::Err(e),
                                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                                Outcome::Panicked(p) => return Outcome::Panicked(p),
                            };
                            released.extend(rows);
                        }
                    }
                }
                ReleaseReservationChunkTarget::Paths => {
                    if let Some(pats) = paths {
                        for chunk in pats.chunks(chunk_size) {
                            let rows = match release_reservations(
                                cx,
                                pool,
                                project_id,
                                agent_id,
                                Some(chunk),
                                reservation_ids,
                            )
                            .await
                            {
                                Outcome::Ok(rows) => rows,
                                Outcome::Err(e) => return Outcome::Err(e),
                                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                                Outcome::Panicked(p) => return Outcome::Panicked(p),
                            };
                            released.extend(rows);
                        }
                    }
                }
            }
            return Outcome::Ok(released);
        }

        let selected = run_with_mvcc_retry(cx, "release_reservations_select", || async {
            let conn = match acquire_conn(cx, pool).await {
                Outcome::Ok(c) => c,
                Outcome::Err(e) => return Outcome::Err(e),
                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                Outcome::Panicked(p) => return Outcome::Panicked(p),
            };

            let tracked_conn = tracked(&*conn);
            // Bulk release scans use IMMEDIATE tx semantics for deterministic
            // visibility on FrankenSQLite.
            try_in_tx!(
                cx,
                &tracked_conn,
                begin_immediate_tx(cx, &tracked_conn).await
            );

            let mut filter_sql = format!(
                " WHERE project_id = ? AND agent_id = ? AND ({ACTIVE_RESERVATION_PREDICATE})"
            );
            let mut filter_params: Vec<Value> =
                vec![Value::BigInt(project_id), Value::BigInt(agent_id)];
            append_release_reservation_filters(
                &mut filter_sql,
                &mut filter_params,
                reservation_ids,
                paths,
            );

            let select_sql = format!("{FILE_RESERVATION_SELECT_COLUMNS_SQL}{filter_sql}");
            let rows_out =
                map_sql_outcome(traw_query(cx, &tracked_conn, &select_sql, &filter_params).await);
            let reservations: Vec<FileReservationRow> = match rows_out {
                Outcome::Ok(rows) => {
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        match decode_file_reservation_row(&row) {
                            Ok(decoded) => out.push(decoded),
                            Err(e) => {
                                rollback_tx(cx, &tracked_conn).await;
                                return Outcome::Err(e);
                            }
                        }
                    }
                    out
                }
                Outcome::Err(e) => {
                    rollback_tx(cx, &tracked_conn).await;
                    return Outcome::Err(e);
                }
                Outcome::Cancelled(r) => {
                    rollback_tx(cx, &tracked_conn).await;
                    return Outcome::Cancelled(r);
                }
                Outcome::Panicked(p) => {
                    rollback_tx(cx, &tracked_conn).await;
                    return Outcome::Panicked(p);
                }
            };

            if reservations.is_empty() {
                try_in_tx!(cx, &tracked_conn, commit_tx(cx, &tracked_conn).await);
                return Outcome::Ok((reservations, Vec::<i64>::new()));
            }

            let target_ids: Vec<i64> = reservations.iter().filter_map(|row| row.id).collect();
            if target_ids.len() != reservations.len() {
                rollback_tx(cx, &tracked_conn).await;
                return Outcome::Err(DbError::Internal(format!(
                    "release_reservations expected {} row ids but found {}",
                    reservations.len(),
                    target_ids.len()
                )));
            }

            // Commit the read transaction first, then delegate writes to the
            // per-id release path which is more stable on FrankenSQLite.
            try_in_tx!(cx, &tracked_conn, commit_tx(cx, &tracked_conn).await);
            Outcome::Ok((reservations, target_ids))
        })
        .await;

        let (reservations, target_ids) = match selected {
            Outcome::Ok(selected) => selected,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        if target_ids.is_empty() {
            return Outcome::Ok(reservations);
        }

        let released_markers =
            match release_reservations_by_ids_matching_expiry(cx, pool, &target_ids, None).await {
                Outcome::Ok(markers) => markers,
                Outcome::Err(e) => return Outcome::Err(e),
                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                Outcome::Panicked(p) => return Outcome::Panicked(p),
            };

        Outcome::Ok(apply_release_markers(reservations, &released_markers))
    }) // Box::pin(async move {
}

/// Release reservations by id, optionally requiring `expires_ts` to be on or
/// before the provided cutoff.
async fn release_reservations_by_ids_matching_expiry(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
    expires_at_or_before: Option<i64>,
) -> Outcome<Vec<ReleasedReservationMarker>, DbError> {
    let expiry_constraint = expires_at_or_before
        .map_or(ReleaseReservationExpiryConstraint::Any, |cutoff| {
            ReleaseReservationExpiryConstraint::OnOrBefore(cutoff)
        });
    release_reservations_by_ids_with_expiry_constraint(cx, pool, ids, expiry_constraint).await
}

/// Internal release primitive that supports exact-match and cutoff-based
/// expiry guards without changing the public DB API.
async fn release_reservations_by_ids_with_expiry_constraint(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
    expiry_constraint: ReleaseReservationExpiryConstraint,
) -> Outcome<Vec<ReleasedReservationMarker>, DbError> {
    if ids.is_empty() {
        return Outcome::Ok(Vec::new());
    }

    run_with_mvcc_retry(cx, "release_reservations_by_ids", || async {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        let tracked = tracked(&*conn);
        try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(
                traw_execute(
                    cx,
                    &tracked,
                    "CREATE TABLE IF NOT EXISTS file_reservation_releases (\
                        reservation_id INTEGER PRIMARY KEY,\
                        released_ts INTEGER NOT NULL\
                     )",
                    &[],
                )
                .await
            )
        );

        let mut release_marker = now_micros();
        let mut released = Vec::with_capacity(ids.len());

        // Build the eligibility check: active reservation not already released.
        // ACTIVE_RESERVATION_PREDICATE already includes the release-ledger
        // exclusion, so no additional NOT IN clause is needed.
        let mut check_sql = format!(
            "SELECT 1 FROM file_reservations \
             WHERE id = ? AND ({ACTIVE_RESERVATION_PREDICATE})"
        );
        match expiry_constraint {
            ReleaseReservationExpiryConstraint::Any => {}
            ReleaseReservationExpiryConstraint::OnOrBefore(_) => {
                check_sql.push_str(" AND expires_ts <= ?");
            }
            ReleaseReservationExpiryConstraint::Exact(_) => {
                check_sql.push_str(" AND expires_ts = ?");
            }
        }
        check_sql.push_str(" LIMIT 1");

        // Record the release in both the base row and the sidecar ledger. The
        // sidecar remains the audit source, while the base row keeps active
        // reservation predicates correct on same-process readers that have
        // already materialized file_reservations.
        let update_sql = "UPDATE file_reservations SET released_ts = ? WHERE id = ?";
        let insert_sql = "INSERT OR IGNORE INTO file_reservation_releases (reservation_id, released_ts) \
             VALUES (?, ?)";

        for id in ids {
            let released_ts = release_marker;

            // Step 1: Check eligibility.
            let mut check_params: Vec<Value> = vec![Value::BigInt(*id)];
            match expiry_constraint {
                ReleaseReservationExpiryConstraint::Any => {}
                ReleaseReservationExpiryConstraint::OnOrBefore(expiry_cutoff)
                | ReleaseReservationExpiryConstraint::Exact(expiry_cutoff) => {
                    check_params.push(Value::BigInt(expiry_cutoff));
                }
            }
            let eligible_rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, &check_sql, &check_params).await)
            );
            if eligible_rows.is_empty() {
                continue;
            }

            // Step 2: Update the base reservation row first.
            let update_params = [Value::BigInt(released_ts), Value::BigInt(*id)];
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, update_sql, &update_params).await)
            );

            // Step 3: Record the release in the sidecar ledger.
            let insert_params = [Value::BigInt(*id), Value::BigInt(released_ts)];
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
            );

            release_marker = release_marker.saturating_add(1);
            released.push(ReleasedReservationMarker {
                id: *id,
                released_ts,
            });
        }

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(released)
    })
    .await
}

/// Renew file reservations
#[allow(clippy::too_many_lines)]
pub async fn renew_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    extend_seconds: i64,
    paths: Option<&[&str]>,
    reservation_ids: Option<&[i64]>,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    let now = now_micros();
    let extend = extend_seconds.saturating_mul(1_000_000);

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Wrap entire read-modify-write in a transaction so partial renewals
    // cannot occur if the process crashes or is cancelled mid-loop.
    run_with_mvcc_retry(cx, "renew_reservations", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        // Fetch candidate reservations first (so tools can report old/new expiry).
        let mut sql = format!(
            "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts \
             FROM file_reservations \
             WHERE project_id = ? AND agent_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts > ?"
        );
        let mut params: Vec<Value> = vec![
            Value::BigInt(project_id),
            Value::BigInt(agent_id),
            Value::BigInt(now),
        ];

        if let Some(ids) = reservation_ids {
            if ids.is_empty() {
                sql.push_str(" AND 0");
            } else {
                sql.push_str(" AND id IN (");
                for (i, id) in ids.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push('?');
                    params.push(Value::BigInt(*id));
                }
                sql.push(')');
            }
        }

        if let Some(pats) = paths {
            if pats.is_empty() {
                sql.push_str(" AND 0");
            } else {
                sql.push_str(" AND (");
                for (i, pat) in pats.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(" OR ");
                    }
                    sql.push_str("path_pattern = ?");
                    params.push(Value::Text((*pat).to_string()));
                }
                sql.push(')');
            }
        }

        let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
        let mut reservations: Vec<FileReservationRow> = match rows_out {
            Outcome::Ok(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    match FileReservationRow::from_row(&r) {
                        Ok(row) => out.push(row),
                        Err(e) => {
                            rollback_tx(cx, &tracked).await;
                            return Outcome::Err(map_sql_error(&e));
                        }
                    }
                }
                out
            }
            Outcome::Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Panicked(p);
            }
        };

        for row in &mut reservations {
            let base = row.expires_ts.max(now);
            row.expires_ts = base.saturating_add(extend);
            let Some(id) = row.id else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(
                    "renew_reservations: expected id to be populated".to_string(),
                ));
            };

            let sql = "UPDATE file_reservations SET expires_ts = ? WHERE id = ?";
            let params = [Value::BigInt(row.expires_ts), Value::BigInt(id)];
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
            );
        }

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(reservations)
    })
    .await
}

/// List file reservations for a project.
///
/// When `active_only` is true, uses `BEGIN IMMEDIATE` to acquire a fresh WAL
/// snapshot — same rationale as [`get_active_reservations`].
#[allow(clippy::too_many_lines)]
pub async fn list_file_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    active_only: bool,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    run_with_mvcc_retry(cx, "list_file_reservations", || {
        list_file_reservations_once(cx, pool, project_id, active_only)
    })
    .await
}

async fn list_file_reservations_once(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    active_only: bool,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Force a fresh WAL snapshot for active-only reads to avoid stale
    // reservation state after a release (Bug #85) or concurrent grant.
    if active_only {
        try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);
    }

    let (sql, params) = if active_only {
        let now = now_micros();
        // GH#180: candidate rows via the cheap `released_ts` predicate (no
        // release-ledger join, no `NOT IN`); the ledger is subtracted in Rust
        // below. Survivors are absent from the ledger, so `fr.released_ts` is
        // their authoritative released_ts.
        let candidate_predicate = active_reservation_candidate_predicate_for("fr");
        (
            format!(
                "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                        fr.created_ts, fr.expires_ts, fr.released_ts AS released_ts \
                 FROM file_reservations fr \
                 WHERE fr.project_id = ? AND {candidate_predicate} AND fr.expires_ts > ? ORDER BY fr.id"
            ),
            vec![Value::BigInt(project_id), Value::BigInt(now)],
        )
    } else {
        (
            // Legacy Python schema stored released_ts as TEXT (e.g. "2026-02-05 02:21:37.212634").
            // Coerce it to INTEGER microseconds so listing historical reservations can't crash.
            // Prefer the sidecar release ledger when present because it is the
            // authoritative release source for modern reservations.
            "SELECT \
                 fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, fr.created_ts, fr.expires_ts, \
                 COALESCE(rr.released_ts, CASE \
                     WHEN fr.released_ts IS NULL THEN NULL \
                     WHEN typeof(fr.released_ts) = 'text' THEN CAST(strftime('%s', fr.released_ts) AS INTEGER) * 1000000 + \
                         CASE WHEN instr(fr.released_ts, '.') > 0 \
                              THEN CAST(substr(REPLACE(fr.released_ts, 'Z', '') || '000000', instr(fr.released_ts, '.') + 1, 6) AS INTEGER) \
                              ELSE 0 \
                         END \
                     ELSE fr.released_ts \
                 END) AS released_ts \
             FROM file_reservations fr \
             LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
             WHERE fr.project_id = ? \
             ORDER BY fr.id"
                .to_string(),
            vec![Value::BigInt(project_id)],
        )
    };

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    let mut result = match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("id") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let proj_id: i64 = match row.get_named("project_id") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let agent_id: i64 = match row.get_named("agent_id") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let path_pattern: String = match row.get_named("path_pattern") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let exclusive: i64 = match row.get_named("exclusive") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let reason: String = match row.get_named("reason") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let created_ts: i64 = match row.get_named("created_ts") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let expires_ts: i64 = match row.get_named("expires_ts") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let released_ts: Option<i64> = match row.get_named("released_ts") {
                    Ok(v) => v,
                    Err(e) => {
                        if active_only {
                            rollback_tx(cx, &tracked).await;
                        }
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                out.push(FileReservationRow {
                    id: Some(id),
                    project_id: proj_id,
                    agent_id,
                    path_pattern,
                    exclusive,
                    reason,
                    created_ts,
                    expires_ts,
                    released_ts,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => {
            if active_only {
                rollback_tx(cx, &tracked).await;
            }
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => {
            if active_only {
                rollback_tx(cx, &tracked).await;
            }
            return Outcome::Cancelled(r);
        }
        Outcome::Panicked(p) => {
            if active_only {
                rollback_tx(cx, &tracked).await;
            }
            return Outcome::Panicked(p);
        }
    };

    // Commit the read-only IMMEDIATE tx.
    if active_only {
        // GH#180: subtract the sidecar release ledger in Rust (the candidate
        // query above intentionally omits the O(N·M) `NOT IN` anti-join).
        if let Outcome::Ok(rows) = &mut result {
            let released_ids = match map_sql_outcome(
                traw_query(cx, &tracked, RELEASED_RESERVATION_IDS_SQL, &[]).await,
            ) {
                Outcome::Ok(id_rows) => {
                    let mut set = std::collections::HashSet::with_capacity(id_rows.len());
                    for row in &id_rows {
                        if let Some(id) = row.get(0).and_then(value_as_i64) {
                            set.insert(id);
                        }
                    }
                    set
                }
                Outcome::Err(e) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(e);
                }
                Outcome::Cancelled(r) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Cancelled(r);
                }
                Outcome::Panicked(p) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Panicked(p);
                }
            };
            rows.retain(|row| row.id.is_none_or(|id| !released_ids.contains(&id)));
        }
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    }
    result
}

/// List unreleased file reservations for a project (includes expired).
///
/// This is used by cleanup logic to avoid scanning the full historical table
/// (released reservations can be unbounded).
pub async fn list_unreleased_file_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    run_with_mvcc_retry(cx, "list_unreleased_file_reservations", || {
        list_unreleased_file_reservations_once(cx, pool, project_id)
    })
    .await
}

async fn list_unreleased_file_reservations_once(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Force a fresh WAL snapshot (same rationale as get_active_reservations).
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    let sql = format!(
        "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, \
         CASE \
             WHEN released_ts IS NULL THEN NULL \
             WHEN typeof(released_ts) = 'text' THEN CAST(strftime('%s', released_ts) AS INTEGER) * 1000000 + \
                 CASE WHEN instr(released_ts, '.') > 0 \
                      THEN CAST(substr(REPLACE(released_ts, 'Z', '') || '000000', instr(released_ts, '.') + 1, 6) AS INTEGER) \
                      ELSE 0 \
                 END \
             ELSE released_ts \
         END AS released_ts \
         FROM file_reservations WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) ORDER BY id"
    );
    let params = vec![Value::BigInt(project_id)];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    let result = match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("id") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let proj_id: i64 = match row.get_named("project_id") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let agent_id: i64 = match row.get_named("agent_id") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let path_pattern: String = match row.get_named("path_pattern") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let exclusive: i64 = match row.get_named("exclusive") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let reason: String = match row.get_named("reason") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let created_ts: i64 = match row.get_named("created_ts") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let expires_ts: i64 = match row.get_named("expires_ts") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                let released_ts: Option<i64> = match row.get_named("released_ts") {
                    Ok(v) => v,
                    Err(e) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(map_sql_error(&e));
                    }
                };
                out.push(FileReservationRow {
                    id: Some(id),
                    project_id: proj_id,
                    agent_id,
                    path_pattern,
                    exclusive,
                    reason,
                    created_ts,
                    expires_ts,
                    released_ts,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => {
            rollback_tx(cx, &tracked).await;
            return Outcome::Cancelled(r);
        }
        Outcome::Panicked(p) => {
            rollback_tx(cx, &tracked).await;
            return Outcome::Panicked(p);
        }
    };

    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    result
}

// =============================================================================
// AgentLink Queries
// =============================================================================

/// Request contact (create pending link)
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn request_contact(
    cx: &Cx,
    pool: &DbPool,
    from_project_id: i64,
    from_agent_id: i64,
    to_project_id: i64,
    to_agent_id: i64,
    reason: &str,
    ttl_seconds: i64,
) -> Outcome<AgentLinkRow, DbError> {
    let now = now_micros();
    let expires = if ttl_seconds > 0 {
        Some(now.saturating_add(ttl_seconds.saturating_mul(1_000_000)))
    } else {
        None
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "request_contact", || async {
        try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

        // FrankenConnection does not consistently support `ON CONFLICT ... DO UPDATE`.
        // Keep this path portable by doing insert-then-refresh on uniqueness conflict
        // inside one transaction.
        let insert_sql = "INSERT INTO agent_links \
            (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
            VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?)";

        let insert_params: Vec<Value> = vec![
            Value::BigInt(from_project_id),
            Value::BigInt(from_agent_id),
            Value::BigInt(to_project_id),
            Value::BigInt(to_agent_id),
            Value::Text(reason.to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
            expires.map_or(Value::Null, Value::BigInt),
        ];
        let is_contact_pair_unique_violation = |err: &DbError| match err {
            DbError::Sqlite(msg) => {
                let msg = msg.to_ascii_lowercase();
                msg.contains("unique constraint failed")
                    && (msg.contains("agent_links.a_project_id")
                        || msg.contains("agent_links.a_agent_id")
                        || msg.contains("agent_links.b_project_id")
                        || msg.contains("agent_links.b_agent_id")
                        || msg.contains("idx_agent_links_pair_unique"))
            }
            _ => false,
        };

        match map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await) {
            Outcome::Ok(_) => {}
            Outcome::Err(e) => {
                if is_contact_pair_unique_violation(&e) {
                    let refresh_sql = "UPDATE agent_links \
                        SET status = 'pending', reason = ?, updated_ts = ?, expires_ts = ? \
                        WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? AND b_agent_id = ? \
                          AND status != 'blocked'";
                    let refresh_params = vec![
                        Value::Text(reason.to_string()),
                        Value::BigInt(now),
                        expires.map_or(Value::Null, Value::BigInt),
                        Value::BigInt(from_project_id),
                        Value::BigInt(from_agent_id),
                        Value::BigInt(to_project_id),
                        Value::BigInt(to_agent_id),
                    ];
                    let _updated_rows = try_in_tx!(
                        cx,
                        &tracked,
                        map_sql_outcome(
                            traw_execute(cx, &tracked, refresh_sql, &refresh_params).await
                        )
                    );
                } else {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(e);
                }
            }
            Outcome::Cancelled(r) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Panicked(p);
            }
        }

        // Fetch the upserted row using explicit columns to avoid SELECT * decoding issues.
        let fetch_sql = format!(
            "{AGENT_LINK_SELECT_COLUMNS_SQL} \
             WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? AND b_agent_id = ? \
             LIMIT 1"
        );
        let fetch_params = [
            Value::BigInt(from_project_id),
            Value::BigInt(from_agent_id),
            Value::BigInt(to_project_id),
            Value::BigInt(to_agent_id),
        ];

        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, &fetch_sql, &fetch_params).await)
        );
        let Some(row) = rows.first() else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::not_found("AgentLink", "inserted/refreshed row"));
        };
        let decoded = match decode_agent_link_row(row) {
            Ok(link) => link,
            Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
        };
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(decoded)
    })
    .await
}

/// Respond to contact request
#[allow(clippy::too_many_arguments)]
pub async fn respond_contact(
    cx: &Cx,
    pool: &DbPool,
    from_project_id: i64,
    from_agent_id: i64,
    to_project_id: i64,
    to_agent_id: i64,
    accept: bool,
    ttl_seconds: i64,
) -> Outcome<(usize, AgentLinkRow), DbError> {
    let now = now_micros();
    let status = if accept { "approved" } else { "blocked" };
    let expires = if ttl_seconds > 0 && accept {
        Some(now.saturating_add(ttl_seconds.saturating_mul(1_000_000)))
    } else {
        None
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "respond_contact", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let existing_sql = format!(
            "{AGENT_LINK_SELECT_COLUMNS_SQL} \
             WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? AND b_agent_id = ? \
             LIMIT 1"
        );
        let existing_params = [
            Value::BigInt(from_project_id),
            Value::BigInt(from_agent_id),
            Value::BigInt(to_project_id),
            Value::BigInt(to_agent_id),
        ];

        let existing_rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, &existing_sql, &existing_params).await)
        );
        let Some(existing_row) = existing_rows.first() else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::not_found(
                "AgentLink",
                format!("{from_project_id}:{from_agent_id}->{to_project_id}:{to_agent_id}"),
            ));
        };
        let mut row = match decode_agent_link_row(existing_row) {
            Ok(link) => link,
            Err(e) => {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(e);
            }
        };
        row.status = status.to_string();
        row.updated_ts = now;
        row.expires_ts = expires;

        let Some(link_id) = row.id else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::invalid(
                "AgentLink.id",
                "existing agent_link row has NULL id",
            ));
        };
        let update_sql =
            "UPDATE agent_links SET status = ?, updated_ts = ?, expires_ts = ? WHERE id = ?";
        let update_params = [
            Value::Text(row.status.clone()),
            Value::BigInt(row.updated_ts),
            row.expires_ts.map_or(Value::Null, Value::BigInt),
            Value::BigInt(link_id),
        ];
        let updated = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, update_sql, &update_params).await)
        );
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        usize::try_from(updated).map_or_else(
            |_| {
                Outcome::Err(DbError::invalid(
                    "row_count",
                    "row count exceeds usize::MAX",
                ))
            },
            |v| Outcome::Ok((v, row)),
        )
    })
    .await
}

/// List contacts for an agent
///
/// Returns (outgoing, incoming) contact links.
pub async fn list_contacts(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
) -> Outcome<(Vec<AgentLinkRow>, Vec<AgentLinkRow>), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Outgoing: links where this agent is "a" side
    let outgoing_sql =
        format!("{AGENT_LINK_SELECT_COLUMNS_SQL} WHERE a_project_id = ? AND a_agent_id = ?");
    let outgoing_params = [Value::BigInt(project_id), Value::BigInt(agent_id)];
    let outgoing =
        match map_sql_outcome(traw_query(cx, &tracked, &outgoing_sql, &outgoing_params).await) {
            Outcome::Ok(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    match decode_agent_link_row(&row) {
                        Ok(link) => out.push(link),
                        Err(e) => return Outcome::Err(e),
                    }
                }
                Outcome::Ok(out)
            }
            Outcome::Err(e) => Outcome::Err(e),
            Outcome::Cancelled(r) => Outcome::Cancelled(r),
            Outcome::Panicked(p) => Outcome::Panicked(p),
        };

    let outgoing_rows = match outgoing {
        Outcome::Ok(rows) => rows,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    // Incoming: links where this agent is "b" side
    let incoming_sql =
        format!("{AGENT_LINK_SELECT_COLUMNS_SQL} WHERE b_project_id = ? AND b_agent_id = ?");
    let incoming_params = [Value::BigInt(project_id), Value::BigInt(agent_id)];
    let incoming =
        match map_sql_outcome(traw_query(cx, &tracked, &incoming_sql, &incoming_params).await) {
            Outcome::Ok(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    match decode_agent_link_row(&row) {
                        Ok(link) => out.push(link),
                        Err(e) => return Outcome::Err(e),
                    }
                }
                Outcome::Ok(out)
            }
            Outcome::Err(e) => Outcome::Err(e),
            Outcome::Cancelled(r) => Outcome::Cancelled(r),
            Outcome::Panicked(p) => Outcome::Panicked(p),
        };

    match incoming {
        Outcome::Ok(incoming_rows) => Outcome::Ok((outgoing_rows, incoming_rows)),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// List approved contact targets for a sender within a project.
pub async fn list_approved_contact_ids(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    candidate_ids: &[i64],
) -> Outcome<Vec<i64>, DbError> {
    if candidate_ids.is_empty() {
        return Outcome::Ok(vec![]);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut out: Vec<i64> = Vec::with_capacity(candidate_ids.len().min(MAX_IN_CLAUSE_ITEMS));
    for chunk in candidate_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let sql = approved_contact_sql(chunk.len());
        let now = now_micros();
        let mut params: Vec<Value> = Vec::with_capacity(chunk.len() + 4);
        params.push(Value::BigInt(project_id));
        params.push(Value::BigInt(sender_id));
        params.push(Value::BigInt(project_id));
        params.push(Value::BigInt(now));
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
        match rows_out {
            Outcome::Ok(rows) => {
                for row in rows {
                    let id: i64 = match row.get_named("b_agent_id") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(id);
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }
    out.sort_unstable();
    out.dedup();
    Outcome::Ok(out)
}

/// List recent contact counterpart IDs for a sender within a project.
pub async fn list_recent_contact_agent_ids(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    sender_id: i64,
    candidate_ids: &[i64],
    since_ts: i64,
) -> Outcome<Vec<i64>, DbError> {
    if candidate_ids.is_empty() {
        return Outcome::Ok(vec![]);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut out: Vec<i64> = Vec::with_capacity(candidate_ids.len().min(MAX_IN_CLAUSE_ITEMS));
    for chunk in candidate_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let sql = recent_contact_union_sql(chunk.len());
        let mut params: Vec<Value> = Vec::with_capacity((chunk.len() * 2) + 6);
        params.push(Value::BigInt(project_id));
        params.push(Value::BigInt(sender_id));
        params.push(Value::BigInt(since_ts));
        for id in chunk {
            params.push(Value::BigInt(*id));
        }
        params.push(Value::BigInt(project_id));
        params.push(Value::BigInt(sender_id));
        params.push(Value::BigInt(since_ts));
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
        match rows_out {
            Outcome::Ok(rows) => {
                for row in rows {
                    let id: i64 = match row.get_named("agent_id") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(id);
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }
    out.sort_unstable();
    out.dedup();
    Outcome::Ok(out)
}

/// Check if contact is allowed between two agents.
///
/// Returns true if there's a non-expired approved link, or if the target agent
/// has an `open` or `auto` contact policy.
pub async fn is_contact_allowed(
    cx: &Cx,
    pool: &DbPool,
    from_project_id: i64,
    from_agent_id: i64,
    to_project_id: i64,
    to_agent_id: i64,
) -> Outcome<bool, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Helper: check if an approved link is still valid (not expired).
    let link_is_valid = |link: &AgentLinkRow| -> bool { link.expires_ts.is_none_or(|ts| ts > now) };

    // Check if there's an approved link in either direction.
    let link_sql = format!(
        "{AGENT_LINK_SELECT_COLUMNS_SQL} \
         WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? AND b_agent_id = ? \
           AND status = 'approved' \
         LIMIT 1"
    );
    let link_params = [
        Value::BigInt(from_project_id),
        Value::BigInt(from_agent_id),
        Value::BigInt(to_project_id),
        Value::BigInt(to_agent_id),
    ];
    let link = match map_sql_outcome(traw_query(cx, &tracked, &link_sql, &link_params).await) {
        Outcome::Ok(rows) => {
            rows.first()
                .map_or(Outcome::Ok(None), |row| match decode_agent_link_row(row) {
                    Ok(link) => Outcome::Ok(Some(link)),
                    Err(e) => Outcome::Err(e),
                })
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    };

    match link {
        Outcome::Ok(Some(ref row)) if link_is_valid(row) => return Outcome::Ok(true),
        Outcome::Ok(_) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // Check reverse direction
    let reverse_params = [
        Value::BigInt(to_project_id),
        Value::BigInt(to_agent_id),
        Value::BigInt(from_project_id),
        Value::BigInt(from_agent_id),
    ];
    let reverse_link =
        match map_sql_outcome(traw_query(cx, &tracked, &link_sql, &reverse_params).await) {
            Outcome::Ok(rows) => {
                rows.first()
                    .map_or(Outcome::Ok(None), |row| match decode_agent_link_row(row) {
                        Ok(link) => Outcome::Ok(Some(link)),
                        Err(e) => Outcome::Err(e),
                    })
            }
            Outcome::Err(e) => Outcome::Err(e),
            Outcome::Cancelled(r) => Outcome::Cancelled(r),
            Outcome::Panicked(p) => Outcome::Panicked(p),
        };

    match reverse_link {
        Outcome::Ok(Some(ref row)) if link_is_valid(row) => return Outcome::Ok(true),
        Outcome::Ok(_) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // Check if target agent has "open" or "auto" contact policy (allows all contacts)
    // Use raw SQL to avoid ORM decoding issues
    let sql = "SELECT contact_policy FROM agents WHERE project_id = ? AND id = ? LIMIT 1";
    let params = [Value::BigInt(to_project_id), Value::BigInt(to_agent_id)];
    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            let policy = rows
                .first()
                .and_then(|r| r.get(0))
                .and_then(|v| match v {
                    Value::Text(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            Outcome::Ok(matches!(policy, "auto" | "open"))
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// Product Queries
// =============================================================================

/// Ensure product exists, creating if necessary.
///
/// Note: Uses raw SQL with explicit columns instead of select!() macro due to
/// frankensqlite ORM limitation with SELECT * column name inference.
pub async fn ensure_product(
    cx: &Cx,
    pool: &DbPool,
    product_uid: Option<&str>,
    name: Option<&str>,
) -> Outcome<ProductRow, DbError> {
    let now = now_micros();
    let uid = product_uid.map_or_else(|| format!("prod_{now}"), String::from);
    let prod_name = name.map_or_else(|| uid.clone(), String::from);

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Use explicit column listing to work around frankensqlite SELECT * issue
    let select_sql =
        "SELECT id, product_uid, name, created_at FROM products WHERE product_uid = ? LIMIT 1";
    let select_params = [Value::Text(uid.clone())];

    // Check if product already exists
    match map_sql_outcome(traw_query(cx, &tracked, select_sql, &select_params).await) {
        Outcome::Ok(rows) => {
            if let Some(r) = rows.first() {
                match decode_product_row_indexed(r) {
                    Ok(row) => return Outcome::Ok(row),
                    Err(e) => return Outcome::Err(e),
                }
            }

            // Product doesn't exist, create it.
            run_with_mvcc_retry(cx, "ensure_product", || async {
                try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);
                let insert_sql = "INSERT INTO products (product_uid, name, created_at) \
                                  VALUES (?, ?, ?) ON CONFLICT(product_uid) DO NOTHING";
                let insert_params = [
                    Value::Text(uid.clone()),
                    Value::Text(prod_name.clone()),
                    Value::BigInt(now),
                ];
                try_in_tx!(
                    cx,
                    &tracked,
                    map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
                );

                // Re-select by stable uid so callers always get the canonical row.
                let reselect_params = [Value::Text(uid.clone())];
                let rows = try_in_tx!(
                    cx,
                    &tracked,
                    map_sql_outcome(traw_query(cx, &tracked, select_sql, &reselect_params).await)
                );
                let Some(found) = rows.first() else {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(DbError::Internal(format!(
                        "product insert/upsert succeeded but re-select failed for uid={uid}"
                    )));
                };
                let fresh = match decode_product_row_indexed(found) {
                    Ok(row) => row,
                    Err(err) => {
                        rollback_tx(cx, &tracked).await;
                        return Outcome::Err(err);
                    }
                };
                try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
                Outcome::Ok(fresh)
            })
            .await
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Link product to projects (creates `product_project_links`).
pub async fn link_product_to_projects(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
    project_ids: &[i64],
) -> Outcome<usize, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    run_with_mvcc_retry(cx, "link_product_to_projects", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let mut linked = 0usize;
        let now = now_micros();
        for &project_id in project_ids {
            // Use INSERT OR IGNORE to handle duplicates gracefully
            let sql = "INSERT OR IGNORE INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)";
            let params = [
                Value::BigInt(product_id),
                Value::BigInt(project_id),
                Value::BigInt(now),
            ];
            let n = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
            );
            if n > 0 {
                linked += 1;
            }
        }

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);

        Outcome::Ok(linked)
    })
    .await
}

/// Get product by UID.
///
/// Note: Uses raw SQL with explicit columns instead of select!() macro due to
/// frankensqlite ORM limitation with SELECT * column name inference.
pub async fn get_product_by_uid(
    cx: &Cx,
    pool: &DbPool,
    product_uid: &str,
) -> Outcome<ProductRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let select_sql =
        "SELECT id, product_uid, name, created_at FROM products WHERE product_uid = ? LIMIT 1";
    let select_params = [Value::Text(product_uid.to_string())];

    match map_sql_outcome(traw_query(cx, &tracked, select_sql, &select_params).await) {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Product", product_uid)),
            |r| match decode_product_row_indexed(r) {
                Ok(row) => Outcome::Ok(row),
                Err(e) => Outcome::Err(e),
            },
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

fn parse_unknown_product_placeholder(key: &str) -> Option<i64> {
    key.strip_prefix("[unknown-product-")?
        .strip_suffix(']')?
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
}

/// Get product by key (product_uid, name, or orphaned placeholder id).
pub async fn get_product_by_key(cx: &Cx, pool: &DbPool, key: &str) -> Outcome<ProductRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let select_sql = "SELECT id, product_uid, name, created_at \
                      FROM products WHERE product_uid = ? OR name = ? LIMIT 1";
    let select_params = [Value::Text(key.to_string()), Value::Text(key.to_string())];

    match map_sql_outcome(traw_query(cx, &tracked, select_sql, &select_params).await) {
        Outcome::Ok(rows) => {
            if let Some(row) = rows.first() {
                return match decode_product_row_indexed(row) {
                    Ok(row) => Outcome::Ok(row),
                    Err(e) => Outcome::Err(e),
                };
            }

            let Some(product_id) = parse_unknown_product_placeholder(key) else {
                return Outcome::Err(DbError::not_found("Product", key));
            };

            let placeholder_sql = "SELECT CAST(COALESCE(MIN(created_at), 0) AS INTEGER) AS created_at \
                                   FROM product_project_links WHERE product_id = ?";
            let placeholder_params = [Value::BigInt(product_id)];

            match map_sql_outcome(
                traw_query(cx, &tracked, placeholder_sql, &placeholder_params).await,
            ) {
                Outcome::Ok(rows) => {
                    let created_at = rows
                        .first()
                        .and_then(|row| row.get_named::<i64>("created_at").ok());
                    match created_at.filter(|ts| *ts > 0) {
                        Some(created_at) => Outcome::Ok(ProductRow {
                            id: Some(product_id),
                            product_uid: key.to_string(),
                            name: key.to_string(),
                            created_at,
                        }),
                        None => Outcome::Err(DbError::not_found("Product", key)),
                    }
                }
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Force-release a single file reservation by ID regardless of owner.
///
/// If `expected_expires_ts` is provided, the release is only performed if the
/// current `expires_ts` matches exactly (prevents concurrent update races).
///
/// Returns the number of rows affected (0 if already released, not found, or mismatch).
pub async fn force_release_reservation(
    cx: &Cx,
    pool: &DbPool,
    reservation_id: i64,
    expected_expires_ts: Option<i64>,
) -> Outcome<usize, DbError> {
    let expiry_constraint = expected_expires_ts
        .map_or(ReleaseReservationExpiryConstraint::Any, |expires_ts| {
            ReleaseReservationExpiryConstraint::Exact(expires_ts)
        });
    match release_reservations_by_ids_with_expiry_constraint(
        cx,
        pool,
        &[reservation_id],
        expiry_constraint,
    )
    .await
    {
        Outcome::Ok(released) => Outcome::Ok(released.len()),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get the most recent mail activity timestamp for an agent.
///
/// Checks:
/// - Messages sent by the agent (`created_ts`)
/// - Messages acknowledged by the agent (`ack_ts`)
/// - Messages read by the agent (`read_ts`)
///
/// Returns the maximum of all these timestamps, or `None` if no activity found.
pub async fn get_agent_last_mail_activity(
    cx: &Cx,
    pool: &DbPool,
    agent_id: i64,
    project_id: i64,
) -> Outcome<Option<i64>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Check messages sent, read, and acked in a single combined query to reduce round-trips
    let sql = "
        SELECT MAX(latest_ts) FROM (
            SELECT MAX(created_ts) as latest_ts FROM messages WHERE sender_id = ? AND project_id = ?
            UNION ALL
            SELECT MAX(MAX(COALESCE(r.read_ts, 0)), MAX(COALESCE(r.ack_ts, 0))) as latest_ts
            FROM message_recipients r
            JOIN messages m ON m.id = r.message_id
            WHERE r.agent_id = ? AND m.project_id = ?
        )
    ";
    let params = [
        Value::BigInt(agent_id),
        Value::BigInt(project_id),
        Value::BigInt(agent_id),
        Value::BigInt(project_id),
    ];
    let max_ts = match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => rows.first().and_then(|r| {
            r.get(0).and_then(|v| match v {
                Value::BigInt(n) if *n > 0 => Some(*n),
                Value::Int(n) if *n > 0 => Some(i64::from(*n)),
                _ => None,
            })
        }),
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    Outcome::Ok(max_ts)
}

pub async fn list_product_projects(
    cx: &Cx,
    pool: &DbPool,
    product_id: i64,
) -> Outcome<Vec<ProjectRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT ppl.project_id AS id, \
                      COALESCE(NULLIF(TRIM(p.slug), ''), '[unknown-project-' || ppl.project_id || ']') AS slug, \
                      COALESCE(NULLIF(TRIM(p.human_key), ''), '[unknown-project-' || ppl.project_id || ']') AS human_key, \
                      CAST(COALESCE(p.created_at, ppl.created_at) AS INTEGER) AS created_at \
               FROM product_project_links ppl \
               LEFT JOIN projects p ON p.id = ppl.project_id \
               WHERE ppl.product_id = ?";
    let params = [Value::BigInt(product_id)];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                match decode_project_row(r) {
                    Ok(row) => out.push(row),
                    Err(e) => return Outcome::Err(e),
                }
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// File Reservation Cleanup Queries
// =============================================================================

/// List distinct project IDs that have unreleased file reservations.
///
/// Used by the cleanup worker to iterate only active projects.
pub async fn project_ids_with_active_reservations(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<Vec<i64>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = format!(
        "SELECT DISTINCT project_id FROM file_reservations WHERE ({ACTIVE_RESERVATION_PREDICATE})"
    );
    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &[]).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                if let Ok(pid) = row.get_named::<i64>("project_id") {
                    out.push(pid);
                }
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Bulk-release all expired file reservations for a project.
///
/// Returns the IDs of expired reservations and marks them released.
pub async fn release_expired_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
) -> Outcome<Vec<i64>, DbError> {
    let now = now_micros();
    let ids = {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let tracked = tracked(&*conn);

        let select_sql = format!(
            "SELECT id FROM file_reservations \
             WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts <= ?"
        );
        let params = [Value::BigInt(project_id), Value::BigInt(now)];
        let rows = match map_sql_outcome(traw_query(cx, &tracked, &select_sql, &params).await) {
            Outcome::Ok(rows) => rows,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            if let Ok(id) = row.get_named::<i64>("id") {
                ids.push(id);
            }
        }
        ids
    };

    if ids.is_empty() {
        return Outcome::Ok(ids);
    }

    match release_reservations_by_ids_matching_expiry(cx, pool, &ids, Some(now)).await {
        Outcome::Ok(markers) => Outcome::Ok(markers.into_iter().map(|marker| marker.id).collect()),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Retention sweep: hard-`DELETE` released/expired file reservations whose
/// release/expiry is older than `older_than_us` (GH#154 item 2).
///
/// Reservations are only ever *logically* released (a `file_reservation_releases`
/// ledger row + `released_ts`), never physically removed by the normal lifecycle,
/// so `file_reservations` grows without bound on a long-lived mailbox (one report
/// observed ~30,000 rows over ~55 days with 0 active). That unbounded growth
/// directly inflates the cost of every active-reservation scan that embeds
/// [`ACTIVE_RESERVATION_PREDICATE`]. This sweep bounds the table.
///
/// A row is eligible when BOTH:
///   1. it is NOT logically active under [`ACTIVE_RESERVATION_PREDICATE`]
///      (i.e. it is released — has a ledger row, or a positive `released_ts`),
///      AND
///   2. its newest "settled" timestamp — `MAX(ledger.released_ts,
///      file_reservations.released_ts, expires_ts)` — is `<= older_than_us`.
///
/// The git archive (`projects/<slug>/file_reservations/<id>.json`) retains the
/// full audit history independently, so the DB delete is non-destructive to the
/// durable record. Matching `file_reservation_releases` rows are removed in the
/// same pass so the sidecar ledger cannot leak orphans. Returns the number of
/// `file_reservations` rows deleted.
///
/// When `project_id` is `Some`, the sweep is scoped to that project; `None`
/// sweeps across all projects. Deletes are batched by id to keep statements
/// bounded.
pub async fn prune_released_file_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: Option<i64>,
    older_than_us: i64,
) -> Outcome<u64, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    // Identify eligible reservation ids. We do NOT trust a correlated DELETE …
    // WHERE NOT (active predicate); instead select ids first (mirrors
    // release_expired_reservations) so the delete is a simple `id IN (...)`.
    // The `file_reservations` table is intentionally NOT aliased here so the
    // shared `ACTIVE_RESERVATION_PREDICATE` (which fully-qualifies
    // `file_reservations.released_ts`) resolves unchanged.
    let mut select_sql = String::from(
        "SELECT file_reservations.id AS id FROM file_reservations \
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = file_reservations.id \
         WHERE NOT (",
    );
    select_sql.push_str(ACTIVE_RESERVATION_PREDICATE);
    select_sql.push_str(
        ") AND MAX(COALESCE(rr.released_ts, 0), \
                   CASE WHEN typeof(file_reservations.released_ts) IN ('integer','real') \
                        THEN file_reservations.released_ts ELSE 0 END, \
                   file_reservations.expires_ts) <= ?",
    );
    let mut params: Vec<Value> = Vec::with_capacity(2);
    if let Some(pid) = project_id {
        select_sql.push_str(" AND file_reservations.project_id = ?");
        params.push(Value::BigInt(older_than_us));
        params.push(Value::BigInt(pid));
    } else {
        params.push(Value::BigInt(older_than_us));
    }

    let rows = match map_sql_outcome(traw_query(cx, &tracked, &select_sql, &params).await) {
        Outcome::Ok(rows) => rows,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let mut ids: Vec<i64> = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Ok(id) = row.get_named::<i64>("id") {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Outcome::Ok(0);
    }

    let mut deleted: u64 = 0;
    for chunk in ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let chunk_params: Vec<Value> = chunk.iter().map(|id| Value::BigInt(*id)).collect();

        // Delete the sidecar ledger rows first (no FK cascade is defined from
        // file_reservation_releases → file_reservations), then the reservations.
        let del_ledger = format!(
            "DELETE FROM file_reservation_releases WHERE reservation_id IN ({placeholders})"
        );
        match map_sql_outcome(traw_execute(cx, &tracked, &del_ledger, &chunk_params).await) {
            Outcome::Ok(_) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }

        let del_res = format!("DELETE FROM file_reservations WHERE id IN ({placeholders})");
        match map_sql_outcome(traw_execute(cx, &tracked, &del_res, &chunk_params).await) {
            Outcome::Ok(affected) => deleted = deleted.saturating_add(affected),
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

    Outcome::Ok(deleted)
}

/// Fetch specific file reservations by their IDs.
///
/// Used by the cleanup worker to retrieve details of released reservations
/// so that updated archive artifacts (with `released_ts`) can be written.
pub async fn get_reservations_by_ids(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
) -> Outcome<Vec<FileReservationRow>, DbError> {
    if ids.is_empty() {
        return Outcome::Ok(vec![]);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut out = Vec::with_capacity(ids.len());

    for chunk in ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, \
                    created_ts, expires_ts, COALESCE(rr.released_ts, fr.released_ts) AS released_ts \
             FROM file_reservations fr \
             LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
             WHERE fr.id IN ({placeholders})"
        );

        let mut params = Vec::with_capacity(chunk.len());
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
                for r in &rows {
                    match decode_file_reservation_row(r) {
                        Ok(row) => out.push(row),
                        Err(e) => return Outcome::Err(e),
                    }
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }
    Outcome::Ok(out)
}

/// Release specific file reservations by their IDs.
///
/// Marks all given IDs as released in the sidecar release ledger when they are
/// still logically active under [`ACTIVE_RESERVATION_PREDICATE`].
/// Returns the IDs newly marked released, in input order.
pub async fn release_reservations_by_ids_returning_ids(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
) -> Outcome<Vec<i64>, DbError> {
    match release_reservations_by_ids_matching_expiry(cx, pool, ids, None).await {
        Outcome::Ok(markers) => Outcome::Ok(markers.into_iter().map(|marker| marker.id).collect()),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Release specific file reservations by their IDs.
///
/// Marks all given IDs as released in the sidecar release ledger when they are
/// still logically active under [`ACTIVE_RESERVATION_PREDICATE`].
/// Returns the number of reservations newly marked released.
pub async fn release_reservations_by_ids(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
) -> Outcome<usize, DbError> {
    match release_reservations_by_ids_returning_ids(cx, pool, ids).await {
        Outcome::Ok(released_ids) => Outcome::Ok(released_ids.len()),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// ACK TTL Worker Queries
// =============================================================================

/// Row returned by [`list_unacknowledged_messages`].
#[derive(Debug)]
pub struct UnackedMessageRow {
    pub message_id: i64,
    pub project_id: i64,
    pub created_ts: i64,
    pub agent_id: i64,
}

/// Decode raw SQL rows into [`UnackedMessageRow`]s, logging a warning for any
/// row with an unexpected column type rather than silently skipping it.
fn decode_unacked_rows(rows: &[sqlmodel_core::Row], caller: &str) -> Vec<UnackedMessageRow> {
    let mut out = Vec::with_capacity(rows.len());
    for (row_idx, r) in rows.iter().enumerate() {
        let mid = match r.get_by_name("id") {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => i64::from(*n),
            _ => {
                tracing::warn!("{caller}: skipping row {row_idx}: unexpected type for m.id");
                continue;
            }
        };
        let pid = match r.get_by_name("project_id") {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => i64::from(*n),
            _ => {
                tracing::warn!(
                    "{caller}: skipping row {row_idx}: unexpected type for m.project_id"
                );
                continue;
            }
        };
        let cts = match r.get_by_name("created_ts") {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => i64::from(*n),
            _ => {
                tracing::warn!(
                    "{caller}: skipping row {row_idx}: unexpected type for m.created_ts"
                );
                continue;
            }
        };
        let aid = match r.get_by_name("agent_id") {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => i64::from(*n),
            _ => {
                tracing::warn!("{caller}: skipping row {row_idx}: unexpected type for mr.agent_id");
                continue;
            }
        };
        out.push(UnackedMessageRow {
            message_id: mid,
            project_id: pid,
            created_ts: cts,
            agent_id: aid,
        });
    }
    out
}

/// List all messages with `ack_required = 1` that have at least one recipient
/// who has not acknowledged (`ack_ts IS NULL`).
///
/// Returns one row per (message, unacked recipient) pair.
pub async fn list_unacknowledged_messages(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<Vec<UnackedMessageRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = "SELECT m.id, m.project_id, m.created_ts, mr.agent_id \
               FROM messages m \
               JOIN message_recipients mr ON mr.message_id = m.id \
               WHERE m.ack_required = 1 AND mr.ack_ts IS NULL \
               LIMIT 10000";

    match map_sql_outcome(traw_query(cx, &tracked, sql, &[]).await) {
        Outcome::Ok(rows) => {
            Outcome::Ok(decode_unacked_rows(&rows, "list_unacknowledged_messages"))
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// List overdue unacknowledged message-recipient pairs.
///
/// Returns rows where:
/// - `ack_required = 1`
/// - recipient `ack_ts IS NULL`
/// - message `created_ts <= overdue_before_ts`
///
/// `overdue_before_ts` is an absolute microsecond timestamp threshold.
pub async fn list_overdue_unacknowledged_messages(
    cx: &Cx,
    pool: &DbPool,
    overdue_before_ts: i64,
) -> Outcome<Vec<UnackedMessageRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let sql = "SELECT m.id, m.project_id, m.created_ts, mr.agent_id \
               FROM messages m \
               JOIN message_recipients mr ON mr.message_id = m.id \
               WHERE m.ack_required = 1 \
                 AND mr.ack_ts IS NULL \
                 AND m.created_ts <= ? \
               LIMIT 10000";
    let params = [Value::BigInt(overdue_before_ts)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => Outcome::Ok(decode_unacked_rows(
            &rows,
            "list_overdue_unacknowledged_messages",
        )),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Row returned by [`fetch_unacked_for_agent`].
#[derive(Debug, Clone)]
pub struct UnackedInboxRow {
    pub message: MessageRow,
    pub kind: String,
    pub sender_name: String,
    pub read_ts: Option<i64>,
}

/// Fetch ack-required messages for a specific agent that have NOT been acknowledged.
///
/// Returns messages ordered by `created_ts` ascending (oldest first), limited to
/// `limit` rows. Each row includes the recipient `read_ts` so callers can report
/// whether the message was at least read even if not acked.
#[allow(clippy::too_many_lines)]
pub async fn fetch_unacked_for_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    limit: usize,
) -> Outcome<Vec<UnackedInboxRow>, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };

    let sql = format!(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                  m.importance, m.ack_required, m.created_ts, m.recipients_json, \
                  m.attachments, \
                  r.kind, COALESCE(s.name, '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, r.read_ts \
           FROM message_recipients r \
           JOIN messages m ON m.id = r.message_id \
           LEFT JOIN agents s ON s.id = m.sender_id \
           WHERE r.agent_id = ? AND m.project_id = ? \
             AND m.ack_required = 1 AND r.ack_ts IS NULL \
           ORDER BY m.created_ts ASC \
           LIMIT ?"
    );

    let params: Vec<Value> = vec![
        Value::BigInt(agent_id),
        Value::BigInt(project_id),
        Value::BigInt(limit_i64),
    ];

    match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_as(0) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let proj_id: i64 = match row.get_as(1) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_id: i64 = match row.get_as(2) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let thread_id: Option<String> = match row.get_as(3) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let subject: String = match row.get_as(4) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let body_md: String = match row.get_as(5) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let importance: String = match row.get_as(6) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let ack_required: i64 = match row.get_as(7) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let created_ts: i64 = match row.get_as(8) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let recipients_json: String = match row.get_as(9) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let attachments: String = match row.get_as(10) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let kind: String = match row.get_as(11) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_name: String = match row.get_as(12) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let read_ts: Option<i64> = match row.get_as(13) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };

                out.push(UnackedInboxRow {
                    message: MessageRow {
                        id: Some(id),
                        project_id: proj_id,
                        sender_id,
                        thread_id,
                        subject,
                        body_md,
                        importance,
                        ack_required,
                        created_ts,
                        recipients_json,
                        attachments,
                    },
                    kind,
                    sender_name,
                    read_ts,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Insert a raw agent row without name validation (for ops/system agents).
///
/// Used by the ACK TTL escalation worker to auto-create holder agents.
pub async fn insert_system_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
    program: &str,
    model: &str,
    task_description: &str,
) -> Outcome<AgentRow, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let found = match run_with_mvcc_retry(cx, "insert_system_agent", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let insert_sql = "INSERT INTO agents \
            (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt) \
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
            ON CONFLICT(project_id, name) DO NOTHING";
        let insert_params = [
            Value::BigInt(project_id),
            Value::Text(name.to_string()),
            Value::Text(program.to_string()),
            Value::Text(model.to_string()),
            Value::Text(task_description.to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
            Value::Text("auto".to_string()),
            Value::Text("auto".to_string()),
            Value::BigInt(0),
        ];
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
        );

        let select_sql = "SELECT id, project_id, name, program, model, task_description, \
                          inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                          registration_token \
                          FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
                          ORDER BY id ASC LIMIT 1";
        let select_params = [Value::BigInt(project_id), Value::Text(name.to_string())];
        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, select_sql, &select_params).await)
        );
        let Some(found) = rows.first().map(decode_agent_row_indexed) else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::Internal(format!(
                "system agent insert/upsert succeeded but re-select failed for {project_id}:{name}"
            )));
        };

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(found)
    })
    .await
    {
        Outcome::Ok(found) => found,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &found);
    Outcome::Ok(found)
}

/// Append an ATC experience row to durable storage, returning the stored row.
///
/// This path is idempotent across repeated signals for the same
/// `(decision_id, effect_id)` pair. The first insert wins and subsequent
/// appends re-select the existing row without mutation.
fn validate_experience_row_for_insert(row: &ExperienceRow) -> std::result::Result<(), DbError> {
    let issues = row.validate();
    if issues.is_empty() {
        return Ok(());
    }
    Err(DbError::invalid("experience_row", issues.join("; ")))
}

fn experience_feature_vector_size(row: &ExperienceRow) -> usize {
    row.features
        .as_ref()
        .map_or(0, |_| std::mem::size_of::<FeatureVector>())
        .saturating_add(
            row.feature_ext
                .as_ref()
                .map_or(0, FeatureExtension::estimated_size),
        )
}

/// Insert a validated ATC experience row and return the assigned durable ID.
///
/// This is the write-facing append API for ATC experience records. The insert
/// is lifecycle-validated before it reaches SQLite, and repeated inserts for
/// the same `(decision_id, effect_id)` pair return the originally assigned
/// durable `experience_id`.
pub async fn insert_experience(
    cx: &Cx,
    pool: &DbPool,
    row: ExperienceRow,
) -> Outcome<u64, DbError> {
    let requested_experience_id = row.experience_id;
    let state = row.state;
    let subsystem = row.subsystem;
    let effect_kind = row.effect_kind;
    let feature_vector_size = experience_feature_vector_size(&row);
    let project_key = row.project_key.clone().unwrap_or_default();

    if let Err(error) = validate_experience_row_for_insert(&row) {
        tracing::warn!(
            requested_experience_id,
            state = %state,
            subsystem = %subsystem,
            effect_kind = %effect_kind,
            feature_vector_size,
            project_key,
            error = %error,
            "atc.db.insert_experience rejected invalid row"
        );
        return Outcome::Err(error);
    }

    let started = std::time::Instant::now();
    match append_atc_experience(cx, pool, &row).await {
        Outcome::Ok(stored) => {
            tracing::debug!(
                experience_id = stored.experience_id,
                requested_experience_id,
                state = %state,
                subsystem = %subsystem,
                effect_kind = %effect_kind,
                feature_vector_size,
                project_key = stored.project_key.as_deref().unwrap_or(""),
                insert_latency_micros = started.elapsed().as_micros(),
                duration_micros = ?stored.resolution_latency_micros(),
                "atc.db.insert_experience"
            );
            Outcome::Ok(stored.experience_id)
        }
        Outcome::Err(error) => {
            tracing::warn!(
                requested_experience_id,
                state = %state,
                subsystem = %subsystem,
                effect_kind = %effect_kind,
                feature_vector_size,
                project_key,
                error = %error,
                "atc.db.insert_experience failed"
            );
            Outcome::Err(error)
        }
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

pub async fn append_atc_experience(
    cx: &Cx,
    pool: &DbPool,
    row: &ExperienceRow,
) -> Outcome<ExperienceRow, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match append_atc_experience_file_backed(pool, row) {
            Ok(value) => Outcome::Ok(value),
            Err(error) => Outcome::Err(error),
        };
    }

    fn sql_i64_id(field: &str, value: u64) -> std::result::Result<i64, DbError> {
        i64::try_from(value).map_err(|_| {
            DbError::Internal(format!("{field} exceeds SQLite INTEGER range: {value}"))
        })
    }

    let decision_id = match sql_i64_id("decision_id", row.decision_id) {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };
    let effect_id = match sql_i64_id("effect_id", row.effect_id) {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };
    let feature_payload = match migrate_feature_payload(
        infer_feature_schema_version(row.features.as_ref(), row.feature_ext.as_ref()),
        row.features,
        row.feature_ext.clone(),
    ) {
        Ok(value) => value,
        Err(error) => {
            return Outcome::Err(DbError::invalid(
                "feature_schema_version",
                error.to_string(),
            ));
        }
    };
    let posterior_json = match encode_json(&row.posterior, "posterior_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };
    let non_execution_json =
        match encode_json_optional(&row.non_execution_reason, "non_execution_json") {
            Ok(value) => value,
            Err(error) => return Outcome::Err(error),
        };
    let outcome_json = match encode_json_optional(&row.outcome, "outcome_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };
    let features_json = match encode_json_optional(&feature_payload.features, "features_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };
    let feature_ext_json =
        match encode_json_optional(&feature_payload.feature_ext, "feature_ext_json") {
            Ok(value) => value,
            Err(error) => return Outcome::Err(error),
        };
    let context_json = match encode_json_optional(&row.context, "context_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };

    {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let tracked = tracked(&*conn);
        match run_with_mvcc_retry(cx, "append_atc_experience", || async {
            try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

            let insert_sql = "INSERT INTO atc_experiences \
                (decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem, decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json, expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy, safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json, feature_schema_version, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                ON CONFLICT(decision_id, effect_id) DO NOTHING";
            let insert_params = [
                Value::BigInt(decision_id),
                Value::BigInt(effect_id),
                Value::Text(row.trace_id.clone()),
                Value::Text(row.claim_id.clone()),
                Value::Text(row.evidence_id.clone()),
                Value::Text(row.state.to_string()),
                Value::Text(row.subsystem.to_string()),
                Value::Text(row.decision_class.clone()),
                Value::Text(row.subject.clone()),
                row.project_key
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                row.policy_id
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                Value::Text(row.effect_kind.to_string()),
                Value::Text(row.action.clone()),
                Value::Text(posterior_json.clone()),
                Value::Double(row.expected_loss),
                row.runner_up_action
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                row.runner_up_loss
                    .map_or(Value::Null, Value::Double),
                Value::Text(row.evidence_summary.clone()),
                Value::BigInt(i64::from(row.calibration_healthy)),
                Value::BigInt(i64::from(row.safe_mode_active)),
                non_execution_json
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                outcome_json
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                features_json
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                feature_ext_json
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
                Value::BigInt(i64::from(feature_payload.schema_version)),
                Value::BigInt(row.created_ts_micros),
                row.dispatched_ts_micros
                    .map_or(Value::Null, Value::BigInt),
                row.executed_ts_micros
                    .map_or(Value::Null, Value::BigInt),
                row.resolved_ts_micros
                    .map_or(Value::Null, Value::BigInt),
                context_json
                    .as_ref()
                    .map_or(Value::Null, |value| Value::Text(value.clone())),
            ];
            try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
            );

            let select_sql = format!(
                "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE decision_id = ? AND effect_id = ? LIMIT 1"
            );
            let select_params = [Value::BigInt(decision_id), Value::BigInt(effect_id)];
            let rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, &select_sql, &select_params).await)
            );
            let Some(stored) = rows.first() else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(format!(
                    "ATC experience append succeeded but re-select failed for decision_id={decision_id} effect_id={effect_id}"
                )));
            };
            if let Err(error) = decode_atc_experience_row(stored) {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(error);
            }

            try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
            Outcome::Ok(())
        })
        .await
        {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
    }

    fetch_durable_atc_experience_by_decision_effect(cx, pool, decision_id, effect_id).await
}

async fn transition_atc_experience_tx(
    cx: &Cx,
    tracked: &TrackedConnection<'_>,
    experience_id: u64,
    sql_experience_id: i64,
    new_state: ExperienceState,
    ts_micros: i64,
    non_execution_reason: Option<&NonExecutionReason>,
    context_patch: Option<&serde_json::Value>,
) -> Outcome<(), DbError> {
    try_in_tx!(cx, tracked, begin_concurrent_tx(cx, tracked).await);

    let select_sql = format!("{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE experience_id = ? LIMIT 1");
    let rows = try_in_tx!(
        cx,
        tracked,
        map_sql_outcome(
            traw_query(
                cx,
                tracked,
                &select_sql,
                &[Value::BigInt(sql_experience_id)]
            )
            .await
        )
    );
    let Some(stored) = rows.first() else {
        rollback_tx(cx, tracked).await;
        return Outcome::Err(DbError::not_found(
            "AtcExperience",
            experience_id.to_string(),
        ));
    };
    let mut experience = match decode_atc_experience_row(stored) {
        Ok(value) => value,
        Err(error) => {
            rollback_tx(cx, tracked).await;
            return Outcome::Err(error);
        }
    };

    if experience.state != new_state
        && let Err(reason) = validate_transition(experience.state, new_state)
    {
        rollback_tx(cx, tracked).await;
        return Outcome::Err(DbError::invalid(
            "state",
            format!(
                "experience {experience_id} cannot transition from {} to {}: {reason}",
                experience.state, new_state
            ),
        ));
    }

    if non_execution_reason.is_some() && !new_state.is_non_execution() {
        rollback_tx(cx, tracked).await;
        return Outcome::Err(DbError::invalid(
            "non_execution_reason",
            format!(
                "non_execution_reason is only valid for throttled/suppressed/skipped states, got {new_state}"
            ),
        ));
    }
    if new_state.is_non_execution()
        && experience.state != new_state
        && non_execution_reason.is_none()
    {
        rollback_tx(cx, tracked).await;
        return Outcome::Err(DbError::invalid(
            "non_execution_reason",
            format!("non_execution_reason is required when transitioning to {new_state}"),
        ));
    }

    if let Err(reason) = experience.transition_to(new_state) {
        rollback_tx(cx, tracked).await;
        return Outcome::Err(DbError::invalid(
            "state",
            format!("experience {experience_id} transition failed: {reason}"),
        ));
    }

    match new_state {
        ExperienceState::Planned => {}
        ExperienceState::Dispatched => {
            if experience.dispatched_ts_micros.is_none() {
                experience.dispatched_ts_micros = Some(ts_micros);
            }
        }
        ExperienceState::Executed
        | ExperienceState::Failed
        | ExperienceState::Throttled
        | ExperienceState::Suppressed
        | ExperienceState::Skipped => {
            if experience.executed_ts_micros.is_none() {
                experience.executed_ts_micros = Some(ts_micros);
            }
        }
        ExperienceState::Open => {}
        ExperienceState::Resolved | ExperienceState::Censored | ExperienceState::Expired => {
            if experience.resolved_ts_micros.is_none() {
                experience.resolved_ts_micros = Some(ts_micros);
            }
        }
    }

    if new_state.is_non_execution() {
        if let Some(reason) = non_execution_reason {
            experience.non_execution_reason = Some(reason.clone());
        }
    } else {
        experience.non_execution_reason = None;
    }
    experience.context = match merge_context_patch(experience.context.take(), context_patch) {
        Ok(value) => value,
        Err(error) => {
            rollback_tx(cx, tracked).await;
            return Outcome::Err(error);
        }
    };

    let non_exec_json =
        match encode_json_optional(&experience.non_execution_reason, "non_execution_json") {
            Ok(value) => value,
            Err(error) => {
                rollback_tx(cx, tracked).await;
                return Outcome::Err(error);
            }
        };
    let context_json = match encode_json_optional(&experience.context, "context_json") {
        Ok(value) => value,
        Err(error) => {
            rollback_tx(cx, tracked).await;
            return Outcome::Err(error);
        }
    };

    let update_sql = "UPDATE atc_experiences SET state = ?, non_execution_json = ?, dispatched_ts = ?, \
         executed_ts = ?, resolved_ts = ?, context_json = ? WHERE experience_id = ?";
    let update_params = [
        Value::Text(experience.state.to_string()),
        non_exec_json
            .as_ref()
            .map_or(Value::Null, |value| Value::Text(value.clone())),
        experience
            .dispatched_ts_micros
            .map_or(Value::Null, Value::BigInt),
        experience
            .executed_ts_micros
            .map_or(Value::Null, Value::BigInt),
        experience
            .resolved_ts_micros
            .map_or(Value::Null, Value::BigInt),
        context_json
            .as_ref()
            .map_or(Value::Null, |value| Value::Text(value.clone())),
        Value::BigInt(sql_experience_id),
    ];
    try_in_tx!(
        cx,
        tracked,
        map_sql_outcome(traw_execute(cx, tracked, update_sql, &update_params).await)
    );

    try_in_tx!(cx, tracked, commit_tx(cx, tracked).await);
    Outcome::Ok(())
}

/// Transition an ATC experience row to a new lifecycle state with optional
/// execution metadata.
///
/// This function atomically updates the experience state and the appropriate
/// timestamp column. The transition is validated by the `ExperienceState`
/// state machine: invalid transitions (e.g., `Resolved → Planned`) return
/// an error.
///
/// **Idempotent**: Same-state transitions succeed without mutation (the
/// state machine treats `(S, S)` as a no-op).
///
/// # Parameters
///
/// - `experience_id`: Which experience row to transition.
/// - `new_state`: Target lifecycle state.
/// - `ts_micros`: Timestamp of the transition (microseconds since epoch).
/// - `non_execution_reason`: If transitioning to Throttled/Suppressed/Skipped,
///   the structured reason for non-execution.
/// - `context_patch`: Optional JSON object merged into `context_json` so the
///   durable row retains bounded execution detail without overwriting the
///   original decision context.
pub async fn transition_atc_experience(
    cx: &Cx,
    pool: &DbPool,
    experience_id: u64,
    new_state: ExperienceState,
    ts_micros: i64,
    non_execution_reason: Option<&NonExecutionReason>,
    context_patch: Option<&serde_json::Value>,
) -> Outcome<(), DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match transition_atc_experience_file_backed(
            pool,
            experience_id,
            new_state,
            ts_micros,
            non_execution_reason,
            context_patch,
        ) {
            Ok(()) => Outcome::Ok(()),
            Err(error) => Outcome::Err(error),
        };
    }

    let sql_experience_id = match i64::try_from(experience_id) {
        Ok(value) => value,
        Err(_) => {
            return Outcome::Err(DbError::Internal(format!(
                "experience_id exceeds SQLite INTEGER range: {experience_id}"
            )));
        }
    };

    let pooled_outcome = {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let tracked = tracked(&*conn);
        run_with_mvcc_retry(cx, "transition_atc_experience", || async {
            transition_atc_experience_tx(
                cx,
                &tracked,
                experience_id,
                sql_experience_id,
                new_state,
                ts_micros,
                non_execution_reason,
                context_patch,
            )
            .await
        })
        .await
    };

    match pooled_outcome {
        Outcome::Err(DbError::NotFound {
            entity: "AtcExperience",
            ..
        }) if pool.sqlite_path() != ":memory:" => {
            match fetch_durable_atc_experience_by_id(cx, pool, sql_experience_id).await {
                Outcome::Ok(Some(_)) => {
                    tracing::warn!(
                        experience_id,
                        "pooled connection missed ATC experience visible from fresh connection; retrying transition on fresh direct handle"
                    );
                    run_with_mvcc_retry(cx, "transition_atc_experience_fresh_handle", || async {
                        let direct = match open_fresh_file_backed_conn(pool) {
                            Ok(conn) => conn,
                            Err(error) => return Outcome::Err(error),
                        };
                        let tracked = tracked(&direct);
                        transition_atc_experience_tx(
                            cx,
                            &tracked,
                            experience_id,
                            sql_experience_id,
                            new_state,
                            ts_micros,
                            non_execution_reason,
                            context_patch,
                        )
                        .await
                    })
                    .await
                }
                Outcome::Ok(None) => Outcome::Err(DbError::not_found(
                    "AtcExperience",
                    experience_id.to_string(),
                )),
                Outcome::Err(error) => Outcome::Err(error),
                Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
                Outcome::Panicked(payload) => Outcome::Panicked(payload),
            }
        }
        other => other,
    }
}

/// Fetch ATC experiences awaiting outcome resolution.
///
/// Returns experiences in `executed` or `open` state, ordered by creation
/// time (oldest first). These are candidates for outcome resolution based
/// on subsequent messaging signals (acks, later activity, thread
/// participation).
///
/// # Parameters
///
/// - `subject`: If provided, filters to experiences for a specific agent.
/// - `limit`: Maximum number of rows to return.
pub async fn fetch_open_atc_experiences(
    cx: &Cx,
    pool: &DbPool,
    subject: Option<&str>,
    limit: u32,
) -> Outcome<Vec<ExperienceRow>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match fetch_open_atc_experiences_file_backed(pool, subject, limit) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let (sql, params) = if let Some(agent) = subject {
        (
            format!(
                "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} \
                 WHERE state IN ('executed', 'open') AND subject = ? COLLATE NOCASE \
                 ORDER BY created_ts ASC LIMIT ?"
            ),
            vec![
                Value::Text(agent.to_string()),
                Value::BigInt(i64::from(limit)),
            ],
        )
    } else {
        (
            format!(
                "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} \
                 WHERE state IN ('executed', 'open') \
                 ORDER BY created_ts ASC LIMIT ?"
            ),
            vec![Value::BigInt(i64::from(limit))],
        )
    };

    let rows = match traw_query(cx, &tracked, &sql, &params).await {
        Outcome::Ok(r) => r,
        Outcome::Err(error) => {
            return Outcome::Err(DbError::Internal(format!(
                "failed to fetch open experiences: {error}"
            )));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut experiences = Vec::with_capacity(rows.len());
    for row in &rows {
        match decode_atc_experience_row(row) {
            Ok(exp) => experiences.push(exp),
            Err(error) => {
                tracing::warn!(%error, "skipping malformed atc_experience row");
            }
        }
    }

    Outcome::Ok(experiences)
}

/// Fetch the latest send-side ATC experience row for a message.
///
/// This is used by Seam 4.x hot-path resolution so `acknowledge_message`
/// and `mark_message_read` can resolve the single `message_sent` experience
/// generated by Seam 3.1. `message_received` rows intentionally remain
/// independent and are not returned by this helper.
pub async fn fetch_message_sent_atc_experience(
    cx: &Cx,
    pool: &DbPool,
    message_id: i64,
) -> Outcome<Option<ExperienceRow>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match fetch_message_sent_atc_experience_file_backed(pool, message_id) {
            Ok(row) => Outcome::Ok(row),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let sql = format!(
        "{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} \
         WHERE decision_class = 'message_sent' \
           AND CAST(json_extract(context_json, '$.message_id') AS INTEGER) = ? \
         ORDER BY experience_id DESC LIMIT 1"
    );
    let params = [Value::BigInt(message_id)];

    match traw_query(cx, &tracked, &sql, &params).await {
        Outcome::Ok(rows) => rows.first().map_or(Outcome::Ok(None), |row| {
            match decode_atc_experience_row(row) {
                Ok(decoded) => Outcome::Ok(Some(decoded)),
                Err(error) => Outcome::Err(error),
            }
        }),
        Outcome::Err(error) => Outcome::Err(DbError::Internal(format!(
            "failed to fetch message_sent experience for message_id={message_id}: {error}"
        ))),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

/// Resolve an ATC experience with an observed outcome.
///
/// Transitions the experience from `open` to `resolved` and stores the
/// outcome data. Callers that still hold an `executed` row must first
/// promote it to `open` via `transition_atc_experience`. This is the
/// terminal attribution step for message-driven experiences.
///
/// **Idempotent**: Resolving an already-resolved experience succeeds
/// without mutation.
pub async fn resolve_atc_experience(
    cx: &Cx,
    pool: &DbPool,
    experience_id: u64,
    outcome: &ExperienceOutcome,
) -> Outcome<(), DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match resolve_atc_experience_file_backed(pool, experience_id, outcome) {
            Ok(()) => Outcome::Ok(()),
            Err(error) => Outcome::Err(error),
        };
    }

    let id = match i64::try_from(experience_id) {
        Ok(value) => value,
        Err(_) => {
            return Outcome::Err(DbError::Internal(format!(
                "experience_id exceeds SQLite INTEGER range: {experience_id}"
            )));
        }
    };

    let outcome_json = match encode_json(outcome, "outcome_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Only update rows that are in `open` state. The state machine
    // requires Executed → Open before Open → Resolved. The caller
    // must ensure the Executed → Open transition happens first (e.g.,
    // via the resolution sweep). A zero-row update is treated as a
    // follow-up state read so stale callers cannot report false success
    // for missing or non-open rows.
    let sql = "UPDATE atc_experiences \
               SET state = 'resolved', resolved_ts = ?, outcome_json = ? \
               WHERE experience_id = ? AND state = 'open'";

    let params = vec![
        Value::BigInt(outcome.observed_ts_micros),
        Value::Text(outcome_json),
        Value::BigInt(id),
    ];

    match traw_execute(cx, &tracked, sql, &params).await {
        Outcome::Ok(rows_affected) => {
            if rows_affected > 0 {
                return Outcome::Ok(());
            }

            match traw_query(
                cx,
                &tracked,
                "SELECT state FROM atc_experiences WHERE experience_id = ? LIMIT 1",
                &[Value::BigInt(id)],
            )
            .await
            {
                Outcome::Ok(rows) => {
                    let state = match rows
                        .first()
                        .map(|row| {
                            row_text(row, 0, "state").and_then(|raw| parse_enum(raw, "state"))
                        })
                        .transpose()
                    {
                        Ok(state) => state,
                        Err(error) => return Outcome::Err(error),
                    };
                    match resolve_atc_experience_noop_result(state, experience_id) {
                        Ok(()) => Outcome::Ok(()),
                        Err(error) => Outcome::Err(error),
                    }
                }
                Outcome::Err(error) => Outcome::Err(DbError::Internal(format!(
                    "failed to inspect experience {experience_id} after resolve no-op: {error}"
                ))),
                Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
                Outcome::Panicked(payload) => Outcome::Panicked(payload),
            }
        }
        Outcome::Err(error) => Outcome::Err(DbError::Internal(format!(
            "failed to resolve experience {experience_id}: {error}"
        ))),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

/// Overwrite the outcome payload for an already-resolved ATC experience.
///
/// This is the narrow escape hatch Seam 4.1 needs for the `ack wins over read`
/// rule: a `mark_message_read` hot-path may resolve the send-side experience as
/// `read`, and a later `acknowledge_message` must upgrade that same resolved row
/// to `acknowledged` without reopening the lifecycle state machine.
pub async fn overwrite_resolved_atc_experience_outcome(
    cx: &Cx,
    pool: &DbPool,
    experience_id: u64,
    outcome: &ExperienceOutcome,
) -> Outcome<(), DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match overwrite_resolved_atc_experience_outcome_file_backed(
            pool,
            experience_id,
            outcome,
        ) {
            Ok(()) => Outcome::Ok(()),
            Err(error) => Outcome::Err(error),
        };
    }

    let id = match i64::try_from(experience_id) {
        Ok(value) => value,
        Err(_) => {
            return Outcome::Err(DbError::Internal(format!(
                "experience_id exceeds SQLite INTEGER range: {experience_id}"
            )));
        }
    };
    let outcome_json = match encode_json(outcome, "outcome_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    run_with_mvcc_retry(cx, "overwrite_resolved_atc_experience_outcome", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(
                traw_query(
                    cx,
                    &tracked,
                    "SELECT state FROM atc_experiences WHERE experience_id = ? LIMIT 1",
                    &[Value::BigInt(id)],
                )
                .await,
            )
        );
        let Some(row) = rows.first() else {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::not_found("experience", experience_id.to_string()));
        };
        let current_state: ExperienceState =
            match row_text(row, 0, "state").and_then(|raw| parse_enum(raw, "state")) {
                Ok(state) => state,
                Err(error) => {
                    rollback_tx(cx, &tracked).await;
                    return Outcome::Err(error);
                }
            };
        if current_state != ExperienceState::Resolved {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::invalid(
                "state",
                format!(
                    "experience {experience_id} is {current_state}, must be resolved to overwrite outcome"
                ),
            ));
        }

        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(
                traw_execute(
                    cx,
                    &tracked,
                    "UPDATE atc_experiences \
                     SET resolved_ts = ?, outcome_json = ? \
                     WHERE experience_id = ? AND state = 'resolved'",
                    &[
                        Value::BigInt(outcome.observed_ts_micros),
                        Value::Text(outcome_json.clone()),
                        Value::BigInt(id),
                    ],
                )
                .await,
            )
        );

        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        Outcome::Ok(())
    })
    .await
}

/// Resolve an ATC experience to any terminal resolution state.
///
/// Handles Open → {Resolved, Censored, Expired} transitions with:
///
/// - **Idempotent same-outcome**: If the experience is already in the
///   requested terminal state, returns `Ok(())` without mutation.
/// - **Conflict rejection**: If the experience is already in a *different*
///   terminal state, returns `InvalidArgument`.
/// - **Pre-condition check**: If the experience is not in `Open` state
///   (and not already terminal), returns `InvalidArgument`.
/// - **Not-found**: If the experience_id doesn't exist, returns `NotFound`.
pub async fn resolve_experience(
    cx: &Cx,
    pool: &DbPool,
    experience_id: u64,
    resolution: &ResolutionKind,
) -> Outcome<(), DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match resolve_experience_file_backed(pool, experience_id, resolution) {
            Ok(()) => Outcome::Ok(()),
            Err(error) => Outcome::Err(error),
        };
    }

    let id = match i64::try_from(experience_id) {
        Ok(value) => value,
        Err(_) => {
            return Outcome::Err(DbError::Internal(format!(
                "experience_id exceeds SQLite INTEGER range: {experience_id}"
            )));
        }
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let select_sql = "SELECT state FROM atc_experiences WHERE experience_id = ? LIMIT 1";
    let rows = match traw_query(cx, &tracked, select_sql, &[Value::BigInt(id)]).await {
        Outcome::Ok(r) => r,
        Outcome::Err(error) => {
            return Outcome::Err(DbError::Internal(format!(
                "resolve_experience select failed: {error}"
            )));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let row = match rows.first() {
        Some(r) => r,
        None => {
            return Outcome::Err(DbError::not_found("experience", experience_id.to_string()));
        }
    };

    let current_state: ExperienceState =
        match row_text(row, 0, "state").and_then(|raw| parse_enum(raw, "state")) {
            Ok(s) => s,
            Err(e) => return Outcome::Err(e),
        };

    let target = resolution.target_state();

    if current_state == target {
        return Outcome::Ok(());
    }
    if current_state.is_terminal() {
        return Outcome::Err(DbError::invalid(
            "state",
            format!(
                "experience {experience_id} already terminal as {current_state}, \
                 cannot re-resolve as {target}"
            ),
        ));
    }
    if current_state != ExperienceState::Open {
        return Outcome::Err(DbError::invalid(
            "state",
            format!("experience {experience_id} is {current_state}, must be open to resolve"),
        ));
    }

    let (sql, params) = match resolution {
        ResolutionKind::Resolved(outcome) => {
            let outcome_json = match encode_json(outcome, "outcome_json") {
                Ok(v) => v,
                Err(e) => return Outcome::Err(e),
            };
            (
                "UPDATE atc_experiences \
                 SET state = 'resolved', resolved_ts = ?, outcome_json = ? \
                 WHERE experience_id = ?",
                vec![
                    Value::BigInt(outcome.observed_ts_micros),
                    Value::Text(outcome_json),
                    Value::BigInt(id),
                ],
            )
        }
        ResolutionKind::Censored { ts_micros } => (
            "UPDATE atc_experiences \
             SET state = 'censored', resolved_ts = ? \
             WHERE experience_id = ?",
            vec![Value::BigInt(*ts_micros), Value::BigInt(id)],
        ),
        ResolutionKind::Expired { ts_micros } => (
            "UPDATE atc_experiences \
             SET state = 'expired', resolved_ts = ? \
             WHERE experience_id = ?",
            vec![Value::BigInt(*ts_micros), Value::BigInt(id)],
        ),
    };

    match traw_execute(cx, &tracked, sql, &params).await {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(error) => Outcome::Err(DbError::Internal(format!(
            "failed to resolve experience {experience_id}: {error}"
        ))),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

/// Update rollup statistics for an ATC experience stratum on resolution.
///
/// Increments the appropriate counters (resolved/censored/expired, correct/
/// incorrect, loss/regret aggregates, EWMA loss, delay stats) in the
/// `atc_experience_rollups` table. Creates the rollup row on first use
/// via upsert.
///
/// The EWMA uses lambda=0.95 (approximately 20-observation half-life).
///
/// **Must be called after each experience resolution** to keep rollup stats
/// current. This is the only write path for rollups — no batch recomputation.
pub async fn update_atc_experience_rollup(
    cx: &Cx,
    pool: &DbPool,
    stratum_key: &str,
    subsystem: &str,
    effect_kind: &str,
    risk_tier: i32,
    resolution_state: ExperienceState,
    correct: bool,
    actual_loss: f64,
    regret: f64,
    delay_micros: i64,
    ts_micros: i64,
) -> Outcome<(), DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match update_atc_experience_rollup_file_backed(
            pool,
            stratum_key,
            subsystem,
            effect_kind,
            risk_tier,
            resolution_state,
            correct,
            actual_loss,
            regret,
            delay_micros,
            ts_micros,
        ) {
            Ok(()) => Outcome::Ok(()),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // EWMA smoothing factor (lambda=0.95, half-life ~14 observations).
    const EWMA_LAMBDA: f64 = 0.95;

    // Determine which counter to increment.
    let (resolved_inc, censored_inc, expired_inc) = match resolution_state {
        ExperienceState::Resolved => (1, 0, 0),
        ExperienceState::Censored => (0, 1, 0),
        ExperienceState::Expired => (0, 0, 1),
        _ => (0, 0, 0),
    };

    let correct_inc: i64 = if correct { 1 } else { 0 };
    let incorrect_inc: i64 = if correct { 0 } else { 1 };

    // Upsert: create the row if it doesn't exist, then update counters.
    // The EWMA update formula: ewma = lambda * ewma + (1 - lambda) * new_value
    // Weight tracks effective sample size: weight = lambda * weight + 1
    let sql = "\
        INSERT INTO atc_experience_rollups \
            (stratum_key, subsystem, effect_kind, risk_tier, \
             total_count, resolved_count, censored_count, expired_count, \
             correct_count, incorrect_count, total_regret, total_loss, \
             ewma_loss, ewma_weight, delay_sum_micros, delay_count, delay_max_micros, \
             last_updated_ts) \
        VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, 1.0, ?, 1, ?, ?) \
        ON CONFLICT(stratum_key) DO UPDATE SET \
            total_count = total_count + 1, \
            resolved_count = resolved_count + ?, \
            censored_count = censored_count + ?, \
            expired_count = expired_count + ?, \
            correct_count = correct_count + ?, \
            incorrect_count = incorrect_count + ?, \
            total_regret = total_regret + ?, \
            total_loss = total_loss + ?, \
            ewma_loss = ? * ewma_loss + ? * ?, \
            ewma_weight = ? * ewma_weight + 1.0, \
            delay_sum_micros = delay_sum_micros + ?, \
            delay_count = delay_count + 1, \
            delay_max_micros = MAX(delay_max_micros, ?), \
            last_updated_ts = ?";

    let one_minus_lambda = 1.0 - EWMA_LAMBDA;

    let params = vec![
        // INSERT values
        Value::Text(stratum_key.to_string()),
        Value::Text(subsystem.to_string()),
        Value::Text(effect_kind.to_string()),
        Value::Int(risk_tier),
        Value::BigInt(resolved_inc),
        Value::BigInt(censored_inc),
        Value::BigInt(expired_inc),
        Value::BigInt(correct_inc),
        Value::BigInt(incorrect_inc),
        Value::Double(regret),
        Value::Double(actual_loss),
        Value::Double(actual_loss), // initial ewma_loss = first value
        Value::BigInt(delay_micros),
        Value::BigInt(delay_micros), // initial delay_max
        Value::BigInt(ts_micros),
        // ON CONFLICT UPDATE values
        Value::BigInt(resolved_inc),
        Value::BigInt(censored_inc),
        Value::BigInt(expired_inc),
        Value::BigInt(correct_inc),
        Value::BigInt(incorrect_inc),
        Value::Double(regret),
        Value::Double(actual_loss),
        Value::Double(EWMA_LAMBDA),      // ewma_loss = lambda * old
        Value::Double(one_minus_lambda), // + (1-lambda) * new
        Value::Double(actual_loss),      // actual loss value
        Value::Double(EWMA_LAMBDA),      // ewma_weight = lambda * old + 1
        Value::BigInt(delay_micros),
        Value::BigInt(delay_micros),
        Value::BigInt(ts_micros),
    ];

    match traw_execute(cx, &tracked, sql, &params).await {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(error) => Outcome::Err(DbError::Internal(format!(
            "failed to update rollup for {stratum_key}: {error}"
        ))),
        Outcome::Cancelled(reason) => Outcome::Cancelled(reason),
        Outcome::Panicked(payload) => Outcome::Panicked(payload),
    }
}

/// Fetch rollup statistics for all strata.
pub async fn fetch_atc_rollups(cx: &Cx, pool: &DbPool) -> Outcome<Vec<AtcRollupRow>, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(payload) => return Outcome::Panicked(payload),
        }
        return match fetch_atc_rollups_file_backed(pool) {
            Ok(rows) => Outcome::Ok(rows),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let sql = "SELECT stratum_key, total_count, resolved_count, censored_count, \
               expired_count, correct_count, incorrect_count, total_regret, total_loss, \
               ewma_loss, ewma_weight \
               FROM atc_experience_rollups ORDER BY stratum_key";

    let rows = match traw_query(cx, &tracked, sql, &[]).await {
        Outcome::Ok(r) => r,
        Outcome::Err(error) => {
            return Outcome::Err(DbError::Internal(format!(
                "failed to fetch rollups: {error}"
            )));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let mut results = Vec::with_capacity(rows.len());
    for row in &rows {
        let stratum_key = row
            .get(0)
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let total = row.get(1).and_then(value_as_i64).unwrap_or(0);
        let resolved = row.get(2).and_then(value_as_i64).unwrap_or(0);
        let censored = row.get(3).and_then(value_as_i64).unwrap_or(0);
        let expired = row.get(4).and_then(value_as_i64).unwrap_or(0);
        let correct = row.get(5).and_then(value_as_i64).unwrap_or(0);
        let incorrect = row.get(6).and_then(value_as_i64).unwrap_or(0);
        let regret = row
            .get(7)
            .and_then(|v| match v {
                Value::Double(f) => Some(*f),
                Value::Float(f) => Some(f64::from(*f)),
                _ => None,
            })
            .unwrap_or(0.0);
        let loss = row
            .get(8)
            .and_then(|v| match v {
                Value::Double(f) => Some(*f),
                Value::Float(f) => Some(f64::from(*f)),
                _ => None,
            })
            .unwrap_or(0.0);
        let ewma = row
            .get(9)
            .and_then(|v| match v {
                Value::Double(f) => Some(*f),
                Value::Float(f) => Some(f64::from(*f)),
                _ => None,
            })
            .unwrap_or(0.0);
        let weight = row
            .get(10)
            .and_then(|v| match v {
                Value::Double(f) => Some(*f),
                Value::Float(f) => Some(f64::from(*f)),
                _ => None,
            })
            .unwrap_or(0.0);
        results.push((
            stratum_key,
            total,
            resolved,
            censored,
            expired,
            correct,
            incorrect,
            regret,
            loss,
            ewma,
            weight,
        ));
    }

    Outcome::Ok(results)
}

// =============================================================================
// ATC Leader Lease — public API
// =============================================================================

/// Attempt to acquire the singleton ATC leader lease.
///
/// Leader election for multi-process ATC coordination. Only the leader
/// instance runs tick-driven operations (experience sweeps, rollup
/// refreshes, retention purges). Followers skip these to avoid conflicts.
///
/// **Semantics:**
/// - If no lease exists: inserts one, returns `Acquired`.
/// - If this instance already holds it: renews, returns `Acquired`.
/// - If another instance holds an expired lease: steals it, returns `Acquired`.
/// - If another instance holds a live lease: returns `NotLeader`.
///
/// Callers should retry on a timer (e.g., every `ttl_micros / 3`).
pub async fn try_acquire_atc_leader_lease(
    cx: &Cx,
    pool: &DbPool,
    instance_id: &str,
    now_micros: i64,
    ttl_micros: i64,
) -> Outcome<LeaseOutcome, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
        return match try_acquire_atc_leader_lease_file_backed(
            pool,
            instance_id,
            now_micros,
            ttl_micros,
        ) {
            Ok(outcome) => Outcome::Ok(outcome),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let rows = match traw_query(
        cx,
        &tracked,
        "SELECT instance_id, renewed_at, ttl_micros FROM atc_leader_lease WHERE lease_slot = 1",
        &[],
    )
    .await
    {
        Outcome::Ok(r) => r,
        Outcome::Err(error) => {
            return Outcome::Err(DbError::Internal(format!(
                "leader_lease select failed: {error}"
            )));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    if let Some(row) = rows.first() {
        let holder = match row_text(row, 0, "instance_id") {
            Ok(s) => s,
            Err(e) => return Outcome::Err(e),
        };
        let renewed = row.get(1).and_then(value_as_i64).unwrap_or(0);
        let ttl = row.get(2).and_then(value_as_i64).unwrap_or(0);

        if holder == instance_id {
            return match traw_execute(
                cx,
                &tracked,
                "UPDATE atc_leader_lease SET renewed_at = ?, ttl_micros = ? WHERE lease_slot = 1",
                &[Value::BigInt(now_micros), Value::BigInt(ttl_micros)],
            )
            .await
            {
                Outcome::Ok(_) => Outcome::Ok(LeaseOutcome::Acquired),
                Outcome::Err(e) => {
                    Outcome::Err(DbError::Internal(format!("leader_lease renew: {e}")))
                }
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            };
        }

        let expires_at = renewed.saturating_add(ttl);
        if now_micros >= expires_at {
            return match traw_execute(
                cx,
                &tracked,
                "UPDATE atc_leader_lease SET instance_id = ?, acquired_at = ?, renewed_at = ?, ttl_micros = ? WHERE lease_slot = 1",
                &[
                    Value::Text(instance_id.to_string()),
                    Value::BigInt(now_micros),
                    Value::BigInt(now_micros),
                    Value::BigInt(ttl_micros),
                ],
            )
            .await
            {
                Outcome::Ok(_) => Outcome::Ok(LeaseOutcome::Acquired),
                Outcome::Err(e) => Outcome::Err(DbError::Internal(format!("leader_lease steal: {e}"))),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            };
        }

        return Outcome::Ok(LeaseOutcome::NotLeader {
            holder,
            expires_at_micros: expires_at,
        });
    }

    match traw_execute(
        cx,
        &tracked,
        "INSERT INTO atc_leader_lease (lease_slot, instance_id, acquired_at, renewed_at, ttl_micros) \
         VALUES (1, ?, ?, ?, ?)",
        &[
            Value::Text(instance_id.to_string()),
            Value::BigInt(now_micros),
            Value::BigInt(now_micros),
            Value::BigInt(ttl_micros),
        ],
    )
    .await
    {
        Outcome::Ok(_) => Outcome::Ok(LeaseOutcome::Acquired),
        Outcome::Err(e) => Outcome::Err(DbError::Internal(format!("leader_lease insert: {e}"))),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Renew the ATC leader lease if this instance still holds it.
///
/// Returns `true` if the lease was renewed, `false` if this instance
/// is no longer the leader (another instance stole the lease).
pub async fn renew_atc_leader_lease(
    cx: &Cx,
    pool: &DbPool,
    instance_id: &str,
    now_micros: i64,
    ttl_micros: i64,
) -> Outcome<bool, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
        return match renew_atc_leader_lease_file_backed(pool, instance_id, now_micros, ttl_micros) {
            Ok(renewed) => Outcome::Ok(renewed),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    let rows = match traw_query(
        cx,
        &tracked,
        "SELECT instance_id FROM atc_leader_lease WHERE lease_slot = 1",
        &[],
    )
    .await
    {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => {
            return Outcome::Err(DbError::Internal(format!("leader_lease renew select: {e}")));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let is_ours = rows
        .first()
        .and_then(|r| r.get(0))
        .is_some_and(|v| matches!(v, Value::Text(s) if s == instance_id));

    if !is_ours {
        return Outcome::Ok(false);
    }

    match traw_execute(
        cx,
        &tracked,
        "UPDATE atc_leader_lease SET renewed_at = ?, ttl_micros = ? \
         WHERE lease_slot = 1 AND instance_id = ?",
        &[
            Value::BigInt(now_micros),
            Value::BigInt(ttl_micros),
            Value::Text(instance_id.to_string()),
        ],
    )
    .await
    {
        Outcome::Ok(_) => Outcome::Ok(true),
        Outcome::Err(e) => Outcome::Err(DbError::Internal(format!("leader_lease renew: {e}"))),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Release the ATC leader lease. No-op if this instance is not the leader.
pub async fn release_atc_leader_lease(
    cx: &Cx,
    pool: &DbPool,
    instance_id: &str,
) -> Outcome<bool, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
        return match release_atc_leader_lease_file_backed(pool, instance_id) {
            Ok(released) => Outcome::Ok(released),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    match traw_execute(
        cx,
        &tracked,
        "DELETE FROM atc_leader_lease WHERE lease_slot = 1 AND instance_id = ?",
        &[Value::Text(instance_id.to_string())],
    )
    .await
    {
        Outcome::Ok(_) => Outcome::Ok(true),
        Outcome::Err(e) => Outcome::Err(DbError::Internal(format!("leader_lease release: {e}"))),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// ATC rollup snapshot / restore
// =============================================================================

/// Snapshot all current ATC rollup rows into JSON, record the snapshot
/// metadata in `atc_rollup_snapshots`, and return the full snapshot.
pub async fn snapshot_atc_rollups(
    cx: &Cx,
    pool: &DbPool,
    now_micros: i64,
) -> Outcome<RollupSnapshot, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
        return match snapshot_atc_rollups_file_backed(pool, now_micros) {
            Ok(snap) => Outcome::Ok(snap),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };
    let tracked = tracked(&*conn);

    let rows = match traw_query(cx, &tracked, ATC_ROLLUP_SNAPSHOT_SELECT_SQL, &[]).await {
        Outcome::Ok(r) => r,
        Outcome::Err(e) => {
            return Outcome::Err(DbError::Internal(format!(
                "snapshot_atc_rollups select: {e}"
            )));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let rollups = rows
        .iter()
        .map(decode_atc_rollup_snapshot_row)
        .collect::<Vec<_>>();

    let payload = match serde_json::to_string(&rollups) {
        Ok(p) => p,
        Err(e) => {
            return Outcome::Err(DbError::Internal(format!(
                "snapshot rollups serialize: {e}"
            )));
        }
    };
    let sha256 = sha256_hex(&payload);
    #[allow(clippy::cast_possible_wrap)]
    let row_count = rollups.len() as i64;

    match traw_execute(
        cx,
        &tracked,
        "INSERT INTO atc_rollup_snapshots (captured_ts, rollup_rows, payload_sha256) \
         VALUES (?, ?, ?)",
        &[
            Value::BigInt(now_micros),
            Value::BigInt(row_count),
            Value::Text(sha256.clone()),
        ],
    )
    .await
    {
        Outcome::Ok(_) => {}
        Outcome::Err(e) => {
            return Outcome::Err(DbError::Internal(format!("snapshot_rollups insert: {e}")));
        }
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    Outcome::Ok(RollupSnapshot {
        captured_ts_micros: now_micros,
        rollup_rows: row_count,
        payload_sha256: sha256,
        payload,
    })
}

/// Restore ATC rollup rows from a previously captured JSON payload.
///
/// Returns the number of rows upserted.
pub async fn restore_atc_rollups(
    cx: &Cx,
    pool: &DbPool,
    payload: &str,
    now_micros: i64,
) -> Outcome<usize, DbError> {
    if pool.sqlite_path() != ":memory:" {
        match ensure_file_backed_atc_pool_initialized(cx, pool).await {
            Outcome::Ok(()) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
        let conn = match open_canonical_atc_conn(pool, "restore_atc_rollups") {
            Ok(c) => c,
            Err(e) => return Outcome::Err(e),
        };
        let result = restore_atc_rollups_from_payload(&conn, payload, now_micros);
        close_canonical_db_conn(conn, "canonical ATC restore connection");
        return match result {
            Ok(count) => Outcome::Ok(count),
            Err(error) => Outcome::Err(error),
        };
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    match restore_atc_rollups_from_payload_pooled(&*conn, payload, now_micros) {
        Ok(count) => Outcome::Ok(count),
        Err(error) => Outcome::Err(error),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::{
        EffectKind, ExperienceOutcome, ExperienceState, ExperienceSubsystem, FeatureExtension,
        FeatureVector, NonExecutionReason,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Id, Metadata, Subscriber, span};

    #[derive(Clone, Default)]
    struct EventCapture {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
        next_id: Arc<AtomicU64>,
    }

    impl EventCapture {
        fn drop_close_count(&self) -> usize {
            self.events
                .lock()
                .expect("event capture lock poisoned")
                .iter()
                .filter(|event| {
                    event.target == "fsqlite::runtime"
                        && event
                            .fields
                            .iter()
                            .any(|(name, value)| name == "event" && value.contains("drop_close"))
                })
                .count()
        }

        fn message_count(&self, message: &str) -> usize {
            self.events
                .lock()
                .expect("event capture lock poisoned")
                .iter()
                .filter(|event| {
                    event
                        .fields
                        .iter()
                        .any(|(name, value)| name == "message" && value.contains(message))
                })
                .count()
        }
    }

    #[derive(Clone, Debug)]
    struct CapturedEvent {
        target: &'static str,
        fields: Vec<(String, String)>,
    }

    #[derive(Default)]
    struct EventFieldCapture {
        fields: Vec<(String, String)>,
    }

    impl Visit for EventFieldCapture {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    impl Subscriber for EventCapture {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn register_callsite(
            &self,
            _metadata: &'static Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }

        fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
            Some(tracing::metadata::LevelFilter::TRACE)
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> Id {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            Id::from_u64(id)
        }

        fn record(&self, _span: &Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = EventFieldCapture::default();
            event.record(&mut fields);
            self.events
                .lock()
                .expect("event capture lock poisoned")
                .push(CapturedEvent {
                    target: event.metadata().target(),
                    fields: fields.fields,
                });
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    // ── D3 (br-bvq1x.4.3): retry budget exhaustion wrapping ──────────

    #[test]
    fn mvcc_retry_exhaustion_wraps_error_with_budget_context() {
        use asupersync::runtime::RuntimeBuilder;
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async move {
            let outcome: Outcome<(), DbError> =
                run_with_mvcc_retry_with_budget(&cx, "test_op", 1, || async {
                    Outcome::Err(DbError::ResourceBusy("database is locked".into()))
                })
                .await;
            let Outcome::Err(err) = outcome else {
                panic!("expected exhaustion error");
            };
            let DbError::RetryBudgetExhausted {
                operation,
                attempts,
                budget,
                elapsed_ms: _,
                inner,
            } = err
            else {
                panic!("expected RetryBudgetExhausted, got: {err}");
            };
            assert_eq!(operation, "test_op");
            assert_eq!(attempts, 2);
            assert_eq!(budget, 2);
            assert!(matches!(*inner, DbError::ResourceBusy(_)));
        });
    }

    #[test]
    fn mvcc_retry_exhaustion_wraps_mvcc_conflicts_too() {
        use asupersync::runtime::RuntimeBuilder;
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async move {
            let outcome: Outcome<(), DbError> =
                run_with_mvcc_retry_with_budget(&cx, "test_mvcc_op", 0, || async {
                    Outcome::Err(DbError::Sqlite(
                        "write conflict on page 42: held by transaction 7".into(),
                    ))
                })
                .await;
            let Outcome::Err(err) = outcome else {
                panic!("expected exhaustion error");
            };
            assert!(
                matches!(
                    err,
                    DbError::RetryBudgetExhausted {
                        attempts: 1,
                        budget: 1,
                        ..
                    }
                ),
                "zero-retry budget still wraps with attempts=1: {err}"
            );
            // The wrapped error keeps the busy classification for envelopes.
            assert_eq!(
                err.classification().class,
                crate::error::DbErrorClass::BusyRetryable
            );
        });
    }

    #[test]
    fn mvcc_retry_success_and_non_busy_errors_pass_through_unwrapped() {
        use asupersync::runtime::RuntimeBuilder;
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        rt.block_on(async move {
            let ok: Outcome<u32, DbError> =
                run_with_mvcc_retry_with_budget(&cx, "test_ok", 1, || async { Outcome::Ok(7) })
                    .await;
            assert!(matches!(ok, Outcome::Ok(7)));

            let not_busy: Outcome<(), DbError> =
                run_with_mvcc_retry_with_budget(&cx, "test_not_busy", 1, || async {
                    Outcome::Err(DbError::not_found("Agent", "BlueLake"))
                })
                .await;
            let Outcome::Err(err) = not_busy else {
                panic!("expected error");
            };
            assert!(
                matches!(err, DbError::NotFound { .. }),
                "non-busy errors must not be wrapped: {err}"
            );
        });
    }

    async fn set_agent_last_active_for_test(cx: &Cx, pool: &DbPool, agent_id: i64, ts: i64) {
        let conn = acquire_conn(cx, pool)
            .await
            .into_result()
            .expect("acquire conn");
        let tracked = tracked(&*conn);
        map_sql_outcome(
            traw_execute(
                cx,
                &tracked,
                "UPDATE agents SET last_active_ts = ? WHERE id = ?",
                &[Value::BigInt(ts), Value::BigInt(agent_id)],
            )
            .await,
        )
        .into_result()
        .expect("update last_active_ts");
    }

    async fn read_agent_last_active_for_test(cx: &Cx, pool: &DbPool, agent_id: i64) -> i64 {
        let conn = acquire_conn(cx, pool)
            .await
            .into_result()
            .expect("acquire conn");
        let tracked = tracked(&*conn);
        let rows = map_sql_outcome(
            traw_query(
                cx,
                &tracked,
                "SELECT last_active_ts FROM agents WHERE id = ?",
                &[Value::BigInt(agent_id)],
            )
            .await,
        )
        .into_result()
        .expect("query last_active_ts");
        let row = rows.first().expect("agent row");
        row.get_named("last_active_ts")
            .expect("decode last_active_ts")
    }

    async fn count_projects_for_human_key_for_test(cx: &Cx, pool: &DbPool, human_key: &str) -> i64 {
        let conn = acquire_conn(cx, pool)
            .await
            .into_result()
            .expect("acquire conn");
        let tracked = tracked(&*conn);
        let rows = map_sql_outcome(
            traw_query(
                cx,
                &tracked,
                "SELECT COUNT(*) AS cnt FROM projects WHERE human_key = ?",
                &[Value::Text(human_key.to_string())],
            )
            .await,
        )
        .into_result()
        .expect("count projects");
        let row = rows.first().expect("count row");
        row.get_named("cnt").expect("decode count")
    }

    async fn count_projects_for_test(cx: &Cx, pool: &DbPool) -> i64 {
        let conn = acquire_conn(cx, pool)
            .await
            .into_result()
            .expect("acquire conn");
        let tracked = tracked(&*conn);
        let rows = map_sql_outcome(
            traw_query(cx, &tracked, "SELECT COUNT(*) AS cnt FROM projects", &[]).await,
        )
        .into_result()
        .expect("count projects");
        let row = rows.first().expect("count row");
        row.get_named("cnt").expect("decode count")
    }

    fn create_file_pool_with_schema_for_test(label: &str) -> (tempfile::TempDir, DbPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir
            .path()
            .join(format!("{label}-{}.db", crate::timestamps::now_micros()));
        let init_conn =
            crate::DbConn::open_file(db_path.display().to_string()).expect("open schema db");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");
        (dir, pool)
    }

    async fn insert_project_row_for_test(cx: &Cx, pool: &DbPool, row: &ProjectRow) {
        let conn = acquire_conn(cx, pool)
            .await
            .into_result()
            .expect("acquire conn");
        let tracked = tracked(&*conn);
        map_sql_outcome(
            traw_execute(
                cx,
                &tracked,
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
                &[
                    Value::BigInt(row.id.expect("project row id")),
                    Value::Text(row.slug.clone()),
                    Value::Text(row.human_key.clone()),
                    Value::BigInt(row.created_at),
                ],
            )
            .await,
        )
        .into_result()
        .expect("insert project row");
    }

    #[test]
    fn cache_scope_for_pool_distinguishes_memory_pools() {
        let config = crate::pool::DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..crate::pool::DbPoolConfig::default()
        };
        let pool_a = DbPool::new(&config).expect("pool a");
        let pool_b = DbPool::new(&config).expect("pool b");

        assert_ne!(cache_scope_for_pool(&pool_a), cache_scope_for_pool(&pool_b));
    }

    #[test]
    fn ensure_project_cache_is_scoped_to_memory_pool() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();
        let suffix = format!("{}_{}", std::process::id(), now_micros());

        let cfg_a = crate::pool::DbPoolConfig {
            database_url: format!("sqlite://file:mem_a_{suffix}?mode=memory&cache=shared"),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let cfg_b = crate::pool::DbPoolConfig {
            database_url: format!("sqlite://file:mem_b_{suffix}?mode=memory&cache=shared"),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool_a = crate::create_pool(&cfg_a).expect("create pool a");
        let pool_b = crate::create_pool(&cfg_b).expect("create pool b");
        let human_key = "/tmp/scoped-project-cache";

        rt.block_on(async {
            let conn_a = acquire_conn(&cx, &pool_a)
                .await
                .into_result()
                .expect("acquire a");
            crate::schema::migrate_to_latest_base(&cx, &*conn_a)
                .await
                .into_result()
                .expect("migrate a");
            drop(conn_a);
            let conn_b = acquire_conn(&cx, &pool_b)
                .await
                .into_result()
                .expect("acquire b");
            crate::schema::migrate_to_latest_base(&cx, &*conn_b)
                .await
                .into_result()
                .expect("migrate b");
            drop(conn_b);

            ensure_project(&cx, &pool_a, human_key)
                .await
                .into_result()
                .expect("ensure project a");
            ensure_project(&cx, &pool_b, human_key)
                .await
                .into_result()
                .expect("ensure project b");

            assert_eq!(
                count_projects_for_human_key_for_test(&cx, &pool_a, human_key).await,
                1,
                "pool a should persist its project row"
            );
            assert_eq!(
                count_projects_for_human_key_for_test(&cx, &pool_b, human_key).await,
                1,
                "pool b should persist its own project row instead of reusing pool a cache"
            );
        });
    }

    #[test]
    fn begin_concurrent_fallback_detects_parser_error() {
        assert!(should_fallback_begin_concurrent(
            "near \"CONCURRENT\": syntax error"
        ));
    }

    #[test]
    fn begin_concurrent_fallback_detects_recovery_error_signatures() {
        assert!(should_fallback_begin_concurrent(
            "Query error: out of memory"
        ));
        assert!(should_fallback_begin_concurrent(
            "QUERY ERROR: OUT OF MEMORY"
        ));
        assert!(should_fallback_begin_concurrent(
            "internal error: cursor stack is empty"
        ));
    }

    #[test]
    fn begin_concurrent_fallback_rejects_non_recovery_errors() {
        assert!(!should_fallback_begin_concurrent("database is locked"));
        assert!(!should_fallback_begin_concurrent("no such table: agents"));
    }

    #[test]
    fn map_sql_error_classifies_lock_errors_as_resource_busy() {
        let err = map_sql_error(&SqlError::Custom("database is locked".to_string()));
        match err {
            DbError::ResourceBusy(message) => {
                assert!(message.contains("database is locked"));
            }
            other => panic!("expected ResourceBusy, got {other:?}"),
        }
    }

    #[test]
    fn map_sql_error_keeps_non_lock_errors_as_sqlite() {
        let err = map_sql_error(&SqlError::Custom("constraint failed".to_string()));
        match err {
            DbError::Sqlite(message) => {
                assert!(message.contains("constraint failed"));
            }
            other => panic!("expected Sqlite, got {other:?}"),
        }
    }

    #[test]
    fn busy_snapshot_resource_busy_still_counts_as_mvcc_error() {
        let err = map_sql_error(&SqlError::Custom(
            "database is busy (snapshot conflict on pages: 7)".to_string(),
        ));
        assert!(matches!(
            &err,
            DbError::ResourceBusy(message) if message.contains("snapshot conflict on pages: 7")
        ));
        assert!(
            is_mvcc_error(&err),
            "BusySnapshot must trigger whole-transaction MVCC retry"
        );
    }

    #[test]
    fn run_with_mvcc_retry_restarts_resource_busy_snapshot_until_success() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let attempts = std::cell::Cell::new(0_u32);

        let result = rt.block_on(async {
            run_with_mvcc_retry(&cx, "test_mvcc_retry", || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                async move {
                    if attempt < 3 {
                        Outcome::Err(DbError::ResourceBusy(
                            "database is busy (snapshot conflict on pages: 7)".to_string(),
                        ))
                    } else {
                        Outcome::Ok(attempt)
                    }
                }
            })
            .await
        });

        assert!(matches!(result, Outcome::Ok(3)));
        assert_eq!(attempts.get(), 3, "must restart the whole transaction body");
    }

    #[test]
    fn run_with_mvcc_retry_restarts_plain_locked_until_success() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let attempts = std::cell::Cell::new(0_u32);

        let result = rt.block_on(async {
            run_with_mvcc_retry(&cx, "test_write_contention_retry", || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                async move {
                    if attempt < 3 {
                        Outcome::Err(DbError::ResourceBusy("database is locked".to_string()))
                    } else {
                        Outcome::Ok(attempt)
                    }
                }
            })
            .await
        });

        assert!(matches!(result, Outcome::Ok(3)));
        assert_eq!(
            attempts.get(),
            3,
            "plain SQLITE_BUSY must restart the transaction"
        );
    }

    #[test]
    fn run_with_mvcc_retry_does_not_retry_non_lock_resource_busy() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let attempts = std::cell::Cell::new(0_u32);

        let result = rt.block_on(async {
            run_with_mvcc_retry(&cx, "test_non_lock_resource_busy", || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                async move {
                    Outcome::<(), DbError>::Err(DbError::ResourceBusy(
                        "Reservation conflict: src/lib.rs overlaps".to_string(),
                    ))
                }
            })
            .await
        });

        assert!(matches!(result, Outcome::Err(DbError::ResourceBusy(_))));
        assert_eq!(
            attempts.get(),
            1,
            "non-lock ResourceBusy must not be retried"
        );
    }

    fn setup_test_pool(db_name: &str) -> (Cx, DbPool, tempfile::TempDir) {
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join(db_name);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build shared ATC test pool runtime");
        match rt.block_on(ensure_file_backed_atc_pool_initialized(&cx, &pool)) {
            Outcome::Ok(()) => {}
            Outcome::Err(error) => {
                panic!("initialize ATC schema in shared test pool: {error}");
            }
            Outcome::Cancelled(reason) => {
                panic!("initialize ATC schema in shared test pool cancelled: {reason}");
            }
            Outcome::Panicked(payload) => std::panic::resume_unwind(Box::new(payload)),
        }
        (cx, pool, dir)
    }

    fn open_direct_repair_connection(db_path: &std::path::Path) -> crate::DbConn {
        crate::open_sqlite_file_with_recovery(
            db_path
                .to_str()
                .expect("direct repair connection path should be utf-8"),
        )
        .expect("open direct repair connection")
    }

    #[test]
    fn atc_rollup_snapshot_payload_preserves_compacted_baseline_fields() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("atc_rollup_snapshot_compacted.db");

        let conn = open_canonical_atc_conn(&pool, "seed compacted rollup")
            .expect("open canonical ATC connection");
        canonical_execute_atc(
            &conn,
            "INSERT INTO atc_experience_rollups (\
                stratum_key, subsystem, effect_kind, risk_tier,\
                total_count, resolved_count, censored_count, expired_count,\
                correct_count, incorrect_count, total_regret, total_loss,\
                ewma_loss, ewma_weight, delay_sum_micros, delay_count,\
                delay_max_micros, last_updated_ts,\
                compacted_total_count, compacted_resolved_count,\
                compacted_censored_count, compacted_expired_count,\
                compacted_correct_count, compacted_incorrect_count,\
                compacted_total_regret, compacted_total_loss,\
                compacted_ewma_loss, compacted_ewma_weight,\
                compacted_delay_sum_micros, compacted_delay_count,\
                compacted_delay_max_micros, compacted_last_updated_ts\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::Text("liveness:probe:0".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe".to_string()),
                Value::Int(0),
                Value::BigInt(7),
                Value::BigInt(6),
                Value::BigInt(0),
                Value::BigInt(1),
                Value::BigInt(5),
                Value::BigInt(1),
                Value::Double(0.75),
                Value::Double(1.25),
                Value::Double(0.40),
                Value::Double(2.0),
                Value::BigInt(12_000),
                Value::BigInt(6),
                Value::BigInt(4_000),
                Value::BigInt(1_700_000_000_000_000),
                Value::BigInt(4),
                Value::BigInt(3),
                Value::BigInt(0),
                Value::BigInt(1),
                Value::BigInt(2),
                Value::BigInt(1),
                Value::Double(0.50),
                Value::Double(0.75),
                Value::Double(0.25),
                Value::Double(1.50),
                Value::BigInt(8_000),
                Value::BigInt(3),
                Value::BigInt(3_000),
                Value::BigInt(1_699_999_000_000_000),
            ],
            "seed compacted rollup",
        )
        .expect("seed compacted rollup");
        close_canonical_db_conn(conn, "seed compacted rollup connection");

        let snapshot = rt
            .block_on(snapshot_atc_rollups(&cx, &pool, 1_700_000_100_000_000))
            .into_result()
            .expect("snapshot rollups");
        let payload: serde_json::Value =
            serde_json::from_str(&snapshot.payload).expect("snapshot payload JSON");
        let first_row = payload
            .as_array()
            .and_then(|rows| rows.first())
            .expect("snapshot row");

        assert_eq!(first_row["compacted_total_count"], 4);
        assert_eq!(first_row["compacted_resolved_count"], 3);
        assert_eq!(
            first_row["compacted_last_updated_ts"],
            1_699_999_000_000_000_i64
        );

        let conn = open_canonical_atc_conn(&pool, "mutate compacted rollup")
            .expect("open canonical ATC connection");
        canonical_execute_atc(
            &conn,
            "UPDATE atc_experience_rollups SET \
                subsystem = 'wrong', effect_kind = 'wrong', risk_tier = 9, \
                compacted_total_count = 0, compacted_resolved_count = 0, \
                compacted_last_updated_ts = 0 \
             WHERE stratum_key = 'liveness:probe:0'",
            &[],
            "mutate compacted rollup",
        )
        .expect("mutate compacted rollup");
        close_canonical_db_conn(conn, "mutate compacted rollup connection");

        let restored = rt
            .block_on(restore_atc_rollups(
                &cx,
                &pool,
                &snapshot.payload,
                1_700_000_200_000_000,
            ))
            .into_result()
            .expect("restore rollups");
        assert_eq!(restored, 1);

        let conn = open_canonical_atc_conn(&pool, "verify compacted rollup restore")
            .expect("open canonical ATC connection");
        let rows = canonical_query_atc_rows(
            &conn,
            "SELECT subsystem, effect_kind, risk_tier, compacted_total_count, \
                    compacted_resolved_count, compacted_last_updated_ts \
             FROM atc_experience_rollups WHERE stratum_key = 'liveness:probe:0'",
            &[],
            "verify compacted rollup restore",
        )
        .expect("query restored compacted rollup");
        close_canonical_db_conn(conn, "verify compacted rollup restore connection");
        let row = rows.first().expect("restored rollup row");
        assert_eq!(row_text_or_default(row, 0), "liveness");
        assert_eq!(row_text_or_default(row, 1), "probe");
        assert_eq!(row_i64_or_default(row, 2), 0);
        assert_eq!(row_i64_or_default(row, 3), 4);
        assert_eq!(row_i64_or_default(row, 4), 3);
        assert_eq!(row_i64_or_default(row, 5), 1_699_999_000_000_000);
    }

    fn make_insert_experience_test_row(
        decision_id: u64,
        effect_id: u64,
        state: ExperienceState,
        created_ts_micros: i64,
    ) -> ExperienceRow {
        let mut row = ExperienceRow {
            experience_id: 0,
            decision_id,
            effect_id,
            trace_id: format!("trc-insert-{decision_id}-{effect_id}"),
            claim_id: format!("clm-insert-{decision_id}-{effect_id}"),
            evidence_id: format!("evi-insert-{decision_id}-{effect_id}"),
            state,
            subsystem: ExperienceSubsystem::Liveness,
            decision_class: "liveness_transition".to_string(),
            subject: format!("agent-{decision_id}-{effect_id}"),
            project_key: Some("/tmp/insert-atc-experience".to_string()),
            policy_id: Some("liveness-incumbent-r1".to_string()),
            effect_kind: EffectKind::Probe,
            action: "ProbeAgent".to_string(),
            posterior: vec![
                ("Alive".to_string(), 0.40),
                ("Flaky".to_string(), 0.35),
                ("Dead".to_string(), 0.25),
            ],
            expected_loss: 1.1,
            runner_up_action: Some("DeferProbe".to_string()),
            runner_up_loss: Some(1.8),
            evidence_summary: "insert API regression row".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            non_execution_reason: None,
            outcome: None,
            created_ts_micros,
            dispatched_ts_micros: None,
            executed_ts_micros: None,
            resolved_ts_micros: None,
            features: Some(FeatureVector {
                posterior_alive_bp: 4000,
                posterior_flaky_bp: 3500,
                expected_loss_bp: 110,
                loss_gap_bp: 70,
                risk_tier: FeatureVector::risk_tier_for(EffectKind::Probe),
                ..FeatureVector::zeroed()
            }),
            feature_ext: Some(
                FeatureExtension::empty()
                    .with_field(
                        "decision_id",
                        i64::try_from(decision_id).expect("decision_id"),
                    )
                    .with_field("effect_id", i64::try_from(effect_id).expect("effect_id")),
            ),
            context: Some(serde_json::json!({
                "source": "insert_experience_test",
                "decision_id": decision_id,
                "effect_id": effect_id,
            })),
        };

        match state {
            ExperienceState::Planned => {}
            ExperienceState::Dispatched => {
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
            }
            ExperienceState::Executed | ExperienceState::Open => {
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
                row.executed_ts_micros = Some(created_ts_micros.saturating_add(2_000));
            }
            ExperienceState::Failed => {
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
                row.executed_ts_micros = Some(created_ts_micros.saturating_add(2_000));
            }
            ExperienceState::Throttled | ExperienceState::Suppressed | ExperienceState::Skipped => {
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
                row.executed_ts_micros = Some(created_ts_micros.saturating_add(2_000));
                row.non_execution_reason = Some(NonExecutionReason::CalibrationFallback {
                    reason: "insert_experience_test".to_string(),
                });
            }
            ExperienceState::Resolved => {
                let resolved_ts_micros = created_ts_micros.saturating_add(4_000);
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
                row.executed_ts_micros = Some(created_ts_micros.saturating_add(2_000));
                row.resolved_ts_micros = Some(resolved_ts_micros);
                row.outcome = Some(ExperienceOutcome {
                    observed_ts_micros: resolved_ts_micros,
                    label: "resolved".to_string(),
                    correct: true,
                    actual_loss: Some(0.25),
                    regret: Some(0.05),
                    evidence: Some(serde_json::json!({"sealed": true})),
                });
            }
            ExperienceState::Censored => {
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
                row.executed_ts_micros = Some(created_ts_micros.saturating_add(2_000));
                row.resolved_ts_micros = Some(created_ts_micros.saturating_add(5_000));
            }
            ExperienceState::Expired => {
                row.dispatched_ts_micros = Some(created_ts_micros.saturating_add(1_000));
                row.executed_ts_micros = Some(created_ts_micros.saturating_add(2_000));
                row.resolved_ts_micros = Some(created_ts_micros.saturating_add(6_000));
            }
        }

        row
    }

    #[test]
    fn append_atc_experience_is_idempotent_by_decision_and_effect() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("append_atc_experience_idempotent.db");

        rt.block_on(async {
            let row = ExperienceRow {
                experience_id: 0,
                decision_id: 41,
                effect_id: 99,
                trace_id: "trc-append-idempotent".to_string(),
                claim_id: "clm-append-idempotent".to_string(),
                evidence_id: "evi-append-idempotent".to_string(),
                state: ExperienceState::Planned,
                subsystem: ExperienceSubsystem::Liveness,
                decision_class: "liveness_transition".to_string(),
                subject: "BlueLake".to_string(),
                project_key: Some("/tmp/append-atc-experience".to_string()),
                policy_id: Some("liveness-incumbent-r1".to_string()),
                effect_kind: EffectKind::Probe,
                action: "ProbeAgent".to_string(),
                posterior: vec![
                    ("Alive".to_string(), 0.25),
                    ("Flaky".to_string(), 0.50),
                    ("Dead".to_string(), 0.25),
                ],
                expected_loss: 1.25,
                runner_up_action: Some("DeferProbe".to_string()),
                runner_up_loss: Some(2.25),
                evidence_summary: "selected for probing".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_000_000,
                dispatched_ts_micros: None,
                executed_ts_micros: None,
                resolved_ts_micros: None,
                features: Some(FeatureVector {
                    posterior_alive_bp: 2500,
                    posterior_flaky_bp: 5000,
                    expected_loss_bp: 125,
                    loss_gap_bp: 100,
                    risk_tier: FeatureVector::risk_tier_for(EffectKind::Probe),
                    ..FeatureVector::zeroed()
                }),
                feature_ext: None,
                context: Some(serde_json::json!({
                    "policy_revision": 7,
                    "action_family": {
                        "kind": "probe_agent",
                        "category": "probe"
                    }
                })),
            };

            let first = append_atc_experience(&cx, &pool, &row)
                .await
                .into_result()
                .expect("first append");
            let second = append_atc_experience(&cx, &pool, &row)
                .await
                .into_result()
                .expect("second append");

            assert_eq!(
                first.experience_id, second.experience_id,
                "duplicate append should reselect the original row"
            );
            assert!(
                first.experience_id > 0,
                "store-assigned id must be populated"
            );

            // ATC experiences live in the sidecar (br-bvq1x.11.7); count them
            // through a canonical connection to it.
            let atc_conn = open_canonical_atc_conn(&pool, "count idempotent atc experiences")
                .expect("open atc sidecar verify conn");
            let rows = canonical_query_atc_rows(
                &atc_conn,
                "SELECT COUNT(*) FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                &[Value::BigInt(41), Value::BigInt(99)],
                "count idempotent atc experiences",
            )
            .expect("count atc experiences");
            close_canonical_db_conn(atc_conn, "count idempotent atc experiences");

            assert_eq!(rows.first().and_then(row_first_i64), Some(1));
        });
    }

    #[test]
    fn insert_experience_returns_assigned_id_and_round_trips_terminal_row() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("insert_experience_roundtrip.db");

        rt.block_on(async {
            let row = make_insert_experience_test_row(
                501,
                601,
                ExperienceState::Resolved,
                1_700_000_100_000_000,
            );
            let mut expected = row.clone();
            let experience_id = insert_experience(&cx, &pool, row)
                .await
                .into_result()
                .expect("insert experience");
            assert!(experience_id > 0, "insert must return assigned durable id");

            let stored = fetch_durable_atc_experience_by_id(
                &cx,
                &pool,
                i64::try_from(experience_id).expect("experience_id fits i64"),
            )
            .await
            .into_result()
            .expect("fetch stored experience")
            .expect("stored row");
            expected.experience_id = experience_id;

            assert_eq!(stored, expected);
            assert_eq!(stored.resolution_latency_micros(), Some(4_000));
        });
    }

    #[test]
    fn insert_experience_rejects_invalid_rows_without_persisting() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("insert_experience_invalid.db");

        rt.block_on(async {
            let mut row = make_insert_experience_test_row(
                777,
                888,
                ExperienceState::Resolved,
                1_700_000_200_000_000,
            );
            row.outcome = None;
            row.resolved_ts_micros = None;

            let error = insert_experience(&cx, &pool, row)
                .await
                .into_result()
                .expect_err("invalid row should be rejected");
            assert!(matches!(
                error,
                asupersync::OutcomeError::Err(DbError::InvalidArgument {
                    field: "experience_row",
                    ..
                })
            ));

            // ATC experiences live in the sidecar (br-bvq1x.11.7).
            let atc_conn = open_canonical_atc_conn(&pool, "count after rejected insert")
                .expect("open atc sidecar verify conn");
            let rows = canonical_query_atc_rows(
                &atc_conn,
                "SELECT COUNT(*) FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                &[Value::BigInt(777), Value::BigInt(888)],
                "count after rejected insert",
            )
            .expect("count experiences after rejected insert");
            close_canonical_db_conn(atc_conn, "count after rejected insert");
            assert_eq!(rows.first().and_then(row_first_i64), Some(0));
        });
    }

    #[test]
    fn insert_experience_is_idempotent_and_returns_same_assigned_id() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("insert_experience_idempotent.db");

        rt.block_on(async {
            let row = make_insert_experience_test_row(
                901,
                902,
                ExperienceState::Planned,
                1_700_000_300_000_000,
            );

            let first = insert_experience(&cx, &pool, row.clone())
                .await
                .into_result()
                .expect("first insert");
            let second = insert_experience(&cx, &pool, row)
                .await
                .into_result()
                .expect("second insert");

            assert_eq!(first, second, "duplicate insert must return the same id");

            // ATC experiences live in the sidecar (br-bvq1x.11.7).
            let atc_conn = open_canonical_atc_conn(&pool, "count duplicate inserts")
                .expect("open atc sidecar verify conn");
            let rows = canonical_query_atc_rows(
                &atc_conn,
                "SELECT COUNT(*) FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                &[Value::BigInt(901), Value::BigInt(902)],
                "count duplicate inserts",
            )
            .expect("count duplicate inserts");
            close_canonical_db_conn(atc_conn, "count duplicate inserts");
            assert_eq!(rows.first().and_then(row_first_i64), Some(1));
        });
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(12))]
        #[test]
        fn insert_experience_roundtrip_property(
            decision_id in 10_000_u64..10_512_u64,
            effect_id in 20_000_u64..20_512_u64,
            created_ts_micros in -500_000_i64..500_000_i64,
            state_tag in 0_u8..5_u8,
        ) {
            use asupersync::runtime::RuntimeBuilder;
            use proptest::prelude::*;

            let state = match state_tag {
                0 => ExperienceState::Planned,
                1 => ExperienceState::Open,
                2 => ExperienceState::Resolved,
                3 => ExperienceState::Censored,
                _ => ExperienceState::Expired,
            };
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            let db_name = format!("insert_experience_property_{decision_id}_{effect_id}_{state_tag}.db");
            let (cx, pool, _dir) = setup_test_pool(&db_name);
            let row = make_insert_experience_test_row(decision_id, effect_id, state, created_ts_micros);
            let expected_latency = row.resolution_latency_micros();
            let mut expected = row.clone();

            let stored = rt.block_on(async {
                let experience_id = insert_experience(&cx, &pool, row)
                    .await
                    .into_result()
                    .expect("insert experience");
                fetch_durable_atc_experience_by_id(
                    &cx,
                    &pool,
                    i64::try_from(experience_id).expect("experience_id fits i64"),
                )
                .await
                .into_result()
                .expect("fetch stored experience")
                .expect("stored row")
            });
            expected.experience_id = stored.experience_id;

            let stored_latency = stored.resolution_latency_micros();
            prop_assert_eq!(stored, expected);
            prop_assert_eq!(stored_latency, expected_latency);
        }
    }

    #[test]
    fn append_atc_experience_normalizes_feature_schema_version() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("append_atc_feature_schema_version.db");
        let db_path = dir.path().join("append_atc_feature_schema_version.db");

        rt.block_on(async {
            let row = ExperienceRow {
                experience_id: 0,
                decision_id: 142,
                effect_id: 242,
                trace_id: "trc-append-feature-schema".to_string(),
                claim_id: "clm-append-feature-schema".to_string(),
                evidence_id: "evi-append-feature-schema".to_string(),
                state: ExperienceState::Planned,
                subsystem: ExperienceSubsystem::Liveness,
                decision_class: "liveness_transition".to_string(),
                subject: "BlueLake".to_string(),
                project_key: Some("/tmp/append-feature-schema".to_string()),
                policy_id: Some("liveness-incumbent-r1".to_string()),
                effect_kind: EffectKind::Probe,
                action: "ProbeAgent".to_string(),
                posterior: vec![
                    ("Alive".to_string(), 0.25),
                    ("Flaky".to_string(), 0.50),
                    ("Dead".to_string(), 0.25),
                ],
                expected_loss: 1.0,
                runner_up_action: Some("DeferProbe".to_string()),
                runner_up_loss: Some(1.5),
                evidence_summary: "selected for probing".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_200_000,
                dispatched_ts_micros: None,
                executed_ts_micros: None,
                resolved_ts_micros: None,
                features: Some(FeatureVector {
                    version: 0,
                    posterior_alive_bp: 2500,
                    posterior_flaky_bp: 5000,
                    risk_tier: FeatureVector::risk_tier_for(EffectKind::Probe),
                    ..FeatureVector::zeroed()
                }),
                feature_ext: Some(FeatureExtension {
                    ext_version: 0,
                    fields: vec![("retry_budget".to_string(), 2)],
                }),
                context: None,
            };

            let stored = append_atc_experience(&cx, &pool, &row)
                .await
                .into_result()
                .expect("append experience");
            assert_eq!(
                stored.features.expect("features").version,
                FEATURE_SCHEMA_VERSION
            );
        });

        // ATC experiences are isolated in the sidecar DB (br-bvq1x.11.7); seed
        // and verify them through a canonical connection to that sidecar.
        let conn = crate::CanonicalDbConn::open_file(
            crate::pool::atc_sidecar_sqlite_path(db_path.to_string_lossy().as_ref()).as_str(),
        )
        .expect("open ATC sidecar");
        let rows = conn
            .query_sync(
                "SELECT feature_schema_version, features_json, feature_ext_json \
                 FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                &[Value::BigInt(142), Value::BigInt(242)],
            )
            .expect("query persisted feature schema version");
        assert_eq!(rows.len(), 1, "expected one stored ATC experience row");
        assert_eq!(
            rows[0]
                .get_named::<i64>("feature_schema_version")
                .expect("feature_schema_version"),
            i64::from(FEATURE_SCHEMA_VERSION)
        );
        let features_json: String = rows[0].get_named("features_json").expect("features_json");
        let feature_ext_json: String = rows[0]
            .get_named("feature_ext_json")
            .expect("feature_ext_json");
        let persisted_features: FeatureVector =
            serde_json::from_str(&features_json).expect("decode features_json");
        let persisted_ext: FeatureExtension =
            serde_json::from_str(&feature_ext_json).expect("decode feature_ext_json");
        assert_eq!(persisted_features.version, FEATURE_SCHEMA_VERSION);
        assert_eq!(persisted_ext.ext_version, 1);
    }

    #[test]
    fn fetch_durable_atc_experience_by_id_migrates_legacy_feature_payloads() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("fetch_legacy_atc_feature_schema.db");
        let db_path = dir.path().join("fetch_legacy_atc_feature_schema.db");

        // ATC experiences are isolated in the sidecar DB (br-bvq1x.11.7); seed
        // and verify them through a canonical connection to that sidecar.
        let conn = crate::CanonicalDbConn::open_file(
            crate::pool::atc_sidecar_sqlite_path(db_path.to_string_lossy().as_ref()).as_str(),
        )
        .expect("open ATC sidecar");
        conn.execute_sync(
            "INSERT INTO atc_experiences (\
                experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem,\
                decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json,\
                expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy,\
                safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json,\
                feature_schema_version, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json,\
                contained_suspected_secret, privacy_classification\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(5),
                Value::BigInt(500),
                Value::BigInt(600),
                Value::Text("trace-legacy-feature".to_string()),
                Value::Text("claim-legacy-feature".to_string()),
                Value::Text("evidence-legacy-feature".to_string()),
                Value::Text("open".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe".to_string()),
                Value::Text("BlueLake".to_string()),
                Value::Text("/tmp/legacy-feature".to_string()),
                Value::Text("policy-v1".to_string()),
                Value::Text("probe".to_string()),
                Value::Text("ProbeAgent".to_string()),
                Value::Text("[]".to_string()),
                Value::Double(0.25),
                Value::Null,
                Value::Null,
                Value::Text("legacy payload".to_string()),
                Value::BigInt(1),
                Value::BigInt(0),
                Value::Null,
                Value::Null,
                Value::Text(
                    serde_json::json!({
                        "version": 0,
                        "posterior_alive_bp": 2500,
                        "posterior_flaky_bp": 5000,
                        "silence_secs": 0,
                        "observation_count": 0,
                        "reservation_count": 0,
                        "conflict_count": 0,
                        "in_deadlock_cycle": false,
                        "throughput_per_min": 0,
                        "inbox_depth": 0,
                        "expected_loss_bp": 0,
                        "loss_gap_bp": 0,
                        "calibration_healthy": true,
                        "safe_mode_active": false,
                        "tick_utilization_bp": 0,
                        "controller_mode": 0,
                        "risk_tier": 0
                    })
                    .to_string(),
                ),
                Value::Text(
                    serde_json::json!({
                        "ext_version": 0,
                        "fields": [["retry_budget", 4]]
                    })
                    .to_string(),
                ),
                Value::BigInt(0),
                Value::BigInt(1_700_000_000_300_000),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::BigInt(0),
                Value::Text("legacy_unclassified".to_string()),
            ],
        )
        .expect("insert legacy ATC experience row");

        rt.block_on(async {
            let stored = fetch_durable_atc_experience_by_id(&cx, &pool, 5)
                .await
                .into_result()
                .expect("fetch durable ATC experience")
                .expect("stored row");
            assert_eq!(
                stored.features.expect("features").version,
                FEATURE_SCHEMA_VERSION
            );
            assert_eq!(stored.feature_ext.expect("feature ext").ext_version, 1);
        });
    }

    #[test]
    fn reprocess_atc_feature_schema_rewrites_legacy_rows() {
        let (_cx, _pool, dir) = setup_test_pool("reprocess_atc_feature_schema.db");
        let db_path = dir.path().join("reprocess_atc_feature_schema.db");
        // ATC experiences are isolated in the sidecar DB (br-bvq1x.11.7); seed
        // and verify them through a canonical connection to that sidecar.
        let conn = crate::CanonicalDbConn::open_file(
            crate::pool::atc_sidecar_sqlite_path(db_path.to_string_lossy().as_ref()).as_str(),
        )
        .expect("open ATC sidecar");

        conn.execute_sync(
            "INSERT INTO atc_experiences (\
                experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem,\
                decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json,\
                expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy,\
                safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json,\
                feature_schema_version, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json,\
                contained_suspected_secret, privacy_classification\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(7),
                Value::BigInt(700),
                Value::BigInt(800),
                Value::Text("trace-reprocess-feature".to_string()),
                Value::Text("claim-reprocess-feature".to_string()),
                Value::Text("evidence-reprocess-feature".to_string()),
                Value::Text("open".to_string()),
                Value::Text("liveness".to_string()),
                Value::Text("probe".to_string()),
                Value::Text("BlueLake".to_string()),
                Value::Text("/tmp/reprocess-feature".to_string()),
                Value::Text("policy-v1".to_string()),
                Value::Text("probe".to_string()),
                Value::Text("ProbeAgent".to_string()),
                Value::Text("[]".to_string()),
                Value::Double(0.25),
                Value::Null,
                Value::Null,
                Value::Text("legacy payload".to_string()),
                Value::BigInt(1),
                Value::BigInt(0),
                Value::Null,
                Value::Null,
                Value::Text(
                    serde_json::json!({
                        "version": 0,
                        "posterior_alive_bp": 2500,
                        "posterior_flaky_bp": 5000,
                        "silence_secs": 0,
                        "observation_count": 0,
                        "reservation_count": 0,
                        "conflict_count": 0,
                        "in_deadlock_cycle": false,
                        "throughput_per_min": 0,
                        "inbox_depth": 0,
                        "expected_loss_bp": 0,
                        "loss_gap_bp": 0,
                        "calibration_healthy": true,
                        "safe_mode_active": false,
                        "tick_utilization_bp": 0,
                        "controller_mode": 0,
                        "risk_tier": 0
                    })
                    .to_string(),
                ),
                Value::Text(
                    serde_json::json!({
                        "ext_version": 0,
                        "fields": [["retry_budget", 5]]
                    })
                    .to_string(),
                ),
                Value::BigInt(0),
                Value::BigInt(1_700_000_000_400_000),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::BigInt(0),
                Value::Text("legacy_unclassified".to_string()),
            ],
        )
        .expect("insert reprocess target row");

        let dry_run = reprocess_atc_feature_schema(&conn, None, Some("BlueLake"), 10, true)
            .expect("dry-run reprocess");
        assert_eq!(dry_run.scanned, 1);
        assert_eq!(dry_run.updated, 1);
        let before = conn
            .query_sync(
                "SELECT feature_schema_version FROM atc_experiences WHERE experience_id = 7",
                &[],
            )
            .expect("query pre-update schema version");
        assert_eq!(
            before[0]
                .get_named::<i64>("feature_schema_version")
                .expect("feature_schema_version before"),
            0
        );

        let applied = reprocess_atc_feature_schema(&conn, None, Some("BlueLake"), 10, false)
            .expect("apply reprocess");
        assert_eq!(applied.scanned, 1);
        assert_eq!(applied.updated, 1);
        assert_eq!(applied.current_schema_version, FEATURE_SCHEMA_VERSION);

        let after = conn
            .query_sync(
                "SELECT feature_schema_version, features_json, feature_ext_json \
                 FROM atc_experiences WHERE experience_id = 7",
                &[],
            )
            .expect("query post-update row");
        assert_eq!(
            after[0]
                .get_named::<i64>("feature_schema_version")
                .expect("feature_schema_version after"),
            i64::from(FEATURE_SCHEMA_VERSION)
        );
        let features_json: String = after[0].get_named("features_json").expect("features_json");
        let feature_ext_json: String = after[0]
            .get_named("feature_ext_json")
            .expect("feature_ext_json");
        let persisted_features: FeatureVector =
            serde_json::from_str(&features_json).expect("decode features_json");
        let persisted_ext: FeatureExtension =
            serde_json::from_str(&feature_ext_json).expect("decode feature_ext_json");
        assert_eq!(persisted_features.version, FEATURE_SCHEMA_VERSION);
        assert_eq!(persisted_ext.ext_version, 1);
    }

    #[test]
    fn transition_atc_experience_validates_and_merges_context() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("transition_atc_experience_capture.db");

        rt.block_on(async {
            let row = ExperienceRow {
                experience_id: 0,
                decision_id: 77,
                effect_id: 101,
                trace_id: "trc-transition-capture".to_string(),
                claim_id: "clm-transition-capture".to_string(),
                evidence_id: "evi-transition-capture".to_string(),
                state: ExperienceState::Planned,
                subsystem: ExperienceSubsystem::Liveness,
                decision_class: "liveness_transition".to_string(),
                subject: "RubyAspen".to_string(),
                project_key: Some("/tmp/transition-atc-experience".to_string()),
                policy_id: Some("liveness-incumbent-r1".to_string()),
                effect_kind: EffectKind::Probe,
                action: "ProbeAgent".to_string(),
                posterior: vec![
                    ("Alive".to_string(), 0.30),
                    ("Flaky".to_string(), 0.50),
                    ("Dead".to_string(), 0.20),
                ],
                expected_loss: 1.0,
                runner_up_action: Some("DeferProbe".to_string()),
                runner_up_loss: Some(1.5),
                evidence_summary: "selected for probing".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_000_100,
                dispatched_ts_micros: None,
                executed_ts_micros: None,
                resolved_ts_micros: None,
                features: Some(FeatureVector::zeroed()),
                feature_ext: None,
                context: Some(serde_json::json!({
                    "policy_revision": 9,
                    "execution": {
                        "attempt": 1,
                        "queued_by": "atc"
                    },
                    "action_family": {
                        "kind": "probe_agent",
                        "category": "probe"
                    }
                })),
            };

            let stored = append_atc_experience(&cx, &pool, &row)
                .await
                .into_result()
                .expect("append experience");

            let non_execution_reason = NonExecutionReason::SafetyGate {
                gate_name: "executor_mode_dry_run".to_string(),
                risk_score: 0.0,
                gate_threshold: 0.0,
            };
            let execution_patch = serde_json::json!({
                "execution": {
                    "status": "suppressed",
                    "classification": "policy_suppression",
                    "detail": "executor_mode_dry_run"
                }
            });
            transition_atc_experience(
                &cx,
                &pool,
                stored.experience_id,
                ExperienceState::Dispatched,
                1_700_000_000_000_200,
                None,
                None,
            )
            .await
            .into_result()
            .expect("mark dispatched");
            let missing_reason = transition_atc_experience(
                &cx,
                &pool,
                stored.experience_id,
                ExperienceState::Suppressed,
                1_700_000_000_000_250,
                None,
                None,
            )
            .await
            .into_result();
            assert!(matches!(
                missing_reason,
                Err(asupersync::OutcomeError::Err(DbError::InvalidArgument {
                    field: "non_execution_reason",
                    ..
                }))
            ));

            transition_atc_experience(
                &cx,
                &pool,
                stored.experience_id,
                ExperienceState::Suppressed,
                1_700_000_000_000_300,
                Some(&non_execution_reason),
                Some(&execution_patch),
            )
            .await
            .into_result()
            .expect("mark suppressed");
            transition_atc_experience(
                &cx,
                &pool,
                stored.experience_id,
                ExperienceState::Suppressed,
                1_700_000_000_000_350,
                None,
                None,
            )
            .await
            .into_result()
            .expect("same-state suppressed retry");

            // ATC experiences are isolated in the sidecar (br-bvq1x.11.7); verify
            // through a canonical connection to it.
            let atc_conn = open_canonical_atc_conn(&pool, "verify transition merged context")
                .expect("open atc sidecar verify conn");
            let rows = canonical_query_atc_rows(
                &atc_conn,
                &format!("{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE experience_id = ? LIMIT 1"),
                &[Value::BigInt(
                    i64::try_from(stored.experience_id).expect("experience id"),
                )],
                "verify transition merged context",
            )
            .expect("query stored experience");
            close_canonical_db_conn(atc_conn, "verify transition merged context");
            let stored = decode_atc_experience_row(rows.first().expect("stored row"))
                .expect("decode stored experience");

            assert_eq!(stored.state, ExperienceState::Suppressed);
            assert_eq!(stored.dispatched_ts_micros, Some(1_700_000_000_000_200));
            assert_eq!(stored.executed_ts_micros, Some(1_700_000_000_000_300));
            assert!(matches!(
                stored.non_execution_reason,
                Some(NonExecutionReason::SafetyGate { ref gate_name, .. })
                    if gate_name == "executor_mode_dry_run"
            ));
            assert_eq!(
                stored
                    .context
                    .as_ref()
                    .and_then(|value| value.get("policy_revision"))
                    .and_then(serde_json::Value::as_i64),
                Some(9)
            );
            assert_eq!(
                stored
                    .context
                    .as_ref()
                    .and_then(|value| value.get("execution"))
                    .and_then(|value| value.get("attempt"))
                    .and_then(serde_json::Value::as_i64),
                Some(1)
            );
            assert_eq!(
                stored
                    .context
                    .as_ref()
                    .and_then(|value| value.get("execution"))
                    .and_then(|value| value.get("queued_by"))
                    .and_then(serde_json::Value::as_str),
                Some("atc")
            );
            assert_eq!(
                stored
                    .context
                    .as_ref()
                    .and_then(|value| value.get("execution"))
                    .and_then(|value| value.get("status"))
                    .and_then(serde_json::Value::as_str),
                Some("suppressed")
            );

            let invalid = transition_atc_experience(
                &cx,
                &pool,
                stored.experience_id,
                ExperienceState::Planned,
                1_700_000_000_000_400,
                None,
                None,
            )
            .await
            .into_result();
            assert!(matches!(
                invalid,
                Err(asupersync::OutcomeError::Err(DbError::InvalidArgument {
                    field: "state",
                    ..
                }))
            ));
        });
    }

    #[test]
    fn transition_atc_experience_recovers_from_stale_pooled_snapshot() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir
            .path()
            .join("transition_atc_experience_stale_snapshot.db");
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let stale_pool = crate::create_pool(&cfg).expect("create stale pool");
        let writer_pool = crate::create_pool(&cfg).expect("create writer pool");

        rt.block_on(async {
            let seed = ExperienceRow {
                experience_id: 0,
                decision_id: 900,
                effect_id: 900,
                trace_id: "trc-transition-stale-seed".to_string(),
                claim_id: "clm-transition-stale-seed".to_string(),
                evidence_id: "evi-transition-stale-seed".to_string(),
                state: ExperienceState::Planned,
                subsystem: ExperienceSubsystem::Liveness,
                decision_class: "liveness_transition".to_string(),
                subject: "BlueLake".to_string(),
                project_key: Some("/tmp/transition-atc-experience-stale".to_string()),
                policy_id: Some("liveness-incumbent-r1".to_string()),
                effect_kind: EffectKind::Probe,
                action: "ProbeAgent".to_string(),
                posterior: vec![
                    ("Alive".to_string(), 0.60),
                    ("Flaky".to_string(), 0.25),
                    ("Dead".to_string(), 0.15),
                ],
                expected_loss: 0.7,
                runner_up_action: Some("DeferProbe".to_string()),
                runner_up_loss: Some(1.2),
                evidence_summary: "seed experience".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_010_000,
                dispatched_ts_micros: None,
                executed_ts_micros: None,
                resolved_ts_micros: None,
                features: Some(FeatureVector::zeroed()),
                feature_ext: None,
                context: None,
            };
            append_atc_experience(&cx, &writer_pool, &seed)
                .await
                .into_result()
                .expect("append seed experience");

            // ATC experiences live in the sidecar (br-bvq1x.11.7); prime a prior
            // reader against it. Canonical connections are opened fresh per ATC
            // op, so the transition below cannot inherit a stale snapshot from
            // this reader — but we still exercise a prior reader to confirm the
            // later transition observes the committed seed.
            let stale_conn = open_canonical_atc_conn(&stale_pool, "prime stale atc reader")
                .expect("open atc sidecar stale reader");
            canonical_query_atc_rows(
                &stale_conn,
                "SELECT experience_id FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                &[Value::BigInt(900), Value::BigInt(900)],
                "prime stale atc reader",
            )
            .expect("seed stale snapshot");
            close_canonical_db_conn(stale_conn, "prime stale atc reader");

            let target = ExperienceRow {
                experience_id: 0,
                decision_id: 901,
                effect_id: 901,
                trace_id: "trc-transition-stale-target".to_string(),
                claim_id: "clm-transition-stale-target".to_string(),
                evidence_id: "evi-transition-stale-target".to_string(),
                state: ExperienceState::Planned,
                subsystem: ExperienceSubsystem::Liveness,
                decision_class: "liveness_transition".to_string(),
                subject: "BlueLake".to_string(),
                project_key: Some("/tmp/transition-atc-experience-stale".to_string()),
                policy_id: Some("liveness-incumbent-r1".to_string()),
                effect_kind: EffectKind::Probe,
                action: "ProbeAgent".to_string(),
                posterior: vec![
                    ("Alive".to_string(), 0.25),
                    ("Flaky".to_string(), 0.50),
                    ("Dead".to_string(), 0.25),
                ],
                expected_loss: 1.1,
                runner_up_action: Some("DeferProbe".to_string()),
                runner_up_loss: Some(1.5),
                evidence_summary: "target experience".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_011_000,
                dispatched_ts_micros: None,
                executed_ts_micros: None,
                resolved_ts_micros: None,
                features: Some(FeatureVector::zeroed()),
                feature_ext: None,
                context: None,
            };
            let stored = append_atc_experience(&cx, &writer_pool, &target)
                .await
                .into_result()
                .expect("append target experience");

            transition_atc_experience(
                &cx,
                &stale_pool,
                stored.experience_id,
                ExperienceState::Dispatched,
                1_700_000_000_011_500,
                None,
                None,
            )
            .await
            .into_result()
            .expect("stale pooled snapshot should recover via fresh-handle retry");

            let durable = fetch_durable_atc_experience_by_id(
                &cx,
                &writer_pool,
                i64::try_from(stored.experience_id).expect("experience id"),
            )
            .await
            .into_result()
            .expect("probe durable ATC experience")
            .expect("durable row");
            assert_eq!(durable.state, ExperienceState::Dispatched);
            assert_eq!(durable.dispatched_ts_micros, Some(1_700_000_000_011_500));
        });
    }

    #[test]
    fn file_backed_atc_writes_preserve_sqlite_quick_check() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("file_backed_atc_quick_check.db");
        let db_path = dir.path().join("file_backed_atc_quick_check.db");
        let large_context_blob = "atc-context-".repeat(160);

        rt.block_on(async {
            for idx in 0_u64..256 {
                let base_ts = 1_700_000_100_000_000_i64
                    .saturating_add(i64::try_from(idx).expect("idx fits i64") * 10_000);
                let row = ExperienceRow {
                    experience_id: 0,
                    decision_id: 10_000 + idx,
                    effect_id: 20_000 + idx,
                    trace_id: format!("trc-atc-quick-check-{idx}"),
                    claim_id: format!("clm-atc-quick-check-{idx}"),
                    evidence_id: format!("evi-atc-quick-check-{idx}"),
                    state: ExperienceState::Planned,
                    subsystem: ExperienceSubsystem::Liveness,
                    decision_class: "liveness_transition".to_string(),
                    subject: format!("Agent-{idx:03}"),
                    project_key: Some("/tmp/atc-quick-check".to_string()),
                    policy_id: Some("liveness-incumbent-r1".to_string()),
                    effect_kind: EffectKind::Probe,
                    action: "ProbeAgent".to_string(),
                    posterior: vec![
                        ("Alive".to_string(), 0.20),
                        ("Flaky".to_string(), 0.55),
                        ("Dead".to_string(), 0.25),
                    ],
                    expected_loss: 1.3,
                    runner_up_action: Some("DeferProbe".to_string()),
                    runner_up_loss: Some(1.9),
                    evidence_summary: format!("probe candidate {idx}"),
                    calibration_healthy: true,
                    safe_mode_active: false,
                    non_execution_reason: None,
                    outcome: None,
                    created_ts_micros: base_ts,
                    dispatched_ts_micros: None,
                    executed_ts_micros: None,
                    resolved_ts_micros: None,
                    features: Some(FeatureVector {
                        posterior_alive_bp: 2000,
                        posterior_flaky_bp: 5500,
                        expected_loss_bp: 130,
                        loss_gap_bp: 60,
                        risk_tier: FeatureVector::risk_tier_for(EffectKind::Probe),
                        ..FeatureVector::zeroed()
                    }),
                    feature_ext: None,
                    context: Some(serde_json::json!({
                        "large_blob": large_context_blob.clone(),
                        "sequence": idx,
                        "effect_family": {
                            "kind": "probe_agent",
                            "category": "probe"
                        }
                    })),
                };

                let stored = append_atc_experience(&cx, &pool, &row)
                    .await
                    .into_result()
                    .expect("append ATC experience");
                transition_atc_experience(
                    &cx,
                    &pool,
                    stored.experience_id,
                    ExperienceState::Dispatched,
                    base_ts + 2_000,
                    None,
                    Some(&serde_json::json!({
                        "execution": {
                            "status": "dispatched",
                            "attempt": idx + 1,
                        }
                    })),
                )
                .await
                .into_result()
                .expect("dispatch ATC experience");
                transition_atc_experience(
                    &cx,
                    &pool,
                    stored.experience_id,
                    ExperienceState::Executed,
                    base_ts + 4_000,
                    None,
                    Some(&serde_json::json!({
                        "execution": {
                            "status": "executed",
                            "result": "probe_ok",
                        }
                    })),
                )
                .await
                .into_result()
                .expect("execute ATC experience");
            }
        });

        pool.wal_checkpoint()
            .expect("checkpoint ATC quick-check database");

        let conn = crate::CanonicalDbConn::open_file(db_path.display().to_string())
            .expect("open canonical quick-check connection");
        conn.execute_raw(crate::schema::PRAGMA_CONN_SETTINGS_SQL)
            .expect("apply canonical quick-check pragmas");
        let rows = conn
            .query_sync("PRAGMA quick_check", &[])
            .expect("run quick_check");
        let detail = rows
            .first()
            .and_then(|row| row.get_named::<String>("quick_check").ok())
            .or_else(|| {
                rows.first().and_then(|row| {
                    row.get(0).and_then(|value| match value {
                        Value::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                })
            })
            .expect("quick_check detail");
        close_canonical_db_conn(conn, "ATC quick-check verification connection");

        assert_eq!(
            detail, "ok",
            "file-backed ATC writes must preserve canonical sqlite integrity"
        );
    }

    #[test]
    fn file_backed_atc_init_repairs_partial_schema_before_runtime_writes() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("partial_atc_schema.db");
        let conn = crate::CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("open canonical sqlite");
        conn.execute_raw(crate::schema::PRAGMA_DB_INIT_BASE_SQL)
            .expect("apply base pragmas");
        conn.execute_raw(
            "CREATE TABLE atc_experiences (
                experience_id INTEGER PRIMARY KEY,
                decision_id INTEGER NOT NULL
            )",
        )
        .expect("create partial experiences table");
        conn.execute_raw(
            "CREATE TABLE atc_experience_rollups (
                stratum_key TEXT PRIMARY KEY
            )",
        )
        .expect("create partial rollups table");
        drop(conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            ensure_file_backed_atc_pool_initialized(&cx, &pool)
                .await
                .into_result()
                .expect("ATC init should repair partial schema via migrations");
        });

        assert!(
            inspect_canonical_atc_schema(&pool)
                .expect("inspect canonical ATC schema")
                .is_empty(),
            "ATC init should converge the partial schema to the full required surface"
        );

        let verify_conn = crate::CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("open canonical sqlite verification connection");
        verify_conn
            .execute_raw(crate::schema::PRAGMA_CONN_SETTINGS_SQL)
            .expect("apply canonical verification pragmas");
        let rows = verify_conn
            .query_sync("PRAGMA quick_check", &[])
            .expect("run canonical quick_check after ATC init");
        let detail = rows
            .first()
            .and_then(|row| row.get_named::<String>("quick_check").ok())
            .or_else(|| {
                rows.first().and_then(|row| {
                    row.get(0).and_then(|value| match value {
                        Value::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                })
            })
            .expect("quick_check detail after ATC init");
        close_canonical_db_conn(verify_conn, "ATC init verification connection");
        assert_eq!(
            detail, "ok",
            "ATC init should leave a canonical sqlite file"
        );
    }

    #[test]
    fn reconstructed_file_backed_atc_runtime_writes_preserve_sqlite_quick_check() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_root = dir.path().join("storage");
        let db_path = dir.path().join("reconstructed_atc_runtime.sqlite3");
        let agent_dir = storage_root
            .join("projects")
            .join("reconstructed-project")
            .join("agents")
            .join("BrownKite");
        std::fs::create_dir_all(&agent_dir).expect("create archive agent dir");
        std::fs::write(
            agent_dir.join("profile.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "name": "BrownKite",
                "program": "codex-cli",
                "model": "gpt-5",
                "task_description": "reconstructed runtime atc test",
                "inception_ts": "2026-04-07T00:00:00Z",
                "last_active_ts": "2026-04-07T00:00:00Z",
                "attachments_policy": "auto",
                "contact_policy": "auto"
            }))
            .expect("serialize profile"),
        )
        .expect("write profile");

        crate::reconstruct::reconstruct_from_archive(&db_path, &storage_root)
            .expect("reconstruct database from archive");

        let cx = Cx::for_testing();
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            for idx in 0_u64..64 {
                let base_ts = 1_700_000_200_000_000_i64
                    .saturating_add(i64::try_from(idx).expect("idx fits i64") * 25_000);
                let row = ExperienceRow {
                    experience_id: 0,
                    decision_id: 30_000 + idx,
                    effect_id: 40_000 + idx,
                    trace_id: format!("trc-reconstruct-atc-{idx}"),
                    claim_id: format!("clm-reconstruct-atc-{idx}"),
                    evidence_id: format!("evi-reconstruct-atc-{idx}"),
                    state: ExperienceState::Planned,
                    subsystem: ExperienceSubsystem::Liveness,
                    decision_class: "liveness_transition".to_string(),
                    subject: "BrownKite".to_string(),
                    project_key: Some("/data/projects/asupersync".to_string()),
                    policy_id: Some("liveness-incumbent-r1".to_string()),
                    effect_kind: EffectKind::Probe,
                    action: "ProbeAgent".to_string(),
                    posterior: vec![
                        ("Alive".to_string(), 0.30),
                        ("Flaky".to_string(), 0.45),
                        ("Dead".to_string(), 0.25),
                    ],
                    expected_loss: 1.0,
                    runner_up_action: Some("DeferProbe".to_string()),
                    runner_up_loss: Some(1.4),
                    evidence_summary: format!("reconstructed runtime probe candidate {idx}"),
                    calibration_healthy: true,
                    safe_mode_active: false,
                    non_execution_reason: None,
                    outcome: None,
                    created_ts_micros: base_ts,
                    dispatched_ts_micros: None,
                    executed_ts_micros: None,
                    resolved_ts_micros: None,
                    features: Some(FeatureVector::zeroed()),
                    feature_ext: None,
                    context: Some(serde_json::json!({
                        "reconstructed": true,
                        "sequence": idx
                    })),
                };

                let stored = append_atc_experience(&cx, &pool, &row)
                    .await
                    .into_result()
                    .expect("append ATC experience on reconstructed db");
                transition_atc_experience(
                    &cx,
                    &pool,
                    stored.experience_id,
                    ExperienceState::Dispatched,
                    base_ts + 2_000,
                    None,
                    None,
                )
                .await
                .into_result()
                .expect("dispatch ATC experience");
                transition_atc_experience(
                    &cx,
                    &pool,
                    stored.experience_id,
                    ExperienceState::Executed,
                    base_ts + 4_000,
                    None,
                    Some(&serde_json::json!({
                        "execution": {
                            "status": "executed",
                            "path": "reconstructed_db_runtime"
                        }
                    })),
                )
                .await
                .into_result()
                .expect("execute ATC experience");
                update_atc_experience_rollup(
                    &cx,
                    &pool,
                    &format!("reconstructed::{idx}"),
                    "liveness",
                    "probe",
                    i32::from(FeatureVector::risk_tier_for(EffectKind::Probe)),
                    ExperienceState::Resolved,
                    true,
                    0.0,
                    0.0,
                    5_000,
                    base_ts + 8_000,
                )
                .await
                .into_result()
                .expect("update ATC rollup");
            }
        });

        pool.wal_checkpoint().expect("checkpoint reconstructed db");
        assert!(
            crate::pool::sqlite_file_is_healthy(&db_path)
                .expect("sqlite quick_check should succeed"),
            "reconstructed database must stay healthy after runtime ATC writes"
        );
    }

    /// br-bvq1x.11.7: ATC telemetry is isolated in the `atc.sqlite3` sidecar.
    /// Verifies the chokepoint redirect (create + write + read on the sidecar),
    /// that the primary mailbox DB never holds `atc_*` tables, and that the
    /// sidecar is created with private (0600) permissions.
    #[test]
    fn atc_telemetry_is_isolated_in_sidecar_and_absent_from_main_db() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("storage.sqlite3");
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        // The sidecar path is the sibling atc.sqlite3.
        let atc_path = pool
            .atc_sqlite_path()
            .expect("file-backed pool exposes a sidecar path");
        assert!(
            atc_path.ends_with("atc.sqlite3"),
            "sidecar path should be atc.sqlite3: {atc_path}"
        );
        assert_ne!(atc_path, db_path.display().to_string());

        let list_atc_tables = |path: &str| -> Vec<String> {
            let conn =
                crate::CanonicalDbConn::open_file(path).expect("open db for atc table probe");
            let rows = conn
                .query_sync(
                    "SELECT name FROM sqlite_master WHERE type='table' \
                     AND name LIKE 'atc\\_%' ESCAPE '\\' ORDER BY name",
                    &[],
                )
                .expect("query atc tables");
            rows.into_iter()
                .filter_map(|r| r.get_named::<String>("name").ok())
                .collect()
        };

        // After init, the primary mailbox DB must have NO atc_* tables.
        assert!(
            list_atc_tables(db_path.to_str().expect("utf8 db path")).is_empty(),
            "primary mailbox DB must not contain atc_* tables after init"
        );

        let row = ExperienceRow {
            experience_id: 0,
            decision_id: 777,
            effect_id: 888,
            trace_id: "trc-sidecar-iso".to_string(),
            claim_id: "clm-sidecar-iso".to_string(),
            evidence_id: "evi-sidecar-iso".to_string(),
            state: ExperienceState::Planned,
            subsystem: ExperienceSubsystem::Liveness,
            decision_class: "liveness_transition".to_string(),
            subject: "TealOtter".to_string(),
            project_key: Some("/tmp/sidecar-iso".to_string()),
            policy_id: Some("liveness-incumbent-r1".to_string()),
            effect_kind: EffectKind::Probe,
            action: "ProbeAgent".to_string(),
            posterior: vec![("Alive".to_string(), 0.6), ("Dead".to_string(), 0.4)],
            expected_loss: 1.0,
            runner_up_action: Some("DeferProbe".to_string()),
            runner_up_loss: Some(1.4),
            evidence_summary: "sidecar isolation probe".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            non_execution_reason: None,
            outcome: None,
            created_ts_micros: 1_700_000_300_000_000,
            dispatched_ts_micros: None,
            executed_ts_micros: None,
            resolved_ts_micros: None,
            features: Some(FeatureVector::zeroed()),
            feature_ext: None,
            context: None,
        };
        let stored_id = rt
            .block_on(insert_experience(&cx, &pool, row))
            .into_result()
            .expect("insert ATC experience");
        assert!(stored_id > 0, "experience id should be assigned");

        // The write must land in the sidecar, which now exists with the schema.
        assert!(
            std::path::Path::new(&atc_path).exists(),
            "sidecar file must exist after the first ATC write"
        );
        let sidecar_tables = list_atc_tables(&atc_path);
        assert!(
            sidecar_tables.contains(&"atc_experiences".to_string()),
            "sidecar must contain atc_experiences: {sidecar_tables:?}"
        );

        // Read back through the chokepoint (also targets the sidecar).
        let fetched = rt
            .block_on(fetch_durable_atc_experience_by_id(
                &cx,
                &pool,
                i64::try_from(stored_id).expect("id fits i64"),
            ))
            .into_result()
            .expect("fetch ATC experience");
        assert!(
            fetched.is_some(),
            "experience must be readable from the sidecar"
        );

        // The primary mailbox DB stays free of atc_* tables after ATC writes.
        assert!(
            list_atc_tables(db_path.to_str().expect("utf8 db path")).is_empty(),
            "primary mailbox DB must remain free of atc_* tables after ATC writes"
        );

        // The sidecar is created private (0600) — same sensitivity as the mail DB.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&atc_path)
                .expect("stat sidecar")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "ATC sidecar must be created with 0600 perms");
        }
    }

    #[test]
    fn fetch_open_atc_experiences_filters_subject_case_insensitively() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("fetch_open_atc_experiences_subject_case.db");

        rt.block_on(async {
            let blue = ExperienceRow {
                experience_id: 0,
                decision_id: 88,
                effect_id: 201,
                trace_id: "trc-fetch-open-blue".to_string(),
                claim_id: "clm-fetch-open-blue".to_string(),
                evidence_id: "evi-fetch-open-blue".to_string(),
                state: ExperienceState::Open,
                subsystem: ExperienceSubsystem::Conflict,
                decision_class: "reservation_conflict".to_string(),
                subject: "BlueLake".to_string(),
                project_key: Some("/tmp/fetch-open-atc".to_string()),
                policy_id: Some("conflict-r1".to_string()),
                effect_kind: EffectKind::Advisory,
                action: "RecommendReservation".to_string(),
                posterior: vec![("Clear".to_string(), 0.20), ("Conflict".to_string(), 0.80)],
                expected_loss: 1.4,
                runner_up_action: Some("Wait".to_string()),
                runner_up_loss: Some(1.8),
                evidence_summary: "conflict signal".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_001_000,
                dispatched_ts_micros: Some(1_700_000_000_001_050),
                executed_ts_micros: Some(1_700_000_000_001_100),
                resolved_ts_micros: None,
                features: Some(FeatureVector::zeroed()),
                feature_ext: None,
                context: None,
            };
            let red = ExperienceRow {
                experience_id: 0,
                decision_id: 89,
                effect_id: 202,
                trace_id: "trc-fetch-open-red".to_string(),
                claim_id: "clm-fetch-open-red".to_string(),
                evidence_id: "evi-fetch-open-red".to_string(),
                state: ExperienceState::Open,
                subsystem: ExperienceSubsystem::Conflict,
                decision_class: "reservation_conflict".to_string(),
                subject: "RedPeak".to_string(),
                project_key: Some("/tmp/fetch-open-atc".to_string()),
                policy_id: Some("conflict-r1".to_string()),
                effect_kind: EffectKind::Advisory,
                action: "RecommendReservation".to_string(),
                posterior: vec![("Clear".to_string(), 0.40), ("Conflict".to_string(), 0.60)],
                expected_loss: 1.2,
                runner_up_action: Some("Wait".to_string()),
                runner_up_loss: Some(1.6),
                evidence_summary: "other conflict signal".to_string(),
                calibration_healthy: true,
                safe_mode_active: false,
                non_execution_reason: None,
                outcome: None,
                created_ts_micros: 1_700_000_000_002_000,
                dispatched_ts_micros: Some(1_700_000_000_002_050),
                executed_ts_micros: Some(1_700_000_000_002_100),
                resolved_ts_micros: None,
                features: Some(FeatureVector::zeroed()),
                feature_ext: None,
                context: None,
            };

            append_atc_experience(&cx, &pool, &blue)
                .await
                .into_result()
                .expect("append BlueLake experience");
            append_atc_experience(&cx, &pool, &red)
                .await
                .into_result()
                .expect("append RedPeak experience");

            let lower = fetch_open_atc_experiences(&cx, &pool, Some("bluelake"), 10)
                .await
                .into_result()
                .expect("fetch lowercase subject");
            assert_eq!(lower.len(), 1);
            assert_eq!(lower[0].subject, "BlueLake");

            let upper = fetch_open_atc_experiences(&cx, &pool, Some("BLUELAKE"), 10)
                .await
                .into_result()
                .expect("fetch uppercase subject");
            assert_eq!(upper.len(), 1);
            assert_eq!(upper[0].subject, "BlueLake");

            let unmatched = fetch_open_atc_experiences(&cx, &pool, Some("greenowl"), 10)
                .await
                .into_result()
                .expect("fetch unmatched subject");
            assert!(unmatched.is_empty());
        });
    }

    #[test]
    fn resolve_atc_experience_requires_open_state_for_in_memory_pool() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let cfg = crate::pool::DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create in-memory pool");

        rt.block_on(async {
            let row = make_insert_experience_test_row(
                410,
                510,
                ExperienceState::Executed,
                1_700_000_000_030_000,
            );
            let stored = append_atc_experience(&cx, &pool, &row)
                .await
                .into_result()
                .expect("append executed experience");

            let outcome = ExperienceOutcome {
                observed_ts_micros: 1_700_000_000_031_000,
                label: "acknowledged".to_string(),
                correct: true,
                actual_loss: Some(0.1),
                regret: Some(0.0),
                evidence: Some(serde_json::json!({
                    "source": "review-round-2",
                    "branch": "memory"
                })),
            };

            let stored_before = fetch_durable_atc_experience_by_id(
                &cx,
                &pool,
                i64::try_from(stored.experience_id).expect("experience id fits i64"),
            )
            .await
            .into_result()
            .expect("fetch inserted experience")
            .expect("experience should exist before resolve attempt");
            assert_eq!(stored_before.state, ExperienceState::Executed);

            let invalid = resolve_atc_experience(&cx, &pool, stored.experience_id, &outcome)
                .await
                .into_result();
            assert!(
                matches!(
                    invalid,
                    Err(asupersync::OutcomeError::Err(DbError::InvalidArgument {
                        field: "state",
                        ..
                    }))
                ),
                "expected invalid state error, got {invalid:?}"
            );

            let stored_after = fetch_durable_atc_experience_by_id(
                &cx,
                &pool,
                i64::try_from(stored.experience_id).expect("experience id fits i64"),
            )
            .await
            .into_result()
            .expect("fetch persisted experience")
            .expect("experience should still exist");
            assert_eq!(stored_after.state, ExperienceState::Executed);
            assert!(stored_after.outcome.is_none());

            let missing =
                resolve_atc_experience(&cx, &pool, stored.experience_id + 10_000, &outcome)
                    .await
                    .into_result();
            assert!(matches!(
                missing,
                Err(asupersync::OutcomeError::Err(DbError::NotFound {
                    entity: "experience",
                    ..
                }))
            ));
        });
    }

    #[test]
    fn resolve_atc_experience_requires_open_state_for_file_backed_pool() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("resolve_atc_experience_requires_open_state.db");

        rt.block_on(async {
            let row = make_insert_experience_test_row(
                411,
                511,
                ExperienceState::Executed,
                1_700_000_000_040_000,
            );
            let stored = append_atc_experience(&cx, &pool, &row)
                .await
                .into_result()
                .expect("append executed experience");

            let outcome = ExperienceOutcome {
                observed_ts_micros: 1_700_000_000_041_000,
                label: "acknowledged".to_string(),
                correct: true,
                actual_loss: Some(0.1),
                regret: Some(0.0),
                evidence: Some(serde_json::json!({
                    "source": "review-round-2",
                    "branch": "file-backed"
                })),
            };

            let invalid = resolve_atc_experience(&cx, &pool, stored.experience_id, &outcome)
                .await
                .into_result();
            assert!(matches!(
                invalid,
                Err(asupersync::OutcomeError::Err(DbError::InvalidArgument {
                    field: "state",
                    ..
                }))
            ));

            let stored_after = fetch_durable_atc_experience_by_id(
                &cx,
                &pool,
                i64::try_from(stored.experience_id).expect("experience id fits i64"),
            )
            .await
            .into_result()
            .expect("fetch persisted experience")
            .expect("experience should still exist");
            assert_eq!(stored_after.state, ExperienceState::Executed);
            assert!(stored_after.outcome.is_none());

            let missing =
                resolve_atc_experience(&cx, &pool, stored.experience_id + 10_000, &outcome)
                    .await
                    .into_result();
            assert!(matches!(
                missing,
                Err(asupersync::OutcomeError::Err(DbError::NotFound {
                    entity: "experience",
                    ..
                }))
            ));
        });
    }

    fn create_inbox_stats_table_for_test(conn: &crate::DbConn) {
        conn.execute_raw(
            "CREATE TABLE IF NOT EXISTS inbox_stats (\
                agent_id INTEGER PRIMARY KEY, \
                total_count INTEGER NOT NULL DEFAULT 0, \
                unread_count INTEGER NOT NULL DEFAULT 0, \
                ack_pending_count INTEGER NOT NULL DEFAULT 0, \
                last_message_ts INTEGER\
            )",
        )
        .expect("create inbox_stats table");
    }

    async fn legacy_list_recent_contact_agent_ids(
        cx: &Cx,
        pool: &DbPool,
        project_id: i64,
        sender_id: i64,
        candidate_ids: &[i64],
        since_ts: i64,
    ) -> Outcome<Vec<i64>, DbError> {
        if candidate_ids.is_empty() {
            return Outcome::Ok(vec![]);
        }

        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let tracked = tracked(&*conn);
        let capped_ids = &candidate_ids[..candidate_ids.len().min(MAX_IN_CLAUSE_ITEMS)];
        let placeholders = placeholders(capped_ids.len());

        let sql_sent = format!(
            "SELECT DISTINCT r.agent_id \
             FROM message_recipients r \
             JOIN messages m ON m.id = r.message_id \
             WHERE m.project_id = ? AND m.sender_id = ? AND m.created_ts > ? \
               AND r.agent_id IN ({placeholders})"
        );
        let mut params_sent: Vec<Value> = Vec::with_capacity(capped_ids.len() + 3);
        params_sent.push(Value::BigInt(project_id));
        params_sent.push(Value::BigInt(sender_id));
        params_sent.push(Value::BigInt(since_ts));
        for id in capped_ids {
            params_sent.push(Value::BigInt(*id));
        }

        let sql_recv = format!(
            "SELECT DISTINCT m.sender_id \
             FROM messages m \
             JOIN message_recipients r ON r.message_id = m.id \
             WHERE m.project_id = ? AND r.agent_id = ? AND m.created_ts > ? \
               AND m.sender_id IN ({placeholders})"
        );
        let mut params_recv: Vec<Value> = Vec::with_capacity(capped_ids.len() + 3);
        params_recv.push(Value::BigInt(project_id));
        params_recv.push(Value::BigInt(sender_id));
        params_recv.push(Value::BigInt(since_ts));
        for id in capped_ids {
            params_recv.push(Value::BigInt(*id));
        }

        let sent_rows = map_sql_outcome(traw_query(cx, &tracked, &sql_sent, &params_sent).await);
        let recv_rows = map_sql_outcome(traw_query(cx, &tracked, &sql_recv, &params_recv).await);

        match (sent_rows, recv_rows) {
            (Outcome::Ok(sent), Outcome::Ok(recv)) => {
                let mut out = Vec::with_capacity(sent.len() + recv.len());
                for row in sent {
                    let id: i64 = match row.get_named("agent_id") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(id);
                }
                for row in recv {
                    let id: i64 = match row.get_named("sender_id") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(id);
                }
                out.sort_unstable();
                out.dedup();
                Outcome::Ok(out)
            }
            (Outcome::Err(e), _) | (_, Outcome::Err(e)) => Outcome::Err(e),
            (Outcome::Cancelled(r), _) | (_, Outcome::Cancelled(r)) => Outcome::Cancelled(r),
            (Outcome::Panicked(p), _) | (_, Outcome::Panicked(p)) => Outcome::Panicked(p),
        }
    }

    #[test]
    fn placeholder_cache_matches_dynamic_for_common_arities() {
        for n in 1..=64 {
            assert_eq!(placeholders(n), build_placeholders(n), "arity={n}");
        }
    }

    #[test]
    fn release_reservation_chunk_plan_none_within_bind_limits() {
        assert_eq!(release_reservation_chunk_plan(64, 64), None);
        assert_eq!(
            release_reservation_chunk_plan(
                MAX_RELEASE_RESERVATION_CHUNK_ITEMS,
                MAX_RELEASE_RESERVATION_CHUNK_ITEMS
            ),
            None
        );
    }

    #[test]
    fn release_reservation_chunk_plan_chunks_ids_when_combined_filters_exceed_limit() {
        let path_count = 400;
        let id_count = 700;
        let Some((target, chunk_size)) = release_reservation_chunk_plan(path_count, id_count)
        else {
            panic!("expected chunking plan");
        };
        assert_eq!(target, ReleaseReservationChunkTarget::ReservationIds);
        assert_eq!(chunk_size, MAX_RELEASE_RESERVATION_CHUNK_ITEMS);
        assert!(
            path_count + chunk_size <= MAX_RELEASE_RESERVATION_FILTER_ITEMS,
            "chunked ids must fit SQLite bind limit"
        );
    }

    #[test]
    fn release_reservation_chunk_plan_chunks_paths_when_ids_consume_budget() {
        let path_count = 600;
        let id_count = 500;
        let Some((target, chunk_size)) = release_reservation_chunk_plan(path_count, id_count)
        else {
            panic!("expected chunking plan");
        };
        assert_eq!(target, ReleaseReservationChunkTarget::Paths);
        assert_eq!(chunk_size, MAX_RELEASE_RESERVATION_CHUNK_ITEMS);
        assert!(
            id_count + chunk_size <= MAX_RELEASE_RESERVATION_FILTER_ITEMS,
            "chunked paths must fit SQLite bind limit"
        );
    }

    #[test]
    fn apply_release_markers_keeps_only_rows_released_by_this_call() {
        let reservations = vec![
            FileReservationRow {
                id: Some(11),
                project_id: 1,
                agent_id: 2,
                path_pattern: "src/a.rs".to_string(),
                exclusive: 1,
                reason: "first".to_string(),
                created_ts: 10,
                expires_ts: 20,
                released_ts: None,
            },
            FileReservationRow {
                id: Some(12),
                project_id: 1,
                agent_id: 2,
                path_pattern: "src/b.rs".to_string(),
                exclusive: 1,
                reason: "second".to_string(),
                created_ts: 11,
                expires_ts: 21,
                released_ts: None,
            },
        ];

        let released = apply_release_markers(
            reservations,
            &[ReleasedReservationMarker {
                id: 12,
                released_ts: 99,
            }],
        );

        assert_eq!(released.len(), 1);
        assert_eq!(released[0].id, Some(12));
        assert_eq!(released[0].released_ts, Some(99));
    }

    #[test]
    fn placeholder_cache_caps_at_max_items() {
        let max = placeholders(MAX_IN_CLAUSE_ITEMS);
        let overflow = placeholders(MAX_IN_CLAUSE_ITEMS + 100);
        assert_eq!(overflow, max);
    }

    #[test]
    fn approved_contact_sql_cache_matches_dynamic_template() {
        for n in [1, 2, 8, 64, MAX_IN_CLAUSE_ITEMS, MAX_IN_CLAUSE_ITEMS + 25] {
            let capped = n.min(MAX_IN_CLAUSE_ITEMS);
            let expected =
                build_approved_contact_sql_with_placeholders(&build_placeholders(capped));
            assert_eq!(approved_contact_sql(n), expected, "arity={n}");
        }
    }

    #[test]
    fn recent_contact_union_sql_cache_matches_dynamic_template() {
        for n in [1, 2, 8, 64, MAX_IN_CLAUSE_ITEMS, MAX_IN_CLAUSE_ITEMS + 25] {
            let capped = n.min(MAX_IN_CLAUSE_ITEMS);
            let expected =
                build_recent_contact_union_sql_with_placeholders(&build_placeholders(capped));
            assert_eq!(recent_contact_union_sql(n), expected, "arity={n}");
        }
    }

    #[test]
    fn sql_template_caches_are_thread_safe() {
        let mut handles = Vec::new();
        for _ in 0..10 {
            handles.push(std::thread::spawn(|| {
                for n in [1, 3, 7, 64, MAX_IN_CLAUSE_ITEMS, MAX_IN_CLAUSE_ITEMS + 10] {
                    let _ = placeholders(n);
                    let _ = approved_contact_sql(n);
                    let _ = recent_contact_union_sql(n);
                }
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("template cache access across threads should not panic");
        }
    }

    #[test]
    fn sanitize_empty_returns_none() {
        assert!(sanitize_fts_query("").is_none());
        assert!(sanitize_fts_query("   ").is_none());
    }

    #[test]
    fn sanitize_unsearchable_patterns() {
        for p in ["*", "**", "***", ".", "..", "...", "?", "??", "???"] {
            assert!(sanitize_fts_query(p).is_none(), "expected None for '{p}'");
        }
    }

    #[test]
    fn sanitize_bare_boolean_operators() {
        assert!(sanitize_fts_query("AND").is_none());
        assert!(sanitize_fts_query("OR").is_none());
        assert!(sanitize_fts_query("NOT").is_none());
        assert!(sanitize_fts_query("and").is_none());
    }

    #[test]
    fn sanitize_operator_only_sequences() {
        assert!(sanitize_fts_query("AND OR NOT").is_none());
        assert!(sanitize_fts_query("(AND) OR").is_none());
        assert!(sanitize_fts_query("NEAR AND").is_none());
    }

    #[test]
    fn sanitize_stopwords_only_with_noise_is_none() {
        assert!(sanitize_fts_query(" (AND) OR NOT NEAR ").is_none());
    }

    #[test]
    fn sanitize_punctuation_only_is_none() {
        assert!(sanitize_fts_query("!!!").is_none());
        assert!(sanitize_fts_query("((()))").is_none());
    }

    #[test]
    fn sanitize_strips_leading_wildcard() {
        assert_eq!(sanitize_fts_query("*foo"), Some("foo".to_string()));
        assert_eq!(sanitize_fts_query("**foo"), Some("foo".to_string()));
    }

    #[test]
    fn sanitize_strips_trailing_lone_wildcard() {
        assert_eq!(sanitize_fts_query("foo *"), Some("foo".to_string()));
        assert!(sanitize_fts_query(" *").is_none());
    }

    #[test]
    fn sanitize_collapses_multiple_spaces() {
        assert_eq!(
            sanitize_fts_query("foo  bar   baz"),
            Some("foo bar baz".to_string())
        );
    }

    #[test]
    fn sanitize_preserves_prefix_wildcard() {
        assert_eq!(sanitize_fts_query("migrat*"), Some("migrat*".to_string()));
    }

    #[test]
    fn sanitize_preserves_boolean_with_terms() {
        assert_eq!(
            sanitize_fts_query("plan AND users"),
            Some("plan AND users".to_string())
        );
    }

    #[test]
    fn sanitize_quotes_hyphenated_tokens() {
        assert_eq!(
            sanitize_fts_query("POL-358"),
            Some("\"POL-358\"".to_string())
        );
        assert_eq!(
            sanitize_fts_query("search for FEAT-123 and bd-42"),
            Some("search for \"FEAT-123\" and \"bd-42\"".to_string())
        );
    }

    #[test]
    fn sanitize_leaves_already_quoted() {
        assert_eq!(
            sanitize_fts_query("\"build plan\""),
            Some("\"build plan\"".to_string())
        );
    }

    #[test]
    fn sanitize_strips_sql_comment_markers() {
        // Double-dash (SQL line comment)
        assert_eq!(sanitize_fts_query("--a"), Some("a".to_string()));
        assert_eq!(
            sanitize_fts_query("foo -- bar"),
            Some("foo bar".to_string())
        );
        assert!(sanitize_fts_query("--").is_none());
        // Block comment markers
        assert_eq!(
            sanitize_fts_query("foo /* bar"),
            Some("foo bar".to_string())
        );
        assert_eq!(
            sanitize_fts_query("foo */ bar"),
            Some("foo bar".to_string())
        );
    }

    #[test]
    fn sanitize_simple_term() {
        assert_eq!(sanitize_fts_query("hello"), Some("hello".to_string()));
    }

    #[test]
    fn extract_terms_basic() {
        let terms = extract_like_terms("foo AND bar OR baz", 5);
        assert_eq!(terms, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn extract_terms_skips_stopwords() {
        let terms = extract_like_terms("AND OR NOT NEAR", 5);
        assert!(terms.is_empty());
    }

    #[test]
    fn extract_terms_skips_short() {
        let terms = extract_like_terms("a b cd ef", 5);
        assert_eq!(terms, vec!["cd", "ef"]);
    }

    #[test]
    fn extract_terms_only_single_char_tokens_returns_empty() {
        let terms = extract_like_terms("a b c d e", 8);
        assert!(terms.is_empty());
    }

    #[test]
    fn extract_terms_mixed_single_and_multi_char_tokens() {
        let terms = extract_like_terms("a bb c dd e ff", 8);
        assert_eq!(terms, vec!["bb", "dd", "ff"]);
    }

    #[test]
    fn extract_terms_respects_max() {
        let terms = extract_like_terms("alpha beta gamma delta epsilon", 3);
        assert_eq!(terms.len(), 3);
    }

    #[test]
    fn extract_terms_deduplicates() {
        let terms = extract_like_terms("foo bar foo bar", 5);
        assert_eq!(terms, vec!["foo", "bar"]);
    }

    #[test]
    fn like_escape_special_chars() {
        assert_eq!(like_escape("100%"), "100\\%");
        assert_eq!(like_escape("a_b"), "a\\_b");
        assert_eq!(like_escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn like_escape_combined_wildcards_and_backslashes() {
        assert_eq!(
            like_escape(r"100%_done\path\_cache%"),
            r"100\%\_done\\path\\\_cache\%"
        );
    }

    #[test]
    fn quote_hyphenated_no_hyphen() {
        assert_eq!(quote_hyphenated_tokens("hello world"), "hello world");
    }

    #[test]
    fn quote_hyphenated_single() {
        assert_eq!(quote_hyphenated_tokens("POL-358"), "\"POL-358\"");
    }

    #[test]
    fn quote_hyphenated_multi_segment() {
        assert_eq!(quote_hyphenated_tokens("foo-bar-baz"), "\"foo-bar-baz\"");
    }

    #[test]
    fn quote_hyphenated_deep_multi_segment() {
        assert_eq!(quote_hyphenated_tokens("a-b-c-d-e-f"), "\"a-b-c-d-e-f\"");
    }

    #[test]
    fn quote_hyphenated_in_context() {
        assert_eq!(
            quote_hyphenated_tokens("search FEAT-123 done"),
            "search \"FEAT-123\" done"
        );
    }

    #[test]
    fn quote_hyphenated_already_quoted() {
        assert_eq!(
            quote_hyphenated_tokens("\"already-quoted\""),
            "\"already-quoted\""
        );
    }

    #[test]
    fn quote_hyphenated_non_ascii() {
        // Non-ASCII chars break ASCII-alphanumeric token spans, so café-latte
        // is NOT recognized as a single hyphenated token (FTS5 default tokenizer
        // also splits on non-ASCII). The important thing is that multi-byte
        // UTF-8 chars pass through without corruption.
        assert_eq!(quote_hyphenated_tokens("café-latte"), "café-latte");
        // Non-ASCII without hyphens should pass through unchanged
        assert_eq!(quote_hyphenated_tokens("日本語"), "日本語");
        // Mixed: ASCII hyphenated + non-ASCII plain - UTF-8 must not corrupt
        assert_eq!(
            quote_hyphenated_tokens("foo-bar 日本語"),
            "\"foo-bar\" 日本語"
        );
        // 4-byte UTF-8 (emoji) must survive
        assert_eq!(quote_hyphenated_tokens("test-case 🎉"), "\"test-case\" 🎉");
    }

    #[test]
    fn register_agent_then_get_agent_by_name_succeeds() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_then_get_agent.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-agent-repro-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let registered = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("first registration"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            assert!(registered.id.is_some(), "register should assign id");

            let fetched = get_agent(&cx, &pool, project_id, "BlueLake")
                .await
                .into_result()
                .expect("get_agent should find newly registered agent");
            assert_eq!(fetched.name, "BlueLake");
            assert_eq!(fetched.program, "codex-cli");
            assert_eq!(fetched.model, "gpt-5");
            assert_eq!(fetched.id, registered.id);
        });
    }

    #[test]
    fn register_agent_succeeds_after_runtime_migrations() {
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_runtime_schema.db");
        let database_url = format!("sqlite:///{}", db_path.display());
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();
        let init_cfg = crate::pool::DbPoolConfig {
            database_url: database_url.clone(),
            min_connections: 1,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let init_pool = crate::create_pool(&init_cfg).expect("initialize runtime schema");
        rt.block_on(async {
            let init_conn = init_pool
                .acquire(&cx)
                .await
                .into_result()
                .expect("acquire initialized runtime schema");
            drop(init_conn);
        });
        drop(init_pool);

        let cfg = crate::pool::DbPoolConfig {
            database_url,
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-runtime-agent-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let registered = register_agent(
                &cx,
                &pool,
                project_id,
                "SilentRiver",
                "codex-cli",
                "gpt-5",
                Some("runtime schema registration"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent on runtime schema");

            assert!(registered.id.is_some(), "register should assign id");
            assert_eq!(registered.name, "SilentRiver");
            assert_eq!(registered.program, "codex-cli");
        });
    }

    #[test]
    fn register_agent_runtime_migration_three_slash_url_is_fresh_handle_durable() {
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_three_slash_runtime.db");
        let database_url = format!("sqlite://{}", db_path.display());
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();
        let cfg = crate::pool::DbPoolConfig {
            database_url,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let project = ensure_project(
                &cx,
                &pool,
                "/data/projects/mcp-agent-mail-three-slash-durability",
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("three-slash durability"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
        });

        let db_path_str = db_path.to_str().expect("utf8 db path");
        let verify =
            crate::CanonicalDbConn::open_file(db_path_str).expect("open fresh canonical handle");
        let project_rows = verify
            .query_sync(
                "SELECT count(*) AS count FROM projects \
                 WHERE slug = 'data-projects-mcp-agent-mail-three-slash-durability'",
                &[],
            )
            .expect("query fresh project count");
        let agent_rows = verify
            .query_sync(
                "SELECT count(*) AS count FROM agents WHERE name = 'BlueLake'",
                &[],
            )
            .expect("query fresh agent count");
        assert_eq!(
            project_rows
                .first()
                .and_then(|row| row.get_named::<i64>("count").ok()),
            Some(1),
            "project must be visible to a fresh canonical handle"
        );
        assert_eq!(
            agent_rows
                .first()
                .and_then(|row| row.get_named::<i64>("count").ok()),
            Some(1),
            "agent must be visible to a fresh canonical handle"
        );
    }

    #[test]
    fn register_agent_runtime_migration_extra_slash_url_is_fresh_handle_durable() {
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_extra_slash_runtime.db");
        let database_url = format!("sqlite:////{}", db_path.display());
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();
        let cfg = crate::pool::DbPoolConfig {
            database_url,
            min_connections: 1,
            max_connections: 1,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let project = ensure_project(
                &cx,
                &pool,
                "/data/projects/mcp-agent-mail-extra-slash-durability",
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("extra-slash durability"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
        });

        let db_path_str = db_path.to_str().expect("utf8 db path");
        let verify =
            crate::CanonicalDbConn::open_file(db_path_str).expect("open fresh canonical handle");
        let project_rows = verify
            .query_sync(
                "SELECT count(*) AS count FROM projects \
                 WHERE slug = 'data-projects-mcp-agent-mail-extra-slash-durability'",
                &[],
            )
            .expect("query fresh project count");
        let agent_rows = verify
            .query_sync(
                "SELECT count(*) AS count FROM agents WHERE name = 'BlueLake'",
                &[],
            )
            .expect("query fresh agent count");
        assert_eq!(
            project_rows
                .first()
                .and_then(|row| row.get_named::<i64>("count").ok()),
            Some(1),
            "project must be visible to a fresh canonical handle"
        );
        assert_eq!(
            agent_rows
                .first()
                .and_then(|row| row.get_named::<i64>("count").ok()),
            Some(1),
            "agent must be visible to a fresh canonical handle"
        );
        drop(pool);
    }

    #[test]
    fn register_agent_case_insensitive_reuses_existing_row() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_case_insensitive_reuse.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        // Simulate environments where NOCASE uniqueness may be missing.
        init_conn
            .execute_raw("DROP INDEX IF EXISTS idx_agents_project_name_nocase")
            .ok();
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-agent-case-reuse-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let initial = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("first"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("initial register");

            let updated = register_agent(
                &cx,
                &pool,
                project_id,
                "bluelake",
                "codex-cli",
                "gpt-5.1",
                Some("second"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("case-insensitive register");

            assert_eq!(
                updated.id, initial.id,
                "case-only name differences must reuse the same agent row"
            );
            assert_eq!(updated.model, "gpt-5.1");

            let agents = list_agents(&cx, &pool, project_id)
                .await
                .into_result()
                .expect("list agents");
            assert_eq!(agents.len(), 1);
        });
    }

    #[test]
    fn get_agent_resolves_case_variant_duplicates_deterministically() {
        // GH#169: when a concurrent multi-pane registration race leaves two
        // case-variant rows (e.g. `bluelake` and `BlueLake`) under the
        // BINARY-unique agents table, get_agent must resolve EVERY casing to the
        // same canonical (first-registered / lowest-id) row. Otherwise a file
        // reservation granted under one resolved id can never be released via
        // another casing (the lease leaks until TTL).
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("get_agent_case_variant_dedup.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        init_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("initialize base schema");
        // Older DBs may lack the NOCASE guard, so case-variant rows can coexist.
        init_conn
            .execute_raw("DROP INDEX IF EXISTS idx_agents_project_name_nocase")
            .ok();
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-get-agent-dup-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            // First registration -> canonical lowest-id row.
            let first = register_agent(
                &cx,
                &pool,
                project_id,
                "bluelake",
                "codex-cli",
                "gpt-5",
                Some("first"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register bluelake");
            let canonical_id = first.id.expect("first id");

            // Inject a second, exact-case-variant row directly (the race that
            // register_agent's NOCASE pre-check would otherwise have collapsed).
            // Use the pool's own connection so the duplicate is guaranteed
            // visible to the subsequent get_agent reads.
            let now = now_micros();
            let dup_sql = "INSERT INTO agents (project_id, name, program, model, task_description, \
                 inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt) \
                 VALUES (?, 'BlueLake', 'codex-cli', 'gpt-5', '', ?, ?, 'auto', 'auto', 0)";
            let dup_params = [
                Value::BigInt(project_id),
                Value::BigInt(now),
                Value::BigInt(now),
            ];
            {
                let conn = acquire_conn(&cx, &pool).await.into_result().expect("conn");
                let tracked = tracked(&*conn);
                map_sql_outcome(traw_execute(&cx, &tracked, dup_sql, &dup_params).await)
                    .into_result()
                    .expect("insert duplicate case variant");
            }
            // Clear any cached name->row mapping so resolution goes through SQL.
            crate::read_cache().clear();

            // Every casing must resolve to the canonical (lowest-id) row.
            for casing in ["BlueLake", "bluelake", "BLUELAKE", "BlUeLaKe"] {
                let resolved = get_agent(&cx, &pool, project_id, casing)
                    .await
                    .into_result()
                    .unwrap_or_else(|e| panic!("get_agent({casing}) failed: {e:?}"));
                assert_eq!(
                    resolved.id,
                    Some(canonical_id),
                    "casing {casing:?} must resolve to the canonical lowest-id row"
                );
                crate::read_cache().clear();
            }
        });
    }

    #[test]
    fn list_agents_deduplicates_case_insensitive_names_without_window_query() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("list_agents_case_dedup_no_window.db");

        rt.block_on(async {
            let project = ensure_project(&cx, &pool, "/tmp/list-agents-case-dedup")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire connection");
            conn.execute_raw("DROP INDEX IF EXISTS idx_agents_project_name_nocase")
                .expect("drop nocase unique index");
            conn.execute_raw(&format!(
                "INSERT INTO agents \
                 (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES \
                 ({project_id}, 'BlueLake', 'codex-cli', 'gpt-5', 'older', 1, 10, 'auto', 'auto'), \
                 ({project_id}, 'bluelake', 'codex-cli', 'gpt-5', 'newer', 2, 20, 'auto', 'auto'), \
                 ({project_id}, 'GreenField', 'codex-cli', 'gpt-5', 'newest', 3, 30, 'auto', 'auto')"
            ))
            .expect("insert duplicate historical agents");
            drop(conn);

            let agents = list_agents(&cx, &pool, project_id)
                .await
                .into_result()
                .expect("list agents");

            assert_eq!(
                agents.iter().map(|agent| agent.name.as_str()).collect::<Vec<_>>(),
                vec!["GreenField", "bluelake"]
            );
            assert_eq!(agents.len(), 2);
        });
    }

    #[test]
    fn list_agents_bounded_applies_limit_and_activity_floor() {
        // GH#154 item 3: list_agents_bounded must cap the result (keeping the
        // most-recently-active agents) and optionally exclude agents idle past
        // an activity floor, without changing the de-dup / ordering contract.
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("list_agents_bounded.db");

        rt.block_on(async {
            let project = ensure_project(&cx, &pool, "/tmp/list-agents-bounded")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire connection");
            // Five distinct agents with strictly increasing last_active_ts.
            conn.execute_raw(&format!(
                "INSERT INTO agents \
                 (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES \
                 ({project_id}, 'Oldest', 'codex-cli', 'gpt-5', '', 1, 100, 'auto', 'auto'), \
                 ({project_id}, 'Older',  'codex-cli', 'gpt-5', '', 1, 200, 'auto', 'auto'), \
                 ({project_id}, 'Mid',    'codex-cli', 'gpt-5', '', 1, 300, 'auto', 'auto'), \
                 ({project_id}, 'Newer',  'codex-cli', 'gpt-5', '', 1, 400, 'auto', 'auto'), \
                 ({project_id}, 'Newest', 'codex-cli', 'gpt-5', '', 1, 500, 'auto', 'auto')"
            ))
            .expect("insert agents");
            drop(conn);

            // Unbounded: all five, most-recent first.
            let all = list_agents_bounded(&cx, &pool, project_id, None, None)
                .await
                .into_result()
                .expect("list all");
            assert_eq!(
                all.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
                vec!["Newest", "Newer", "Mid", "Older", "Oldest"]
            );

            // Limit keeps the most-recently-active.
            let capped = list_agents_bounded(&cx, &pool, project_id, None, Some(2))
                .await
                .into_result()
                .expect("list capped");
            assert_eq!(
                capped.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
                vec!["Newest", "Newer"]
            );

            // Activity floor excludes agents below the floor.
            let recent = list_agents_bounded(&cx, &pool, project_id, Some(350), None)
                .await
                .into_result()
                .expect("list recent");
            assert_eq!(
                recent.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
                vec!["Newest", "Newer"]
            );

            // Floor + limit compose.
            let recent_capped = list_agents_bounded(&cx, &pool, project_id, Some(150), Some(2))
                .await
                .into_result()
                .expect("list recent capped");
            assert_eq!(
                recent_capped.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
                vec!["Newest", "Newer"]
            );
        });
    }

    #[test]
    fn prune_released_file_reservations_deletes_old_released_keeps_active_and_recent() {
        // GH#154 item 2: the retention sweep hard-DELETEs released/expired
        // reservations past the horizon while preserving (a) still-active
        // reservations and (b) recently-released ones, and removes the matching
        // sidecar release-ledger rows.
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("prune_released_reservations.db");

        rt.block_on(async {
            let project = ensure_project(&cx, &pool, "/tmp/prune-reservations")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");
            let agent = register_agent(
                &cx, &pool, project_id, "RedFox", "codex-cli", "gpt-5", Some("holder"),
                Some("auto"), None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.expect("agent id");

            let day_us: i64 = 86_400 * 1_000_000;
            let now = now_micros();
            let old_release = now - 40 * day_us; // older than 30-day horizon
            let recent_release = now - day_us; // within horizon

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire connection");
            // id 1: released long ago (eligible). id 2: released recently (keep).
            // id 3: active (keep).
            conn.execute_raw(&format!(
                "INSERT INTO file_reservations \
                 (id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) \
                 VALUES \
                 (1, {project_id}, {agent_id}, 'a.rs', 1, 'r', {old_release}, {old_release}, {old_release}), \
                 (2, {project_id}, {agent_id}, 'b.rs', 1, 'r', {recent_release}, {recent_release}, {recent_release}), \
                 (3, {project_id}, {agent_id}, 'c.rs', 1, 'r', {now}, {future}, NULL)",
                future = now + 3600 * 1_000_000,
            ))
            .expect("insert reservations");
            // Sidecar ledger row for the old released reservation.
            conn.execute_raw(&format!(
                "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (1, {old_release})"
            ))
            .expect("insert release ledger");
            drop(conn);

            let older_than_us = now - 30 * day_us;
            let deleted = prune_released_file_reservations(&cx, &pool, Some(project_id), older_than_us)
                .await
                .into_result()
                .expect("prune");
            assert_eq!(deleted, 1, "only the long-released reservation is pruned");

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("reacquire connection");
            let remaining = conn
                .query_sync("SELECT id FROM file_reservations ORDER BY id", &[])
                .expect("select remaining");
            let ids: Vec<i64> = remaining
                .iter()
                .filter_map(|r| r.get_named::<i64>("id").ok())
                .collect();
            assert_eq!(ids, vec![2, 3], "active + recently-released survive");
            // The ledger row for the pruned reservation is gone.
            let ledger = conn
                .query_sync(
                    "SELECT reservation_id FROM file_reservation_releases WHERE reservation_id = 1",
                    &[],
                )
                .expect("select ledger");
            assert!(ledger.is_empty(), "pruned reservation's ledger row is removed");
        });
    }

    #[test]
    fn register_agent_without_task_description_clears_existing_description() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_preserve_task_desc.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-agent-preserve-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let initial = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("keep me"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("initial register agent");
            assert_eq!(initial.task_description, "keep me");

            let updated = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5.1",
                None,
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("update register agent");
            assert_eq!(updated.task_description, "");
            assert_eq!(updated.model, "gpt-5.1");

            let fetched = get_agent(&cx, &pool, project_id, "BlueLake")
                .await
                .into_result()
                .expect("get_agent after update");
            assert_eq!(fetched.task_description, "");
            assert_eq!(fetched.model, "gpt-5.1");
        });
    }

    #[test]
    fn create_agent_duplicate_returns_duplicate_error() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("create_agent_duplicate_error.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-agent-dup-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("first"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("first create agent");

            let err = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("second"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect_err("duplicate create should fail");

            match err {
                asupersync::OutcomeError::Err(DbError::Duplicate { entity, identifier }) => {
                    assert_eq!(entity, "agent");
                    assert!(identifier.contains("BlueLake"));
                    assert!(identifier.contains(&project_id.to_string()));
                }
                other => panic!("expected duplicate error, got: {other:?}"),
            }
        });
    }

    #[test]
    fn ensure_project_and_project_lookups_succeed() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("ensure_project_and_lookups.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let human_key = format!("/tmp/am-project-lookups-{base}");

            let ensured = ensure_project(&cx, &pool, &human_key)
                .await
                .into_result()
                .expect("ensure project");
            let by_slug = get_project_by_slug(&cx, &pool, &ensured.slug)
                .await
                .into_result()
                .expect("lookup by slug");
            let by_human_key = get_project_by_human_key(&cx, &pool, &human_key)
                .await
                .into_result()
                .expect("lookup by human_key");

            assert_eq!(ensured.id, by_slug.id);
            assert_eq!(ensured.id, by_human_key.id);
            assert_eq!(ensured.slug, by_slug.slug);
            assert_eq!(human_key, by_human_key.human_key);
        });
    }

    #[cfg(unix)]
    #[test]
    fn ensure_project_collapses_symlink_and_realpath_aliases() {
        use asupersync::runtime::RuntimeBuilder;
        use std::os::unix::fs::symlink;

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1"),
                ("WORKTREES_ENABLED", "0"),
                ("PROJECT_IDENTITY_MODE", "dir"),
            ],
            || {
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                let cx = asupersync::Cx::for_testing();
                let (dir, pool) = create_file_pool_with_schema_for_test("symlink-aliases");

                let real = dir.path().join("real-project");
                std::fs::create_dir_all(&real).expect("create real project");
                let link = dir.path().join("project-link");
                symlink(&real, &link).expect("create symlink");

                let real_key = real.canonicalize().expect("canonical real");
                let real_key = real_key.to_string_lossy().to_string();
                let link_key = link.to_string_lossy().to_string();

                rt.block_on(async {
                    let from_link = ensure_project(&cx, &pool, &link_key)
                        .await
                        .into_result()
                        .expect("ensure via link");
                    let from_real = ensure_project(&cx, &pool, &real_key)
                        .await
                        .into_result()
                        .expect("ensure via realpath");
                    let by_raw_link = get_project_by_human_key(&cx, &pool, &link_key)
                        .await
                        .into_result()
                        .expect("lookup by raw link");

                    assert_eq!(from_link.id, from_real.id);
                    assert_eq!(from_link.id, by_raw_link.id);
                    assert_eq!(from_link.human_key, real_key);
                    assert_eq!(from_real.human_key, real_key);
                    assert_eq!(count_projects_for_test(&cx, &pool).await, 1);
                });
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_project_reuses_existing_raw_symlink_project_row() {
        use asupersync::runtime::RuntimeBuilder;
        use std::os::unix::fs::symlink;

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1"),
                ("WORKTREES_ENABLED", "0"),
                ("PROJECT_IDENTITY_MODE", "dir"),
            ],
            || {
                let rt = RuntimeBuilder::current_thread()
                    .build()
                    .expect("build runtime");
                let cx = asupersync::Cx::for_testing();
                let (dir, pool) = create_file_pool_with_schema_for_test("raw-symlink-reuse");

                let real = dir.path().join("real-project");
                std::fs::create_dir_all(&real).expect("create real project");
                let link = dir.path().join("project-link");
                symlink(&real, &link).expect("create symlink");

                let real_key = real.canonicalize().expect("canonical real");
                let real_key = real_key.to_string_lossy().to_string();
                let link_key = link.to_string_lossy().to_string();
                let legacy_raw_slug = mcp_agent_mail_core::slugify(&link_key);
                let legacy_row = ProjectRow::new(legacy_raw_slug.clone(), link_key.clone());
                let legacy_row = ProjectRow {
                    id: Some(41),
                    ..legacy_row
                };

                rt.block_on(async {
                    insert_project_row_for_test(&cx, &pool, &legacy_row).await;

                    let ensured = ensure_project(&cx, &pool, &real_key)
                        .await
                        .into_result()
                        .expect("ensure via realpath");
                    let by_raw_link = get_project_by_human_key(&cx, &pool, &link_key)
                        .await
                        .into_result()
                        .expect("lookup by raw link");

                    assert_eq!(ensured.id, legacy_row.id);
                    assert_eq!(ensured.slug, legacy_raw_slug);
                    assert_eq!(ensured.human_key, link_key);
                    assert_eq!(by_raw_link.id, legacy_row.id);
                    assert_eq!(count_projects_for_test(&cx, &pool).await, 1);
                });
            },
        );
    }

    #[test]
    fn project_lookup_tolerates_null_created_at_metadata() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("project_null_created_at.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open nullable project schema connection");
        init_conn
            .execute_raw("CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at INTEGER)")
            .expect("create nullable project table");
        init_conn
            .execute_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, NULL)",
                &[
                    Value::BigInt(1),
                    Value::Text("null-created-at".to_string()),
                    Value::Text("/tmp/null-created-at".to_string()),
                ],
            )
            .expect("insert project with missing created_at metadata");
        crate::close_db_conn(init_conn, "project null created_at test init connection");

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool_without_startup_init(&cfg).expect("create pool");

        rt.block_on(async {
            let project = get_project_by_slug(&cx, &pool, "null-created-at")
                .await
                .into_result()
                .expect("lookup project with nullable created_at metadata");

            assert_eq!(project.id, Some(1));
            assert_eq!(project.slug, "null-created-at");
            assert_eq!(project.human_key, "/tmp/null-created-at");
            assert_eq!(project.created_at, 0);
        });
    }

    #[test]
    fn list_thread_messages_limit_returns_latest_window_in_order() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("thread_limit_latest_window.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-thread-limit-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let sender_id = sender.id.expect("sender id");
            let recipient_id = recipient.id.expect("recipient id");
            let recipients = [(recipient_id, "to")];

            for idx in 1..=4 {
                create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    sender_id,
                    &format!("msg-{idx}"),
                    "body",
                    Some("THREAD-LIMIT"),
                    "normal",
                    false,
                    "[]",
                    &recipients,
                )
                .await
                .into_result()
                .expect("create message");
            }

            let rows = list_thread_messages(&cx, &pool, project_id, "THREAD-LIMIT", Some(2))
                .await
                .into_result()
                .expect("list thread messages");

            assert_eq!(rows.len(), 2, "should return the requested window size");
            assert_eq!(rows[0].subject, "msg-3");
            assert_eq!(rows[1].subject, "msg-4");
        });
    }

    #[test]
    fn create_message_with_recipients_dedupes_duplicate_recipient_ids() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("dedupe_duplicate_recipients.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-dedupe-recip-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient_one = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient one");
            let recipient_two = register_agent(
                &cx,
                &pool,
                project_id,
                "PurpleElk",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient two");

            let sender_id = sender.id.expect("sender id");
            let r1 = recipient_one.id.expect("recipient one id");
            let r2 = recipient_two.id.expect("recipient two id");

            // The same agent id (r1) appears twice — once as `to`, once as `cc` —
            // alongside a second distinct recipient. The primary key is
            // (message_id, agent_id), so without de-duplication the second insert
            // of r1 fails UNIQUE and the call returns a false isError even though
            // the message and every distinct recipient were persisted (#243 Bug 2).
            let recipients = [(r1, "to"), (r2, "to"), (r1, "cc")];

            let message = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "dedupe-subject",
                "body",
                None,
                "normal",
                false,
                "[]",
                &recipients,
            )
            .await
            .into_result()
            .expect("create message with a duplicated recipient id must succeed");

            let message_id = message.id.expect("message id");
            let rows = list_message_recipients_by_message(&cx, &pool, project_id, message_id)
                .await
                .into_result()
                .expect("list recipients");

            assert_eq!(
                rows.len(),
                2,
                "exactly one recipient row per distinct agent id"
            );
            let mut names: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
            names.sort();
            assert_eq!(
                names,
                vec!["GreenStone".to_string(), "PurpleElk".to_string()],
                "both distinct recipients persisted exactly once"
            );
        });
    }

    #[test]
    fn list_thread_messages_without_limit_orders_in_chronological_order() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("thread_no_limit_order.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-thread-no-limit-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let sender_id = sender.id.expect("sender id");
            let recipient_id = recipient.id.expect("recipient id");
            let recipients = [(recipient_id, "to")];

            for idx in 1..=2 {
                create_message_with_recipients(
                    &cx,
                    &pool,
                    project_id,
                    sender_id,
                    &format!("msg-{idx}"),
                    "body",
                    Some("THREAD-NO-LIMIT"),
                    "normal",
                    false,
                    "[]",
                    &recipients,
                )
                .await
                .into_result()
                .expect("create message");
            }

            let rows = list_thread_messages(&cx, &pool, project_id, "THREAD-NO-LIMIT", None)
                .await
                .into_result()
                .expect("list thread messages");

            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].subject, "msg-1");
            assert_eq!(rows[1].subject, "msg-2");
        });
    }

    #[test]
    fn list_thread_messages_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("thread_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-thread-orphaned-sender-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let sender_id = sender.id.expect("sender id");
            let recipient_id = recipient.id.expect("recipient id");
            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "thread survives sender drift",
                "body",
                Some("THREAD-ORPHANED-SENDER"),
                "normal",
                false,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
            .into_result()
            .expect("create threaded message");

            cleanup_committed_agent_after_consistency_failure(
                &cx, &pool, project_id, sender_id, "BlueLake",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = list_thread_messages(&cx, &pool, project_id, "THREAD-ORPHANED-SENDER", None)
                .await
                .into_result()
                .expect("list thread messages");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].from, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(rows[0].subject, "thread survives sender drift");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn list_numeric_thread_roots_with_replies_returns_only_roots_with_children() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("thread_roots_with_replies.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-thread-roots-with-replies-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let sender_id = sender.id.expect("sender id");
            let recipient_id = recipient.id.expect("recipient id");
            let recipients = [(recipient_id, "to")];

            let root_with_reply = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "root-with-reply",
                "body",
                None,
                "normal",
                false,
                "[]",
                &recipients,
            )
            .await
            .into_result()
            .expect("create root with reply");
            let root_with_reply_id = root_with_reply.id.expect("root with reply id");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                recipient_id,
                "reply",
                "body",
                Some(&root_with_reply_id.to_string()),
                "normal",
                false,
                "[]",
                &[(sender_id, "to")],
            )
            .await
            .into_result()
            .expect("create reply");

            let root_without_reply = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "root-without-reply",
                "body",
                None,
                "normal",
                false,
                "[]",
                &recipients,
            )
            .await
            .into_result()
            .expect("create root without reply");
            let root_without_reply_id = root_without_reply.id.expect("root without reply id");

            let roots = list_numeric_thread_roots_with_replies(
                &cx,
                &pool,
                project_id,
                &[root_with_reply_id, root_without_reply_id],
            )
            .await
            .into_result()
            .expect("list numeric thread roots with replies");

            assert_eq!(roots, vec![root_with_reply_id]);
        });
    }

    #[test]
    fn release_reservations_empty_id_filter_matches_nothing() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("release_empty_filter.db");

        rt.block_on(async {
            let base = now_micros();
            let project =
                ensure_project(&cx, &pool, &format!("/tmp/am-release-empty-filter-{base}"))
                    .await
                    .into_result()
                    .expect("ensure project");
            let project_id = project.id.expect("project id");

            let agent = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.expect("agent id");

            create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/main.rs"],
                3600,
                true,
                "test",
            )
            .await
            .into_result()
            .expect("create reservation");

            let released = release_reservations(&cx, &pool, project_id, agent_id, None, Some(&[]))
                .await
                .into_result()
                .expect("release reservations");
            assert!(released.is_empty(), "empty filter must not release all");

            let active = list_file_reservations(&cx, &pool, project_id, true)
                .await
                .into_result()
                .expect("list active reservations");
            assert_eq!(active.len(), 1, "reservation should remain active");
        });
    }

    #[test]
    fn release_reservations_by_ids_returning_ids_omits_already_released_rows() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("release_ids_exact.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-release-ids-exact-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let agent = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.expect("agent id");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/main.rs", "src/lib.rs"],
                3600,
                true,
                "test",
            )
            .await
            .into_result()
            .expect("create reservations");
            let first_id = created[0].id.expect("first reservation id");
            let second_id = created[1].id.expect("second reservation id");

            release_reservations_by_ids(&cx, &pool, &[first_id])
                .await
                .into_result()
                .expect("release first reservation");

            let released_ids =
                release_reservations_by_ids_returning_ids(&cx, &pool, &[first_id, second_id])
                    .await
                    .into_result()
                    .expect("release exact id set");
            assert_eq!(released_ids, vec![second_id]);
        });
    }

    #[test]
    fn release_reservations_clear_same_process_reacquire_conflicts() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("release_clears_same_process_conflicts.db");
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 2,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-release-stale-snapshot-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let holder = register_agent(
                &cx,
                &pool,
                project_id,
                "RedFox",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register holder");
            let holder_id = holder.id.expect("holder id");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                holder_id,
                &["src/critical.rs"],
                3600,
                true,
                "holder",
            )
            .await
            .into_result()
            .expect("create reservation");
            let reservation_id = created[0].id.expect("reservation id");

            let stale_conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire stale snapshot connection");
            let stale_tracked = tracked(&*stale_conn);
            map_sql_outcome(
                traw_query(
                    &cx,
                    &stale_tracked,
                    "SELECT id FROM file_reservations WHERE id = ?",
                    &[Value::BigInt(reservation_id)],
                )
                .await,
            )
            .into_result()
            .expect("seed stale snapshot");

            let released = release_reservations(
                &cx,
                &pool,
                project_id,
                holder_id,
                Some(&["src/critical.rs"]),
                None,
            )
            .await
            .into_result()
            .expect("release reservation");
            assert_eq!(
                released.len(),
                1,
                "release must target the held reservation"
            );
            assert_eq!(released[0].id, Some(reservation_id));
            assert!(
                released[0].released_ts.is_some(),
                "release response must carry released_ts"
            );

            drop(stale_conn);

            let active = get_active_reservations(&cx, &pool, project_id)
                .await
                .into_result()
                .expect("read active reservations");
            assert!(
                active.is_empty(),
                "released reservation must not remain visible to same-process reacquire checks"
            );

            let verify_conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire verification connection");
            let verify_tracked = tracked(&*verify_conn);
            let rows = map_sql_outcome(
                traw_query(
                    &cx,
                    &verify_tracked,
                    "SELECT released_ts FROM file_reservations WHERE id = ?",
                    &[Value::BigInt(reservation_id)],
                )
                .await,
            )
            .into_result()
            .expect("read released_ts");
            assert_eq!(rows.len(), 1);
            assert!(rows[0].get(0).and_then(value_as_i64).is_some());
        });
    }

    #[test]
    fn create_file_reservations_rejects_exact_request_that_hits_existing_glob() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("reservation_conflict_glob_to_exact.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-reservation-conflict-glob-to-exact-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let holder = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register holder");
            let holder_id = holder.id.expect("holder id");

            let requester = register_agent(
                &cx,
                &pool,
                project_id,
                "RedHarbor",
                "codex-cli",
                "gpt-5",
                Some("requester"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register requester");
            let requester_id = requester.id.expect("requester id");

            create_file_reservations(
                &cx,
                &pool,
                project_id,
                holder_id,
                &["src/**"],
                3600,
                true,
                "holder",
            )
            .await
            .into_result()
            .expect("create holder reservation");

            let err = create_file_reservations(
                &cx,
                &pool,
                project_id,
                requester_id,
                &["src/main.rs"],
                3600,
                true,
                "requester",
            )
            .await
            .into_result()
            .expect_err("exact request should conflict");

            match err {
                asupersync::OutcomeError::Err(DbError::ResourceBusy(message)) => {
                    assert!(message.contains("src/main.rs"));
                    assert!(message.contains("src/**"));
                }
                other => panic!("expected ResourceBusy, got {other:?}"),
            }
        });
    }

    #[test]
    fn create_file_reservations_rejects_glob_request_that_hits_existing_exact_path() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("reservation_conflict_exact_to_glob.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-reservation-conflict-exact-to-glob-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let holder = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register holder");
            let holder_id = holder.id.expect("holder id");

            let requester = register_agent(
                &cx,
                &pool,
                project_id,
                "RedHarbor",
                "codex-cli",
                "gpt-5",
                Some("requester"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register requester");
            let requester_id = requester.id.expect("requester id");

            create_file_reservations(
                &cx,
                &pool,
                project_id,
                holder_id,
                &["src/main.rs"],
                3600,
                true,
                "holder",
            )
            .await
            .into_result()
            .expect("create holder reservation");

            let err = create_file_reservations(
                &cx,
                &pool,
                project_id,
                requester_id,
                &["src/**"],
                3600,
                true,
                "requester",
            )
            .await
            .into_result()
            .expect_err("glob request should conflict");

            match err {
                asupersync::OutcomeError::Err(DbError::ResourceBusy(message)) => {
                    assert!(message.contains("src/**"));
                    assert!(message.contains("src/main.rs"));
                }
                other => panic!("expected ResourceBusy, got {other:?}"),
            }
        });
    }

    #[test]
    fn create_file_reservations_allows_single_level_glob_when_existing_exact_is_deeper() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) =
            setup_test_pool("reservation_no_conflict_shallow_glob_to_deep_exact.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-reservation-no-conflict-shallow-glob-to-deep-exact-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let holder = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register holder");
            let holder_id = holder.id.expect("holder id");

            let requester = register_agent(
                &cx,
                &pool,
                project_id,
                "RedHarbor",
                "codex-cli",
                "gpt-5",
                Some("requester"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register requester");
            let requester_id = requester.id.expect("requester id");

            create_file_reservations(
                &cx,
                &pool,
                project_id,
                holder_id,
                &["src/auth/sub/file.rs"],
                3600,
                true,
                "holder",
            )
            .await
            .into_result()
            .expect("create holder reservation");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                requester_id,
                &["src/auth/*"],
                3600,
                true,
                "requester",
            )
            .await
            .into_result()
            .expect("single-level glob should not conflict with deeper exact path");

            assert_eq!(created.len(), 1);
            assert_eq!(created[0].path_pattern, "src/auth/*");
        });
    }

    #[test]
    fn create_file_reservations_allows_disjoint_simple_globs_with_same_prefix() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("reservation_no_conflict_disjoint_simple_globs.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-reservation-no-conflict-disjoint-simple-globs-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let holder = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register holder");
            let holder_id = holder.id.expect("holder id");

            let requester = register_agent(
                &cx,
                &pool,
                project_id,
                "RedHarbor",
                "codex-cli",
                "gpt-5",
                Some("requester"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register requester");
            let requester_id = requester.id.expect("requester id");

            create_file_reservations(
                &cx,
                &pool,
                project_id,
                holder_id,
                &["src/*.rs"],
                3600,
                true,
                "holder",
            )
            .await
            .into_result()
            .expect("create holder reservation");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                requester_id,
                &["src/*.txt"],
                3600,
                true,
                "requester",
            )
            .await
            .into_result()
            .expect("disjoint simple globs should not conflict");

            assert_eq!(created.len(), 1);
            assert_eq!(created[0].path_pattern, "src/*.txt");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn release_reservations_by_ids_matching_expiry_skips_rows_renewed_after_scan() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("release_matching_expiry.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-release-matching-expiry-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let agent = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.expect("agent id");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/main.rs"],
                3600,
                true,
                "test",
            )
            .await
            .into_result()
            .expect("create reservation");
            let reservation_id = created[0].id.expect("reservation id");
            let cutoff = now_micros();

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire connection");
            let tracked = tracked(&*conn);

            let expired_params = [
                Value::BigInt(cutoff.saturating_sub(1)),
                Value::BigInt(reservation_id),
            ];
            map_sql_outcome(
                traw_execute(
                    &cx,
                    &tracked,
                    "UPDATE file_reservations SET expires_ts = ? WHERE id = ?",
                    &expired_params,
                )
                .await,
            )
            .into_result()
            .expect("mark reservation expired");

            let renewed_params = [
                Value::BigInt(cutoff.saturating_add(60_000_000)),
                Value::BigInt(reservation_id),
            ];
            map_sql_outcome(
                traw_execute(
                    &cx,
                    &tracked,
                    "UPDATE file_reservations SET expires_ts = ? WHERE id = ?",
                    &renewed_params,
                )
                .await,
            )
            .into_result()
            .expect("renew reservation after scan");
            drop(conn);

            let released = release_reservations_by_ids_matching_expiry(
                &cx,
                &pool,
                &[reservation_id],
                Some(cutoff),
            )
            .await
            .into_result()
            .expect("release matching expiry");
            assert!(
                released.is_empty(),
                "renewed reservation must not be force-released by stale expiry snapshot"
            );

            let active = list_file_reservations(&cx, &pool, project_id, true)
                .await
                .into_result()
                .expect("list active reservations");
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].id, Some(reservation_id));
            assert!(active[0].released_ts.is_none());
        });
    }

    #[test]
    fn force_release_reservation_requires_exact_expiry_match() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("force_release_exact_expiry.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-force-release-exact-expiry-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let agent = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.expect("agent id");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/main.rs"],
                3600,
                true,
                "test",
            )
            .await
            .into_result()
            .expect("create reservation");
            let reservation_id = created[0].id.expect("reservation id");
            let original_expires = created[0].expires_ts;

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire connection");
            let tracked = tracked(&*conn);
            let changed_params = [
                Value::BigInt(original_expires.saturating_sub(1)),
                Value::BigInt(reservation_id),
            ];
            map_sql_outcome(
                traw_execute(
                    &cx,
                    &tracked,
                    "UPDATE file_reservations SET expires_ts = ? WHERE id = ?",
                    &changed_params,
                )
                .await,
            )
            .into_result()
            .expect("change reservation expiry");
            drop(conn);

            let released =
                force_release_reservation(&cx, &pool, reservation_id, Some(original_expires))
                    .await
                    .into_result()
                    .expect("force release with exact expiry guard");
            assert_eq!(
                released, 0,
                "force release must fail when expires_ts changed, even if it stayed earlier"
            );

            let active = list_file_reservations(&cx, &pool, project_id, true)
                .await
                .into_result()
                .expect("list active reservations");
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].id, Some(reservation_id));
            assert!(active[0].released_ts.is_none());
            assert_eq!(active[0].expires_ts, original_expires.saturating_sub(1));
        });
    }

    #[test]
    fn renew_reservations_empty_id_filter_matches_nothing() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("renew_empty_filter.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-renew-empty-filter-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let agent = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("holder"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.expect("agent id");

            let created = create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/main.rs"],
                3600,
                true,
                "test",
            )
            .await
            .into_result()
            .expect("create reservation");
            let original_expires = created[0].expires_ts;

            let renewed =
                renew_reservations(&cx, &pool, project_id, agent_id, 600, None, Some(&[]))
                    .await
                    .into_result()
                    .expect("renew reservations");
            assert!(renewed.is_empty(), "empty filter must not renew all");

            let active = list_file_reservations(&cx, &pool, project_id, true)
                .await
                .into_result()
                .expect("list active reservations");
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].expires_ts, original_expires);
        });
    }

    /// Requires trigger-body execution. Under `FrankenSQLite`, `CREATE TRIGGER`
    /// can succeed while the trigger body is never run, so this harness cannot
    /// reliably suppress committed recipient rows.
    #[test]
    #[ignore = "FrankenSQLite does not reliably execute trigger bodies for this harness"]
    fn create_message_with_recipients_rejects_missing_recipient_rows_after_commit() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("message_recipient_durability_guard.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-recipient-durability', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto')",
            )
            .expect("seed sender");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (2, 1, 'GreenStone', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto')",
            )
            .expect("seed recipient");
        init_conn
            .execute_raw(
                "CREATE TRIGGER suppress_recipients_after_insert \
                 AFTER INSERT ON message_recipients \
                 BEGIN \
                   DELETE FROM message_recipients \
                    WHERE message_id = NEW.message_id \
                      AND agent_id = NEW.agent_id \
                      AND kind = NEW.kind; \
                 END;",
            )
            .expect("install recipient suppression trigger");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let project_id = 1_i64;
            let sender_id = 1_i64;
            let recipients = [(2_i64, "to")];

            let err = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "durability-test",
                "body",
                Some("THREAD-DURABILITY"),
                "normal",
                false,
                "[]",
                &recipients,
            )
            .await
            .into_result()
            .expect_err("missing recipient rows must not return success");

            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("message recipient rows not visible after commit"),
                        "unexpected error message: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }
        });
    }

    #[test]
    fn verify_message_recipients_visible_after_commit_rejects_committed_message_without_recipients()
    {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("message_recipient_probe_missing_rows.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-recipient-probe-missing', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto')",
            )
            .expect("seed sender");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (2, 1, 'GreenStone', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto')",
            )
            .expect("seed recipient");
        init_conn
            .execute_raw(
                "INSERT INTO messages \
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                 VALUES (1, 1, 1, 'THREAD-DURABILITY', 'durability-test', 'body', 'normal', 0, 0, '[]')",
            )
            .expect("seed committed message without recipient rows");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let err =
                verify_message_recipients_visible_after_commit(&cx, &pool, 1, 1, &[(2, "to")])
                    .await
                    .into_result()
                    .expect_err("missing committed recipient rows must fail durability probe");

            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("message recipient rows not visible after commit"),
                        "unexpected error message: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn cleanup_committed_message_after_consistency_failure_removes_orphaned_message_state() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir
            .path()
            .join("cleanup_committed_message_after_probe_failure.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "CREATE TABLE IF NOT EXISTS inbox_stats (
                    agent_id INTEGER PRIMARY KEY,
                    total_count INTEGER NOT NULL DEFAULT 0,
                    unread_count INTEGER NOT NULL DEFAULT 0,
                    ack_pending_count INTEGER NOT NULL DEFAULT 0,
                    last_message_ts INTEGER
                )",
            )
            .expect("ensure inbox_stats");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (1, 'cleanup-project', '/tmp/am-cleanup-message', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES
                    (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto'),
                    (2, 1, 'GreenStone', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto')",
            )
            .expect("seed agents");
        init_conn
            .execute_raw(
                "INSERT INTO messages
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
                 VALUES (1, 1, 1, 'THREAD-CLEANUP', 'cleanup', 'body', 'normal', 1, 100, '[]')",
            )
            .expect("seed message");
        init_conn
            .execute_raw(
                "INSERT INTO message_recipients
                 (message_id, agent_id, kind, read_ts, ack_ts)
                 VALUES (1, 2, 'to', NULL, NULL)",
            )
            .expect("seed recipient");
        init_conn
            .execute_raw(
                "INSERT OR REPLACE INTO inbox_stats
                 (agent_id, total_count, unread_count, ack_pending_count, last_message_ts)
                 VALUES (2, 1, 1, 1, 100)",
            )
            .expect("seed inbox stats");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            cleanup_committed_message_after_consistency_failure(&cx, &pool, 1, 1, &[2])
                .await
                .into_result()
                .expect("cleanup should succeed");
        });

        let verify_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open verify connection");
        let message_rows = verify_conn
            .query_sync("SELECT COUNT(*) AS count FROM messages WHERE id = 1", &[])
            .expect("query messages after cleanup");
        let recipient_rows = verify_conn
            .query_sync(
                "SELECT COUNT(*) AS count FROM message_recipients WHERE message_id = 1",
                &[],
            )
            .expect("query recipients after cleanup");
        let stats_rows = verify_conn
            .query_sync(
                "SELECT COUNT(*) AS count FROM inbox_stats WHERE agent_id = 2",
                &[],
            )
            .expect("query inbox_stats after cleanup");

        assert_eq!(
            message_rows[0].get_named::<i64>("count").unwrap_or(-1),
            0,
            "cleanup must delete the orphaned message row"
        );
        assert_eq!(
            recipient_rows[0].get_named::<i64>("count").unwrap_or(-1),
            0,
            "cleanup must delete recipient rows for the failed message"
        );
        assert_eq!(
            stats_rows[0].get_named::<i64>("count").unwrap_or(-1),
            0,
            "cleanup must rebuild inbox_stats so stale recipient counts are removed"
        );
    }

    #[test]
    fn cleanup_committed_message_after_consistency_failure_rebuilds_stats_when_recipient_rows_are_missing()
     {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir
            .path()
            .join("cleanup_committed_message_missing_recipients.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "CREATE TABLE IF NOT EXISTS inbox_stats (
                    agent_id INTEGER PRIMARY KEY,
                    total_count INTEGER NOT NULL DEFAULT 0,
                    unread_count INTEGER NOT NULL DEFAULT 0,
                    ack_pending_count INTEGER NOT NULL DEFAULT 0,
                    last_message_ts INTEGER
                )",
            )
            .expect("ensure inbox_stats");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (1, 'cleanup-project', '/tmp/am-cleanup-message-missing-recips', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES
                    (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto'),
                    (2, 1, 'GreenStone', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto')",
            )
            .expect("seed agents");
        init_conn
            .execute_raw(
                "INSERT INTO messages
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
                 VALUES (1, 1, 1, 'THREAD-CLEANUP', 'cleanup', 'body', 'normal', 1, 100, '[]')",
            )
            .expect("seed message");
        init_conn
            .execute_raw(
                "INSERT OR REPLACE INTO inbox_stats
                 (agent_id, total_count, unread_count, ack_pending_count, last_message_ts)
                 VALUES (2, 1, 1, 1, 100)",
            )
            .expect("seed stale inbox stats");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            cleanup_committed_message_after_consistency_failure(&cx, &pool, 1, 1, &[2])
                .await
                .into_result()
                .expect("cleanup should succeed");
        });

        let verify_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open verify connection");
        let message_rows = verify_conn
            .query_sync("SELECT COUNT(*) AS count FROM messages WHERE id = 1", &[])
            .expect("query messages after cleanup");
        let stats_rows = verify_conn
            .query_sync(
                "SELECT COUNT(*) AS count FROM inbox_stats WHERE agent_id = 2",
                &[],
            )
            .expect("query inbox_stats after cleanup");

        assert_eq!(
            message_rows[0].get_named::<i64>("count").unwrap_or(-1),
            0,
            "cleanup must delete the orphaned message row"
        );
        assert_eq!(
            stats_rows[0].get_named::<i64>("count").unwrap_or(-1),
            0,
            "cleanup must clear stale inbox_stats even when recipient rows are already missing"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn create_message_with_recipients_rebuilds_inbox_stats_across_sqlite_bind_chunks() {
        use asupersync::runtime::RuntimeBuilder;
        use mcp_agent_mail_core::test_harness::Harness;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("create_message_rebuild_inbox_stats_chunks.db");

        rt.block_on(async {
            let conn = crate::open_sqlite_file_with_recovery(pool.sqlite_path())
                .expect("open sqlite connection for inbox_stats setup");
            create_inbox_stats_table_for_test(&conn);
            drop(conn);

            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-inbox-chunks-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("chunk sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            let harness = Harness::with_seed(17, "inbox_stats_chunking");
            let recipient_names = harness.agent_names(SQLITE_MAX_BIND_PARAMS + 1);
            let mut recipients = Vec::with_capacity(recipient_names.len());
            let mut first_recipient_id = None;
            let mut last_recipient_id = None;
            for name in recipient_names {
                let agent = create_agent(
                    &cx,
                    &pool,
                    project_id,
                    &name,
                    "e2e-test",
                    "test-model",
                    Some("chunk recipient"),
                    Some("auto"),
                )
                .await
                .into_result()
                .expect("create recipient");
                let agent_id = agent.id.expect("recipient id");
                first_recipient_id.get_or_insert(agent_id);
                last_recipient_id = Some(agent_id);
                recipients.push((agent_id, "to"));
            }

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "chunked inbox stats rebuild",
                "body",
                Some("THREAD-INBOX-CHUNKS"),
                "normal",
                false,
                "[]",
                &recipients,
            )
            .await
            .into_result()
            .expect("create message for many recipients");

            for recipient_id in [
                first_recipient_id.expect("first recipient id"),
                last_recipient_id.expect("last recipient id"),
            ] {
                let stats = get_inbox_stats(&cx, &pool, recipient_id)
                    .await
                    .into_result()
                    .expect("fetch inbox stats")
                    .expect("stats row");
                assert_eq!(stats.total_count, 1, "recipient should see one message");
                assert_eq!(stats.unread_count, 1, "recipient should see one unread");
                assert_eq!(
                    stats.ack_pending_count, 0,
                    "non-ack-required message should not create ack debt"
                );
            }
        });
    }

    #[test]
    fn cleanup_committed_agent_after_consistency_failure_removes_orphaned_agent_state() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir
            .path()
            .join("cleanup_committed_agent_after_probe_failure.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (1, 'cleanup-project', '/tmp/am-cleanup-agent', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto')",
            )
            .expect("seed agent");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let cached = get_agent(&cx, &pool, 1, "BlueLake")
                .await
                .into_result()
                .expect("populate cache");
            assert_eq!(cached.id, Some(1));

            cleanup_committed_agent_after_consistency_failure(&cx, &pool, 1, 1, "BlueLake")
                .await
                .into_result()
                .expect("cleanup should succeed");
        });

        let verify_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open verify connection");
        let agent_rows = verify_conn
            .query_sync("SELECT COUNT(*) AS count FROM agents WHERE id = 1", &[])
            .expect("query agents after cleanup");
        assert_eq!(
            agent_rows[0].get_named::<i64>("count").unwrap_or(-1),
            0,
            "cleanup must delete the orphaned agent row"
        );
        assert!(
            crate::cache::read_cache()
                .get_agent_scoped(pool.sqlite_path(), 1, "BlueLake")
                .is_none(),
            "cleanup must invalidate cached agent rows"
        );
    }

    /// Requires C `SQLite` trigger execution. `FrankenSQLite`'s VDBE cannot fire
    /// BEFORE INSERT triggers (CREATE TRIGGER returns Ok but the trigger body
    /// is never executed during INSERT), so this test is skipped when
    /// `DbConn = FrankenConnection`.
    #[test]
    #[ignore = "FrankenSQLite VDBE does not fire BEFORE INSERT triggers"]
    fn register_agent_rejects_suppressed_agent_insert() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_suppressed_insert.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-register-durability', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "CREATE TRIGGER suppress_agents_before_insert \
                 BEFORE INSERT ON agents \
                 BEGIN \
                   SELECT RAISE(IGNORE); \
                 END;",
            )
            .expect("install agent suppression trigger");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let err = register_agent(
                &cx,
                &pool,
                1,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect_err("suppressed insert must not return success");

            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("agent upsert succeeded but re-select failed")
                            || msg.contains("agent row not visible after commit"),
                        "unexpected error: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }
        });
    }

    /// Requires C `SQLite` trigger execution. `FrankenSQLite`'s VDBE cannot fire
    /// BEFORE INSERT triggers (CREATE TRIGGER returns Ok but the trigger body
    /// is never executed during INSERT), so this test is skipped when
    /// `DbConn = FrankenConnection`.
    #[test]
    #[ignore = "FrankenSQLite VDBE does not fire BEFORE INSERT triggers"]
    fn create_agent_rejects_suppressed_agent_insert() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("create_agent_suppressed_insert.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-create-durability', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "CREATE TRIGGER suppress_agents_before_insert \
                 BEFORE INSERT ON agents \
                 BEGIN \
                   SELECT RAISE(IGNORE); \
                 END;",
            )
            .expect("install agent suppression trigger");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let err = create_agent(
                &cx,
                &pool,
                1,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect_err("suppressed insert must not return success");

            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("agent insert succeeded but re-select failed")
                            || msg.contains("agent row not visible after commit"),
                        "unexpected error: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }
        });
    }

    #[test]
    fn durability_probe_for_agent_visibility_ignores_uncommitted_writer_state() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("agent_durability_probe_uncommitted.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-agent-durability', 0)",
            )
            .expect("seed project");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire writer conn");
            let tracked = tracked(&*conn);
            begin_immediate_tx(&cx, &tracked)
                .await
                .into_result()
                .expect("begin immediate");

            let now = now_micros();
            let insert_sql = "INSERT INTO agents \
                (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
            let insert_params = [
                Value::BigInt(1),
                Value::Text("BlueLake".to_string()),
                Value::Text("codex-cli".to_string()),
                Value::Text("gpt-5".to_string()),
                Value::Text("writer-uncommitted".to_string()),
                Value::BigInt(now),
                Value::BigInt(now),
                Value::Text("auto".to_string()),
                Value::Text("auto".to_string()),
            ];
            map_sql_outcome(traw_execute(&cx, &tracked, insert_sql, &insert_params).await)
                .into_result()
                .expect("insert uncommitted row");

            let err = verify_agent_visible_after_commit(&cx, &pool, 1, "BlueLake")
                .await
                .into_result()
                .expect_err("fresh-connection durability probe must not see uncommitted row");
            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("agent row not visible after commit"),
                        "unexpected error: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }

            rollback_tx(&cx, &tracked).await;
        });
    }

    #[test]
    fn durability_probe_rejects_pooled_only_retained_autocommit_agent() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("pooled_only_retained_autocommit_agent.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-pooled-only-agent', 0)",
            )
            .expect("seed project");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire pooled conn");
            conn.execute_raw("PRAGMA autocommit_retain = ON")
                .expect("enable retained autocommit for regression setup");
            if let Err(error) = conn.execute_raw("PRAGMA fsqlite.concurrent_mode = OFF") {
                let message = error.to_string();
                assert!(
                    message.contains("unknown database fsqlite"),
                    "force retained-autocommit candidate mode: {error}"
                );
                return;
            }
            conn.execute_raw(
                "INSERT INTO agents \
                 (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 'BlueLake', 'codex-cli', 'gpt-5', 'pooled-only', 0, 0, 'auto', 'auto')",
            )
            .expect("park agent insert in retained autocommit state");
            drop(conn);

            let err = verify_agent_visible_after_commit(&cx, &pool, 1, "BlueLake")
                .await
                .into_result()
                .expect_err("fresh durability probe must reject pooled-only retained rows");
            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("agent row not visible after commit"),
                        "unexpected error: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }
        });
    }

    /// Verify that the durability probe returns an Internal error when asked
    /// about an agent name that has never been inserted. This is the
    /// FrankenSQLite-safe counterpart of the `#[ignore]`d trigger-based tests:
    /// rather than suppressing an INSERT via a BEFORE INSERT trigger (which
    /// requires C `SQLite` VDBE), we simply ask the probe about a name that was
    /// never written.
    #[test]
    fn durability_probe_rejects_never_inserted_agent() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("probe_rejects_ghost.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-probe-rejects-ghost', 0)",
            )
            .expect("seed project");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            // No agent "GhostAgent" was ever inserted — the probe must reject.
            let err = verify_agent_visible_after_commit(&cx, &pool, 1, "GhostAgent")
                .await
                .into_result()
                .expect_err("probe must reject non-existent agent");

            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("agent row not visible after commit"),
                        "unexpected error: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }
        });
    }

    /// Verify that the durability probe succeeds for a committed agent.
    /// Complements `durability_probe_rejects_never_inserted_agent` by
    /// confirming the probe returns `Ok(AgentRow)` on the happy path.
    #[test]
    fn durability_probe_succeeds_for_committed_agent() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("probe_succeeds_committed.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-probe-succeeds', 0)",
            )
            .expect("seed project");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            // Register an agent through the normal path (commits to DB).
            let agent = register_agent(
                &cx,
                &pool,
                1,
                "BlueLake",
                "claude-code",
                "opus-4.6",
                Some("durability test"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register_agent should succeed");

            assert_eq!(agent.name, "BlueLake");
            assert_eq!(agent.project_id, 1);

            // The durability probe must find the committed agent.
            let probed = verify_agent_visible_after_commit(&cx, &pool, 1, "BlueLake")
                .await
                .into_result()
                .expect("probe must find committed agent");

            assert_eq!(probed.name, "BlueLake");
            assert_eq!(probed.project_id, 1);
        });
    }

    #[test]
    fn durability_probe_succeeds_under_reserved_lock_for_committed_agent() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("probe_reserved_lock_committed.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-probe-reserved-lock', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'durable', 0, 0, 'auto', 'auto')",
            )
            .expect("seed committed agent");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        let lock_conn = crate::DbConn::open_file(db_path.display().to_string()).expect("open lock");
        lock_conn
            .execute_raw("PRAGMA busy_timeout = 1")
            .expect("set lock busy_timeout");
        lock_conn
            .execute_raw("BEGIN IMMEDIATE")
            .expect("hold reserved sqlite lock");

        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let pool_for_thread = pool;
        let probe_thread = std::thread::spawn(move || {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build thread runtime");
            let cx = asupersync::Cx::for_testing();
            let result = rt.block_on(async {
                verify_agent_visible_after_commit(&cx, &pool_for_thread, 1, "BlueLake")
                    .await
                    .into_result()
                    .map(|agent| agent.name)
                    .map_err(|err| format!("durability probe failed: {err}"))
            });
            result_tx.send(result).expect("send probe result");
        });

        let probed_name = match result_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(result) => result.expect("durability probe should succeed under reserved lock"),
            Err(err) => {
                let _ = lock_conn.execute_raw("ROLLBACK");
                probe_thread
                    .join()
                    .expect("join probe thread after timeout");
                panic!("durability probe should not stall under reserved lock: {err}");
            }
        };
        assert_eq!(probed_name, "BlueLake");

        lock_conn
            .execute_raw("ROLLBACK")
            .expect("release sqlite lock");
        probe_thread.join().expect("join probe thread");
    }

    /// Regression test for br-97gc6.2.1: a successful COMMIT followed by a
    /// transient BUSY on the durability probe must NOT propagate the error to
    /// the caller. The probe retry loop should absorb the transient failure and
    /// return the committed row.
    #[test]
    fn durability_probe_retries_transient_busy_after_committed_write() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("probe_busy_retry.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'busy-retry-project', '/tmp/am-probe-busy-retry', 0)",
            )
            .expect("seed project");
        // Insert the agent (committed and durable).
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'durable', 0, 0, 'auto', 'auto')",
            )
            .expect("seed committed agent");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        // Hold an EXCLUSIVE lock on the DB from a separate connection to force
        // the first probe attempt to get SQLITE_BUSY.  We use a 1ms busy_timeout
        // on the blocker so the probe's own busy_timeout can expire quickly.
        let blocker_conn =
            crate::DbConn::open_file(db_path.display().to_string()).expect("open blocker");
        blocker_conn
            .execute_raw("PRAGMA busy_timeout = 1")
            .expect("set blocker busy_timeout");
        blocker_conn
            .execute_raw("BEGIN EXCLUSIVE")
            .expect("acquire exclusive lock to block probe");

        // Run the durability probe in a background thread so we can release
        // the lock from the main thread partway through.
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let pool_for_probe = pool;
        let probe_thread = std::thread::spawn(move || {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build probe runtime");
            let cx = asupersync::Cx::for_testing();
            let result = rt.block_on(async {
                verify_agent_visible_after_commit(&cx, &pool_for_probe, 1, "BlueLake")
                    .await
                    .into_result()
                    .map(|agent| agent.name)
                    .map_err(|err| format!("{err}"))
            });
            result_tx.send(result).expect("send result");
        });

        // Wait a moment then release the exclusive lock so the retry succeeds.
        std::thread::sleep(std::time::Duration::from_millis(60));
        blocker_conn
            .execute_raw("ROLLBACK")
            .expect("release exclusive lock");

        let probed_name = match result_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => result.expect(
                "durability probe should succeed after transient busy (br-97gc6.2.1 regression)",
            ),
            Err(err) => {
                probe_thread.join().expect("join after timeout");
                panic!("durability probe stalled or failed: {err}");
            }
        };
        assert_eq!(probed_name, "BlueLake");
        probe_thread.join().expect("join probe thread");
    }

    /// Verify that `is_probe_transient_busy` correctly classifies errors.
    #[test]
    fn probe_transient_busy_classification() {
        // ResourceBusy with "database is busy" → transient
        assert!(is_probe_transient_busy(&DbError::ResourceBusy(
            "database is busy".to_string()
        )));
        // Sqlite with "database is locked" → transient
        assert!(is_probe_transient_busy(&DbError::Sqlite(
            "database is locked".to_string()
        )));
        // ResourceBusy with "snapshot conflict on pages: 5" → transient (MVCC)
        assert!(is_probe_transient_busy(&DbError::ResourceBusy(
            "database is busy (snapshot conflict on pages: 5)".to_string()
        )));
        // Internal error → not transient
        assert!(!is_probe_transient_busy(&DbError::Internal(
            "agent row not visible after commit".to_string()
        )));
        // Pool error → not transient
        assert!(!is_probe_transient_busy(&DbError::Pool(
            "pool exhausted".to_string()
        )));
    }

    #[test]
    fn post_commit_probe_transient_errors_are_advisory_consistency_errors_are_hard() {
        // GH#179: transient / execution probe failures prove nothing about the
        // durability of the already-committed row, so they must be advisory
        // (never trigger destructive cleanup of committed data).
        assert!(!is_hard_post_commit_probe_error(&DbError::Sqlite(
            "database is locked".to_string(),
        )));
        assert!(!is_hard_post_commit_probe_error(&DbError::Sqlite(
            "disk I/O error".to_string(),
        )));
        assert!(!is_hard_post_commit_probe_error(&DbError::Pool(
            "database is locked".to_string(),
        )));
        assert!(!is_hard_post_commit_probe_error(&DbError::PoolExhausted {
            message: "pool exhausted".to_string(),
            pool_size: 1,
            max_overflow: 0,
        }));
        assert!(!is_hard_post_commit_probe_error(&DbError::ResourceBusy(
            "database is busy (snapshot conflict on pages: 5)".to_string(),
        )));
        // The cancel/panic wrapper errors are likewise advisory: a cancelled or
        // panicked probe cannot prove the committed row is inconsistent.
        assert!(!is_hard_post_commit_probe_error(
            &post_commit_probe_cancelled_error("create_message_with_recipients", "1:42",)
        ));
        assert!(!is_hard_post_commit_probe_error(
            &post_commit_probe_panicked_error("create_message_with_recipients", "1:42", "boom",)
        ));
        // Only a probe that RAN and observed a genuine ghost success is hard.
        assert!(is_hard_post_commit_probe_error(&DbError::Internal(
            "message row not visible after commit for message_id=1 project_id=1".to_string(),
        )));
        assert!(is_hard_post_commit_probe_error(&DbError::Internal(
            "message recipient rows not visible after commit for message_id=1: expected=2 actual=0"
                .to_string(),
        )));
    }

    #[test]
    fn post_commit_probe_requires_cleanup_only_on_proven_inconsistency() {
        // GH#179 regression: a transient probe error after a committed write must
        // NOT trigger cleanup, regardless of the writer's own sample.
        let transient = DbError::Sqlite("database is locked".to_string());
        assert!(!post_commit_probe_requires_cleanup(&transient, false));
        assert!(!post_commit_probe_requires_cleanup(&transient, true));

        // A genuine ghost-success verdict triggers cleanup only when the writer's
        // own post-commit sample also failed to confirm the row is present.
        let ghost = DbError::Internal(
            "message recipient rows not visible after commit for message_id=1: expected=2 actual=0"
                .to_string(),
        );
        assert!(post_commit_probe_requires_cleanup(&ghost, false));
        // Writer confirms the message row landed → cross-handle visibility lag,
        // not missing data → never delete.
        assert!(!post_commit_probe_requires_cleanup(&ghost, true));
    }

    #[test]
    fn writer_sample_refutation_is_scoped_to_the_probe_verdict_class() {
        // GH#179 follow-up: the writer's own sample only refutes the ghost class
        // it actually contradicts.
        let message_ghost = DbError::Internal(
            "message row not visible after commit for message_id=1 project_id=1".to_string(),
        );
        let recipient_ghost = DbError::Internal(
            "message recipient rows not visible after commit for message_id=1: expected=2 actual=0"
                .to_string(),
        );
        let both = WriterPostCommitCounts {
            message_count: Some(1),
            recipient_count: Some(2),
        };
        let message_only = WriterPostCommitCounts {
            message_count: Some(1),
            recipient_count: Some(0),
        };
        let none = WriterPostCommitCounts::default();

        // A message-row ghost is refuted by the message row alone.
        assert!(writer_sample_refutes_probe_verdict(&message_ghost, both, 2));
        assert!(writer_sample_refutes_probe_verdict(
            &message_ghost,
            message_only,
            2
        ));
        assert!(!writer_sample_refutes_probe_verdict(
            &message_ghost,
            none,
            2
        ));

        // A recipient-rows ghost needs the full expected recipient count too:
        // a present message row with zero recipient rows is delivered to nobody.
        assert!(writer_sample_refutes_probe_verdict(
            &recipient_ghost,
            both,
            2
        ));
        assert!(!writer_sample_refutes_probe_verdict(
            &recipient_ghost,
            message_only,
            2
        ));
        assert!(!writer_sample_refutes_probe_verdict(
            &recipient_ghost,
            both,
            3
        ));
        assert!(!writer_sample_refutes_probe_verdict(
            &recipient_ghost,
            none,
            2
        ));
    }

    #[test]
    fn post_commit_probe_cancelled_and_panicked_are_deterministic_errors() {
        match post_commit_probe_cancelled_error("register_agent", "1:BlueLake") {
            DbError::ResourceBusy(message) => {
                assert!(message.contains("register_agent"));
                assert!(message.contains("1:BlueLake"));
                assert!(message.contains("cancelled"));
            }
            other => panic!("expected ResourceBusy, got {other:?}"),
        }

        match post_commit_probe_panicked_error("create_message_with_recipients", "1:42", "boom") {
            DbError::Internal(message) => {
                assert!(message.contains("create_message_with_recipients"));
                assert!(message.contains("1:42"));
                assert!(message.contains("boom"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_created_agent_after_probe_failure_deletes_agent_row() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("create_agent_probe_cleanup.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'cleanup-project', '/tmp/am-create-agent-cleanup', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (7, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'cleanup target', 0, 0, 'auto', 'auto')",
            )
            .expect("seed agent");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let err = cleanup_created_agent_after_post_commit_probe_failure(
                &cx,
                &pool,
                1,
                7,
                "BlueLake",
                DbError::Internal("agent row not visible after commit for 1:BlueLake".into()),
            )
            .await;
            assert!(
                matches!(err, DbError::Internal(ref message) if message.contains("agent row not visible after commit")),
                "unexpected cleanup result: {err:?}"
            );

            let rows = durability_probe_query(
                &cx,
                &pool,
                "SELECT COUNT(*) FROM agents WHERE project_id = ? AND name = ?",
                &[Value::BigInt(1), Value::Text("BlueLake".to_string())],
            )
            .await
            .into_result()
            .expect("count query should succeed");
            let remaining = rows.first().and_then(row_first_i64).expect("count row");
            assert_eq!(remaining, 0, "cleanup should delete the committed agent");
        });
    }

    #[test]
    fn finalize_register_agent_post_commit_probe_returns_committed_existing_agent_on_probe_error() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("register_agent_probe_existing_row.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-register-existing', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (7, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'existing', 0, 0, 'auto', 'auto')",
            )
            .expect("seed agent");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let existing = get_agent(&cx, &pool, 1, "BlueLake")
                .await
                .into_result()
                .expect("fetch existing agent");
            let durable = finalize_register_agent_post_commit_probe(
                &cx,
                &pool,
                1,
                "BlueLake",
                &existing,
                false,
                Outcome::Err(DbError::Internal(
                    "agent row not visible after commit for 1:BlueLake".into(),
                )),
            )
            .await
            .into_result()
            .expect("existing-row probe errors should stay advisory");
            assert!(
                durable.is_none(),
                "existing-row register_agent should fall back to the committed row"
            );

            let rows = durability_probe_query(
                &cx,
                &pool,
                "SELECT COUNT(*) FROM agents WHERE project_id = ? AND name = ?",
                &[Value::BigInt(1), Value::Text("BlueLake".to_string())],
            )
            .await
            .into_result()
            .expect("count query should succeed");
            let remaining = rows.first().and_then(row_first_i64).expect("count row");
            assert_eq!(remaining, 1, "existing agent row must not be deleted");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn durability_probe_for_message_recipients_ignores_uncommitted_writer_state() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("message_durability_probe_uncommitted.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-message-durability', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto')",
            )
            .expect("seed sender");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (2, 1, 'GreenStone', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto')",
            )
            .expect("seed recipient");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire writer conn");
            let tracked = tracked(&*conn);
            begin_immediate_tx(&cx, &tracked)
                .await
                .into_result()
                .expect("begin immediate");

            let message_insert = "INSERT INTO messages \
                (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
            let message_params = [
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("THREAD-DURABILITY".to_string()),
                Value::Text("subject".to_string()),
                Value::Text("body".to_string()),
                Value::Text("normal".to_string()),
                Value::BigInt(0),
                Value::BigInt(now_micros()),
                Value::Text("[]".to_string()),
            ];
            map_sql_outcome(traw_execute(&cx, &tracked, message_insert, &message_params).await)
                .into_result()
                .expect("insert uncommitted message");

            let message_id_rows = map_sql_outcome(
                traw_query(&cx, &tracked, "SELECT last_insert_rowid()", &[]).await,
            )
            .into_result()
            .expect("query last_insert_rowid");
            let message_id = message_id_rows
                .first()
                .and_then(row_first_i64)
                .expect("message id from last_insert_rowid");

            let recipient_insert = "INSERT INTO message_recipients \
                (message_id, agent_id, kind, read_ts, ack_ts) VALUES (?, ?, ?, NULL, NULL)";
            let recipient_params = [
                Value::BigInt(message_id),
                Value::BigInt(2),
                Value::Text("to".to_string()),
            ];
            map_sql_outcome(traw_execute(&cx, &tracked, recipient_insert, &recipient_params).await)
                .into_result()
                .expect("insert uncommitted recipient");

            let err = verify_message_recipients_visible_after_commit(
                &cx,
                &pool,
                1,
                message_id,
                &[(2, "to")],
            )
                .await
                .into_result()
                .expect_err("fresh-connection durability probe must not see uncommitted rows");
            match err {
                asupersync::OutcomeError::Err(DbError::Internal(msg)) => {
                    assert!(
                        msg.contains("message row not visible after commit"),
                        "unexpected error: {msg}"
                    );
                }
                other => panic!("expected internal durability error, got: {other:?}"),
            }

            rollback_tx(&cx, &tracked).await;
        });
    }

    #[test]
    fn message_visibility_probe_consistency_error_classifier_is_narrow() {
        assert!(is_message_visibility_probe_consistency_error(
            &DbError::Internal(
                "message row not visible after commit for message_id=7 project_id=1".to_string()
            )
        ));
        assert!(is_message_visibility_probe_consistency_error(&DbError::Internal(
            "message recipient rows not visible after commit for message_id=7: expected=1 actual=0"
                .to_string()
        )));
        assert!(!is_message_visibility_probe_consistency_error(
            &DbError::Internal(
                "message recipient durability check failed: missing kind for message_id=7"
                    .to_string()
            )
        ));
        assert!(!is_message_visibility_probe_consistency_error(
            &DbError::Sqlite("database is locked".to_string())
        ));
    }

    #[test]
    fn verify_message_recipients_visible_with_pooled_handle_sees_committed_rows() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("message_visibility_probe_pooled.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        init_conn
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (1, 'durability-project', '/tmp/am-message-pooled-visibility', 0)",
            )
            .expect("seed project");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 1, 'BlueLake', 'codex-cli', 'gpt-5', 'sender', 0, 0, 'auto', 'auto')",
            )
            .expect("seed sender");
        init_conn
            .execute_raw(
                "INSERT INTO agents \
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (2, 1, 'GreenStone', 'codex-cli', 'gpt-5', 'recipient', 0, 0, 'auto', 'auto')",
            )
            .expect("seed recipient");
        init_conn
            .execute_raw(
                "INSERT INTO messages \
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                 VALUES (1, 1, 1, 'THREAD-DURABILITY', 'durability-test', 'body', 'normal', 0, 0, '[]')",
            )
            .expect("seed committed message");
        init_conn
            .execute_raw(
                "INSERT INTO message_recipients \
                 (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 2, 'to', NULL, NULL)",
            )
            .expect("seed committed recipient");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 2,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            verify_message_recipients_visible_with_probe_mode(
                &cx,
                &pool,
                1,
                1,
                &[(2, "to")],
                MessageVisibilityProbeMode::PooledHandle,
            )
            .await
            .into_result()
            .expect("pooled handle should confirm committed message visibility");
        });
    }

    #[test]
    fn create_message_with_recipients_pool_drop_closes_cleanly() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let capture = EventCapture::default();
        let iterations = 3usize;

        tracing::subscriber::with_default(capture.clone(), || {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");

            for iteration in 0..iterations {
                let dir = tempdir().expect("tempdir");
                let db_path = dir
                    .path()
                    .join(format!("message_recipients_drop_close_{iteration}.db"));
                let seed_conn = crate::DbConn::open_file(db_path.display().to_string())
                    .expect("open base schema connection");
                let seed_conn = crate::guard_db_conn(
                    seed_conn,
                    "queries::tests message_recipients drop_close seed",
                );
                seed_conn
                    .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
                    .expect("apply init PRAGMAs");
                let init_sql = crate::schema::init_schema_sql_base();
                seed_conn
                    .execute_raw(&init_sql)
                    .expect("initialize base schema");
                drop(seed_conn);

                let cfg = crate::pool::DbPoolConfig {
                    database_url: format!("sqlite:///{}", db_path.display()),
                    min_connections: 1,
                    max_connections: 1,
                    run_migrations: false,
                    warmup_connections: 0,
                    ..Default::default()
                };
                let pool = crate::create_pool(&cfg).expect("create pool");

                rt.block_on(async {
                    let cx = Cx::for_testing();
                    let project = ensure_project(
                        &cx,
                        &pool,
                        &format!("/tmp/am-message-recipient-drop-close-{iteration}"),
                    )
                    .await
                    .into_result()
                    .expect("ensure project");
                    let project_id = project.id.expect("project id");

                    let sender_name = "BlueLake".to_string();
                    let sender = create_agent(
                        &cx,
                        &pool,
                        project_id,
                        &sender_name,
                        "codex-cli",
                        "gpt-5",
                        Some("sender"),
                        Some("auto"),
                    )
                    .await
                    .into_result()
                    .expect("create sender");
                    let sender_id = sender.id.expect("sender id");

                    let recipient_name = "GreenStone".to_string();
                    let recipient = create_agent(
                        &cx,
                        &pool,
                        project_id,
                        &recipient_name,
                        "codex-cli",
                        "gpt-5",
                        Some("recipient"),
                        Some("auto"),
                    )
                    .await
                    .into_result()
                    .expect("create recipient");
                    let recipient_id = recipient.id.expect("recipient id");

                    let message = create_message_with_recipients(
                        &cx,
                        &pool,
                        project_id,
                        sender_id,
                        &format!("drop-close-subject-{iteration}"),
                        "body",
                        Some("THREAD-DROP-CLOSE"),
                        "normal",
                        false,
                        "[]",
                        &[(recipient_id, "to")],
                    )
                    .await
                    .into_result()
                    .expect("create message with recipients");
                    let message_id = message.id.expect("message id");

                    let recipients =
                        list_message_recipients_by_message(&cx, &pool, project_id, message_id)
                            .await
                            .into_result()
                            .expect("list recipients");
                    assert_eq!(recipients.len(), 1, "recipient row should persist");
                    assert_eq!(recipients[0].name, recipient_name);
                    assert_eq!(recipients[0].kind, "to");
                });

                drop(pool);
            }
        });

        assert_eq!(
            capture.drop_close_count(),
            0,
            "pooled connection teardown should close cleanly without drop_close warnings"
        );
    }

    #[test]
    fn create_message_with_recipients_file_backed_survives_concurrent_writers() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir
            .path()
            .join("create_message_with_recipients_concurrent_writers.db");
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 4,
            max_connections: 12,
            run_migrations: true,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");
        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = Cx::for_testing();

        let (project_id, sender_id, recipient_id) = rt.block_on(async {
            let project = ensure_project(&cx, &pool, "/tmp/am-concurrent-message-durability")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");
            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("concurrent sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let recipient = create_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("concurrent recipient"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create recipient");
            (
                project_id,
                sender.id.expect("sender id"),
                recipient.id.expect("recipient id"),
            )
        });

        let thread_count = 8usize;
        let messages_per_thread = 8usize;
        let capture = EventCapture::default();
        let dispatch = tracing::Dispatch::new(capture.clone());
        let start_barrier = std::sync::Arc::new(std::sync::Barrier::new(thread_count));
        let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let handles: Vec<_> = (0..thread_count)
            .map(|thread_idx| {
                let pool = pool.clone();
                let dispatch = dispatch.clone();
                let start_barrier = std::sync::Arc::clone(&start_barrier);
                let failures = std::sync::Arc::clone(&failures);
                std::thread::spawn(move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        let rt = RuntimeBuilder::current_thread()
                            .build()
                            .expect("build thread runtime");
                        start_barrier.wait();
                        for message_idx in 0..messages_per_thread {
                            let cx = Cx::for_testing();
                            let subject = format!("writer-{thread_idx}-message-{message_idx}");
                            let body = format!("body-{thread_idx}-{message_idx}");
                            match rt.block_on(async {
                                create_message_with_recipients(
                                    &cx,
                                    &pool,
                                    project_id,
                                    sender_id,
                                    &subject,
                                    &body,
                                    Some("THREAD-CONCURRENT-DURABILITY"),
                                    "normal",
                                    false,
                                    "[]",
                                    &[(recipient_id, "to")],
                                )
                                .await
                            }) {
                                Outcome::Ok(row) => {
                                    assert!(row.id.is_some(), "created message must include id");
                                }
                                Outcome::Err(error) => {
                                    failures
                                        .lock()
                                        .expect("failures mutex")
                                        .push(format!(
                                            "thread {thread_idx} message {message_idx}: {error:?}"
                                        ));
                                    break;
                                }
                                Outcome::Cancelled(reason) => {
                                    failures
                                        .lock()
                                        .expect("failures mutex")
                                        .push(format!(
                                            "thread {thread_idx} message {message_idx}: cancelled {reason:?}"
                                        ));
                                    break;
                                }
                                Outcome::Panicked(payload) => {
                                    panic!("thread {thread_idx} message {message_idx} panicked: {payload}");
                                }
                            }
                        }
                    });
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("writer thread join");
        }

        let failures = failures.lock().expect("failures mutex");
        assert!(
            failures.is_empty(),
            "concurrent create_message_with_recipients failures:\n{}",
            failures.join("\n")
        );
        drop(failures);

        assert_eq!(
            capture.message_count("post_commit_checkpoint_failed_after_successful_commit"),
            0,
            "concurrent writers should not emit post-commit checkpoint warnings"
        );

        rt.block_on(async {
            let rows = durability_probe_query(
                &cx,
                &pool,
                "SELECT COUNT(*) FROM messages WHERE project_id = ?",
                &[Value::BigInt(project_id)],
            )
            .await
            .into_result()
            .expect("count committed messages");
            let count = rows
                .first()
                .and_then(row_first_i64)
                .expect("message count row");
            assert_eq!(
                count,
                i64::try_from(thread_count * messages_per_thread)
                    .expect("message count fits in i64"),
                "all committed messages must remain visible after concurrent writes"
            );
        });
    }

    #[test]
    fn set_contact_policy_by_name_preserves_lookup_and_cache() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("set_policy_by_name_lookup.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-policy-repro-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let registered = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("policy update test"),
                Some("inline"),
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            assert_eq!(registered.contact_policy, "auto");
            assert_eq!(registered.attachments_policy, "inline");

            let updated =
                set_agent_contact_policy_by_name(&cx, &pool, project_id, "BlueLake", "open")
                    .await
                    .into_result()
                    .expect("set policy by exact name");
            assert!(updated.id.is_some(), "updated row should include id");
            assert_eq!(updated.name, "BlueLake");
            assert_eq!(updated.program, "codex-cli");
            assert_eq!(updated.contact_policy, "open");
            assert_eq!(updated.attachments_policy, "inline");

            // Whitespace around input name should not break lookup/update.
            let updated2 = set_agent_contact_policy_by_name(
                &cx,
                &pool,
                project_id,
                "  BlueLake \t",
                "contacts_only",
            )
            .await
            .into_result()
            .expect("set policy by trimmed name");
            assert_eq!(updated2.contact_policy, "contacts_only");
            assert_eq!(updated2.attachments_policy, "inline");

            let updated3 =
                set_agent_contact_policy_by_name(&cx, &pool, project_id, "bluelake", "closed")
                    .await
                    .into_result()
                    .expect("set policy by lowercase name");
            assert_eq!(updated3.name, "BlueLake");
            assert_eq!(updated3.contact_policy, "closed");
            assert_eq!(updated3.attachments_policy, "inline");

            let fetched = get_agent(&cx, &pool, project_id, "BlueLake")
                .await
                .into_result()
                .expect("get_agent should work after policy updates");
            assert_eq!(fetched.contact_policy, "closed");
            assert_eq!(fetched.attachments_policy, "inline");

            let cached = crate::read_cache()
                .get_agent_scoped(pool.sqlite_path(), project_id, "BlueLake")
                .expect("cache entry should be refreshed");
            assert_eq!(cached.contact_policy, "closed");
            assert_eq!(cached.attachments_policy, "inline");
        });
    }

    #[test]
    fn insert_system_agent_reselects_existing_name_case_insensitively() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("insert_system_agent_case_insensitive.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-system-agent-case-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let first = insert_system_agent(
                &cx,
                &pool,
                project_id,
                "AckEscalator",
                "worker",
                "gpt-5",
                "first insert",
            )
            .await
            .into_result()
            .expect("insert first system agent");
            let second = insert_system_agent(
                &cx,
                &pool,
                project_id,
                "ackescalator",
                "worker",
                "gpt-5",
                "second insert should reuse existing row",
            )
            .await
            .into_result()
            .expect("reselect existing system agent after case-insensitive conflict");

            assert_eq!(second.id, first.id);
            assert_eq!(second.name, "AckEscalator");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::similar_names)]
    fn request_contact_refreshes_existing_pair_without_on_conflict_do_update() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("request_contact_refresh_pair.db");

        rt.block_on(async {
            let base = now_micros();
            let project_a = ensure_project(&cx, &pool, &format!("/tmp/am-contact-a-{base}"))
                .await
                .into_result()
                .expect("ensure project A");
            let project_b = ensure_project(&cx, &pool, &format!("/tmp/am-contact-b-{base}"))
                .await
                .into_result()
                .expect("ensure project B");
            let project_a_id = project_a.id.expect("project A id");
            #[allow(clippy::similar_names)]
            let project_b_id = project_b.id.expect("project B id");

            let from = register_agent(
                &cx,
                &pool,
                project_a_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let to = register_agent(
                &cx,
                &pool,
                project_b_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let from_id = from.id.expect("sender id");
            let to_id = to.id.expect("recipient id");

            let first = request_contact(
                &cx,
                &pool,
                project_a_id,
                from_id,
                project_b_id,
                to_id,
                "initial",
                3_600,
            )
            .await
            .into_result()
            .expect("initial request_contact");
            let first_id = first.id.expect("first link id");

            let refreshed = request_contact(
                &cx,
                &pool,
                project_a_id,
                from_id,
                project_b_id,
                to_id,
                "refreshed",
                120,
            )
            .await
            .into_result()
            .expect("second request_contact should refresh existing row");

            assert_eq!(refreshed.id, Some(first_id));
            assert_eq!(refreshed.status, "pending");
            assert_eq!(refreshed.reason, "refreshed");
            assert!(refreshed.expires_ts.is_some(), "refresh should set TTL");

            let (outgoing, incoming) = list_contacts(&cx, &pool, project_a_id, from_id)
                .await
                .into_result()
                .expect("list contacts");
            assert_eq!(outgoing.len(), 1, "should keep exactly one outgoing link");
            assert!(incoming.is_empty(), "sender should not have incoming links");
            assert_eq!(outgoing[0].id, Some(first_id));
            assert_eq!(outgoing[0].reason, "refreshed");

            let (to_outgoing, to_incoming) = list_contacts(&cx, &pool, project_b_id, to_id)
                .await
                .into_result()
                .expect("list recipient contacts");
            assert!(
                to_outgoing.is_empty(),
                "recipient should not have outgoing links"
            );
            assert_eq!(
                to_incoming.len(),
                1,
                "recipient should see one incoming link"
            );
            assert_eq!(to_incoming[0].id, Some(first_id));
            assert_eq!(to_incoming[0].reason, "refreshed");
        });
    }

    #[test]
    fn request_contact_retries_transient_busy_lock() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("request_contact_retries_busy.db");
        let db_path = dir.path().join("request_contact_retries_busy.db");

        rt.block_on(async {
            let base = now_micros();
            let project_a = ensure_project(&cx, &pool, &format!("/tmp/am-contact-retry-a-{base}"))
                .await
                .into_result()
                .expect("ensure project A");
            let project_b = ensure_project(&cx, &pool, &format!("/tmp/am-contact-retry-b-{base}"))
                .await
                .into_result()
                .expect("ensure project B");
            let sender_project_id = project_a.id.expect("project A id");
            let recipient_project_id = project_b.id.expect("project B id");

            let from = register_agent(
                &cx,
                &pool,
                sender_project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let to = register_agent(
                &cx,
                &pool,
                recipient_project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let pooled = match pool.acquire(&cx).await {
                Outcome::Ok(conn) => conn,
                Outcome::Err(err) => panic!("acquire failed: {err}"),
                Outcome::Cancelled(_) => panic!("acquire cancelled"),
                Outcome::Panicked(panic) => panic!("acquire panicked: {}", panic.message()),
            };
            pooled
                .execute_sync("PRAGMA busy_timeout = 1", &[])
                .expect("set pooled busy_timeout");
            drop(pooled);

            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let lock_path = db_path.to_string_lossy().into_owned();
            let lock_thread = std::thread::spawn(move || {
                let lock_conn = crate::DbConn::open_file(&lock_path).expect("open lock db");
                lock_conn
                    .execute_raw("PRAGMA busy_timeout = 1;")
                    .expect("set lock busy_timeout");
                lock_conn
                    .execute_raw("BEGIN EXCLUSIVE")
                    .expect("hold exclusive sqlite lock");
                ready_tx.send(()).expect("signal lock ready");
                std::thread::sleep(std::time::Duration::from_millis(150));
                lock_conn
                    .execute_raw("ROLLBACK")
                    .expect("release sqlite lock");
            });

            ready_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("wait for lock thread");

            let result = request_contact(
                &cx,
                &pool,
                sender_project_id,
                from.id.expect("sender id"),
                recipient_project_id,
                to.id.expect("recipient id"),
                "transient busy retry",
                300,
            )
            .await
            .into_result();

            lock_thread.join().expect("join lock thread");

            let link = result.expect("request_contact should retry past transient busy");
            assert_eq!(link.status, "pending");
            assert_eq!(link.reason, "transient busy retry");
        });
    }

    #[test]
    fn register_agent_preserves_existing_attachment_policy_on_other_agent_upserts() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("register_agent_attachment_preservation.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-register-preserve-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let red = register_agent(
                &cx,
                &pool,
                project_id,
                "RedFox",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("inline"),
                None,
            )
            .await
            .into_result()
            .expect("register red");
            assert_eq!(red.attachments_policy, "inline");

            let blue = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueBear",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register blue");
            assert_eq!(blue.attachments_policy, "auto");

            let red_after = get_agent(&cx, &pool, project_id, "RedFox")
                .await
                .into_result()
                .expect("fetch red after blue registration");
            assert_eq!(
                red_after.attachments_policy, "inline",
                "registering another agent must not clobber existing attachment policy"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn list_recent_contact_agent_ids_union_matches_legacy_queries() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("recent_contact_union_matches_legacy.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-recent-union-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("union sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            let peer_sent = create_agent(
                &cx,
                &pool,
                project_id,
                "GreenCastle",
                "e2e-test",
                "test-model",
                Some("union peer sent"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sent peer");
            let peer_sent_id = peer_sent.id.expect("peer_sent id");

            let peer_recv = create_agent(
                &cx,
                &pool,
                project_id,
                "RedBear",
                "e2e-test",
                "test-model",
                Some("union peer recv"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create recv peer");
            let peer_recv_id = peer_recv.id.expect("peer_recv id");

            let peer_extra = create_agent(
                &cx,
                &pool,
                project_id,
                "OrangeFinch",
                "e2e-test",
                "test-model",
                Some("union peer extra"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create extra peer");
            let peer_extra_id = peer_extra.id.expect("peer_extra id");

            // Older message should be filtered out by since_ts.
            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "old sent message",
                "old body",
                Some("THREAD-OLD"),
                "normal",
                false,
                "[]",
                &[(peer_sent_id, "to")],
            )
            .await
            .into_result()
            .expect("create old sent message");

            let since_ts = now_micros().saturating_sub(1_000);

            // Sent branch hit.
            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "new sent message",
                "new body",
                Some("THREAD-SENT"),
                "normal",
                false,
                "[]",
                &[(peer_sent_id, "to"), (peer_extra_id, "to")],
            )
            .await
            .into_result()
            .expect("create recent sent message");

            // Received branch hit.
            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                peer_recv_id,
                "new recv message",
                "new body",
                Some("THREAD-RECV"),
                "normal",
                false,
                "[]",
                &[(sender_id, "to")],
            )
            .await
            .into_result()
            .expect("create recent received message");

            let candidate_ids = vec![peer_sent_id, peer_recv_id, peer_extra_id];
            let union_ids = list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &candidate_ids,
                since_ts,
            )
            .await
            .into_result()
            .expect("run union implementation");
            let legacy_ids = legacy_list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &candidate_ids,
                since_ts,
            )
            .await
            .into_result()
            .expect("run legacy baseline");

            assert_eq!(union_ids, legacy_ids, "union must match legacy baseline");
            let mut expected = vec![peer_sent_id, peer_recv_id, peer_extra_id];
            expected.sort_unstable();
            assert_eq!(union_ids, expected);
        });
    }

    #[test]
    fn list_recent_contact_agent_ids_empty_candidates_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("recent_contact_empty_candidates.db");

        rt.block_on(async {
            let rows = list_recent_contact_agent_ids(&cx, &pool, 1, 1, &[], now_micros())
                .await
                .into_result()
                .expect("empty candidates should short-circuit");
            assert!(rows.is_empty());
        });
    }

    #[test]
    fn list_recent_contact_agent_ids_no_results_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("recent_contact_no_results.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-recent-empty-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("no-result sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            let peer = create_agent(
                &cx,
                &pool,
                project_id,
                "GreenCastle",
                "e2e-test",
                "test-model",
                Some("no-result peer"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create peer");
            let peer_id = peer.id.expect("peer id");

            let rows = list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &[peer_id],
                now_micros(),
            )
            .await
            .into_result()
            .expect("no-result query");
            assert!(rows.is_empty());
        });
    }

    #[test]
    fn list_recent_contact_agent_ids_dedups_bidirectional_contacts() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("recent_contact_bidirectional_dedup.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-recent-dedup-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("dedup sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            let peer = create_agent(
                &cx,
                &pool,
                project_id,
                "GreenCastle",
                "e2e-test",
                "test-model",
                Some("dedup peer"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create peer");
            let peer_id = peer.id.expect("peer id");

            let since_ts = now_micros().saturating_sub(1_000);

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "sender to peer",
                "body",
                Some("THREAD-DEDUPE-1"),
                "normal",
                false,
                "[]",
                &[(peer_id, "to")],
            )
            .await
            .into_result()
            .expect("create sender->peer");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                peer_id,
                "peer to sender",
                "body",
                Some("THREAD-DEDUPE-2"),
                "normal",
                false,
                "[]",
                &[(sender_id, "to")],
            )
            .await
            .into_result()
            .expect("create peer->sender");

            let union_ids = list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &[peer_id],
                since_ts,
            )
            .await
            .into_result()
            .expect("run union implementation");
            let legacy_ids = legacy_list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &[peer_id],
                since_ts,
            )
            .await
            .into_result()
            .expect("run legacy baseline");

            assert_eq!(union_ids, vec![peer_id]);
            assert_eq!(legacy_ids, vec![peer_id]);
        });
    }

    #[test]
    fn list_recent_contact_agent_ids_received_only_uses_agent_id_alias() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("recent_contact_received_alias.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-recent-alias-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("alias sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            let peer = create_agent(
                &cx,
                &pool,
                project_id,
                "GreenCastle",
                "e2e-test",
                "test-model",
                Some("alias peer"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create peer");
            let peer_id = peer.id.expect("peer id");

            let since_ts = now_micros().saturating_sub(1_000);

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                peer_id,
                "received only",
                "body",
                Some("THREAD-ALIAS"),
                "normal",
                false,
                "[]",
                &[(sender_id, "to")],
            )
            .await
            .into_result()
            .expect("create peer->sender");

            let union_ids = list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &[peer_id],
                since_ts,
            )
            .await
            .into_result()
            .expect("run union implementation");
            let legacy_ids = legacy_list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &[peer_id],
                since_ts,
            )
            .await
            .into_result()
            .expect("run legacy baseline");

            assert_eq!(union_ids, vec![peer_id]);
            assert_eq!(union_ids, legacy_ids);
        });
    }

    #[test]
    #[allow(clippy::cast_possible_wrap)]
    fn list_recent_contact_agent_ids_queries_across_all_candidate_chunks() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("recent_contact_candidate_cap.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-recent-cap-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("cap sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            let target = create_agent(
                &cx,
                &pool,
                project_id,
                "GreenCastle",
                "e2e-test",
                "test-model",
                Some("cap target"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create target");
            let target_id = target.id.expect("target id");

            let since_ts = now_micros().saturating_sub(1_000);

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                target_id,
                "target sent message",
                "body",
                Some("THREAD-CAP"),
                "normal",
                false,
                "[]",
                &[(sender_id, "to")],
            )
            .await
            .into_result()
            .expect("create target->sender");

            let mut candidate_ids: Vec<i64> = (0..MAX_IN_CLAUSE_ITEMS as i64)
                .map(|idx| 10_000 + idx)
                .collect();
            // Place this valid target beyond the first chunk.
            candidate_ids.push(target_id);

            let union_ids = list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &candidate_ids,
                since_ts,
            )
            .await
            .into_result()
            .expect("run union implementation");
            let legacy_ids = legacy_list_recent_contact_agent_ids(
                &cx,
                &pool,
                project_id,
                sender_id,
                &candidate_ids,
                since_ts,
            )
            .await
            .into_result()
            .expect("run legacy baseline");

            assert_eq!(
                union_ids,
                vec![target_id],
                "target in a later chunk should still match"
            );
            assert!(
                legacy_ids.is_empty(),
                "legacy baseline demonstrates the former capped behavior"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn run_like_fallback_handles_over_100_terms() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("like_fallback_100_terms.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-like-fallback-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");
            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("like fallback test"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                "term01 term02 term03 term04 term05",
                "needle payload for like fallback",
                Some("THREAD-LIKE"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create message");

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire conn");
            let search_tracked = tracked(&*conn);

            let mut terms = Vec::new();
            for _ in 0..120 {
                terms.push("needle".to_string());
            }
            assert!(terms.len() > 100, "test must use >100 terms");

            let rows = run_like_fallback(&cx, &search_tracked, project_id, &terms, 25)
                .await
                .into_result()
                .expect("run like fallback");
            assert_eq!(rows.len(), 1, "fallback should match the seeded message");

            let subject: String = rows[0].get_named("subject").expect("subject");
            assert!(
                subject.contains("term01"),
                "returned message should contain seeded subject terms"
            );
        });
    }

    #[test]
    fn run_like_fallback_uses_term_union_semantics() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("like_fallback_union.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-like-union-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");
            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("like fallback union"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                "needle only",
                "contains needle token",
                Some("THREAD-LIKE-UNION"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create message");

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire conn");
            let search_tracked = tracked(&*conn);
            let terms = vec!["needle".to_string(), "missing".to_string()];

            let rows = run_like_fallback(&cx, &search_tracked, project_id, &terms, 25)
                .await
                .into_result()
                .expect("run like fallback");
            assert_eq!(
                rows.len(),
                1,
                "fallback should match when any extracted term appears"
            );
        });
    }

    #[test]
    fn search_messages_empty_corpus_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("empty_corpus_search.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-empty-corpus-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let rows = search_messages(&cx, &pool, project_id, "needle", 25)
                .await
                .into_result()
                .expect("search on empty corpus");
            assert!(rows.is_empty());
        });
    }

    #[test]
    fn search_messages_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("search_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-search-orphaned-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("orphaned search sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                "needle subject",
                "needle body",
                Some("THREAD-ORPHANED-SEARCH"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create message");

            cleanup_committed_agent_after_consistency_failure(
                &cx, &pool, project_id, sender_id, "BlueLake",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = search_messages(&cx, &pool, project_id, "needle", 25)
                .await
                .into_result()
                .expect("search messages");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].from, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(rows[0].subject, "needle subject");
        });
    }

    #[test]
    fn search_messages_for_product_empty_corpus_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("empty_corpus_product_search.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-empty-product-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_empty_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product to project");

            let rows = search_messages_for_product(&cx, &pool, product_id, "needle", 25)
                .await
                .into_result()
                .expect("product search on empty corpus");
            assert!(rows.is_empty());
        });
    }

    #[test]
    fn search_messages_for_product_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("product_search_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-product-search-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("orphaned product search sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                "needle product subject",
                "needle product body",
                Some("THREAD-ORPHANED-PRODUCT-SEARCH"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create message");

            cleanup_committed_agent_after_consistency_failure(
                &cx, &pool, project_id, sender_id, "BlueLake",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = search_messages_for_product(&cx, &pool, product_id, "needle", 25)
                .await
                .into_result()
                .expect("product search");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].from, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(rows[0].project_id, project_id);
        });
    }

    #[test]
    fn list_product_projects_keeps_orphaned_project_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("product_list_orphaned_project.db");

        rt.block_on(async {
            let base = now_micros();
            let project =
                ensure_project(&cx, &pool, &format!("/tmp/am-product-list-orphaned-{base}"))
                    .await
                    .into_result()
                    .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_list_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let db_path = dir.path().join("product_list_orphaned_project.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let projects = list_product_projects(&cx, &pool, product_id)
                .await
                .into_result()
                .expect("list product projects");
            let orphaned_project = projects
                .iter()
                .find(|row| row.id == Some(project_id))
                .expect("orphaned project link remains visible");

            assert_eq!(
                orphaned_project.slug,
                format!("[unknown-project-{project_id}]")
            );
            assert_eq!(
                orphaned_project.human_key,
                format!("[unknown-project-{project_id}]")
            );
        });
    }

    #[test]
    fn list_projects_keeps_product_link_only_orphaned_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("list_projects_product_orphaned.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-list-projects-product-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_list_projects_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let db_path = dir.path().join("list_projects_product_orphaned.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let projects = list_projects(&cx, &pool)
                .await
                .into_result()
                .expect("list projects");
            let orphaned_project = projects
                .iter()
                .find(|row| row.id == Some(project_id))
                .expect("orphaned project remains visible");

            assert_eq!(
                orphaned_project.slug,
                format!("[unknown-project-{project_id}]")
            );
            assert_eq!(
                orphaned_project.human_key,
                format!("[unknown-project-{project_id}]")
            );
        });
    }

    #[test]
    fn get_project_by_slug_falls_back_to_orphaned_placeholder_inventory() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("get_project_by_slug_orphaned.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-get-project-by-slug-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_get_project_slug_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let db_path = dir.path().join("get_project_by_slug_orphaned.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let lookup =
                get_project_by_slug(&cx, &pool, &format!("[unknown-project-{project_id}]"))
                    .await
                    .into_result()
                    .expect("lookup orphaned placeholder slug");

            assert_eq!(lookup.id, Some(project_id));
            assert_eq!(lookup.slug, format!("[unknown-project-{project_id}]"));
            assert_eq!(lookup.human_key, format!("[unknown-project-{project_id}]"));
        });
    }

    #[test]
    fn get_project_by_human_key_falls_back_to_orphaned_placeholder_inventory() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("get_project_by_human_key_orphaned.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-get-project-by-human-key-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_get_project_human_key_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let db_path = dir.path().join("get_project_by_human_key_orphaned.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let lookup =
                get_project_by_human_key(&cx, &pool, &format!("[unknown-project-{project_id}]"))
                    .await
                    .into_result()
                    .expect("lookup orphaned placeholder human_key");

            assert_eq!(lookup.id, Some(project_id));
            assert_eq!(lookup.slug, format!("[unknown-project-{project_id}]"));
            assert_eq!(lookup.human_key, format!("[unknown-project-{project_id}]"));
        });
    }

    #[test]
    fn get_project_by_id_falls_back_to_orphaned_placeholder_inventory() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("get_project_by_id_orphaned.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-get-project-by-id-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_get_project_id_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let db_path = dir.path().join("get_project_by_id_orphaned.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let lookup = get_project_by_id(&cx, &pool, project_id)
                .await
                .into_result()
                .expect("lookup orphaned placeholder id");

            assert_eq!(lookup.id, Some(project_id));
            assert_eq!(lookup.slug, format!("[unknown-project-{project_id}]"));
            assert_eq!(lookup.human_key, format!("[unknown-project-{project_id}]"));
        });
    }

    #[test]
    fn get_product_by_key_falls_back_to_orphaned_placeholder_inventory() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("get_product_by_key_orphaned.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-get-product-by-key-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let uid = format!("prod_get_product_key_orphaned_{base}");
            let product = ensure_product(&cx, &pool, Some(uid.as_str()), Some(uid.as_str()))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_id])
                .await
                .into_result()
                .expect("link product");

            let db_path = dir.path().join("get_product_by_key_orphaned.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM products WHERE id = ?",
                    &[Value::BigInt(product_id)],
                )
                .expect("orphan product row");

            let lookup = get_product_by_key(&cx, &pool, &format!("[unknown-product-{product_id}]"))
                .await
                .into_result()
                .expect("lookup orphaned placeholder product");

            assert_eq!(lookup.id, Some(product_id));
            assert_eq!(
                lookup.product_uid,
                format!("[unknown-product-{product_id}]")
            );
            assert_eq!(lookup.name, format!("[unknown-product-{product_id}]"));
            assert!(lookup.created_at > 0);
        });
    }

    #[test]
    #[allow(clippy::similar_names)]
    #[allow(clippy::too_many_lines)]
    fn fetch_inbox_for_product_agent_uses_single_global_fan_in() {
        use asupersync::runtime::RuntimeBuilder;
        use std::sync::Arc;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("product_inbox_global_fan_in.db");

        rt.block_on(async {
            let base = now_micros();
            let project_a = ensure_project(&cx, &pool, &format!("/tmp/am-product-inbox-a-{base}"))
                .await
                .into_result()
                .expect("ensure project a");
            let project_b = ensure_project(&cx, &pool, &format!("/tmp/am-product-inbox-b-{base}"))
                .await
                .into_result()
                .expect("ensure project b");
            let project_c = ensure_project(&cx, &pool, &format!("/tmp/am-product-inbox-c-{base}"))
                .await
                .into_result()
                .expect("ensure project c");
            let project_a_id = project_a.id.expect("project a id");
            let project_b_id = project_b.id.expect("project b id");
            let project_c_id = project_c.id.expect("project c id");
            let mut linked_project_ids = vec![project_a_id, project_b_id, project_c_id];
            const EXTRA_LINKED_PROJECTS: usize = 40;
            for index in 0..EXTRA_LINKED_PROJECTS {
                let project = ensure_project(
                    &cx,
                    &pool,
                    &format!("/tmp/am-product-inbox-extra-{base}-{index}"),
                )
                .await
                .into_result()
                .expect("ensure extra linked project");
                linked_project_ids.push(project.id.expect("extra project id"));
            }

            let product_uid = format!("prod_product_inbox_{base}");
            let product = ensure_product(&cx, &pool, Some(&product_uid), Some(&product_uid))
                .await
                .into_result()
                .expect("ensure product");
            let product_id = product.id.expect("product id");
            link_product_to_projects(&cx, &pool, product_id, &linked_project_ids)
                .await
                .into_result()
                .expect("link product projects");
            assert_eq!(linked_project_ids.len(), EXTRA_LINKED_PROJECTS + 3);

            let recipient_a = create_agent(
                &cx,
                &pool,
                project_a_id,
                "BlueLake",
                "test",
                "model",
                Some("recipient a"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create recipient a");
            let recipient_b = create_agent(
                &cx,
                &pool,
                project_b_id,
                "BlueLake",
                "test",
                "model",
                Some("recipient b"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create recipient b");
            let sender_a = create_agent(
                &cx,
                &pool,
                project_a_id,
                "RedStone",
                "test",
                "model",
                Some("sender a"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender a");
            let sender_b = create_agent(
                &cx,
                &pool,
                project_b_id,
                "GreenCastle",
                "test",
                "model",
                Some("sender b"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender b");
            let sender_c = create_agent(
                &cx,
                &pool,
                project_c_id,
                "PurpleHarbor",
                "test",
                "model",
                Some("sender c"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender c");

            let older = create_message_with_recipients(
                &cx,
                &pool,
                project_a_id,
                sender_a.id.expect("sender a id"),
                "older normal",
                "older body",
                Some("prod-inbox"),
                "normal",
                false,
                "[]",
                &[(recipient_a.id.expect("recipient a id"), "to")],
            )
            .await
            .into_result()
            .expect("create older message");
            let newer = create_message_with_recipients(
                &cx,
                &pool,
                project_b_id,
                sender_b.id.expect("sender b id"),
                "newer urgent",
                "newer body",
                Some("prod-inbox"),
                "urgent",
                true,
                "[]",
                &[(recipient_b.id.expect("recipient b id"), "to")],
            )
            .await
            .into_result()
            .expect("create newer message");
            let skipped = create_message_with_recipients(
                &cx,
                &pool,
                project_c_id,
                sender_c.id.expect("sender c id"),
                "should skip",
                "skip body",
                Some("prod-inbox"),
                "urgent",
                false,
                "[]",
                &[(sender_c.id.expect("sender c id"), "to")],
            )
            .await
            .into_result()
            .expect("create skipped message");

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire conn");
            let base_ts = 1_800_000_000_000_000_i64;
            for (id, ts, body) in [
                (older.id.expect("older id"), base_ts, "older body hydrated"),
                (
                    newer.id.expect("newer id"),
                    base_ts + 1_000_000,
                    "newer body hydrated",
                ),
                (
                    skipped.id.expect("skipped id"),
                    base_ts + 2_000_000,
                    "skipped body",
                ),
            ] {
                conn.execute_sync(
                    "UPDATE messages SET created_ts = ?, body_md = ? WHERE id = ?",
                    &[
                        Value::BigInt(ts),
                        Value::Text(body.to_string()),
                        Value::BigInt(id),
                    ],
                )
                .expect("stabilize message timestamp");
            }
            drop(conn);

            let tracker = Arc::new(crate::QueryTracker::new());
            tracker.enable(None);

            let metadata_rows = {
                tracker.reset();
                let _guard = crate::set_active_tracker(Arc::clone(&tracker));
                fetch_inbox_for_product_agent_metadata(
                    &cx, &pool, product_id, "BlueLake", false, None, 10,
                )
                .await
                .into_result()
                .expect("fetch product metadata inbox")
            };
            let snapshot = tracker.snapshot();
            assert_eq!(
                snapshot.total,
                1,
                "product metadata inbox should use one fan-in query for {} linked projects",
                linked_project_ids.len()
            );
            assert_eq!(
                snapshot.per_table.get("product_project_links").copied(),
                Some(1),
                "product metadata inbox should start from the product-project fan-in table"
            );
            assert_eq!(metadata_rows.len(), 2);
            assert_eq!(metadata_rows[0].message.subject, "newer urgent");
            assert_eq!(metadata_rows[1].message.subject, "older normal");
            assert!(
                metadata_rows
                    .iter()
                    .all(|row| row.message.body_md.is_empty()),
                "metadata product inbox must not hydrate message bodies"
            );

            let since_rows = {
                tracker.reset();
                let _guard = crate::set_active_tracker(Arc::clone(&tracker));
                fetch_inbox_for_product_agent_metadata(
                    &cx,
                    &pool,
                    product_id,
                    "bluelake",
                    false,
                    Some(base_ts),
                    10,
                )
                .await
                .into_result()
                .expect("fetch product metadata inbox since timestamp")
            };
            let snapshot = tracker.snapshot();
            assert_eq!(
                snapshot.total, 1,
                "product metadata inbox with since filter should use one fan-in query"
            );
            assert_eq!(since_rows.len(), 1);
            assert_eq!(since_rows[0].message.subject, "newer urgent");
            assert!(
                since_rows.iter().all(|row| row.message.body_md.is_empty()),
                "metadata product inbox must not hydrate message bodies"
            );

            let urgent_rows = {
                tracker.reset();
                let _guard = crate::set_active_tracker(Arc::clone(&tracker));
                fetch_inbox_for_product_agent(&cx, &pool, product_id, "BlueLake", true, None, 10)
                    .await
                    .into_result()
                    .expect("fetch urgent product inbox")
            };
            let snapshot = tracker.snapshot();
            assert_eq!(
                snapshot.total, 1,
                "product full-body urgent inbox should use one fan-in query"
            );
            assert_eq!(urgent_rows.len(), 1);
            assert_eq!(urgent_rows[0].message.subject, "newer urgent");
            assert_eq!(urgent_rows[0].message.body_md, "newer body hydrated");
        });
    }

    #[test]
    #[allow(clippy::similar_names)]
    #[allow(clippy::too_many_lines)]
    fn search_messages_for_product_ranks_across_projects() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("product_search_across_projects.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project_a = ensure_project(&cx, &pool, &format!("/tmp/am-prod-search-a-{base}"))
                .await
                .into_result()
                .expect("ensure project A");
            let project_a_id = project_a.id.expect("project A id");

            let project_b = ensure_project(&cx, &pool, &format!("/tmp/am-prod-search-b-{base}"))
                .await
                .into_result()
                .expect("ensure project B");
            let project_b_id = project_b.id.expect("project B id");

            let product_uid = format!("prod_search_rank_{base}");
            let product = ensure_product(
                &cx,
                &pool,
                Some(product_uid.as_str()),
                Some(product_uid.as_str()),
            )
            .await
            .into_result()
            .expect("ensure product");
            let product_id = product.id.expect("product id");

            link_product_to_projects(&cx, &pool, product_id, &[project_a_id, project_b_id])
                .await
                .into_result()
                .expect("link product to projects");

            let sender_a = create_agent(
                &cx,
                &pool,
                project_a_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("product search project A"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender A");
            let sender_a_id = sender_a.id.expect("sender A id");

            let sender_b = create_agent(
                &cx,
                &pool,
                project_b_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("product search project B"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender B");
            let sender_b_id = sender_b.id.expect("sender B id");

            create_message(
                &cx,
                &pool,
                project_a_id,
                sender_a_id,
                "alpha project-a signal",
                "body A",
                Some("THREAD-A"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create project A message");

            create_message(
                &cx,
                &pool,
                project_b_id,
                sender_b_id,
                "alpha project-b signal",
                "body B",
                Some("THREAD-B"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create project B message");

            // Base schema intentionally omits FTS virtual tables, so this query
            // deterministically exercises LIKE fallback across linked projects.
            let rows = search_messages_for_product(&cx, &pool, product_id, "alpha", 25)
                .await
                .into_result()
                .expect("search messages for product");

            assert_eq!(rows.len(), 2, "must return hits from both linked projects");
            assert_eq!(
                rows[0].project_id, project_b_id,
                "project B should rank first due to id DESC in LIKE fallback"
            );
            assert_eq!(
                rows[1].project_id, project_a_id,
                "project A should rank second"
            );
            assert_eq!(rows[0].subject, "alpha project-b signal");
            assert_eq!(rows[1].subject, "alpha project-a signal");
        });
    }

    #[test]
    fn expired_reservations_query_uses_inclusive_cutoff() {
        let select_sql = format!(
            "SELECT id FROM file_reservations \
             WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts <= ?"
        );
        assert!(select_sql.contains("expires_ts <= ?"));
        assert!(!select_sql.contains("expires_ts < ?"));
        // #154 item 1: the release-ledger exclusion is an uncorrelated anti-join
        // (`id NOT IN (SELECT reservation_id ...)`), not a correlated `NOT EXISTS`
        // subquery FrankenSQLite routes to its slow in-memory interpreter.
        assert!(select_sql.contains("NOT IN"));
        assert!(!select_sql.contains("NOT EXISTS"));
    }

    #[test]
    fn active_reservation_predicate_for_alias_retargets_release_ledger_probe() {
        let aliased = active_reservation_predicate_for("fr");
        // The uncorrelated ledger anti-join is retargeted to the alias.
        assert!(aliased.contains("fr.id NOT IN"));
        assert!(!aliased.contains("file_reservations.id NOT IN"));
        assert!(aliased.contains("SELECT reservation_id FROM file_reservation_releases"));
        assert!(aliased.contains("fr.released_ts IS NULL"));
    }

    #[test]
    fn active_reservation_predicate_avoids_correlated_subquery() {
        // The correlated `NOT EXISTS (... WHERE reservation_id = file_reservations.id)`
        // form degraded to ~5s on a 30k-row mailbox under FrankenSQLite (#154
        // item 1: routed to the in-memory interpreter). The uncorrelated set
        // form materializes the released set once.
        assert!(ACTIVE_RESERVATION_PREDICATE.contains(
            "file_reservations.id NOT IN (\n        SELECT reservation_id FROM file_reservation_releases"
        ));
        assert!(!ACTIVE_RESERVATION_PREDICATE.contains("NOT EXISTS"));
        assert!(!ACTIVE_RESERVATION_PREDICATE.contains("WHERE reservation_id ="));
    }

    #[test]
    fn aliased_active_reservation_queries_use_alias_safe_predicate() {
        let active_predicate = active_reservation_predicate_for("fr");
        let get_active_sql = format!(
            "SELECT fr.id \
             FROM file_reservations fr \
             LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
             WHERE fr.project_id = ? AND ({active_predicate}) AND fr.expires_ts > ?"
        );
        let list_active_sql = format!(
            "SELECT fr.id \
             FROM file_reservations fr \
             LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
             WHERE fr.project_id = ? AND ({active_predicate}) AND fr.expires_ts > ? ORDER BY fr.id"
        );

        for sql in [get_active_sql, list_active_sql] {
            assert!(
                sql.contains("fr.released_ts"),
                "expected aliased released_ts in: {sql}"
            );
            assert!(
                sql.contains("reservation_id = fr.id"),
                "expected aliased ledger join in: {sql}"
            );
            assert!(
                !sql.contains("file_reservations.released_ts"),
                "raw table-qualified predicate must not leak into aliased query: {sql}"
            );
        }
    }

    // ─── Global query tests (br-2bbt.14.1) ───────────────────────────────────

    #[test]
    fn global_inbox_row_struct_has_project_context() {
        // Verify GlobalInboxRow struct has all required fields
        let row = GlobalInboxRow {
            message: MessageRow {
                id: Some(1),
                project_id: 10,
                sender_id: 100,
                thread_id: Some("t1".to_string()),
                subject: "Test".to_string(),
                body_md: "Body".to_string(),
                importance: "normal".to_string(),
                ack_required: 0,
                created_ts: 1000,
                recipients_json: "{}".to_string(),
                attachments: "[]".to_string(),
            },
            kind: "to".to_string(),
            sender_name: "Alice".to_string(),
            ack_ts: None,
            project_id: 10,
            project_slug: "my-project".to_string(),
        };

        assert_eq!(row.project_id, 10);
        assert_eq!(row.project_slug, "my-project");
        assert_eq!(row.message.subject, "Test");
    }

    #[test]
    fn project_unread_count_struct_has_required_fields() {
        let count = ProjectUnreadCount {
            project_id: 1,
            project_slug: "backend".to_string(),
            unread_count: 42,
        };

        assert_eq!(count.project_id, 1);
        assert_eq!(count.project_slug, "backend");
        assert_eq!(count.unread_count, 42);
    }

    #[test]
    fn global_search_row_struct_has_project_context() {
        let row = GlobalSearchRow {
            id: 1,
            sender_id: 0,
            subject: "Hello".to_string(),
            importance: "high".to_string(),
            ack_required: 1,
            created_ts: 2000,
            thread_id: Some("thread-1".to_string()),
            from: "Bob".to_string(),
            body_md: "Content here".to_string(),
            project_id: 5,
            project_slug: "frontend".to_string(),
        };

        assert_eq!(row.id, 1);
        assert_eq!(row.project_id, 5);
        assert_eq!(row.project_slug, "frontend");
        assert_eq!(row.from, "Bob");
    }

    #[test]
    fn fetch_inbox_global_empty_database_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("global_inbox_empty.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let _ = ensure_project(&cx, &pool, &format!("/tmp/am-global-empty-{base}"))
                .await
                .into_result()
                .expect("ensure project");

            // Query for non-existent agent
            let rows = fetch_inbox_global(&cx, &pool, "NonExistentAgent", false, None, 25)
                .await
                .into_result()
                .expect("fetch inbox global on empty");

            assert!(rows.is_empty());
        });
    }

    #[test]
    fn fetch_inbox_global_matches_agent_name_case_insensitively() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("global_inbox_case_insensitive.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-global-case-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Case-insensitive global inbox",
                "Body",
                Some("global-case-thread"),
                "normal",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            let rows = fetch_inbox_global(&cx, &pool, "bluelake", false, None, 25)
                .await
                .into_result()
                .expect("fetch inbox global");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].sender_name, "GreenStone");
            assert_eq!(rows[0].project_slug, project.slug);
        });
    }

    #[test]
    fn fetch_inbox_global_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("global_inbox_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project =
                ensure_project(&cx, &pool, &format!("/tmp/am-global-inbox-orphaned-{base}"))
                    .await
                    .into_result()
                    .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let sender_id = sender.id.expect("sender id");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "Global inbox survives sender drift",
                "Body",
                Some("global-inbox-orphaned-thread"),
                "normal",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            cleanup_committed_agent_after_consistency_failure(
                &cx,
                &pool,
                project_id,
                sender_id,
                "GreenStone",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = fetch_inbox_global(&cx, &pool, "BlueLake", false, None, 25)
                .await
                .into_result()
                .expect("fetch global inbox");

            let row = rows
                .iter()
                .find(|row| row.message.subject == "Global inbox survives sender drift")
                .expect("find orphaned global inbox row");
            assert_eq!(row.sender_name, UNKNOWN_SENDER_DISPLAY);
        });
    }

    #[test]
    fn fetch_inbox_global_keeps_orphaned_project_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("global_inbox_orphaned_project.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-global-inbox-orphaned-project-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Global inbox survives project drift",
                "Body",
                Some("global-inbox-orphaned-project-thread"),
                "normal",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            let db_path = dir.path().join("global_inbox_orphaned_project.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let rows = fetch_inbox_global(&cx, &pool, "BlueLake", false, None, 25)
                .await
                .into_result()
                .expect("fetch global inbox");

            let row = rows
                .iter()
                .find(|row| row.message.subject == "Global inbox survives project drift")
                .expect("find orphaned-project global inbox row");
            assert_eq!(row.project_id, project_id);
            assert_eq!(row.project_slug, format!("[unknown-project-{project_id}]"));
        });
    }

    #[test]
    fn fetch_unacked_for_agent_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("unacked_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-unacked-orphaned-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let sender_id = sender.id.expect("sender id");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");
            let recipient_id = recipient.id.expect("recipient id");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "Ack survives sender drift",
                "Body",
                Some("unacked-orphaned-thread"),
                "high",
                true,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
            .into_result()
            .expect("create ack-required message");

            cleanup_committed_agent_after_consistency_failure(
                &cx,
                &pool,
                project_id,
                sender_id,
                "GreenStone",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = fetch_unacked_for_agent(&cx, &pool, project_id, recipient_id, 25)
                .await
                .into_result()
                .expect("fetch unacked for agent");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].sender_name, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(rows[0].message.subject, "Ack survives sender drift");
            assert_eq!(rows[0].kind, "to");
        });
    }

    #[test]
    fn fetch_inbox_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("inbox_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-inbox-orphaned-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let sender_id = sender.id.expect("sender id");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");
            let recipient_id = recipient.id.expect("recipient id");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "Inbox survives sender drift",
                "Body",
                Some("inbox-orphaned-thread"),
                "normal",
                false,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
            .into_result()
            .expect("create inbox message");

            cleanup_committed_agent_after_consistency_failure(
                &cx,
                &pool,
                project_id,
                sender_id,
                "GreenStone",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = fetch_inbox(&cx, &pool, project_id, recipient_id, false, None, 25)
                .await
                .into_result()
                .expect("fetch inbox");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].sender_name, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(rows[0].message.subject, "Inbox survives sender drift");
        });
    }

    #[test]
    fn fetch_inbox_ack_required_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("inbox_ack_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-inbox-ack-orphaned-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let sender_id = sender.id.expect("sender id");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");
            let recipient_id = recipient.id.expect("recipient id");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender_id,
                "Ack inbox survives sender drift",
                "Body",
                Some("inbox-ack-orphaned-thread"),
                "high",
                true,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
            .into_result()
            .expect("create ack inbox message");

            cleanup_committed_agent_after_consistency_failure(
                &cx,
                &pool,
                project_id,
                sender_id,
                "GreenStone",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = fetch_inbox_ack_required(&cx, &pool, project_id, recipient_id, 25)
                .await
                .into_result()
                .expect("fetch ack-required inbox");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].sender_name, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(rows[0].message.subject, "Ack inbox survives sender drift");
        });
    }

    #[test]
    fn recipient_lookup_helpers_keep_orphaned_recipient_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("recipient_lookup_orphaned_agent.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-recipient-lookup-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                Some("sender"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                Some("recipient"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register recipient");
            let recipient_id = recipient.id.expect("recipient id");

            let message = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Recipient helpers survive agent drift",
                "Body",
                Some("recipient-lookup-orphaned-thread"),
                "normal",
                false,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
            .into_result()
            .expect("create message");
            let message_id = message.id.expect("message id");

            let db_path = dir.path().join("recipient_lookup_orphaned_agent.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            let recipient_rows_before_delete = repair_conn
                .query_sync(
                    "SELECT COUNT(*) AS count FROM message_recipients WHERE message_id = ? AND agent_id = ?",
                    &[Value::BigInt(message_id), Value::BigInt(recipient_id)],
                )
                .expect("count recipient rows before orphaning");
            assert_eq!(
                recipient_rows_before_delete[0]
                    .get_named::<i64>("count")
                    .unwrap_or(-1),
                1,
                "message creation must persist the message_recipients row"
            );
            repair_conn
                .execute_sync(
                    "DELETE FROM agents WHERE id = ? AND project_id = ?",
                    &[Value::BigInt(recipient_id), Value::BigInt(project_id)],
                )
                .expect("orphan recipient row");
            repair_conn
                .execute_sync(
                    "INSERT OR IGNORE INTO message_recipients \
                     (message_id, agent_id, kind, read_ts, ack_ts) \
                     VALUES (?, ?, 'to', NULL, NULL)",
                    &[Value::BigInt(message_id), Value::BigInt(recipient_id)],
                )
                .expect("restore legacy orphaned recipient row fixture");
            let recipient_rows = repair_conn
                .query_sync(
                    "SELECT COUNT(*) AS count FROM message_recipients WHERE message_id = ? AND agent_id = ?",
                    &[Value::BigInt(message_id), Value::BigInt(recipient_id)],
                )
                .expect("count orphaned recipient rows");
            assert_eq!(
                recipient_rows[0].get_named::<i64>("count").unwrap_or(-1),
                1,
                "intentional orphan fixture must keep the message_recipients row"
            );
            crate::cache::read_cache().invalidate_agent_scoped(
                &cache_scope_for_pool(&pool),
                project_id,
                "BlueLake",
                Some(recipient_id),
            );

            let expected = format!("[unknown-agent-{recipient_id}]");

            let names =
                list_message_recipient_names_for_messages(&cx, &pool, project_id, &[message_id])
                    .await
                    .into_result()
                    .expect("list recipient names for messages");
            assert_eq!(names, vec![expected.clone()]);

            let details = list_message_recipients_by_message(&cx, &pool, project_id, message_id)
                .await
                .into_result()
                .expect("list recipients by message");
            assert_eq!(details.len(), 1);
            assert_eq!(details[0].name, expected);
            assert_eq!(details[0].kind, "to");

            let grouped = list_message_recipient_names_by_message(&cx, &pool, &[message_id])
                .await
                .into_result()
                .expect("list recipient names by message");
            assert_eq!(
                grouped.get(&message_id),
                Some(&vec![format!("[unknown-agent-{recipient_id}]")])
            );
        });
    }

    #[test]
    fn count_unread_global_empty_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("global_unread_empty.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let _ = ensure_project(&cx, &pool, &format!("/tmp/am-unread-empty-{base}"))
                .await
                .into_result()
                .expect("ensure project");

            let counts = count_unread_global(&cx, &pool, "NonExistentAgent")
                .await
                .into_result()
                .expect("count unread global on empty");

            assert!(counts.is_empty());
        });
    }

    #[test]
    fn count_unread_global_matches_agent_name_case_insensitively() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("global_unread_case_insensitive.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(&cx, &pool, &format!("/tmp/am-unread-case-{base}"))
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Case-insensitive unread count",
                "Body",
                Some("global-unread-thread"),
                "high",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            let counts = count_unread_global(&cx, &pool, "bluelake")
                .await
                .into_result()
                .expect("count unread global");

            assert_eq!(counts.len(), 1);
            assert_eq!(counts[0].project_slug, project.slug);
            assert_eq!(counts[0].unread_count, 1);
        });
    }

    #[test]
    fn count_unread_global_keeps_orphaned_project_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("global_unread_orphaned_project.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-unread-orphaned-project-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Unread count survives project drift",
                "Body",
                Some("global-unread-orphaned-project-thread"),
                "high",
                false,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            let db_path = dir.path().join("global_unread_orphaned_project.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let counts = count_unread_global(&cx, &pool, "BlueLake")
                .await
                .into_result()
                .expect("count unread global");

            let orphaned_project = counts
                .iter()
                .find(|count| count.project_id == project_id)
                .expect("orphaned project unread count remains visible");
            assert_eq!(
                orphaned_project.project_slug,
                format!("[unknown-project-{project_id}]")
            );
            assert_eq!(orphaned_project.unread_count, 1);
        });
    }

    #[test]
    fn search_messages_global_empty_corpus_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("global_search_empty.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let base = now_micros();
            let _ = ensure_project(&cx, &pool, &format!("/tmp/am-search-empty-{base}"))
                .await
                .into_result()
                .expect("ensure project");

            let rows = search_messages_global(&cx, &pool, "needle", 25)
                .await
                .into_result()
                .expect("search global on empty corpus");

            assert!(rows.is_empty());
        });
    }

    #[test]
    fn search_messages_global_empty_query_returns_empty() {
        use asupersync::runtime::RuntimeBuilder;
        use tempfile::tempdir;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("global_search_empty_q.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string())
            .expect("open base schema connection");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("apply init PRAGMAs");
        let init_sql = crate::schema::init_schema_sql_base();
        init_conn
            .execute_raw(&init_sql)
            .expect("initialize base schema");
        drop(init_conn);
        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            let rows = search_messages_global(&cx, &pool, "", 25)
                .await
                .into_result()
                .expect("search global with empty query");

            assert!(rows.is_empty());
        });
    }

    #[test]
    fn search_messages_global_keeps_orphaned_sender_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("global_search_orphaned_sender.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-global-search-orphaned-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("orphaned global search sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");
            let sender_id = sender.id.expect("sender id");

            create_message(
                &cx,
                &pool,
                project_id,
                sender_id,
                "needle global subject",
                "needle global body",
                Some("THREAD-ORPHANED-GLOBAL-SEARCH"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create message");

            cleanup_committed_agent_after_consistency_failure(
                &cx, &pool, project_id, sender_id, "BlueLake",
            )
            .await
            .into_result()
            .expect("orphan sender row");

            let rows = search_messages_global(&cx, &pool, "needle", 25)
                .await
                .into_result()
                .expect("search global");

            let row = rows
                .iter()
                .find(|row| row.subject == "needle global subject")
                .expect("find orphaned global search row");
            assert_eq!(row.from, UNKNOWN_SENDER_DISPLAY);
            assert_eq!(row.project_id, project_id);
        });
    }

    #[test]
    fn search_messages_global_keeps_orphaned_project_rows_visible() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, dir) = setup_test_pool("global_search_orphaned_project.db");

        rt.block_on(async {
            let base = now_micros();
            let project = ensure_project(
                &cx,
                &pool,
                &format!("/tmp/am-global-search-orphaned-project-{base}"),
            )
            .await
            .into_result()
            .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = create_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "e2e-test",
                "test-model",
                Some("orphaned global search project sender"),
                Some("auto"),
            )
            .await
            .into_result()
            .expect("create sender");

            create_message(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "needle global project subject",
                "needle global project body",
                Some("THREAD-ORPHANED-GLOBAL-PROJECT-SEARCH"),
                "normal",
                false,
                "[]",
            )
            .await
            .into_result()
            .expect("create message");

            let db_path = dir.path().join("global_search_orphaned_project.db");
            let repair_conn = open_direct_repair_connection(&db_path);
            repair_conn
                .execute_sync(
                    "DELETE FROM projects WHERE id = ?",
                    &[Value::BigInt(project_id)],
                )
                .expect("orphan project row");

            let rows = search_messages_global(&cx, &pool, "needle", 25)
                .await
                .into_result()
                .expect("search global");

            let row = rows
                .iter()
                .find(|row| row.subject == "needle global project subject")
                .expect("find orphaned-project global search row");
            assert_eq!(row.project_id, project_id);
            assert_eq!(row.project_slug, format!("[unknown-project-{project_id}]"));
            assert_eq!(row.from, "BlueLake");
        });
    }

    // ─── rebuild_indexes removal regression tests (br-3h13.16.5) ────────────

    #[test]
    #[allow(clippy::too_many_lines)]
    fn write_ops_succeed_without_reindex_even_with_data_issues() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("no_reindex_regression.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string()).expect("open");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("pragmas");
        init_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("base schema");

        // Insert a project and agent for project=1
        init_conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('proj1', '/tmp/proj1', 0)",
            )
            .expect("insert proj1");
        init_conn
            .execute_raw(
                "INSERT INTO agents (project_id, name, program, model, task_description, \
                 inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (1, 'RedFox', 'cc', 'opus', '', 0, 0, 'auto', 'auto')",
            )
            .expect("insert agent");

        // Simulate data issue: drop the NOCASE unique index, then insert
        // case-duplicate agents in project=2 (a different project).
        init_conn
            .execute_raw(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('proj2', '/tmp/proj2', 0)",
            )
            .expect("insert proj2");
        init_conn
            .execute_raw("DROP INDEX IF EXISTS idx_agents_project_name_nocase")
            .ok();
        init_conn
            .execute_raw(
                "INSERT INTO agents (project_id, name, program, model, task_description, \
                 inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (2, 'BlueLake', 'cc', 'opus', '', 0, 0, 'auto', 'auto')",
            )
            .expect("insert BlueLake proj2");
        init_conn
            .execute_raw(
                "INSERT INTO agents (project_id, name, program, model, task_description, \
                 inception_ts, last_active_ts, attachments_policy, contact_policy) \
                 VALUES (2, 'bluelake', 'cc', 'opus', '', 0, 0, 'auto', 'auto')",
            )
            .expect("insert bluelake (case dup) proj2");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            // ensure_project for a NEW project should work despite proj2 data issues
            let proj3 = ensure_project(&cx, &pool, "/tmp/proj3")
                .await
                .into_result()
                .expect("ensure_project should succeed without REINDEX");
            assert!(proj3.id.is_some());

            // register_agent on proj1 should work
            let agent = register_agent(
                &cx,
                &pool,
                1,
                "RedFox",
                "claude-code",
                "opus-4.6",
                Some("regression test"),
                Some("auto"),
                None,
            )
            .await
            .into_result()
            .expect("register_agent should succeed without REINDEX");
            assert_eq!(agent.name, "RedFox");

            // create_agent on proj3 should work
            let proj3_id = proj3.id.unwrap();
            let new_agent = create_agent(
                &cx, &pool, proj3_id, "GoldHawk", "codex", "gpt-5.2", None, None,
            )
            .await
            .into_result()
            .expect("create_agent should succeed without REINDEX");
            assert_eq!(new_agent.name, "GoldHawk");

            // Verify all data is queryable via indexes
            let fetched = get_agent(&cx, &pool, 1, "RedFox")
                .await
                .into_result()
                .expect("index lookup should work without REINDEX");
            assert_eq!(fetched.program, "claude-code");
        });
    }

    #[test]
    fn commit_tx_and_contact_policy_ops_work_without_reindex() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("no_reindex_ops.db");
        let init_conn = crate::DbConn::open_file(db_path.display().to_string()).expect("open");
        init_conn
            .execute_raw(crate::schema::PRAGMA_DB_INIT_SQL)
            .expect("pragmas");
        init_conn
            .execute_raw(&crate::schema::init_schema_sql_base())
            .expect("base schema");
        drop(init_conn);

        let cfg = crate::pool::DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 1,
            max_connections: 1,
            run_migrations: false,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool = crate::create_pool(&cfg).expect("create pool");

        rt.block_on(async {
            // Setup: create project + agent
            let project = ensure_project(&cx, &pool, "/tmp/commit-ops-test")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.unwrap();

            let agent = register_agent(
                &cx,
                &pool,
                project_id,
                "SwiftPeak",
                "cc",
                "opus",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register agent");
            let agent_id = agent.id.unwrap();

            // Test set_agent_contact_policy
            let updated = set_agent_contact_policy(&cx, &pool, agent_id, "open")
                .await
                .into_result()
                .expect("set_agent_contact_policy should succeed without REINDEX");
            assert_eq!(updated.contact_policy, "open");

            // Test set_agent_contact_policy_by_name
            let updated2 =
                set_agent_contact_policy_by_name(&cx, &pool, project_id, "SwiftPeak", "closed")
                    .await
                    .into_result()
                    .expect("set_agent_contact_policy_by_name should succeed without REINDEX");
            assert_eq!(updated2.contact_policy, "closed");

            // Test flush_deferred_touches (even when cache is empty, should not error)
            flush_deferred_touches(&cx, &pool)
                .await
                .into_result()
                .expect("flush_deferred_touches should succeed without REINDEX");

            // Seed the touch cache and verify flush works
            crate::cache::read_cache().enqueue_touch_scoped(
                &cache_scope_for_pool(&pool),
                agent_id,
                now_micros(),
            );
            flush_deferred_touches(&cx, &pool)
                .await
                .into_result()
                .expect("flush_deferred_touches with pending touch should succeed");

            // Verify the agent's last_active_ts was updated
            let refetched = get_agent(&cx, &pool, project_id, "SwiftPeak")
                .await
                .into_result()
                .expect("refetch agent");
            assert!(
                refetched.last_active_ts > 0,
                "last_active_ts should be updated after touch flush"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn deferred_touch_flush_is_scoped_to_memory_pool() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();
        let unique = format!(
            "{}-{}",
            std::process::id(),
            mcp_agent_mail_core::timestamps::now_micros()
        );

        let cfg_a = crate::pool::DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            storage_root: Some(std::path::PathBuf::from(format!(
                "/tmp/deferred-touch-scope-a-{unique}"
            ))),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let cfg_b = crate::pool::DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            storage_root: Some(std::path::PathBuf::from(format!(
                "/tmp/deferred-touch-scope-b-{unique}"
            ))),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool_a = crate::create_pool(&cfg_a).expect("create pool a");
        let pool_b = crate::create_pool(&cfg_b).expect("create pool b");

        rt.block_on(async {
            let conn_a = acquire_conn(&cx, &pool_a)
                .await
                .into_result()
                .expect("acquire a");
            crate::schema::migrate_to_latest_base(&cx, &*conn_a)
                .await
                .into_result()
                .expect("migrate a");
            drop(conn_a);
            let conn_b = acquire_conn(&cx, &pool_b)
                .await
                .into_result()
                .expect("acquire b");
            crate::schema::migrate_to_latest_base(&cx, &*conn_b)
                .await
                .into_result()
                .expect("migrate b");
            drop(conn_b);

            let project_a = ensure_project(&cx, &pool_a, "/tmp/deferred-touch-scope-a")
                .await
                .into_result()
                .expect("ensure project a");
            let project_b = ensure_project(&cx, &pool_b, "/tmp/deferred-touch-scope-b")
                .await
                .into_result()
                .expect("ensure project b");

            let agent_a = register_agent(
                &cx,
                &pool_a,
                project_a.id.expect("project a id"),
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register agent a");
            let agent_b = register_agent(
                &cx,
                &pool_b,
                project_b.id.expect("project b id"),
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register agent b");

            let first_agent_id = agent_a.id.expect("agent a id");
            let second_agent_id = agent_b.id.expect("agent b id");
            assert_eq!(
                first_agent_id, second_agent_id,
                "fresh in-memory pools should allocate matching first agent ids"
            );

            set_agent_last_active_for_test(&cx, &pool_a, first_agent_id, 0).await;
            set_agent_last_active_for_test(&cx, &pool_b, second_agent_id, 0).await;

            touch_agent(&cx, &pool_a, first_agent_id)
                .await
                .into_result()
                .expect("queue deferred touch in pool a");

            flush_deferred_touches(&cx, &pool_b)
                .await
                .into_result()
                .expect("flush deferred touches for pool b");
            assert_eq!(
                read_agent_last_active_for_test(&cx, &pool_b, second_agent_id).await,
                0,
                "pool b flush must not consume deferred touches from pool a"
            );

            flush_deferred_touches(&cx, &pool_a)
                .await
                .into_result()
                .expect("flush deferred touches for pool a");
            assert!(
                read_agent_last_active_for_test(&cx, &pool_a, first_agent_id).await > 0,
                "pool a flush should still apply its own deferred touch"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn mark_all_messages_read_in_project_marks_large_inboxes_without_touching_other_projects() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("mark_all_messages_read_in_project_large.db");

        rt.block_on(async {
            let project_a = ensure_project(&cx, &pool, "/tmp/am-mark-all-read-large-a")
                .await
                .into_result()
                .expect("ensure project A");
            let project_b = ensure_project(&cx, &pool, "/tmp/am-mark-all-read-large-b")
                .await
                .into_result()
                .expect("ensure project B");

            let sender_a = register_agent(
                &cx,
                &pool,
                project_a.id.unwrap_or(0),
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender A");
            let recipient_a = register_agent(
                &cx,
                &pool,
                project_a.id.unwrap_or(0),
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient A");
            let sender_b = register_agent(
                &cx,
                &pool,
                project_b.id.unwrap_or(0),
                "RedCat",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender B");
            let recipient_b = register_agent(
                &cx,
                &pool,
                project_b.id.unwrap_or(0),
                "AmberHill",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient B");

            let conn = crate::open_sqlite_file_with_recovery(pool.sqlite_path())
                .expect("open sqlite connection");
            conn.execute_raw("BEGIN IMMEDIATE")
                .expect("begin insert transaction");
            for idx in 0_i64..10_050 {
                let message_id = idx + 1;
                let created_ts = 1_700_000_000_000_000_i64 + idx;
                conn.execute_raw(&format!(
                    "INSERT INTO messages \
                     (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                     VALUES ({message_id}, {}, {}, 'bulk-thread', 'bulk-{message_id}', 'body', 'normal', 0, {created_ts}, '[]')",
                    project_a.id.unwrap_or(0),
                    sender_a.id.unwrap_or(0),
                ))
                .expect("insert project A message");
                conn.execute_raw(&format!(
                    "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
                     VALUES ({message_id}, {}, 'to', NULL, NULL)",
                    recipient_a.id.unwrap_or(0),
                ))
                .expect("insert project A recipient");
            }
            conn.execute_raw(&format!(
                "INSERT INTO messages \
                 (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                 VALUES (20001, {}, {}, 'other-thread', 'other', 'body', 'normal', 0, 1800000000000000, '[]')",
                project_b.id.unwrap_or(0),
                sender_b.id.unwrap_or(0),
            ))
            .expect("insert project B message");
            conn.execute_raw(&format!(
                "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
                 VALUES (20001, {}, 'to', NULL, NULL)",
                recipient_b.id.unwrap_or(0),
            ))
            .expect("insert project B recipient");
            conn.execute_raw("COMMIT").expect("commit seed transaction");
            drop(conn);

            let marked_count = mark_all_messages_read_in_project(
                &cx,
                &pool,
                project_a.id.unwrap_or(0),
                recipient_a.id.unwrap_or(0),
            )
            .await
            .into_result()
            .expect("mark large project inbox read");

            assert_eq!(marked_count, 10_050);

            let unread_a = fetch_inbox_unread(
                &cx,
                &pool,
                project_a.id.unwrap_or(0),
                recipient_a.id.unwrap_or(0),
                false,
                None,
                10_100,
            )
            .await
            .into_result()
            .expect("fetch unread project A");
            assert!(
                unread_a.is_empty(),
                "project A inbox should be fully marked read"
            );

            let unread_b = fetch_inbox_unread(
                &cx,
                &pool,
                project_b.id.unwrap_or(0),
                recipient_b.id.unwrap_or(0),
                false,
                None,
                10,
            )
            .await
            .into_result()
            .expect("fetch unread project B");
            assert_eq!(unread_b.len(), 1, "other project inbox must stay unread");
        });
    }

    #[test]
    #[allow(clippy::similar_names)]
    #[allow(clippy::too_many_lines)]
    fn acknowledge_messages_batch_marks_large_ack_wave_with_per_item_status() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("acknowledge_messages_batch_large_wave.db");

        rt.block_on(async {
            let project_a = ensure_project(&cx, &pool, "/tmp/am-batch-ack-project-a")
                .await
                .into_result()
                .expect("ensure project A");
            let project_b = ensure_project(&cx, &pool, "/tmp/am-batch-ack-project-b")
                .await
                .into_result()
                .expect("ensure project B");
            let project_a_id = project_a.id.expect("project A id");
            let project_b_id = project_b.id.expect("project B id");

            let sender_a = register_agent(
                &cx,
                &pool,
                project_a_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender A");
            let recipient_a = register_agent(
                &cx,
                &pool,
                project_a_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient A");
            let sender_b = register_agent(
                &cx,
                &pool,
                project_b_id,
                "AmberHill",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender B");
            let recipient_b = register_agent(
                &cx,
                &pool,
                project_b_id,
                "PurpleRiver",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient B");

            let recipient_a_id = recipient_a.id.expect("recipient A id");
            let recipient_b_id = recipient_b.id.expect("recipient B id");
            let sender_a_id = sender_a.id.expect("sender A id");
            let sender_b_id = sender_b.id.expect("sender B id");

            let mut existing_ids = Vec::with_capacity(120);
            for idx in 0_i64..120 {
                let message = create_message_with_recipients(
                    &cx,
                    &pool,
                    project_a_id,
                    sender_a_id,
                    &format!("batch-{idx}"),
                    "body",
                    Some("batch-ack"),
                    "normal",
                    true,
                    "[]",
                    &[(recipient_a_id, "to")],
                )
                .await
                .into_result()
                .expect("create project A message");
                existing_ids.push(message.id.expect("project A message id"));
            }

            let other_project_message = create_message_with_recipients(
                &cx,
                &pool,
                project_b_id,
                sender_b_id,
                "other",
                "body",
                Some("batch-ack-other"),
                "urgent",
                true,
                "[]",
                &[(recipient_b_id, "to")],
            )
            .await
            .into_result()
            .expect("create other project message");
            let other_project_id = other_project_message.id.expect("other project message id");
            let missing_id = existing_ids
                .iter()
                .copied()
                .chain(std::iter::once(other_project_id))
                .max()
                .expect("seeded message ids")
                + 10_000;

            let (old_read_ts, old_ack_ts) = acknowledge_message(
                &cx,
                &pool,
                recipient_a_id,
                existing_ids[0],
            )
            .await
            .into_result()
            .expect("pre-ack first message");

            let mut requested = existing_ids.clone();
            requested.insert(5, existing_ids[4]);
            requested.insert(20, missing_id);
            requested.push(other_project_id);

            let tracker = Arc::new(crate::QueryTracker::new());
            tracker.enable(None);
            let results = {
                tracker.reset();
                let _guard = crate::set_active_tracker(Arc::clone(&tracker));
                acknowledge_messages_batch(&cx, &pool, recipient_a_id, &requested)
                    .await
                    .into_result()
                    .expect("batch acknowledge")
            };
            let snapshot = tracker.snapshot();
            assert!(
                snapshot.total <= 7,
                "batch acknowledge should use a fixed query count for a 100+ message wave, got {snapshot:?}"
            );
            assert_eq!(
                snapshot.per_table.get("message_recipients").copied(),
                Some(2),
                "one chunked update and one chunked read-back should touch message_recipients"
            );

            assert_eq!(
                results.len(),
                existing_ids.len() + 2,
                "duplicate input IDs are collapsed in first-seen order"
            );
            assert_eq!(results[4].message_id, existing_ids[4]);
            assert_eq!(results[5].message_id, existing_ids[5]);

            let found_count = results.iter().filter(|result| result.found).count();
            assert_eq!(found_count, existing_ids.len());
            let missing: Vec<i64> = results
                .iter()
                .filter(|result| !result.found)
                .map(|result| result.message_id)
                .collect();
            assert_eq!(missing, vec![missing_id, other_project_id]);

            let first = results
                .iter()
                .find(|result| result.message_id == existing_ids[0])
                .expect("first result");
            assert_eq!(first.read_ts, Some(old_read_ts));
            assert_eq!(first.ack_ts, Some(old_ack_ts));
            for result in results.iter().filter(|result| result.found).skip(1) {
                assert!(result.read_ts.is_some(), "read_ts set for {result:?}");
                assert!(result.ack_ts.is_some(), "ack_ts set for {result:?}");
            }

            let stats = get_inbox_stats(&cx, &pool, recipient_a_id)
                .await
                .into_result()
                .expect("get recipient stats")
                .expect("recipient stats row");
            assert_eq!(stats.total_count, 120);
            assert_eq!(stats.unread_count, 0);
            assert_eq!(stats.ack_pending_count, 0);

            let conn = crate::open_sqlite_file_with_recovery(pool.sqlite_path())
                .expect("open sqlite connection for verification");
            let ph = placeholders(existing_ids.len());
            let mut acked_params = Vec::with_capacity(1 + existing_ids.len());
            acked_params.push(Value::BigInt(recipient_a_id));
            acked_params.extend(
                existing_ids
                    .iter()
                    .copied()
                    .map(Value::BigInt),
            );
            let acked_rows = conn
                .query_sync(
                    &format!(
                        "SELECT COUNT(*) AS count \
                     FROM message_recipients \
                     WHERE agent_id = ? AND message_id IN ({ph}) \
                     AND read_ts IS NOT NULL AND ack_ts IS NOT NULL"
                    ),
                    &acked_params,
                )
                .expect("query acknowledged rows");
            let acked_count: i64 = acked_rows[0].get_named("count").expect("acked count");
            assert_eq!(acked_count, 120);

            let other_rows = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients \
                     WHERE agent_id = ? AND message_id = ?",
                    &[
                        Value::BigInt(recipient_b_id),
                        Value::BigInt(other_project_id),
                    ],
                )
                .expect("query other project row");
            let other_read_ts: Option<i64> =
                other_rows[0].get_named("read_ts").expect("other read_ts");
            let other_ack_ts: Option<i64> =
                other_rows[0].get_named("ack_ts").expect("other ack_ts");
            assert_eq!(other_read_ts, None);
            assert_eq!(other_ack_ts, None);
        });
    }

    #[test]
    fn mark_message_read_keeps_ack_required_messages_pending() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("mark_message_read_keeps_ack_pending.db");

        rt.block_on(async {
            let project = ensure_project(&cx, &pool, "/tmp/am-mark-read-keeps-ack-pending")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient");
            let recipient_id = recipient.id.expect("recipient id");

            let message = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Ack should stay pending after read",
                "Body",
                Some("mark-read-keeps-ack-pending"),
                "normal",
                true,
                "[]",
                &[(recipient_id, "to")],
            )
            .await
            .into_result()
            .expect("create message");
            let message_id = message.id.expect("message id");

            let read_ts = mark_message_read(&cx, &pool, recipient_id, message_id)
                .await
                .into_result()
                .expect("mark message read");

            let stats = get_inbox_stats(&cx, &pool, recipient_id)
                .await
                .into_result()
                .expect("get inbox stats")
                .expect("recipient stats row");
            assert_eq!(stats.unread_count, 0);
            assert_eq!(stats.ack_pending_count, 1);

            let conn = crate::open_sqlite_file_with_recovery(pool.sqlite_path())
                .expect("open sqlite connection for verification");
            let rows = conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients \
                     WHERE agent_id = ? AND message_id = ?",
                    &[Value::BigInt(recipient_id), Value::BigInt(message_id)],
                )
                .expect("query recipient row");
            assert_eq!(rows.len(), 1, "expected one recipient row");
            let row = rows.first().expect("recipient row");
            let stored_read_ts: Option<i64> = row.get_named("read_ts").expect("read_ts");
            let stored_ack_ts: Option<i64> = row.get_named("ack_ts").expect("ack_ts");
            assert_eq!(stored_read_ts, Some(read_ts));
            assert_eq!(stored_ack_ts, None);
        });
    }

    #[test]
    fn mark_message_read_tolerates_missing_inbox_stats_table() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("mark_message_read_missing_inbox_stats.db");

        rt.block_on(async {
            let project = ensure_project(&cx, &pool, "/tmp/am-mark-read-missing-inbox-stats")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let message = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "Missing inbox_stats should be tolerated",
                "Body",
                Some("mark-read-missing-inbox-stats"),
                "normal",
                true,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            let read_ts = mark_message_read(
                &cx,
                &pool,
                recipient.id.expect("recipient id"),
                message.id.expect("message id"),
            )
            .await
            .into_result()
            .expect("mark message read without inbox_stats table");

            assert!(read_ts > 0);
        });
    }

    #[test]
    fn mark_message_read_tolerates_non_missing_inbox_stats_errors() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let (cx, pool, _dir) = setup_test_pool("mark_message_read_inbox_stats_view.db");

        rt.block_on(async {
            let conn = crate::open_sqlite_file_with_recovery(pool.sqlite_path())
                .expect("open sqlite connection");
            create_inbox_stats_table_for_test(&conn);
            drop(conn);

            let project = ensure_project(&cx, &pool, "/tmp/am-mark-read-inbox-stats-view")
                .await
                .into_result()
                .expect("ensure project");
            let project_id = project.id.expect("project id");

            let sender = register_agent(
                &cx,
                &pool,
                project_id,
                "BlueLake",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register sender");
            let recipient = register_agent(
                &cx,
                &pool,
                project_id,
                "GreenStone",
                "codex-cli",
                "gpt-5",
                None,
                None,
                None,
            )
            .await
            .into_result()
            .expect("register recipient");

            let message = create_message_with_recipients(
                &cx,
                &pool,
                project_id,
                sender.id.expect("sender id"),
                "View-backed inbox_stats should not block read state",
                "Body",
                Some("mark-read-inbox-stats-view"),
                "normal",
                true,
                "[]",
                &[(recipient.id.expect("recipient id"), "to")],
            )
            .await
            .into_result()
            .expect("create message");

            let conn = crate::open_sqlite_file_with_recovery(pool.sqlite_path())
                .expect("open sqlite connection");
            conn.execute_raw("DROP TABLE inbox_stats")
                .expect("drop inbox_stats table");
            conn.execute_raw(
                "CREATE VIEW inbox_stats AS \
                 SELECT 0 AS agent_id, \
                        0 AS total_count, \
                        0 AS unread_count, \
                        0 AS ack_pending_count, \
                        NULL AS last_message_ts",
            )
            .expect("create inbox_stats view");
            drop(conn);

            // inbox_stats rebuild errors are best-effort (silently ignored),
            // so mark_message_read succeeds even when inbox_stats is a VIEW.
            match mark_message_read(
                &cx,
                &pool,
                recipient.id.expect("recipient id"),
                message.id.expect("message id"),
            )
            .await
            {
                Outcome::Ok(_ts) => { /* expected: inbox_stats errors are non-fatal */ }
                other => panic!(
                    "mark_message_read should succeed despite inbox_stats errors, got {other:?}"
                ),
            }
        });
    }

    #[test]
    fn inbox_stats_rebuild_error_filter_only_tolerates_known_compatibility_cases() {
        assert!(is_tolerable_inbox_stats_rebuild_error(&DbError::Sqlite(
            "no such table: inbox_stats".to_string()
        )));
        assert!(is_tolerable_inbox_stats_rebuild_error(&DbError::Sqlite(
            "cannot modify inbox_stats because it is a view".to_string()
        )));
        assert!(!is_tolerable_inbox_stats_rebuild_error(&DbError::Sqlite(
            "database is locked while executing DELETE FROM inbox_stats".to_string()
        )));
        assert!(!is_tolerable_inbox_stats_rebuild_error(&DbError::Sqlite(
            "disk I/O error while updating inbox_stats".to_string()
        )));
    }

    // ─── Property tests ───────────────────────────────────────────────────────

    mod proptest_queries {
        use super::*;
        use proptest::prelude::*;

        fn pt_config() -> ProptestConfig {
            ProptestConfig {
                cases: 1000,
                max_shrink_iters: 5000,
                ..ProptestConfig::default()
            }
        }

        proptest! {
            #![proptest_config(pt_config())]

            /// `placeholders(n)` produces exactly `min(n, 500)` question marks.
            #[test]
            fn prop_placeholders_count_matches(n in 0..=600usize) {
                let result = placeholders(n);
                let capped = n.min(MAX_IN_CLAUSE_ITEMS);
                if capped == 0 {
                    prop_assert!(result.is_empty());
                } else {
                    let question_marks = result.matches('?').count();
                    prop_assert_eq!(question_marks, capped);
                    // Verify comma-separated format
                    let parts: Vec<&str> = result.split(", ").collect();
                    prop_assert_eq!(parts.len(), capped);
                    for part in &parts {
                        prop_assert_eq!(*part, "?");
                    }
                }
            }

            /// `like_escape` escapes all `%`, `_`, `\` chars; never double-escapes.
            #[test]
            fn prop_like_escape_no_unescaped_wildcards(term in ".*") {
                let escaped = like_escape(&term);
                // Walk the escaped string: every `%` and `_` must be preceded by `\`
                let chars: Vec<char> = escaped.chars().collect();
                let mut i = 0;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        // Skip the escaped char
                        i += 2;
                    } else {
                        prop_assert!(chars[i] != '%' && chars[i] != '_');
                        i += 1;
                    }
                }
                // Round-trip: un-escape and compare to original.
                let unescaped = escaped
                    .replace("\\%", "%")
                    .replace("\\_", "_")
                    .replace("\\\\", "\\");
                prop_assert_eq!(unescaped, term);
            }

            /// `sanitize_fts_query` never returns SQL injection markers.
            #[test]
            fn prop_fts_sanitize_no_sqlite_injection(query in ".*") {
                if let Some(sanitized) = sanitize_fts_query(&query) {
                    prop_assert!(!sanitized.contains("; DROP"));
                    prop_assert!(!sanitized.contains("--"));
                    prop_assert!(!sanitized.is_empty());
                }
            }

            /// (a) MATCH-syntax escaping: sanitized output must never contain
            /// FTS5 operators that could alter query semantics when interpolated
            /// into a MATCH clause (even though FTS5 is currently decommissioned,
            /// this guards against future re-enablement).
            #[test]
            fn prop_fts_sanitize_no_match_operators(query in ".*") {
                if let Some(sanitized) = sanitize_fts_query(&query) {
                    prop_assert!(!sanitized.contains("/*"), "C-style comment open in sanitized output");
                    prop_assert!(!sanitized.contains("*/"), "C-style comment close in sanitized output");
                    // Column filter syntax: {col}: or col:
                    prop_assert!(!sanitized.contains('{'), "FTS5 column filter brace in sanitized output");
                    prop_assert!(!sanitized.contains('}'), "FTS5 column filter brace in sanitized output");
                    // Caret prefix operator
                    prop_assert!(!sanitized.contains('^'), "FTS5 initial-token operator in sanitized output");
                    // Square brackets (column filter alt syntax)
                    prop_assert!(!sanitized.contains('['), "bracket in sanitized output");
                    prop_assert!(!sanitized.contains(']'), "bracket in sanitized output");
                    // Tilde (NEAR proximity)
                    prop_assert!(!sanitized.contains('~'), "tilde/proximity operator in sanitized output");
                    // Backslash has no FTS5 meaning; must not appear raw
                    prop_assert!(!sanitized.contains('\\'), "backslash in sanitized output");
                }
            }

            /// Full pipeline: extract_like_terms → like_escape → wrap in %...%
            /// must produce only parameterizable LIKE patterns with no unescaped
            /// wildcards regardless of input.
            #[test]
            fn prop_like_pipeline_safe(query in "\\PC{0,500}") {
                let terms = extract_like_terms(&query, 10);
                for term in &terms {
                    prop_assert!(term.len() >= 2, "term too short: {:?}", term);
                    let upper = term.to_ascii_uppercase();
                    prop_assert!(
                        !["AND", "OR", "NOT", "NEAR"].contains(&upper.as_str()),
                        "stopword leaked: {:?}", term
                    );
                    let escaped = like_escape(term);
                    let pattern = format!("%{}%", escaped);
                    // The wrapping %...% are the only unescaped wildcards
                    let inner = &pattern[1..pattern.len() - 1];
                    let chars: Vec<char> = inner.chars().collect();
                    let mut i = 0;
                    while i < chars.len() {
                        if chars[i] == '\\' {
                            i += 2;
                        } else {
                            prop_assert!(
                                chars[i] != '%' && chars[i] != '_',
                                "unescaped LIKE wildcard in pattern: {:?}", pattern
                            );
                            i += 1;
                        }
                    }
                }
                prop_assert!(terms.len() <= 10, "exceeded max_terms");
            }
        }

        /// (b) Control-character handling: embedded NULL, CR/LF, ESC, and other
        /// C0/C1 control bytes must not cause panics or produce SQL-unsafe output.
        #[test]
        fn fts_control_character_corpus() {
            let inputs: Vec<&[u8]> = vec![
                b"hello\x00world",
                b"test\x01\x02\x03",
                b"query\x1b[31mred", // ANSI escape
                b"line\r\nbreak",
                b"tab\there",
                b"null\x00",
                b"\x00\x00\x00",
                b"mixed\x00\x0a\x0d\x1b\x7fend",
                b"valid prefix\x00hidden suffix",
                b"\xc0\xaf",            // overlong UTF-8
                b"\xfe\xff",            // invalid UTF-8 BOM
                b"ok\xe2\x80\x8b\x00z", // zero-width space + null
            ];
            for raw in &inputs {
                let query = String::from_utf8_lossy(raw);
                let _ = sanitize_fts_query(&query);
                let terms = extract_like_terms(&query, 5);
                for term in &terms {
                    let escaped = like_escape(term);
                    assert!(
                        !escaped.contains('\x00'),
                        "NULL byte survived pipeline for input {:?}",
                        raw
                    );
                }
            }
        }

        /// (c) Pathological query structures: deeply nested quotes, extreme
        /// lengths, Unicode confusables, and repeated operators.
        #[test]
        fn fts_pathological_corpus() {
            let quotes_1k = "\"".repeat(1000);
            let quoted_terms_500 = "\"a\" ".repeat(500);
            let empty_pairs_200 = "\"\"".repeat(200);
            let a_10k = "a".repeat(10_000);
            let test_2k = "test ".repeat(2000);
            let or_1k = "OR ".repeat(1000);
            let near_500 = "NEAR(".repeat(500);
            let nested_parens = format!("{}test{}", "(".repeat(500), ")".repeat(500));
            let tabs_1k = format!("a{}b", "\t".repeat(1000));
            let wildcards_100 = format!("{}x", "*".repeat(100));

            let cases: Vec<(&str, &str)> = vec![
                (&quotes_1k, "1000 quotes"),
                (&quoted_terms_500, "500 quoted terms"),
                (&empty_pairs_200, "200 empty quote pairs"),
                (&a_10k, "10K single char"),
                (&test_2k, "2K repeated terms"),
                (&or_1k, "1K operators"),
                (&near_500, "500 NEAR opens"),
                ("\u{FF21}\u{FF2E}\u{FF24}", "fullwidth AND"),
                ("\u{200B}test\u{200B}", "zero-width space wrapped"),
                ("\u{2000}\u{2001}\u{2002}\u{2003}", "various Unicode spaces"),
                ("\u{202E}injection\u{202C}", "RTL override"),
                ("\u{FEFF}bom\u{FEFF}", "BOM chars"),
                (&nested_parens, "500 nested parens"),
                (&tabs_1k, "1K tabs"),
                (&wildcards_100, "100 leading wildcards"),
                (
                    "col1:injection col2:attack {col3}:payload",
                    "FTS5 column filters",
                ),
                (
                    "NEAR(attack payload, 1) AND ^prefix",
                    "FTS5 proximity + prefix",
                ),
            ];

            for (input, label) in &cases {
                let sanitized = sanitize_fts_query(input);
                if let Some(ref s) = sanitized {
                    assert!(!s.is_empty(), "{label}: sanitized to empty string");
                    assert!(!s.contains("--"), "{label}: SQL comment marker survived");
                    assert!(!s.contains("/*"), "{label}: C-comment open survived");
                    assert!(!s.contains("*/"), "{label}: C-comment close survived");
                }

                let terms = extract_like_terms(input, 10);
                assert!(terms.len() <= 10, "{label}: exceeded max_terms");
                for term in &terms {
                    assert!(term.len() >= 2, "{label}: term too short: {term:?}");
                    let escaped = like_escape(term);
                    let unescaped = escaped
                        .replace("\\%", "%")
                        .replace("\\_", "_")
                        .replace("\\\\", "\\");
                    assert_eq!(unescaped, *term, "{label}: like_escape round-trip failed");
                }
            }
        }
    }
}
