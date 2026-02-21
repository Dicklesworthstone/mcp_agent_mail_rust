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
use mcp_agent_mail_core::pattern_overlap::CompiledPattern;
use sqlmodel::prelude::*;
use sqlmodel_core::{Connection, Dialect, Error as SqlError, IsolationLevel, PreparedStatement};
use sqlmodel_core::{Row as SqlRow, TransactionOps, Value};
use sqlmodel_query::{raw_execute, raw_query};
use std::path::Path;
use std::sync::OnceLock;

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
    DbError::Sqlite(e.to_string())
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
pub const ACTIVE_RESERVATION_PREDICATE: &str = "released_ts IS NULL \
    OR (typeof(released_ts) IN ('integer', 'real') AND released_ts <= 0) \
    OR (typeof(released_ts) = 'text' AND lower(trim(released_ts)) IN ('', '0', 'null', 'none')) \
    OR (typeof(released_ts) = 'text' \
      AND length(trim(released_ts)) > 0 \
      AND trim(released_ts) GLOB '*[0-9]*' \
      AND REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(\
            trim(released_ts),\
            '0',''),'1',''),'2',''),'3',''),'4',''),'5',''),'6',''),'7',''),'8',''),'9',''),'.',''),'+',''),'-','') = '' \
      AND CAST(trim(released_ts) AS REAL) <= 0)";

/// Decode `ProductRow` from raw SQL query result using positional (indexed) column access.
/// Expected column order: `id`, `product_uid`, `name`, `created_at`.
fn decode_product_row_indexed(row: &SqlRow) -> std::result::Result<ProductRow, DbError> {
    let id = row.get(0).and_then(|v| match v {
        Value::BigInt(n) => Some(*n),
        Value::Int(n) => Some(i64::from(*n)),
        _ => None,
    });
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
    let created_at = row
        .get(3)
        .and_then(|v| match v {
            Value::BigInt(n) => Some(*n),
            Value::Int(n) => Some(i64::from(*n)),
            _ => None,
        })
        .unwrap_or(0);

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
        row.get(idx)
            .and_then(|v| match v {
                Value::BigInt(n) => Some(*n),
                Value::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(0)
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
        row.get(idx).and_then(|v| match v {
            Value::BigInt(n) => Some(*n),
            Value::Int(n) => Some(i64::from(*n)),
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
    }
}

fn value_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::BigInt(n) => Some(*n),
        Value::Int(n) => Some(i64::from(*n)),
        Value::SmallInt(n) => Some(i64::from(*n)),
        Value::TinyInt(n) => Some(i64::from(*n)),
        _ => None,
    }
}

pub(crate) fn row_first_i64(row: &SqlRow) -> Option<i64> {
    row.get(0).and_then(value_as_i64)
}

/// `SQLite` default `SQLITE_MAX_VARIABLE_NUMBER` is 999 (32766 in newer builds).
/// We cap IN-clause item counts well below that to prevent excessively large
/// SQL strings and parameter arrays from untrusted input.
const MAX_IN_CLAUSE_ITEMS: usize = 500;

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

fn approved_contact_sql(item_count: usize) -> String {
    let capped = item_count.min(MAX_IN_CLAUSE_ITEMS);
    if capped == 0 {
        return build_approved_contact_sql_with_placeholders("");
    }

    let cache = APPROVED_CONTACT_SQL_CACHE.get_or_init(|| {
        (1..=MAX_IN_CLAUSE_ITEMS)
            .map(|count| build_approved_contact_sql_with_placeholders(&placeholders(count)))
            .collect::<Vec<_>>()
    });
    cache[capped - 1].clone()
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

fn recent_contact_union_sql(item_count: usize) -> String {
    let capped = item_count.min(MAX_IN_CLAUSE_ITEMS);
    if capped == 0 {
        return build_recent_contact_union_sql_with_placeholders("");
    }

    let cache = RECENT_CONTACT_SQL_CACHE.get_or_init(|| {
        (1..=MAX_IN_CLAUSE_ITEMS)
            .map(|count| build_recent_contact_union_sql_with_placeholders(&placeholders(count)))
            .collect::<Vec<_>>()
    });
    cache[capped - 1].clone()
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

/// Begin a concurrent write transaction (MVCC page-level concurrent writes).
///
/// Falls back to `BEGIN IMMEDIATE` on backends that do not support
/// `BEGIN CONCURRENT`, or when `FSQLITE_CONCURRENT_MODE=false`.
async fn begin_concurrent_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    if !*CONCURRENT_MODE_ENABLED {
        return begin_immediate_tx(cx, tracked).await;
    }
    match map_sql_outcome(tracked.execute(cx, "BEGIN CONCURRENT", &[]).await).map(|_| ()) {
        Outcome::Err(DbError::Sqlite(msg))
            if msg.to_ascii_lowercase().contains("near \"concurrent\"") =>
        {
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
fn is_mvcc_error(e: &DbError) -> bool {
    matches!(e, DbError::Sqlite(msg) if crate::error::is_mvcc_conflict(msg))
}

/// Sleep with exponential backoff for MVCC retry.
///
/// Base: 10 ms, max: 200 ms, ±25 % jitter (via existing LCG in `retry` module).
fn mvcc_backoff(attempt: u32) {
    use crate::retry::RetryConfig;
    let config = RetryConfig {
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(200),
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

    let slug = generate_slug(human_key);

    // Fast path: check cache first
    if let Some(cached) = crate::cache::read_cache().get_project(&slug) {
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
                        crate::cache::read_cache().put_project(&row);
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
    crate::cache::read_cache().put_project(&fresh);
    Outcome::Ok(fresh)
}

/// Get project by slug (cache-first)
pub async fn get_project_by_slug(
    cx: &Cx,
    pool: &DbPool,
    slug: &str,
) -> Outcome<ProjectRow, DbError> {
    if let Some(cached) = crate::cache::read_cache().get_project(slug) {
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
                    crate::cache::read_cache().put_project(&row);
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
    if let Some(cached) = crate::cache::read_cache().get_project_by_human_key(human_key) {
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
                    crate::cache::read_cache().put_project(&row);
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

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    // Defaults for INSERT (new row)
    let insert_attach_pol = attachments_policy.unwrap_or("auto");
    let insert_task_desc = task_description.unwrap_or_default();

    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    // Dynamic UPSERT: preserve existing values if input is None
    let task_desc_clause = if task_description.is_some() {
        "excluded.task_description"
    } else {
        "agents.task_description"
    };
    let attach_pol_clause = if attachments_policy.is_some() {
        "excluded.attachments_policy"
    } else {
        "agents.attachments_policy"
    };

    let upsert_sql = format!(
        "INSERT INTO agents \
        (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
        ON CONFLICT(project_id, name) DO UPDATE SET \
            program = excluded.program, \
            model = excluded.model, \
            task_description = {task_desc_clause}, \
            last_active_ts = excluded.last_active_ts, \
            attachments_policy = {attach_pol_clause}"
    );

    let upsert_params = [
        Value::BigInt(project_id),
        Value::Text(name.to_string()),
        Value::Text(program.to_string()),
        Value::Text(model.to_string()),
        Value::Text(insert_task_desc.to_string()),
        Value::BigInt(now),
        Value::BigInt(now),
        Value::Text(insert_attach_pol.to_string()),
        Value::Text("auto".to_string()),
    ];
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, &upsert_sql, &upsert_params).await)
    );

    // Dynamic Normalize: enforce fields on matching rows, preserving if None
    let mut normalize_sets = vec!["program = ?", "model = ?", "last_active_ts = ?"];
    let mut normalize_params = vec![
        Value::Text(program.to_string()),
        Value::Text(model.to_string()),
        Value::BigInt(now),
    ];

    if let Some(td) = task_description {
        normalize_sets.push("task_description = ?");
        normalize_params.push(Value::Text(td.to_string()));
    }
    if let Some(ap) = attachments_policy {
        normalize_sets.push("attachments_policy = ?");
        normalize_params.push(Value::Text(ap.to_string()));
    }

    let normalize_sql = format!(
        "UPDATE agents SET {} WHERE project_id = ? AND name = ?",
        normalize_sets.join(", ")
    );
    normalize_params.push(Value::BigInt(project_id));
    normalize_params.push(Value::Text(name.to_string()));

    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, &normalize_sql, &normalize_params).await)
    );

    let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                     inception_ts, last_active_ts, attachments_policy, contact_policy \
                     FROM agents \
                     WHERE project_id = ? AND name = ? \
                     ORDER BY last_active_ts DESC, id DESC \
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
    crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &fresh);
    Outcome::Ok(fresh)
}

/// Create a new agent identity, failing if the name is already taken.
///
/// Unlike `register_agent` (which does an upsert), this function enforces
/// strict uniqueness and returns `DbError::Duplicate` when the identity exists.
#[allow(clippy::too_many_arguments)]
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

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    let task_desc = task_description.unwrap_or_default();
    let attach_pol = attachments_policy.unwrap_or("auto");
    let fetch_sql = "SELECT id, project_id, name, program, model, task_description, \
                     inception_ts, last_active_ts, attachments_policy, contact_policy \
                     FROM agents WHERE project_id = ? AND name = ? LIMIT 1";
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
                        && (msg.contains("agents.project_id") || msg.contains("agents.name"))
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
    let fresh = found;
    crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &fresh);
    Outcome::Ok(fresh)
}

/// Get agent by project and name (cache-first)
pub async fn get_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
) -> Outcome<AgentRow, DbError> {
    if let Some(cached) =
        crate::cache::read_cache().get_agent_scoped(pool.sqlite_path(), project_id, name)
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

    // Optimized: filter by name directly in SQL.
    let sql = "SELECT id, project_id, name, program, model, task_description, \
               inception_ts, last_active_ts, attachments_policy, contact_policy \
               FROM agents WHERE project_id = ? AND name = ? LIMIT 1";
    let params = [Value::BigInt(project_id), Value::Text(name.to_string())];

    match map_sql_outcome(traw_query(cx, &tracked, sql, &params).await) {
        Outcome::Ok(rows) => rows.first().map_or_else(
            || Outcome::Err(DbError::not_found("Agent", format!("{project_id}:{name}"))),
            |row| {
                let agent = decode_agent_row_indexed(row);
                crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &agent);
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
        crate::cache::read_cache().get_agent_by_id_scoped(pool.sqlite_path(), agent_id)
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
                crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &agent);
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
               FROM agents WHERE project_id = ?";
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
    for id in agent_ids {
        if let Some(cached) = cache.get_agent_by_id(*id) {
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
                    crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &agent);
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
    let should_flush = crate::cache::read_cache().enqueue_touch(agent_id, now);

    if should_flush {
        flush_deferred_touches(cx, pool).await
    } else {
        Outcome::Ok(())
    }
}

/// Immediately flush all pending deferred touch updates to the DB.
/// Call this on server shutdown or when precise `last_active_ts` is needed.
pub async fn flush_deferred_touches(cx: &Cx, pool: &DbPool) -> Outcome<(), DbError> {
    let pending = crate::cache::read_cache().drain_touches();
    if pending.is_empty() {
        return Outcome::Ok(());
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => {
            re_enqueue_touches(&pending);
            return Outcome::Err(e);
        }
        Outcome::Cancelled(r) => {
            re_enqueue_touches(&pending);
            return Outcome::Cancelled(r);
        }
        Outcome::Panicked(p) => {
            re_enqueue_touches(&pending);
            return Outcome::Panicked(p);
        }
    };

    let tracked = tracked(&*conn);

    // Batch all updates in a single transaction.
    match begin_concurrent_tx(cx, &tracked).await {
        Outcome::Ok(()) => {}
        other => {
            re_enqueue_touches(&pending);
            return match other {
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
                Outcome::Ok(()) => unreachable!(),
            };
        }
    }

    // Batch UPDATE using VALUES CTE: 1 prepare/execute per chunk instead of per-agent.
    // SQLite parameter limit is 999; 2 params per row → max 499 per chunk.
    let entries: Vec<_> = pending.iter().collect();

    for chunk in entries.chunks(400) {
        let placeholders = std::iter::repeat_n("(?,?)", chunk.len()).collect::<Vec<_>>();
        let sql = format!(
            "WITH batch(agent_id, new_ts) AS (VALUES {}) \
             UPDATE agents SET last_active_ts = MAX(agents.last_active_ts, batch.new_ts) \
             FROM batch WHERE agents.id = batch.agent_id",
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
                re_enqueue_touches(&pending);
                return Outcome::Err(e);
            }
            Outcome::Cancelled(r) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                re_enqueue_touches(&pending);
                return Outcome::Cancelled(r);
            }
            Outcome::Panicked(p) => {
                let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
                re_enqueue_touches(&pending);
                return Outcome::Panicked(p);
            }
        }
    }

    match map_sql_outcome(traw_execute(cx, &tracked, "COMMIT", &[]).await) {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(e) => {
            let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
            re_enqueue_touches(&pending);
            Outcome::Err(e)
        }
        Outcome::Cancelled(r) => {
            let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
            re_enqueue_touches(&pending);
            Outcome::Cancelled(r)
        }
        Outcome::Panicked(p) => {
            let _ = map_sql_outcome(traw_execute(cx, &tracked, "ROLLBACK", &[]).await);
            re_enqueue_touches(&pending);
            Outcome::Panicked(p)
        }
    }
}

/// Re-enqueue touches that failed to flush, so they aren't lost.
fn re_enqueue_touches(pending: &std::collections::HashMap<i64, i64>) {
    let cache = crate::cache::read_cache();
    for (&agent_id, &ts) in pending {
        cache.enqueue_touch(agent_id, ts);
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

    // Read current row first so we can preserve attachments_policy explicitly.
    // Some engines can clobber this field on partial UPDATE statements.
    let current_sql = "SELECT id, project_id, name, program, model, task_description, \
                       inception_ts, last_active_ts, attachments_policy, contact_policy \
                       FROM agents WHERE id = ? LIMIT 1";
    let current_rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_query(cx, &tracked, current_sql, &[Value::BigInt(agent_id)]).await)
    );
    let Some(current_agent) = current_rows.first().map(decode_agent_row_indexed) else {
        rollback_tx(cx, &tracked).await;
        return Outcome::Err(DbError::not_found("Agent", agent_id.to_string()));
    };

    let now = now_micros();
    let sql = "UPDATE agents SET attachments_policy = ?, contact_policy = ?, last_active_ts = ? WHERE id = ?";
    let params = [
        Value::Text(current_agent.attachments_policy.clone()),
        Value::Text(policy.to_string()),
        Value::BigInt(now),
        Value::BigInt(agent_id),
    ];
    try_in_tx!(
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
    crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &agent);
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
                       FROM agents WHERE project_id = ? AND name = ? \
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

    let sql = "UPDATE agents SET attachments_policy = ?, contact_policy = ?, last_active_ts = ? WHERE id = ?";
    let params = [
        Value::Text(current_agent.attachments_policy.clone()),
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
    crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &agent);
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
    pub attachments: String,
    pub from: String,
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
                id: None, // populated below via reload if possible, or left None?
                // Ideally we reload, but batch insert makes reloading IDs hard in SQLite without RETURNING.
                // FrankenSQLite supports RETURNING?
                // Let's assume standard SQLite logic. RETURNING is supported in recent SQLite.
                // But to be safe and consistent with other queries, we might skip ID reload if not strictly needed?
                // Wait, the tool returns IDs. We need them.
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

        // If we can use RETURNING, great. If not, we have to query back.
        // Let's use RETURNING clause which is robust in modern SQLite (3.35+).
        // Since we control the environment (bundled sqlite), we can rely on RETURNING.
        query.push_str(" RETURNING id");

        let ids_outcome = map_sql_outcome(traw_query(cx, &tracked, &query, &params).await);
        let id_rows = try_in_tx!(cx, &tracked, ids_outcome);

        // Assign IDs back to created_rows slice corresponding to this chunk
        let start_idx = created_rows.len() - chunk.len();
        for (j, row) in id_rows.iter().enumerate() {
            if let Some(id) = row_first_i64(row)
                && let Some(cr) = created_rows.get_mut(start_idx + j)
            {
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

    // Insert message using RETURNING id to safely get the generated ID without races.
    let sql = "INSERT INTO messages \
               (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
               RETURNING id";
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

    let rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_query(cx, &tracked, sql, &params).await)
    );
    let message_id = rows
        .first()
        .and_then(row_first_i64)
        .ok_or_else(|| DbError::Internal("Message INSERT RETURNING id failed".to_string()));

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
#[allow(clippy::too_many_arguments)]
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

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    let max = *MVCC_MAX_RETRIES;

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
                mvcc_backoff(attempt);
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
            Outcome::Ok(row) => {
                // Invalidate cached inbox stats for all recipients.
                let cache = crate::cache::read_cache();
                let cache_scope = pool.sqlite_path();
                for (agent_id, _kind) in recipients {
                    cache.invalidate_inbox_stats_scoped(cache_scope, *agent_id);
                }
                return Outcome::Ok(row);
            }
            other => return other,
        }
    }
    // Unreachable: loop always returns via `attempt == max` or success.
    Outcome::Err(DbError::Internal("MVCC retry loop fell through".into()))
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

    // Insert message using RETURNING id to safely get the generated ID without races.
    let sql = "INSERT INTO messages \
               (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
               RETURNING id";
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

    let rows = try_in_tx!(
        cx,
        tracked,
        map_sql_outcome(traw_query(cx, tracked, sql, &params).await)
    );
    let message_id = rows
        .first()
        .and_then(row_first_i64)
        .ok_or_else(|| DbError::Internal("Message INSERT RETURNING id failed".to_string()));

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
                    m.importance, m.ack_required, m.created_ts, m.attachments, a.name as from_name \
             FROM messages m \
             JOIN agents a ON a.id = m.sender_id \
             WHERE m.id IN ({placeholders})"
        );

        let params: Vec<Value> = chunk.iter().map(|&id| Value::BigInt(id)).collect();

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
            Outcome::Ok(rows) => {
                for row in rows {
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
                    let attachments: String = match row.get_named("attachments") {
                        Ok(v) => v,
                        Err(e) => return Outcome::Err(map_sql_error(&e)),
                    };
                    let from: String = match row.get_named("from_name") {
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

    let mut sql = String::from(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                m.importance, m.ack_required, m.created_ts, m.attachments, a.name as from_name \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
         WHERE m.project_id = ? AND ",
    );

    let mut params: Vec<Value> = vec![Value::BigInt(project_id)];

    if let Ok(root_id) = thread_id.parse::<i64>() {
        sql.push_str("(m.id = ? OR m.thread_id = ?)");
        params.push(Value::BigInt(root_id));
    } else {
        sql.push_str("m.thread_id = ?");
    }
    params.push(Value::Text(thread_id.to_string()));

    sql.push_str(" ORDER BY m.created_ts ASC");

    if let Some(limit) = limit {
        if limit < 1 {
            return Outcome::Err(DbError::invalid("limit", "limit must be at least 1"));
        }
        let Ok(limit_i64) = i64::try_from(limit) else {
            return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
        };
        sql.push_str(" LIMIT ?");
        params.push(Value::BigInt(limit_i64));
    }

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
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
                let attachments: String = match row.get_named("attachments") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let from: String = match row.get_named("from_name") {
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
                    attachments,
                    from,
                });
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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

    for chunk in message_ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT DISTINCT a.name \
             FROM message_recipients r \
             JOIN agents a ON a.id = r.agent_id \
             JOIN messages m ON m.id = r.message_id \
             WHERE m.project_id = ? AND r.message_id IN ({placeholders})"
        );

        let mut params: Vec<Value> = Vec::with_capacity(chunk.len() + 1);
        params.push(Value::BigInt(project_id));
        for id in chunk {
            params.push(Value::BigInt(*id));
        }

        match map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await) {
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
                      ack_required, created_ts, attachments \
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
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut sql = String::from(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                m.importance, m.ack_required, m.created_ts, m.attachments, r.kind, s.name as sender_name, r.ack_ts \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         JOIN agents s ON s.id = m.sender_id \
         WHERE r.agent_id = ? AND m.project_id = ?",
    );

    let mut params: Vec<Value> = vec![Value::BigInt(agent_id), Value::BigInt(project_id)];

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
                let attachments: String = match row.get_named("attachments") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let kind: String = match row.get_named("kind") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_name: String = match row.get_named("sender_name") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let ack_ts: Option<i64> = match row.get_named("ack_ts") {
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
                        attachments,
                    },
                    kind,
                    sender_name,
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

    // FTS5 doesn't support leading wildcards (*foo); strip and recurse
    if let Some(stripped) = result.strip_prefix('*') {
        return sanitize_fts_query(stripped);
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
        if !terms.iter().any(|t| t == token) {
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
    // Approximate FTS whitespace semantics (term union) in LIKE fallback mode.
    let where_clause = where_parts.join(" OR ");
    params.push(Value::BigInt(limit));

    let sql = format!(
        "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name, m.body_md \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
         WHERE m.project_id = ? AND ({where_clause}) \
         ORDER BY m.id ASC \
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
    // Approximate FTS whitespace semantics (term union) in LIKE fallback mode.
    let where_clause = where_parts.join(" OR ");
    params.push(Value::BigInt(limit));

    let sql = format!(
        "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name, m.body_md, m.project_id \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
         JOIN product_project_links ppl ON ppl.project_id = m.project_id \
         WHERE ppl.product_id = ? AND ({where_clause}) \
         ORDER BY m.id ASC \
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

    let rows_out = if let Some(ref fts_query) = sanitized {
        // FTS5-backed search with relevance ordering.
        let sql = "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name, m.body_md \
                   FROM fts_messages \
                   JOIN messages m ON m.id = fts_messages.message_id \
                   JOIN agents a ON a.id = m.sender_id \
                   WHERE m.project_id = ? AND fts_messages MATCH ? \
                   ORDER BY bm25(fts_messages, 10.0, 1.0) ASC, m.id ASC \
                   LIMIT ?";
        let params = [
            Value::BigInt(project_id),
            Value::Text(fts_query.clone()),
            Value::BigInt(limit_i64),
        ];
        let fts_result = traw_query(cx, &tracked, sql, &params).await;

        // On FTS failure, fall back to LIKE with extracted terms
        match &fts_result {
            Outcome::Err(_) => {
                tracing::warn!("FTS query failed for '{}', attempting LIKE fallback", query);
                let terms = extract_like_terms(query, 5);
                if terms.is_empty() {
                    Outcome::Ok(Vec::new())
                } else {
                    run_like_fallback(cx, &tracked, project_id, &terms, limit_i64).await
                }
            }
            _ => map_sql_outcome(fts_result),
        }
    } else {
        // Empty/unsearchable query: return empty results
        Outcome::Ok(Vec::new())
    };
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("id") {
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
                let from: String = match row.get_named("from_name") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let body_md: String = row.get_named("body_md").unwrap_or_default();

                out.push(SearchRow {
                    id,
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
    let rows_out = if let Some(ref fts_query) = sanitized {
        let sql = "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name, m.body_md, m.project_id \
                   FROM fts_messages \
                   JOIN messages m ON m.id = fts_messages.message_id \
                   JOIN agents a ON a.id = m.sender_id \
                   JOIN product_project_links ppl ON ppl.project_id = m.project_id \
                   WHERE ppl.product_id = ? AND fts_messages MATCH ? \
                   ORDER BY bm25(fts_messages, 10.0, 1.0) ASC, m.id ASC \
                   LIMIT ?";
        let params = [
            Value::BigInt(product_id),
            Value::Text(fts_query.clone()),
            Value::BigInt(limit_i64),
        ];
        let fts_result = traw_query(cx, &tracked, sql, &params).await;

        match &fts_result {
            Outcome::Err(_) => {
                tracing::warn!(
                    "Product FTS query failed for '{}', attempting LIKE fallback",
                    query
                );
                let terms = extract_like_terms(query, 5);
                if terms.is_empty() {
                    Outcome::Ok(Vec::new())
                } else {
                    run_like_fallback_product(cx, &tracked, product_id, &terms, limit_i64).await
                }
            }
            _ => map_sql_outcome(fts_result),
        }
    } else {
        Outcome::Ok(Vec::new())
    };

    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("id") {
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
                // Use positional access for aliased columns where ORM column name inference
                // incorrectly parses "a.name as from_name" as "name as" instead of "from_name".
                // Column order: id(0), subject(1), importance(2), ack_required(3),
                // created_ts(4), thread_id(5), from_name(6), body_md(7), project_id(8)
                let from: String = match row.get_as(6) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let body_md: String = row.get_as(7).unwrap_or_default();
                let project_id: i64 = match row.get_as(8) {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };

                out.push(SearchRowWithProject {
                    id,
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
                m.importance, m.ack_required, m.created_ts, m.attachments, \
                r.kind, s.name as sender_name, r.ack_ts, p.slug as project_slug \
         FROM message_recipients r \
         JOIN agents a ON a.id = r.agent_id \
         JOIN messages m ON m.id = r.message_id \
         JOIN agents s ON s.id = m.sender_id \
         JOIN projects p ON p.id = m.project_id \
         WHERE a.name = ?",
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
                let id: i64 = row.get_named("id").unwrap_or(0);
                let project_id: i64 = row.get_named("project_id").unwrap_or(0);
                let sender_id: i64 = row.get_named("sender_id").unwrap_or(0);
                let thread_id: Option<String> = row.get_named("thread_id").unwrap_or(None);
                let subject: String = row.get_named("subject").unwrap_or_default();
                let body_md: String = row.get_named("body_md").unwrap_or_default();
                let importance: String = row.get_named("importance").unwrap_or_default();
                let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
                let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
                let attachments: String = row.get_named("attachments").unwrap_or_default();
                let kind: String = row.get_named("kind").unwrap_or_default();
                let sender_name: String = row.get_named("sender_name").unwrap_or_default();
                let ack_ts: Option<i64> = row.get_named("ack_ts").unwrap_or(None);
                let project_slug: String = row.get_named("project_slug").unwrap_or_default();

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
               JOIN agents a ON a.id = r.agent_id \
               JOIN messages m ON m.id = r.message_id \
               JOIN projects p ON p.id = m.project_id \
               WHERE a.name = ? AND r.read_ts IS NULL \
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
    let rows_out = if let Some(ref fts_query) = sanitized {
        // FTS5-backed search across all projects.
        let sql = "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, \
                          m.thread_id, a.name as from_name, m.body_md, \
                          m.project_id, p.slug as project_slug \
                   FROM fts_messages \
                   JOIN messages m ON m.id = fts_messages.message_id \
                   JOIN agents a ON a.id = m.sender_id \
                   JOIN projects p ON p.id = m.project_id \
                   WHERE fts_messages MATCH ? \
                   ORDER BY bm25(fts_messages, 10.0, 1.0) ASC, m.id ASC \
                   LIMIT ?";
        let params = [Value::Text(fts_query.clone()), Value::BigInt(limit_i64)];
        let fts_result = traw_query(cx, &tracked, sql, &params).await;

        match &fts_result {
            Outcome::Err(_) => {
                tracing::warn!(
                    "Global FTS query failed for '{}', attempting LIKE fallback",
                    query
                );
                let terms = extract_like_terms(query, 5);
                if terms.is_empty() {
                    Outcome::Ok(Vec::new())
                } else {
                    run_like_fallback_global(cx, &tracked, &terms, limit_i64).await
                }
            }
            _ => map_sql_outcome(fts_result),
        }
    } else {
        Outcome::Ok(Vec::new())
    };

    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = row.get_named("id").unwrap_or(0);
                let subject: String = row.get_named("subject").unwrap_or_default();
                let importance: String = row.get_named("importance").unwrap_or_default();
                let ack_required: i64 = row.get_named("ack_required").unwrap_or(0);
                let created_ts: i64 = row.get_named("created_ts").unwrap_or(0);
                let thread_id: Option<String> = row.get_named("thread_id").unwrap_or(None);
                let from: String = row.get_named("from_name").unwrap_or_default();
                let body_md: String = row.get_named("body_md").unwrap_or_default();
                let project_id: i64 = row.get_named("project_id").unwrap_or(0);
                let project_slug: String = row.get_named("project_slug").unwrap_or_default();

                out.push(GlobalSearchRow {
                    id,
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
    let mut params: Vec<Value> = Vec::with_capacity(terms.len() + 1);

    for term in terms {
        conditions.push("(m.subject LIKE ? OR m.body_md LIKE ?)");
        let pattern = format!("%{term}%");
        params.push(Value::Text(pattern.clone()));
        params.push(Value::Text(pattern));
    }

    let sql = format!(
        "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, \
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
    let rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
    );
    if rows == 0 {
        rollback_tx(cx, &tracked).await;
        return Outcome::Err(DbError::not_found(
            "MessageRecipient",
            format!("{agent_id}:{message_id}"),
        ));
    }

    // Invalidate cached inbox stats (unread_count may have changed).
    crate::cache::read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), agent_id);

    // Read back the actual stored timestamp (may differ from `now` on
    // idempotent calls where COALESCE preserved the original value).
    let read_sql = "SELECT read_ts FROM message_recipients WHERE agent_id = ? AND message_id = ?";
    let read_params = [Value::BigInt(agent_id), Value::BigInt(message_id)];
    let ts = match map_sql_outcome(traw_query(cx, &tracked, read_sql, &read_params).await) {
        Outcome::Ok(rows) => rows
            .first()
            .and_then(|r| r.get(0))
            .and_then(|v| match v {
                Value::BigInt(n) => Some(*n),
                Value::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
            .unwrap_or(now),
        Outcome::Err(_) => now,
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
    crate::cache::read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), agent_id);

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
            Outcome::Err(_) => (now, now),
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
    let cache_scope = pool.sqlite_path();
    if let Some(cached) = crate::cache::read_cache().get_inbox_stats_scoped(cache_scope, agent_id) {
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
                crate::cache::read_cache().put_inbox_stats_scoped(cache_scope, &stats);
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

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let sql = format!(
        "{FILE_RESERVATION_SELECT_COLUMNS_SQL} \
         WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts > ?"
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

/// Release file reservations
pub async fn release_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    paths: Option<&[&str]>,
    reservation_ids: Option<&[i64]>,
) -> Outcome<Vec<FileReservationRow>, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    let mut sql = format!(
        "UPDATE file_reservations SET released_ts = ? \
         WHERE project_id = ? AND agent_id = ? AND ({ACTIVE_RESERVATION_PREDICATE})"
    );
    let mut params: Vec<Value> = vec![
        Value::BigInt(now),
        Value::BigInt(project_id),
        Value::BigInt(agent_id),
    ];

    if let Some(ids) = reservation_ids
        && !ids.is_empty()
    {
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

    if let Some(pats) = paths
        && !pats.is_empty()
    {
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

    sql.push_str(" RETURNING id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts");

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    let result = match rows_out {
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
            Outcome::Err(e)
        }
        Outcome::Cancelled(r) => {
            rollback_tx(cx, &tracked).await;
            Outcome::Cancelled(r)
        }
        Outcome::Panicked(p) => {
            rollback_tx(cx, &tracked).await;
            Outcome::Panicked(p)
        }
    };

    if result.is_ok() {
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    }
    result
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
         WHERE project_id = ? AND agent_id = ? AND ({ACTIVE_RESERVATION_PREDICATE})"
    );
    let mut params: Vec<Value> = vec![Value::BigInt(project_id), Value::BigInt(agent_id)];

    if let Some(ids) = reservation_ids
        && !ids.is_empty()
    {
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

    if let Some(pats) = paths
        && !pats.is_empty()
    {
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
        (
            format!(
                "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts FROM file_reservations WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) AND expires_ts > ? ORDER BY id"
            ),
            vec![Value::BigInt(project_id), Value::BigInt(now)],
        )
    } else {
        (
            // Legacy Python schema stored released_ts as TEXT (e.g. "2026-02-05 02:21:37.212634").
            // Coerce it to INTEGER microseconds so listing historical reservations can't crash.
            "SELECT \
                 id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, \
                 CASE \
                     WHEN released_ts IS NULL THEN NULL \
                     WHEN typeof(released_ts) = 'text' THEN CAST(strftime('%s', released_ts) AS INTEGER) * 1000000 + \
                         CASE WHEN instr(released_ts, '.') > 0 \
                              THEN CAST(substr(released_ts || '000000', instr(released_ts, '.') + 1, 6) AS INTEGER) \
                              ELSE 0 \
                         END \
                     ELSE released_ts \
                 END AS released_ts \
             FROM file_reservations \
             WHERE project_id = ? \
             ORDER BY id"
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
        "SELECT id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts FROM file_reservations WHERE project_id = ? AND ({ACTIVE_RESERVATION_PREDICATE}) ORDER BY id"
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
#[allow(clippy::too_many_arguments)]
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
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    // Atomic upsert: INSERT OR IGNORE + UPDATE to avoid TOCTOU race under
    // concurrent send_message auto-handshake (multiple agents requesting
    // contact for the same pair simultaneously).
    let upsert_sql = "INSERT INTO agent_links \
        (a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) \
        VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?) \
        ON CONFLICT(a_project_id, a_agent_id, b_project_id, b_agent_id) DO UPDATE SET \
            status = 'pending', reason = excluded.reason, updated_ts = excluded.updated_ts, \
            expires_ts = excluded.expires_ts";

    let upsert_params: Vec<Value> = vec![
        Value::BigInt(from_project_id),
        Value::BigInt(from_agent_id),
        Value::BigInt(to_project_id),
        Value::BigInt(to_agent_id),
        Value::Text(reason.to_string()),
        Value::BigInt(now),
        Value::BigInt(now),
        expires.map_or(Value::Null, Value::BigInt),
    ];

    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, upsert_sql, &upsert_params).await)
    );

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
        return Outcome::Err(DbError::not_found("AgentLink", "upserted row"));
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
    let updated = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(update!(&row).execute(cx, &tracked).await)
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
/// Returns the number of rows affected (0 if already released or not found).
pub async fn force_release_reservation(
    cx: &Cx,
    pool: &DbPool,
    reservation_id: i64,
) -> Outcome<usize, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    let sql = "UPDATE file_reservations SET released_ts = ? WHERE id = ? AND released_ts IS NULL";
    let params = [Value::BigInt(now), Value::BigInt(reservation_id)];

    let n = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await)
    );
    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    usize::try_from(n).map_or_else(
        |_| {
            Outcome::Err(DbError::invalid(
                "row_count",
                "row count exceeds usize::MAX",
            ))
        },
        Outcome::Ok,
    )
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

    // Check messages sent
    let sql_sent =
        "SELECT MAX(created_ts) as max_ts FROM messages WHERE sender_id = ? AND project_id = ?";
    let params = [Value::BigInt(agent_id), Value::BigInt(project_id)];
    let sent_ts = match map_sql_outcome(traw_query(cx, &tracked, sql_sent, &params).await) {
        Outcome::Ok(rows) => rows.first().and_then(|r| {
            r.get(0).and_then(|v| match v {
                Value::BigInt(n) => Some(*n),
                Value::Int(n) => Some(i64::from(*n)),
                _ => None,
            })
        }),
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    // Check message reads/acks by this agent
    let sql_read = "SELECT MAX(COALESCE(r.read_ts, 0)), MAX(COALESCE(r.ack_ts, 0)) \
                    FROM message_recipients r \
                    JOIN messages m ON m.id = r.message_id \
                    WHERE r.agent_id = ? AND m.project_id = ?";
    let params2 = [Value::BigInt(agent_id), Value::BigInt(project_id)];
    let (read_ts, ack_ts) =
        match map_sql_outcome(traw_query(cx, &tracked, sql_read, &params2).await) {
            Outcome::Ok(rows) => {
                let row = rows.first();
                let read = row.and_then(|r| {
                    r.get(0).and_then(|v| match v {
                        Value::BigInt(n) if *n > 0 => Some(*n),
                        Value::Int(n) if *n > 0 => Some(i64::from(*n)),
                        _ => None,
                    })
                });
                let ack = row.and_then(|r| {
                    r.get(1).and_then(|v| match v {
                        Value::BigInt(n) if *n > 0 => Some(*n),
                        Value::Int(n) if *n > 0 => Some(i64::from(*n)),
                        _ => None,
                    })
                });
                (read, ack)
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        };

    // Return the maximum of all timestamps
    let max_ts = [sent_ts, read_ts, ack_ts].into_iter().flatten().max();

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

    let sql = "SELECT DISTINCT project_id FROM file_reservations WHERE released_ts IS NULL";
    let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &[]).await);
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
/// Sets `released_ts = now` for all unreleased reservations whose `expires_ts`
/// has elapsed. Returns the IDs of released reservations.
const EXPIRED_RESERVATIONS_WHERE_SQL: &str =
    "project_id = ? AND released_ts IS NULL AND expires_ts <= ?";

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

    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    // First, collect the IDs to be released.
    let select_sql =
        format!("SELECT id FROM file_reservations WHERE {EXPIRED_RESERVATIONS_WHERE_SQL}");
    let params = [Value::BigInt(project_id), Value::BigInt(now)];
    let rows = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_query(cx, &tracked, &select_sql, &params).await)
    );
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        if let Ok(id) = row.get_named::<i64>("id") {
            ids.push(id);
        }
    }

    if ids.is_empty() {
        try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
        return Outcome::Ok(ids);
    }

    // Update them all at once.
    let update_sql = format!(
        "UPDATE file_reservations SET released_ts = ? WHERE {EXPIRED_RESERVATIONS_WHERE_SQL}"
    );
    let update_params = [
        Value::BigInt(now),
        Value::BigInt(project_id),
        Value::BigInt(now),
    ];
    try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(traw_execute(cx, &tracked, &update_sql, &update_params).await)
    );
    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    Outcome::Ok(ids)
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
                    created_ts, expires_ts, released_ts \
             FROM file_reservations \
             WHERE id IN ({placeholders})"
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
/// Sets `released_ts = now` for all given IDs that have `released_ts IS NULL`.
/// Returns the number of rows affected.
pub async fn release_reservations_by_ids(
    cx: &Cx,
    pool: &DbPool,
    ids: &[i64],
) -> Outcome<usize, DbError> {
    if ids.is_empty() {
        return Outcome::Ok(0);
    }

    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);
    try_in_tx!(cx, &tracked, begin_concurrent_tx(cx, &tracked).await);

    let mut total_affected = 0usize;

    for chunk in ids.chunks(MAX_IN_CLAUSE_ITEMS) {
        // Build parameterized IN clause.
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let sql = format!(
            "UPDATE file_reservations SET released_ts = ? WHERE id IN ({}) AND released_ts IS NULL",
            placeholders.join(",")
        );

        let mut params = Vec::with_capacity(1 + chunk.len());
        params.push(Value::BigInt(now));
        for &id in chunk {
            params.push(Value::BigInt(id));
        }

        let n = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(traw_execute(cx, &tracked, &sql, &params).await)
        );
        total_affected = total_affected.saturating_add(usize::try_from(n).unwrap_or(0));
    }

    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);
    Outcome::Ok(total_affected)
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
               WHERE m.ack_required = 1 AND mr.ack_ts IS NULL";

    match map_sql_outcome(traw_query(cx, &tracked, sql, &[]).await) {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                let mid = match r.get(0) {
                    Some(Value::BigInt(n)) => *n,
                    Some(Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                let pid = match r.get(1) {
                    Some(Value::BigInt(n)) => *n,
                    Some(Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                let cts = match r.get(2) {
                    Some(Value::BigInt(n)) => *n,
                    Some(Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                let aid = match r.get(3) {
                    Some(Value::BigInt(n)) => *n,
                    Some(Value::Int(n)) => i64::from(*n),
                    _ => continue,
                };
                out.push(UnackedMessageRow {
                    message_id: mid,
                    project_id: pid,
                    created_ts: cts,
                    agent_id: aid,
                });
            }
            Outcome::Ok(out)
        }
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
                      m.importance, m.ack_required, m.created_ts, m.attachments, \
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
                let id: i64 = match row.get_named("id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let proj_id: i64 = match row.get_named("project_id") {
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
                let attachments: String = match row.get_named("attachments") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let kind: String = match row.get_named("kind") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let sender_name: String = match row.get_named("sender_name") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                let read_ts: Option<i64> = match row.get_named("read_ts") {
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
                      FROM agents WHERE project_id = ? AND name = ? LIMIT 1";
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
    crate::cache::read_cache().put_agent_scoped(pool.sqlite_path(), &found);
    Outcome::Ok(found)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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

            let fetched = get_agent(&cx, &pool, project_id, "BlueLake")
                .await
                .into_result()
                .expect("get_agent should work after policy updates");
            assert_eq!(fetched.contact_policy, "contacts_only");
            assert_eq!(fetched.attachments_policy, "inline");

            let cached = crate::read_cache()
                .get_agent_scoped(pool.sqlite_path(), project_id, "BlueLake")
                .expect("cache entry should be refreshed");
            assert_eq!(cached.contact_policy, "contacts_only");
            assert_eq!(cached.attachments_policy, "inline");
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

            let since_ts = now_micros();

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
    fn expired_reservations_where_clause_is_inclusive() {
        assert!(EXPIRED_RESERVATIONS_WHERE_SQL.contains("expires_ts <= ?"));
        assert!(!EXPIRED_RESERVATIONS_WHERE_SQL.contains("expires_ts < ?"));
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
            crate::cache::read_cache().enqueue_touch(agent_id, now_micros());
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
