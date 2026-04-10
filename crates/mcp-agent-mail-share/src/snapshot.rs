//! Step 1: SQLite snapshot creation via SQL-level dump and restore.
//!
//! Creates an atomic, clean FrankenSQLite copy of the source database suitable for
//! offline manipulation (scoping, scrubbing, finalization with FTS5/VACUUM).
//!
//! Instead of a byte-level file copy we read schema + data through the runtime
//! driver and re-create them in a fresh destination file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use mcp_agent_mail_db::DbConn;
use sqlmodel_core::Value;

use crate::ShareError;

#[cfg(test)]
// Historical alias name retained in tests; this still uses FrankenSQLite `DbConn`.
type SqliteConnection = DbConn;

/// Known tables produced by the `mcp-agent-mail-db` schema.
///
/// Order matters: tables with foreign-key references must come after the
/// tables they reference so that data can be inserted without violating
/// constraints (when `PRAGMA foreign_keys = ON`).
#[derive(Clone, Copy)]
struct KnownTable {
    name: &'static str,
    page_by_column: Option<&'static str>,
    primary_key_columns: &'static [&'static str],
    columns: &'static [&'static str],
}

const KNOWN_TABLES: &[KnownTable] = &[
    KnownTable {
        name: "projects",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &["id", "slug", "human_key", "created_at"],
    },
    KnownTable {
        name: "products",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &["id", "product_uid", "name", "created_at"],
    },
    KnownTable {
        name: "product_project_links",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &["id", "product_id", "project_id", "created_at"],
    },
    KnownTable {
        name: "agents",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
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
    },
    KnownTable {
        name: "messages",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "project_id",
            "sender_id",
            "thread_id",
            "subject",
            "body_md",
            "importance",
            "ack_required",
            "created_ts",
            "recipients_json",
            "attachments",
        ],
    },
    KnownTable {
        name: "message_recipients",
        page_by_column: None,
        primary_key_columns: &["message_id", "agent_id", "kind"],
        columns: &["message_id", "agent_id", "kind", "read_ts", "ack_ts"],
    },
    KnownTable {
        name: "file_reservations",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
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
    },
    KnownTable {
        name: "file_reservation_releases",
        page_by_column: Some("reservation_id"),
        primary_key_columns: &["reservation_id"],
        columns: &["reservation_id", "released_ts"],
    },
    KnownTable {
        name: "agent_links",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
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
    },
    KnownTable {
        name: "project_sibling_suggestions",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
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
    },
    KnownTable {
        name: "inbox_stats",
        page_by_column: Some("agent_id"),
        primary_key_columns: &["agent_id"],
        columns: &[
            "agent_id",
            "total_count",
            "unread_count",
            "ack_pending_count",
            "last_message_ts",
        ],
    },
    KnownTable {
        name: "tool_metrics_snapshots",
        page_by_column: Some("id"),
        primary_key_columns: &["id"],
        columns: &[
            "id",
            "collected_ts",
            "tool_name",
            "calls",
            "errors",
            "cluster",
            "capabilities_json",
            "complexity",
            "latency_avg_ms",
            "latency_min_ms",
            "latency_max_ms",
            "latency_p50_ms",
            "latency_p95_ms",
            "latency_p99_ms",
            "latency_is_slow",
        ],
    },
];

/// Create a snapshot of the source SQLite database at `destination`.
///
/// 1. Opens source DB with FrankenSQLite (runtime driver).
/// 2. If `checkpoint` is true, runs `PRAGMA wal_checkpoint(TRUNCATE)`.
/// 3. Transfers schema + data to a fresh destination file.
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
    rebuild_sqlite_snapshot_with_pragmas(source, destination, checkpoint, &[])
}

pub(crate) fn rebuild_sqlite_snapshot_with_pragmas(
    source: &Path,
    destination: &Path,
    checkpoint: bool,
    destination_pragmas: &[&str],
) -> Result<PathBuf, ShareError> {
    let source = crate::resolve_share_sqlite_path(source);

    match std::fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(ShareError::Validation {
                message: format!("snapshot source is not a real file: {}", source.display()),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ShareError::SnapshotSourceNotFound {
                path: source.display().to_string(),
            });
        }
        Err(error) => return Err(ShareError::Io(error)),
    }

    // Resolve destination to absolute path
    let dest = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()?.join(destination)
    };

    // Never overwrite
    match std::fs::symlink_metadata(&dest) {
        Ok(_) => {
            return Err(ShareError::SnapshotDestinationExists {
                path: dest.display().to_string(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ShareError::Io(error)),
    }

    // Create parent dirs
    if let Some(parent) = dest.parent() {
        ensure_real_directory(parent)?;
    }

    if checkpoint {
        mcp_agent_mail_db::pool::wal_checkpoint_truncate_path(&source).map_err(|e| {
            ShareError::Sqlite {
                message: format!(
                    "cannot checkpoint source DB {} before snapshot: {e}",
                    source.display()
                ),
            }
        })?;
    }

    let source_str = source.display().to_string();

    // Create destination with FrankenSQLite. Page size must be chosen before
    // page 1 is initialized, so honor any requested destination page_size here.
    let dest_str = dest.display().to_string();
    let dst_conn = if let Some(page_size) = destination_page_size_bytes(destination_pragmas)? {
        DbConn::open_file_with_page_size(&dest_str, page_size).map_err(|e| ShareError::Sqlite {
            message: format!(
                "cannot create destination DB {dest_str} with page size {page_size}: {e}"
            ),
        })?
    } else {
        DbConn::open_file(&dest_str).map_err(|e| ShareError::Sqlite {
            message: format!("cannot create destination DB {dest_str}: {e}"),
        })?
    };
    for pragma in destination_pragmas {
        dst_conn
            .execute_raw(pragma)
            .map_err(|e| ShareError::Sqlite {
                message: format!("failed to apply destination pragma {pragma:?}: {e}"),
            })?;
    }

    let src = DbConn::open_file(&source_str).map_err(|e| ShareError::Sqlite {
        message: format!("cannot open source DB {source_str}: {e}"),
    })?;
    let transfer_result = transfer_tables_frank(&src, &dst_conn);
    let src_close_result = src.close_sync().map_err(|e| ShareError::Sqlite {
        message: format!("failed to close source DB {source_str}: {e}"),
    });
    let dst_close_result = dst_conn.close_sync().map_err(|e| ShareError::Sqlite {
        message: format!("failed to close destination DB {dest_str}: {e}"),
    });
    transfer_result?;
    src_close_result?;
    dst_close_result?;

    Ok(dest)
}

fn ensure_real_directory(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        use std::path::Component;

        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(std::io::Error::other(format!(
                    "refusing to create snapshot directory with parent traversal: {}",
                    path.display()
                )));
            }
            Component::Normal(part) => current.push(part),
        }

        if current.as_os_str().is_empty() {
            continue;
        }

        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(std::io::Error::other(format!(
                        "refusing to traverse symlinked snapshot directory {}",
                        current.display()
                    )));
                }
                if !metadata.file_type().is_dir() {
                    return Err(std::io::Error::other(format!(
                        "snapshot parent component is not a directory: {}",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Transfer tables from a source snapshot to a fresh destination database.
fn transfer_tables_frank(src: &DbConn, dst: &DbConn) -> Result<(), ShareError> {
    for table in KNOWN_TABLES {
        let source_columns = source_columns_frank(src, table.name)?;
        if source_columns.is_empty() {
            continue;
        }
        let available_columns = available_columns(table, &source_columns);
        if available_columns.is_empty() {
            continue;
        }

        create_dst_table(dst, table)?;
        let insert_sql = build_insert(table.name, table.columns);
        let select_columns = quoted_column_list(&available_columns);
        let page_by_column = table
            .page_by_column
            .filter(|column| source_columns.contains(*column));
        let mut last_page_value: i64 = -1;
        loop {
            let (select_sql, params): (String, Vec<Value>) =
                if let Some(page_by_column) = page_by_column {
                    (
                        format!(
                            "SELECT {select_columns} FROM \"{}\" WHERE \"{page_by_column}\" > ?1 \
                         ORDER BY \"{page_by_column}\" ASC LIMIT 1000",
                            table.name
                        ),
                        vec![Value::BigInt(last_page_value)],
                    )
                } else {
                    (
                        format!("SELECT {select_columns} FROM \"{}\"", table.name),
                        vec![],
                    )
                };

            let rows = src
                .query_sync(&select_sql, &params)
                .map_err(|e| ShareError::Sqlite {
                    message: format!("SELECT from {} failed: {e}", table.name),
                })?;

            if rows.is_empty() {
                break;
            }

            for row in &rows {
                let values: Vec<Value> = table
                    .columns
                    .iter()
                    .map(|c| {
                        row.get_by_name(c)
                            .cloned()
                            .or_else(|| snapshot_default_value(table.name, c))
                            .unwrap_or(Value::Null)
                    })
                    .collect();
                if let Some(page_by_column) = page_by_column {
                    last_page_value = extract_page_value(row, table.name, page_by_column)?;
                }
                dst.execute_sync(&insert_sql, &values)
                    .map_err(|e| ShareError::Sqlite {
                        message: format!("INSERT into {} failed: {e}", table.name),
                    })?;
            }
            if page_by_column.is_none() {
                break;
            }
        }
    }
    Ok(())
}

/// Create a table in the destination database.
fn create_dst_table(dst: &DbConn, table: &KnownTable) -> Result<(), ShareError> {
    let col_defs: Vec<String> = table.columns.iter().map(|c| format!("\"{c}\"")).collect();

    let pk_suffix = primary_key_suffix(table.primary_key_columns);

    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" ({}{pk_suffix})",
        table.name,
        col_defs.join(", ")
    );
    dst.execute_raw(&create_sql)
        .map_err(|e| ShareError::Sqlite {
            message: format!("CREATE TABLE {} failed: {e}", table.name),
        })
}

/// Build INSERT OR REPLACE SQL for a destination table.
fn build_insert(table: &str, columns: &[&str]) -> String {
    let col_list = quoted_column_list(columns);
    let placeholders = (0..columns.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT OR REPLACE INTO \"{table}\" ({col_list}) VALUES ({placeholders})")
}

fn destination_page_size_bytes(destination_pragmas: &[&str]) -> Result<Option<u32>, ShareError> {
    let mut requested = None;
    for pragma in destination_pragmas {
        let compact: String = pragma
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .collect();
        let compact = compact.trim_end_matches(';').to_ascii_lowercase();
        let Some(raw_value) = compact.strip_prefix("pragmapage_size=") else {
            continue;
        };
        let page_size = raw_value
            .parse::<u32>()
            .map_err(|_| ShareError::Validation {
                message: format!("invalid destination page_size pragma: {pragma}"),
            })?;
        requested = Some(page_size);
    }
    Ok(requested)
}

fn quoted_column_list(columns: &[&str]) -> String {
    columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn primary_key_suffix(primary_key_columns: &[&str]) -> String {
    if primary_key_columns.is_empty() {
        return String::new();
    }
    format!(", PRIMARY KEY({})", quoted_column_list(primary_key_columns))
}

fn source_columns_frank(src: &DbConn, table: &str) -> Result<HashSet<String>, ShareError> {
    let rows = src
        .query_sync(&format!("PRAGMA table_info(\"{table}\")"), &[])
        .map_err(|e| ShareError::Sqlite {
            message: format!("PRAGMA table_info({table}) failed: {e}"),
        })?;
    Ok(extract_column_names(&rows))
}

fn extract_column_names(rows: &[sqlmodel_core::Row]) -> HashSet<String> {
    rows.iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .collect()
}

fn extract_page_value(
    row: &sqlmodel_core::Row,
    table: &str,
    page_by_column: &str,
) -> Result<i64, ShareError> {
    let Some(val) = row.get_by_name(page_by_column) else {
        return Err(ShareError::Sqlite {
            message: format!("missing pagination column {page_by_column} while copying {table}"),
        });
    };
    match val {
        Value::BigInt(v) => Ok(*v),
        Value::Int(v) => Ok(i64::from(*v)),
        _ => Err(ShareError::Sqlite {
            message: format!("unexpected non-integer pagination column {table}.{page_by_column}"),
        }),
    }
}

fn available_columns<'a>(table: &'a KnownTable, source_columns: &HashSet<String>) -> Vec<&'a str> {
    table
        .columns
        .iter()
        .copied()
        .filter(|column| source_columns.contains(*column))
        .collect()
}

fn snapshot_default_value(table: &str, column: &str) -> Option<Value> {
    match (table, column) {
        ("messages", "recipients_json") => Some(Value::Text("{}".to_string())),
        _ => None,
    }
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

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_source_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_source = dir.path().join("real-source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        let conn = DbConn::open_file(real_source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);

        let linked_source = dir.path().join("linked-source.sqlite3");
        symlink(&real_source, &linked_source).unwrap();

        let err = create_sqlite_snapshot(&linked_source, &dest, false)
            .expect_err("symlinked sources must fail validation");
        assert!(matches!(err, ShareError::Validation { .. }));
        assert!(err.to_string().contains("real file"));
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

        // Snapshot it into a standalone SQLite bundle database.
        let result = create_sqlite_snapshot(&source, &dest, false);
        assert!(result.is_ok());
        assert!(dest.exists());

        // Verify data in the copied snapshot.
        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let name: String = rows[0].get_named("slug").unwrap();
        assert_eq!(name, "hello");

        // Verify integrity on the copied snapshot.
        let rows = copy_conn.query_sync("PRAGMA integrity_check", &[]).unwrap();
        let result: String = rows[0].get_named("integrity_check").unwrap();
        assert_eq!(result, "ok");
    }

    #[test]
    fn snapshot_uses_absolute_candidate_for_missing_relative_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("shadow-source.sqlite3");
        let dest = dir.path().join("shadow-dest.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO projects VALUES (1, 'shadow', '/shadow', 0)")
            .unwrap();
        drop(conn);

        let relative_source = PathBuf::from(source.strip_prefix("/").unwrap());
        assert!(!relative_source.exists());

        create_sqlite_snapshot(&relative_source, &dest, false).unwrap();
        assert!(dest.exists());
        assert!(!relative_source.exists());

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT slug FROM projects WHERE id = 1", &[])
            .unwrap();
        let slug: String = rows[0].get_named("slug").unwrap();
        assert_eq!(slug, "shadow");
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

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_destination_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at INTEGER)",
        )
        .unwrap();
        drop(conn);

        let linked_parent = dir.path().join("linked-parent");
        symlink(&outside, &linked_parent).unwrap();
        let dest = linked_parent.join("dest.sqlite3");

        let err = create_sqlite_snapshot(&source, &dest, false)
            .expect_err("symlinked destination parents must fail");
        assert!(matches!(err, ShareError::Io(_)));
        assert!(err.to_string().contains("symlinked snapshot directory"));
    }

    #[test]
    fn snapshot_preserves_runtime_recipient_and_reservation_state() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("runtime.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id INTEGER PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                recipients_json TEXT NOT NULL DEFAULT '{}', \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE file_reservation_releases (\
                reservation_id INTEGER PRIMARY KEY, \
                released_ts INTEGER NOT NULL\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE inbox_stats (\
                agent_id INTEGER PRIMARY KEY, \
                total_count INTEGER NOT NULL DEFAULT 0, \
                unread_count INTEGER NOT NULL DEFAULT 0, \
                ack_pending_count INTEGER NOT NULL DEFAULT 0, \
                last_message_ts INTEGER\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "CREATE TABLE tool_metrics_snapshots (\
                id INTEGER PRIMARY KEY, \
                collected_ts INTEGER NOT NULL, \
                tool_name TEXT NOT NULL, \
                calls INTEGER NOT NULL DEFAULT 0, \
                errors INTEGER NOT NULL DEFAULT 0, \
                cluster TEXT NOT NULL DEFAULT '', \
                capabilities_json TEXT NOT NULL DEFAULT '[]', \
                complexity TEXT NOT NULL DEFAULT 'unknown', \
                latency_avg_ms REAL, \
                latency_min_ms REAL, \
                latency_max_ms REAL, \
                latency_p50_ms REAL, \
                latency_p95_ms REAL, \
                latency_p99_ms REAL, \
                latency_is_slow INTEGER NOT NULL DEFAULT 0\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (\
                1, 7, 11, 'br-1', 'subject', 'body', 'high', 1, 12345, \
                '{\"to\":[\"Alice\"],\"cc\":[\"Bob\"],\"bcc\":[\"Carol\"]}', \
                '[{\"path\":\"a.txt\"}]'\
            )",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO file_reservation_releases VALUES (42, 9001)")
            .unwrap();
        conn.execute_raw("INSERT INTO inbox_stats VALUES (11, 5, 2, 1, 12345)")
            .unwrap();
        conn.execute_raw(
            "INSERT INTO tool_metrics_snapshots VALUES (\
                3, 222, 'send_message', 9, 1, 'messaging', '[\"attachments\"]', \
                'medium', 12.5, 8.0, 20.0, 11.0, 18.0, 19.0, 1\
            )",
        )
        .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT recipients_json, attachments FROM messages WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        let attachments: String = rows[0].get_named("attachments").unwrap();
        assert_eq!(
            recipients_json,
            "{\"to\":[\"Alice\"],\"cc\":[\"Bob\"],\"bcc\":[\"Carol\"]}"
        );
        assert_eq!(attachments, "[{\"path\":\"a.txt\"}]");

        let rows = copy_conn
            .query_sync(
                "SELECT released_ts FROM file_reservation_releases WHERE reservation_id = 42",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let released_ts: i64 = rows[0].get_named("released_ts").unwrap();
        assert_eq!(released_ts, 9001);

        let rows = copy_conn
            .query_sync(
                "SELECT total_count FROM inbox_stats WHERE agent_id = 11",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let total_count: i64 = rows[0].get_named("total_count").unwrap();
        assert_eq!(total_count, 5);

        let rows = copy_conn
            .query_sync(
                "SELECT tool_name, capabilities_json FROM tool_metrics_snapshots WHERE id = 3",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let tool_name: String = rows[0].get_named("tool_name").unwrap();
        let capabilities_json: String = rows[0].get_named("capabilities_json").unwrap();
        assert_eq!(tool_name, "send_message");
        assert_eq!(capabilities_json, "[\"attachments\"]");
    }

    #[test]
    fn snapshot_preserves_multiple_recipient_kinds_for_same_agent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("runtime.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE message_recipients (\
                message_id INTEGER NOT NULL, \
                agent_id INTEGER NOT NULL, \
                kind TEXT NOT NULL, \
                read_ts INTEGER, \
                ack_ts INTEGER, \
                PRIMARY KEY(message_id, agent_id, kind)\
            )",
        )
        .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 7, 'to', 111, NULL)")
            .unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 7, 'cc', NULL, 222)")
            .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT kind, read_ts, ack_ts \
                 FROM message_recipients \
                 WHERE message_id = 1 AND agent_id = 7 \
                 ORDER BY kind",
                &[],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "snapshot should preserve both recipient rows"
        );

        let cc_kind: String = rows[0].get_named("kind").unwrap();
        let cc_read_ts: Option<i64> = rows[0].get_named("read_ts").unwrap();
        let cc_ack_ts: Option<i64> = rows[0].get_named("ack_ts").unwrap();
        assert_eq!(cc_kind, "cc");
        assert_eq!(cc_read_ts, None);
        assert_eq!(cc_ack_ts, Some(222));

        let to_kind: String = rows[1].get_named("kind").unwrap();
        let to_read_ts: Option<i64> = rows[1].get_named("read_ts").unwrap();
        let to_ack_ts: Option<i64> = rows[1].get_named("ack_ts").unwrap();
        assert_eq!(to_kind, "to");
        assert_eq!(to_read_ts, Some(111));
        assert_eq!(to_ack_ts, None);
    }

    #[test]
    fn snapshot_keeps_legacy_messages_and_defaults_missing_recipients_json() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("legacy.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id INTEGER PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES (1, 7, 11, 'br-legacy', 'subject', 'body', 'normal', 0, 12345, '[]')",
        )
        .unwrap();
        drop(conn);

        create_sqlite_snapshot(&source, &dest, false).unwrap();

        let copy_conn = SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync(
                "SELECT subject, recipients_json FROM messages WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let subject: String = rows[0].get_named("subject").unwrap();
        let recipients_json: String = rows[0].get_named("recipients_json").unwrap();
        assert_eq!(subject, "subject");
        assert_eq!(recipients_json, "{}");
    }

    #[test]
    fn snapshot_errors_on_non_integer_pagination_key() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bad_key.sqlite3");
        let dest = dir.path().join("snapshot.sqlite3");

        let conn = DbConn::open_file(source.display().to_string()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (\
                id TEXT PRIMARY KEY, \
                project_id INTEGER, \
                sender_id INTEGER, \
                thread_id TEXT, \
                subject TEXT DEFAULT '', \
                body_md TEXT DEFAULT '', \
                importance TEXT DEFAULT 'normal', \
                ack_required INTEGER DEFAULT 0, \
                created_ts INTEGER DEFAULT 0, \
                recipients_json TEXT NOT NULL DEFAULT '{}', \
                attachments TEXT DEFAULT '[]'\
            )",
        )
        .unwrap();
        conn.execute_raw(
            "INSERT INTO messages VALUES ('oops', 7, 11, 'br-bad', 'subject', 'body', 'normal', 0, 12345, '{}', '[]')",
        )
        .unwrap();
        drop(conn);

        let err = create_sqlite_snapshot(&source, &dest, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unexpected non-integer pagination column messages.id"),
            "unexpected error: {msg}"
        );
    }

    /// Full pipeline integration: snapshot → scope/scrub/finalize →
    /// canonical bundle export → sign → verify
    #[test]
    fn full_pipeline_integration() {
        use crate::crypto::{sign_manifest, verify_bundle};
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

        let snapshot = dir.path().join("snapshot.sqlite3");
        let context =
            create_snapshot_context(&source, &snapshot, &[], crate::ScrubPreset::Standard).unwrap();
        assert!(context.snapshot_path.exists());
        assert!(!context.scope.projects.is_empty());
        assert!(context.scrub_summary.secrets_replaced >= 0);

        let output = dir.path().join("bundle");
        let export = crate::export_bundle_from_snapshot_context(
            &context,
            &output,
            &storage,
            &crate::BundleExportConfig {
                allow_absolute_attachment_paths: true,
                ..crate::BundleExportConfig::default()
            },
        )
        .unwrap();
        assert_eq!(export.attachment_manifest.stats.inline, 1); // small file → inline
        assert!(export.chunk_manifest.is_none());
        assert!(
            export
                .viewer_assets
                .iter()
                .any(|path| path == "viewer/index.html")
        );
        assert!(output.join("viewer/data/messages.json").exists());
        assert!(output.join("viewer/index.html").exists());
        assert!(output.join("viewer/pages/index.html").exists());
        assert!(export.static_render.pages_generated > 0);
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
        assert_eq!(manifest["database"]["sha256"], export.db_sha256);

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

    #[test]
    fn export_bundle_replaces_stale_owned_outputs_but_preserves_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();

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
            "INSERT INTO messages VALUES (1, 1, 1, 'T1', 'Hello', 'Body text', \
             'normal', 0, '2026-01-01', '[{\"type\":\"file\",\"path\":\"test.txt\",\"media_type\":\"text/plain\"}]')",
        ).unwrap();
        conn.execute_raw("INSERT INTO message_recipients VALUES (1, 1, 'to', NULL, NULL)")
            .unwrap();
        drop(conn);

        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("test.txt"), b"attachment content").unwrap();

        let snapshot = dir.path().join("snapshot.sqlite3");
        let context =
            create_snapshot_context(&source, &snapshot, &[], crate::ScrubPreset::Archive).unwrap();

        let output = dir.path().join("bundle");
        std::fs::create_dir_all(output.join("viewer/data")).unwrap();
        std::fs::create_dir_all(output.join("viewer/pages/messages")).unwrap();
        std::fs::create_dir_all(output.join("attachments")).unwrap();
        std::fs::create_dir_all(output.join("chunks")).unwrap();
        std::fs::write(output.join("viewer/obsolete.js"), "stale viewer asset").unwrap();
        std::fs::write(
            output.join("viewer/data/redaction_audit.json"),
            "{\"stale\":true}",
        )
        .unwrap();
        std::fs::write(
            output.join("viewer/pages/messages/999.html"),
            "<html>stale page</html>",
        )
        .unwrap();
        std::fs::write(output.join("attachments/stale.bin"), "stale attachment").unwrap();
        std::fs::write(output.join("chunks/00000.bin"), "stale chunk").unwrap();
        std::fs::write(output.join("chunks.sha256"), "stale checksum").unwrap();
        std::fs::write(
            output.join("mailbox.sqlite3.config.json"),
            "{\"stale\":true}",
        )
        .unwrap();
        std::fs::write(output.join("manifest.sig.json"), "{\"stale\":true}").unwrap();
        std::fs::write(output.join("mailbox.sqlite3-wal"), "stale wal").unwrap();
        std::fs::write(output.join("mailbox.sqlite3-shm"), "stale shm").unwrap();
        std::fs::write(output.join("keep.txt"), "preserve me").unwrap();

        let export = crate::export_bundle_from_snapshot_context(
            &context,
            &output,
            &storage,
            &crate::BundleExportConfig {
                allow_absolute_attachment_paths: true,
                inline_attachment_threshold: 1,
                detach_attachment_threshold: 1024,
                chunk_threshold: usize::MAX,
                ..crate::BundleExportConfig::default()
            },
        )
        .unwrap();

        assert!(export.chunk_manifest.is_none());
        assert!(output.join("viewer/index.html").exists());
        assert!(output.join("viewer/data/messages.json").exists());
        assert!(output.join("viewer/pages/messages/1.html").exists());
        assert!(output.join("attachments").exists());
        assert!(
            !output.join("viewer/obsolete.js").exists(),
            "stale viewer assets must be removed"
        );
        assert!(
            !output.join("viewer/data/redaction_audit.json").exists(),
            "stale optional data files must be removed when not regenerated"
        );
        assert!(
            !output.join("viewer/pages/messages/999.html").exists(),
            "stale rendered pages must be removed"
        );
        assert!(
            !output.join("attachments/stale.bin").exists(),
            "stale attachment files must be removed"
        );
        assert!(
            !output.join("chunks").exists(),
            "stale chunk directories must be removed when chunking is disabled"
        );
        assert!(!output.join("chunks.sha256").exists());
        assert!(!output.join("mailbox.sqlite3.config.json").exists());
        assert!(!output.join("manifest.sig.json").exists());
        assert!(!output.join("mailbox.sqlite3-wal").exists());
        assert!(!output.join("mailbox.sqlite3-shm").exists());
        assert_eq!(
            std::fs::read_to_string(output.join("keep.txt")).unwrap(),
            "preserve me"
        );
    }
}
