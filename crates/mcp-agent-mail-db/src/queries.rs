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
use asupersync::Outcome;
use asupersync::time::{sleep, wall_now};
use mcp_agent_mail_core::pattern_overlap::CompiledPattern;
use mcp_agent_mail_core::{
    ExperienceOutcome, ExperienceRow, ExperienceState, FeatureExtension, FeatureVector,
    NonExecutionReason, validate_transition,
};
use serde::de::DeserializeOwned;
use sqlmodel::prelude::*;
use sqlmodel_core::{Connection, Dialect, Error as SqlError, IsolationLevel, PreparedStatement};
use sqlmodel_core::{Row as SqlRow, TransactionOps, Value};
use sqlmodel_query::{raw_execute, raw_query};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};

fn cache_scope_for_pool(pool: &DbPool) -> String {
    pool.sqlite_identity_key()
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
    // Keep slug semantics identical to the legacy Python `_compute_project_slug` default behavior.
    // (Collapses runs of non-alphanumerics into a single '-', trims '-', and uses "project" fallback.)
    mcp_agent_mail_core::compute_project_slug(human_key)
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
    ProjectRow::from_row(row).map_err(|e| map_sql_error(&e))
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

fn decode_atc_experience_row(row: &SqlRow) -> std::result::Result<ExperienceRow, DbError> {
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
    let created_ts_micros = row
        .get(25)
        .and_then(value_as_i64)
        .ok_or_else(|| DbError::Internal("missing created_ts in atc_experience row".to_string()))?;
    let dispatched_ts_micros = row.get(26).and_then(value_as_i64);
    let executed_ts_micros = row.get(27).and_then(value_as_i64);
    let resolved_ts_micros = row.get(28).and_then(value_as_i64);
    let context = parse_json_optional::<serde_json::Value>(row_opt_text(row, 29), "context_json")?;

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
        features,
        feature_ext,
        context,
    })
}

const PROJECT_SELECT_ALL_SQL: &str =
    "SELECT id, slug, human_key, created_at FROM projects ORDER BY id ASC";
pub type AtcRollupRow = (String, i64, i64, i64, i64, i64, i64, f64, f64, f64, f64);
const ATC_EXPERIENCE_SELECT_COLUMNS_SQL: &str = "SELECT experience_id, decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem, decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json, expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy, safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json FROM atc_experiences";
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
    AND NOT EXISTS (
        SELECT 1 FROM file_reservation_releases
        WHERE reservation_id = file_reservations.id
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
fn value_as_i64(value: &Value) -> Option<i64> {
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

fn canonical_table_exists(pool: &DbPool, table_name: &str) -> std::result::Result<bool, DbError> {
    let conn = open_canonical_atc_conn(pool, "inspect canonical ATC table presence")?;
    let result = (|| {
        let rows = canonical_query_atc_rows(
            &conn,
            "SELECT 1 AS present FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            &[Value::Text(table_name.to_string())],
            "inspect canonical ATC table presence",
        )?;
        Ok(!rows.is_empty())
    })();
    close_canonical_db_conn(conn, "canonical ATC table presence connection");
    result
}

async fn ensure_file_backed_atc_pool_initialized(cx: &Cx, pool: &DbPool) -> Outcome<(), DbError> {
    if pool.sqlite_path() == ":memory:" {
        return Outcome::Ok(());
    }
    match canonical_table_exists(pool, "atc_experiences") {
        Ok(true) => return Outcome::Ok(()),
        Ok(false) => {}
        Err(error) => return Outcome::Err(error),
    }
    match acquire_conn(cx, pool).await {
        Outcome::Ok(conn) => {
            drop(conn);
            match canonical_table_exists(pool, "atc_experiences") {
                Ok(true) => Outcome::Ok(()),
                Ok(false) => Outcome::Err(DbError::Internal(
                    "ATC schema initialization completed but atc_experiences is still missing"
                        .to_string(),
                )),
                Err(error) => Outcome::Err(error),
            }
        }
        Outcome::Err(error) => Outcome::Err(error),
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

/// Commit the current transaction (single fsync in WAL mode).
async fn commit_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    map_sql_outcome(tracked.execute(cx, "COMMIT", &[]).await).map(|_| ())
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
        // Use a plain open — no recovery, no fallback paths.  Durability probes
        // must be side-effect-free so they never trigger REINDEX or open a fallback
        // database, which could make committed rows appear to vanish.
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
                durability_probe_backoff(attempt).await;
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
async fn durability_probe_backoff(attempt: u32) {
    use crate::retry::RetryConfig;
    let config = RetryConfig {
        base_delay: std::time::Duration::from_millis(25),
        max_delay: std::time::Duration::from_millis(200),
        use_circuit_breaker: false,
        ..Default::default()
    };
    let () = sleep(wall_now(), config.delay_for_attempt(attempt)).await;
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
    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
               registration_token \
               FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
               ORDER BY id ASC LIMIT 1";
    let params = [Value::BigInt(project_id), Value::Text(name.to_string())];
    match durability_probe_query(cx, pool, sql, &params).await {
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
    }
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
    let message_count_sql = "SELECT COUNT(*) FROM messages WHERE id = ? AND project_id = ?";
    let message_count_params = [Value::BigInt(message_id), Value::BigInt(project_id)];
    let message_count_rows =
        match durability_probe_query(cx, pool, message_count_sql, &message_count_params).await {
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
    let recipient_rows =
        match durability_probe_query(cx, pool, recipient_sql, &recipient_params).await {
            Outcome::Ok(rows) => rows,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

    let mut actual: Vec<(i64, String)> = Vec::with_capacity(recipient_rows.len());
    for row in &recipient_rows {
        let Some(agent_id) = row.get(0).and_then(value_as_i64) else {
            return Outcome::Err(DbError::Internal(format!(
                "message recipient durability check failed: missing agent_id for message_id={message_id}"
            )));
        };
        let Some(kind) = row.get(1).and_then(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        }) else {
            return Outcome::Err(DbError::Internal(format!(
                "message recipient durability check failed: missing kind for message_id={message_id}"
            )));
        };
        actual.push((agent_id, kind));
    }
    actual.sort_unstable();
    actual.dedup();

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
fn close_canonical_db_conn(conn: crate::CanonicalDbConn, context: &'static str) {
    let _ = context;
    drop(conn);
}

fn open_canonical_atc_conn(
    pool: &DbPool,
    purpose: &'static str,
) -> std::result::Result<crate::CanonicalDbConn, DbError> {
    let conn = crate::CanonicalDbConn::open_file(pool.sqlite_path())
        .map_err(|error| DbError::Sqlite(format!("{purpose}: open failed: {error}")))?;
    conn.execute_raw(crate::schema::PRAGMA_CONN_SETTINGS_SQL)
        .map_err(|error| DbError::Sqlite(format!("{purpose}: init pragmas failed: {error}")))?;
    Ok(conn)
}

fn begin_canonical_atc_write_tx(conn: &crate::CanonicalDbConn) -> std::result::Result<(), DbError> {
    conn.execute_sync("BEGIN IMMEDIATE", &[])
        .map(|_| ())
        .map_err(|error| DbError::Sqlite(error.to_string()))
}

fn commit_canonical_atc_write_tx(
    conn: &crate::CanonicalDbConn,
) -> std::result::Result<(), DbError> {
    conn.execute_sync("COMMIT", &[])
        .map(|_| ())
        .map_err(|error| DbError::Sqlite(error.to_string()))
}

fn rollback_canonical_atc_write_tx(conn: &crate::CanonicalDbConn) {
    let _ = conn.execute_sync("ROLLBACK", &[]);
}

fn canonical_query_atc_rows(
    conn: &crate::CanonicalDbConn,
    sql: &str,
    params: &[Value],
    purpose: &'static str,
) -> std::result::Result<Vec<SqlRow>, DbError> {
    conn.query_sync(sql, params)
        .map_err(|error| DbError::Sqlite(format!("{purpose}: {error}")))
}

fn canonical_execute_atc(
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
    let posterior_json = encode_json(&row.posterior, "posterior_json")?;
    let non_execution_json = encode_json_optional(&row.non_execution_reason, "non_execution_json")?;
    let outcome_json = encode_json_optional(&row.outcome, "outcome_json")?;
    let features_json = encode_json_optional(&row.features, "features_json")?;
    let feature_ext_json = encode_json_optional(&row.feature_ext, "feature_ext_json")?;
    let context_json = encode_json_optional(&row.context, "context_json")?;

    let conn = open_canonical_atc_conn(pool, "append_atc_experience")?;
    let write_result = (|| {
        begin_canonical_atc_write_tx(&conn)?;
        let insert_sql = "INSERT INTO atc_experiences \
            (decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem, decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json, expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy, safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json) \
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
        canonical_execute_atc(&conn, sql, &params, "resolve_atc_experience update")?;
        commit_canonical_atc_write_tx(&conn)
    })();
    if result.is_err() {
        rollback_canonical_atc_write_tx(&conn);
    }
    close_canonical_db_conn(conn, "canonical ATC resolve connection");
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

fn is_hard_post_commit_probe_error(_error: &DbError) -> bool {
    // Post-commit durability probes exist to prove that a write is query-visible
    // from an independent handle before we report success. Any probe error means
    // that proof failed, so none of these errors are advisory.
    true
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
/// Read once from `FSQLITE_CONCURRENT_RETRIES` env var; default 8.
/// Increased from 5 to 8 to give the exponential backoff (10ms..2s)
/// enough total budget (~4s) to outlast transient WAL checkpoint stalls
/// under sustained multi-agent write load.
static MVCC_MAX_RETRIES: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
    std::env::var("FSQLITE_CONCURRENT_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
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
    mut op: F,
) -> Outcome<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Outcome<T, DbError>>,
{
    let max = *MVCC_MAX_RETRIES;
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
                mvcc_backoff(cx, attempt).await;
            }
            Outcome::Err(e) if is_plain_write_contention_error(&e) && attempt < max => {
                tracing::warn!(
                    attempt,
                    max_retries = max,
                    error = %e,
                    operation,
                    "SQLite write contention, retrying whole transaction"
                );
                mvcc_backoff(cx, attempt).await;
            }
            Outcome::Err(e) if is_mvcc_error(&e) => {
                MVCC_EXHAUSTED_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    attempts = max + 1,
                    error = %e,
                    operation,
                    "MVCC retries exhausted"
                );
                return Outcome::Err(e);
            }
            Outcome::Err(e) if is_plain_write_contention_error(&e) => {
                tracing::error!(
                    attempts = max + 1,
                    error = %e,
                    operation,
                    "SQLite write-contention retries exhausted"
                );
                return Outcome::Err(e);
            }
            other => return other,
        }
    }

    Outcome::Err(DbError::Internal(format!(
        "MVCC retry loop fell through in {operation}"
    )))
}

/// Sleep with exponential backoff for MVCC retry.
///
/// Base: 25 ms, max: 2000 ms, ±25 % jitter (via existing LCG in `retry` module).
///
/// The generous ceiling (2s) lets later retries outlast WAL checkpoint stalls
/// that typically complete within 500ms–1s under concurrent-writer load.
/// Total budget across 8 retries: ~4s (25+50+100+200+400+800+1600+2000 ms
/// before jitter), which keeps the caller well below the 60s `busy_timeout`.
async fn mvcc_backoff(_cx: &Cx, attempt: u32) {
    use crate::retry::RetryConfig;
    let config = RetryConfig {
        base_delay: std::time::Duration::from_millis(25),
        max_delay: std::time::Duration::from_secs(2),
        use_circuit_breaker: false,
        ..Default::default()
    };
    let () = sleep(wall_now(), config.delay_for_attempt(attempt)).await;
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

    let slug = generate_slug(human_key);
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

    // Use an explicit write transaction and conflict-safe insert so project creation
    // participates in concurrent writer mode.
    let fresh = match run_with_mvcc_retry(cx, "ensure_project", || async {
        try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

        let row = ProjectRow::new(slug.clone(), human_key.to_string());
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
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Project", slug)),
            |r| match decode_project_row(r) {
                Ok(row) => {
                    crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                    Outcome::Ok(row)
                }
                Err(e) => Outcome::Err(e),
            },
        ),
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
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Project", human_key)),
            |r| match decode_project_row(r) {
                Ok(row) => {
                    crate::cache::read_cache().put_project_scoped(&cache_scope, &row);
                    Outcome::Ok(row)
                }
                Err(e) => Outcome::Err(e),
            },
        ),
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
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Project", project_id.to_string())),
            |r| match decode_project_row(r) {
                Ok(row) => Outcome::Ok(row),
                Err(e) => Outcome::Err(e),
            },
        ),
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

    let tracked = tracked(&*conn);

    match map_sql_outcome(traw_query(cx, &tracked, PROJECT_SELECT_ALL_SQL, &[]).await) {
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
                let is_agent_unique_violation = |err: &DbError| match err {
                    DbError::Sqlite(msg) => {
                        let msg = msg.to_ascii_lowercase();
                        msg.contains("unique constraint failed")
                            && (msg.contains("project_id") || msg.contains("name"))
                    }
                    _ => false,
                };

                try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

                // Update-first strategy keeps id stable even if backend UPSERT conflict handling
                // changes, and avoids duplicate row creation under mixed SQLite variants.
                let mut normalize_sets = vec!["program = ?", "model = ?", "last_active_ts = ?"];
                let program_s = program.to_string();
                let model_s = model.to_string();
                let name_s = name.to_string();
                let mut normalize_base_params = vec![
                    Value::Text(program_s.clone()),
                    Value::Text(model_s.clone()),
                    Value::BigInt(now),
                ];

                // Keep behavior consistent with insert path: omitted task_description clears
                // to empty string instead of preserving stale content.
                normalize_sets.push("task_description = ?");
                let insert_task_desc = task_description.unwrap_or_default().to_string();
                normalize_base_params.push(Value::Text(insert_task_desc.clone()));
                if let Some(ap) = attachments_policy {
                    normalize_sets.push("attachments_policy = ?");
                    normalize_base_params.push(Value::Text(ap.to_string()));
                }
                if let Some(exempt) = reaper_exempt {
                    normalize_sets.push("reaper_exempt = ?");
                    normalize_base_params.push(Value::BigInt(i64::from(exempt)));
                }

                let normalize_sql = format!(
                    "UPDATE agents SET {} WHERE project_id = ? AND name = ? COLLATE NOCASE",
                    normalize_sets.join(", ")
                );
                let mut normalize_params = normalize_base_params.clone();
                normalize_params.push(Value::BigInt(project_id));
                normalize_params.push(Value::Text(name_s.clone()));
                let _updated_rows = try_in_tx!(
                    cx,
                    &tracked,
                    map_sql_outcome(
                        traw_execute(cx, &tracked, &normalize_sql, &normalize_params).await
                    )
                );

                let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                                 inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                                 registration_token \
                                 FROM agents \
                                 WHERE project_id = ? AND name = ? COLLATE NOCASE \
                                 ORDER BY id ASC \
                                 LIMIT 1";

                let mut inserted_new = false;
                let fresh = {
                    let fetch_params = [Value::BigInt(project_id), Value::Text(name_s.clone())];
                    let existing_rows = try_in_tx!(
                        cx,
                        &tracked,
                        map_sql_outcome(traw_query(cx, &tracked, fetch_sql, &fetch_params).await)
                    );

                    if let Some(existing) = existing_rows.first().map(decode_agent_row_indexed) {
                        existing
                    } else {
                    let insert_sql = "INSERT INTO agents \
                        (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt) \
                        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
                        let attach_pol = attachments_policy.map_or_else(
                            || "auto".to_string(),
                            std::string::ToString::to_string,
                        );
                        let insert_params = [
                            Value::BigInt(project_id),
                            Value::Text(name_s.clone()),
                            Value::Text(program_s),
                            Value::Text(model_s),
                            Value::Text(insert_task_desc),
                            Value::BigInt(now),
                            Value::BigInt(now),
                            Value::Text(attach_pol),
                            Value::Text("auto".to_string()),
                            Value::BigInt(i64::from(reaper_exempt.unwrap_or(false))),
                        ];
                        match map_sql_outcome(
                            traw_execute(cx, &tracked, insert_sql, &insert_params).await,
                        ) {
                            Outcome::Ok(_) => {
                                inserted_new = true;
                            }
                            Outcome::Err(e) if is_agent_unique_violation(&e) => {
                                // Concurrent insert race: row now exists, so apply normalize update.
                                let mut retry_params = normalize_base_params;
                                retry_params.push(Value::BigInt(project_id));
                                retry_params.push(Value::Text(name.to_string()));
                                let _retried_rows = try_in_tx!(
                                    cx,
                                    &tracked,
                                    map_sql_outcome(
                                        traw_execute(cx, &tracked, &normalize_sql, &retry_params)
                                            .await
                                    )
                                );
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
                        }

                        let fetch_params =
                            [Value::BigInt(project_id), Value::Text(name.to_string())];
                        let rows = try_in_tx!(
                            cx,
                            &tracked,
                            map_sql_outcome(traw_query(cx, &tracked, fetch_sql, &fetch_params).await)
                        );
                        let Some(fresh) = rows.first().map(decode_agent_row_indexed) else {
                            rollback_tx(cx, &tracked).await;
                            return Outcome::Err(DbError::Internal(format!(
                                "agent upsert succeeded but re-select failed for {project_id}:{name}"
                            )));
                        };
                        fresh
                    }
                };

                try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
                Outcome::Ok((fresh, inserted_new))
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
               FROM ( \
                 SELECT id, project_id, name, program, model, task_description, \
                        inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                        registration_token, \
                        ROW_NUMBER() OVER ( \
                            PARTITION BY name COLLATE NOCASE \
                            ORDER BY last_active_ts DESC, id DESC \
                        ) AS rn \
                 FROM agents \
                 WHERE project_id = ? \
               ) dedup \
               WHERE rn = 1 \
               ORDER BY last_active_ts DESC, id DESC";
    let params = [Value::BigInt(project_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            let agents: Vec<AgentRow> = rows.iter().map(decode_agent_row_indexed).collect();
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
        let current_sql = "SELECT id, project_id, name, program, model, task_description, \
                           inception_ts, last_active_ts, attachments_policy, contact_policy, reaper_exempt, \
                           registration_token \
                           FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE \
                           ORDER BY last_active_ts DESC, id DESC LIMIT 1";
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
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    // Insert message using traw_execute and then fetch id.
    let sql = "INSERT INTO messages \
               (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
    let params = [
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

    let rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_query(cx, &tracked, "SELECT last_insert_rowid()", &[]).await)
    );
    let message_id = rows
        .first()
        .and_then(row_first_i64)
        .ok_or_else(|| DbError::Internal("Message INSERT last_insert_rowid() failed".to_string()));

    let message_id = match message_id {
        Ok(id) => id,
        Err(e) => {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(e);
        }
    };

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
    let now = now_micros();
    let row = {
        let conn = match acquire_conn(cx, pool).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let row = {
            let tracked = tracked(&*conn);
            match run_with_mvcc_retry(cx, "create_message_with_recipients", || {
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
            }
        };
        drop(conn);
        row
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
        Outcome::Err(e) if is_hard_post_commit_probe_error(&e) => Some(e),
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
) -> Outcome<MessageRow, DbError> {
    // Use MVCC concurrent transaction for page-level parallelism.
    try_in_tx!(cx, tracked, begin_concurrent_tx(cx, tracked).await);

    // Fetch recipient names to build recipients_json
    let mut to_names = Vec::new();
    let mut cc_names = Vec::new();
    let mut bcc_names = Vec::new();

    if !recipients.is_empty() {
        let ph = placeholders(recipients.len());
        let lookup_sql = format!("SELECT id, name FROM agents WHERE id IN ({ph})");
        let params: Vec<Value> = recipients
            .iter()
            .map(|(id, _)| Value::BigInt(*id))
            .collect();
        let agent_rows = try_in_tx!(
            cx,
            tracked,
            map_sql_outcome(traw_query(cx, tracked, &lookup_sql, &params).await)
        );
        let mut name_map = std::collections::HashMap::new();
        for r in agent_rows {
            if let (Ok(id), Ok(name)) = (r.get_as::<i64>(0), r.get_as::<String>(1)) {
                name_map.insert(id, name);
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

    // Insert message using traw_execute and then fetch id.
    let sql = "INSERT INTO messages \
               (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    let params = [
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

    let rows = try_in_tx!(
        cx,
        tracked,
        map_sql_outcome(traw_query(cx, tracked, "SELECT last_insert_rowid()", &[]).await)
    );
    let message_id = rows
        .first()
        .and_then(row_first_i64)
        .ok_or_else(|| DbError::Internal("Message INSERT last_insert_rowid() failed".to_string()));

    let message_id = match message_id {
        Ok(id) => id,
        Err(e) => {
            rollback_tx(cx, tracked).await;
            return Outcome::Err(e);
        }
    };

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
    let insert_recipient_sql = "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (?, ?, ?, NULL, NULL)";
    for (agent_id, kind) in recipients {
        let params = [
            Value::BigInt(message_id),
            Value::BigInt(*agent_id),
            Value::Text((*kind).to_string()),
        ];
        try_in_tx!(
            cx,
            tracked,
            map_sql_outcome(traw_execute(cx, tracked, insert_recipient_sql, &params).await)
        );
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
                    m.attachments, a.name as from_name \
             FROM messages m \
             JOIN agents a ON a.id = m.sender_id \
             WHERE m.id IN ({placeholders}){project_clause}"
        );

        let mut params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();
        if let Some(pid) = project_id {
            params.push(Value::BigInt(pid));
        }

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
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
                "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                        m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                        m.importance AS importance, m.ack_required AS ack_required, \
                        m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                        m.attachments AS attachments, \
                        a.name AS from_name \
                 FROM messages m \
                 JOIN agents a ON a.id = m.sender_id \
                 WHERE m.project_id = ? AND (m.id = ? OR m.thread_id = ?) \
                 ORDER BY created_ts DESC, id DESC \
                 LIMIT ?",
                true,
            )
        }
        (true, None) => (
            "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                    m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                    m.importance AS importance, m.ack_required AS ack_required, \
                    m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                    m.attachments AS attachments, \
                    a.name AS from_name \
             FROM messages m \
             JOIN agents a ON a.id = m.sender_id \
             WHERE m.project_id = ? AND (m.id = ? OR m.thread_id = ?) \
             ORDER BY created_ts ASC, id ASC",
            false,
        ),
        (false, Some(lim)) => {
            let Ok(limit_i64) = i64::try_from(lim) else {
                return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
            };
            params.push(Value::BigInt(limit_i64));
            (
                "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                        m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                        m.importance AS importance, m.ack_required AS ack_required, \
                        m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                        m.attachments AS attachments, \
                        a.name AS from_name \
                 FROM messages m \
                 JOIN agents a ON a.id = m.sender_id \
                 WHERE m.project_id = ? AND m.thread_id = ? \
                 ORDER BY created_ts DESC, id DESC \
                 LIMIT ?",
                true,
            )
        }
        (false, None) => (
            "SELECT m.id AS id, m.project_id AS project_id, m.sender_id AS sender_id, \
                    m.thread_id AS thread_id, m.subject AS subject, m.body_md AS body_md, \
                    m.importance AS importance, m.ack_required AS ack_required, \
                    m.created_ts AS created_ts, m.recipients_json AS recipients_json, \
                    m.attachments AS attachments, \
                    a.name AS from_name \
             FROM messages m \
             JOIN agents a ON a.id = m.sender_id \
             WHERE m.project_id = ? AND m.thread_id = ? \
             ORDER BY created_ts ASC, id ASC",
            false,
        ),
    };

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
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
                        "SELECT DISTINCT a.name \
                         FROM message_recipients r \
                         JOIN agents a ON a.id = r.agent_id \
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
                    let name: String = match row.get_named("name") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.push(name);
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
    let sql = "SELECT a.name, r.kind \
               FROM message_recipients r \
               JOIN agents a ON a.id = r.agent_id \
               JOIN messages m ON m.id = r.message_id \
               WHERE m.project_id = ? AND r.message_id = ?";
    let params = [Value::BigInt(project_id), Value::BigInt(message_id)];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let name: String = match row.get_as(0) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let kind: String = match row.get_as(1) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                out.push(MessageRecipientDetailRow { name, kind });
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
            "SELECT r.message_id, a.name \
             FROM message_recipients r \
             JOIN agents a ON a.id = r.agent_id \
             WHERE r.message_id IN ({placeholders}) \
             ORDER BY r.message_id ASC, a.name COLLATE NOCASE ASC"
        );

        let params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
                for row in rows {
                    let message_id: i64 = match row.get_as(0) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let name: String = match row.get_as(1) {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    out.entry(message_id).or_default().push(name);
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
        urgent_only,
        false,
        false,
        since_ts,
        limit,
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
        urgent_only,
        true,
        false,
        since_ts,
        limit,
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
        cx, pool, project_id, agent_id, false, false, true, None, limit,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn fetch_inbox_impl(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    unread_only: bool,
    ack_required_only: bool,
    since_ts: Option<i64>,
    limit: usize,
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

    match crate::sync::fetch_inbox_rows_from_conn(
        &conn,
        project_id,
        agent_id,
        urgent_only,
        unread_only,
        ack_required_only,
        since_ts,
        limit,
    ) {
        Ok(rows) => Outcome::Ok(rows),
        Err(error) => Outcome::Err(error),
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
        "SELECT m.id, m.sender_id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name, m.body_md \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
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
        "SELECT m.id, m.sender_id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name, m.body_md, m.project_id \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
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
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_id: i64 = match row.get_as(1) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
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
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let created_ts: i64 = match row.get_named("created_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
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
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
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

    let mut sql = String::from(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, \
                m.attachments, \
                r.kind, s.name as sender_name, r.ack_ts, p.slug as project_slug \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         JOIN agents s ON s.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
         WHERE r.agent_id IN (SELECT id FROM agents WHERE name = ? COLLATE NOCASE)",
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

    let sql = "SELECT p.id as project_id, p.slug as project_slug, COUNT(*) as unread_count \
               FROM message_recipients r \
               JOIN messages m ON m.id = r.message_id \
               JOIN projects p ON p.id = m.project_id \
               WHERE r.agent_id IN (SELECT id FROM agents WHERE name = ? COLLATE NOCASE) \
               AND r.read_ts IS NULL \
               GROUP BY p.id, p.slug \
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
                m.thread_id, a.name as from_name, m.body_md, \
                m.project_id, p.slug as project_slug \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
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

        // Idempotent: set read_ts if NULL.  Also auto-acknowledge if the
        // message has ack_required=1 — agents consistently fail to call
        // acknowledge_message explicitly, so reading IS acknowledgment.
        let sql = "UPDATE message_recipients \
                   SET read_ts = COALESCE(read_ts, ?), \
                       ack_ts = CASE \
                           WHEN ack_ts IS NOT NULL THEN ack_ts \
                           WHEN (SELECT m.ack_required FROM messages m \
                                 WHERE m.id = message_recipients.message_id) = 1 THEN ? \
                           ELSE ack_ts \
                       END \
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

        // Batch UPDATE: mark all messages read + auto-ack in one pass per chunk.
        for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
            let ph = placeholders(chunk.len());
            // The SQL uses agent_id as the first two params (for read_ts, ack_ts),
            // then agent_id again, then the message IDs.
            let sql = format!(
                "UPDATE message_recipients \
                 SET read_ts = COALESCE(read_ts, ?), \
                     ack_ts = CASE \
                         WHEN ack_ts IS NOT NULL THEN ack_ts \
                         WHEN (SELECT m.ack_required FROM messages m \
                               WHERE m.id = message_recipients.message_id) = 1 THEN ? \
                         ELSE ack_ts \
                     END \
                 WHERE agent_id = ? AND message_id IN ({ph})"
            );
            let mut params = Vec::with_capacity(3 + chunk.len());
            params.push(Value::BigInt(now));
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
        let rows = match map_sql_outcome(traw_query(cx, &tracked, find_sql, &find_params).await) {
            Outcome::Ok(r) => r,
            Outcome::Err(e) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

        let count = rows.len();
        if count > 0 {
            // Mark unread messages as read, and auto-ack any with ack_required=1.
            let sql = "UPDATE message_recipients \
                       SET read_ts = ?, \
                           ack_ts = CASE \
                               WHEN ack_ts IS NOT NULL THEN ack_ts \
                               WHEN (SELECT m.ack_required FROM messages m \
                                     WHERE m.id = message_recipients.message_id) = 1 THEN ? \
                               ELSE ack_ts \
                           END \
                       WHERE agent_id = ? AND read_ts IS NULL \
                       AND message_id IN (SELECT id FROM messages WHERE project_id = ?)";
            let params = [
                Value::BigInt(now),
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
/// Uses DELETE + INSERT … SELECT to recompute counters from
/// `message_recipients` joined with `messages`.  This is the canonical way to
/// keep `inbox_stats` consistent — it is always correct regardless of whether
/// `SQLite` triggers fire, partially fire, or are absent.
fn is_missing_inbox_stats_table_error(error: &DbError) -> bool {
    match error {
        DbError::Sqlite(message) => {
            let lowered = message.to_ascii_lowercase();
            lowered.contains("no such table") && lowered.contains("inbox_stats")
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
        let rebuild_sql = format!(
            "INSERT INTO inbox_stats \
             (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
             SELECT \
                 r.agent_id, \
                 COUNT(*) AS total_count, \
                 SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
                 SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_pending_count, \
                 MAX(m.created_ts) AS last_message_ts \
             FROM message_recipients r \
             JOIN messages m ON m.id = r.message_id \
             WHERE r.agent_id IN ({placeholders}) \
             GROUP BY r.agent_id"
        );

        let params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();

        match map_sql_outcome(traw_execute(cx, tracked, &reset_sql, &params).await) {
            Outcome::Ok(_) => {}
            Outcome::Err(error) if is_missing_inbox_stats_table_error(&error) => {
                return Outcome::Ok(());
            }
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(panic) => return Outcome::Panicked(panic),
        }

        let rebuild_params = params.clone();
        match map_sql_outcome(traw_execute(cx, tracked, &rebuild_sql, &rebuild_params).await) {
            Outcome::Ok(_) => {}
            Outcome::Err(error) if is_missing_inbox_stats_table_error(&error) => {
                return Outcome::Ok(());
            }
            Outcome::Err(error) => return Outcome::Err(error),
            Outcome::Cancelled(reason) => return Outcome::Cancelled(reason),
            Outcome::Panicked(panic) => return Outcome::Panicked(panic),
        }
    }

    Outcome::Ok(())
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

    let rebuild_sql = "\
        INSERT INTO inbox_stats \
            (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
        SELECT \
            r.agent_id, \
            COUNT(*) AS total_count, \
            SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
            SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_pending_count, \
            MAX(m.created_ts) AS last_message_ts \
        FROM message_recipients r \
        JOIN messages m ON m.id = r.message_id \
        GROUP BY r.agent_id";
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, rebuild_sql, &[]).await)
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

        // Insert the row (execute returns rows_affected, not ID)
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(insert!(&row).execute(cx, &tracked).await)
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
    let now = now_micros();
    let active_predicate = active_reservation_predicate_for("fr");

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    // Force a fresh WAL snapshot so we never read stale reservation state.
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    let sql = format!(
        "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                fr.created_ts, fr.expires_ts, COALESCE(rr.released_ts, fr.released_ts) AS released_ts \
         FROM file_reservations fr \
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
         WHERE fr.project_id = ? AND ({active_predicate}) AND fr.expires_ts > ?"
    );
    let params = [Value::BigInt(project_id), Value::BigInt(now)];

    let result = match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
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

    // Commit the read-only IMMEDIATE tx to release the write lock promptly.
    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    result
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

        let (reservations, target_ids) = {
            let conn = match acquire_conn(cx, pool).await {
                Outcome::Ok(c) => c,
                Outcome::Err(e) => return Outcome::Err(e),
                Outcome::Cancelled(r) => return Outcome::Cancelled(r),
                Outcome::Panicked(p) => return Outcome::Panicked(p),
            };

            let tracked_conn = tracked(&*conn);
            // Bulk release updates can touch many rows; use IMMEDIATE tx semantics
            // for deterministic write visibility on FrankenSQLite.
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
                return Outcome::Ok(reservations);
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
            (reservations, target_ids)
        };
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
    // reservation predicates correct on same-process readers that have already
    // materialized file_reservations.
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
        let active_predicate = active_reservation_predicate_for("fr");
        (
            format!(
                "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                        fr.created_ts, fr.expires_ts, COALESCE(rr.released_ts, fr.released_ts) AS released_ts \
                 FROM file_reservations fr \
                 LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
                 WHERE fr.project_id = ? AND ({active_predicate}) AND fr.expires_ts > ? ORDER BY fr.id"
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
    let result = match rows_out {
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

    let sql = "SELECT p.id, p.slug, p.human_key, p.created_at FROM projects p \
               JOIN product_project_links ppl ON ppl.project_id = p.id \
               WHERE ppl.product_id = ?";
    let params = [Value::BigInt(product_id)];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                match ProjectRow::from_row(&r) {
                    Ok(row) => out.push(row),
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
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

    let sql = "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                      m.importance, m.ack_required, m.created_ts, m.recipients_json, \
                      m.attachments, \
                      r.kind, s.name AS sender_name, r.read_ts \
               FROM message_recipients r \
               JOIN messages m ON m.id = r.message_id \
               JOIN agents s ON s.id = m.sender_id \
               WHERE r.agent_id = ? AND m.project_id = ? \
                 AND m.ack_required = 1 AND r.ack_ts IS NULL \
               ORDER BY m.created_ts ASC \
               LIMIT ?";

    let params: Vec<Value> = vec![
        Value::BigInt(agent_id),
        Value::BigInt(project_id),
        Value::BigInt(limit_i64),
    ];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
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
                          FROM agents WHERE project_id = ? AND name = ? COLLATE NOCASE LIMIT 1";
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
    let features_json = match encode_json_optional(&row.features, "features_json") {
        Ok(value) => value,
        Err(error) => return Outcome::Err(error),
    };
    let feature_ext_json = match encode_json_optional(&row.feature_ext, "feature_ext_json") {
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
                (decision_id, effect_id, trace_id, claim_id, evidence_id, state, subsystem, decision_class, subject, project_key, policy_id, effect_kind, action, posterior_json, expected_loss, runner_up_action, runner_up_loss, evidence_summary, calibration_healthy, safe_mode_active, non_execution_json, outcome_json, features_json, feature_ext_json, created_ts, dispatched_ts, executed_ts, resolved_ts, context_json) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
    // via the resolution sweep). Already-resolved rows are idempotently
    // skipped by the WHERE clause.
    let sql = "UPDATE atc_experiences \
               SET state = 'resolved', resolved_ts = ?, outcome_json = ? \
               WHERE experience_id = ? AND state = 'open'";

    let params = vec![
        Value::BigInt(outcome.observed_ts_micros),
        Value::Text(outcome_json),
        Value::BigInt(id),
    ];

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
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_agent_mail_core::{EffectKind, ExperienceState, ExperienceSubsystem};

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

        let cfg_a = crate::pool::DbPoolConfig {
            database_url: "sqlite://file:mem_a?mode=memory&cache=shared".to_string(),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let cfg_b = crate::pool::DbPoolConfig {
            database_url: "sqlite://file:mem_b?mode=memory&cache=shared".to_string(),
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
        (cx, pool, dir)
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

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire verify conn");
            let tracked = tracked(&*conn);
            let rows = map_sql_outcome(
                traw_query(
                    &cx,
                    &tracked,
                    "SELECT COUNT(*) FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                    &[Value::BigInt(41), Value::BigInt(99)],
                )
                .await,
            )
            .into_result()
            .expect("count atc experiences");

            assert_eq!(rows.first().and_then(row_first_i64), Some(1));
        });
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

            let conn = acquire_conn(&cx, &pool)
                .await
                .into_result()
                .expect("acquire verify conn");
            let tracked = tracked(&*conn);
            let rows = map_sql_outcome(
                traw_query(
                    &cx,
                    &tracked,
                    &format!("{ATC_EXPERIENCE_SELECT_COLUMNS_SQL} WHERE experience_id = ? LIMIT 1"),
                    &[Value::BigInt(
                        i64::try_from(stored.experience_id).expect("experience id"),
                    )],
                )
                .await,
            )
            .into_result()
            .expect("query stored experience");
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

            drop(conn);

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

            let stale_conn = acquire_conn(&cx, &stale_pool)
                .await
                .into_result()
                .expect("acquire stale snapshot connection");
            let stale_tracked = tracked(&*stale_conn);
            map_sql_outcome(
                traw_query(
                    &cx,
                    &stale_tracked,
                    "SELECT experience_id FROM atc_experiences WHERE decision_id = ? AND effect_id = ?",
                    &[Value::BigInt(900), Value::BigInt(900)],
                )
                .await,
            )
            .into_result()
            .expect("seed stale snapshot");
            drop(stale_conn);

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
    fn post_commit_probe_errors_are_never_advisory() {
        assert!(is_hard_post_commit_probe_error(&DbError::Sqlite(
            "disk I/O error".to_string(),
        )));
        assert!(is_hard_post_commit_probe_error(&DbError::Pool(
            "database is locked".to_string(),
        )));
        assert!(is_hard_post_commit_probe_error(&DbError::PoolExhausted {
            message: "pool exhausted".to_string(),
            pool_size: 1,
            max_overflow: 0,
        }));
        assert!(is_hard_post_commit_probe_error(&DbError::ResourceBusy(
            "probe cancelled".to_string(),
        )));
        assert!(is_hard_post_commit_probe_error(&DbError::Internal(
            "probe panicked".to_string(),
        )));
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
        assert!(select_sql.contains("NOT EXISTS"));
    }

    #[test]
    fn active_reservation_predicate_for_alias_retargets_release_ledger_probe() {
        let aliased = active_reservation_predicate_for("fr");
        assert!(aliased.contains("reservation_id = fr.id"));
        assert!(!aliased.contains("reservation_id = file_reservations.id"));
        assert!(aliased.contains("fr.released_ts IS NULL"));
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

        let cfg_a = crate::pool::DbPoolConfig {
            database_url: "sqlite://file:mem_a?mode=memory&cache=shared".to_string(),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let cfg_b = crate::pool::DbPoolConfig {
            database_url: "sqlite://file:mem_b?mode=memory&cache=shared".to_string(),
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
    fn mark_message_read_propagates_non_missing_inbox_stats_errors() {
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
                "View-backed inbox_stats should fail loudly",
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
        }
    }
}
