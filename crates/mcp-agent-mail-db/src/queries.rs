//! Database query operations
//!
//! CRUD operations for all models using `sqlmodel_rust`.
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
use sqlmodel::prelude::*;
use sqlmodel_core::{Connection, Dialect, Error as SqlError, IsolationLevel, PreparedStatement};
use sqlmodel_core::{Row as SqlRow, TransactionOps, Value};
use sqlmodel_query::{raw_execute, raw_query};

// =============================================================================
// Tracked query wrappers
// =============================================================================

struct TrackedConnection<'conn> {
    inner: &'conn sqlmodel_sqlite::SqliteConnection,
}

impl<'conn> TrackedConnection<'conn> {
    fn new(inner: &'conn sqlmodel_sqlite::SqliteConnection) -> Self {
        Self { inner }
    }
}

struct TrackedTransaction<'conn> {
    inner: sqlmodel_sqlite::SqliteTransaction<'conn>,
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

/// `SQLite` default `SQLITE_MAX_VARIABLE_NUMBER` is 999 (32766 in newer builds).
/// We cap IN-clause item counts well below that to prevent excessively large
/// SQL strings and parameter arrays from untrusted input.
const MAX_IN_CLAUSE_ITEMS: usize = 500;

fn placeholders(count: usize) -> String {
    let capped = count.min(MAX_IN_CLAUSE_ITEMS);
    std::iter::repeat_n("?", capped)
        .collect::<Vec<_>>()
        .join(", ")
}

async fn acquire_conn(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<sqlmodel_pool::PooledConnection<sqlmodel_sqlite::SqliteConnection>, DbError> {
    map_sql_outcome(pool.acquire(cx).await)
}

fn tracked(conn: &sqlmodel_sqlite::SqliteConnection) -> TrackedConnection<'_> {
    TrackedConnection::new(conn)
}

// =============================================================================
// Transaction helpers
// =============================================================================

/// Begin an immediate write transaction (acquires the WAL write lock).
async fn begin_immediate_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    map_sql_outcome(tracked.execute(cx, "BEGIN IMMEDIATE", &[]).await).map(|_| ())
}

/// Commit the current transaction (single fsync in WAL mode).
async fn commit_tx(cx: &Cx, tracked: &TrackedConnection<'_>) -> Outcome<(), DbError> {
    map_sql_outcome(tracked.execute(cx, "COMMIT", &[]).await).map(|_| ())
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

/// Ensure a project exists, creating if necessary.
///
/// Returns the project row (existing or newly created).
/// Uses the in-memory cache to avoid DB round-trips on repeated calls.
pub async fn ensure_project(
    cx: &Cx,
    pool: &DbPool,
    human_key: &str,
) -> Outcome<ProjectRow, DbError> {
    // Validate absolute path
    if !human_key.starts_with('/') {
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
    let existing = map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("slug").eq(slug.as_str()))
            .first(cx, &tracked)
            .await,
    );
    match existing {
        Outcome::Ok(Some(row)) => {
            crate::cache::read_cache().put_project(&row);
            Outcome::Ok(row)
        }
        Outcome::Ok(None) => {
            let mut row = ProjectRow::new(slug.clone(), human_key.to_string());
            let id_out = map_sql_outcome(insert!(&row).execute(cx, &tracked).await);
            match id_out {
                Outcome::Ok(id) => {
                    row.id = Some(id);
                    crate::cache::read_cache().put_project(&row);
                    Outcome::Ok(row)
                }
                Outcome::Err(e) => {
                    // Concurrency/race hardening: if another caller created the project after our
                    // initial SELECT, the INSERT may fail with a UNIQUE constraint violation on
                    // projects.slug. In that case, re-select and return the existing row.
                    let is_unique_slug = match &e {
                        DbError::Sqlite(msg) => {
                            let msg = msg.to_ascii_lowercase();
                            msg.contains("unique constraint failed")
                                && msg.contains("projects.slug")
                        }
                        _ => false,
                    };

                    if !is_unique_slug {
                        return Outcome::Err(e);
                    }

                    match map_sql_outcome(
                        select!(ProjectRow)
                            .filter(Expr::col("slug").eq(slug.as_str()))
                            .first(cx, &tracked)
                            .await,
                    ) {
                        Outcome::Ok(Some(row)) => {
                            crate::cache::read_cache().put_project(&row);
                            Outcome::Ok(row)
                        }
                        Outcome::Ok(None) => Outcome::Err(e),
                        Outcome::Err(select_err) => Outcome::Err(select_err),
                        Outcome::Cancelled(r) => Outcome::Cancelled(r),
                        Outcome::Panicked(p) => Outcome::Panicked(p),
                    }
                }
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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

    match map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("slug").eq(slug))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => {
            crate::cache::read_cache().put_project(&row);
            Outcome::Ok(row)
        }
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", slug)),
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

    match map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("human_key").eq(human_key))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => {
            crate::cache::read_cache().put_project(&row);
            Outcome::Ok(row)
        }
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", human_key)),
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

    match map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("id").eq(project_id))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", project_id.to_string())),
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

    map_sql_outcome(select!(ProjectRow).all(cx, &tracked).await)
}

// =============================================================================
// Agent Queries
// =============================================================================

/// Register or update an agent
#[allow(clippy::too_many_arguments)]
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

    // Atomic upsert: INSERT ... ON CONFLICT(project_id, name) DO UPDATE.
    // Avoids TOCTOU race between SELECT and INSERT that could trigger a
    // UNIQUE constraint violation under concurrent registration.
    let task_desc = task_description.unwrap_or_default();
    let attach_pol = attachments_policy.unwrap_or("auto");

    let upsert_sql = "\
        INSERT INTO agents (project_id, name, program, model, task_description, \
                            inception_ts, last_active_ts, attachments_policy, contact_policy) \
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'auto') \
        ON CONFLICT(project_id, name) DO UPDATE SET \
            program = excluded.program, \
            model = excluded.model, \
            task_description = excluded.task_description, \
            last_active_ts = excluded.last_active_ts, \
            attachments_policy = excluded.attachments_policy \
        RETURNING *";
    let params = [
        Value::BigInt(project_id),
        Value::Text(name.to_string()),
        Value::Text(program.to_string()),
        Value::Text(model.to_string()),
        Value::Text(task_desc.to_string()),
        Value::BigInt(now),
        Value::BigInt(now),
        Value::Text(attach_pol.to_string()),
    ];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, upsert_sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => rows.into_iter().next().map_or_else(
            || {
                Outcome::Err(DbError::Internal(
                    "register_agent: upsert RETURNING produced no rows".to_string(),
                ))
            },
            |r| match AgentRow::from_row(&r) {
                Ok(row) => {
                    crate::cache::read_cache().put_agent(&row);
                    Outcome::Ok(row)
                }
                Err(e) => Outcome::Err(map_sql_error(&e)),
            },
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Create a new agent identity, failing if the name is already taken.
///
/// Unlike `register_agent` (which does an upsert), this function uses
/// `INSERT ... ON CONFLICT DO NOTHING` to atomically check uniqueness
/// and insert in a single statement, eliminating the TOCTOU race between
/// a separate `get_agent` check and `register_agent` upsert.
///
/// Returns `DbError::Duplicate` if the name already exists.
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

    let task_desc = task_description.unwrap_or_default();
    let attach_pol = attachments_policy.unwrap_or("auto");

    // Atomic insert-if-absent: ON CONFLICT DO NOTHING means no row is
    // returned when the name already exists in this project.
    let insert_sql = "\
        INSERT INTO agents (project_id, name, program, model, task_description, \
                            inception_ts, last_active_ts, attachments_policy, contact_policy) \
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'auto') \
        ON CONFLICT(project_id, name) DO NOTHING \
        RETURNING *";
    let params = [
        Value::BigInt(project_id),
        Value::Text(name.to_string()),
        Value::Text(program.to_string()),
        Value::Text(model.to_string()),
        Value::Text(task_desc.to_string()),
        Value::BigInt(now),
        Value::BigInt(now),
        Value::Text(attach_pol.to_string()),
    ];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, insert_sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            rows.into_iter().next().map_or_else(
                // No rows returned: name already exists (ON CONFLICT DO NOTHING)
                || {
                    Outcome::Err(DbError::duplicate(
                        "agent",
                        format!("{name} (project {project_id})"),
                    ))
                },
                |r| match AgentRow::from_row(&r) {
                    Ok(row) => {
                        crate::cache::read_cache().put_agent(&row);
                        Outcome::Ok(row)
                    }
                    Err(e) => Outcome::Err(map_sql_error(&e)),
                },
            )
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get agent by project and name (cache-first)
pub async fn get_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
) -> Outcome<AgentRow, DbError> {
    if let Some(cached) = crate::cache::read_cache().get_agent(project_id, name) {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    match map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(project_id))
            .filter(Expr::col("name").eq(name))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => {
            crate::cache::read_cache().put_agent(&row);
            Outcome::Ok(row)
        }
        Outcome::Ok(None) => {
            Outcome::Err(DbError::not_found("Agent", format!("{project_id}:{name}")))
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get agent by id (cache-first).
pub async fn get_agent_by_id(cx: &Cx, pool: &DbPool, agent_id: i64) -> Outcome<AgentRow, DbError> {
    if let Some(cached) = crate::cache::read_cache().get_agent_by_id(agent_id) {
        return Outcome::Ok(cached);
    }

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    match map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("id").eq(agent_id))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => {
            crate::cache::read_cache().put_agent(&row);
            Outcome::Ok(row)
        }
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Agent", agent_id.to_string())),
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

    map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(project_id))
            .all(cx, &tracked)
            .await,
    )
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

    // Batch all updates in a single transaction
    match map_sql_outcome(traw_execute(cx, &tracked, "BEGIN IMMEDIATE", &[]).await) {
        Outcome::Ok(_) => {}
        other => {
            re_enqueue_touches(&pending);
            return match other {
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
                Outcome::Ok(_) => unreachable!(),
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
             UPDATE agents SET last_active_ts = batch.new_ts \
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

    let now = now_micros();
    let sql = "UPDATE agents SET contact_policy = ?, last_active_ts = ? WHERE id = ?";
    let params = [
        Value::Text(policy.to_string()),
        Value::BigInt(now),
        Value::BigInt(agent_id),
    ];
    let out = map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await);

    match out {
        Outcome::Ok(_) => {
            // Fetch updated agent
            match map_sql_outcome(
                select!(AgentRow)
                    .filter(Expr::col("id").eq(agent_id))
                    .first(cx, &tracked)
                    .await,
            ) {
                Outcome::Ok(Some(row)) => {
                    crate::cache::read_cache().put_agent(&row);
                    Outcome::Ok(row)
                }
                Outcome::Ok(None) => {
                    Outcome::Err(DbError::not_found("Agent", agent_id.to_string()))
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

    let mut row = MessageRow {
        id: None,
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

    let id_out = map_sql_outcome(insert!(&row).execute(cx, &tracked).await);
    match id_out {
        Outcome::Ok(id) => {
            row.id = Some(id);
            Outcome::Ok(row)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Create a message AND insert all recipients in a single `SQLite` transaction.
///
/// This eliminates N+2 separate auto-commit writes (1 message INSERT + N
/// recipient INSERTs) into a single transaction with 1 fsync.
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

    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    // Insert message
    let mut row = MessageRow {
        id: None,
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

    let message_id = try_in_tx!(
        cx,
        &tracked,
        map_sql_outcome(insert!(&row).execute(cx, &tracked).await)
    );
    row.id = Some(message_id);

    // Insert all recipients within the same transaction
    for (agent_id, kind) in recipients {
        let recip = MessageRecipientRow {
            message_id,
            agent_id: *agent_id,
            kind: (*kind).to_string(),
            read_ts: None,
            ack_ts: None,
        };
        try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(insert!(&recip).execute(cx, &tracked).await)
        );
    }

    // COMMIT (single fsync)
    try_in_tx!(cx, &tracked, commit_tx(cx, &tracked).await);

    // Invalidate cached inbox stats for all recipients.
    let cache = crate::cache::read_cache();
    let cache_scope = pool.sqlite_path();
    for (agent_id, _kind) in recipients {
        cache.invalidate_inbox_stats_scoped(cache_scope, *agent_id);
    }

    Outcome::Ok(row)
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

    let placeholders = placeholders(message_ids.len());
    let sql = format!(
        "SELECT DISTINCT a.name \
         FROM message_recipients r \
         JOIN agents a ON a.id = r.agent_id \
         JOIN messages m ON m.id = r.message_id \
         WHERE m.project_id = ? AND r.message_id IN ({placeholders})"
    );

    let capped_ids = &message_ids[..message_ids.len().min(MAX_IN_CLAUSE_ITEMS)];
    let mut params: Vec<Value> = Vec::with_capacity(capped_ids.len() + 1);
    params.push(Value::BigInt(project_id));
    for id in capped_ids {
        params.push(Value::BigInt(*id));
    }

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let name: String = match row.get_named("name") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                out.push(name);
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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

    match map_sql_outcome(
        select!(MessageRow)
            .filter(Expr::col("id").eq(message_id))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Message", message_id.to_string())),
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

    // Bare boolean operators without terms
    let upper = trimmed.to_ascii_uppercase();
    if matches!(upper.as_str(), "AND" | "OR" | "NOT") {
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

    // Collapse multiple consecutive spaces
    while result.contains("  ") {
        result = result.replace("  ", " ");
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
            out.push(bytes[i] as char);
            i += 1;
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
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Extract LIKE fallback terms from a raw search query.
///
/// Returns up to `max_terms` alphanumeric tokens (min 2 chars each),
/// excluding FTS boolean keywords.
fn extract_like_terms(query: &str, max_terms: usize) -> Vec<String> {
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
fn like_escape(term: &str) -> String {
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
    let where_clause = where_parts.join(" AND ");
    params.push(Value::BigInt(limit));

    let sql = format!(
        "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name \
         FROM messages m \
         JOIN agents a ON a.id = m.sender_id \
         WHERE m.project_id = ? AND {where_clause} \
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
        let sql = "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name \
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

                out.push(SearchRow {
                    id,
                    subject,
                    importance,
                    ack_required,
                    created_ts,
                    thread_id,
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
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

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

    // Idempotent: only set read_ts if currently NULL.
    let sql = "UPDATE message_recipients SET read_ts = COALESCE(read_ts, ?) WHERE agent_id = ? AND message_id = ?";
    let params = [
        Value::BigInt(now),
        Value::BigInt(agent_id),
        Value::BigInt(message_id),
    ];
    let out = map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await);
    match out {
        Outcome::Ok(rows) => {
            if rows == 0 {
                return Outcome::Err(DbError::not_found(
                    "MessageRecipient",
                    format!("{agent_id}:{message_id}"),
                ));
            }
            // Invalidate cached inbox stats (unread_count may have changed).
            crate::cache::read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), agent_id);

            // Read back the actual stored timestamp (may differ from `now` on
            // idempotent calls where COALESCE preserved the original value).
            let read_sql =
                "SELECT read_ts FROM message_recipients WHERE agent_id = ? AND message_id = ?";
            let read_params = [Value::BigInt(agent_id), Value::BigInt(message_id)];
            let ts_out = map_sql_outcome(traw_query(cx, &tracked, read_sql, &read_params).await);
            match ts_out {
                Outcome::Ok(rows) => {
                    let ts = rows
                        .first()
                        .and_then(|r| r.get(0))
                        .and_then(|v| match v {
                            Value::BigInt(n) => Some(*n),
                            Value::Int(n) => Some(i64::from(*n)),
                            _ => None,
                        })
                        .unwrap_or(now);
                    Outcome::Ok(ts)
                }
                Outcome::Err(_) => Outcome::Ok(now),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
    let out = map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await);
    match out {
        Outcome::Ok(rows) => {
            if rows == 0 {
                return Outcome::Err(DbError::not_found(
                    "MessageRecipient",
                    format!("{agent_id}:{message_id}"),
                ));
            }
            // Invalidate cached inbox stats (ack_pending_count may have changed).
            crate::cache::read_cache().invalidate_inbox_stats_scoped(pool.sqlite_path(), agent_id);

            // Read back the actual stored timestamps (may differ from `now` on
            // idempotent calls where COALESCE preserved the original values).
            let read_sql = "SELECT read_ts, ack_ts FROM message_recipients WHERE agent_id = ? AND message_id = ?";
            let read_params = [Value::BigInt(agent_id), Value::BigInt(message_id)];
            let ts_out = map_sql_outcome(traw_query(cx, &tracked, read_sql, &read_params).await);
            match ts_out {
                Outcome::Ok(rows) => {
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
                    Outcome::Ok((read_ts, ack_ts))
                }
                Outcome::Err(_) => Outcome::Ok((now, now)),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

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

        let id = try_in_tx!(
            cx,
            &tracked,
            map_sql_outcome(insert!(&row).execute(cx, &tracked).await)
        );
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

    map_sql_outcome(
        select!(FileReservationRow)
            .filter(Expr::col("project_id").eq(project_id))
            .filter(Expr::col("released_ts").is_null())
            .filter(Expr::col("expires_ts").gt(now))
            .all(cx, &tracked)
            .await,
    )
}

/// Release file reservations
pub async fn release_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
    paths: Option<&[&str]>,
    reservation_ids: Option<&[i64]>,
) -> Outcome<usize, DbError> {
    let now = now_micros();

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let tracked = tracked(&*conn);

    let mut sql = String::from(
        "UPDATE file_reservations SET released_ts = ? \
         WHERE project_id = ? AND agent_id = ? AND released_ts IS NULL",
    );
    let mut params: Vec<Value> = vec![
        Value::BigInt(now),
        Value::BigInt(project_id),
        Value::BigInt(agent_id),
    ];

    if let Some(ids) = reservation_ids {
        if !ids.is_empty() {
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
        if !pats.is_empty() {
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

    let out = map_sql_outcome(traw_execute(cx, &tracked, &sql, &params).await);
    match out {
        Outcome::Ok(n) => usize::try_from(n).map_or_else(
            |_| {
                Outcome::Err(DbError::invalid(
                    "row_count",
                    "row count exceeds usize::MAX",
                ))
            },
            Outcome::Ok,
        ),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

    // Fetch candidate reservations first (so tools can report old/new expiry).
    let mut sql = String::from(
        "SELECT * FROM file_reservations \
         WHERE project_id = ? AND agent_id = ? AND released_ts IS NULL",
    );
    let mut params: Vec<Value> = vec![Value::BigInt(project_id), Value::BigInt(agent_id)];

    if let Some(ids) = reservation_ids {
        if !ids.is_empty() {
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
        if !pats.is_empty() {
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
        (
            "SELECT * FROM file_reservations WHERE project_id = ? AND released_ts IS NULL AND expires_ts > ? ORDER BY id".to_string(),
            vec![Value::BigInt(project_id), Value::BigInt(now)],
        )
    } else {
        (
            "SELECT * FROM file_reservations WHERE project_id = ? ORDER BY id".to_string(),
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

    let sql =
        "SELECT * FROM file_reservations WHERE project_id = ? AND released_ts IS NULL ORDER BY id";
    let params = vec![Value::BigInt(project_id)];

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, sql, &params).await);
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

    let upsert_out = map_sql_outcome(traw_execute(cx, &tracked, upsert_sql, &upsert_params).await);
    match upsert_out {
        Outcome::Ok(_) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // Fetch the upserted row.
    let fetch = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(from_project_id))
            .filter(Expr::col("a_agent_id").eq(from_agent_id))
            .filter(Expr::col("b_project_id").eq(to_project_id))
            .filter(Expr::col("b_agent_id").eq(to_agent_id))
            .first(cx, &tracked)
            .await,
    );

    match fetch {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("AgentLink", "upserted row")),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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

    let existing = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(from_project_id))
            .filter(Expr::col("a_agent_id").eq(from_agent_id))
            .filter(Expr::col("b_project_id").eq(to_project_id))
            .filter(Expr::col("b_agent_id").eq(to_agent_id))
            .first(cx, &tracked)
            .await,
    );

    match existing {
        Outcome::Ok(Some(mut row)) => {
            row.status = status.to_string();
            row.updated_ts = now;
            row.expires_ts = expires;
            let out = map_sql_outcome(update!(&row).execute(cx, &tracked).await);
            match out {
                Outcome::Ok(n) => usize::try_from(n).map_or_else(
                    |_| {
                        Outcome::Err(DbError::invalid(
                            "row_count",
                            "row count exceeds usize::MAX",
                        ))
                    },
                    |v| Outcome::Ok((v, row)),
                ),
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Ok(None) => Outcome::Err(DbError::not_found(
            "AgentLink",
            format!("{from_project_id}:{from_agent_id}->{to_project_id}:{to_agent_id}"),
        )),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
    let outgoing = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(project_id))
            .filter(Expr::col("a_agent_id").eq(agent_id))
            .all(cx, &tracked)
            .await,
    );

    let outgoing_rows = match outgoing {
        Outcome::Ok(rows) => rows,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    // Incoming: links where this agent is "b" side
    let incoming = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("b_project_id").eq(project_id))
            .filter(Expr::col("b_agent_id").eq(agent_id))
            .all(cx, &tracked)
            .await,
    );

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

    let capped_ids = &candidate_ids[..candidate_ids.len().min(MAX_IN_CLAUSE_ITEMS)];
    let placeholders = placeholders(capped_ids.len());
    let sql = format!(
        "SELECT b_agent_id FROM agent_links \
         WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? \
           AND status = 'approved' AND b_agent_id IN ({placeholders})"
    );

    let mut params: Vec<Value> = Vec::with_capacity(capped_ids.len() + 3);
    params.push(Value::BigInt(project_id));
    params.push(Value::BigInt(sender_id));
    params.push(Value::BigInt(project_id));
    for id in capped_ids {
        params.push(Value::BigInt(*id));
    }

    let rows_out = map_sql_outcome(traw_query(cx, &tracked, &sql, &params).await);
    match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: i64 = match row.get_named("b_agent_id") {
                    Ok(v) => v,
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                };
                out.push(id);
            }
            Outcome::Ok(out)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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
            let mut out = Vec::new();
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

    // Check if there's an approved link in either direction
    let link = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(from_project_id))
            .filter(Expr::col("a_agent_id").eq(from_agent_id))
            .filter(Expr::col("b_project_id").eq(to_project_id))
            .filter(Expr::col("b_agent_id").eq(to_agent_id))
            .filter(Expr::col("status").eq("approved"))
            .first(cx, &tracked)
            .await,
    );

    match link {
        Outcome::Ok(Some(ref row)) if link_is_valid(row) => return Outcome::Ok(true),
        Outcome::Ok(_) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // Check reverse direction
    let reverse_link = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(to_project_id))
            .filter(Expr::col("a_agent_id").eq(to_agent_id))
            .filter(Expr::col("b_project_id").eq(from_project_id))
            .filter(Expr::col("b_agent_id").eq(from_agent_id))
            .filter(Expr::col("status").eq("approved"))
            .first(cx, &tracked)
            .await,
    );

    match reverse_link {
        Outcome::Ok(Some(ref row)) if link_is_valid(row) => return Outcome::Ok(true),
        Outcome::Ok(_) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // Check if target agent has "open" or "auto" contact policy (allows all contacts)
    let target_agent = map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(to_project_id))
            .filter(Expr::col("id").eq(to_agent_id))
            .first(cx, &tracked)
            .await,
    );

    match target_agent {
        Outcome::Ok(Some(agent)) => {
            Outcome::Ok(matches!(agent.contact_policy.as_str(), "auto" | "open"))
        }
        Outcome::Ok(None) => Outcome::Ok(false),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// Product Queries
// =============================================================================

/// Ensure product exists, creating if necessary.
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

    // Check if product already exists
    let existing = map_sql_outcome(
        select!(ProductRow)
            .filter(Expr::col("product_uid").eq(uid.as_str()))
            .first(cx, &tracked)
            .await,
    );

    match existing {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => {
            let mut row = ProductRow {
                id: None,
                product_uid: uid,
                name: prod_name,
                created_at: now,
            };

            let id_out = map_sql_outcome(insert!(&row).execute(cx, &tracked).await);
            match id_out {
                Outcome::Ok(id) => {
                    row.id = Some(id);
                    Outcome::Ok(row)
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

    try_in_tx!(cx, &tracked, begin_immediate_tx(cx, &tracked).await);

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

    match map_sql_outcome(
        select!(ProductRow)
            .filter(Expr::col("product_uid").eq(product_uid))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Product", product_uid)),
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

    let sql = "UPDATE file_reservations SET released_ts = ? WHERE id = ? AND released_ts IS NULL";
    let params = [Value::BigInt(now), Value::BigInt(reservation_id)];

    let out = map_sql_outcome(traw_execute(cx, &tracked, sql, &params).await);
    match out {
        Outcome::Ok(n) => usize::try_from(n).map_or_else(
            |_| {
                Outcome::Err(DbError::invalid(
                    "row_count",
                    "row count exceeds usize::MAX",
                ))
            },
            Outcome::Ok,
        ),
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

    let sql = "SELECT p.* FROM projects p \
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

    // First, collect the IDs to be released.
    let select_sql =
        format!("SELECT id FROM file_reservations WHERE {EXPIRED_RESERVATIONS_WHERE_SQL}");
    let params = [Value::BigInt(project_id), Value::BigInt(now)];
    let ids = match map_sql_outcome(traw_query(cx, &tracked, &select_sql, &params).await) {
        Outcome::Ok(rows) => {
            let mut ids = Vec::with_capacity(rows.len());
            for row in rows {
                if let Ok(id) = row.get_named::<i64>("id") {
                    ids.push(id);
                }
            }
            ids
        }
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    if ids.is_empty() {
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
    match map_sql_outcome(traw_execute(cx, &tracked, &update_sql, &update_params).await) {
        Outcome::Ok(_) => Outcome::Ok(ids),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
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

    // Build parameterized IN clause.
    let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
    let sql = format!(
        "UPDATE file_reservations SET released_ts = ? WHERE id IN ({}) AND released_ts IS NULL",
        placeholders.join(",")
    );

    let mut params = Vec::with_capacity(1 + ids.len());
    params.push(Value::BigInt(now));
    for &id in ids {
        params.push(Value::BigInt(id));
    }

    let out = map_sql_outcome(traw_execute(cx, &tracked, &sql, &params).await);
    match out {
        Outcome::Ok(n) => usize::try_from(n).map_or_else(
            |_| {
                Outcome::Err(DbError::invalid(
                    "row_count",
                    "row count exceeds usize::MAX",
                ))
            },
            Outcome::Ok,
        ),
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

    // Check if already exists.
    match map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(project_id))
            .filter(Expr::col("name").eq(name))
            .first(cx, &tracked)
            .await,
    ) {
        Outcome::Ok(Some(row)) => return Outcome::Ok(row),
        Outcome::Ok(None) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    let mut row = AgentRow {
        id: None,
        project_id,
        name: name.to_string(),
        program: program.to_string(),
        model: model.to_string(),
        task_description: task_description.to_string(),
        inception_ts: now,
        last_active_ts: now,
        attachments_policy: "auto".to_string(),
        contact_policy: "auto".to_string(),
    };

    match map_sql_outcome(insert!(&row).execute(cx, &tracked).await) {
        Outcome::Ok(id) => {
            row.id = Some(id);
            Outcome::Ok(row)
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
    fn expired_reservations_where_clause_is_inclusive() {
        assert!(EXPIRED_RESERVATIONS_WHERE_SQL.contains("expires_ts <= ?"));
        assert!(!EXPIRED_RESERVATIONS_WHERE_SQL.contains("expires_ts < ?"));
    }
}
