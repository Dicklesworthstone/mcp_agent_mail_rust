//! Last-known-healthy verified snapshots (bead br-bvq1x.11.2 / K2).
//!
//! The integrity guard already produces a transactionally consistent proactive
//! `.bak` through SQLite's online-backup API. K2 layers a *verified* snapshot on
//! top of that: a snapshot is only recorded as "known-healthy" once a **full**
//! `PRAGMA integrity_check` passes, and a JSON metadata sidecar records when it
//! was taken, that it was integrity-verified, the schema version, and per-table
//! row counts. Recovery can then restore from that fast, lossless snapshot
//! before falling back to the slower archive-derived rebuild — and report which
//! source it used (K1 loss-honesty).
//!
//! This module deliberately reuses the existing backup/restore primitives in
//! [`crate::pool`] (`create_proactive_backup`, `sqlite_file_is_healthy`) rather
//! than forking a parallel recovery path, per the K2 revision note.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{DbError, DbResult};
use crate::integrity::CheckKind;

const SNAPSHOT_METADATA_SCHEMA: u32 = 1;
const MAX_SNAPSHOT_METADATA_BYTES: u64 = 64 * 1024;

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
    /// Metadata contract version. Unknown versions are never authoritative.
    pub schema: u32,
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
    /// Exact length of the `.bak` bytes described by this record.
    pub snapshot_size_bytes: u64,
    /// Full-file SHA-256 binding the timestamp/count claims to exact `.bak` bytes.
    pub snapshot_sha256: String,
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

fn snapshot_file_identity(path: &Path) -> DbResult<(u64, String)> {
    let mut file =
        mcp_agent_mail_core::disk::open_regular_file_no_follow(path).map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot identity: cannot open {}: {error}",
                path.display()
            ))
        })?;
    snapshot_file_identity_from_reader(&mut file, path)
}

fn snapshot_file_identity_from_reader(
    file: &mut std::fs::File,
    path: &Path,
) -> DbResult<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot identity: cannot read {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        hasher.update(&buffer[..read]);
    }
    Ok((size, hex::encode(hasher.finalize())))
}

/// Write the metadata sidecar atomically (tmp file + rename).
fn write_snapshot_metadata(primary: &Path, meta: &VerifiedSnapshotMetadata) -> DbResult<()> {
    use std::io::Write as _;

    let meta_path = snapshot_meta_path(primary);
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| DbError::Sqlite(format!("snapshot metadata serialize: {e}")))?;
    if u64::try_from(json.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_METADATA_BYTES {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata exceeds the {MAX_SNAPSHOT_METADATA_BYTES}-byte contract limit"
        )));
    }
    let parent = meta_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !std::fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata parent {} is not a real directory",
            parent.display()
        )));
    }
    match std::fs::symlink_metadata(&meta_path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(DbError::Sqlite(format!(
                "snapshot metadata destination {} is not a regular non-symlink file",
                meta_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DbError::Sqlite(format!(
                "snapshot metadata cannot inspect destination {}: {error}",
                meta_path.display()
            )));
        }
    }

    let mut staged = tempfile::Builder::new()
        .prefix(".snapshot-meta-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot metadata cannot create a unique stage in {}: {error}",
                parent.display()
            ))
        })?;
    staged.write_all(&json).map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata cannot write stage {}: {error}",
            staged.path().display()
        ))
    })?;
    staged.as_file().sync_all().map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata cannot sync stage {}: {error}",
            staged.path().display()
        ))
    })?;

    // Revalidate the destination immediately before the atomic replace so an
    // occupied symlink/non-file can never redirect or absorb the write.
    match std::fs::symlink_metadata(&meta_path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(DbError::Sqlite(format!(
                "snapshot metadata destination {} changed to a non-regular file",
                meta_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DbError::Sqlite(error.to_string())),
    }
    let persisted = staged.persist(&meta_path).map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata atomic publish {}: {}",
            meta_path.display(),
            error.error
        ))
    })?;
    persisted.sync_all().map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata cannot sync published file {}: {error}",
            meta_path.display()
        ))
    })?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot metadata cannot sync parent {}: {error}",
                parent.display()
            ))
        })?;
    Ok(())
}

/// Read the metadata sidecar for a primary database path, if present and parseable.
#[must_use]
pub fn read_snapshot_metadata(primary: &Path) -> Option<VerifiedSnapshotMetadata> {
    let meta_path = snapshot_meta_path(primary);
    let bytes = mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
        &meta_path,
        MAX_SNAPSHOT_METADATA_BYTES,
    )
    .ok()?;
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
    if !crate::pool::sqlite_recovery_candidate_is_standalone(&bak) {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata: {} has companion SQLite or FrankenSQLite state",
            bak.display()
        )));
    }
    let parent = bak
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut source =
        mcp_agent_mail_core::disk::open_regular_file_no_follow(&bak).map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot metadata: cannot open {}: {error}",
                bak.display()
            ))
        })?;
    let mut staged = tempfile::Builder::new()
        .prefix(".snapshot-inspect-")
        .suffix(".sqlite3")
        .tempfile_in(parent)
        .map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot metadata: cannot create private inspection stage in {}: {error}",
                parent.display()
            ))
        })?;
    std::io::copy(&mut source, staged.as_file_mut()).map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata: cannot stage {} for immutable inspection: {error}",
            bak.display()
        ))
    })?;
    staged.as_file().sync_all().map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata: cannot sync private inspection stage for {}: {error}",
            bak.display()
        ))
    })?;
    drop(source);

    let staged_path = staged.path();
    let identity_before = snapshot_file_identity(staged_path)?;
    if !crate::pool::sqlite_recovery_candidate_passes_full_integrity_check(staged_path).map_err(
        |error| {
            DbError::Sqlite(format!(
                "snapshot metadata: cannot verify immutable stage for {}: {error}",
                bak.display()
            ))
        },
    )? {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata: {} is not a fully valid Agent Mail database",
            bak.display()
        )));
    }
    let source_path = primary.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "snapshot metadata: path {} is not valid UTF-8",
            primary.display()
        ))
    })?;
    let bak_path = bak.to_str().ok_or_else(|| {
        DbError::Sqlite(format!(
            "snapshot metadata: path {} is not valid UTF-8",
            bak.display()
        ))
    })?;
    let conn = crate::pool::open_immutable_canonical_sqlite(staged_path).map_err(|error| {
        DbError::Sqlite(format!(
            "snapshot metadata: cannot inspect immutable stage for {}: {error}",
            bak.display()
        ))
    })?;
    let row_counts = count_key_table_rows_canonical(&conn);
    let schema_version = read_schema_version_canonical(&conn);
    drop(conn);
    if !crate::pool::sqlite_recovery_candidate_is_standalone(staged_path) {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata: private inspection stage for {} gained companion state",
            bak.display()
        )));
    }
    let identity_after = snapshot_file_identity(staged_path)?;
    if identity_after != identity_before {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata: immutable inspection stage for {} changed while it was being verified",
            bak.display()
        )));
    }
    if !crate::pool::sqlite_recovery_candidate_is_standalone(&bak)
        || snapshot_file_identity(&bak)? != identity_after
    {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata: {} changed while its immutable generation was being verified",
            bak.display()
        )));
    }
    let (snapshot_size_bytes, snapshot_sha256) = identity_after;
    let meta = VerifiedSnapshotMetadata {
        schema: SNAPSHOT_METADATA_SCHEMA,
        created_us,
        integrity_verified: true,
        integrity_kind: CheckKind::Full.to_string(),
        schema_version,
        row_counts,
        source_path: source_path.to_string(),
        snapshot_path: bak_path.to_string(),
        snapshot_size_bytes,
        snapshot_sha256,
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
    let bak = snapshot_bak_path(primary);
    if meta.schema != SNAPSHOT_METADATA_SCHEMA
        || !meta.integrity_verified
        || meta.integrity_kind != CheckKind::Full.to_string()
        || meta.source_path != primary.to_str()?
        || meta.snapshot_path != bak.to_str()?
    {
        return None;
    }
    if !std::fs::symlink_metadata(&bak).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return None;
    }
    if !crate::pool::sqlite_recovery_candidate_is_standalone(&bak) {
        return None;
    }
    let (size, sha256) = snapshot_file_identity(&bak).ok()?;
    if size != meta.snapshot_size_bytes || sha256 != meta.snapshot_sha256 {
        return None;
    }
    Some(meta)
}

/// Restore the primary database from the latest verified snapshot, if one
/// exists and still passes a full integrity check.
///
/// This is the fast, lossless recovery path: it copies the verified `.bak` into
/// a staging file beside the primary, re-verifies it, then publishes it through
/// the unified receipt-backed recovery boundary. Returns
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

    // Re-verify the exact, standalone snapshot main before trusting it. The
    // metadata could be stale relative to a changed main file, while a WAL or
    // namespace companion would describe state that a one-file copy drops.
    if !crate::pool::sqlite_recovery_candidate_is_standalone(&bak) {
        tracing::warn!(
            snapshot = %bak.display(),
            "verified snapshot has companion SQLite or FrankenSQLite state; not restoring one file from that family"
        );
        return Ok(None);
    }
    match crate::pool::sqlite_recovery_candidate_passes_full_integrity_check(&bak) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                snapshot = %bak.display(),
                "verified snapshot failed strict canonical re-verification; not restoring from it"
            );
            return Ok(None);
        }
        Err(error) => {
            tracing::warn!(
                snapshot = %bak.display(),
                error = %error,
                "verified snapshot could not be re-verified; not restoring from it"
            );
            return Ok(None);
        }
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
            let main_is_free = matches!(
                std::fs::symlink_metadata(&candidate),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            );
            let family_is_free =
                main_is_free && crate::pool::sqlite_recovery_candidate_is_standalone(&candidate);
            family_is_free.then_some(candidate)
        })
        .ok_or_else(|| {
            DbError::Sqlite(format!(
                "snapshot restore: exhausted candidate names beside {}",
                primary.display()
            ))
        })?;
    let mut source = mcp_agent_mail_core::disk::open_regular_file_no_follow(&bak).map_err(|e| {
        DbError::Sqlite(format!(
            "snapshot restore: open source {}: {e}",
            bak.display()
        ))
    })?;
    let mut destination = mcp_agent_mail_core::disk::create_new_private_file_no_follow(&staged)
        .map_err(|e| {
            DbError::Sqlite(format!(
                "snapshot restore: create stage {}: {e}",
                staged.display()
            ))
        })?;
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(&destination).map_err(
        |error| {
            DbError::Sqlite(format!(
                "snapshot restore: protect stage {}: {error}",
                staged.display()
            ))
        },
    )?;
    std::io::copy(&mut source, &mut destination).map_err(|e| {
        DbError::Sqlite(format!(
            "snapshot restore: copy {} -> {}: {e}",
            bak.display(),
            staged.display()
        ))
    })?;
    destination.sync_all().map_err(|e| {
        DbError::Sqlite(format!(
            "snapshot restore: sync staged copy {}: {e}",
            staged.display()
        ))
    })?;
    drop(destination);
    drop(source);
    if !crate::pool::sqlite_recovery_candidate_is_standalone(&bak) {
        tracing::warn!(
            snapshot = %bak.display(),
            staged = %staged.display(),
            "verified snapshot gained companion state during staging; preserving the stage and declining restore"
        );
        return Ok(None);
    }
    if !crate::pool::sqlite_recovery_candidate_is_standalone(&staged) {
        return Err(DbError::Sqlite(format!(
            "snapshot restore: staged copy {} has companion state and was preserved for inspection",
            staged.display()
        )));
    }
    let (staged_size, staged_sha256) = snapshot_file_identity(&staged)?;
    if staged_size != meta.snapshot_size_bytes || staged_sha256 != meta.snapshot_sha256 {
        return Err(DbError::Sqlite(format!(
            "snapshot restore: staged copy {} does not match recorded snapshot identity and was preserved for inspection",
            staged.display()
        )));
    }
    if !matches!(
        crate::pool::sqlite_recovery_candidate_passes_full_integrity_check(&staged),
        Ok(true)
    ) {
        return Err(DbError::Sqlite(format!(
            "snapshot restore: staged copy {} failed full integrity check and was preserved for inspection",
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
        // use to stand in for the production online backup capture all rows.
        conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE);", &[])
            .expect("checkpoint");
    }

    fn make_restorable_mailbox_db(path: &Path) {
        let conn = crate::CanonicalDbConn::open_file(path.display().to_string()).expect("open db");
        conn.execute_raw("PRAGMA journal_mode = WAL;").expect("wal");
        conn.execute_raw(&crate::schema::init_schema_sql_base())
            .expect("mailbox schema");
        conn.execute_raw(
            "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (1, 'snapshot-project', '/snapshot-project', 1);
             INSERT INTO agents
                 (id, project_id, name, program, model, task_description,
                  inception_ts, last_active_ts, attachments_policy, contact_policy,
                  reaper_exempt, registration_token)
                 VALUES
                 (1, 1, 'BlueLake', 'codex-cli', 'test', '', 1, 1,
                  'auto', 'auto', 0, NULL);
             INSERT INTO messages
                 (id, project_id, sender_id, subject, body_md, importance,
                  ack_required, created_ts, recipients_json, attachments)
                 VALUES
                 (1, 1, 1, 'a', 'a', 'normal', 0, 1, '{}', '[]'),
                 (2, 1, 1, 'b', 'b', 'normal', 0, 2, '{}', '[]'),
                 (3, 1, 1, 'c', 'c', 'normal', 0, 3, '{}', '[]');",
        )
        .expect("seed mailbox");
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
        make_restorable_mailbox_db(&primary);
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
    fn recording_metadata_refreshes_the_existing_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();

        let first = record_snapshot_metadata(&primary, 1).expect("first record");
        let second = record_snapshot_metadata(&primary, 2).expect("refresh record");

        assert_eq!(first.snapshot_sha256, second.snapshot_sha256);
        assert_eq!(second.created_us, 2);
        assert_eq!(read_snapshot_metadata(&primary), Some(second));
    }

    #[test]
    fn recording_metadata_requires_a_real_backup() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);

        let error = record_snapshot_metadata(&primary, 1).expect_err("missing backup must fail");
        assert!(error.to_string().contains("cannot open"));
        assert!(
            std::fs::symlink_metadata(snapshot_meta_path(&primary)).is_err(),
            "a failed record must not mint authoritative metadata"
        );
    }

    #[test]
    fn changed_snapshot_bytes_invalidate_metadata_without_touching_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        let primary_before = std::fs::read(&primary).unwrap();
        let bak = snapshot_bak_path(&primary);
        std::fs::copy(&primary, &bak).unwrap();
        record_snapshot_metadata(&primary, 42).unwrap();

        let original_len = std::fs::metadata(&bak).unwrap().len();
        let mut changed = std::fs::read(&bak).unwrap();
        let tail = changed.last_mut().expect("snapshot is not empty");
        *tail ^= 0x01;
        std::fs::write(&bak, &changed).unwrap();
        assert_eq!(
            std::fs::metadata(&bak).unwrap().len(),
            original_len,
            "the regression must isolate SHA binding rather than size mismatch"
        );

        assert!(latest_verified_snapshot(&primary).is_none());
        assert!(
            restore_from_verified_snapshot(&primary, dir.path())
                .expect("tamper rejection is not an operational error")
                .is_none()
        );
        assert_eq!(
            std::fs::read(&primary).unwrap(),
            primary_before,
            "rejected snapshot bytes must not disturb the live primary"
        );
    }

    #[test]
    fn metadata_from_a_different_path_is_not_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let mut meta = record_snapshot_metadata(&primary, 42).unwrap();
        meta.snapshot_path = dir
            .path()
            .join("some-other.sqlite3.bak")
            .to_string_lossy()
            .into_owned();
        write_snapshot_metadata(&primary, &meta).unwrap();

        assert!(
            latest_verified_snapshot(&primary).is_none(),
            "metadata replayed from another path must not authorize this backup"
        );
    }

    #[test]
    fn unknown_metadata_schema_is_not_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let mut meta = record_snapshot_metadata(&primary, 42).unwrap();
        meta.schema = SNAPSHOT_METADATA_SCHEMA + 1;
        write_snapshot_metadata(&primary, &meta).unwrap();

        assert!(latest_verified_snapshot(&primary).is_none());
    }

    #[test]
    fn oversized_metadata_is_rejected_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        std::fs::write(
            snapshot_meta_path(&primary),
            vec![b' '; usize::try_from(MAX_SNAPSHOT_METADATA_BYTES + 1).unwrap()],
        )
        .unwrap();

        assert!(read_snapshot_metadata(&primary).is_none());
    }

    #[test]
    fn latest_verified_snapshot_none_when_unverified_or_missing_bak() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);

        // No metadata at all.
        assert!(latest_verified_snapshot(&primary).is_none());

        // Metadata present but bak file missing -> not a valid snapshot.
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta = record_snapshot_metadata(&primary, 1).unwrap();
        let relocated = dir.path().join("preserved-former-snapshot.sqlite3");
        std::fs::rename(snapshot_bak_path(&primary), &relocated).unwrap();
        assert!(
            latest_verified_snapshot(&primary).is_none(),
            "missing .bak means no restorable snapshot even with metadata: {meta:?}"
        );
        assert!(
            relocated.exists(),
            "the former snapshot remains inspectable"
        );
    }

    #[test]
    fn restore_from_verified_snapshot_recovers_corrupt_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);

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

    #[cfg(unix)]
    #[test]
    fn symlinked_snapshot_is_neither_latest_nor_restorable() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        let bak = snapshot_bak_path(&primary);
        std::fs::copy(&primary, &bak).unwrap();
        let meta = record_snapshot_metadata(&primary, 42).unwrap();
        let relocated = dir.path().join("relocated-snapshot.sqlite3");
        std::fs::rename(&bak, &relocated).unwrap();
        symlink(&relocated, &bak).unwrap();

        assert!(
            latest_verified_snapshot(&primary).is_none(),
            "a symlink must not be labeled as the latest verified snapshot: {meta:?}"
        );
        assert!(
            restore_from_verified_snapshot(&primary, dir.path())
                .expect("symlink rejection is not an operational error")
                .is_none(),
            "a symlinked snapshot must never authorize restore"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_metadata_destination_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let sentinel = dir.path().join("metadata-sentinel.json");
        std::fs::write(&sentinel, b"operator evidence").unwrap();
        symlink(&sentinel, snapshot_meta_path(&primary)).unwrap();

        let error = record_snapshot_metadata(&primary, 42)
            .expect_err("metadata publish must reject a symlink destination");
        assert!(error.to_string().contains("non-symlink"));
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"operator evidence");
    }

    #[test]
    fn snapshot_with_committed_wal_state_is_not_restored_as_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        let bak = snapshot_bak_path(&primary);
        std::fs::copy(&primary, &bak).unwrap();
        record_snapshot_metadata(&primary, 42).unwrap();

        let snapshot_writer = crate::CanonicalDbConn::open_file(
            bak.to_str().expect("temporary path should be valid UTF-8"),
        )
        .unwrap();
        snapshot_writer
            .execute_raw("PRAGMA journal_mode = WAL")
            .unwrap();
        snapshot_writer
            .execute_raw(
                "INSERT INTO messages
                    (id, project_id, sender_id, subject, body_md, importance,
                     ack_required, created_ts, recipients_json, attachments)
                 VALUES
                    (99, 1, 1, 'wal-only', 'wal-only', 'normal', 0, 99, '{}', '[]')",
            )
            .unwrap();
        let wal = crate::pool::sqlite_path_with_suffix(&bak, "-wal");
        assert!(
            std::fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 32),
            "fixture must keep committed state in an adjacent WAL"
        );
        assert!(
            latest_verified_snapshot(&primary).is_none(),
            "metadata must stop labeling a multi-file snapshot generation as verified and restorable"
        );

        std::fs::write(&primary, b"corrupt live generation").unwrap();
        assert!(
            restore_from_verified_snapshot(&primary, dir.path())
                .expect("family rejection is not an operational error")
                .is_none(),
            "a snapshot family must not be truncated to its main file"
        );
        assert_eq!(
            std::fs::read(&primary).unwrap(),
            b"corrupt live generation",
            "rejected snapshot family must not disturb the live primary"
        );
        drop(snapshot_writer);
    }
}
