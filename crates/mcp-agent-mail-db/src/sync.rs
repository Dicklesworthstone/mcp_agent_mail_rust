//! Synchronous database helpers.
//!
//! Exposes blocking DB queries used by UI loops and backgrounds threads
//! that cannot easily integrate with the async `sqlmodel_pool`.

use crate::error::DbError;
use crate::models::MessageRow;
use crate::queries::InboxRow;
use crate::DbConn;
use sqlmodel_core::Value;

const MAX_SYNC_IN_CLAUSE_ITEMS: usize = 500;

/// Synchronously update the thread ID of a message.
///
/// Returns `Ok(true)` if the thread ID was updated, `Ok(false)` if it was already the target ID.
/// Returns `Err` if the message was not found or if a database error occurred.
pub fn update_message_thread_id(
    conn: &DbConn,
    message_id: i64,
    target_thread_id: &str,
) -> Result<bool, DbError> {
    let target_thread_id = target_thread_id.trim();
    if target_thread_id.is_empty() {
        return Ok(false);
    }

    let lookup_sql = "SELECT thread_id FROM messages WHERE id = ? LIMIT 1";
    let rows = conn
        .query_sync(lookup_sql, &[Value::BigInt(message_id)])
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let mut row_iter = rows.into_iter();
    let row = row_iter.next().ok_or_else(|| DbError::NotFound {
        entity: "Message",
        identifier: message_id.to_string(),
    })?;

    let current_thread_id = row.get_named::<String>("thread_id").ok();

    if current_thread_id.as_deref() == Some(target_thread_id) {
        return Ok(false);
    }

    let update_sql = "UPDATE messages SET thread_id = ? WHERE id = ?";
    conn.execute_sync(
        update_sql,
        &[
            Value::Text(target_thread_id.to_string()),
            Value::BigInt(message_id),
        ],
    )
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    Ok(true)
}

/// Fetch inbox rows using native SQLite sync reads.
///
/// This bypasses the frankensqlite async/runtime path for inbox-style joins,
/// which is safer for operational mailbox reads and CLI fallback paths.
#[allow(clippy::too_many_arguments)]
pub fn fetch_inbox_native_sqlite_by_ids(
    sqlite_path: &str,
    project_id: i64,
    agent_id: i64,
    urgent_only: bool,
    unread_only: bool,
    ack_required_only: bool,
    since_ts: Option<i64>,
    limit: usize,
) -> Result<Vec<InboxRow>, DbError> {
    let conn = if sqlite_path == ":memory:" {
        sqlmodel_sqlite::SqliteConnection::open_memory()
    } else {
        sqlmodel_sqlite::SqliteConnection::open_file(sqlite_path)
    }
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let _ = conn.execute_raw("PRAGMA busy_timeout = 250");

    let mut sql = String::from(
        "SELECT m.id, m.project_id, m.sender_id, m.thread_id, m.subject, m.body_md, \
                m.importance, m.ack_required, m.created_ts, m.recipients_json, m.attachments, \
                r.kind, s.name AS sender_name, r.read_ts, r.ack_ts \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         JOIN agents s ON s.id = m.sender_id \
         WHERE r.agent_id = ? AND m.project_id = ?",
    );

    let mut params = vec![Value::BigInt(agent_id), Value::BigInt(project_id)];
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

    let limit_i64 =
        i64::try_from(limit).map_err(|_| DbError::invalid("limit", "limit exceeds i64::MAX"))?;
    sql.push_str(" ORDER BY m.created_ts DESC LIMIT ?");
    params.push(Value::BigInt(limit_i64));

    let rows = conn
        .query_sync(&sql, &params)
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row
            .get_named("id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let project_id: i64 = row
            .get_named("project_id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let sender_id: i64 = row
            .get_named("sender_id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let thread_id: Option<String> = row
            .get_named("thread_id")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let subject: String = row
            .get_named("subject")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let body_md: String = row
            .get_named("body_md")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let importance: String = row
            .get_named("importance")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let ack_required: i64 = row
            .get_named("ack_required")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let created_ts: i64 = row
            .get_named("created_ts")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let recipients_json: String = row
            .get_named("recipients_json")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let attachments: String = row
            .get_named("attachments")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let kind: String = row
            .get_named("kind")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let sender_name: String = row
            .get_named("sender_name")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let read_ts: Option<i64> = row
            .get_named("read_ts")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let ack_ts: Option<i64> = row
            .get_named("ack_ts")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;

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

    Ok(out)
}

fn open_sync_conn(sqlite_path: &str) -> Result<DbConn, DbError> {
    let conn = if sqlite_path == ":memory:" {
        DbConn::open_memory()
    } else {
        DbConn::open_file(sqlite_path.to_string())
    }
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let _ = conn.execute_raw("PRAGMA busy_timeout = 60000");
    Ok(conn)
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count).collect::<Vec<_>>().join(",")
}

fn is_missing_inbox_stats_table_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("no such table") && lowered.contains("inbox_stats")
}

fn rebuild_agent_inbox_stats_sync(conn: &DbConn, agent_id: i64) -> Result<(), DbError> {
    let params = [Value::BigInt(agent_id)];
    match conn.execute_sync("DELETE FROM inbox_stats WHERE agent_id = ?", &params) {
        Ok(_) => {}
        Err(err) => {
            let message = err.to_string();
            if is_missing_inbox_stats_table_error(&message) {
                return Ok(());
            }
            return Err(DbError::Sqlite(message));
        }
    }

    let rebuild_sql = "INSERT INTO inbox_stats \
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
    match conn.execute_sync(rebuild_sql, &params) {
        Ok(_) => Ok(()),
        Err(err) => {
            let message = err.to_string();
            if is_missing_inbox_stats_table_error(&message) {
                Ok(())
            } else {
                Err(DbError::Sqlite(message))
            }
        }
    }
}

fn mark_messages_read_batch_sync_conn(
    conn: &DbConn,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<(), DbError> {
    if message_ids.is_empty() {
        return Ok(());
    }

    let mut unique_message_ids = message_ids.to_vec();
    unique_message_ids.sort_unstable();
    unique_message_ids.dedup();

    begin_sync_write_tx(conn)?;

    let result = (|| -> Result<(), DbError> {
        let now = crate::now_micros();
        for chunk in unique_message_ids.chunks(MAX_SYNC_IN_CLAUSE_ITEMS) {
            let sql = format!(
                "UPDATE message_recipients \
                 SET read_ts = COALESCE(read_ts, ?), \
                     ack_ts = CASE \
                         WHEN ack_ts IS NOT NULL THEN ack_ts \
                         WHEN (SELECT m.ack_required FROM messages m \
                               WHERE m.id = message_recipients.message_id) = 1 THEN ? \
                         ELSE ack_ts \
                     END \
                 WHERE agent_id = ? AND message_id IN ({})",
                placeholders(chunk.len())
            );
            let mut params = Vec::with_capacity(3 + chunk.len());
            params.push(Value::BigInt(now));
            params.push(Value::BigInt(now));
            params.push(Value::BigInt(agent_id));
            for &message_id in chunk {
                params.push(Value::BigInt(message_id));
            }
            conn.execute_sync(&sql, &params)
                .map_err(|e| DbError::Sqlite(e.to_string()))?;
        }

        rebuild_agent_inbox_stats_sync(conn, agent_id)
    })();

    match result {
        Ok(()) => {
            commit_sync_write_tx(conn)?;
            Ok(())
        }
        Err(err) => {
            rollback_sync_write_tx(conn);
            Err(err)
        }
    }
}

/// Synchronously batch-mark multiple messages as read for a single agent.
///
/// This bypasses the async MVCC write path used by `fetch_inbox` follow-up
/// auto-read handling. It is intended for operational mailbox reads where a
/// direct SQLite transaction is more reliable than the pooled async path.
pub fn mark_messages_read_batch_sync(
    sqlite_path: &str,
    agent_id: i64,
    message_ids: &[i64],
) -> Result<(), DbError> {
    let conn = open_sync_conn(sqlite_path)?;
    let result = mark_messages_read_batch_sync_conn(&conn, agent_id, message_ids);
    crate::close_db_conn(conn, "mark_messages_read_batch_sync connection");
    result
}

fn begin_sync_write_tx(conn: &DbConn) -> Result<(), DbError> {
    conn.execute_sync("BEGIN IMMEDIATE", &[])
        .map(|_| ())
        .map_err(|e| DbError::Sqlite(e.to_string()))
}

fn commit_sync_write_tx(conn: &DbConn) -> Result<(), DbError> {
    conn.execute_sync("COMMIT", &[])
        .map(|_| ())
        .map_err(|e| DbError::Sqlite(e.to_string()))
}

fn rollback_sync_write_tx(conn: &DbConn) {
    let _ = conn.execute_sync("ROLLBACK", &[]);
}

fn is_agent_name_unique_violation(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("unique constraint failed")
        && normalized.contains("agents.project_id")
        && normalized.contains("agents.name")
}

fn lookup_agent_id_by_name(
    conn: &DbConn,
    project_id: i64,
    agent_name: &str,
) -> Result<Option<i64>, DbError> {
    let rows = conn
        .query_sync(
            "SELECT id FROM agents \
             WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE \
             ORDER BY id ASC LIMIT 1",
            &[
                Value::BigInt(project_id),
                Value::Text(agent_name.trim().to_string()),
            ],
        )
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.get_named::<i64>("id").ok()))
}

fn resolve_root_project_id(conn: &DbConn) -> Result<i64, DbError> {
    let project_row = conn
        .query_sync("SELECT id FROM projects ORDER BY id LIMIT 1", &[])
        .map_err(|e| DbError::Sqlite(e.to_string()))?
        .into_iter()
        .next();

    project_row
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| DbError::NotFound {
            entity: "Project",
            identifier: "any".into(),
        })
}

fn resolve_or_create_sender_id(
    conn: &DbConn,
    project_id: i64,
    sender_name: &str,
    now: i64,
) -> Result<i64, DbError> {
    if let Some(sender_id) = lookup_agent_id_by_name(conn, project_id, sender_name)? {
        return Ok(sender_id);
    }

    match conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) \
         VALUES (?1, ?2, 'tui-overseer', 'human', 'Human operator via TUI', ?3, ?4)",
        &[
            Value::BigInt(project_id),
            Value::Text(sender_name.trim().to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
        ],
    ) {
        Ok(_) => {}
        Err(err) => {
            let message = err.to_string();
            if is_agent_name_unique_violation(&message)
                && let Some(sender_id) = lookup_agent_id_by_name(conn, project_id, sender_name)?
            {
                return Ok(sender_id);
            }
            return Err(DbError::Sqlite(message));
        }
    }

    lookup_agent_id_by_name(conn, project_id, sender_name)?
        .ok_or_else(|| DbError::Internal("Failed to resolve sender ID after insert".into()))
}

struct RootMessageInput<'a> {
    subject: &'a str,
    body_md: &'a str,
    importance: &'a str,
    thread_id: Option<&'a str>,
}

fn insert_root_message(
    conn: &DbConn,
    project_id: i64,
    sender_id: i64,
    now: i64,
    message: &RootMessageInput<'_>,
) -> Result<i64, DbError> {
    let thread_id_val = message
        .thread_id
        .map(str::trim)
        .filter(|tid| !tid.is_empty())
        .map_or(Value::Null, |tid| Value::Text(tid.to_string()));

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(sender_id),
            Value::Text(message.subject.to_string()),
            Value::Text(message.body_md.to_string()),
            Value::Text(message.importance.to_string()),
            thread_id_val,
            Value::BigInt(now),
        ],
    )
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let msg_rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| DbError::Sqlite(e.to_string()))?;
    msg_rows
        .into_iter()
        .next()
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| DbError::Internal("Message insert returned no ID".into()))
}

fn insert_message_recipients(
    conn: &DbConn,
    project_id: i64,
    msg_id: i64,
    recipients: &[(String, String)],
) -> Result<(), DbError> {
    use std::collections::HashSet;

    let mut inserted_recipient_ids: HashSet<i64> = HashSet::new();
    let mut missing_names: Vec<String> = Vec::new();
    let mut missing_seen: HashSet<String> = HashSet::new();

    for (name, kind) in recipients {
        let Some(aid) = lookup_agent_id_by_name(conn, project_id, name)? else {
            let normalized = name.trim().to_ascii_lowercase();
            if missing_seen.insert(normalized) {
                missing_names.push(name.trim().to_string());
            }
            continue;
        };

        if inserted_recipient_ids.insert(aid) {
            conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, ?3)",
                &[
                    Value::BigInt(msg_id),
                    Value::BigInt(aid),
                    Value::Text(kind.clone()),
                ],
            )
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        }
    }

    if !missing_names.is_empty() {
        return Err(DbError::not_found(
            "Agent",
            format!(
                "unknown recipients in project {project_id}: {}",
                missing_names.join(", ")
            ),
        ));
    }

    Ok(())
}

fn sync_message_recipients_json(conn: &DbConn, msg_id: i64) -> Result<(), DbError> {
    let rows = conn
        .query_sync(
            "SELECT a.name AS name, mr.kind AS kind \
             FROM message_recipients mr \
             JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? \
             ORDER BY CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END, \
                      a.name COLLATE NOCASE",
            &[Value::BigInt(msg_id)],
        )
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let recipients_json = rows
        .into_iter()
        .map(|row| {
            let name = row
                .get_named::<String>("name")
                .map_err(|e| DbError::Sqlite(e.to_string()))?;
            let kind = row
                .get_named::<String>("kind")
                .map_err(|e| DbError::Sqlite(e.to_string()))?;
            Ok(serde_json::json!({
                "name": name,
                "kind": kind,
            }))
        })
        .collect::<Result<Vec<_>, DbError>>()
        .and_then(|payload| {
            serde_json::to_string(&payload)
                .map_err(|e| DbError::Internal(format!("failed to encode recipients JSON: {e}")))
        })?;

    conn.execute_sync(
        "UPDATE messages SET recipients_json = ? WHERE id = ?",
        &[Value::Text(recipients_json), Value::BigInt(msg_id)],
    )
    .map(|_| ())
    .map_err(|e| DbError::Sqlite(e.to_string()))
}

/// Dispatch a message from the first available project (TUI context).
///
/// Handles project resolution, sender auto-registration (for overseer),
/// message insertion, and recipient linking in a single transaction.
pub fn dispatch_root_message(
    conn: &DbConn,
    sender_name: &str,
    subject: &str,
    body_md: &str,
    importance: &str,
    thread_id: Option<&str>,
    recipients: &[(String, String)], // (name, kind)
) -> Result<i64, DbError> {
    use crate::timestamps::now_micros;

    let project_id = resolve_root_project_id(conn)?;
    begin_sync_write_tx(conn)?;

    let dispatch_result = (|| -> Result<i64, DbError> {
        let now = now_micros();
        let sender_id = resolve_or_create_sender_id(conn, project_id, sender_name, now)?;
        let message_input = RootMessageInput {
            subject,
            body_md,
            importance,
            thread_id,
        };
        let msg_id = insert_root_message(conn, project_id, sender_id, now, &message_input)?;
        insert_message_recipients(conn, project_id, msg_id, recipients)?;
        sync_message_recipients_json(conn, msg_id)?;
        Ok(msg_id)
    })();

    match dispatch_result {
        Ok(msg_id) => {
            commit_sync_write_tx(conn)?;
            Ok(msg_id)
        }
        Err(err) => {
            rollback_sync_write_tx(conn);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    /// Helper: open an in-memory DB with the full schema applied.
    fn test_conn() -> DbConn {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
            .expect("apply PRAGMAs");
        let init_sql = schema::init_schema_sql_base();
        conn.execute_raw(&init_sql).expect("init schema");
        conn
    }

    /// Insert a project and return its id.
    fn insert_project(conn: &DbConn) -> i64 {
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES ('test', '/tmp/test', 1000000)",
            &[],
        )
        .expect("insert project");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("query last id")
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .expect("get project id")
    }

    /// Insert an agent and return its id.
    fn insert_agent(conn: &DbConn, project_id: i64, name: &str) -> i64 {
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (?1, ?2, 'test', 'test', 'test', 1000000, 1000000)",
            &[Value::BigInt(project_id), Value::Text(name.to_string())],
        )
        .expect("insert agent");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("query last id")
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .expect("get agent id")
    }

    /// Insert a message and return its id.
    fn insert_message(conn: &DbConn, project_id: i64, sender_id: i64, thread_id: &str) -> i64 {
        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts) \
             VALUES (?1, ?2, 'test subject', 'test body', 'normal', 0, ?3, 1000000)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                Value::Text(thread_id.to_string()),
            ],
        )
        .expect("insert message");
        conn.query_sync("SELECT last_insert_rowid() AS id", &[])
            .expect("query last id")
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .expect("get message id")
    }

    // ── update_message_thread_id tests ───────────────────────────────

    #[test]
    fn update_thread_id_empty_target_returns_false() {
        let conn = test_conn();
        assert!(!update_message_thread_id(&conn, 1, "").unwrap());
        assert!(!update_message_thread_id(&conn, 1, "   ").unwrap());
    }

    #[test]
    fn update_thread_id_nonexistent_message_returns_not_found() {
        let conn = test_conn();
        let err = update_message_thread_id(&conn, 99999, "new-thread").unwrap_err();
        assert!(
            matches!(
                err,
                DbError::NotFound {
                    entity: "Message",
                    ..
                }
            ),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn update_thread_id_same_value_returns_false() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let aid = insert_agent(&conn, pid, "TestAgent");
        let mid = insert_message(&conn, pid, aid, "original-thread");

        let result = update_message_thread_id(&conn, mid, "original-thread").unwrap();
        assert!(
            !result,
            "should return false when thread_id is already the target"
        );
    }

    #[test]
    fn update_thread_id_different_value_returns_true() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let aid = insert_agent(&conn, pid, "TestAgent");
        let mid = insert_message(&conn, pid, aid, "old-thread");

        let result = update_message_thread_id(&conn, mid, "new-thread").unwrap();
        assert!(result, "should return true when thread_id changes");

        // Verify the update persisted
        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(mid)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "new-thread");
    }

    #[test]
    fn update_thread_id_trims_whitespace() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let aid = insert_agent(&conn, pid, "TestAgent");
        let mid = insert_message(&conn, pid, aid, "old");

        let result = update_message_thread_id(&conn, mid, "  new-thread  ").unwrap();
        assert!(result);

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(mid)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "new-thread");
    }

    // ── dispatch_root_message tests ──────────────────────────────────

    #[test]
    fn dispatch_root_message_no_project_returns_not_found() {
        let conn = test_conn();
        let err = dispatch_root_message(&conn, "SomeAgent", "Hello", "Body", "normal", None, &[])
            .unwrap_err();
        assert!(
            matches!(
                err,
                DbError::NotFound {
                    entity: "Project",
                    ..
                }
            ),
            "expected Project NotFound, got {err:?}"
        );
    }

    #[test]
    fn dispatch_root_message_auto_registers_sender() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        // NewAgent doesn't exist yet — dispatch should auto-register
        let msg_id = dispatch_root_message(
            &conn,
            "NewAgent",
            "Auto-register test",
            "Should auto-register the sender",
            "normal",
            None,
            &[],
        )
        .unwrap();

        assert!(msg_id > 0);

        // Verify agent was created
        let rows = conn
            .query_sync(
                "SELECT name, program FROM agents WHERE name = 'NewAgent'",
                &[],
            )
            .unwrap();
        let row = rows.into_iter().next().expect("agent should exist");
        assert_eq!(row.get_named::<String>("program").unwrap(), "tui-overseer");
    }

    #[test]
    fn dispatch_root_message_uses_existing_sender() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _aid = insert_agent(&conn, pid, "ExistingAgent");

        let msg_id = dispatch_root_message(
            &conn,
            "ExistingAgent",
            "Existing agent test",
            "Body",
            "high",
            Some("thread-123"),
            &[],
        )
        .unwrap();

        assert!(msg_id > 0);

        // Verify only one agent with that name
        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM agents WHERE name = 'ExistingAgent'",
                &[],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(cnt, 1, "should not create duplicate agent");
    }

    #[test]
    fn dispatch_root_message_with_thread_id() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id = dispatch_root_message(
            &conn,
            "Agent",
            "Thread test",
            "Body",
            "normal",
            Some("br-42"),
            &[],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "br-42");
    }

    #[test]
    fn dispatch_root_message_trims_thread_id() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id = dispatch_root_message(
            &conn,
            "Agent",
            "Thread trim test",
            "Body",
            "normal",
            Some("  br-100  "),
            &[],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let thread_id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("thread_id").ok())
            .unwrap();
        assert_eq!(thread_id, "br-100");
    }

    #[test]
    fn dispatch_root_message_without_thread_id() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id =
            dispatch_root_message(&conn, "Agent", "No thread", "Body", "normal", None, &[])
                .unwrap();

        let rows = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let row = rows.into_iter().next().expect("message should exist");
        // thread_id should be NULL
        assert!(row.get_named::<String>("thread_id").is_err());
    }

    #[test]
    fn dispatch_root_message_links_recipients() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let _r1 = insert_agent(&conn, pid, "Recipient1");
        let _r2 = insert_agent(&conn, pid, "Recipient2");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Multi-recipient",
            "Body",
            "normal",
            None,
            &[
                ("Recipient1".to_string(), "to".to_string()),
                ("Recipient2".to_string(), "cc".to_string()),
            ],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM message_recipients WHERE message_id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(cnt, 2, "should have 2 recipients");

        let message_rows = conn
            .query_sync(
                "SELECT recipients_json FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let recipients_json = message_rows
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<String>("recipients_json").ok())
            .unwrap();
        assert!(recipients_json.contains("Recipient1"));
        assert!(recipients_json.contains("Recipient2"));
    }

    #[test]
    fn dispatch_root_message_duplicate_recipient_inserted_once() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let _r1 = insert_agent(&conn, pid, "Recipient1");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Duplicate recipient",
            "Body",
            "normal",
            None,
            &[
                ("Recipient1".to_string(), "to".to_string()),
                ("Recipient1".to_string(), "cc".to_string()),
            ],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT kind FROM message_recipients WHERE message_id = ? ORDER BY kind",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "duplicate recipients should be de-duplicated"
        );
        let kind = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("kind").ok())
            .unwrap();
        assert_eq!(kind, "to", "first occurrence should win");
    }

    #[test]
    fn dispatch_root_message_reuses_sender_case_insensitively() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "BlueLake");

        let msg_id = dispatch_root_message(
            &conn,
            "bluelake",
            "Sender case fold",
            "Body",
            "normal",
            None,
            &[],
        )
        .unwrap();

        let sender_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM agents \
                 WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE",
                &[Value::BigInt(pid), Value::Text("BlueLake".to_string())],
            )
            .unwrap();
        let sender_count = sender_rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(sender_count, 1, "sender lookup should be case-insensitive");

        let msg_rows = conn
            .query_sync(
                "SELECT sender_id FROM messages WHERE id = ?1",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let actual_sender_id = msg_rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("sender_id").ok())
            .unwrap();
        assert_eq!(actual_sender_id, sender_id);
    }

    #[test]
    fn dispatch_root_message_resolves_recipients_case_insensitively() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "BlueLake");

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Recipient case fold",
            "Body",
            "normal",
            None,
            &[("bluelake".to_string(), "to".to_string())],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT agent_id, kind FROM message_recipients WHERE message_id = ?1",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let row = rows.into_iter().next().expect("recipient row should exist");
        assert_eq!(row.get_named::<i64>("agent_id").unwrap(), recipient_id);
        assert_eq!(row.get_named::<String>("kind").unwrap(), "to");
    }

    #[test]
    fn dispatch_root_message_unknown_recipient_returns_not_found_and_rolls_back() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let err = dispatch_root_message(
            &conn,
            "Sender",
            "Unknown recipient",
            "Body",
            "normal",
            None,
            &[("NonexistentAgent".to_string(), "to".to_string())],
        )
        .expect_err("unknown recipient should fail");

        assert!(
            matches!(
                err,
                DbError::NotFound {
                    entity: "Agent",
                    ..
                }
            ),
            "expected agent not found, got {err:?}"
        );

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE subject = 'Unknown recipient'",
                &[],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(
            cnt, 0,
            "message insert should roll back on unknown recipient"
        );
    }

    #[test]
    fn dispatch_root_message_recipient_insert_error_rolls_back_message() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let _sender = insert_agent(&conn, pid, "Sender");
        let _recipient = insert_agent(&conn, pid, "Recipient1");

        conn.execute_raw(
            "CREATE TRIGGER fail_message_recipient_insert \
             BEFORE INSERT ON message_recipients \
             BEGIN \
                 SELECT RAISE(ABORT, 'forced recipient insert failure'); \
             END;",
        )
        .expect("install failing recipient trigger");

        let err = dispatch_root_message(
            &conn,
            "Sender",
            "Rollback recipient error",
            "Body",
            "normal",
            None,
            &[("Recipient1".to_string(), "to".to_string())],
        )
        .expect_err("recipient insert should fail when table is missing");
        assert!(matches!(err, DbError::Sqlite(_)));

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS cnt FROM messages WHERE subject = 'Rollback recipient error'",
                &[],
            )
            .unwrap();
        let cnt = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("cnt").ok())
            .unwrap();
        assert_eq!(
            cnt, 0,
            "message insert should roll back on recipient failure"
        );
    }

    #[test]
    fn dispatch_root_message_stores_importance() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id =
            dispatch_root_message(&conn, "Agent", "Urgent", "Body", "urgent", None, &[]).unwrap();

        let rows = conn
            .query_sync(
                "SELECT importance FROM messages WHERE id = ?",
                &[Value::BigInt(msg_id)],
            )
            .unwrap();
        let importance = rows
            .into_iter()
            .next()
            .and_then(|r| r.get_named::<String>("importance").ok())
            .unwrap();
        assert_eq!(importance, "urgent");
    }

    #[test]
    fn mark_messages_read_batch_sync_updates_rows_and_inbox_stats() {
        let conn = test_conn();
        let pid = insert_project(&conn);
        let sender_id = insert_agent(&conn, pid, "Sender");
        let recipient_id = insert_agent(&conn, pid, "Recipient");

        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts, recipients_json, attachments) \
             VALUES (?1, ?2, 'ack', 'body', 'normal', 1, NULL, 1000001, '[]', '[]')",
            &[Value::BigInt(pid), Value::BigInt(sender_id)],
        )
        .unwrap();
        let ack_message_id = conn
            .query_sync("SELECT last_insert_rowid() AS id", &[])
            .unwrap()
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("id").ok())
            .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[Value::BigInt(ack_message_id), Value::BigInt(recipient_id)],
        )
        .unwrap();

        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts, recipients_json, attachments) \
             VALUES (?1, ?2, 'plain', 'body', 'normal', 0, NULL, 1000002, '[]', '[]')",
            &[Value::BigInt(pid), Value::BigInt(sender_id)],
        )
        .unwrap();
        let plain_message_id = conn
            .query_sync("SELECT last_insert_rowid() AS id", &[])
            .unwrap()
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("id").ok())
            .unwrap();
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, 'to')",
            &[Value::BigInt(plain_message_id), Value::BigInt(recipient_id)],
        )
        .unwrap();

        mark_messages_read_batch_sync_conn(
            &conn,
            recipient_id,
            &[plain_message_id, ack_message_id, ack_message_id],
        )
        .unwrap();

        let rows = conn
            .query_sync(
                "SELECT message_id, read_ts, ack_ts FROM message_recipients \
                 WHERE agent_id = ?1 ORDER BY message_id",
                &[Value::BigInt(recipient_id)],
            )
            .unwrap();
        assert_eq!(rows.len(), 2, "expected two recipient rows");

        let first = &rows[0];
        let second = &rows[1];
        let first_message_id = first.get_named::<i64>("message_id").unwrap();
        let second_message_id = second.get_named::<i64>("message_id").unwrap();

        for row in [first, second] {
            assert!(
                row.get_named::<i64>("read_ts").is_ok(),
                "read_ts should be populated after sync batch mark-read"
            );
        }

        let ack_row = if first_message_id == ack_message_id {
            first
        } else {
            second
        };
        let plain_row = if second_message_id == plain_message_id {
            second
        } else {
            first
        };
        assert!(
            ack_row.get_named::<i64>("ack_ts").is_ok(),
            "ack_required message should auto-ack on read"
        );
        assert!(
            plain_row.get_named::<i64>("ack_ts").is_err(),
            "non-ack-required message should keep ack_ts NULL"
        );

        let stats_row = conn
            .query_sync(
                "SELECT total_count, unread_count, ack_pending_count FROM inbox_stats WHERE agent_id = ?1",
                &[Value::BigInt(recipient_id)],
            )
            .unwrap()
            .into_iter()
            .next()
            .expect("inbox_stats row should exist");
        assert_eq!(stats_row.get_named::<i64>("total_count").unwrap(), 2);
        assert_eq!(stats_row.get_named::<i64>("unread_count").unwrap(), 0);
        assert_eq!(stats_row.get_named::<i64>("ack_pending_count").unwrap(), 0);
    }
}
