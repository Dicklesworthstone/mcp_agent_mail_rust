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
    AgentLinkRow, AgentRow, FileReservationRow, MessageRecipientRow, MessageRow, ProductRow,
    ProjectRow,
};
use crate::pool::DbPool;
use crate::timestamps::now_micros;
use asupersync::Outcome;
use sqlmodel::prelude::*;
use sqlmodel_core::Error as SqlError;
use sqlmodel_core::Value;
use sqlmodel_query::{raw_execute, raw_query};

// =============================================================================
// Tracked query wrappers
// =============================================================================

/// Execute a raw query with timing instrumentation.
async fn traw_query(
    cx: &Cx,
    conn: &sqlmodel_sqlite::SqliteConnection,
    sql: &str,
    params: &[Value],
) -> Outcome<Vec<sqlmodel_core::Row>, SqlError> {
    let start = crate::tracking::query_timer();
    let result = raw_query(cx, conn, sql, params).await;
    let elapsed = crate::tracking::elapsed_us(start);
    crate::QUERY_TRACKER.record(sql, elapsed);
    result
}

/// Execute a raw statement with timing instrumentation.
async fn traw_execute(
    cx: &Cx,
    conn: &sqlmodel_sqlite::SqliteConnection,
    sql: &str,
    params: &[Value],
) -> Outcome<u64, SqlError> {
    let start = crate::tracking::query_timer();
    let result = raw_execute(cx, conn, sql, params).await;
    let elapsed = crate::tracking::elapsed_us(start);
    crate::QUERY_TRACKER.record(sql, elapsed);
    result
}

// =============================================================================
// Project Queries
// =============================================================================

/// Generate a URL-safe slug from a human key (path).
#[must_use]
pub fn generate_slug(human_key: &str) -> String {
    human_key
        .trim_start_matches('/')
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
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

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

async fn acquire_conn(
    cx: &Cx,
    pool: &DbPool,
) -> Outcome<sqlmodel_pool::PooledConnection<sqlmodel_sqlite::SqliteConnection>, DbError> {
    map_sql_outcome(pool.acquire(cx).await)
}

/// Ensure a project exists, creating if necessary.
///
/// Returns the project row (existing or newly created).
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

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    // Match legacy semantics: slug is the stable identity; `human_key` is informative.
    let existing = map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("slug").eq(slug.as_str()))
            .first(cx, &*conn)
            .await,
    );
    match existing {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => {
            let mut row = ProjectRow::new(slug, human_key.to_string());
            let id_out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
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

/// Get project by slug
pub async fn get_project_by_slug(
    cx: &Cx,
    pool: &DbPool,
    slug: &str,
) -> Outcome<ProjectRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    match map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("slug").eq(slug))
            .first(cx, &*conn)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", slug)),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get project by `human_key`
pub async fn get_project_by_human_key(
    cx: &Cx,
    pool: &DbPool,
    human_key: &str,
) -> Outcome<ProjectRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    match map_sql_outcome(
        select!(ProjectRow)
            .filter(Expr::col("human_key").eq(human_key))
            .first(cx, &*conn)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => Outcome::Err(DbError::not_found("Project", human_key)),
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

    map_sql_outcome(select!(ProjectRow).all(cx, &*conn).await)
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

    // Check for existing agent (project_id, name) unique.
    let existing = map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(project_id))
            .filter(Expr::col("name").eq(name))
            .first(cx, &*conn)
            .await,
    );

    match existing {
        Outcome::Ok(Some(mut row)) => {
            row.program = program.to_string();
            row.model = model.to_string();
            row.task_description = task_description.unwrap_or_default().to_string();
            row.last_active_ts = now;
            row.attachments_policy = attachments_policy.unwrap_or("auto").to_string();

            // Keep inception_ts stable.
            let updated = map_sql_outcome(update!(&row).execute(cx, &*conn).await);
            match updated {
                Outcome::Ok(_) => Outcome::Ok(row),
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Ok(None) => {
            let mut row = AgentRow {
                id: None,
                project_id,
                name: name.to_string(),
                program: program.to_string(),
                model: model.to_string(),
                task_description: task_description.unwrap_or_default().to_string(),
                inception_ts: now,
                last_active_ts: now,
                attachments_policy: attachments_policy.unwrap_or("auto").to_string(),
                contact_policy: "auto".to_string(),
            };

            let id_out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
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

/// Get agent by project and name
pub async fn get_agent(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    name: &str,
) -> Outcome<AgentRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    match map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(project_id))
            .filter(Expr::col("name").eq(name))
            .first(cx, &*conn)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
        Outcome::Ok(None) => {
            Outcome::Err(DbError::not_found("Agent", format!("{project_id}:{name}")))
        }
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
    }
}

/// Get agent by id.
pub async fn get_agent_by_id(cx: &Cx, pool: &DbPool, agent_id: i64) -> Outcome<AgentRow, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    match map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("id").eq(agent_id))
            .first(cx, &*conn)
            .await,
    ) {
        Outcome::Ok(Some(row)) => Outcome::Ok(row),
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

    map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(project_id))
            .all(cx, &*conn)
            .await,
    )
}

/// Update agent's `last_active_ts`
pub async fn touch_agent(cx: &Cx, pool: &DbPool, agent_id: i64) -> Outcome<(), DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let now = now_micros();
    let sql = "UPDATE agents SET last_active_ts = ? WHERE id = ?";
    let params = [Value::BigInt(now), Value::BigInt(agent_id)];
    match map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await) {
        Outcome::Ok(_) => Outcome::Ok(()),
        Outcome::Err(e) => Outcome::Err(e),
        Outcome::Cancelled(r) => Outcome::Cancelled(r),
        Outcome::Panicked(p) => Outcome::Panicked(p),
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

    let now = now_micros();
    let sql = "UPDATE agents SET contact_policy = ?, last_active_ts = ? WHERE id = ?";
    let params = [
        Value::Text(policy.to_string()),
        Value::BigInt(now),
        Value::BigInt(agent_id),
    ];
    let out = map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await);

    match out {
        Outcome::Ok(_) => {
            // Fetch updated agent
            match map_sql_outcome(
                select!(AgentRow)
                    .filter(Expr::col("id").eq(agent_id))
                    .first(cx, &*conn)
                    .await,
            ) {
                Outcome::Ok(Some(row)) => Outcome::Ok(row),
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

    let id_out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
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

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, &sql, &params).await);
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

    let placeholders = placeholders(message_ids.len());
    let sql = format!(
        "SELECT DISTINCT a.name \
         FROM message_recipients r \
         JOIN agents a ON a.id = r.agent_id \
         JOIN messages m ON m.id = r.message_id \
         WHERE m.project_id = ? AND r.message_id IN ({placeholders})"
    );

    let mut params: Vec<Value> = Vec::with_capacity(message_ids.len() + 1);
    params.push(Value::BigInt(project_id));
    for id in message_ids {
        params.push(Value::BigInt(*id));
    }

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, &sql, &params).await);
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

    match map_sql_outcome(
        select!(MessageRow)
            .filter(Expr::col("id").eq(message_id))
            .first(cx, &*conn)
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

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, &sql, &params).await);
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

    // Minimal parity implementation: LIKE on subject/body; deterministic order by id ASC.
    let like = format!("%{query}%");
    let sql = "SELECT m.id, m.subject, m.importance, m.ack_required, m.created_ts, m.thread_id, a.name as from_name \
               FROM messages m \
               JOIN agents a ON a.id = m.sender_id \
               WHERE m.project_id = ? AND (m.subject LIKE ? OR m.body_md LIKE ?) \
               ORDER BY m.id ASC \
               LIMIT ?";
    let Ok(limit_i64) = i64::try_from(limit) else {
        return Outcome::Err(DbError::invalid("limit", "limit exceeds i64::MAX"));
    };
    let params = [
        Value::BigInt(project_id),
        Value::Text(like.clone()),
        Value::Text(like),
        Value::BigInt(limit_i64),
    ];

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, sql, &params).await);
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

    for (agent_id, kind) in recipients {
        let row = MessageRecipientRow {
            message_id,
            agent_id: *agent_id,
            kind: (*kind).to_string(),
            read_ts: None,
            ack_ts: None,
        };

        let out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
        match out {
            Outcome::Ok(_) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

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

    // Idempotent: only set read_ts if currently NULL.
    let sql = "UPDATE message_recipients SET read_ts = COALESCE(read_ts, ?) WHERE agent_id = ? AND message_id = ?";
    let params = [
        Value::BigInt(now),
        Value::BigInt(agent_id),
        Value::BigInt(message_id),
    ];
    let out = map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await);
    match out {
        Outcome::Ok(rows) => {
            if rows == 0 {
                Outcome::Err(DbError::not_found(
                    "MessageRecipient",
                    format!("{agent_id}:{message_id}"),
                ))
            } else {
                Outcome::Ok(now)
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
    let out = map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await);
    match out {
        Outcome::Ok(rows) => {
            if rows == 0 {
                Outcome::Err(DbError::not_found(
                    "MessageRecipient",
                    format!("{agent_id}:{message_id}"),
                ))
            } else {
                Outcome::Ok((now, now))
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
    let expires = now + (ttl_seconds * 1_000_000);

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

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

        let id_out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
        match id_out {
            Outcome::Ok(id) => {
                row.id = Some(id);
                out.push(row);
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

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

    map_sql_outcome(
        select!(FileReservationRow)
            .filter(Expr::col("project_id").eq(project_id))
            .filter(Expr::col("released_ts").is_null())
            .filter(Expr::col("expires_ts").gt(now))
            .all(cx, &*conn)
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

    let out = map_sql_outcome(traw_execute(cx, &*conn, &sql, &params).await);
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
    let extend = extend_seconds * 1_000_000;

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

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

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, &sql, &params).await);
    let mut reservations: Vec<FileReservationRow> = match rows_out {
        Outcome::Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                match FileReservationRow::from_row(&r) {
                    Ok(row) => out.push(row),
                    Err(e) => return Outcome::Err(map_sql_error(&e)),
                }
            }
            out
        }
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    for row in &mut reservations {
        let base = row.expires_ts.max(now);
        row.expires_ts = base + extend;
        let Some(id) = row.id else {
            return Outcome::Err(DbError::Internal(
                "renew_reservations: expected id to be populated".to_string(),
            ));
        };

        let sql = "UPDATE file_reservations SET expires_ts = ? WHERE id = ?";
        let params = [Value::BigInt(row.expires_ts), Value::BigInt(id)];
        let updated = map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await);
        match updated {
            Outcome::Ok(_) => {}
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

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

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, &sql, &params).await);
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
        Some(now + (ttl_seconds * 1_000_000))
    } else {
        None
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    // Try existing link first (unique on (a_project_id, a_agent_id, b_project_id, b_agent_id)).
    let existing = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(from_project_id))
            .filter(Expr::col("a_agent_id").eq(from_agent_id))
            .filter(Expr::col("b_project_id").eq(to_project_id))
            .filter(Expr::col("b_agent_id").eq(to_agent_id))
            .first(cx, &*conn)
            .await,
    );

    match existing {
        Outcome::Ok(Some(mut row)) => {
            row.status = "pending".to_string();
            row.reason = reason.to_string();
            row.updated_ts = now;
            row.expires_ts = expires;
            let out = map_sql_outcome(update!(&row).execute(cx, &*conn).await);
            match out {
                Outcome::Ok(_) => Outcome::Ok(row),
                Outcome::Err(e) => Outcome::Err(e),
                Outcome::Cancelled(r) => Outcome::Cancelled(r),
                Outcome::Panicked(p) => Outcome::Panicked(p),
            }
        }
        Outcome::Ok(None) => {
            let mut row = AgentLinkRow {
                id: None,
                a_project_id: from_project_id,
                a_agent_id: from_agent_id,
                b_project_id: to_project_id,
                b_agent_id: to_agent_id,
                status: "pending".to_string(),
                reason: reason.to_string(),
                created_ts: now,
                updated_ts: now,
                expires_ts: expires,
            };
            let id_out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
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
        Some(now + (ttl_seconds * 1_000_000))
    } else {
        None
    };

    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    let existing = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(from_project_id))
            .filter(Expr::col("a_agent_id").eq(from_agent_id))
            .filter(Expr::col("b_project_id").eq(to_project_id))
            .filter(Expr::col("b_agent_id").eq(to_agent_id))
            .first(cx, &*conn)
            .await,
    );

    match existing {
        Outcome::Ok(Some(mut row)) => {
            row.status = status.to_string();
            row.updated_ts = now;
            row.expires_ts = expires;
            let out = map_sql_outcome(update!(&row).execute(cx, &*conn).await);
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

    // Outgoing: links where this agent is "a" side
    let outgoing = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(project_id))
            .filter(Expr::col("a_agent_id").eq(agent_id))
            .all(cx, &*conn)
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
            .all(cx, &*conn)
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

    let placeholders = placeholders(candidate_ids.len());
    let sql = format!(
        "SELECT b_agent_id FROM agent_links \
         WHERE a_project_id = ? AND a_agent_id = ? AND b_project_id = ? \
           AND status = 'approved' AND b_agent_id IN ({placeholders})"
    );

    let mut params: Vec<Value> = Vec::with_capacity(candidate_ids.len() + 3);
    params.push(Value::BigInt(project_id));
    params.push(Value::BigInt(sender_id));
    params.push(Value::BigInt(project_id));
    for id in candidate_ids {
        params.push(Value::BigInt(*id));
    }

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, &sql, &params).await);
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

    let placeholders = placeholders(candidate_ids.len());
    let sql_sent = format!(
        "SELECT DISTINCT r.agent_id \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         WHERE m.project_id = ? AND m.sender_id = ? AND m.created_ts > ? \
           AND r.agent_id IN ({placeholders})"
    );
    let mut params_sent: Vec<Value> = Vec::with_capacity(candidate_ids.len() + 3);
    params_sent.push(Value::BigInt(project_id));
    params_sent.push(Value::BigInt(sender_id));
    params_sent.push(Value::BigInt(since_ts));
    for id in candidate_ids {
        params_sent.push(Value::BigInt(*id));
    }

    let sql_recv = format!(
        "SELECT DISTINCT m.sender_id \
         FROM messages m \
         JOIN message_recipients r ON r.message_id = m.id \
         WHERE m.project_id = ? AND r.agent_id = ? AND m.created_ts > ? \
           AND m.sender_id IN ({placeholders})"
    );
    let mut params_recv: Vec<Value> = Vec::with_capacity(candidate_ids.len() + 3);
    params_recv.push(Value::BigInt(project_id));
    params_recv.push(Value::BigInt(sender_id));
    params_recv.push(Value::BigInt(since_ts));
    for id in candidate_ids {
        params_recv.push(Value::BigInt(*id));
    }

    let sent_rows = map_sql_outcome(traw_query(cx, &*conn, &sql_sent, &params_sent).await);
    let recv_rows = map_sql_outcome(traw_query(cx, &*conn, &sql_recv, &params_recv).await);

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
/// Returns true if there's an approved link or if both agents have `auto` contact policy.
pub async fn is_contact_allowed(
    cx: &Cx,
    pool: &DbPool,
    from_project_id: i64,
    from_agent_id: i64,
    to_project_id: i64,
    to_agent_id: i64,
) -> Outcome<bool, DbError> {
    let conn = match acquire_conn(cx, pool).await {
        Outcome::Ok(c) => c,
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    };

    // Check if there's an approved link in either direction
    let link = map_sql_outcome(
        select!(AgentLinkRow)
            .filter(Expr::col("a_project_id").eq(from_project_id))
            .filter(Expr::col("a_agent_id").eq(from_agent_id))
            .filter(Expr::col("b_project_id").eq(to_project_id))
            .filter(Expr::col("b_agent_id").eq(to_agent_id))
            .filter(Expr::col("status").eq("approved"))
            .first(cx, &*conn)
            .await,
    );

    match link {
        Outcome::Ok(Some(_)) => return Outcome::Ok(true),
        Outcome::Ok(None) => {}
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
            .first(cx, &*conn)
            .await,
    );

    match reverse_link {
        Outcome::Ok(Some(_)) => return Outcome::Ok(true),
        Outcome::Ok(None) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }

    // Check if target agent has "auto" contact policy (allows all contacts)
    let target_agent = map_sql_outcome(
        select!(AgentRow)
            .filter(Expr::col("project_id").eq(to_project_id))
            .filter(Expr::col("id").eq(to_agent_id))
            .first(cx, &*conn)
            .await,
    );

    match target_agent {
        Outcome::Ok(Some(agent)) => Outcome::Ok(agent.contact_policy == "auto"),
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

    // Check if product already exists
    let existing = map_sql_outcome(
        select!(ProductRow)
            .filter(Expr::col("product_uid").eq(uid.as_str()))
            .first(cx, &*conn)
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

            let id_out = map_sql_outcome(insert!(&row).execute(cx, &*conn).await);
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
        let out = map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await);

        match out {
            Outcome::Ok(n) => {
                if n > 0 {
                    linked += 1;
                }
            }
            Outcome::Err(e) => return Outcome::Err(e),
            Outcome::Cancelled(r) => return Outcome::Cancelled(r),
            Outcome::Panicked(p) => return Outcome::Panicked(p),
        }
    }

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

    match map_sql_outcome(
        select!(ProductRow)
            .filter(Expr::col("product_uid").eq(product_uid))
            .first(cx, &*conn)
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

    let sql = "UPDATE file_reservations SET released_ts = ? WHERE id = ? AND released_ts IS NULL";
    let params = [Value::BigInt(now), Value::BigInt(reservation_id)];

    let out = map_sql_outcome(traw_execute(cx, &*conn, sql, &params).await);
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

    // Check messages sent
    let sql_sent =
        "SELECT MAX(created_ts) as max_ts FROM messages WHERE sender_id = ? AND project_id = ?";
    let params = [Value::BigInt(agent_id), Value::BigInt(project_id)];
    let sent_ts = match map_sql_outcome(traw_query(cx, &*conn, sql_sent, &params).await) {
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
    let (read_ts, ack_ts) = match map_sql_outcome(traw_query(cx, &*conn, sql_read, &params2).await)
    {
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

    let sql = "SELECT p.* FROM projects p \
               JOIN product_project_links ppl ON ppl.project_id = p.id \
               WHERE ppl.product_id = ?";
    let params = [Value::BigInt(product_id)];

    let rows_out = map_sql_outcome(traw_query(cx, &*conn, sql, &params).await);
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
