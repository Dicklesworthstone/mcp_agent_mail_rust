//! Steps 4–7: FTS, materialized views, performance indexes, and export finalization.
//!
//! Operates on a scoped+scrubbed snapshot in-place.

use std::path::Path;

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::queries::UNKNOWN_SENDER_DISPLAY;

use crate::ShareError;

type Conn = DbConn;
const UNKNOWN_RECIPIENT_DISPLAY: &str = "[unknown recipient]";
const UNKNOWN_PROJECT_SLUG_PREFIX: &str = "[unknown-project-";
const SQLITE_SNAPSHOT_SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

#[cfg(test)]
// Historical alias name retained in tests; this still uses FrankenSQLite `DbConn`.
type SqliteConnection = DbConn;

#[cfg(test)]
static FAIL_FINALIZE_SNAPSHOT_STAGE_BEFORE_PUBLISH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_FINALIZE_EXPORT_AFTER_FTS_BUILD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Result of the full finalization pipeline.
#[derive(Debug, Clone)]
pub struct FinalizeResult {
    pub fts_enabled: bool,
    pub views_created: Vec<String>,
    pub indexes_created: Vec<String>,
}

/// Step 4: Build FTS5 search index on messages.
///
/// Returns `true` if FTS5 was available and the index was created.
/// Returns `false` if FTS5 is not compiled into SQLite (graceful fallback).
pub fn build_search_indexes(snapshot_path: &Path) -> Result<bool, ShareError> {
    let conn = open_conn(snapshot_path)?;
    build_search_indexes_with_conn(&conn)
}

fn build_search_indexes_with_conn(conn: &Conn) -> Result<bool, ShareError> {
    // Check if thread_id column exists
    let has_thread_id = column_exists(conn, "messages", "thread_id")?;

    // Create FTS5 virtual table
    let create_sql = "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(\
            message_id UNINDEXED, \
            subject, \
            body, \
            importance UNINDEXED, \
            project_slug UNINDEXED, \
            thread_key UNINDEXED, \
            created_ts UNINDEXED\
        )";
    let create_result = conn.execute_raw(create_sql);

    if let Err(e) = create_result {
        let msg = e.to_string();
        // FTS5 not available — not an error, just means no search.
        // FrankenConnection reports "not implemented" for VIRTUAL TABLE.
        if msg.contains("fts5")
            || msg.contains("unknown tokenizer")
            || msg.contains("no such module")
            || msg.contains("not implemented")
        {
            return Ok(false);
        }
        return Err(ShareError::Sqlite {
            message: format!("FTS5 CREATE failed: {msg}"),
        });
    }

    // If the snapshot DB already contains an old/incompatible fts_messages schema (from an earlier
    // export run), rebuild it so the populate SQL stays valid.
    let mut needs_rebuild = false;
    for col in [
        "message_id",
        "subject",
        "body",
        "importance",
        "project_slug",
        "thread_key",
        "created_ts",
    ] {
        if !column_exists(conn, "fts_messages", col)? {
            needs_rebuild = true;
            break;
        }
    }

    if needs_rebuild {
        let drop_result = conn.execute_raw("DROP TABLE IF EXISTS fts_messages");
        if let Err(e) = drop_result {
            let msg = e.to_string();
            if msg.contains("fts5")
                || msg.contains("unknown tokenizer")
                || msg.contains("no such module")
            {
                return Ok(false);
            }
            return Err(ShareError::Sqlite {
                message: format!("FTS5 DROP failed: {msg}"),
            });
        }

        let recreate_result = conn.execute_raw(create_sql);
        if let Err(e) = recreate_result {
            let msg = e.to_string();
            if msg.contains("fts5")
                || msg.contains("unknown tokenizer")
                || msg.contains("no such module")
            {
                return Ok(false);
            }
            return Err(ShareError::Sqlite {
                message: format!("FTS5 CREATE (rebuild) failed: {msg}"),
            });
        }
    }

    // Clear any existing data (idempotent re-runs)
    conn.execute_raw("DELETE FROM fts_messages")
        .map_err(|e| ShareError::Sqlite {
            message: format!("DELETE FROM fts_messages failed: {e}"),
        })?;

    let message_count_rows = conn
        .query_sync("SELECT COUNT(*) AS cnt FROM messages", &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("FTS message count failed: {e}"),
        })?;
    let message_count = message_count_rows
        .first()
        .and_then(|row| row.get_named::<i64>("cnt").ok())
        .unwrap_or(0);
    if message_count == 0 {
        return Ok(true);
    }

    // Populate from messages + projects
    let project_slug_expr = format!(
        "COALESCE(NULLIF(TRIM((SELECT p.slug FROM projects p WHERE p.id = m.project_id LIMIT 1)), ''), \
             CASE \
                 WHEN m.project_id IS NULL THEN '' \
                 ELSE '{UNKNOWN_PROJECT_SLUG_PREFIX}' || m.project_id || ']' \
             END \
         )"
    );

    let insert_sql = if has_thread_id {
        format!(
            "INSERT INTO fts_messages(message_id, subject, body, importance, project_slug, thread_key, created_ts) \
         SELECT \
             m.id, \
             COALESCE(m.subject, ''), \
             COALESCE(m.body_md, ''), \
             COALESCE(m.importance, ''), \
             {project_slug_expr}, \
             CASE \
                 WHEN m.thread_id IS NULL OR m.thread_id = '' THEN printf('msg:%d', m.id) \
                 ELSE m.thread_id \
             END, \
             COALESCE(m.created_ts, '') \
         FROM messages AS m"
        )
    } else {
        format!(
            "INSERT INTO fts_messages(message_id, subject, body, importance, project_slug, thread_key, created_ts) \
         SELECT \
             m.id, \
             COALESCE(m.subject, ''), \
             COALESCE(m.body_md, ''), \
             COALESCE(m.importance, ''), \
             {project_slug_expr}, \
             printf('msg:%d', m.id), \
             COALESCE(m.created_ts, '') \
         FROM messages AS m"
        )
    };

    conn.execute_raw(&insert_sql)
        .map_err(|e| ShareError::Sqlite {
            message: format!("FTS populate failed: {e}"),
        })?;

    // Optimize FTS index
    conn.execute_raw("INSERT INTO fts_messages(fts_messages) VALUES('optimize')")
        .map_err(|e| ShareError::Sqlite {
            message: format!("FTS optimize failed: {e}"),
        })?;

    Ok(true)
}

/// Step 5: Build materialized views for the static viewer.
///
/// Creates:
/// - `message_overview_mv` (denormalized message list with sender info)
/// - `attachments_by_message_mv` (flattened JSON attachments)
/// - `fts_search_overview_mv` (pre-computed snippets, only if FTS5 available)
pub fn build_materialized_views(
    snapshot_path: &Path,
    fts_enabled: bool,
) -> Result<Vec<String>, ShareError> {
    let conn = open_conn(snapshot_path)?;
    build_materialized_views_with_conn(&conn, fts_enabled)
}

fn build_materialized_views_with_conn(
    conn: &Conn,
    fts_enabled: bool,
) -> Result<Vec<String>, ShareError> {
    let mut created = Vec::new();

    if !table_exists(conn, "message_recipients")? {
        return Err(ShareError::Validation {
            message: "snapshot missing required table: message_recipients".to_string(),
        });
    }

    let has_thread_id = column_exists(conn, "messages", "thread_id")?;
    let has_sender_id = column_exists(conn, "messages", "sender_id")?;

    // --- message_overview_mv ---
    conn.execute_raw("DROP TABLE IF EXISTS message_overview_mv")
        .map_err(sql_err)?;

    let thread_expr = if has_thread_id {
        "NULLIF(TRIM(m.thread_id), '')"
    } else {
        "printf('msg:%d', m.id)"
    };
    let sender_expr = if has_sender_id {
        format!(
            "COALESCE(NULLIF(TRIM((SELECT a.name FROM agents a WHERE a.id = m.sender_id LIMIT 1)), ''), '{UNKNOWN_SENDER_DISPLAY}') AS sender_name"
        )
    } else {
        format!("'{UNKNOWN_SENDER_DISPLAY}' AS sender_name")
    };
    let recipients_join = format!(
        "LEFT JOIN ( \
             SELECT ordered_recipients.message_id, \
                    GROUP_CONCAT(ordered_recipients.name, ', ') AS recipients \
             FROM ( \
                 SELECT DISTINCT \
                     mr.message_id, \
                     COALESCE(NULLIF(TRIM(ag.name), ''), '{UNKNOWN_RECIPIENT_DISPLAY}') AS name \
                 FROM message_recipients mr \
                 LEFT JOIN agents ag ON ag.id = mr.agent_id \
                 ORDER BY mr.message_id, name \
             ) AS ordered_recipients \
             GROUP BY ordered_recipients.message_id \
         ) AS recipient_rollup ON recipient_rollup.message_id = m.id"
    );
    let recipients_expr = "COALESCE(recipient_rollup.recipients, '') AS recipients";
    let attachments_expr = "CASE \
             WHEN json_valid(COALESCE(m.attachments, '[]')) \
             THEN json_array_length(COALESCE(m.attachments, '[]')) \
             ELSE 0 \
         END AS attachment_count";

    // frankensqlite has a quirk where CREATE TABLE ... AS SELECT with complex
    // sources (correlated subqueries + nested derived tables in the FROM
    // clause) returns Ok but does NOT register the table in sqlite_master,
    // which then blows up when later DDL (CREATE INDEX, etc.) tries to use
    // the table. Sidestep it by creating the table with explicit columns and
    // then INSERTing the SELECT result — the INSERT path doesn't depend on
    // schema inference from the SELECT and works reliably.
    let create_table_sql = "\
        CREATE TABLE message_overview_mv (\
             id INTEGER, \
             project_id INTEGER, \
             thread_id TEXT, \
             subject TEXT, \
             importance TEXT, \
             ack_required INTEGER, \
             created_ts INTEGER, \
             sender_name TEXT, \
             body_length INTEGER, \
             attachment_count INTEGER, \
             latest_snippet TEXT, \
             recipients TEXT\
         )";
    conn.execute_raw(create_table_sql)
        .map_err(|e| ShareError::Sqlite {
            message: format!("message_overview_mv create failed: {e}"),
        })?;

    let overview_sql = format!(
        "INSERT INTO message_overview_mv \
         SELECT \
             m.id, \
             m.project_id, \
             {thread_expr} AS thread_id, \
             m.subject, \
             m.importance, \
             m.ack_required, \
             m.created_ts, \
             {sender_expr}, \
             LENGTH(m.body_md) AS body_length, \
             {attachments_expr}, \
             SUBSTR(COALESCE(m.body_md, ''), 1, 280) AS latest_snippet, \
             {recipients_expr} \
         FROM messages m \
         {recipients_join} \
         ORDER BY m.created_ts DESC"
    );
    conn.execute_raw(&overview_sql)
        .map_err(|e| ShareError::Sqlite {
            message: format!("message_overview_mv populate failed: {e}\nSQL: {overview_sql}"),
        })?;

    for idx in [
        "CREATE INDEX idx_msg_overview_created ON message_overview_mv(created_ts DESC)",
        "CREATE INDEX idx_msg_overview_thread ON message_overview_mv(thread_id, created_ts DESC)",
        "CREATE INDEX idx_msg_overview_project ON message_overview_mv(project_id, created_ts DESC)",
        "CREATE INDEX idx_msg_overview_importance ON message_overview_mv(importance, created_ts DESC)",
    ] {
        conn.execute_raw(idx).map_err(|e| ShareError::Sqlite {
            message: format!("message_overview_mv index create failed for {idx:?}: {e}"),
        })?;
    }
    created.push("message_overview_mv".to_string());

    // --- attachments_by_message_mv ---
    conn.execute_raw("DROP TABLE IF EXISTS attachments_by_message_mv")
        .map_err(sql_err)?;

    conn.execute_raw(
        "CREATE TABLE attachments_by_message_mv (\
         message_id INTEGER, \
         project_id INTEGER, \
         thread_id TEXT, \
         created_ts TEXT, \
         attachment_type TEXT, \
         media_type TEXT, \
         path TEXT, \
         size_bytes INTEGER\
         )",
    )
    .map_err(sql_err)?;
    populate_attachments_by_message_mv(conn, has_thread_id)?;

    for idx in [
        "CREATE INDEX idx_attach_by_msg ON attachments_by_message_mv(message_id)",
        "CREATE INDEX idx_attach_by_type ON attachments_by_message_mv(attachment_type, created_ts DESC)",
        "CREATE INDEX idx_attach_by_project ON attachments_by_message_mv(project_id, created_ts DESC)",
    ] {
        conn.execute_raw(idx).map_err(sql_err)?;
    }
    conn.execute_raw(
        "UPDATE message_overview_mv \
         SET attachment_count = COALESCE(( \
             SELECT COUNT(*) \
             FROM attachments_by_message_mv AS attachments \
             WHERE attachments.message_id = message_overview_mv.id \
         ), 0)",
    )
    .map_err(sql_err)?;
    created.push("attachments_by_message_mv".to_string());

    // --- fts_search_overview_mv (only if FTS5 available) ---
    if fts_enabled {
        conn.execute_raw("DROP TABLE IF EXISTS fts_search_overview_mv")
            .map_err(sql_err)?;

        // Same frankensqlite CTA quirk as message_overview_mv: split the
        // CREATE TABLE AS SELECT into an explicit CREATE TABLE + INSERT INTO
        // ... SELECT so the new table actually lands in sqlite_master.
        let create_fts_table_sql = "\
            CREATE TABLE fts_search_overview_mv (\
                 rowid INTEGER, \
                 id INTEGER, \
                 subject TEXT, \
                 created_ts INTEGER, \
                 importance TEXT, \
                 sender_name TEXT, \
                 snippet TEXT\
             )";
        let fts_overview_sql = if has_sender_id {
            format!(
                "INSERT INTO fts_search_overview_mv \
             SELECT \
                 m.id AS rowid, \
                 m.id, \
                 m.subject, \
                 m.created_ts, \
                 m.importance, \
                 COALESCE(NULLIF(TRIM((SELECT a.name FROM agents a WHERE a.id = m.sender_id LIMIT 1)), ''), '{UNKNOWN_SENDER_DISPLAY}') AS sender_name, \
                 SUBSTR(m.body_md, 1, 200) AS snippet \
             FROM messages m \
                 ORDER BY m.created_ts DESC"
            )
        } else {
            format!(
                "INSERT INTO fts_search_overview_mv \
             SELECT \
                 m.id AS rowid, \
                 m.id, \
                 m.subject, \
                 m.created_ts, \
                 m.importance, \
                 '{UNKNOWN_SENDER_DISPLAY}' AS sender_name, \
                 SUBSTR(m.body_md, 1, 200) AS snippet \
             FROM messages m \
             ORDER BY m.created_ts DESC"
            )
        };

        if conn.execute_raw(create_fts_table_sql).is_ok()
            && conn.execute_raw(&fts_overview_sql).is_ok()
        {
            for idx in [
                "CREATE INDEX idx_fts_overview_rowid ON fts_search_overview_mv(rowid)",
                "CREATE INDEX idx_fts_overview_created ON fts_search_overview_mv(created_ts DESC)",
            ] {
                conn.execute_raw(idx).map_err(sql_err)?;
            }
            created.push("fts_search_overview_mv".to_string());
        }
        // else: FTS5 not available at view creation time — skip gracefully
    }

    Ok(created)
}

fn populate_attachments_by_message_mv(conn: &Conn, has_thread_id: bool) -> Result<(), ShareError> {
    let select_sql = if has_thread_id {
        "SELECT id, project_id, thread_id, created_ts, attachments FROM messages"
    } else {
        "SELECT id, project_id, created_ts, attachments FROM messages"
    };
    let rows = conn
        .query_sync(select_sql, &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("attachments source query failed: {e}"),
        })?;

    for row in rows {
        let message_id: i64 = row.get_named("id").map_err(sql_err)?;
        let project_id: Option<i64> = row.get_named("project_id").map_err(sql_err)?;
        let created_ts: Option<String> = row.get_named("created_ts").map_err(sql_err)?;
        let thread_id = if has_thread_id {
            row.get_named::<Option<String>>("thread_id")
                .map_err(sql_err)?
                .and_then(|value| {
                    let trimmed = value.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                })
        } else {
            None
        };
        let attachments_json: Option<String> = row.get_named("attachments").map_err(sql_err)?;
        let Some(attachments_json) = attachments_json else {
            continue;
        };
        let Ok(serde_json::Value::Array(attachments)) =
            serde_json::from_str::<serde_json::Value>(&attachments_json)
        else {
            continue;
        };

        for attachment in attachments {
            let serde_json::Value::Object(fields) = attachment else {
                continue;
            };
            let attachment_type = fields
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let media_type = fields
                .get("media_type")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let path = fields
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let size_bytes = fields.get("bytes").and_then(serde_json::Value::as_i64);

            conn.execute_sync(
                "INSERT INTO attachments_by_message_mv (\
                 message_id, project_id, thread_id, created_ts, \
                 attachment_type, media_type, path, size_bytes\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                &[
                    sqlmodel_core::Value::BigInt(message_id),
                    project_id
                        .map(sqlmodel_core::Value::BigInt)
                        .unwrap_or(sqlmodel_core::Value::Null),
                    thread_id
                        .clone()
                        .map(sqlmodel_core::Value::Text)
                        .unwrap_or(sqlmodel_core::Value::Null),
                    created_ts
                        .clone()
                        .map(sqlmodel_core::Value::Text)
                        .unwrap_or(sqlmodel_core::Value::Null),
                    attachment_type
                        .map(sqlmodel_core::Value::Text)
                        .unwrap_or(sqlmodel_core::Value::Null),
                    media_type
                        .map(sqlmodel_core::Value::Text)
                        .unwrap_or(sqlmodel_core::Value::Null),
                    path.map(sqlmodel_core::Value::Text)
                        .unwrap_or(sqlmodel_core::Value::Null),
                    size_bytes
                        .map(sqlmodel_core::Value::BigInt)
                        .unwrap_or(sqlmodel_core::Value::Null),
                ],
            )
            .map_err(|e| ShareError::Sqlite {
                message: format!("attachments_by_message_mv insert failed: {e}"),
            })?;
        }
    }

    Ok(())
}

/// Step 6: Create performance indexes (lowercase columns + covering indexes).
pub fn create_performance_indexes(snapshot_path: &Path) -> Result<Vec<String>, ShareError> {
    let conn = open_conn(snapshot_path)?;
    create_performance_indexes_with_conn(&conn)
}

fn create_performance_indexes_with_conn(conn: &Conn) -> Result<Vec<String>, ShareError> {
    let mut indexes = Vec::new();

    let has_sender_id = column_exists(conn, "messages", "sender_id")?;
    let has_thread_id = column_exists(conn, "messages", "thread_id")?;

    // Export snapshots are static. Drop any legacy FTS triggers so our later `UPDATE messages ...`
    // statements can't fail if the snapshot rebuilds `fts_messages` with a different schema.
    for trigger in [
        // Older naming
        "messages_ai",
        "messages_ad",
        "messages_au",
        // Current naming (matches the DB schema in `mcp-agent-mail-db`)
        "fts_messages_ai",
        "fts_messages_ad",
        "fts_messages_au",
    ] {
        // Use bracket-escaping for the identifier to prevent SQL injection
        // if this pattern is ever copied to dynamic trigger names.
        conn.execute_raw(&format!("DROP TRIGGER IF EXISTS [{trigger}]"))
            .map_err(sql_err)?;
    }

    // Add lowercase columns (suppress error if already exist)
    let _ = conn.execute_raw("ALTER TABLE messages ADD COLUMN subject_lower TEXT");
    let _ = conn.execute_raw("ALTER TABLE messages ADD COLUMN sender_lower TEXT");

    // Populate lowercase columns
    if has_sender_id {
        conn.execute_raw(&format!(
            "UPDATE messages SET \
                 subject_lower = LOWER(COALESCE(subject, '')), \
                 sender_lower = LOWER(\
                     COALESCE(\
                         NULLIF(TRIM((SELECT name FROM agents WHERE agents.id = messages.sender_id)), ''), \
                         '{UNKNOWN_SENDER_DISPLAY}'\
                     )\
                 )"
        ))
        .map_err(sql_err)?;
    } else {
        conn.execute_raw(&format!(
            "UPDATE messages SET \
                 subject_lower = LOWER(COALESCE(subject, '')), \
                 sender_lower = LOWER('{UNKNOWN_SENDER_DISPLAY}')"
        ))
        .map_err(sql_err)?;
    }

    // Create covering indexes
    for (name, ddl) in [
        (
            "idx_messages_created_ts",
            "CREATE INDEX IF NOT EXISTS idx_messages_created_ts ON messages(created_ts DESC)",
        ),
        (
            "idx_messages_subject_lower",
            "CREATE INDEX IF NOT EXISTS idx_messages_subject_lower ON messages(subject_lower)",
        ),
        (
            "idx_messages_sender_lower",
            "CREATE INDEX IF NOT EXISTS idx_messages_sender_lower ON messages(sender_lower)",
        ),
    ] {
        conn.execute_raw(ddl).map_err(sql_err)?;
        indexes.push(name.to_string());
    }

    // Conditional indexes for optional columns
    if has_sender_id
        && conn
            .execute_raw(
                "CREATE INDEX IF NOT EXISTS idx_messages_sender ON messages(sender_id, created_ts DESC)",
            )
            .is_ok()
    {
        indexes.push("idx_messages_sender".to_string());
    }
    if has_thread_id
        && conn
            .execute_raw(
                "CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id, created_ts DESC)",
            )
            .is_ok()
    {
        indexes.push("idx_messages_thread".to_string());
    }

    Ok(indexes)
}

/// Step 7: Finalize snapshot storage for export.
///
/// This rebuilds the snapshot into a compact fresh database with the export
/// page size, then applies the final journal mode and statistics pragmas.
pub fn finalize_snapshot_for_export(snapshot_path: &Path) -> Result<(), ShareError> {
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let (_temp_dir, rebuilt_path, backup_path) = prepare_rebuilt_snapshot_storage(&snapshot_path)?;

    apply_export_finalize_pragmas(&rebuilt_path)?;
    replace_snapshot_with_rebuilt_path(&rebuilt_path, &snapshot_path, &backup_path)?;
    cleanup_snapshot_sidecars(&snapshot_path);

    Ok(())
}

/// Run steps 4–7 in sequence on a scoped+scrubbed snapshot.
pub fn finalize_export_db(snapshot_path: &Path) -> Result<FinalizeResult, ShareError> {
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    finalize_snapshot_for_export(&snapshot_path)?;
    let (_rollback_stage, rollback_source_path, rollback_backup_path) =
        prepare_post_finalize_rollback_snapshot(&snapshot_path)?;

    let result = (|| -> Result<FinalizeResult, ShareError> {
        let fts_enabled = build_search_indexes(&snapshot_path)?;
        #[cfg(test)]
        if FAIL_FINALIZE_EXPORT_AFTER_FTS_BUILD.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(ShareError::Sqlite {
                message: "forced finalize failure after FTS build".to_string(),
            });
        }
        let views_created = build_materialized_views(&snapshot_path, fts_enabled)?;
        let indexes_created = create_performance_indexes(&snapshot_path)?;
        let conn = open_conn(&snapshot_path)?;
        conn.execute_raw("PRAGMA analysis_limit=400")
            .map_err(sql_err)?;
        conn.execute_raw("ANALYZE").map_err(sql_err)?;
        conn.execute_raw("PRAGMA optimize").map_err(sql_err)?;

        Ok(FinalizeResult {
            fts_enabled,
            views_created,
            indexes_created,
        })
    })();

    match result {
        Ok(result) => Ok(result),
        Err(error) => {
            replace_snapshot_with_rebuilt_path(
                &rollback_source_path,
                &snapshot_path,
                &rollback_backup_path,
            )
            .map_err(|rollback_error| {
                ShareError::Io(std::io::Error::other(format!(
                    "finalize export failed ({error}); restoring pre-FTS snapshot state also failed ({rollback_error})"
                )))
            })?;
            cleanup_snapshot_sidecars(&snapshot_path);
            Err(error)
        }
    }
}

fn prepare_rebuilt_snapshot_storage(
    snapshot_path: &Path,
) -> Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf), ShareError> {
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let parent = snapshot_path
        .parent()
        .ok_or_else(|| ShareError::Io(std::io::Error::other("snapshot path has no parent")))?;
    let temp_dir = tempfile::Builder::new()
        .prefix("am-share-finalize-")
        .tempdir_in(parent)?;
    let rebuilt_path = temp_dir.path().join("mailbox.sqlite3");

    crate::snapshot::rebuild_sqlite_snapshot_with_pragmas(
        &snapshot_path,
        &rebuilt_path,
        false,
        &["PRAGMA journal_mode='DELETE'", "PRAGMA page_size=1024"],
    )?;

    let backup_path = temp_dir.path().join("mailbox.backup.sqlite3");
    Ok((temp_dir, rebuilt_path, backup_path))
}

fn prepare_post_finalize_rollback_snapshot(
    snapshot_path: &Path,
) -> Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf), ShareError> {
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let parent = snapshot_path
        .parent()
        .ok_or_else(|| ShareError::Io(std::io::Error::other("snapshot path has no parent")))?;
    let temp_dir = tempfile::Builder::new()
        .prefix("am-share-finalize-rollback-")
        .tempdir_in(parent)?;
    let rollback_source_path = temp_dir.path().join("mailbox.pre-fts.sqlite3");
    std::fs::copy(&snapshot_path, &rollback_source_path).map_err(|error| {
        ShareError::Io(std::io::Error::other(format!(
            "failed to snapshot pre-FTS export state from {}: {error}",
            snapshot_path.display()
        )))
    })?;
    let rollback_backup_path = temp_dir.path().join("mailbox.failed.sqlite3");
    Ok((temp_dir, rollback_source_path, rollback_backup_path))
}

#[cfg(test)]
fn rewrite_snapshot_storage(snapshot_path: &Path) -> Result<(), ShareError> {
    let snapshot_path = crate::require_real_share_sqlite_path(snapshot_path)?;
    let (_temp_dir, rebuilt_path, backup_path) = prepare_rebuilt_snapshot_storage(&snapshot_path)?;
    replace_snapshot_with_rebuilt_path(&rebuilt_path, &snapshot_path, &backup_path)?;
    cleanup_snapshot_sidecars(&snapshot_path);
    Ok(())
}

fn apply_export_finalize_pragmas(snapshot_path: &Path) -> Result<(), ShareError> {
    #[cfg(test)]
    if FAIL_FINALIZE_SNAPSHOT_STAGE_BEFORE_PUBLISH.swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return Err(ShareError::Sqlite {
            message: "forced finalize failure before publish".to_string(),
        });
    }

    let conn = open_conn(snapshot_path)?;
    conn.execute_raw("PRAGMA journal_mode='DELETE'")
        .map_err(sql_err)?;
    conn.execute_raw("PRAGMA analysis_limit=400")
        .map_err(sql_err)?;
    conn.execute_raw("ANALYZE").map_err(sql_err)?;
    conn.execute_raw("PRAGMA optimize").map_err(sql_err)?;
    Ok(())
}

fn cleanup_snapshot_sidecars(snapshot_path: &Path) {
    for suffix in SQLITE_SNAPSHOT_SIDECAR_SUFFIXES {
        let mut sidecar_os = snapshot_path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar_path = std::path::PathBuf::from(sidecar_os);
        if sidecar_path.exists() {
            let _ = std::fs::remove_file(sidecar_path);
        }
    }
}

fn replace_snapshot_with_rebuilt_path(
    rebuilt_path: &Path,
    snapshot_path: &Path,
    backup_path: &Path,
) -> Result<(), ShareError> {
    std::fs::rename(snapshot_path, backup_path).map_err(|error| {
        ShareError::Io(std::io::Error::other(format!(
            "failed to stage existing snapshot for replacement: {error}"
        )))
    })?;

    if let Err(rename_error) = std::fs::rename(rebuilt_path, snapshot_path) {
        if let Err(rollback_error) = std::fs::rename(backup_path, snapshot_path) {
            return Err(ShareError::Io(std::io::Error::other(format!(
                "failed to replace snapshot via rename ({rename_error}); rollback failed ({rollback_error})"
            ))));
        }
        return Err(ShareError::Io(std::io::Error::other(format!(
            "failed to replace snapshot via rename ({rename_error})"
        ))));
    }

    Ok(())
}

// --- helpers ---

fn open_conn(path: &Path) -> Result<Conn, ShareError> {
    let path = crate::require_real_share_sqlite_path(path)?;
    let path_str = path.display().to_string();
    Conn::open_file(&path_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot open {path_str}: {e}"),
    })
}

fn sql_err(e: impl std::fmt::Display) -> ShareError {
    ShareError::Sqlite {
        message: e.to_string(),
    }
}

fn table_exists(conn: &Conn, table: &str) -> Result<bool, ShareError> {
    let sql = format!(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '{}' LIMIT 1",
        table.replace('\'', "''")
    );
    let rows = conn.query_sync(&sql, &[]).map_err(|e| ShareError::Sqlite {
        message: format!("sqlite_master lookup for {table} failed: {e}"),
    })?;
    Ok(!rows.is_empty())
}

fn column_exists(conn: &Conn, table: &str, column: &str) -> Result<bool, ShareError> {
    // PRAGMA table_info returns 0 rows on FrankenConnection; fall back to
    // a direct SELECT probe when PRAGMA yields nothing.
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table})"), &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA table_info({table}) failed: {e}"),
        })?;
    if !rows.is_empty() {
        for row in &rows {
            let name: String = row.get_named("name").unwrap_or_default();
            if name == column {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    // Fallback: try to SELECT the column directly.
    let probe = format!("SELECT \"{column}\" FROM \"{table}\" LIMIT 0");
    match conn.query_sync(&probe, &[]) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_size(path: &std::path::Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    /// Create a test DB with the standard schema.
    fn create_test_db(dir: &std::path::Path) -> std::path::PathBuf {
        let db_path = dir.join("test_finalize.sqlite3");
        let conn = SqliteConnection::open_file(db_path.display().to_string()).unwrap();

        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, \
             program TEXT DEFAULT '', model TEXT DEFAULT '', task_description TEXT DEFAULT '', \
             inception_ts TEXT DEFAULT '', last_active_ts TEXT DEFAULT '', \
             attachments_policy TEXT DEFAULT 'auto', contact_policy TEXT DEFAULT 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
             kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, \
             PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, \
             agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, \
             reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', \
             released_ts TEXT)",
        )
        .unwrap();

        // Insert test data
        conn.execute_raw(
            "INSERT INTO projects VALUES (1, 'proj-alpha', '/data/alpha', '2025-01-01T00:00:00Z')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'AlphaAgent', 'claude-code', 'opus-4', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 1, 'TKT-1', 'Hello World', \
             'This is a test message with some content.', 'normal', 0, '2025-01-01T10:00:00Z', '[]')",
        ).unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (2, 1, 1, 'TKT-1', 'With Attachments', \
             'Message with files.', 'high', 1, '2025-01-01T11:00:00Z', \
             '[{\"type\":\"file\",\"media_type\":\"text/plain\",\"path\":\"data.txt\",\"bytes\":1024}]')",
        ).unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (2, 1, 'to', NULL, NULL)")
            .unwrap();

        db_path
    }

    #[cfg(unix)]
    #[test]
    fn finalize_export_db_rejects_symlinked_snapshot() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let linked = dir.path().join("linked.sqlite3");
        symlink(&db, &linked).unwrap();

        let err =
            finalize_export_db(&linked).expect_err("symlinked snapshots must fail validation");
        assert!(matches!(err, ShareError::Validation { .. }));
        assert!(err.to_string().contains("real file"));
    }

    #[test]
    fn fts_creates_and_populates() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok, "FTS5 should be available");

        // Verify data in FTS table
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM fts_messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2, "should have 2 FTS entries");

        // Verify FTS search works
        let results = conn
            .query_sync(
                "SELECT message_id FROM fts_messages WHERE fts_messages MATCH 'Hello'",
                &[],
            )
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn fts_rebuilds_when_schema_is_incompatible() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Simulate a legacy export that created an FTS table without newer columns like
        // "importance" and "thread_key". The export pipeline should rebuild it.
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE VIRTUAL TABLE fts_messages USING fts5(subject, body, project_slug UNINDEXED)",
        )
        .unwrap();
        drop(conn);

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok, "FTS5 should be available");

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("PRAGMA table_info(fts_messages)", &[])
            .unwrap();
        let columns: Vec<String> = rows
            .iter()
            .map(|r| r.get_named::<String>("name").unwrap())
            .collect();
        assert!(columns.contains(&"importance".to_string()));
        assert!(columns.contains(&"thread_key".to_string()));

        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM fts_messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2, "should have 2 FTS entries after rebuild");
    }

    #[test]
    fn materialized_views_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let fts_ok = build_search_indexes(&db).unwrap();
        let views = build_materialized_views(&db, fts_ok).unwrap();

        assert!(views.contains(&"message_overview_mv".to_string()));
        assert!(views.contains(&"attachments_by_message_mv".to_string()));
        if fts_ok {
            assert!(views.contains(&"fts_search_overview_mv".to_string()));
        }

        // Verify message_overview_mv
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM message_overview_mv", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2);

        // Verify sender_name populated
        let rows = conn
            .query_sync(
                "SELECT sender_name FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let name: String = rows[0].get_named("sender_name").unwrap();
        assert_eq!(name, "AlphaAgent");

        // Verify attachments_by_message_mv has 1 row (only msg 2 has attachments)
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM attachments_by_message_mv", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn materialized_views_aggregate_all_recipients_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (2, 1, 'BetaAgent', 'codex-cli', 'gpt-5', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 2, 'cc', NULL, NULL)")
            .unwrap();
        drop(conn);

        build_materialized_views(&db, false).unwrap();

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT recipients FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let recipients: String = rows[0].get_named("recipients").unwrap();
        assert_eq!(recipients, "AlphaAgent, BetaAgent");
    }

    #[test]
    fn materialized_views_dedupe_and_normalize_recipient_display_names() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (2, 1, 'BetaAgent', 'codex-cli', 'gpt-5', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (3, 1, 'BetaAgent', 'codex-cli', 'gpt-5', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (4, 1, '   ', 'codex-cli', 'gpt-5', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 2, 'cc', NULL, NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 3, 'bcc', NULL, NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 4, 'cc', NULL, NULL)")
            .unwrap();
        drop(conn);

        build_materialized_views(&db, false).unwrap();

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT recipients FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let recipients: String = rows[0].get_named("recipients").unwrap();
        assert_eq!(recipients, "AlphaAgent, BetaAgent, [unknown recipient]");
    }

    #[test]
    fn materialized_views_order_recipients_per_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (2, 1, 'BetaAgent', 'codex-cli', 'gpt-5', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (3, 1, 'ZetaAgent', 'codex-cli', 'gpt-5', 'testing', \
             '2025-01-01T00:00:00Z', '2025-01-01T12:00:00Z', 'auto', 'auto')",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 3, 'cc', NULL, NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (2, 2, 'cc', NULL, NULL)")
            .unwrap();
        drop(conn);

        build_materialized_views(&db, false).unwrap();

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT id, recipients FROM message_overview_mv ORDER BY id ASC",
                &[],
            )
            .unwrap();
        let message_one_recipients: String = rows[0].get_named("recipients").unwrap();
        let message_two_recipients: String = rows[1].get_named("recipients").unwrap();
        assert_eq!(message_one_recipients, "AlphaAgent, ZetaAgent");
        assert_eq!(message_two_recipients, "AlphaAgent, BetaAgent");
    }

    #[test]
    fn materialized_views_tolerate_malformed_attachment_json() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_sync(
            "UPDATE messages SET attachments = ? WHERE id = 1",
            &[sqlmodel_core::Value::Text("not valid json {".to_string())],
        )
        .unwrap();
        drop(conn);

        build_materialized_views(&db, false).unwrap();

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT attachment_count FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let attachment_count: i64 = rows[0].get_named("attachment_count").unwrap();
        assert_eq!(attachment_count, 0);
    }

    #[test]
    fn performance_indexes_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let indexes = create_performance_indexes(&db).unwrap();
        assert!(indexes.contains(&"idx_messages_created_ts".to_string()));
        assert!(indexes.contains(&"idx_messages_subject_lower".to_string()));
        assert!(indexes.contains(&"idx_messages_sender_lower".to_string()));
        assert!(indexes.contains(&"idx_messages_sender".to_string()));
        assert!(indexes.contains(&"idx_messages_thread".to_string()));

        // Verify lowercase columns populated
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT subject_lower, sender_lower FROM messages WHERE id = 1",
                &[],
            )
            .unwrap();
        let subj: String = rows[0].get_named("subject_lower").unwrap();
        let sender: String = rows[0].get_named("sender_lower").unwrap();
        assert_eq!(subj, "hello world");
        assert_eq!(sender, "alphaagent");
    }

    #[test]
    fn finalize_sets_journal_mode_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        finalize_snapshot_for_export(&db).unwrap();

        // Verify journal mode
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn.query_sync("PRAGMA journal_mode", &[]).unwrap();
        let mode: String = rows[0].get_named("journal_mode").unwrap();
        assert_eq!(mode, "delete");

        // Verify page size
        let rows = conn.query_sync("PRAGMA page_size", &[]).unwrap();
        let page_size: i64 = rows[0].get_named("page_size").unwrap();
        assert_eq!(page_size, 1024);

        // Verify integrity
        let rows = conn.query_sync("PRAGMA integrity_check", &[]).unwrap();
        let result: String = rows[0].get_named("integrity_check").unwrap();
        assert_eq!(result, "ok");
    }

    #[test]
    fn finalize_shrinks_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Inflate DB then delete to leave free pages.
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let big_body = "x".repeat(10_000);
        for i in 0..200 {
            conn.execute_raw(&format!(
                "INSERT INTO messages VALUES ({}, 1, 1, 'TKT-9', 'Bloat', '{}', \
                 'normal', 0, '2025-01-02T00:00:00Z', '[]')",
                1000 + i,
                big_body
            ))
            .unwrap();
        }
        conn.execute_raw("DELETE FROM messages WHERE id >= 1000")
            .unwrap();
        drop(conn);

        let before = file_size(&db);
        finalize_snapshot_for_export(&db).unwrap();
        let after = file_size(&db);

        assert!(
            after < before,
            "expected VACUUM to shrink DB (before={before}, after={after})"
        );
    }

    #[test]
    fn finalize_snapshot_for_export_keeps_original_when_stage_finalize_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let original_bytes = std::fs::read(&db).unwrap();

        FAIL_FINALIZE_SNAPSHOT_STAGE_BEFORE_PUBLISH
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = finalize_snapshot_for_export(&db)
            .expect_err("forced staged finalize failure should abort before publish");

        assert!(
            err.to_string()
                .contains("forced finalize failure before publish"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read(&db).unwrap(),
            original_bytes,
            "late finalize failure should leave the original snapshot bytes untouched"
        );

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn.query_sync("PRAGMA page_size", &[]).unwrap();
        let page_size: i64 = rows[0].get_named("page_size").unwrap();
        assert_ne!(
            page_size, 1024,
            "failed staged finalize should not publish the rebuilt export-sized snapshot"
        );
    }

    #[test]
    fn full_finalize_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let result = finalize_export_db(&db).unwrap();
        assert!(result.fts_enabled);
        assert!(!result.views_created.is_empty());
        assert!(!result.indexes_created.is_empty());

        // Verify everything is queryable
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();

        // FTS search
        let rows = conn
            .query_sync(
                "SELECT message_id FROM fts_messages WHERE fts_messages MATCH 'test'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);

        // Overview view
        let rows = conn
            .query_sync(
                "SELECT sender_name, attachment_count FROM message_overview_mv ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        let attach_count: i64 = rows[1].get_named("attachment_count").unwrap();
        assert_eq!(attach_count, 1);

        // Journal mode
        let rows = conn.query_sync("PRAGMA journal_mode", &[]).unwrap();
        let mode: String = rows[0].get_named("journal_mode").unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn finalize_drops_legacy_fts_triggers_if_schema_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Simulate the server schema having a different FTS layout + triggers that refer to
        // `fts_messages(message_id, ...)`. The share export pipeline rebuilds `fts_messages`, so
        // those triggers must be removed before any message updates.
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE VIRTUAL TABLE fts_messages USING fts5(message_id UNINDEXED, subject, body)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TRIGGER fts_messages_ai AFTER INSERT ON messages BEGIN \
                 INSERT INTO fts_messages(rowid, message_id, subject, body) VALUES (NEW.id, NEW.id, NEW.subject, NEW.body_md); \
             END;",
        ).unwrap();
        conn.execute_raw(
            "CREATE TRIGGER fts_messages_ad AFTER DELETE ON messages BEGIN \
                 DELETE FROM fts_messages WHERE rowid = OLD.id; \
             END;",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TRIGGER fts_messages_au AFTER UPDATE ON messages BEGIN \
                 DELETE FROM fts_messages WHERE rowid = OLD.id; \
                 INSERT INTO fts_messages(rowid, message_id, subject, body) VALUES (NEW.id, NEW.id, NEW.subject, NEW.body_md); \
             END;",
        ).unwrap();
        drop(conn);

        let result = finalize_export_db(&db);
        assert!(
            result.is_ok(),
            "finalize_export_db should succeed even with legacy FTS triggers: {result:?}"
        );
    }

    #[test]
    fn conformance_fts_ddl() {
        // Verify FTS DDL matches the fixture exactly
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mcp-agent-mail-conformance/tests/conformance/fixtures/share");

        let source = fixture_dir.join("minimal.sqlite3");
        if !source.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("fts_test.sqlite3");
        crate::create_sqlite_snapshot(&source, &snapshot, false).unwrap();

        let fts_ok = build_search_indexes(&snapshot).unwrap();
        assert!(fts_ok);

        // Verify the virtual table schema matches
        let conn = SqliteConnection::open_file(snapshot.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT sql FROM sqlite_master WHERE name = 'fts_messages'",
                &[],
            )
            .unwrap();
        assert!(!rows.is_empty(), "fts_messages should exist in schema");
        let sql: String = rows[0].get_named("sql").unwrap();
        assert!(sql.contains("fts5"), "should be FTS5 table");
        assert!(sql.contains("subject"), "should have subject column");
        assert!(sql.contains("body"), "should have body column");
    }

    #[test]
    fn fts_on_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Remove all messages so the DB is empty
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM message_recipients").unwrap();
        conn.execute_raw("DELETE FROM messages").unwrap();
        drop(conn);

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok, "FTS5 should still succeed on empty table");

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM fts_messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(
            count, 0,
            "FTS should have 0 entries for empty messages table"
        );
    }

    #[test]
    fn fts_idempotent_reruns() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Run FTS twice - should be idempotent
        let first = build_search_indexes(&db).unwrap();
        assert!(first);

        let second = build_search_indexes(&db).unwrap();
        assert!(second);

        // Verify same data (no duplicates)
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM fts_messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2, "idempotent re-run should not duplicate entries");
    }

    #[test]
    fn fts_keeps_orphaned_project_placeholder_slug() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM projects WHERE id = 1")
            .unwrap();
        drop(conn);

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok);

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT project_slug FROM fts_messages WHERE message_id = 1",
                &[],
            )
            .unwrap();
        let project_slug: String = rows[0].get_named("project_slug").unwrap();
        assert_eq!(project_slug, "[unknown-project-1]");
    }

    #[test]
    fn materialized_views_without_fts() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        // Build views with fts_enabled=false — should skip fts_search_overview_mv
        let views = build_materialized_views(&db, false).unwrap();
        assert!(views.contains(&"message_overview_mv".to_string()));
        assert!(views.contains(&"attachments_by_message_mv".to_string()));
        assert!(
            !views.contains(&"fts_search_overview_mv".to_string()),
            "should not create fts_search_overview_mv when fts is disabled"
        );
    }

    #[test]
    fn materialized_views_on_empty_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM message_recipients").unwrap();
        conn.execute_raw("DELETE FROM messages").unwrap();
        drop(conn);

        let views = build_materialized_views(&db, false).unwrap();
        assert!(views.contains(&"message_overview_mv".to_string()));
        assert!(views.contains(&"attachments_by_message_mv".to_string()));

        // Verify overview is empty
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM message_overview_mv", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn materialized_views_missing_recipients_table_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DROP TABLE message_recipients").unwrap();
        drop(conn);

        let err =
            build_materialized_views(&db, false).expect_err("missing recipients table must fail");
        assert!(
            matches!(err, ShareError::Validation { .. }),
            "unexpected error type: {err:?}"
        );
        assert!(
            err.to_string().contains("message_recipients"),
            "error should identify the missing required table: {err}"
        );
    }

    #[test]
    fn finalize_export_db_rolls_back_partial_post_publish_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        FAIL_FINALIZE_EXPORT_AFTER_FTS_BUILD.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = finalize_export_db(&db)
            .expect_err("forced post-FTS failure should roll back partial finalization state");
        assert!(
            err.to_string()
                .contains("forced finalize failure after FTS build"),
            "unexpected error: {err}"
        );

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn.query_sync("PRAGMA page_size", &[]).unwrap();
        let page_size: i64 = rows[0].get_named("page_size").unwrap();
        assert_eq!(
            page_size, 1024,
            "storage finalization should remain published even when later steps fail"
        );

        let rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'fts_messages'",
                &[],
            )
            .unwrap();
        assert!(
            rows.is_empty(),
            "failed finalization should roll back partially created FTS tables"
        );

        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2, "snapshot should remain queryable after rollback");
    }

    /// Create a test DB without sender_id column on messages.
    fn create_test_db_no_sender_id(dir: &std::path::Path) -> std::path::PathBuf {
        let db_path = dir.join("test_no_sender.sqlite3");
        let conn = SqliteConnection::open_file(db_path.display().to_string()).unwrap();

        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, \
             thread_id TEXT, subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
             kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();

        conn.execute_raw("INSERT INTO projects VALUES (1, 'proj', '/data/proj', '2025-01-01')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents VALUES (1, 1, 'TestAgent')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 'TKT-1', 'Test', 'Body.', 'normal', 0, '2025-01-01', '[]')",
        ).unwrap();

        db_path
    }

    #[test]
    fn performance_indexes_without_sender_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db_no_sender_id(dir.path());

        let indexes = create_performance_indexes(&db).unwrap();
        assert!(indexes.contains(&"idx_messages_created_ts".to_string()));
        assert!(indexes.contains(&"idx_messages_subject_lower".to_string()));
        assert!(indexes.contains(&"idx_messages_sender_lower".to_string()));
        // idx_messages_sender should NOT be created (no sender_id column)
        assert!(
            !indexes.contains(&"idx_messages_sender".to_string()),
            "should not create sender index when sender_id is absent"
        );

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT sender_lower FROM messages WHERE id = 1", &[])
            .unwrap();
        let sender: String = rows[0].get_named("sender_lower").unwrap();
        assert_eq!(sender, UNKNOWN_SENDER_DISPLAY.to_lowercase());
    }

    /// Create a test DB without thread_id column on messages.
    fn create_test_db_no_thread_id(dir: &std::path::Path) -> std::path::PathBuf {
        let db_path = dir.join("test_no_thread.sqlite3");
        let conn = SqliteConnection::open_file(db_path.display().to_string()).unwrap();

        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at TEXT DEFAULT '')",
        ).unwrap();
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER, \
             subject TEXT DEFAULT '', body_md TEXT DEFAULT '', \
             importance TEXT DEFAULT 'normal', ack_required INTEGER DEFAULT 0, \
             created_ts TEXT DEFAULT '', attachments TEXT DEFAULT '[]')",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, \
             kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();

        conn.execute_raw("INSERT INTO projects VALUES (1, 'proj', '/data/proj', '2025-01-01')")
            .unwrap();
        conn.execute_raw("INSERT INTO agents VALUES (1, 1, 'TestAgent')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 1, 'Test', 'Body.', 'normal', 0, '2025-01-01', '[]')",
        ).unwrap();

        db_path
    }

    #[test]
    fn performance_indexes_without_thread_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db_no_thread_id(dir.path());

        let indexes = create_performance_indexes(&db).unwrap();
        assert!(indexes.contains(&"idx_messages_created_ts".to_string()));
        // idx_messages_thread should NOT be created (no thread_id column)
        assert!(
            !indexes.contains(&"idx_messages_thread".to_string()),
            "should not create thread index when thread_id is absent"
        );
    }

    #[test]
    fn fts_without_thread_id_uses_msg_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db_no_thread_id(dir.path());

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok);

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT thread_key FROM fts_messages WHERE message_id = 1",
                &[],
            )
            .unwrap();
        let thread_key: String = rows[0].get_named("thread_key").unwrap();
        assert_eq!(
            thread_key, "msg:1",
            "should use 'msg:N' format when thread_id column absent"
        );
    }

    #[test]
    fn fts_without_thread_id_keeps_orphaned_project_placeholder_slug() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db_no_thread_id(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM projects WHERE id = 1")
            .unwrap();
        drop(conn);

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok);

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT project_slug FROM fts_messages WHERE message_id = 1",
                &[],
            )
            .unwrap();
        let project_slug: String = rows[0].get_named("project_slug").unwrap();
        assert_eq!(project_slug, "[unknown-project-1]");
    }

    #[test]
    fn column_exists_returns_false_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();

        assert!(!column_exists(&conn, "messages", "nonexistent_column").unwrap());
    }

    #[test]
    fn column_exists_returns_true_for_existing() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();

        assert!(column_exists(&conn, "messages", "subject").unwrap());
        assert!(column_exists(&conn, "messages", "sender_id").unwrap());
        assert!(column_exists(&conn, "messages", "thread_id").unwrap());
        assert!(column_exists(&conn, "projects", "slug").unwrap());
    }

    #[test]
    fn finalize_export_db_on_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM message_recipients").unwrap();
        conn.execute_raw("DELETE FROM messages").unwrap();
        conn.execute_raw("DELETE FROM agents").unwrap();
        conn.execute_raw("DELETE FROM projects").unwrap();
        drop(conn);

        let result = finalize_export_db(&db).unwrap();
        // FTS should still succeed but have no data
        assert!(result.fts_enabled);
        assert!(!result.views_created.is_empty());
        assert!(!result.indexes_created.is_empty());
    }

    #[test]
    fn replace_snapshot_with_rebuilt_path_restores_original_on_publish_failure() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snapshot.sqlite3");
        let backup = dir.path().join("backup.sqlite3");
        let missing_rebuilt = dir.path().join("missing-rebuilt.sqlite3");

        std::fs::write(&snapshot, b"original snapshot").unwrap();

        let err = replace_snapshot_with_rebuilt_path(&missing_rebuilt, &snapshot, &backup)
            .expect_err("missing rebuilt snapshot should fail replacement");
        assert!(
            err.to_string()
                .contains("failed to replace snapshot via rename"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read(&snapshot).unwrap(),
            b"original snapshot",
            "rollback should restore the original snapshot bytes"
        );
        assert!(
            !backup.exists(),
            "successful rollback should not leave the backup path behind"
        );
    }

    #[test]
    fn rewrite_snapshot_storage_removes_stale_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());
        let journal_path = std::path::PathBuf::from(format!("{}-journal", db.display()));
        let wal_path = std::path::PathBuf::from(format!("{}-wal", db.display()));
        let shm_path = std::path::PathBuf::from(format!("{}-shm", db.display()));

        std::fs::write(&journal_path, b"stale-journal").unwrap();
        std::fs::write(&wal_path, b"stale-wal").unwrap();
        std::fs::write(&shm_path, b"stale-shm").unwrap();

        rewrite_snapshot_storage(&db).unwrap();

        assert!(!journal_path.exists(), "rollback journal should be removed");
        assert!(!wal_path.exists(), "WAL sidecar should be removed");
        assert!(!shm_path.exists(), "SHM sidecar should be removed");

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2, "snapshot should remain queryable after rewrite");
    }

    #[test]
    fn materialized_views_without_sender_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db_no_sender_id(dir.path());

        let views = build_materialized_views(&db, false).unwrap();
        assert!(views.contains(&"message_overview_mv".to_string()));

        // Verify overview uses the stable sender placeholder.
        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT sender_name FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let name: String = rows[0].get_named("sender_name").unwrap();
        assert_eq!(name, UNKNOWN_SENDER_DISPLAY);
    }

    #[test]
    fn materialized_views_replace_blank_sender_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("UPDATE agents SET name = '   ' WHERE id = 1")
            .unwrap();
        drop(conn);

        let views = build_materialized_views(&db, true).unwrap();
        assert!(views.contains(&"message_overview_mv".to_string()));
        assert!(views.contains(&"fts_search_overview_mv".to_string()));

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let overview_rows = conn
            .query_sync(
                "SELECT sender_name FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let overview_name: String = overview_rows[0].get_named("sender_name").unwrap();
        assert_eq!(overview_name, UNKNOWN_SENDER_DISPLAY);

        let fts_rows = conn
            .query_sync(
                "SELECT sender_name FROM fts_search_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let fts_name: String = fts_rows[0].get_named("sender_name").unwrap();
        assert_eq!(fts_name, UNKNOWN_SENDER_DISPLAY);
    }

    #[test]
    fn materialized_views_keep_orphaned_sender_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("DELETE FROM agents WHERE id = 1").unwrap();
        drop(conn);

        build_materialized_views(&db, false).unwrap();

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT sender_name FROM message_overview_mv WHERE id = 1",
                &[],
            )
            .unwrap();
        let name: String = rows[0].get_named("sender_name").unwrap();
        assert_eq!(name, UNKNOWN_SENDER_DISPLAY);
    }

    #[test]
    fn materialized_views_normalize_blank_thread_ids() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        conn.execute_raw("UPDATE messages SET thread_id = '   ' WHERE id = 2")
            .unwrap();
        drop(conn);

        let views = build_materialized_views(&db, false).unwrap();
        assert!(views.contains(&"message_overview_mv".to_string()));
        assert!(views.contains(&"attachments_by_message_mv".to_string()));

        let conn = SqliteConnection::open_file(db.display().to_string()).unwrap();
        let overview_rows = conn
            .query_sync(
                "SELECT thread_id FROM message_overview_mv WHERE id = 2",
                &[],
            )
            .unwrap();
        let attach_rows = conn
            .query_sync(
                "SELECT thread_id FROM attachments_by_message_mv WHERE message_id = 2",
                &[],
            )
            .unwrap();

        assert!(
            overview_rows[0]
                .get_named::<Option<String>>("thread_id")
                .is_ok_and(|tid| tid.is_none()),
            "blank thread IDs should normalize to NULL in message_overview_mv"
        );
        assert!(
            attach_rows[0]
                .get_named::<Option<String>>("thread_id")
                .is_ok_and(|tid| tid.is_none()),
            "blank thread IDs should normalize to NULL in attachments_by_message_mv"
        );
    }

    #[test]
    fn conformance_views_structure() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mcp-agent-mail-conformance/tests/conformance/fixtures/share");

        let source = fixture_dir.join("minimal.sqlite3");
        if !source.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("views_test.sqlite3");
        crate::create_sqlite_snapshot(&source, &snapshot, false).unwrap();

        let fts_ok = build_search_indexes(&snapshot).unwrap();
        let views = build_materialized_views(&snapshot, fts_ok).unwrap();

        assert!(views.contains(&"message_overview_mv".to_string()));
        assert!(views.contains(&"attachments_by_message_mv".to_string()));

        // Verify overview has expected columns
        let conn = SqliteConnection::open_file(snapshot.display().to_string()).unwrap();
        let rows = conn
            .query_sync("PRAGMA table_info(message_overview_mv)", &[])
            .unwrap();
        let columns: Vec<String> = rows
            .iter()
            .map(|r| r.get_named::<String>("name").unwrap())
            .collect();
        for expected in [
            "id",
            "project_id",
            "thread_id",
            "subject",
            "importance",
            "ack_required",
            "created_ts",
            "sender_name",
            "body_length",
            "attachment_count",
            "latest_snippet",
            "recipients",
        ] {
            assert!(
                columns.contains(&expected.to_string()),
                "message_overview_mv should have column: {expected}"
            );
        }
    }
}
