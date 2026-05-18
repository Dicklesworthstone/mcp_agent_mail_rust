//! Message-ID floor recovery (mcp_agent_mail#160).
//!
//! When automatic recovery fails to atomically promote a reconstructed
//! candidate database, the live SQLite can keep serving traffic from a
//! state where its `MAX(id)` is below `archive_latest_message_id`. New
//! INSERTs then re-use IDs that the archive already considers canonical,
//! producing the duplicate-canonical-file failure mode reported on the
//! original issue ("raw canonical files=3866 (duplicate files=56 across
//! 30 message id(s))").
//!
//! This module gives the pool warmup a belt-and-suspenders fix: on every
//! connection-pool open, scan the archive for the maximum message id,
//! compare it to the database's `MAX(id)` and `sqlite_sequence` row, and
//! advance `sqlite_sequence['messages'].seq` to the floor if the database
//! is behind. The next INSERT will then receive `floor + 1`, which is
//! guaranteed to be larger than anything in the archive.
//!
//! Safe to call on every startup — when the DB is already at or ahead of
//! the archive it's a no-op.

use std::path::Path;

use sqlmodel_core::Value;
use sqlmodel_sqlite::SqliteConnection;

use crate::error::{DbError, DbResult};

/// Scan the archive at `storage_root` for the maximum message id found
/// in any canonical message file. Returns `None` when no archive exists
/// or no canonical files were parsed.
///
/// The walk is bounded by the archive layout: only
/// `projects/*/messages/YYYY/MM/*.md` files are read, and only their
/// JSON frontmatter is parsed (not the body). This is deliberately
/// the same shape `archive_anomaly::collect_project_canonical_messages`
/// uses so the two scanners agree on what counts as "in the archive".
#[must_use]
pub fn max_message_id_in_archive(storage_root: &Path) -> Option<i64> {
    let projects_dir = storage_root.join("projects");
    let entries = std::fs::read_dir(&projects_dir).ok()?;
    let mut max_id: Option<i64> = None;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let messages = entry.path().join("messages");
        if let Some(candidate) = scan_messages_dir_max_id(&messages) {
            max_id = Some(match max_id {
                Some(current) => current.max(candidate),
                None => candidate,
            });
        }
    }
    max_id
}

fn scan_messages_dir_max_id(dir: &Path) -> Option<i64> {
    let mut max_id: Option<i64> = None;
    let years = std::fs::read_dir(dir).ok()?;
    for year in years.flatten() {
        let Ok(ft) = year.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let Some(year_name) = year
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if year_name.len() != 4 || !year_name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(months) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(mft) = month.file_type() else { continue };
            if !mft.is_dir() || mft.is_symlink() {
                continue;
            }
            let Some(month_name) = month
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if month_name.len() != 2 || !month_name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(month.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let Ok(fft) = file.file_type() else { continue };
                if !fft.is_file() || fft.is_symlink() {
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if let Some(id) = extract_message_id_from_frontmatter(&path) {
                    max_id = Some(match max_id {
                        Some(current) => current.max(id),
                        None => id,
                    });
                }
            }
        }
    }
    max_id
}

fn extract_message_id_from_frontmatter(path: &Path) -> Option<i64> {
    let content = std::fs::read_to_string(path).ok()?;
    // The canonical archive frontmatter format is `---json\n{...}\n---\n`
    // (NOT a markdown ```json``` fence). Reuse the same extractor the
    // archive_anomaly walker uses so the two scanners always agree on
    // which files are "in the archive" and what id they carry.
    let json_body = crate::archive_anomaly::extract_json_frontmatter(&content)?.trim();
    let parsed: serde_json::Value = serde_json::from_str(json_body).ok()?;
    parsed
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
}

/// Compare the database's current `messages` allocator floor (the larger
/// of `MAX(id) FROM messages` and `sqlite_sequence.seq` for the messages
/// table) against `archive_max_id`.
///
/// If the archive is ahead, advance `sqlite_sequence['messages'].seq` so
/// the next INSERT receives `archive_max_id + 1`.
///
/// Returns the new floor (the seq value persisted) when an advance
/// happened, or `None` when the database was already at or ahead of the
/// archive and no change was made.
///
/// # Errors
///
/// Returns `DbError::Sqlite` when the underlying queries fail. Missing
/// `sqlite_sequence` row for `messages` is treated as `seq = 0` and is
/// inserted as part of the advance (not an error).
pub fn advance_messages_id_floor(
    conn: &SqliteConnection,
    archive_max_id: Option<i64>,
) -> DbResult<Option<i64>> {
    let Some(archive_max) = archive_max_id else {
        return Ok(None);
    };
    if archive_max <= 0 {
        return Ok(None);
    }

    let db_max_id: i64 = conn
        .query_sync("SELECT COALESCE(MAX(id), 0) AS max_id FROM messages", &[])
        .map_err(|e| DbError::Sqlite(format!("id_floor: read MAX(id): {e}")))?
        .first()
        .and_then(|row| row.get_named("max_id").ok())
        .unwrap_or(0);

    let seq_value: i64 = conn
        .query_sync(
            "SELECT COALESCE(seq, 0) AS seq FROM sqlite_sequence WHERE name = 'messages'",
            &[],
        )
        .map_err(|e| DbError::Sqlite(format!("id_floor: read sqlite_sequence: {e}")))?
        .first()
        .and_then(|row| row.get_named("seq").ok())
        .unwrap_or(0);

    let current_floor = db_max_id.max(seq_value);
    if current_floor >= archive_max {
        // DB is already at or ahead of the archive; nothing to do.
        return Ok(None);
    }

    // Advance: ensure the sqlite_sequence row exists, then bump it to
    // `archive_max` so the next AUTOINCREMENT allocates `archive_max + 1`.
    // INSERT OR IGNORE first to create the row if missing, then UPDATE
    // unconditionally — INSERT OR REPLACE would clobber other tables
    // sharing sqlite_sequence rows.
    conn.execute_raw("INSERT OR IGNORE INTO sqlite_sequence (name, seq) VALUES ('messages', 0)")
        .map_err(|e| DbError::Sqlite(format!("id_floor: ensure sqlite_sequence row: {e}")))?;
    conn.execute_sync(
        "UPDATE sqlite_sequence SET seq = ? WHERE name = 'messages'",
        &[Value::BigInt(archive_max)],
    )
    .map_err(|e| DbError::Sqlite(format!("id_floor: advance sqlite_sequence: {e}")))?;

    tracing::warn!(
        archive_max,
        db_max_id,
        previous_seq = seq_value,
        new_seq = archive_max,
        "advanced messages id allocator: archive_latest_message_id > db_max(messages); \
         subsequent INSERTs will receive ids strictly greater than the archive (mcp_agent_mail#160)"
    );

    Ok(Some(archive_max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_canonical_message(
        root: &Path,
        project: &str,
        year: &str,
        month: &str,
        filename: &str,
        id: i64,
    ) {
        let dir = root
            .join("projects")
            .join(project)
            .join("messages")
            .join(year)
            .join(month);
        fs::create_dir_all(&dir).unwrap();
        // Use the canonical archive frontmatter format (---json ... ---),
        // matching what archive_anomaly and reconstruct read.
        let body =
            format!("---json\n{{\"id\": {id}, \"subject\": \"x\"}}\n---\n\n# subject\n\nbody");
        fs::write(dir.join(filename), body).unwrap();
    }

    #[test]
    fn max_message_id_in_archive_finds_max_across_projects_years_months() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_canonical_message(root, "proj-a", "2026", "01", "01__1.md", 1);
        write_canonical_message(root, "proj-a", "2026", "02", "15__3823.md", 3823);
        write_canonical_message(root, "proj-b", "2026", "05", "16__3846.md", 3846);
        write_canonical_message(root, "proj-b", "2026", "05", "16__400.md", 400);

        let max = max_message_id_in_archive(root);
        assert_eq!(max, Some(3846));
    }

    #[test]
    fn max_message_id_in_archive_returns_none_for_empty_root() {
        let dir = tempdir().unwrap();
        assert_eq!(max_message_id_in_archive(dir.path()), None);
    }

    #[test]
    fn max_message_id_in_archive_skips_non_year_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let bogus = root
            .join("projects")
            .join("proj")
            .join("messages")
            .join("notayear");
        fs::create_dir_all(&bogus).unwrap();
        fs::write(bogus.join("01__99.md"), "---json\n{\"id\":99}\n---\n").unwrap();
        // The malformed year dir should be skipped — nothing else is in the
        // archive — so the scanner returns None.
        assert_eq!(max_message_id_in_archive(root), None);
    }

    #[test]
    fn max_message_id_in_archive_ignores_files_without_canonical_frontmatter() {
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("projects/proj/messages/2026/05/body-only.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Body has a JSON-shaped code block but it isn't the canonical
        // `---json ... ---` frontmatter, so the parser must not pick it up.
        fs::write(
            &path,
            "# subject\n\n```json\n{\"id\": 999, \"subject\": \"not frontmatter\"}\n```\n",
        )
        .unwrap();

        assert_eq!(max_message_id_in_archive(dir.path()), None);
    }

    #[test]
    fn advance_messages_id_floor_bumps_sequence_and_next_insert() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("floor.db");
        let conn = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                subject TEXT NOT NULL
            )",
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO messages (id, subject) VALUES (?, ?)",
            &[Value::BigInt(10), Value::Text("existing".to_string())],
        )
        .unwrap();

        assert_eq!(
            advance_messages_id_floor(&conn, Some(25)).unwrap(),
            Some(25)
        );

        let rows = conn
            .query_sync(
                "SELECT seq AS seq FROM sqlite_sequence WHERE name = 'messages'",
                &[],
            )
            .unwrap();
        let seq = rows[0].get_named::<i64>("seq").unwrap();
        assert_eq!(seq, 25);

        conn.execute_sync(
            "INSERT INTO messages (subject) VALUES (?)",
            &[Value::Text("next".to_string())],
        )
        .unwrap();
        let rows = conn
            .query_sync("SELECT MAX(id) AS max_id FROM messages", &[])
            .unwrap();
        let max_id = rows[0].get_named::<i64>("max_id").unwrap();
        assert_eq!(max_id, 26);
    }
}
