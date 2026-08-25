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
use std::sync::atomic::{AtomicI64, Ordering};

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
/// Returns the resulting persisted floor when the allocator row was advanced
/// or repaired, or `None` when the database was already at or ahead of the
/// archive with exactly one authoritative sequence row.
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
    advance_messages_id_floor_with(
        archive_max_id,
        |sql, params| {
            conn.query_sync(sql, params)
                .map_err(|error| error.to_string())
        },
        |sql| conn.execute_raw(sql).map_err(|error| error.to_string()),
    )
}

/// Advance the live FrankenSQLite allocator without opening the mailbox main
/// inode through canonical SQLite.
pub(crate) fn advance_messages_id_floor_franken(
    conn: &crate::DbConn,
    archive_max_id: Option<i64>,
) -> DbResult<Option<i64>> {
    advance_messages_id_floor_with(
        archive_max_id,
        |sql, params| {
            conn.query_sync(sql, params)
                .map_err(|error| error.to_string())
        },
        |sql| conn.execute_raw(sql).map_err(|error| error.to_string()),
    )
}

fn advance_messages_id_floor_with<Q, E>(
    archive_max_id: Option<i64>,
    query: Q,
    execute_raw: E,
) -> DbResult<Option<i64>>
where
    Q: Fn(&str, &[Value]) -> Result<Vec<sqlmodel_core::Row>, String>,
    E: Fn(&str) -> Result<(), String>,
{
    let Some(archive_max) = archive_max_id else {
        return Ok(None);
    };
    if archive_max <= 0 {
        return Ok(None);
    }

    let db_rows = query("SELECT COALESCE(MAX(id), 0) AS max_id FROM messages", &[])
        .map_err(|e| DbError::Sqlite(format!("id_floor: read MAX(id): {e}")))?;
    let db_max_id = db_rows
        .first()
        .ok_or_else(|| DbError::Sqlite("id_floor: MAX(id) returned no row".to_string()))?
        .get_named::<i64>("max_id")
        .map_err(|e| DbError::Sqlite(format!("id_floor: decode MAX(id): {e}")))?;

    let seq_rows = query(
        "SELECT COUNT(*) AS row_count, COALESCE(MAX(seq), 0) AS seq \
         FROM sqlite_sequence WHERE name = 'messages'",
        &[],
    )
    .map_err(|e| DbError::Sqlite(format!("id_floor: read sqlite_sequence: {e}")))?;
    let seq_row = seq_rows.first().ok_or_else(|| {
        DbError::Sqlite("id_floor: sqlite_sequence aggregate returned no row".to_string())
    })?;
    let seq_row_count = seq_row
        .get_named::<i64>("row_count")
        .map_err(|e| DbError::Sqlite(format!("id_floor: decode sequence row count: {e}")))?;
    let seq_value = seq_row
        .get_named::<i64>("seq")
        .map_err(|e| DbError::Sqlite(format!("id_floor: decode sequence value: {e}")))?;

    let current_floor = db_max_id.max(seq_value);
    let desired_floor = current_floor.max(archive_max);
    if seq_row_count == 1 && seq_value >= desired_floor {
        // DB is already at or ahead of the archive; nothing to do.
        return Ok(None);
    }

    // sqlite_sequence does not declare `name` UNIQUE, so INSERT OR IGNORE can
    // create duplicate allocator rows. Repair cardinality and advance the
    // floor under one write transaction. The aggregate assignment preserves
    // a higher sequence committed after the reads above but before BEGIN.
    let repair_sql = format!(
        "BEGIN IMMEDIATE; \
         UPDATE sqlite_sequence \
            SET seq = (SELECT MAX(CASE WHEN seq > {desired_floor} \
                                       THEN seq ELSE {desired_floor} END) \
                         FROM sqlite_sequence WHERE name = 'messages') \
          WHERE name = 'messages'; \
         DELETE FROM sqlite_sequence \
          WHERE name = 'messages' \
            AND rowid <> (SELECT MIN(rowid) FROM sqlite_sequence \
                           WHERE name = 'messages'); \
         INSERT INTO sqlite_sequence (name, seq) \
              SELECT 'messages', {desired_floor} \
               WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence \
                                  WHERE name = 'messages'); \
         UPDATE sqlite_sequence \
            SET seq = CASE WHEN seq < {desired_floor} \
                           THEN {desired_floor} ELSE seq END \
          WHERE name = 'messages'; \
         COMMIT;"
    );
    if let Err(error) = execute_raw(&repair_sql) {
        let rollback = execute_raw("ROLLBACK;");
        let rollback_detail = rollback.err().map_or_else(String::new, |rollback_error| {
            format!("; rollback also failed: {rollback_error}")
        });
        return Err(DbError::Sqlite(format!(
            "id_floor: repair/advance sqlite_sequence: {error}{rollback_detail}"
        )));
    }

    let persisted_rows = query(
        "SELECT COUNT(*) AS row_count, COALESCE(MAX(seq), 0) AS seq \
         FROM sqlite_sequence WHERE name = 'messages'",
        &[],
    )
    .map_err(|e| DbError::Sqlite(format!("id_floor: verify sqlite_sequence repair: {e}")))?;
    let persisted_row = persisted_rows.first().ok_or_else(|| {
        DbError::Sqlite("id_floor: repaired sqlite_sequence returned no row".to_string())
    })?;
    let persisted_count = persisted_row
        .get_named::<i64>("row_count")
        .map_err(|e| DbError::Sqlite(format!("id_floor: decode repaired row count: {e}")))?;
    let persisted_floor = persisted_row
        .get_named::<i64>("seq")
        .map_err(|e| DbError::Sqlite(format!("id_floor: decode repaired sequence: {e}")))?;
    if persisted_count != 1 || persisted_floor < desired_floor {
        return Err(DbError::Sqlite(format!(
            "id_floor: allocator repair verification failed: row_count={persisted_count}, seq={persisted_floor}, required_floor={desired_floor}"
        )));
    }

    tracing::warn!(
        archive_max,
        db_max_id,
        previous_seq = seq_value,
        previous_sequence_rows = seq_row_count,
        new_seq = persisted_floor,
        "repaired or advanced the messages id allocator; subsequent INSERTs will remain strictly above the durable database/archive floor (mcp_agent_mail#160)"
    );

    Ok(Some(persisted_floor))
}

/// Process-wide, per-database monotonic message-id allocator
/// (mcp_agent_mail#176).
///
/// # Why this exists
///
/// Message ids are normally allocated by SQLite's `AUTOINCREMENT` and read
/// back from the inserted row. That is correct only while the live SQLite's
/// durable allocator state (`MAX(id)` / `sqlite_sequence`) reliably advances
/// across consecutive INSERTs. Issue #176 documented a state where it does
/// **not**: after a corruption recovery the live database is held *suspect*
/// by the `idx_agents_project_name_nocase` integrity false-positive (the #151
/// family) and falls back to the canonical engine, and in that mode the
/// durable high-water mark advances at startup but not per-write. The result
/// is that message `N+1` is handed the **same** id as message `N`, the
/// canonical-archive writer (correctly, per #130) rejects the duplicate
/// `__<id>.md` file, and the sticky durability latch refuses all further
/// writes — a *non-terminating* recovery.
///
/// This allocator makes id allocation reuse-proof regardless of which surface
/// is authoritative. It derives the next id as
/// `max(in_memory_high_water, db_floor, archive_max) + 1` **atomically per
/// allocation** (the fix direction the issue recommends), so two consecutive
/// allocations in one process can never collide even when the live SQLite's
/// durable state fails to advance between them.
///
/// The allocator is keyed by the shared connection pool's identity (see
/// [`DbPool::message_id_allocator`](crate::DbPool::message_id_allocator)), so
/// every `DbPool` wrapper of the same underlying database shares one
/// high-water mark — "persist/share the floor increment across pool
/// connections".
#[derive(Debug)]
pub struct MessageIdAllocator {
    /// The largest id this process has handed out for this database.
    /// `0` means "no id allocated yet" (the first allocation seeds it).
    high_water: AtomicI64,
    /// The authoritative archive floor, or a negative initialization state.
    /// Publishing the value rather than a separate boolean prevents another
    /// writer from observing "seeded" without also observing the actual floor.
    archive_floor: AtomicI64,
}

const ARCHIVE_FLOOR_UNSEEDED: i64 = -1;
const ARCHIVE_FLOOR_SEEDING: i64 = -2;

struct ArchiveSeedGuard<'a> {
    archive_floor: &'a AtomicI64,
    armed: bool,
}

impl Drop for ArchiveSeedGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // The scanner panicked before publishing a floor. Let a later
            // allocation retry instead of leaving every writer spinning.
            self.archive_floor
                .store(ARCHIVE_FLOOR_UNSEEDED, Ordering::Release);
        }
    }
}

impl MessageIdAllocator {
    /// Create a fresh allocator with no ids handed out yet. Callers should
    /// resolve a *shared* allocator per database via
    /// [`DbPool::message_id_allocator`](crate::DbPool::message_id_allocator)
    /// rather than constructing one directly.
    #[must_use]
    pub fn new() -> Self {
        Self {
            high_water: AtomicI64::new(0),
            archive_floor: AtomicI64::new(ARCHIVE_FLOOR_UNSEEDED),
        }
    }

    /// Whether the archive max still needs to be folded into the high-water
    /// mark. This is exposed for diagnostics; callers should let
    /// [`MessageIdAllocator::allocate`] decide whether to invoke its archive
    /// scanner so scan completion cannot be published prematurely.
    #[must_use]
    pub fn needs_archive_seed(&self) -> bool {
        self.archive_floor.load(Ordering::Acquire) < 0
    }

    fn archive_floor_with<F>(&self, archive_scan: F) -> i64
    where
        F: FnOnce() -> i64,
    {
        let mut archive_scan = Some(archive_scan);
        loop {
            let state = self.archive_floor.load(Ordering::Acquire);
            if state >= 0 {
                return state;
            }
            if state == ARCHIVE_FLOOR_SEEDING {
                std::thread::yield_now();
                continue;
            }
            debug_assert_eq!(state, ARCHIVE_FLOOR_UNSEEDED);
            if self
                .archive_floor
                .compare_exchange(
                    ARCHIVE_FLOOR_UNSEEDED,
                    ARCHIVE_FLOOR_SEEDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }

            let mut guard = ArchiveSeedGuard {
                archive_floor: &self.archive_floor,
                armed: true,
            };
            let scan = archive_scan
                .take()
                .expect("archive scanner is consumed only by the elected initializer");
            let floor = scan().max(0);
            self.archive_floor.store(floor, Ordering::Release);
            guard.armed = false;
            return floor;
        }
    }

    /// Allocate the next message id.
    ///
    /// * `db_floor` — `max(MAX(id) FROM messages, sqlite_sequence.seq)` read
    ///   from the live database for the messages table.
    /// * `archive_scan` — returns the maximum message id found in the
    ///   canonical archive, or `0` when no messages were found. It is invoked
    ///   exactly once while the archive floor is initialized. Concurrent
    ///   first allocations wait for and reuse that same published floor.
    ///
    /// Returns an id strictly greater than `db_floor`, the archive floor, and any
    /// id previously handed out by this allocator in this process. The
    /// returned id is what the caller MUST use for both the DB row and the
    /// canonical archive filename so the two never diverge.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Internal`] when every positive SQLite row id has
    /// been exhausted. Reissuing `i64::MAX` would violate the uniqueness
    /// guarantee, so exhaustion must fail closed.
    pub fn allocate<F>(&self, db_floor: i64, archive_scan: F) -> DbResult<i64>
    where
        F: FnOnce() -> i64,
    {
        let archive_floor = self.archive_floor_with(archive_scan);
        let mut current = self.high_water.load(Ordering::Acquire);
        loop {
            let base = current.max(db_floor).max(archive_floor).max(0);
            let Some(next) = base.checked_add(1) else {
                return Err(DbError::Internal(
                    "message id allocator exhausted the positive i64 row-id range".to_string(),
                ));
            };
            match self.high_water.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(next),
                Err(observed) => current = observed,
            }
        }
    }

    /// The largest id handed out so far (`0` if none). Test/diagnostic use.
    #[must_use]
    pub fn current_high_water(&self) -> i64 {
        self.high_water.load(Ordering::Acquire)
    }
}

impl Default for MessageIdAllocator {
    fn default() -> Self {
        Self::new()
    }
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
        assert_eq!(rows.len(), 1, "messages must have one allocator row");
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

    #[test]
    fn advance_messages_id_floor_consolidates_duplicate_sequence_rows() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("duplicate-floor.db");
        let conn = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 subject TEXT NOT NULL
             );
             INSERT INTO messages (id, subject) VALUES (10, 'existing');
             INSERT INTO sqlite_sequence (name, seq) VALUES ('messages', 7);",
        )
        .unwrap();

        assert_eq!(
            advance_messages_id_floor(&conn, Some(25)).unwrap(),
            Some(25)
        );
        let rows = conn
            .query_sync(
                "SELECT seq FROM sqlite_sequence WHERE name = 'messages'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "duplicate allocator rows must be removed");
        assert_eq!(rows[0].get_named::<i64>("seq").unwrap(), 25);

        conn.execute_raw("INSERT INTO messages (subject) VALUES ('next');")
            .unwrap();
        let rows = conn
            .query_sync("SELECT MAX(id) AS max_id FROM messages", &[])
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("max_id").unwrap(), 26);
    }

    #[test]
    fn stale_id_floor_advance_preserves_newer_committed_sequence() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stale-floor.db");
        let conn = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 subject TEXT NOT NULL
             );
             INSERT INTO messages (id, subject) VALUES (10, 'existing');",
        )
        .unwrap();
        let newer = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        let injected = std::cell::Cell::new(false);

        let result = advance_messages_id_floor_with(
            Some(25),
            |sql, params| {
                conn.query_sync(sql, params)
                    .map_err(|error| error.to_string())
            },
            |sql| {
                if !injected.replace(true) {
                    newer
                        .execute_raw(
                            "UPDATE sqlite_sequence SET seq = 100 WHERE name = 'messages';",
                        )
                        .map_err(|error| error.to_string())?;
                }
                conn.execute_raw(sql).map_err(|error| error.to_string())
            },
        )
        .unwrap();

        assert_eq!(result, Some(100));
        let rows = conn
            .query_sync(
                "SELECT seq FROM sqlite_sequence WHERE name = 'messages'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_named::<i64>("seq").unwrap(), 100);
    }

    #[test]
    fn allocator_hands_out_strictly_increasing_ids() {
        let alloc = MessageIdAllocator::new();
        // First allocation seeds from the larger of db_floor / archive_seed.
        assert_eq!(alloc.allocate(1128, || 1128).unwrap(), 1129);
        // db_floor stays at 1128 (the durable allocator failed to advance,
        // exactly the #176 suspect-mode scenario), but the in-memory
        // high-water carries forward so the next id is still fresh.
        assert_eq!(alloc.allocate(1128, || 0).unwrap(), 1130);
        assert_eq!(alloc.allocate(1128, || 0).unwrap(), 1131);
        assert_eq!(alloc.current_high_water(), 1131);
    }

    #[test]
    fn allocator_reuse_proof_when_durable_floor_regresses() {
        // Models the catastrophic case: the live SQLite is suspect, so its
        // MAX(id)/sqlite_sequence not only fail to advance but can read back
        // *below* an id we already handed out (e.g. a write that never landed
        // durably). The allocator must still never re-issue an id.
        let alloc = MessageIdAllocator::new();
        let first = alloc.allocate(1128, || 1128).unwrap();
        assert_eq!(first, 1129);
        // db_floor regresses to 1000; archive_seed is 0. Without the in-memory
        // guard this would re-issue 1001 and collide with the archive.
        let second = alloc.allocate(1000, || 0).unwrap();
        assert!(
            second > first,
            "allocator re-issued or regressed: first={first} second={second}"
        );
        assert_eq!(second, 1130);
    }

    #[test]
    fn allocator_starts_at_one_for_empty_db_and_archive() {
        let alloc = MessageIdAllocator::new();
        assert_eq!(alloc.allocate(0, || 0).unwrap(), 1);
        assert_eq!(alloc.allocate(0, || 0).unwrap(), 2);
    }

    #[test]
    fn allocator_archive_seed_gate_flips_once() {
        let alloc = MessageIdAllocator::new();
        assert!(alloc.needs_archive_seed());
        assert_eq!(alloc.allocate(0, || 0).unwrap(), 1);
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn allocator_scans_archive_only_until_floor_is_published() {
        let alloc = MessageIdAllocator::new();
        let scans = std::cell::Cell::new(0_u32);
        let first = alloc
            .allocate(10, || {
                scans.set(scans.get() + 1);
                50
            })
            .unwrap();
        let second = alloc
            .allocate(10, || {
                scans.set(scans.get() + 1);
                100
            })
            .unwrap();

        assert_eq!(first, 51);
        assert_eq!(second, 52);
        assert_eq!(scans.get(), 1);
    }

    #[test]
    fn concurrent_allocator_waits_for_authoritative_archive_floor() {
        let alloc = std::sync::Arc::new(MessageIdAllocator::new());
        let (scan_started_tx, scan_started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_scan_tx, release_scan_rx) = std::sync::mpsc::sync_channel(0);

        let first_alloc = std::sync::Arc::clone(&alloc);
        let first = std::thread::spawn(move || {
            first_alloc.allocate(0, || {
                scan_started_tx.send(()).unwrap();
                release_scan_rx.recv().unwrap();
                100
            })
        });
        scan_started_rx.recv().unwrap();

        let (second_started_tx, second_started_rx) = std::sync::mpsc::sync_channel(0);
        let (second_result_tx, second_result_rx) = std::sync::mpsc::sync_channel(0);
        let second_alloc = std::sync::Arc::clone(&alloc);
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            let result = second_alloc.allocate(0, || {
                panic!("concurrent allocator ran a second archive scan")
            });
            second_result_tx.send(result).unwrap();
        });
        second_started_rx.recv().unwrap();

        assert!(
            second_result_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "concurrent allocation completed before the authoritative archive floor was published"
        );
        release_scan_tx.send(()).unwrap();

        let first_id = first.join().unwrap().unwrap();
        let second_id = second_result_rx.recv().unwrap().unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(first_id.min(second_id), 101);
        assert_eq!(first_id.max(second_id), 102);
        second.join().unwrap();
    }

    #[test]
    fn allocator_fails_closed_when_row_id_space_is_exhausted() {
        let alloc = MessageIdAllocator::new();
        let error = alloc.allocate(i64::MAX, || 0).unwrap_err();
        assert!(
            error.to_string().contains("exhausted"),
            "unexpected exhaustion error: {error}"
        );
        assert_eq!(alloc.current_high_water(), 0);
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn allocator_retries_archive_seed_after_scanner_panic() {
        let alloc = MessageIdAllocator::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = alloc.allocate(0, || panic!("injected archive scan panic"));
        }));
        assert!(panic.is_err());
        assert!(alloc.needs_archive_seed());
        assert_eq!(alloc.allocate(0, || 40).unwrap(), 41);
        assert!(!alloc.needs_archive_seed());
    }
}
