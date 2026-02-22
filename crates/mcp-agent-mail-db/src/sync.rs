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

    let current_thread_id = rows
        .into_iter()
        .next()
        .and_then(|row| row.get_named::<String>("thread_id").ok());

    let Some(current_thread_id) = current_thread_id else {
        return Err(DbError::NotFound {
            entity: "Message",
            identifier: message_id.to_string(),
        });
    };

    if current_thread_id == target_thread_id {
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
