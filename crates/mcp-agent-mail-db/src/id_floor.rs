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

use std::future::Future;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use asupersync::runtime::JoinError;
use asupersync::sync::{LockError, Mutex as AsyncMutex, OnceCell, OwnedMutexGuard};
use asupersync::{CancelReason, Cx, Outcome, PanicPayload};
use sqlmodel_core::Value;
use sqlmodel_sqlite::SqliteConnection;

use crate::error::{DbError, DbResult};

/// Maximum prefix read from any canonical message while locating its JSON
/// frontmatter. Message bodies can be arbitrarily large and are irrelevant to
/// id allocation, so the scanner fails closed when the closing frontmatter
/// marker is not present within this bound.
const MAX_FRONTMATTER_PREFIX_BYTES: u64 = 256 * 1024;

/// Scan the archive at `storage_root` for the maximum message id found
/// in any canonical message file. Returns `Ok(None)` when no archive exists
/// or no canonical files are present.
///
/// The walk is bounded by the archive layout: only
/// `projects/*/messages/YYYY/MM/*.md` files are read, and only their
/// JSON frontmatter is parsed (not the body). This is deliberately
/// the same shape `archive_anomaly::collect_project_canonical_messages`
/// uses so the two scanners agree on what counts as "in the archive".
/// # Errors
///
/// Returns an error for every directory-walk, file-read, UTF-8, frontmatter,
/// JSON, or message-id failure encountered in the canonical archive layout.
/// An incomplete scan is never safe to publish as an authoritative zero floor.
pub fn max_message_id_in_archive(storage_root: &Path) -> DbResult<Option<i64>> {
    let projects_dir = storage_root.join("projects");
    let Some(entries) = read_optional_dir(&projects_dir)? else {
        return Ok(None);
    };
    let mut max_id: Option<i64> = None;
    for entry in entries {
        let entry = entry.map_err(|error| archive_scan_error(&projects_dir, "walk", error))?;
        let ft = entry
            .file_type()
            .map_err(|error| archive_scan_error(&entry.path(), "read file type", error))?;
        if ft.is_symlink() {
            return Err(archive_scan_error(
                &entry.path(),
                "walk canonical project",
                "symlinks are not authoritative archive directories",
            ));
        }
        if !ft.is_dir() {
            continue;
        }
        let messages = entry.path().join("messages");
        if let Some(candidate) = scan_messages_dir_max_id(&messages)? {
            max_id = Some(match max_id {
                Some(current) => current.max(candidate),
                None => candidate,
            });
        }
    }
    Ok(max_id)
}

fn read_optional_dir(path: &Path) -> DbResult<Option<std::fs::ReadDir>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(archive_scan_error(path, "read metadata", error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(archive_scan_error(
            path,
            "walk canonical archive directory",
            "symlinks are not authoritative archive directories",
        ));
    }
    if !metadata.is_dir() {
        return Err(archive_scan_error(
            path,
            "walk canonical archive directory",
            "expected a directory",
        ));
    }
    std::fs::read_dir(path)
        .map(Some)
        .map_err(|error| archive_scan_error(path, "read directory", error))
}

fn archive_scan_error(path: &Path, operation: &str, error: impl std::fmt::Display) -> DbError {
    DbError::Internal(format!(
        "message id archive scan: {operation} {}: {error}",
        path.display()
    ))
}

fn scan_messages_dir_max_id(dir: &Path) -> DbResult<Option<i64>> {
    let mut max_id: Option<i64> = None;
    let Some(years) = read_optional_dir(dir)? else {
        return Ok(None);
    };
    for year in years {
        let year = year.map_err(|error| archive_scan_error(dir, "walk", error))?;
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
        let ft = year
            .file_type()
            .map_err(|error| archive_scan_error(&year.path(), "read file type", error))?;
        if ft.is_symlink() {
            return Err(archive_scan_error(
                &year.path(),
                "walk canonical message year",
                "symlinks are not authoritative archive directories",
            ));
        }
        if !ft.is_dir() {
            return Err(archive_scan_error(
                &year.path(),
                "walk canonical message year",
                "expected a directory",
            ));
        }
        let year_path = year.path();
        let months = std::fs::read_dir(&year_path)
            .map_err(|error| archive_scan_error(&year_path, "read directory", error))?;
        for month in months {
            let month = month.map_err(|error| archive_scan_error(&year_path, "walk", error))?;
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
            let mft = month
                .file_type()
                .map_err(|error| archive_scan_error(&month.path(), "read file type", error))?;
            if mft.is_symlink() {
                return Err(archive_scan_error(
                    &month.path(),
                    "walk canonical message month",
                    "symlinks are not authoritative archive directories",
                ));
            }
            if !mft.is_dir() {
                return Err(archive_scan_error(
                    &month.path(),
                    "walk canonical message month",
                    "expected a directory",
                ));
            }
            let month_path = month.path();
            let files = std::fs::read_dir(&month_path)
                .map_err(|error| archive_scan_error(&month_path, "read directory", error))?;
            for file in files {
                let file =
                    file.map_err(|error| archive_scan_error(&month_path, "walk", error))?;
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let fft = file
                    .file_type()
                    .map_err(|error| archive_scan_error(&path, "read file type", error))?;
                if fft.is_symlink() {
                    return Err(archive_scan_error(
                        &path,
                        "walk canonical message file",
                        "symlinks are not authoritative archive files",
                    ));
                }
                if !fft.is_file() {
                    return Err(archive_scan_error(
                        &path,
                        "walk canonical message file",
                        "expected a regular file",
                    ));
                }
                let id = extract_message_id_from_frontmatter(&path)?;
                max_id = Some(match max_id {
                    Some(current) => current.max(id),
                    None => id,
                });
            }
        }
    }
    Ok(max_id)
}

fn extract_message_id_from_frontmatter(path: &Path) -> DbResult<i64> {
    let file = mcp_agent_mail_core::disk::open_regular_file_no_follow(path)
        .map_err(|error| {
            archive_scan_error(path, "open canonical message without following", error)
        })?;
    let mut prefix = Vec::with_capacity(8 * 1024);
    let mut reader = BufReader::new(file).take(MAX_FRONTMATTER_PREFIX_BYTES + 1);
    let frontmatter_end = loop {
        let bytes_before_line = prefix.len();
        let bytes_read = reader
            .read_until(b'\n', &mut prefix)
            .map_err(|error| archive_scan_error(path, "read canonical frontmatter", error))?;
        if prefix.len() as u64 > MAX_FRONTMATTER_PREFIX_BYTES {
            return Err(archive_scan_error(
                path,
                "parse canonical message",
                format!(
                    "frontmatter exceeds the {MAX_FRONTMATTER_PREFIX_BYTES}-byte scan bound"
                ),
            ));
        }
        if bytes_read == 0 {
            return Err(archive_scan_error(
                path,
                "parse canonical message",
                "missing canonical ---json frontmatter or closing marker",
            ));
        }
        let line = &prefix[bytes_before_line..];
        if bytes_before_line == 0 && line != b"---json\n" && line != b"---json\r\n" {
            return Err(archive_scan_error(
                path,
                "parse canonical message",
                "missing canonical ---json frontmatter header",
            ));
        }
        if bytes_before_line > 0 && (line == b"---\n" || line == b"---\r\n" || line == b"---") {
            break prefix.len();
        }
    };
    let content = std::str::from_utf8(&prefix[..frontmatter_end])
        .map_err(|error| archive_scan_error(path, "decode canonical frontmatter as UTF-8", error))?;

    // The canonical archive frontmatter format is `---json\n{...}\n---\n`
    // (NOT a markdown ```json``` fence). Reuse the same extractor the
    // archive_anomaly walker uses so the two scanners always agree on
    // which files are "in the archive" and what id they carry.
    let json_body = crate::archive_anomaly::extract_json_frontmatter(content)
        .ok_or_else(|| archive_scan_error(path, "parse canonical message", "missing frontmatter"))?
        .trim();
    let parsed: serde_json::Value = serde_json::from_str(json_body)
        .map_err(|error| archive_scan_error(path, "parse canonical frontmatter JSON", error))?;
    parsed
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            archive_scan_error(
                path,
                "parse canonical message id",
                "id must be a positive i64",
            )
        })
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

    // Take the writer reservation before observing either durable floor. A
    // pre-transaction read can become stale before the repair starts: if a
    // concurrent explicit-id INSERT commits while a degraded engine fails to
    // advance sqlite_sequence, repairing from the earlier snapshot can leave
    // the sequence below a row that is already durable.
    execute_raw("BEGIN IMMEDIATE;")
        .map_err(|e| DbError::Sqlite(format!("id_floor: begin allocator repair: {e}")))?;

    let repair_result = (|| -> DbResult<Option<(i64, i64, i64, i64)>> {
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
        // floor under the writer transaction acquired above.
        let repair_sql = format!(
            "UPDATE sqlite_sequence \
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
              WHERE name = 'messages';"
        );
        execute_raw(&repair_sql).map_err(|error| {
            DbError::Sqlite(format!(
                "id_floor: repair/advance sqlite_sequence: {error}"
            ))
        })?;

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

        Ok(Some((
            persisted_floor,
            db_max_id,
            seq_value,
            seq_row_count,
        )))
    })();

    let repaired = match repair_result {
        Ok(repaired) => repaired,
        Err(error) => {
            let rollback_detail = execute_raw("ROLLBACK;").err().map_or_else(
                String::new,
                |rollback_error| format!("; rollback also failed: {rollback_error}"),
            );
            if rollback_detail.is_empty() {
                return Err(error);
            }
            return Err(DbError::Sqlite(format!("{error}{rollback_detail}")));
        }
    };
    if let Err(error) = execute_raw("COMMIT;") {
        let rollback_detail = execute_raw("ROLLBACK;").err().map_or_else(
            String::new,
            |rollback_error| format!("; rollback also failed: {rollback_error}"),
        );
        return Err(DbError::Sqlite(format!(
            "id_floor: commit allocator repair: {error}{rollback_detail}"
        )));
    }

    let Some((persisted_floor, db_max_id, seq_value, seq_row_count)) = repaired else {
        return Ok(None);
    };
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
/// File-backed allocators are keyed by normalized SQLite identity (see
/// [`DbPool::message_id_allocator`](crate::DbPool::message_id_allocator)), so
/// independent pools for the same live database share one high-water mark.
/// In-memory databases remain scoped to their underlying pool `Arc`.
pub struct MessageIdAllocator {
    /// The largest id this process has handed out for this database.
    /// `0` means "no id allocated yet" (the first allocation seeds it).
    high_water: AtomicI64,
    /// Terminal invalidation published before recovery exposes a replacement
    /// allocator. Surviving wrappers of an independently constructed old pool
    /// retain this allocator, so they must fail closed instead of diverging
    /// from the replacement's high-water authority.
    retired: AtomicBool,
    /// The authoritative archive floor. `OnceCell::get_or_try_init` gives
    /// concurrent async callers a non-spinning wait, and resets to uninitialized
    /// after an error, cancellation, panic outcome, or dropped initializer.
    archive_floor: OnceCell<i64>,
    /// Cancel-aware admission for the first archive scan. `OnceCell` provides
    /// publication/retry semantics; the mutex makes competing callers wait on
    /// their own `Cx` so cancellation does not depend on the elected scanner
    /// finishing first.
    archive_init_lock: Arc<AsyncMutex<()>>,
}

impl std::fmt::Debug for MessageIdAllocator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MessageIdAllocator")
            .field("high_water", &self.current_high_water())
            .field("retired", &self.is_retired())
            .field("archive_floor", &self.archive_floor.get().copied())
            .finish_non_exhaustive()
    }
}

enum ArchiveFloorInitError {
    Db(Box<DbError>),
    Cancelled(CancelReason),
    Panicked(PanicPayload),
}

fn cancellation_reason(cx: &Cx, context: &'static str) -> Option<CancelReason> {
    // `checkpoint` observes budgets and respects an active cancellation mask;
    // reading the raw flag would incorrectly abort masked cleanup work.
    if cx.checkpoint().is_ok() {
        return None;
    }
    Some(
        cx.cancel_reason()
            .unwrap_or_else(|| CancelReason::user(context)),
    )
}

async fn scan_archive_floor(cx: &Cx, storage_root: PathBuf) -> Outcome<i64, DbError> {
    if let Some(reason) = cancellation_reason(cx, "message id archive scan cancelled") {
        return Outcome::Cancelled(reason);
    }

    let scan_result = match cx.spawn_blocking(move |_child_cx| {
        max_message_id_in_archive(&storage_root)
    }) {
        Ok(handle) => match handle.await {
            Ok(result) => result,
            Err(JoinError::Cancelled(reason)) => return Outcome::Cancelled(reason),
            Err(JoinError::Panicked(payload)) => return Outcome::Panicked(payload),
            Err(JoinError::PolledAfterCompletion) => {
                return Outcome::Err(DbError::Internal(
                    "message id archive scan task handle was polled after completion".to_string(),
                ));
            }
        },
        Err(error) => {
            if let Some(reason) = cancellation_reason(cx, "message id archive scan cancelled") {
                return Outcome::Cancelled(reason);
            }
            return Outcome::Err(DbError::Internal(format!(
                "message id archive scan task admission failed: {error}"
            )));
        }
    };

    if let Some(reason) = cancellation_reason(cx, "message id archive scan cancelled") {
        return Outcome::Cancelled(reason);
    }
    match scan_result {
        Ok(max_id) => Outcome::Ok(max_id.unwrap_or(0)),
        Err(error) => Outcome::Err(error),
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
            retired: AtomicBool::new(false),
            archive_floor: OnceCell::new(),
            archive_init_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Whether the archive max still needs to be folded into the high-water
    /// mark. This is exposed for diagnostics; callers should let
    /// [`MessageIdAllocator::allocate`] decide whether to invoke its archive
    /// scanner so scan completion cannot be published prematurely.
    #[must_use]
    pub fn needs_archive_seed(&self) -> bool {
        !self.is_retired() && !self.archive_floor.is_initialized()
    }

    fn retired_error() -> DbError {
        DbError::Internal(
            "message id allocator was retired after database recovery".to_string(),
        )
    }

    /// Permanently revoke this allocator before a replacement database
    /// generation publishes a new allocator for the same file identity.
    ///
    /// The registry intentionally leaves old per-pool handles pointing here:
    /// every surviving wrapper then observes the terminal state rather than
    /// silently joining the replacement generation.
    pub(crate) fn retire(&self) -> bool {
        !self.retired.swap(true, Ordering::AcqRel)
    }

    #[must_use]
    pub(crate) fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    async fn allocate_with<F, Fut>(
        &self,
        cx: &Cx,
        db_floor: i64,
        archive_scan: F,
    ) -> Outcome<i64, DbError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Outcome<i64, DbError>>,
    {
        if self.is_retired() {
            return Outcome::Err(Self::retired_error());
        }
        if let Some(reason) = cancellation_reason(cx, "message id allocation cancelled") {
            return Outcome::Cancelled(reason);
        }
        let archive_floor = if let Some(floor) = self.archive_floor.get() {
            *floor
        } else {
            let _init_guard = match OwnedMutexGuard::lock(
                Arc::clone(&self.archive_init_lock),
                cx,
            )
            .await
            {
                Ok(guard) => guard,
                Err(LockError::Cancelled) => {
                    return Outcome::Cancelled(cx.cancel_reason().unwrap_or_else(|| {
                        CancelReason::user("message id archive initialization cancelled")
                    }));
                }
                Err(LockError::TimedOut(_)) => {
                    return Outcome::Cancelled(
                        cx.cancel_reason().unwrap_or_else(CancelReason::deadline),
                    );
                }
                Err(error) => {
                    return Outcome::Err(DbError::Internal(format!(
                        "message id archive initialization lock failed: {error}"
                    )));
                }
            };
            if self.is_retired() {
                return Outcome::Err(Self::retired_error());
            }
            if let Some(reason) = cancellation_reason(cx, "message id allocation cancelled") {
                return Outcome::Cancelled(reason);
            }
            match self
                .archive_floor
                .get_or_try_init(|| async {
                    match archive_scan().await {
                        Outcome::Ok(floor) => Ok(floor.max(0)),
                        Outcome::Err(error) => Err(ArchiveFloorInitError::Db(Box::new(error))),
                        Outcome::Cancelled(reason) => {
                            Err(ArchiveFloorInitError::Cancelled(reason))
                        }
                        Outcome::Panicked(payload) => {
                            Err(ArchiveFloorInitError::Panicked(payload))
                        }
                    }
                })
                .await
            {
                Ok(floor) => *floor,
                Err(ArchiveFloorInitError::Db(error)) => return Outcome::Err(*error),
                Err(ArchiveFloorInitError::Cancelled(reason)) => {
                    return Outcome::Cancelled(reason);
                }
                Err(ArchiveFloorInitError::Panicked(payload)) => {
                    return Outcome::Panicked(payload);
                }
            }
        };
        // Recovery can retire the allocator while its one-time archive scan is
        // awaited. Never let that stale initialization publish a post-recovery
        // message id.
        if self.is_retired() {
            return Outcome::Err(Self::retired_error());
        }
        if let Some(reason) = cancellation_reason(cx, "message id allocation cancelled") {
            return Outcome::Cancelled(reason);
        }

        let mut current = self.high_water.load(Ordering::Acquire);
        loop {
            if self.is_retired() {
                return Outcome::Err(Self::retired_error());
            }
            let base = current.max(db_floor).max(archive_floor).max(0);
            let Some(next) = base.checked_add(1) else {
                return Outcome::Err(DbError::Internal(
                    "message id allocator exhausted the positive i64 row-id range".to_string(),
                ));
            };
            match self.high_water.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Linearize invalidation against the successful CAS. If
                    // recovery retired this allocator concurrently, burn the
                    // tentative local high-water value but never return it to
                    // a caller that could write through the old pool.
                    if self.is_retired() {
                        return Outcome::Err(Self::retired_error());
                    }
                    return Outcome::Ok(next);
                }
                Err(observed) => {
                    if let Some(reason) =
                        cancellation_reason(cx, "message id allocation cancelled")
                    {
                        return Outcome::Cancelled(reason);
                    }
                    current = observed;
                }
            }
        }
    }

    /// Allocate the next message id.
    ///
    /// `db_floor` is the maximum of the live `messages.id` and every
    /// `sqlite_sequence.seq` row for `messages`. The archive is scanned at
    /// most once successfully; concurrent first allocations wait without
    /// spinning, and a failed/cancelled/panicked scan remains retryable.
    ///
    /// Returns an id strictly greater than `db_floor`, the archive floor, and
    /// every id previously handed out by this allocator. The returned id must
    /// be used for both the DB row and canonical archive filename.
    ///
    /// # Errors
    ///
    /// Returns the archive scan error without publishing a floor, or
    /// [`DbError::Internal`] when the positive SQLite row-id range is
    /// exhausted. Cancellation and panic retain their exact [`Outcome`]
    /// variants.
    pub async fn allocate(
        &self,
        cx: &Cx,
        db_floor: i64,
        storage_root: &Path,
    ) -> Outcome<i64, DbError> {
        let storage_root = storage_root.to_path_buf();
        self.allocate_with(cx, db_floor, || async move {
            scan_archive_floor(cx, storage_root).await
        })
        .await
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

        let max = max_message_id_in_archive(root).expect("scan canonical archive");
        assert_eq!(max, Some(3846));
    }

    #[test]
    fn max_message_id_in_archive_returns_none_for_empty_root() {
        let dir = tempdir().unwrap();
        assert_eq!(max_message_id_in_archive(dir.path()).unwrap(), None);
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
        assert_eq!(max_message_id_in_archive(root).unwrap(), None);
    }

    #[test]
    fn max_message_id_in_archive_rejects_files_without_canonical_frontmatter() {
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

        let error = max_message_id_in_archive(dir.path())
            .expect_err("an incomplete canonical scan must fail closed");
        assert!(
            error.to_string().contains("frontmatter header"),
            "unexpected malformed-frontmatter error: {error}"
        );
    }

    #[test]
    fn max_message_id_in_archive_stops_before_large_non_utf8_body() {
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("projects/proj/messages/2026/05/01__77.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = b"---json\n{\"id\":77}\n---\n".to_vec();
        bytes.extend(std::iter::repeat_n(0xff, 1024 * 1024));
        fs::write(&path, bytes).unwrap();

        assert_eq!(
            max_message_id_in_archive(dir.path()).unwrap(),
            Some(77),
            "the bounded scanner must neither read nor UTF-8 decode the message body"
        );
    }

    #[cfg(unix)]
    #[test]
    fn max_message_id_in_archive_rejects_canonical_message_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let target = dir.path().join("target.md");
        fs::write(&target, "---json\n{\"id\":88}\n---\n").unwrap();
        let link = dir
            .path()
            .join("projects/proj/messages/2026/05/01__88.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();

        let error = max_message_id_in_archive(dir.path())
            .expect_err("a canonical-position symlink makes the scan incomplete");
        assert!(
            error.to_string().contains("symlinks are not authoritative"),
            "unexpected symlink error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn frontmatter_opener_itself_refuses_leaf_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let target = dir.path().join("target.md");
        let link = dir.path().join("swapped-leaf.md");
        fs::write(&target, "---json\n{\"id\":91}\n---\n").unwrap();
        symlink(&target, &link).unwrap();

        let error = extract_message_id_from_frontmatter(&link)
            .expect_err("the leaf opener must not follow a post-classification symlink");
        assert!(
            error.to_string().contains("without following"),
            "unexpected no-follow error: {error}"
        );
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

        assert_eq!(
            result, None,
            "the transaction must observe the newer allocator floor and leave it unchanged"
        );
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
    fn id_floor_transaction_observes_newer_row_with_regressed_sequence() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stale-row-floor.db");
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
                // Deterministically commit a newer explicit-id row at the
                // transaction-admission seam, then model the degraded engine
                // that failed to retain its corresponding sequence advance.
                // Moving BEGIN below the floor reads makes this test repair to
                // 25 and exposes the stale-read bug.
                if sql == "BEGIN IMMEDIATE;" && !injected.replace(true) {
                    newer
                        .execute_raw(
                            "INSERT INTO messages (id, subject) VALUES (100, 'newer');
                             UPDATE sqlite_sequence SET seq = 10 WHERE name = 'messages';",
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
                "SELECT COUNT(*) AS row_count, MAX(seq) AS seq FROM sqlite_sequence \
                 WHERE name = 'messages'",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("row_count").unwrap(), 1);
        assert_eq!(rows[0].get_named::<i64>("seq").unwrap(), 100);
    }

    #[test]
    fn id_floor_admission_failure_preserves_caller_transaction() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("caller-owned-transaction.db");
        let conn = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 subject TEXT NOT NULL
             );
             CREATE TABLE caller_sentinel (value TEXT NOT NULL);
             INSERT INTO messages (id, subject) VALUES (10, 'existing');
             BEGIN IMMEDIATE;
             INSERT INTO caller_sentinel (value) VALUES ('must-survive');",
        )
        .unwrap();

        let error = advance_messages_id_floor(&conn, Some(25))
            .expect_err("repair must not nest inside a caller-owned transaction");
        assert!(
            error.to_string().contains("begin allocator repair"),
            "unexpected admission error: {error}"
        );
        conn.execute_raw("COMMIT;")
            .expect("the caller must retain ownership of its transaction");

        let observer = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        let rows = observer
            .query_sync("SELECT value FROM caller_sentinel", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("value").unwrap(),
            "must-survive"
        );
    }

    #[test]
    fn id_floor_mid_repair_failure_rolls_back_owned_transaction() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("mid-repair-rollback.db");
        let conn = SqliteConnection::open_file(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_raw(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 subject TEXT NOT NULL
             );
             INSERT INTO messages (id, subject) VALUES (10, 'existing');",
        )
        .unwrap();

        let error = advance_messages_id_floor_with(
            Some(25),
            |sql, params| {
                conn.query_sync(sql, params)
                    .map_err(|error| error.to_string())
            },
            |sql| {
                if sql.starts_with("UPDATE sqlite_sequence") {
                    return conn
                        .execute_raw(&format!(
                            "{sql} INSERT INTO missing_repair_failure_table VALUES (1);"
                        ))
                        .map_err(|error| error.to_string());
                }
                conn.execute_raw(sql).map_err(|error| error.to_string())
            },
        )
        .expect_err("injected mid-repair error must propagate");
        assert!(
            error
                .to_string()
                .contains("repair/advance sqlite_sequence"),
            "unexpected repair error: {error}"
        );

        let rows = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count, MAX(seq) AS seq FROM sqlite_sequence \
                 WHERE name = 'messages'",
                &[],
            )
            .unwrap();
        assert_eq!(rows[0].get_named::<i64>("row_count").unwrap(), 1);
        assert_eq!(rows[0].get_named::<i64>("seq").unwrap(), 10);
        conn.execute_raw("BEGIN IMMEDIATE; ROLLBACK;")
            .expect("repair rollback must leave no transaction open");
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build allocator test runtime")
            .block_on(future)
    }

    fn allocate_from_archive(
        allocator: &MessageIdAllocator,
        db_floor: i64,
        storage_root: &Path,
    ) -> Outcome<i64, DbError> {
        block_on(async {
            let cx = Cx::current().expect("runtime installs allocator context");
            allocator.allocate(&cx, db_floor, storage_root).await
        })
    }

    fn expect_allocated(outcome: Outcome<i64, DbError>) -> i64 {
        match outcome {
            Outcome::Ok(id) => id,
            other => panic!("expected successful allocation, got {other:?}"),
        }
    }

    #[test]
    fn allocator_hands_out_strictly_increasing_ids() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        // First allocation seeds from the larger of db_floor / archive_seed.
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 1128, || async {
                Outcome::Ok(1128)
            }))),
            1129
        );
        // db_floor stays at 1128 (the durable allocator failed to advance,
        // exactly the #176 suspect-mode scenario), but the in-memory
        // high-water carries forward so the next id is still fresh. The scan
        // closure must not run after the authoritative floor is published.
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 1128, || async {
                panic!("published archive floor must be reused")
            }))),
            1130
        );
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 1128, || async {
                panic!("published archive floor must be reused")
            }))),
            1131
        );
        assert_eq!(alloc.current_high_water(), 1131);
    }

    #[test]
    fn allocator_reuse_proof_when_durable_floor_regresses() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        let first = expect_allocated(block_on(alloc.allocate_with(&cx, 1128, || async {
            Outcome::Ok(1128)
        })));
        assert_eq!(first, 1129);
        let second = expect_allocated(block_on(alloc.allocate_with(&cx, 1000, || async {
            panic!("archive floor must already be initialized")
        })));
        assert!(
            second > first,
            "allocator re-issued or regressed: first={first} second={second}"
        );
        assert_eq!(second, 1130);
    }

    #[test]
    fn allocator_starts_at_one_for_empty_db_and_archive() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 0, || async {
                Outcome::Ok(0)
            }))),
            1
        );
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 0, || async {
                panic!("archive floor must already be initialized")
            }))),
            2
        );
    }

    #[test]
    fn allocator_archive_seed_gate_flips_once() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        assert!(alloc.needs_archive_seed());
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 0, || async {
                Outcome::Ok(0)
            }))),
            1
        );
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn allocator_scans_archive_only_until_floor_is_published() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        let scans = std::cell::Cell::new(0_u32);
        let first = expect_allocated(block_on(alloc.allocate_with(&cx, 10, || async {
            scans.set(scans.get() + 1);
            Outcome::Ok(50)
        })));
        let second = expect_allocated(block_on(alloc.allocate_with(&cx, 10, || async {
            scans.set(scans.get() + 1);
            Outcome::Ok(100)
        })));

        assert_eq!(first, 51);
        assert_eq!(second, 52);
        assert_eq!(scans.get(), 1);
    }

    #[test]
    fn concurrent_allocator_initializes_once_and_allocates_unique_ids() {
        let alloc = std::sync::Arc::new(MessageIdAllocator::new());
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let scans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (scan_started_tx, scan_started_rx) = std::sync::mpsc::channel();
        let (release_scan_tx, release_scan_rx) = std::sync::mpsc::sync_channel(0);
        let release_scan_rx =
            std::sync::Arc::new(std::sync::Mutex::new(release_scan_rx));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let alloc = std::sync::Arc::clone(&alloc);
            let start = std::sync::Arc::clone(&start);
            let scans = std::sync::Arc::clone(&scans);
            let scan_started_tx = scan_started_tx.clone();
            let release_scan_rx = std::sync::Arc::clone(&release_scan_rx);
            handles.push(std::thread::spawn(move || {
                let cx = Cx::for_testing();
                start.wait();
                block_on(alloc.allocate_with(&cx, 0, || async move {
                    scans.fetch_add(1, Ordering::SeqCst);
                    scan_started_tx.send(()).unwrap();
                    release_scan_rx.lock().unwrap().recv().unwrap();
                    Outcome::Ok(100)
                }))
            }));
        }
        drop(scan_started_tx);
        start.wait();
        scan_started_rx.recv().unwrap();
        release_scan_tx.send(()).unwrap();

        let mut ids = handles
            .into_iter()
            .map(|handle| expect_allocated(handle.join().unwrap()))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, vec![101, 102]);
        assert_eq!(
            scans.load(Ordering::SeqCst),
            1,
            "concurrent first allocations must share one archive scan"
        );
    }

    #[test]
    fn allocator_fails_closed_when_row_id_space_is_exhausted() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        let outcome = block_on(alloc.allocate_with(&cx, i64::MAX, || async {
            Outcome::Ok(0)
        }));
        let Outcome::Err(error) = outcome else {
            panic!("row-id exhaustion must return an error")
        };
        assert!(
            error.to_string().contains("exhausted"),
            "unexpected exhaustion error: {error}"
        );
        assert_eq!(alloc.current_high_water(), 0);
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn allocator_retries_real_archive_scan_after_frontmatter_error() {
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("projects/proj/messages/2026/05/01__40.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not canonical frontmatter").unwrap();
        let alloc = MessageIdAllocator::new();
        let first = allocate_from_archive(&alloc, 0, dir.path());
        let Outcome::Err(error) = first else {
            panic!("malformed archive must fail allocation")
        };
        assert!(error.to_string().contains("frontmatter"));
        assert!(alloc.needs_archive_seed());
        assert_eq!(alloc.current_high_water(), 0);

        fs::write(&path, "---json\n{\"id\":40}\n---\n").unwrap();
        assert_eq!(
            expect_allocated(allocate_from_archive(&alloc, 0, dir.path())),
            41
        );
        assert!(!alloc.needs_archive_seed());
    }

    #[cfg(unix)]
    #[test]
    fn allocator_retries_after_canonical_symlink_scan_error() {
        use std::os::unix::fs::symlink;

        let bad = tempdir().unwrap();
        let target = bad.path().join("target.md");
        fs::write(&target, "---json\n{\"id\":88}\n---\n").unwrap();
        let link = bad
            .path()
            .join("projects/proj/messages/2026/05/01__88.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();

        let good = tempdir().unwrap();
        write_canonical_message(good.path(), "proj", "2026", "05", "01__88.md", 88);
        let alloc = MessageIdAllocator::new();
        let first = allocate_from_archive(&alloc, 0, bad.path());
        assert!(matches!(
            first,
            Outcome::Err(error) if error.to_string().contains("symlinks are not authoritative")
        ));
        assert!(alloc.needs_archive_seed());
        assert_eq!(alloc.current_high_water(), 0);

        assert_eq!(
            expect_allocated(allocate_from_archive(&alloc, 0, good.path())),
            89
        );
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn runtime_unavailable_archive_scan_fails_closed_and_remains_retryable() {
        let dir = tempdir().unwrap();
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();

        let first = block_on(alloc.allocate(&cx, 0, dir.path()));
        assert!(matches!(
            first,
            Outcome::Err(error)
                if error.to_string().contains("archive scan task admission failed")
        ));
        assert!(alloc.needs_archive_seed());
        assert_eq!(alloc.current_high_water(), 0);

        assert_eq!(
            expect_allocated(allocate_from_archive(&alloc, 0, dir.path())),
            1
        );
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn retirement_during_awaited_archive_seed_prevents_id_publication() {
        let alloc = Arc::new(MessageIdAllocator::new());
        let worker_alloc = Arc::clone(&alloc);
        let scan_started = Arc::new(std::sync::Barrier::new(2));
        let scan_release = Arc::new(std::sync::Barrier::new(2));
        let worker_started = Arc::clone(&scan_started);
        let worker_release = Arc::clone(&scan_release);
        let handle = std::thread::spawn(move || {
            let cx = Cx::for_testing();
            block_on(worker_alloc.allocate_with(&cx, 0, || async move {
                worker_started.wait();
                worker_release.wait();
                Outcome::Ok(90)
            }))
        });

        scan_started.wait();
        assert!(alloc.retire(), "first retirement must publish terminal state");
        scan_release.wait();
        assert!(matches!(
            handle.join().expect("join allocator worker"),
            Outcome::Err(error) if error.to_string().contains("retired after database recovery")
        ));
        assert_eq!(alloc.current_high_water(), 0);
        assert!(!alloc.needs_archive_seed());
    }

    #[test]
    fn allocator_preserves_cancelled_outcome_and_retries_seed() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        let expected = CancelReason::user("injected archive cancellation");
        let first = block_on(alloc.allocate_with(&cx, 0, || {
            let expected = expected.clone();
            async move { Outcome::Cancelled(expected) }
        }));
        let Outcome::Cancelled(observed) = first else {
            panic!("cancellation must retain its Outcome variant")
        };
        assert_eq!(observed, expected);
        assert!(alloc.needs_archive_seed());
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 0, || async {
                Outcome::Ok(40)
            }))),
            41
        );
    }

    #[test]
    fn allocator_preserves_panicked_outcome_and_retries_seed() {
        let alloc = MessageIdAllocator::new();
        let cx = Cx::for_testing();
        let expected = PanicPayload::new("injected archive panic");
        let first = block_on(alloc.allocate_with(&cx, 0, || {
            let expected = expected.clone();
            async move { Outcome::Panicked(expected) }
        }));
        let Outcome::Panicked(observed) = first else {
            panic!("panic must retain its Outcome variant")
        };
        assert_eq!(observed, expected);
        assert!(alloc.needs_archive_seed());
        assert_eq!(
            expect_allocated(block_on(alloc.allocate_with(&cx, 0, || async {
                Outcome::Ok(40)
            }))),
            41
        );
    }
}
