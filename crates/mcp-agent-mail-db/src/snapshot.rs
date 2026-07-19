//! Last-known-healthy verified snapshots (bead br-bvq1x.11.2 / K2).
//!
//! The integrity guard already produces a proactive `.bak` copy of the live
//! database after a WAL checkpoint. K2 layers a *verified* snapshot on top of
//! that: a snapshot is only recorded as "known-healthy" once a **full**
//! `PRAGMA integrity_check` passes, and a JSON metadata sidecar records when it
//! was taken, that it was integrity-verified, the schema version, and per-table
//! row counts. Recovery can then restore from that fast, lossless snapshot
//! before falling back to the slower archive-derived rebuild — and report which
//! source it used (K1 loss-honesty).
//!
//! This module deliberately reuses the existing backup/restore primitives in
//! [`crate::pool`] (`create_proactive_backup`, `sqlite_file_is_healthy`,
//! `wal_checkpoint_truncate_path`) rather than forking a parallel recovery
//! path, per the K2 revision note.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{DbError, DbResult};
use crate::integrity::{CheckKind, MailboxIntegrityStatus, inspect_mailbox_integrity};

/// Tables whose row counts are recorded in snapshot metadata. These are the
/// core coordination tables; a missing table is skipped (best-effort) so the
/// snapshot still records what it can on partial schemas.
const SNAPSHOT_ROW_COUNT_TABLES: &[&str] = &[
    "projects",
    "agents",
    "messages",
    "message_recipients",
    "file_reservations",
];

/// Metadata describing a verified-clean database snapshot.
///
/// Persisted as a JSON sidecar next to the `.bak` file so recovery can decide
/// whether the snapshot is trustworthy (integrity-verified) and recent enough
/// to prefer over an archive rebuild, and so operators can see exactly what was
/// captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedSnapshotMetadata {
    /// When the snapshot was taken (microseconds since the Unix epoch).
    pub created_us: i64,
    /// Whether a full integrity check passed before recording. Always `true`
    /// for a snapshot this module writes — recorded explicitly so a reader
    /// never has to infer "known-healthy" from the file's mere existence.
    pub integrity_verified: bool,
    /// Which integrity check was run (`"integrity_check"` for the full scan).
    pub integrity_kind: String,
    /// `PRAGMA user_version` of the snapshot at capture time.
    pub schema_version: i64,
    /// Per-table row counts at capture time (best-effort; missing tables omitted).
    pub row_counts: BTreeMap<String, i64>,
    /// Absolute path of the live database the snapshot was taken from.
    pub source_path: String,
    /// Absolute path of the `.bak` snapshot file this metadata describes.
    pub snapshot_path: String,
    /// Binary version that produced the snapshot (for path/version-confusion triage).
    pub binary_version: String,
}

/// Resolve the `.bak` snapshot path for a primary database path.
#[must_use]
pub fn snapshot_bak_path(primary: &Path) -> PathBuf {
    crate::pool::sqlite_path_with_file_name_suffix(primary, ".bak", "storage.sqlite3.bak")
}

/// Resolve the metadata sidecar path for a primary database path.
#[must_use]
pub fn snapshot_meta_path(primary: &Path) -> PathBuf {
    let bak = snapshot_bak_path(primary);
    let mut name = bak.file_name().map_or_else(
        || std::ffi::OsString::from("storage.sqlite3.bak"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(".meta.json");
    bak.with_file_name(name)
}

/// Count rows in the core coordination tables on an already-open connection.
///
/// Best-effort: a table that does not exist (or whose count errors) is omitted
/// rather than failing the whole snapshot.
#[must_use]
pub fn count_key_table_rows(conn: &crate::DbConn) -> BTreeMap<String, i64> {
    count_key_table_rows_with(|sql| conn.query_sync(sql, &[]).map_err(|error| error.to_string()))
}

fn count_key_table_rows_canonical(conn: &crate::CanonicalDbConn) -> BTreeMap<String, i64> {
    count_key_table_rows_with(|sql| conn.query_sync(sql, &[]).map_err(|error| error.to_string()))
}

fn count_key_table_rows_with<F>(mut query: F) -> BTreeMap<String, i64>
where
    F: FnMut(&str) -> Result<Vec<sqlmodel_core::Row>, String>,
{
    let mut counts = BTreeMap::new();
    for table in SNAPSHOT_ROW_COUNT_TABLES {
        // Table names are a fixed allowlist (never user input), so this format
        // cannot inject SQL.
        let sql = format!("SELECT COUNT(*) AS n FROM {table}");
        if let Ok(rows) = query(&sql)
            && let Some(row) = rows.first()
            && let Ok(n) = row.get_named::<i64>("n")
        {
            counts.insert((*table).to_string(), n);
        }
    }
    counts
}

/// Read the `PRAGMA user_version` for an open connection (0 on any error).
fn read_schema_version_canonical(conn: &crate::CanonicalDbConn) -> i64 {
    conn.query_sync("PRAGMA user_version", &[])
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<i64>("user_version").ok())
        .unwrap_or(0)
}

/// Write the metadata sidecar atomically (tmp file + rename).
fn write_snapshot_metadata(primary: &Path, meta: &VerifiedSnapshotMetadata) -> DbResult<()> {
    let meta_path = snapshot_meta_path(primary);
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| DbError::Sqlite(format!("snapshot metadata serialize: {e}")))?;
    let tmp = {
        let mut name = meta_path.file_name().map_or_else(
            || std::ffi::OsString::from("snapshot.meta.json"),
            std::ffi::OsStr::to_os_string,
        );
        name.push(".tmp");
        meta_path.with_file_name(name)
    };
    std::fs::write(&tmp, &json)
        .map_err(|e| DbError::Sqlite(format!("snapshot metadata write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &meta_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        DbError::Sqlite(format!(
            "snapshot metadata publish {}: {e}",
            meta_path.display()
        ))
    })?;
    Ok(())
}

/// Read the metadata sidecar for a primary database path, if present and parseable.
#[must_use]
pub fn read_snapshot_metadata(primary: &Path) -> Option<VerifiedSnapshotMetadata> {
    let meta_path = snapshot_meta_path(primary);
    let bytes = std::fs::read(&meta_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Record a verified snapshot's metadata after the `.bak` has been produced.
///
/// Opens the freshly-written `.bak` to capture row counts + schema version,
/// then writes the JSON sidecar. Returns the recorded metadata.
pub fn record_snapshot_metadata(
    primary: &Path,
    created_us: i64,
) -> DbResult<VerifiedSnapshotMetadata> {
    let bak = snapshot_bak_path(primary);
    let (row_counts, schema_version) =
        if let Ok(conn) = crate::CanonicalDbConn::open_file(bak.display().to_string()) {
            (
                count_key_table_rows_canonical(&conn),
                read_schema_version_canonical(&conn),
            )
        } else {
            (BTreeMap::new(), 0)
        };
    let meta = VerifiedSnapshotMetadata {
        created_us,
        integrity_verified: true,
        integrity_kind: CheckKind::Full.to_string(),
        schema_version,
        row_counts,
        source_path: primary.display().to_string(),
        snapshot_path: bak.display().to_string(),
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_snapshot_metadata(primary, &meta)?;
    Ok(meta)
}

/// Return the latest verified snapshot's metadata for a primary path, but only
/// if it is recorded as integrity-verified AND the `.bak` file actually exists.
#[must_use]
pub fn latest_verified_snapshot(primary: &Path) -> Option<VerifiedSnapshotMetadata> {
    let meta = read_snapshot_metadata(primary)?;
    if !meta.integrity_verified {
        return None;
    }
    if !snapshot_bak_path(primary).is_file() {
        return None;
    }
    Some(meta)
}

/// Restore the primary database from the latest verified snapshot, if one
/// exists and still passes a full integrity check.
///
/// This is the fast, lossless recovery path: it copies the verified `.bak` into
/// a staging file beside the primary, re-verifies it, removes any stale
/// publishes it through the unified receipt-backed recovery boundary. Returns
/// the metadata of the snapshot used, or `Ok(None)` when there is no trustworthy
/// snapshot to restore from (caller should fall back to the archive-derived
/// rebuild).
pub fn restore_from_verified_snapshot(
    primary: &Path,
    storage_root: &Path,
) -> DbResult<Option<VerifiedSnapshotMetadata>> {
    let Some(meta) = latest_verified_snapshot(primary) else {
        return Ok(None);
    };
    let bak = snapshot_bak_path(primary);

    // Re-verify the snapshot itself before trusting it — the sidecar could be
    // stale relative to a bak that was corrupted on disk after recording.
    let verdict = inspect_mailbox_integrity(&bak, CheckKind::Full);
    if verdict.status != MailboxIntegrityStatus::Healthy {
        tracing::warn!(
            snapshot = %bak.display(),
            status = ?verdict.status,
            detail = %verdict.detail,
            "verified snapshot failed re-verification; not restoring from it"
        );
        return Ok(None);
    }

    // Stage the snapshot beside the primary, then validate the staged copy.
    let staged = (0_u32..10_000)
        .find_map(|suffix| {
            let mut name = primary.file_name().map_or_else(
                || std::ffi::OsString::from("storage.sqlite3"),
                std::ffi::OsStr::to_os_string,
            );
            if suffix == 0 {
                name.push(".snapshot-restore.tmp");
            } else {
                name.push(format!(".snapshot-restore-{suffix:04}.tmp"));
            }
            let candidate = primary.with_file_name(name);
            let family_is_free = std::fs::symlink_metadata(&candidate).is_err()
                && ["-journal", "-wal", "-shm"].into_iter().all(|suffix| {
                    std::fs::symlink_metadata(crate::pool::sqlite_path_with_suffix(
                        &candidate, suffix,
                    ))
                    .is_err()
                });
            family_is_free.then_some(candidate)
        })
        .ok_or_else(|| {
            DbError::Sqlite(format!(
                "snapshot restore: exhausted candidate names beside {}",
                primary.display()
            ))
        })?;
    std::fs::copy(&bak, &staged).map_err(|e| {
        DbError::Sqlite(format!(
            "snapshot restore: copy {} -> {}: {e}",
            bak.display(),
            staged.display()
        ))
    })?;
    if !matches!(
        crate::pool::sqlite_recovery_candidate_is_healthy(&staged),
        Ok(true)
    ) {
        return Err(DbError::Sqlite(format!(
            "snapshot restore: staged copy {} failed health check and was preserved for inspection",
            staged.display()
        )));
    }

    crate::pool::promote_recovery_candidate(primary, &staged, storage_root).map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot restore: promote {} -> {}: {error}",
            staged.display(),
            primary.display()
        ))
    })?;

    mcp_agent_mail_core::global_metrics()
        .db
        .snapshot_restored_total
        .inc();
    tracing::info!(
        source = %meta.snapshot_path,
        created_us = meta.created_us,
        "recovered database from last-known-healthy verified snapshot"
    );
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db(path: &Path) {
        let conn = crate::DbConn::open_file(path.display().to_string()).expect("open db");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT);
             CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT);",
        )
        .expect("schema");
        conn.execute_raw("INSERT INTO messages (body) VALUES ('a'), ('b'), ('c');")
            .expect("seed messages");
        conn.execute_raw("INSERT INTO agents (name) VALUES ('BlueLake');")
            .expect("seed agents");
        // Flush the WAL into the main file so the plain file-copies these tests
        // use to stand in for a checkpointed proactive backup capture all rows
        // (the production path checkpoints via `create_proactive_backup`).
        conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[])
            .expect("checkpoint");
    }

    #[test]
    fn snapshot_paths_are_siblings_of_primary() {
        let primary = Path::new("/tmp/mailbox/storage.sqlite3");
        assert_eq!(
            snapshot_bak_path(primary),
            Path::new("/tmp/mailbox/storage.sqlite3.bak")
        );
        assert_eq!(
            snapshot_meta_path(primary),
            Path::new("/tmp/mailbox/storage.sqlite3.bak.meta.json")
        );
    }

    #[test]
    fn count_key_table_rows_skips_missing_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("count.sqlite3");
        make_db(&path);
        let conn = crate::DbConn::open_file(path.display().to_string()).unwrap();
        let counts = count_key_table_rows(&conn);
        assert_eq!(counts.get("messages"), Some(&3));
        assert_eq!(counts.get("agents"), Some(&1));
        // `projects` table was never created -> omitted, not zero/error.
        assert!(!counts.contains_key("projects"));
    }

    #[test]
    fn record_and_read_metadata_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_db(&primary);
        // Stand in for the .bak by copying the primary.
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();

        let meta = record_snapshot_metadata(&primary, 1_700_000_000_000_000).expect("record");
        assert!(meta.integrity_verified);
        assert_eq!(meta.integrity_kind, "integrity_check");
        assert_eq!(meta.row_counts.get("messages"), Some(&3));
        assert_eq!(meta.created_us, 1_700_000_000_000_000);

        let read = read_snapshot_metadata(&primary).expect("read back");
        assert_eq!(read, meta);
        assert_eq!(
            latest_verified_snapshot(&primary).as_ref(),
            Some(&meta),
            "a recorded + present snapshot is the latest verified one"
        );
    }

    #[test]
    fn latest_verified_snapshot_none_when_unverified_or_missing_bak() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_db(&primary);

        // No metadata at all.
        assert!(latest_verified_snapshot(&primary).is_none());

        // Metadata present but bak file missing -> not a valid snapshot.
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta = record_snapshot_metadata(&primary, 1).unwrap();
        std::fs::remove_file(snapshot_bak_path(&primary)).unwrap();
        assert!(
            latest_verified_snapshot(&primary).is_none(),
            "missing .bak means no restorable snapshot even with metadata: {meta:?}"
        );
    }

    #[test]
    fn restore_from_verified_snapshot_recovers_corrupt_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_db(&primary);

        // Capture a verified snapshot (copy primary -> .bak, record metadata).
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta = record_snapshot_metadata(&primary, 42).unwrap();
        assert_eq!(meta.row_counts.get("messages"), Some(&3));

        // Corrupt the primary: overwrite with garbage so it is no longer a DB.
        std::fs::write(&primary, b"this is not a sqlite database at all").unwrap();
        // Deliberately leave any SQLite sidecars in place. The unified
        // promotion boundary must quarantine the complete old generation;
        // the snapshot caller must not need to pre-clean live artifacts.

        // Restore from the verified snapshot.
        let used = restore_from_verified_snapshot(&primary, dir.path())
            .expect("restore should succeed")
            .expect("a verified snapshot should have been used");
        assert_eq!(used.created_us, 42);

        // The restored primary is healthy and has the snapshot's rows.
        let conn = crate::DbConn::open_file(primary.display().to_string()).unwrap();
        let rows = conn
            .query_sync("SELECT COUNT(*) AS n FROM messages", &[])
            .expect("query restored db");
        assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 3);
    }

    #[test]
    fn restore_returns_none_without_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_db(&primary);
        // No snapshot recorded -> nothing to restore, caller falls back to archive.
        assert!(
            restore_from_verified_snapshot(&primary, dir.path())
                .expect("ok")
                .is_none(),
            "no verified snapshot => Ok(None), not an error"
        );
    }
}
