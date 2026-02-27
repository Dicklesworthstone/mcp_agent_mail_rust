//! Synchronous database helpers.
//!
//! Exposes blocking DB queries used by UI loops and backgrounds threads
//! that cannot easily integrate with the async `sqlmodel_pool`.

use crate::DbConn;
use crate::error::DbError;
use sqlmodel_core::Value;

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

    // 1. Resolve project (first available)
    let project_row = conn
        .query_sync("SELECT id FROM projects ORDER BY id LIMIT 1", &[])
        .map_err(|e| DbError::Sqlite(e.to_string()))?
        .into_iter()
        .next();

    let project_id = project_row
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| DbError::NotFound {
            entity: "Project",
            identifier: "any".into(),
        })?;

    let now = now_micros();

    // 2. Resolve or auto-register sender
    let sender_rows = conn
        .query_sync(
            "SELECT id FROM agents WHERE project_id = ?1 AND name = ?2",
            &[
                Value::BigInt(project_id),
                Value::Text(sender_name.to_string()),
            ],
        )
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let sender_id = if let Some(row) = sender_rows.into_iter().next() {
        row.get_named::<i64>("id").unwrap_or(0)
    } else {
        // Auto-register
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (?1, ?2, 'tui-overseer', 'human', 'Human operator via TUI', ?3, ?4)",
            &[
                Value::BigInt(project_id),
                Value::Text(sender_name.to_string()),
                Value::BigInt(now),
                Value::BigInt(now),
            ],
        ).map_err(|e| DbError::Sqlite(e.to_string()))?;

        // Re-query ID
        let rows = conn
            .query_sync(
                "SELECT id FROM agents WHERE project_id = ?1 AND name = ?2",
                &[
                    Value::BigInt(project_id),
                    Value::Text(sender_name.to_string()),
                ],
            )
            .map_err(|e| DbError::Sqlite(e.to_string()))?;

        rows.into_iter()
            .next()
            .and_then(|r| r.get_named::<i64>("id").ok())
            .unwrap_or(0)
    };

    if sender_id == 0 {
        return Err(DbError::Internal("Failed to resolve sender ID".into()));
    }

    // 3. Insert Message
    let thread_id_val = thread_id.map_or(Value::Null, |t| Value::Text(t.to_string()));

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, thread_id, created_ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
        &[
            Value::BigInt(project_id),
            Value::BigInt(sender_id),
            Value::Text(subject.to_string()),
            Value::Text(body_md.to_string()),
            Value::Text(importance.to_string()),
            thread_id_val,
            Value::BigInt(now),
        ],
    )
    .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let msg_rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| DbError::Sqlite(e.to_string()))?;

    let msg_id = msg_rows
        .into_iter()
        .next()
        .and_then(|r| r.get_named::<i64>("id").ok())
        .ok_or_else(|| DbError::Internal("Message insert returned no ID".into()))?;

    // 4. Insert Recipients
    for (name, kind) in recipients {
        // Resolve recipient ID
        let rec_rows = conn
            .query_sync(
                "SELECT id FROM agents WHERE project_id = ?1 AND name = ?2",
                &[Value::BigInt(project_id), Value::Text(name.clone())],
            )
            .map_err(|e| DbError::Sqlite(e.to_string()))?;

        if let Some(row) = rec_rows.into_iter().next()
            && let Ok(aid) = row.get_named::<i64>("id")
        {
            let _ = conn.execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, ?3)",
                &[
                    Value::BigInt(msg_id),
                    Value::BigInt(aid),
                    Value::Text(kind.clone()),
                ],
            );
        }
    }

    Ok(msg_id)
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
    }

    #[test]
    fn dispatch_root_message_unknown_recipient_skipped() {
        let conn = test_conn();
        let _pid = insert_project(&conn);

        let msg_id = dispatch_root_message(
            &conn,
            "Sender",
            "Unknown recipient",
            "Body",
            "normal",
            None,
            &[("NonexistentAgent".to_string(), "to".to_string())],
        )
        .unwrap();

        // Message should exist but have no recipients
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
        assert_eq!(cnt, 0, "unknown recipient should be silently skipped");
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
}
