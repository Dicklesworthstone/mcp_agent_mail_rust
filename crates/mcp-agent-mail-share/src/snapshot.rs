//! Step 1: SQLite snapshot creation via SQL-level dump and restore.
//!
//! Creates an atomic, clean C-SQLite copy of the source database suitable for
//! offline manipulation (scoping, scrubbing, finalization with FTS5/VACUUM).
//!
//! The source DB may be in FrankenSQLite format (used at runtime), which is
//! not file-level compatible with C SQLite.  Instead of a byte-level file
//! copy we read schema + data through the FrankenSQLite driver and re-create
//! them in a fresh C-SQLite file.

use std::path::{Path, PathBuf};

use mcp_agent_mail_db::DbConn;
use sqlmodel_core::Value;
use sqlmodel_sqlite::SqliteConnection;

use crate::ShareError;

/// Known tables produced by the `mcp-agent-mail-db` schema.
///
/// Order matters: tables with foreign-key references must come after the
/// tables they reference so that data can be inserted without violating
/// constraints (when `PRAGMA foreign_keys = ON`).
///
/// Each entry: (table_name, has_id_primary_key, column_names).
const KNOWN_TABLES: &[(&str, bool, &[&str])] = &[
    ("projects", true, &["id", "slug", "human_key", "created_at"]),
    (
        "products",
        true,
        &["id", "product_uid", "name", "created_at"],
    ),
    (
        "product_project_links",
        true,
        &["id", "product_id", "project_id", "created_at"],
    ),
    (
        "agents",
        true,
        &[
            "id",
            "project_id",
            "name",
            "program",
            "model",
            "task_description",
            "inception_ts",
            "last_active_ts",
            "attachments_policy",
            "contact_policy",
        ],
    ),
    (
        "messages",
        true,
        &[
            "id",
            "project_id",
            "sender_id",
            "thread_id",
            "subject",
            "body_md",
            "importance",
            "ack_required",
            "created_ts",
            "attachments",
        ],
    ),
    (
        "message_recipients",
        false,
        &["message_id", "agent_id", "kind", "read_ts", "ack_ts"],
    ),
    (
        "file_reservations",
        true,
        &[
            "id",
            "project_id",
            "agent_id",
            "path_pattern",
            "exclusive",
            "reason",
            "created_ts",
            "expires_ts",
            "released_ts",
        ],
    ),
    (
        "agent_links",
        true,
        &[
            "id",
            "a_project_id",
            "a_agent_id",
            "b_project_id",
            "b_agent_id",
            "status",
            "reason",
            "created_ts",
            "updated_ts",
            "expires_ts",
        ],
    ),
    (
        "project_sibling_suggestions",
        true,
        &[
            "id",
            "project_a_id",
            "project_b_id",
            "score",
            "status",
            "rationale",
            "created_ts",
            "evaluated_ts",
            "confirmed_ts",
            "dismissed_ts",
        ],
    ),
];

/// Create a snapshot of the source SQLite database at `destination`.
///
/// 1. Opens source DB with FrankenSQLite (runtime driver).
/// 2. If `checkpoint` is true, runs `PRAGMA wal_checkpoint(TRUNCATE)`.
/// 3. Transfers schema + data to a fresh C-SQLite destination file.
///
/// Returns the destination path on success.
///
/// # Errors
///
/// - [`ShareError::SnapshotSourceNotFound`] if `source` does not exist.
/// - [`ShareError::SnapshotDestinationExists`] if `destination` already exists.
/// - [`ShareError::Sqlite`] on any SQLite error.
/// - [`ShareError::Io`] on filesystem errors.
pub fn create_sqlite_snapshot(
    source: &Path,
    destination: &Path,
    checkpoint: bool,
) -> Result<PathBuf, ShareError> {
    // Validate source exists
    if !source.exists() {
        return Err(ShareError::SnapshotSourceNotFound {
            path: source.display().to_string(),
        });
    }

    // Resolve destination to absolute path
    let dest = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()?.join(destination)
    };

    // Never overwrite
    if dest.exists() {
        return Err(ShareError::SnapshotDestinationExists {
            path: dest.display().to_string(),
        });
    }

    // Create parent dirs
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let source_str = source.display().to_string();

    // Create destination with C SQLite
    let dest_str = dest.display().to_string();
    let dst_conn = SqliteConnection::open_file(&dest_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot create destination DB {dest_str}: {e}"),
    })?;

    // Try FrankenSQLite first (runtime format), fall back to C SQLite.
    // Runtime databases are created by FrankenSQLite which produces files that
    // C SQLite cannot read.  Fixture/external databases are C SQLite and
    // FrankenSQLite may not be able to read those.
    let frank_ok = match DbConn::open_file(&source_str) {
        Ok(src) => {
            if checkpoint {
                let _ = src.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)");
            }
            // Probe with a known table to confirm we can actually read.
            match src.query_sync("SELECT \"id\" FROM \"projects\" LIMIT 1", &[]) {
                Ok(_) => {
                    transfer_tables_frank(&src, &dst_conn)?;
                    true
                }
                Err(_) => false,
            }
        }
        Err(_) => false,
    };

    if !frank_ok {
        // Fall back to C SQLite for the source (e.g. fixture files).
        let src_c = SqliteConnection::open_file(&source_str).map_err(|e| ShareError::Sqlite {
            message: format!("cannot open source DB {source_str}: {e}"),
        })?;
        if checkpoint {
            let _ = src_c.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)");
        }
        transfer_tables_c(&src_c, &dst_conn)?;
    }

    Ok(dest)
}

/// Transfer tables from a FrankenSQLite source to a C-SQLite destination.
fn transfer_tables_frank(src: &DbConn, dst: &SqliteConnection) -> Result<(), ShareError> {
    for &(table, has_id, columns) in KNOWN_TABLES {
        let col_list = columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");

        // Probe whether the table exists in the source.
        let probe = format!("SELECT {col_list} FROM \"{table}\" LIMIT 1");
        if src.query_sync(&probe, &[]).is_err() {
            continue;
        }

        create_dst_table(dst, table, has_id, columns)?;
        let (insert_sql, placeholders_count) = build_insert(table, columns, &col_list);

        let mut last_id: i64 = -1;
        loop {
            let (select_sql, params): (String, Vec<Value>) = if has_id {
                (
                    format!(
                        "SELECT {col_list} FROM \"{table}\" WHERE \"id\" > ?1 \
                         ORDER BY \"id\" ASC LIMIT 1000"
                    ),
                    vec![Value::BigInt(last_id)],
                )
            } else {
                (format!("SELECT {col_list} FROM \"{table}\""), vec![])
            };

            let rows = src
                .query_sync(&select_sql, &params)
                .map_err(|e| ShareError::Sqlite {
                    message: format!("SELECT from {table} failed: {e}"),
                })?;

            if rows.is_empty() {
                break;
            }

            for row in &rows {
                let values: Vec<Value> = columns
                    .iter()
                    .map(|c| row.get_by_name(c).cloned().unwrap_or(Value::Null))
                    .collect();
                if has_id && let Some(val) = row.get_by_name("id") {
                    last_id = match val {
                        Value::BigInt(v) => *v,
                        Value::Int(v) => i64::from(*v),
                        _ => last_id,
                    };
                }
                dst.execute_sync(&insert_sql, &values)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("INSERT into {table} failed: {e}"),
                    })?;
            }
            if !has_id {
                break;
            }
        }
        let _ = placeholders_count;
    }
    Ok(())
}

/// Transfer tables from a C-SQLite source to a C-SQLite destination.
fn transfer_tables_c(src: &SqliteConnection, dst: &SqliteConnection) -> Result<(), ShareError> {
    for &(table, has_id, columns) in KNOWN_TABLES {
        let col_list = columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");

        // Probe whether the table exists in the source.
        let probe = format!("SELECT {col_list} FROM \"{table}\" LIMIT 1");
        if src.query_sync(&probe, &[]).is_err() {
            continue;
        }

        create_dst_table(dst, table, has_id, columns)?;
        let (insert_sql, _) = build_insert(table, columns, &col_list);

        let mut last_id: i64 = -1;
        loop {
            let (select_sql, params): (String, Vec<Value>) = if has_id {
                (
                    format!(
                        "SELECT {col_list} FROM \"{table}\" WHERE \"id\" > ?1 \
                         ORDER BY \"id\" ASC LIMIT 1000"
                    ),
                    vec![Value::BigInt(last_id)],
                )
            } else {
                (format!("SELECT {col_list} FROM \"{table}\""), vec![])
            };

            let rows = src
                .query_sync(&select_sql, &params)
                .map_err(|e| ShareError::Sqlite {
                    message: format!("SELECT from {table} failed: {e}"),
                })?;

            if rows.is_empty() {
                break;
            }

            for row in &rows {
                let values: Vec<Value> = columns
                    .iter()
                    .map(|c| row.get_by_name(c).cloned().unwrap_or(Value::Null))
                    .collect();
                if has_id && let Some(val) = row.get_by_name("id") {
                    last_id = match val {
                        Value::BigInt(v) => *v,
                        Value::Int(v) => i64::from(*v),
                        _ => last_id,
                    };
                }
                dst.execute_sync(&insert_sql, &values)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("INSERT into {table} failed: {e}"),
                    })?;
            }
            if !has_id {
                break;
            }
        }
    }
    Ok(())
}

/// Create a table in the C-SQLite destination.
fn create_dst_table(
    dst: &SqliteConnection,
    table: &str,
    has_id: bool,
    columns: &[&str],
) -> Result<(), ShareError> {
    let col_defs: Vec<String> = columns
        .iter()
        .map(|c| {
            if *c == "id" {
                format!("\"{c}\" INTEGER PRIMARY KEY")
            } else {
                format!("\"{c}\"")
            }
        })
        .collect();

    let pk_suffix = if !has_id && table == "message_recipients" {
        ", PRIMARY KEY(\"message_id\", \"agent_id\")"
    } else {
        ""
    };

    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{table}\" ({}{pk_suffix})",
        col_defs.join(", ")
    );
    dst.execute_raw(&create_sql)
        .map_err(|e| ShareError::Sqlite {
            message: format!("CREATE TABLE {table} failed: {e}"),
        })
}

/// Build INSERT OR REPLACE SQL and return it with placeholder count.
fn build_insert(table: &str, columns: &[&str], col_list: &str) -> (String, usize) {
    let placeholders = (0..columns.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT OR REPLACE INTO \"{table}\" ({col_list}) VALUES ({placeholders})");
    (sql, columns.len())
}

/// Full snapshot preparation pipeline.
///
/// 1. Create snapshot
/// 2. Apply project scope
/// 3. Scrub data
/// 4. Finalize (FTS, materialized views, performance indexes, VACUUM)
pub fn create_snapshot_context(
    source: &Path,
    snapshot_path: &Path,
    project_filters: &[String],
    scrub_preset: crate::ScrubPreset,
) -> Result<SnapshotContext, ShareError> {
    create_sqlite_snapshot(source, snapshot_path, true)?;
    let scope = crate::apply_project_scope(snapshot_path, project_filters)?;
    let scrub_summary = crate::scrub_snapshot(snapshot_path, scrub_preset)?;
    let finalize = crate::finalize_export_db(snapshot_path)?;

    Ok(SnapshotContext {
        snapshot_path: snapshot_path.to_path_buf(),
        scope,
        scrub_summary,
        fts_enabled: finalize.fts_enabled,
    })
}

/// Context returned by the snapshot preparation pipeline.
#[derive(Debug, Clone)]
pub struct SnapshotContext {
    pub snapshot_path: PathBuf,
    pub scope: crate::scope::ProjectScopeResult,
    pub scrub_summary: crate::scrub::ScrubSummary,
    pub fts_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_source_not_found() {
        let result = create_sqlite_snapshot(
            Path::new("/nonexistent/db.sqlite3"),
            Path::new("/tmp/dest.sqlite3"),
            true,
        );
        assert!(matches!(
            result,
            Err(ShareError::SnapshotSourceNotFound { .. })
        ));
    }

    #[test]
    fn snapshot_creates_valid_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        // Create a minimal source DB with FrankenSQLite (like runtime).
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'hello', '/test', 0)")
            .unwrap();
        drop(conn);

        // Snapshot it (converts FrankenSQLite → C SQLite).
        let result = create_sqlite_snapshot(&source, &dest, false);
        assert!(result.is_ok());
        assert!(dest.exists());

        // Verify data in copy using C SQLite.
        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let name: String = rows[0].get_named("slug").unwrap();
        assert_eq!(name, "hello");

        // Verify integrity on the C SQLite copy.
        let rows = copy_conn.query_sync("PRAGMA integrity_check", &[]).unwrap();
        let result: String = rows[0].get_named("integrity_check").unwrap();
        assert_eq!(result, "ok");
    }

    #[test]
    fn snapshot_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        // Create source
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);
        std::fs::write(&dest, b"existing").unwrap();

        let result = create_sqlite_snapshot(&source, &dest, false);
        assert!(matches!(
            result,
            Err(ShareError::SnapshotDestinationExists { .. })
        ));
    }

    /// Full pipeline integration: snapshot → scope → scrub → finalize →
    /// attachments → chunk → scaffold → sign → verify
    #[test]
    fn full_pipeline_integration() {
        use crate::bundle::{
            bundle_attachments, compute_viewer_sri, export_viewer_data, maybe_chunk_database,
            write_bundle_scaffolding,
        };
        use crate::crypto::{sign_manifest, verify_bundle};
        use crate::hosting::detect_hosting_hints;
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();

        // 1. Create a seeded source database with FrankenSQLite (like runtime).
        let source = dir.path().join("source.sqlite3");
        let conn = DbConn::open_file(source.display().to_string()).unwrap();
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
             kind TEXT DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY(message_id, agent_id))",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER, \
             agent_id INTEGER, path_pattern TEXT, exclusive INTEGER DEFAULT 1, \
             reason TEXT DEFAULT '', created_ts TEXT DEFAULT '', expires_ts TEXT DEFAULT '', \
             released_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER, \
             a_agent_id INTEGER, b_project_id INTEGER, b_agent_id INTEGER, \
             status TEXT DEFAULT 'pending', reason TEXT DEFAULT '', \
             created_ts TEXT DEFAULT '', updated_ts TEXT DEFAULT '', expires_ts TEXT)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'myproj', '/test/proj', '')")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO agents VALUES (1, 1, 'Alice', 'claude-code', 'opus', 'testing', '', '', 'auto', 'auto')",
        ).unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 1, 1, 'T1', 'Hello', 'Body text with api_key=SECRET123', \
             'normal', 0, '2026-01-01', '[{\"type\":\"file\",\"path\":\"test.txt\",\"media_type\":\"text/plain\"}]')",
        ).unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        drop(conn);

        // Create an attachment file
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("test.txt"), b"attachment content").unwrap();

        // 2. Snapshot (FrankenSQLite → C SQLite conversion)
        let snapshot = dir.path().join("snapshot.sqlite3");
        create_sqlite_snapshot(&source, &snapshot, false).unwrap();
        assert!(snapshot.exists());

        // 3. Project scope (keep all)
        let scope = crate::apply_project_scope(&snapshot, &[]).unwrap();
        assert!(!scope.projects.is_empty());

        // 4. Scrub (standard preset)
        let scrub = crate::scrub_snapshot(&snapshot, crate::ScrubPreset::Standard).unwrap();
        assert!(scrub.secrets_replaced >= 0);

        // 5. Finalize (FTS + views + indexes)
        let finalize = crate::finalize_export_db(&snapshot).unwrap();

        // 6. Bundle attachments
        let output = dir.path().join("bundle");
        std::fs::create_dir_all(&output).unwrap();
        let att_manifest = bundle_attachments(
            &snapshot,
            &output,
            &storage,
            crate::INLINE_ATTACHMENT_THRESHOLD,
            crate::DETACH_ATTACHMENT_THRESHOLD,
            true,
        )
        .unwrap();
        assert_eq!(att_manifest.stats.inline, 1); // small file → inline

        // 7. Copy DB to bundle
        let db_dest = output.join("mailbox.sqlite3");
        std::fs::copy(&snapshot, &db_dest).unwrap();
        let db_bytes = std::fs::read(&db_dest).unwrap();
        let db_sha256 = hex::encode(Sha256::digest(&db_bytes));

        // 8. Maybe chunk (should not chunk — small DB)
        let chunk = maybe_chunk_database(
            &db_dest,
            &output,
            crate::DEFAULT_CHUNK_THRESHOLD,
            crate::DEFAULT_CHUNK_SIZE,
        )
        .unwrap();
        assert!(chunk.is_none());

        // 9. Viewer data export
        let viewer_data = export_viewer_data(&snapshot, &output, finalize.fts_enabled).unwrap();
        assert!(output.join("viewer/data/messages.json").exists());

        // 10. Viewer SRI
        let sri = compute_viewer_sri(&output);

        // 11. Hosting hints
        let hints = detect_hosting_hints(&output);

        // 12. Write scaffolding
        write_bundle_scaffolding(
            &output,
            &scope,
            &scrub,
            &att_manifest,
            chunk.as_ref(),
            crate::DEFAULT_CHUNK_THRESHOLD,
            crate::DEFAULT_CHUNK_SIZE,
            &hints,
            finalize.fts_enabled,
            "mailbox.sqlite3",
            &db_sha256,
            db_bytes.len() as u64,
            Some(&viewer_data),
            &sri,
        )
        .unwrap();
        assert!(output.join("manifest.json").exists());
        assert!(output.join("README.md").exists());
        assert!(output.join("HOW_TO_DEPLOY.md").exists());
        assert!(output.join("index.html").exists());
        assert!(output.join("_headers").exists());
        assert!(output.join(".nojekyll").exists());

        // 13. Verify manifest.json is valid JSON with sorted keys
        let manifest_text = std::fs::read_to_string(output.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["schema_version"], "0.1.0");
        assert_eq!(manifest["database"]["path"], "mailbox.sqlite3");
        assert_eq!(manifest["database"]["sha256"], db_sha256);

        // Keys should be alphabetically sorted
        if let Some(obj) = manifest.as_object() {
            let keys: Vec<&String> = obj.keys().collect();
            let mut sorted_keys = keys.clone();
            sorted_keys.sort();
            assert_eq!(keys, sorted_keys, "top-level keys should be sorted");
        }

        // 14. Sign and verify
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, [42u8; 32]).unwrap();
        sign_manifest(
            &output.join("manifest.json"),
            &key_path,
            &output.join("manifest.sig.json"),
            false,
        )
        .unwrap();

        let verify = verify_bundle(&output, None).unwrap();
        assert!(verify.signature_checked);
        assert!(verify.signature_verified);
    }
}
