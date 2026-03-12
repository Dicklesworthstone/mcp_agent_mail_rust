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
use sqlmodel::prelude::*;
use sqlmodel_core::{Connection, Dialect, Error as SqlError, IsolationLevel, PreparedStatement};
use sqlmodel_core::{Row as SqlRow, TransactionOps, Value};
use sqlmodel_query::{raw_execute, raw_query};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

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

const PROJECT_SELECT_ALL_SQL: &str =
    "SELECT id, slug, human_key, created_at FROM projects ORDER BY id ASC";
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
/// `inception_ts`, `last_active_ts`, `attachments_policy`, `contact_policy`.
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
           AND status = 'approved' AND b_agent_id IN ({placeholders})"
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

fn tracked(conn: &crate::DbConn) -> TrackedConnection<'_> {
    TrackedConnection::new(conn)
}

// =============================================================================
// Transaction helpers
// =============================================================================

/// Whether `BEGIN CONCURRENT` is enabled (MVCC page-level writes).
///
/// Read once from `FSQLITE_CONCURRENT_MODE` env var; defaults to `true`.
/// When `false`, all transactions use `BEGIN IMMEDIATE` (single-writer).
static CONCURRENT_MODE_ENABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    std::env::var("FSQLITE_CONCURRENT_MODE")
        .ok()
        .is_none_or(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
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

/// Execute a durability probe query from a fresh connection when file-backed.
///
/// This avoids false positives where the writer connection can still observe
/// transient state that is not yet durable/visible from independent handles.
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

    let probe_conn = match crate::pool::open_sqlite_file_with_recovery(pool.sqlite_path()) {
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
    drop(probe_conn);
    out
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
               inception_ts, last_active_ts, attachments_policy, contact_policy \
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

    let mut affected_agent_ids =
        Vec::with_capacity(recipient_agent_ids.len() + recipient_rows.len());
    for agent_id in recipient_agent_ids {
        if !affected_agent_ids.contains(agent_id) {
            affected_agent_ids.push(*agent_id);
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
        if !affected_agent_ids.contains(&agent_id) {
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

    let reset_stats_sql = "DELETE FROM inbox_stats WHERE agent_id = ?";
    let rebuild_stats_sql = "INSERT INTO inbox_stats \
         (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
         SELECT \
             r.agent_id, \
             COUNT(*) AS total_count, \
             SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
             SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_pending_count, \
             MAX(m.created_ts) AS last_message_ts \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         WHERE r.agent_id = ? \
         GROUP BY r.agent_id";
    for agent_id in &affected_agent_ids {
        let stats_params = [Value::BigInt(*agent_id)];
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, reset_stats_sql, &stats_params).await)
        );
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, rebuild_stats_sql, &stats_params).await)
        );
    }

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
/// page-level collisions). Read once from `FSQLITE_CONCURRENT_RETRIES`
/// env var; default 5.
static MVCC_MAX_RETRIES: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
    std::env::var("FSQLITE_CONCURRENT_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
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

/// Sleep with exponential backoff for MVCC retry.
///
/// Base: 10 ms, max: 200 ms, ±25 % jitter (via existing LCG in `retry` module).
async fn mvcc_backoff(_cx: &Cx, attempt: u32) {
    use crate::retry::RetryConfig;
    let config = RetryConfig {
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(200),
        use_circuit_breaker: false,
        ..Default::default()
    };
    let _ = sleep(wall_now(), config.delay_for_attempt(attempt)).await;
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

            // FrankenSQLite's `changes()` always returns 0, so we cannot rely
            // on the UPDATE return value to decide whether the row exists.
            // Use an explicit SELECT existence check instead.
            let exists_sql = "SELECT 1 FROM agents \
                              WHERE project_id = ? AND name = ? COLLATE NOCASE \
                              LIMIT 1";
            let exists_params = [Value::BigInt(project_id), Value::Text(name_s.clone())];
            let exists_rows = try_in_tx!(
                cx,
                &tracked,
                map_sql_outcome(traw_query(cx, &tracked, exists_sql, &exists_params).await)
            );

            let mut inserted_new = false;
            if exists_rows.is_empty() {
                let insert_sql = "INSERT INTO agents \
                    (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
                let attach_pol =
                    attachments_policy.map_or_else(|| "auto".to_string(), |ap| ap.to_string());
                let insert_params = [
                    Value::BigInt(project_id),
                    Value::Text(name_s),
                    Value::Text(program_s),
                    Value::Text(model_s),
                    Value::Text(insert_task_desc),
                    Value::BigInt(now),
                    Value::BigInt(now),
                    Value::Text(attach_pol),
                    Value::Text("auto".to_string()),
                ];
                match map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
                {
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
                                traw_execute(cx, &tracked, &normalize_sql, &retry_params).await
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
            }

            let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                             inception_ts, last_active_ts, attachments_policy, contact_policy \
                             FROM agents \
                             WHERE project_id = ? AND name = ? COLLATE NOCASE \
                             ORDER BY id ASC \
                             LIMIT 1";
            let fetch_params = [Value::BigInt(project_id), Value::Text(name.to_string())];
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

            try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
            (fresh, inserted_new)
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
                             inception_ts, last_active_ts, attachments_policy, contact_policy \
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
                (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
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
               inception_ts, last_active_ts, attachments_policy, contact_policy \
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
               inception_ts, last_active_ts, attachments_policy, contact_policy \
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
               inception_ts, last_active_ts, attachments_policy, contact_policy \
               FROM ( \
                 SELECT id, project_id, name, program, model, task_description, \
                        inception_ts, last_active_ts, attachments_policy, contact_policy, \
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
             inception_ts, last_active_ts, attachments_policy, contact_policy \
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

    // Batch all updates in a single transaction.
    match begin_concurrent_tx(cx, &tracked).await {
        Outcome::Ok(()) => {}
        other => {
            re_enqueue_touches(&cache_scope, &pending);
            return match other {
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
                Outcome::Ok(()) => unreachable!(),
            };
        }
    }

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
                re_enqueue_touches(&cache_scope, &pending);
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                re_enqueue_touches(&cache_scope, &pending);
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                re_enqueue_touches(&cache_scope, &pending);
                return Outcome::Panicked(p);
            }
        }
    }

    match map_sql_outcome(traw_execute(cx, &tracked, "COMMIT", &[]).await) {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(e) => {
            let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
            re_enqueue_touches(&cache_scope, &pending);
            Outcome::Err(e)
        }
        Outcome::Cancelled(r) => {
            let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
            re_enqueue_touches(&cache_scope, &pending);
            Outcome::Cancelled(r)
        }
        Outcome::Panicked(p) => {
            let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
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
                     inception_ts, last_active_ts, attachments_policy, contact_policy \
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
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);
    let now = now_micros();

    // Resolve row first so we can preserve attachments_policy explicitly.
    let current_sql = "SELECT id, project_id, name, program, model, task_description, \
                       inception_ts, last_active_ts, attachments_policy, contact_policy \
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
                     inception_ts, last_active_ts, attachments_policy, contact_policy \
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

        // Use RETURNING to map inserted IDs deterministically to this chunk without race windows.
        query.push_str(" RETURNING id");
        let id_rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, &query, &params).await)
        );

        let start_idx = created_rows.len() - chunk.len();
        if id_rows.len() != chunk.len() {
            rollback_tx(cx, &tracked).await;
            return Outcome::Err(DbError::Internal(format!(
                "file reservation insert returned {} ids for {} rows",
                id_rows.len(),
                chunk.len()
            )));
        }

        for (j, row) in id_rows.iter().enumerate() {
            let Some(id) = row_first_i64(row) else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(
                    "file reservation insert RETURNING id yielded non-integer id".to_string(),
                ));
            };
            let Some(cr) = created_rows.get_mut(start_idx + j) else {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::Internal(
                    "file reservation insert ID mapping overflowed result buffer".to_string(),
                ));
            };
            cr.id = Some(id);
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
            let max = *MVCC_MAX_RETRIES;
            let mut committed: Option<MessageRow> = None;

            for attempt in 0..=max {
                match create_message_with_recipients_tx(
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
                .await
                {
                    Outcome::Err(e) if is_mvcc_error(&e) && attempt < max => {
                        MVCC_RETRIES_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(
                            attempt,
                            max_retries = max,
                            error = %e,
                            "MVCC write conflict in create_message, retrying"
                        );
                        mvcc_backoff(cx, attempt).await;
                    }
                    Outcome::Err(e) if is_mvcc_error(&e) => {
                        MVCC_EXHAUSTED_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(
                            attempts = max + 1,
                            error = %e,
                            "MVCC retries exhausted in create_message"
                        );
                        return Outcome::Err(e);
                    }
                    Outcome::Ok(created) => {
                        committed = Some(created);
                        break;
                    }
                    other => return other,
                }
            }

            match committed {
                Some(created) => {
                    let Some(_message_id) = created.id else {
                        return Outcome::Err(DbError::Internal(
                            "message commit succeeded but returned row has no id".to_string(),
                        ));
                    };
                    created
                }
                None => {
                    return Outcome::Err(DbError::Internal(
                        "MVCC retry loop fell through without committed row".into(),
                    ));
                }
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
#[allow(clippy::too_many_arguments)]
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
        let id_list = recipients
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let lookup_sql = format!("SELECT id, name FROM agents WHERE id IN ({id_list})");
        let agent_rows = try_in_tx!(
            cx,
            tracked,
            map_sql_outcome(traw_query(cx, tracked, &lookup_sql, &[]).await)
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
                    "to" => to_names.push(name.clone()),
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

    // COMMIT (single fsync)
    try_in_tx!(cx, tracked, commit_tx(cx, tracked).await);

    Outcome::Ok(row)
}

/// Fetch detailed message information for a batch of message IDs.
///
/// Used for hydrating search results (e.g. from vector search) where
/// the index does not store full content.
pub async fn get_messages_details_by_ids(
    cx: &Cx,
    pool: &DbPool,
    message_ids: &[i64],
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
        let sql = format!(
            "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                    m.importance, m.ack_required, m.created_ts, m.recipients_json, \
                    m.attachments, a.name as from_name \
             FROM messages m \
             JOIN agents a ON a.id = m.sender_id \
             WHERE m.id IN ({placeholders})"
        );

        let params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();

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
                let kind_rank = |kind: &str| match kind.to_ascii_lowercase().as_str() {
                    "to" => 0,
                    "cc" => 1,
                    "bcc" => 2,
                    _ => 3,
                };
                kind_rank(&left.kind)
                    .cmp(&kind_rank(&right.kind))
                    .then_with(|| {
                        left.name
                            .to_ascii_lowercase()
                            .cmp(&right.name.to_ascii_lowercase())
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
        names.sort_by_key(|name| name.to_ascii_lowercase());
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
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut sql = String::from(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, r.kind, s.name as sender_name, r.read_ts, r.ack_ts \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         JOIN agents s ON s.id = m.sender_id \
         WHERE r.agent_id = ? AND m.project_id = ?",
    );

    let mut params: Vec<Value> = vec![Value::BigInt(agent_id), Value::BigInt(project_id)];

    if urgent_only {
        sql.push_str(" AND m.importance IN ('high', 'urgent')");
    }
    if unread_only {
        sql.push_str(" AND r.read_ts IS NULL");
    }
    if ack_required_only {
        sql.push_str(" AND m.ack_required = 1 AND r.ack_ts IS NULL");
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
                let ack_ts: Option<i64> = match row.get_as(14) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };

                out.push(InboxRow {
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
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
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
        if STOPWORDS.contains(&token.to_ascii_uppercase().as_str()) {
            continue;
        }
        let lower = token.to_lowercase();
        if !terms.iter().any(|t| t.to_lowercase() == lower) {
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
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    // Idempotent: only set read_ts if currently NULL.
    let sql = "UPDATE message_recipients SET read_ts = COALESCE(read_ts, ?) WHERE agent_id = ? AND message_id = ?";
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

    // Invalidate cached inbox stats (unread_count may have changed).
    crate::cache::read_cache().invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

    // Read back the actual stored timestamp (may differ from `now` on
    // idempotent calls where COALESCE preserved the original value).
    //
    // We intentionally do not trust `rows_affected` from the UPDATE above:
    // under some backend/runtime combinations, updates that clearly match
    // a row can report 0. Existence is determined by this read-back query.
    let read_sql = "SELECT read_ts FROM message_recipients WHERE agent_id = ? AND message_id = ?";
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
    }

    crate::cache::read_cache().invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);

    let count_i64 = count as i64;
    Outcome::Ok(count_i64)
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

    // Invalidate cached inbox stats (ack_pending_count may have changed).
    crate::cache::read_cache().invalidate_inbox_stats_scoped(&cache_scope_for_pool(pool), agent_id);

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

// =============================================================================
// FileReservation Queries
// =============================================================================

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

    let mut active_patterns = Vec::with_capacity(active_rows.len());
    for row in active_rows {
        if let Ok(pat) = row.get_named::<String>("path_pattern") {
            active_patterns.push(CompiledPattern::new(&pat));
        }
    }

    for path in paths {
        let req_pat = CompiledPattern::new(path);
        for active_pat in &active_patterns {
            if req_pat.overlaps(active_pat) {
                rollback_tx(cx, &tracked).await;
                return Outcome::Err(DbError::ResourceBusy(format!(
                    "Reservation conflict: '{}' overlaps with active exclusive reservation '{}'",
                    path,
                    active_pat.normalized()
                )));
            }
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

/// Get active file reservations for a project
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

    let sql = format!(
        "SELECT fr.id, fr.project_id, fr.agent_id, fr.path_pattern, fr.\"exclusive\", fr.reason, \
                fr.created_ts, fr.expires_ts, COALESCE(rr.released_ts, fr.released_ts) AS released_ts \
         FROM file_reservations fr \
         LEFT JOIN file_reservation_releases rr ON rr.reservation_id = fr.id \
         WHERE fr.project_id = ? AND ({active_predicate}) AND fr.expires_ts > ?"
    );
    let params = [Value::BigInt(project_id), Value::BigInt(now)];

    match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                match decode_file_reservation_row(&row) {
                    Ok(decoded) => out.push(decoded),
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

        let mut filter_sql =
            format!(" WHERE project_id = ? AND agent_id = ? AND ({ACTIVE_RESERVATION_PREDICATE})");
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

async fn release_reservations_by_ids_matching_expiry(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
    expires_at_or_before: Option<i64>,
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
    let mut probe_sql = format!(
        "SELECT 1 FROM file_reservations WHERE id = ? AND ({ACTIVE_RESERVATION_PREDICATE})"
    );
    if expires_at_or_before.is_some() {
        probe_sql.push_str(" AND expires_ts <= ?");
    }
    probe_sql.push_str(" LIMIT 1");
    let release_sql = "INSERT OR REPLACE INTO file_reservation_releases (reservation_id, released_ts) VALUES (?, ?)";

    for id in ids {
        let mut probe_params = Vec::with_capacity(2);
        probe_params.push(Value::BigInt(*id));
        if let Some(expiry_cutoff) = expires_at_or_before {
            probe_params.push(Value::BigInt(expiry_cutoff));
        }
        let rows = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_query(cx, &tracked, &probe_sql, &probe_params).await)
        );
        if rows.is_empty() {
            continue;
        }

        let released_ts = release_marker;
        let release_params = [Value::BigInt(*id), Value::BigInt(released_ts)];
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, release_sql, &release_params).await)
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
}

/// List file reservations for a project
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
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let proj_id: i64 = match row.get_named("project_id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let agent_id: i64 = match row.get_named("agent_id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let path_pattern: String = match row.get_named("path_pattern") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let exclusive: i64 = match row.get_named("exclusive") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let reason: String = match row.get_named("reason") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let created_ts: i64 = match row.get_named("created_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let expires_ts: i64 = match row.get_named("expires_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let released_ts: Option<i64> = match row.get_named("released_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
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
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let proj_id: i64 = match row.get_named("project_id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let agent_id: i64 = match row.get_named("agent_id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let path_pattern: String = match row.get_named("path_pattern") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let exclusive: i64 = match row.get_named("exclusive") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let reason: String = match row.get_named("reason") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let created_ts: i64 = match row.get_named("created_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let expires_ts: i64 = match row.get_named("expires_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let released_ts: Option<i64> = match row.get_named("released_ts") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
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
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
                    WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? AND b_agent_id = ?";
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
                    map_sql_outcome(traw_execute(cx, &tracked, refresh_sql, &refresh_params).await)
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
        let mut params: Vec<Value> = Vec::with_capacity(chunk.len() + 3);
        params.push(Value::BigInt(project_id));
        params.push(Value::BigInt(sender_id));
        params.push(Value::BigInt(project_id));
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
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

        let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
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

/// List projects linked to a product.
/// Force-release a single file reservation by ID regardless of owner.
///
/// If `expected_expires_ts` is provided, the release is only performed if the
/// current `expires_ts` matches exactly (prevents concurrent renewal races).
///
/// Returns the number of rows affected (0 if already released, not found, or mismatch).
pub async fn force_release_reservation(
    cx: &Cx,
    pool: &DbPool,
    reservation_id: i64,
    expected_expires_ts: Option<i64>,
) -> Outcome<usize, DbError> {
    match release_reservations_by_ids_matching_expiry(
        cx,
        pool,
        &[reservation_id],
        expected_expires_ts,
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
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    let insert_sql = "INSERT INTO agents \
        (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
    ];
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, insert_sql, &insert_params).await)
    );

    let select_sql = "SELECT id, project_id, name, program, model, task_description, \
                      inception_ts, last_active_ts, attachments_policy, contact_policy \
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
    crate::cache::read_cache().put_agent_scoped(&cache_scope_for_pool(pool), &found);
    Outcome::Ok(found)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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

        let cfg = crate::pool::DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool_a = crate::create_pool(&cfg).expect("create pool a");
        let pool_b = crate::create_pool(&cfg).expect("create pool b");
        let human_key = "/tmp/scoped-project-cache";

        rt.block_on(async {
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

    fn setup_test_pool(db_name: &str) -> (Cx, DbPool, tempfile::TempDir) {
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join(db_name);

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
        (cx, pool, dir)
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
                rows[0].project_id, project_a_id,
                "project A should rank first"
            );
            assert_eq!(
                rows[1].project_id, project_b_id,
                "project B should rank second"
            );
            assert_eq!(rows[0].subject, "alpha project-a signal");
            assert_eq!(rows[1].subject, "alpha project-b signal");
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
    fn deferred_touch_flush_is_scoped_to_memory_pool() {
        use asupersync::runtime::RuntimeBuilder;

        let rt = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let cx = asupersync::Cx::for_testing();

        let cfg = crate::pool::DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            min_connections: 1,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        };
        let pool_a = crate::create_pool(&cfg).expect("create pool a");
        let pool_b = crate::create_pool(&cfg).expect("create pool b");

        rt.block_on(async {
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
            )
            .await
            .into_result()
            .expect("register agent b");

            let agent_a_id = agent_a.id.expect("agent a id");
            let agent_b_id = agent_b.id.expect("agent b id");
            assert_eq!(
                agent_a_id, agent_b_id,
                "fresh in-memory pools should allocate matching first agent ids"
            );

            set_agent_last_active_for_test(&cx, &pool_a, agent_a_id, 0).await;
            set_agent_last_active_for_test(&cx, &pool_b, agent_b_id, 0).await;

            touch_agent(&cx, &pool_a, agent_a_id)
                .await
                .into_result()
                .expect("queue deferred touch in pool a");

            flush_deferred_touches(&cx, &pool_b)
                .await
                .into_result()
                .expect("flush deferred touches for pool b");
            assert_eq!(
                read_agent_last_active_for_test(&cx, &pool_b, agent_b_id).await,
                0,
                "pool b flush must not consume deferred touches from pool a"
            );

            flush_deferred_touches(&cx, &pool_a)
                .await
                .into_result()
                .expect("flush deferred touches for pool a");
            assert!(
                read_agent_last_active_for_test(&cx, &pool_a, agent_a_id).await > 0,
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
            )
            .await
            .into_result()
            .expect("register recipient A");
            let sender_b = register_agent(
                &cx,
                &pool,
                project_b.id.unwrap_or(0),
                "RedField",
                "codex-cli",
                "gpt-5",
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
