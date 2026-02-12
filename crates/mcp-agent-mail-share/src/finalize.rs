//! Steps 4–7: FTS, materialized views, performance indexes, and export finalization.
//!
//! Operates on a scoped+scrubbed snapshot in-place.

use std::path::Path;

use crate::ShareError;

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

    // Check if thread_id column exists
    let has_thread_id = column_exists(&conn, "messages", "thread_id")?;

    // Create FTS5 virtual table
    let create_sql = "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(\
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
        // FTS5 not available — not an error, just means no search
        if msg.contains("fts5")
            || msg.contains("unknown tokenizer")
            || msg.contains("no such module")
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
        "subject",
        "body",
        "importance",
        "project_slug",
        "thread_key",
        "created_ts",
    ] {
        if !column_exists(&conn, "fts_messages", col)? {
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

    // Populate from messages + projects
    let insert_sql = if has_thread_id {
        "INSERT INTO fts_messages(rowid, subject, body, importance, project_slug, thread_key, created_ts) \
         SELECT \
             m.id, \
             COALESCE(m.subject, ''), \
             COALESCE(m.body_md, ''), \
             COALESCE(m.importance, ''), \
             COALESCE(p.slug, ''), \
             CASE \
                 WHEN m.thread_id IS NULL OR m.thread_id = '' THEN printf('msg:%d', m.id) \
                 ELSE m.thread_id \
             END, \
             COALESCE(m.created_ts, '') \
         FROM messages AS m \
         LEFT JOIN projects AS p ON p.id = m.project_id"
    } else {
        "INSERT INTO fts_messages(rowid, subject, body, importance, project_slug, thread_key, created_ts) \
         SELECT \
             m.id, \
             COALESCE(m.subject, ''), \
             COALESCE(m.body_md, ''), \
             COALESCE(m.importance, ''), \
             COALESCE(p.slug, ''), \
             printf('msg:%d', m.id), \
             COALESCE(m.created_ts, '') \
         FROM messages AS m \
         LEFT JOIN projects AS p ON p.id = m.project_id"
    };

    conn.execute_raw(insert_sql)
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
    let mut created = Vec::new();

    // Ensure recipients table exists to satisfy LEFT JOINs in view creation.
    conn.execute_raw(
        "CREATE TABLE IF NOT EXISTS message_recipients (message_id INTEGER, agent_id INTEGER)",
    )
    .map_err(sql_err)?;

    let has_thread_id = column_exists(&conn, "messages", "thread_id")?;
    let has_sender_id = column_exists(&conn, "messages", "sender_id")?;

    // --- message_overview_mv ---
    conn.execute_raw("DROP TABLE IF EXISTS message_overview_mv")
        .map_err(sql_err)?;

    let thread_expr = if has_thread_id {
        "m.thread_id"
    } else {
        "printf('msg:%d', m.id)"
    };
    let (sender_expr, join_clause) = if has_sender_id {
        (
            "a.name AS sender_name",
            "JOIN agents a ON m.sender_id = a.id",
        )
    } else {
        ("'' AS sender_name", "")
    };

    let overview_sql = format!(
        "CREATE TABLE message_overview_mv AS \
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
             json_array_length(m.attachments) AS attachment_count, \
             SUBSTR(COALESCE(m.body_md, ''), 1, 280) AS latest_snippet, \
             COALESCE(r.recipients, '') AS recipients \
         FROM messages m \
         {join_clause} \
         LEFT JOIN ( \
             SELECT \
                 mr.message_id, \
                 GROUP_CONCAT(COALESCE(ag.name, ''), ', ') AS recipients \
             FROM message_recipients mr \
             LEFT JOIN agents ag ON ag.id = mr.agent_id \
             GROUP BY mr.message_id \
         ) r ON r.message_id = m.id \
         ORDER BY m.created_ts DESC"
    );
    conn.execute_raw(&overview_sql).map_err(sql_err)?;

    for idx in [
        "CREATE INDEX idx_msg_overview_created ON message_overview_mv(created_ts DESC)",
        "CREATE INDEX idx_msg_overview_thread ON message_overview_mv(thread_id, created_ts DESC)",
        "CREATE INDEX idx_msg_overview_project ON message_overview_mv(project_id, created_ts DESC)",
        "CREATE INDEX idx_msg_overview_importance ON message_overview_mv(importance, created_ts DESC)",
    ] {
        conn.execute_raw(idx).map_err(sql_err)?;
    }
    created.push("message_overview_mv".to_string());

    // --- attachments_by_message_mv ---
    conn.execute_raw("DROP TABLE IF EXISTS attachments_by_message_mv")
        .map_err(sql_err)?;

    let attach_thread_expr = if has_thread_id { "m.thread_id" } else { "NULL" };
    let attach_sql = format!(
        "CREATE TABLE attachments_by_message_mv AS \
         SELECT \
             m.id AS message_id, \
             m.project_id, \
             {attach_thread_expr} AS thread_id, \
             m.created_ts, \
             json_extract(value, '$.type') AS attachment_type, \
             json_extract(value, '$.media_type') AS media_type, \
             json_extract(value, '$.path') AS path, \
             CAST(json_extract(value, '$.bytes') AS INTEGER) AS size_bytes \
         FROM messages m, \
              json_each(m.attachments) \
         WHERE m.attachments != '[]'"
    );
    conn.execute_raw(&attach_sql).map_err(sql_err)?;

    for idx in [
        "CREATE INDEX idx_attach_by_msg ON attachments_by_message_mv(message_id)",
        "CREATE INDEX idx_attach_by_type ON attachments_by_message_mv(attachment_type, created_ts DESC)",
        "CREATE INDEX idx_attach_by_project ON attachments_by_message_mv(project_id, created_ts DESC)",
    ] {
        conn.execute_raw(idx).map_err(sql_err)?;
    }
    created.push("attachments_by_message_mv".to_string());

    // --- fts_search_overview_mv (only if FTS5 available) ---
    if fts_enabled {
        conn.execute_raw("DROP TABLE IF EXISTS fts_search_overview_mv")
            .map_err(sql_err)?;

        // Use m.id for the rowid column since id INTEGER PRIMARY KEY aliases rowid.
        let fts_overview_sql = if has_sender_id {
            "CREATE TABLE fts_search_overview_mv AS \
             SELECT \
                 m.id AS rowid, \
                 m.id, \
                 m.subject, \
                 m.created_ts, \
                 m.importance, \
                 a.name AS sender_name, \
                 SUBSTR(m.body_md, 1, 200) AS snippet \
             FROM messages m \
             JOIN agents a ON m.sender_id = a.id \
             ORDER BY m.created_ts DESC"
        } else {
            "CREATE TABLE fts_search_overview_mv AS \
             SELECT \
                 m.id AS rowid, \
                 m.id, \
                 m.subject, \
                 m.created_ts, \
                 m.importance, \
                 '' AS sender_name, \
                 SUBSTR(m.body_md, 1, 200) AS snippet \
             FROM messages m \
             ORDER BY m.created_ts DESC"
        };

        match conn.execute_raw(fts_overview_sql) {
            Ok(_) => {
                for idx in [
                    "CREATE INDEX idx_fts_overview_rowid ON fts_search_overview_mv(rowid)",
                    "CREATE INDEX idx_fts_overview_created ON fts_search_overview_mv(created_ts DESC)",
                ] {
                    conn.execute_raw(idx).map_err(sql_err)?;
                }
                created.push("fts_search_overview_mv".to_string());
            }
            Err(_) => {
                // FTS5 not available at view creation time — skip gracefully
            }
        }
    }

    Ok(created)
}

/// Step 6: Create performance indexes (lowercase columns + covering indexes).
pub fn create_performance_indexes(snapshot_path: &Path) -> Result<Vec<String>, ShareError> {
    let conn = open_conn(snapshot_path)?;
    let mut indexes = Vec::new();

    let has_sender_id = column_exists(&conn, "messages", "sender_id")?;
    let has_thread_id = column_exists(&conn, "messages", "thread_id")?;

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
        conn.execute_raw(&format!("DROP TRIGGER IF EXISTS {trigger}"))
            .map_err(sql_err)?;
    }

    // Add lowercase columns (suppress error if already exist)
    let _ = conn.execute_raw("ALTER TABLE messages ADD COLUMN subject_lower TEXT");
    let _ = conn.execute_raw("ALTER TABLE messages ADD COLUMN sender_lower TEXT");

    // Populate lowercase columns
    if has_sender_id {
        conn.execute_raw(
            "UPDATE messages SET \
                 subject_lower = LOWER(COALESCE(subject, '')), \
                 sender_lower = LOWER(\
                     COALESCE(\
                         (SELECT name FROM agents WHERE agents.id = messages.sender_id), \
                         ''\
                     )\
                 )",
        )
        .map_err(sql_err)?;
    } else {
        conn.execute_raw(
            "UPDATE messages SET \
                 subject_lower = LOWER(COALESCE(subject, '')), \
                 sender_lower = ''",
        )
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

/// Step 7: Finalize snapshot for export (journal mode, page size, VACUUM, ANALYZE).
///
/// Must be called last, after all schema modifications.
pub fn finalize_snapshot_for_export(snapshot_path: &Path) -> Result<(), ShareError> {
    let conn = open_conn(snapshot_path)?;

    conn.execute_raw("PRAGMA journal_mode=DELETE")
        .map_err(sql_err)?;
    conn.execute_raw("PRAGMA page_size=1024").map_err(sql_err)?;
    conn.execute_raw("VACUUM").map_err(sql_err)?;
    conn.execute_raw("PRAGMA analysis_limit=400")
        .map_err(sql_err)?;
    conn.execute_raw("ANALYZE").map_err(sql_err)?;
    conn.execute_raw("PRAGMA optimize").map_err(sql_err)?;

    Ok(())
}

/// Run steps 4–7 in sequence on a scoped+scrubbed snapshot.
pub fn finalize_export_db(snapshot_path: &Path) -> Result<FinalizeResult, ShareError> {
    let fts_enabled = build_search_indexes(snapshot_path)?;
    let views_created = build_materialized_views(snapshot_path, fts_enabled)?;
    let indexes_created = create_performance_indexes(snapshot_path)?;
    finalize_snapshot_for_export(snapshot_path)?;

    Ok(FinalizeResult {
        fts_enabled,
        views_created,
        indexes_created,
    })
}

// --- helpers ---

fn open_conn(path: &Path) -> Result<mcp_agent_mail_db::DbConn, ShareError> {
    let path_str = path.display().to_string();
    mcp_agent_mail_db::DbConn::open_file(&path_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot open {path_str}: {e}"),
    })
}

fn sql_err(e: impl std::fmt::Display) -> ShareError {
    ShareError::Sqlite {
        message: e.to_string(),
    }
}

fn column_exists(
    conn: &mcp_agent_mail_db::DbConn,
    table: &str,
    column: &str,
) -> Result<bool, ShareError> {
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table})"), &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA table_info({table}) failed: {e}"),
        })?;
    for row in &rows {
        let name: String = row.get_named("name").unwrap_or_default();
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
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
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).unwrap();

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

    #[test]
    fn fts_creates_and_populates() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok, "FTS5 should be available");

        // Verify data in FTS table
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS cnt FROM fts_messages", &[])
            .unwrap();
        let count: i64 = rows[0].get_named("cnt").unwrap();
        assert_eq!(count, 2, "should have 2 FTS entries");

        // Verify FTS search works
        let results = conn
            .query_sync(
                "SELECT rowid FROM fts_messages WHERE fts_messages MATCH 'Hello'",
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
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE VIRTUAL TABLE fts_messages USING fts5(subject, body, project_slug UNINDEXED)",
        )
        .unwrap();
        drop(conn);

        let fts_ok = build_search_indexes(&db).unwrap();
        assert!(fts_ok, "FTS5 should be available");

        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
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
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
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
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
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
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
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
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
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
    fn full_finalize_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let db = create_test_db(dir.path());

        let result = finalize_export_db(&db).unwrap();
        assert!(result.fts_enabled);
        assert!(!result.views_created.is_empty());
        assert!(!result.indexes_created.is_empty());

        // Verify everything is queryable
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();

        // FTS search
        let rows = conn
            .query_sync(
                "SELECT rowid FROM fts_messages WHERE fts_messages MATCH 'test'",
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
        let conn = mcp_agent_mail_db::DbConn::open_file(db.display().to_string()).unwrap();
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
            "finalize_export_db should succeed even with legacy FTS triggers"
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
        let conn =
            mcp_agent_mail_db::DbConn::open_file(snapshot.display().to_string()).unwrap();
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
        let conn =
            mcp_agent_mail_db::DbConn::open_file(snapshot.display().to_string()).unwrap();
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
