//! Step 1: SQLite snapshot creation via WAL checkpoint + VACUUM INTO.
//!
//! Creates an atomic, clean copy of the source database suitable for
//! offline manipulation (scoping, scrubbing, etc.).

use std::path::{Path, PathBuf};

use crate::ShareError;

/// Create a snapshot of the source SQLite database at `destination`.
///
/// 1. Opens source DB in read-only mode.
/// 2. If `checkpoint` is true, runs `PRAGMA wal_checkpoint(PASSIVE)` to
///    flush as much WAL data as possible without blocking writers.
/// 3. Uses `VACUUM INTO` to atomically create a clean, compacted copy.
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

    // Open source connection (read-only is fine for snapshot)
    let source_str = source.display().to_string();
    let conn = sqlmodel_sqlite::SqliteConnection::open_file(&source_str).map_err(|e| {
        ShareError::Sqlite {
            message: format!("cannot open source DB {source_str}: {e}"),
        }
    })?;

    // Checkpoint WAL if requested
    if checkpoint {
        conn.execute_raw("PRAGMA wal_checkpoint(PASSIVE)")
            .map_err(|e| ShareError::Sqlite {
                message: format!("WAL checkpoint failed: {e}"),
            })?;
    }

    // VACUUM INTO creates an atomic, clean copy of the database.
    // Available since SQLite 3.27.0 (2019-02-07).
    // We must quote the path properly for SQL — use single-quote escaping.
    let dest_sql = dest.display().to_string().replace('\'', "''");
    conn.execute_raw(&format!("VACUUM INTO '{dest_sql}'"))
        .map_err(|e| ShareError::Sqlite {
            message: format!("VACUUM INTO failed: {e}"),
        })?;

    Ok(dest)
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
        assert!(matches!(result, Err(ShareError::SnapshotSourceNotFound { .. })));
    }

    #[test]
    fn snapshot_creates_valid_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        // Create a minimal source DB
        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(source.display().to_string()).unwrap();
        conn.execute_raw("CREATE TABLE test_data (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        conn.execute_raw("INSERT INTO test_data VALUES (1, 'hello')")
            .unwrap();
        drop(conn);

        // Snapshot it
        let result = create_sqlite_snapshot(&source, &dest, true);
        assert!(result.is_ok());
        assert!(dest.exists());

        // Verify data in copy
        let copy_conn =
            sqlmodel_sqlite::SqliteConnection::open_file(dest.display().to_string()).unwrap();
        let rows = copy_conn
            .query_sync("SELECT name FROM test_data WHERE id = 1", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let name: String = rows[0].get_named("name").unwrap();
        assert_eq!(name, "hello");
    }

    #[test]
    fn snapshot_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let dest = dir.path().join("dest.sqlite3");

        // Create source and dest
        let conn =
            sqlmodel_sqlite::SqliteConnection::open_file(source.display().to_string()).unwrap();
        conn.execute_raw("CREATE TABLE t (id INTEGER)").unwrap();
        drop(conn);
        std::fs::write(&dest, b"existing").unwrap();

        let result = create_sqlite_snapshot(&source, &dest, true);
        assert!(matches!(
            result,
            Err(ShareError::SnapshotDestinationExists { .. })
        ));
    }
}
