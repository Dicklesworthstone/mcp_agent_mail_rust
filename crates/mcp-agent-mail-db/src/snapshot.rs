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

#[cfg(unix)]
#[path = "snapshot_authority.rs"]
mod authority;
#[cfg(unix)]
use authority::SnapshotAuthority;
#[cfg(unix)]
pub use authority::SnapshotRetentionGuard;

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _};
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
    /// Absolute canonical `.bak` publication slot used when these bytes were
    /// captured. Rotation may later move the same hash-bound bytes to a strict
    /// `.bak.<timestamp>` sibling; readers derive that live path from the
    /// recovery-candidate classifier rather than trusting a path from JSON.
    pub snapshot_path: String,
    /// Exact length of the `.bak` bytes described by this record.
    pub snapshot_size_bytes: u64,
    /// Full-file SHA-256 binding the timestamp/count claims to exact `.bak` bytes.
    pub snapshot_sha256: String,
    /// Binary version that produced the snapshot (for path/version-confusion triage).
    pub binary_version: String,
}

/// The exact unversioned record emitted by v0.3.30 (GH#355). It is recognized
/// only so a freshly verified snapshot can supersede it without losing the old
/// evidence. These claims never authorize a restore or identify backup bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySnapshotMetadata {
    created_us: i64,
    integrity_verified: bool,
    integrity_kind: String,
    schema_version: i64,
    row_counts: BTreeMap<String, i64>,
    source_path: String,
    snapshot_path: String,
    binary_version: String,
}

#[derive(Debug, PartialEq, Eq)]
enum SnapshotMetadataAuthority {
    Missing,
    Current(Vec<u8>),
    Legacy(Vec<u8>),
}

fn legacy_metadata_matches_namespace(
    meta: &LegacySnapshotMetadata,
    primary: &Path,
    canonical_bak: &Path,
) -> bool {
    meta.binary_version == "0.3.30"
        && meta.created_us > 0
        && meta.integrity_verified
        && meta.integrity_kind == CheckKind::Full.to_string()
        && meta.schema_version >= 0
        && meta.row_counts.iter().all(|(table, count)| {
            SNAPSHOT_ROW_COUNT_TABLES.contains(&table.as_str()) && *count >= 0
        })
        && primary
            .to_str()
            .is_some_and(|path| meta.source_path == path)
        && canonical_bak
            .to_str()
            .is_some_and(|path| meta.snapshot_path == path)
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

fn normalized_snapshot_primary(primary: &Path) -> DbResult<PathBuf> {
    crate::pool::validate_sqlite_target_path(primary, "verified snapshot primary")
        .map_err(|error| DbError::Sqlite(error.to_string()))?;
    let anchored = if primary.is_absolute() {
        primary.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "snapshot path: cannot resolve current directory for {}: {error}",
                    primary.display()
                ))
            })?
            .join(primary)
    };
    match std::fs::canonicalize(&anchored) {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file_name = anchored.file_name().ok_or_else(|| {
                DbError::Sqlite(format!(
                    "snapshot path: {} has no database file name",
                    primary.display()
                ))
            })?;
            let parent = anchored
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let canonical_parent = std::fs::canonicalize(parent).map_err(|parent_error| {
                DbError::Sqlite(format!(
                    "snapshot path: cannot canonicalize parent {}: {parent_error}",
                    parent.display()
                ))
            })?;
            Ok(canonical_parent.join(file_name))
        }
        Err(error) => Err(DbError::Sqlite(format!(
            "snapshot path: cannot canonicalize {}: {error}",
            anchored.display()
        ))),
    }
}

fn metadata_matches_snapshot_namespace(
    meta: &VerifiedSnapshotMetadata,
    primary: &Path,
    canonical_bak: &Path,
) -> bool {
    meta.schema == SNAPSHOT_METADATA_SCHEMA
        && meta.integrity_verified
        && meta.integrity_kind == CheckKind::Full.to_string()
        && primary
            .to_str()
            .is_some_and(|path| meta.source_path == path)
        && canonical_bak
            .to_str()
            .is_some_and(|path| meta.snapshot_path == path)
        && meta.snapshot_sha256.len() == 64
        && meta
            .snapshot_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
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
    let mut buffer = vec![0_u8; 64 * 1024];
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

fn existing_snapshot_metadata_authority(
    meta_path: &Path,
    primary: &Path,
    canonical_bak: &Path,
) -> DbResult<SnapshotMetadataAuthority> {
    match std::fs::symlink_metadata(meta_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let bytes = mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
                meta_path,
                MAX_SNAPSHOT_METADATA_BYTES,
            )
            .map_err(|error| {
                DbError::Sqlite(format!(
                    "snapshot metadata cannot read existing authority {}: {error}",
                    meta_path.display()
                ))
            })?;
            match serde_json::from_slice::<VerifiedSnapshotMetadata>(&bytes) {
                Ok(existing) => {
                    if !metadata_matches_snapshot_namespace(&existing, primary, canonical_bak) {
                        return Err(DbError::Sqlite(format!(
                            "snapshot metadata refuses to replace foreign or future authority {}",
                            meta_path.display()
                        )));
                    }
                    Ok(SnapshotMetadataAuthority::Current(bytes))
                }
                Err(error) => {
                    // Unknown versions, partially written current records and
                    // operator evidence must not be mistaken for legacy output.
                    if serde_json::from_slice::<LegacySnapshotMetadata>(&bytes).is_ok_and(|meta| {
                        legacy_metadata_matches_namespace(&meta, primary, canonical_bak)
                    }) {
                        Ok(SnapshotMetadataAuthority::Legacy(bytes))
                    } else {
                        Err(DbError::Sqlite(format!(
                            "snapshot metadata refuses to replace unrecognized regular file {}: {error}",
                            meta_path.display()
                        )))
                    }
                }
            }
        }
        Ok(_) => Err(DbError::Sqlite(format!(
            "snapshot metadata destination {} is not a regular non-symlink file",
            meta_path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SnapshotMetadataAuthority::Missing)
        }
        Err(error) => Err(DbError::Sqlite(format!(
            "snapshot metadata cannot inspect destination {}: {error}",
            meta_path.display()
        ))),
    }
}

/// Preserve the old record before publishing new authority. The content-derived
/// name makes crash retries idempotent; an occupied name is accepted only when
/// it already contains these exact bytes in a regular, non-symlink file.
fn preserve_legacy_snapshot_metadata(meta_path: &Path, bytes: &[u8]) -> DbResult<PathBuf> {
    use std::io::Write as _;

    let parent = meta_path.parent().ok_or_else(|| {
        DbError::Sqlite("snapshot metadata has no evidence directory".to_string())
    })?;
    let mut name = meta_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".legacy-{}.json",
        hex::encode(Sha256::digest(bytes))
    ));
    let preserved = parent.join(name);
    let verify_existing = || -> DbResult<()> {
        let existing = mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
            &preserved,
            MAX_SNAPSHOT_METADATA_BYTES,
        )
        .map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot metadata cannot inspect preserved legacy evidence {}: {error}",
                preserved.display()
            ))
        })?;
        if existing != bytes {
            return Err(DbError::Sqlite(format!(
                "snapshot metadata legacy evidence {} is occupied by different bytes",
                preserved.display()
            )));
        }
        Ok(())
    };
    match std::fs::symlink_metadata(&preserved) {
        Ok(_) => verify_existing()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut stage = tempfile::Builder::new()
                .prefix(".snapshot-legacy-")
                .tempfile_in(parent)
                .map_err(|error| {
                    DbError::Sqlite(format!(
                        "snapshot metadata cannot stage legacy evidence: {error}"
                    ))
                })?;
            mcp_agent_mail_core::disk::set_private_writable_file_permissions(stage.as_file())
                .and_then(|()| stage.write_all(bytes))
                .and_then(|()| stage.as_file().sync_all())
                .map_err(|error| {
                    DbError::Sqlite(format!(
                        "snapshot metadata cannot preserve legacy evidence: {error}"
                    ))
                })?;
            match stage.persist_noclobber(&preserved) {
                Ok(file) => file.sync_all().map_err(|error| {
                    DbError::Sqlite(format!(
                        "snapshot metadata cannot sync legacy evidence: {error}"
                    ))
                })?,
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_existing()?;
                }
                Err(error) => {
                    return Err(DbError::Sqlite(format!(
                        "snapshot metadata cannot publish legacy evidence {}: {}",
                        preserved.display(),
                        error.error
                    )));
                }
            }
        }
        Err(error) => {
            return Err(DbError::Sqlite(format!(
                "snapshot metadata cannot inspect legacy evidence {}: {error}",
                preserved.display()
            )));
        }
    }
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            DbError::Sqlite(format!(
                "snapshot metadata cannot sync legacy evidence directory: {error}"
            ))
        })?;
    Ok(preserved)
}

/// Write the metadata sidecar atomically (tmp file + rename).
fn write_snapshot_metadata(primary: &Path, meta: &VerifiedSnapshotMetadata) -> DbResult<()> {
    use std::io::Write as _;

    let primary = normalized_snapshot_primary(primary)?;
    let canonical_bak = snapshot_bak_path(&primary);
    if !metadata_matches_snapshot_namespace(meta, &primary, &canonical_bak) {
        return Err(DbError::Sqlite(
            "snapshot metadata refuses to publish a foreign or unsupported authority record"
                .to_string(),
        ));
    }
    let meta_path = snapshot_meta_path(&primary);
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
    let replacing_authority =
        existing_snapshot_metadata_authority(&meta_path, &primary, &canonical_bak)?;

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
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(staged.as_file()).map_err(
        |error| {
            DbError::Sqlite(format!(
                "snapshot metadata cannot secure stage {}: {error}",
                staged.path().display()
            ))
        },
    )?;
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
    if let SnapshotMetadataAuthority::Legacy(bytes) = &replacing_authority {
        let preserved = preserve_legacy_snapshot_metadata(&meta_path, bytes)?;
        tracing::info!(
            metadata = %meta_path.display(),
            evidence = %preserved.display(),
            "preserved v0.3.30 snapshot metadata before publishing a freshly verified snapshot"
        );
    }
    let still_owned = existing_snapshot_metadata_authority(&meta_path, &primary, &canonical_bak)?;
    if still_owned != replacing_authority {
        return Err(DbError::Sqlite(format!(
            "snapshot metadata destination {} changed during publication",
            meta_path.display()
        )));
    }
    let persisted = if replacing_authority == SnapshotMetadataAuthority::Missing {
        staged.persist_noclobber(&meta_path)
    } else {
        staged.persist(&meta_path)
    }
    .map_err(|error| {
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
    let primary = normalized_snapshot_primary(primary).ok()?;
    let meta_path = snapshot_meta_path(&primary);
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
/// Unix publishers share one persistent per-primary authority with verified
/// readers. Contention refuses before creating an inspection stage.
pub fn record_snapshot_metadata(
    primary: &Path,
    created_us: i64,
) -> DbResult<VerifiedSnapshotMetadata> {
    let primary = normalized_snapshot_primary(primary)?;
    #[cfg(unix)]
    let authority = SnapshotAuthority::acquire(&primary, true).map_err(snapshot_authority_error)?;
    let bak = snapshot_bak_path(&primary);
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
    #[cfg(unix)]
    authority.revalidate().map_err(snapshot_authority_error)?;
    write_snapshot_metadata(&primary, &meta)?;
    Ok(meta)
}

#[cfg(unix)]
fn snapshot_authority_error(error: std::io::Error) -> DbError {
    DbError::Sqlite(format!(
        "snapshot authority unavailable; retry without replacing evidence: {error}"
    ))
}

struct SelectedVerifiedSnapshot {
    metadata: VerifiedSnapshotMetadata,
    actual_path: PathBuf,
    source: std::fs::File,
    // Retain publication admission through staging, never through promotion.
    #[cfg(unix)]
    authority: Option<SnapshotAuthority>,
}

fn select_verified_snapshot(primary: &Path) -> Option<SelectedVerifiedSnapshot> {
    match select_verified_snapshot_checked(primary) {
        Ok(selected) => selected,
        Err(error) => {
            tracing::debug!(%error, "verified snapshot selection is temporarily unavailable");
            None
        }
    }
}

fn select_verified_snapshot_checked(primary: &Path) -> DbResult<Option<SelectedVerifiedSnapshot>> {
    let primary = normalized_snapshot_primary(primary)?;
    // Do not create a lock just because a read probed a fresh mailbox.
    if read_snapshot_metadata(&primary).is_none() {
        return Ok(None);
    }
    #[cfg(unix)]
    let authority =
        SnapshotAuthority::acquire(&primary, false).map_err(snapshot_authority_error)?;
    let selected = select_verified_snapshot_inner(&primary);
    #[cfg(unix)]
    {
        authority.revalidate().map_err(snapshot_authority_error)?;
        Ok(selected.map(|mut selected| {
            selected.authority = Some(authority);
            selected
        }))
    }
    #[cfg(not(unix))]
    {
        Ok(selected)
    }
}

fn select_verified_snapshot_inner(primary: &Path) -> Option<SelectedVerifiedSnapshot> {
    let primary = normalized_snapshot_primary(primary).ok()?;
    let meta = read_snapshot_metadata(&primary)?;
    let canonical_bak = snapshot_bak_path(&primary);
    if !metadata_matches_snapshot_namespace(&meta, &primary, &canonical_bak) {
        return None;
    }

    for candidate in crate::pool::sqlite_backup_candidates(&primary) {
        if !crate::pool::sqlite_recovery_candidate_is_standalone(&candidate) {
            continue;
        }
        let mut source = match mcp_agent_mail_core::disk::open_regular_file_no_follow(&candidate) {
            Ok(source) => source,
            Err(_) => continue,
        };
        if !source
            .metadata()
            .is_ok_and(|metadata| metadata.len() == meta.snapshot_size_bytes)
        {
            continue;
        }
        let identity = match snapshot_file_identity_from_reader(&mut source, &candidate) {
            Ok(identity) => identity,
            Err(_) => continue,
        };
        if identity.0 != meta.snapshot_size_bytes || identity.1 != meta.snapshot_sha256 {
            continue;
        }
        if !matches!(
            crate::pool::sqlite_recovery_candidate_passes_full_integrity_check(&candidate),
            Ok(true)
        ) || !crate::pool::sqlite_recovery_candidate_is_standalone(&candidate)
        {
            continue;
        }
        let current = match mcp_agent_mail_core::disk::open_regular_file_no_follow(&candidate) {
            Ok(current) => current,
            Err(_) => continue,
        };
        let retained_identity = match source.try_clone().and_then(same_file::Handle::from_file) {
            Ok(identity) => identity,
            Err(_) => continue,
        };
        let current_identity = match same_file::Handle::from_file(current) {
            Ok(identity) => identity,
            Err(_) => continue,
        };
        if retained_identity != current_identity || source.rewind().is_err() {
            continue;
        }
        return Some(SelectedVerifiedSnapshot {
            metadata: meta,
            actual_path: candidate,
            source,
            #[cfg(unix)]
            authority: None,
        });
    }
    None
}

/// Return the latest verified snapshot's metadata for a primary path. The
/// metadata remains authoritative when its exact bytes have been durably
/// rotated from `.bak` to a strict `.bak.<timestamp>` generation.
#[must_use]
pub fn latest_verified_snapshot(primary: &Path) -> Option<VerifiedSnapshotMetadata> {
    select_verified_snapshot(primary).map(|selected| selected.metadata)
}

/// Resolve the strict backup path currently carrying the verified bytes.
/// Consumers such as retention use this to pin the one authoritative rotated
/// generation without ever trusting a path read from JSON.
#[must_use]
pub fn verified_snapshot_source_path(primary: &Path) -> Option<PathBuf> {
    select_verified_snapshot(primary).map(|selected| selected.actual_path)
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
    let primary = normalized_snapshot_primary(primary)?;
    let Some(mut selected) = select_verified_snapshot_checked(&primary)? else {
        return Ok(None);
    };
    let meta = selected.metadata;
    let actual_snapshot_path = selected.actual_path;

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
    std::io::copy(&mut selected.source, &mut destination).map_err(|e| {
        DbError::Sqlite(format!(
            "snapshot restore: copy {} -> {}: {e}",
            actual_snapshot_path.display(),
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
    drop(selected.source);
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

    // The staged bytes are independently hash-bound and fully verified. Never
    // retain snapshot admission while promotion drains database borrowers.
    #[cfg(unix)]
    if let Some(authority) = selected.authority {
        authority.revalidate().map_err(snapshot_authority_error)?;
        drop(authority);
    }

    crate::pool::promote_recovery_candidate(&primary, &staged, storage_root).map_err(|error| {
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
        source = %actual_snapshot_path.display(),
        recorded_origin = %meta.snapshot_path,
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

    #[cfg(unix)]
    #[test]
    fn snapshot_publication_respects_a_live_authority_owner_before_staging() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().canonicalize().unwrap().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let before = record_snapshot_metadata(&primary, 41).unwrap();
        let held = authority::SnapshotAuthority::acquire(&primary, true).unwrap();
        let before_names = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let result = record_snapshot_metadata(&primary, 42);
        assert!(
            result.is_err(),
            "another publisher must not publish while authority is held"
        );
        assert_eq!(read_snapshot_metadata(&primary), Some(before));
        let after_names = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            before_names, after_names,
            "refusal precedes inspection staging"
        );
        drop(held);
        assert_eq!(
            record_snapshot_metadata(&primary, 43).unwrap().created_us,
            43
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_selection_holds_shared_authority_and_restore_refuses_busy_publication() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().canonicalize().unwrap().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta = record_snapshot_metadata(&primary, 41).unwrap();
        let first = select_verified_snapshot(&primary).unwrap();
        let mut second = select_verified_snapshot(&primary).unwrap();
        let before = std::fs::read(snapshot_meta_path(&primary)).unwrap();
        assert!(record_snapshot_metadata(&primary, 42).is_err());
        assert_eq!(std::fs::read(snapshot_meta_path(&primary)).unwrap(), before);
        drop(first);
        assert!(record_snapshot_metadata(&primary, 42).is_err());
        let copied =
            snapshot_file_identity_from_reader(&mut second.source, &second.actual_path).unwrap();
        assert_eq!(copied, (meta.snapshot_size_bytes, meta.snapshot_sha256));
        drop(second);
        let newer = record_snapshot_metadata(&primary, 43).unwrap();
        let held = SnapshotAuthority::acquire(&primary, true).unwrap();
        assert!(
            select_verified_snapshot(&primary).is_none(),
            "a selector cannot ignore the active publisher"
        );
        let primary_before = std::fs::read(&primary).unwrap();
        let names = || {
            std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let before_names = names();
        let error = restore_from_verified_snapshot(&primary, dir.path()).unwrap_err();
        assert!(error.to_string().contains("snapshot authority unavailable"));
        assert_eq!(
            names(),
            before_names,
            "a busy restore must not stage or promote"
        );
        assert_eq!(std::fs::read(&primary).unwrap(), primary_before);
        assert_eq!(read_snapshot_metadata(&primary), Some(newer));
        drop(held);
        assert!(latest_verified_snapshot(&primary).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_admission_is_released_after_verification_error_and_empty_reads_are_neutral() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().canonicalize().unwrap().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        let names = || {
            std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let before = names();
        assert!(latest_verified_snapshot(&primary).is_none());
        assert!(verified_snapshot_source_path(&primary).is_none());
        assert!(
            restore_from_verified_snapshot(&primary, dir.path())
                .unwrap()
                .is_none()
        );
        assert_eq!(names(), before);
        let bak = snapshot_bak_path(&primary);
        std::fs::write(&bak, b"invalid retained backup").unwrap();
        assert!(record_snapshot_metadata(&primary, 41).is_err());
        let held = SnapshotAuthority::acquire(&primary, true).unwrap();
        drop(held);
        std::fs::rename(&bak, dir.path().join("retained-invalid-backup")).unwrap();
        std::fs::copy(&primary, &bak).unwrap();
        assert!(record_snapshot_metadata(&primary, 42).is_ok());
        assert_eq!(
            std::fs::read(dir.path().join("retained-invalid-backup")).unwrap(),
            b"invalid retained backup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_publication_excludes_another_process_and_recovers_after_owner_death() {
        use std::io::Read as _;
        const CHILD: &str = "AM_TEST_SNAPSHOT_AUTHORITY_OWNER";
        if let Some(primary) = std::env::var_os(CHILD) {
            let primary = PathBuf::from(primary);
            let _authority = SnapshotAuthority::acquire(&primary, true).unwrap();
            std::fs::write(primary.with_extension("owner-ready"), b"ready").unwrap();
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().canonicalize().unwrap().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta = record_snapshot_metadata(&primary, 41).unwrap();
        let lock_path = primary.with_file_name("storage.sqlite3.bak.authority.lock");
        use std::os::unix::fs::MetadataExt as _;
        let inode = std::fs::metadata(&lock_path).unwrap().ino();
        let name = std::thread::current().name().unwrap().to_string();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, &primary)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ready = primary.with_extension("owner-ready");
        while !ready.exists() && std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if !ready.exists() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "snapshot lock owner failed to start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // Observe all outcomes before terminating the owner so a regression
        // cannot strand the child behind an assertion's unwinding.
        let refused_record = record_snapshot_metadata(&primary, 42).is_err();
        let refused_select = select_verified_snapshot(&primary).is_none();
        let during = read_snapshot_metadata(&primary);
        let still_running = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(refused_record && refused_select && still_running);
        assert_eq!(during, Some(meta));
        assert_eq!(
            record_snapshot_metadata(&primary, 43).unwrap().created_us,
            43
        );
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().ino(),
            inode,
            "owner death releases admission without stale-lock deletion"
        );
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
        std::fs::write(
            snapshot_meta_path(&primary),
            serde_json::to_vec_pretty(&meta).unwrap(),
        )
        .unwrap();

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
        let meta_path = snapshot_meta_path(&primary);
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
        let before = std::fs::read(&meta_path).unwrap();

        assert!(latest_verified_snapshot(&primary).is_none());
        let error = record_snapshot_metadata(&primary, 43)
            .expect_err("future metadata authority must never be overwritten");
        assert!(error.to_string().contains("future authority"));
        assert_eq!(std::fs::read(&meta_path).unwrap(), before);
    }

    #[test]
    fn malformed_snapshot_hash_is_not_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let mut meta = record_snapshot_metadata(&primary, 42).unwrap();
        meta.snapshot_sha256 = "g".repeat(64);
        std::fs::write(
            snapshot_meta_path(&primary),
            serde_json::to_vec_pretty(&meta).unwrap(),
        )
        .unwrap();

        assert!(
            latest_verified_snapshot(&primary).is_none(),
            "non-hex metadata must be rejected before it can authorize backup bytes"
        );
    }

    #[test]
    fn regular_metadata_sentinel_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta_path = snapshot_meta_path(&primary);
        let sentinel = b"operator evidence, not snapshot metadata";
        std::fs::write(&meta_path, sentinel).unwrap();

        let error = record_snapshot_metadata(&primary, 42)
            .expect_err("unrecognized regular metadata must fail closed");

        assert!(error.to_string().contains("unrecognized regular file"));
        assert_eq!(std::fs::read(&meta_path).unwrap(), sentinel);
    }

    fn legacy_metadata_bytes(primary: &Path) -> Vec<u8> {
        let primary = normalized_snapshot_primary(primary).unwrap();
        serde_json::to_vec_pretty(&serde_json::json!({
            "created_us": 1_789_546_475_731_049_i64,
            "integrity_verified": true,
            "integrity_kind": "integrity_check",
            "schema_version": 1,
            "row_counts": {"agents": 21, "messages": 5257},
            "source_path": primary.to_str().unwrap(),
            "snapshot_path": snapshot_bak_path(&primary).to_str().unwrap(),
            "binary_version": "0.3.30"
        }))
        .unwrap()
    }

    fn preserved_legacy_paths(parent: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".meta.json.legacy-")
            })
            .collect()
    }

    #[test]
    fn legacy_metadata_is_preserved_before_fresh_verification_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        let primary_before = std::fs::read(&primary).unwrap();
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta_path = snapshot_meta_path(&primary);
        let legacy = legacy_metadata_bytes(&primary);
        std::fs::write(&meta_path, &legacy).unwrap();

        assert!(read_snapshot_metadata(&primary).is_none());
        assert!(latest_verified_snapshot(&primary).is_none());
        assert!(
            restore_from_verified_snapshot(&primary, dir.path())
                .unwrap()
                .is_none(),
            "legacy claims must never grant restore authority"
        );

        let fresh = record_snapshot_metadata(&primary, 42).expect("upgrade old metadata");
        assert_eq!(fresh.schema, SNAPSHOT_METADATA_SCHEMA);
        assert_eq!(fresh.created_us, 42);
        assert_eq!(fresh.row_counts.get("messages"), Some(&3));
        assert_eq!(fresh.row_counts.get("agents"), Some(&1));
        assert_eq!(fresh.snapshot_size_bytes, primary_before.len() as u64);
        assert_eq!(
            fresh.snapshot_sha256,
            hex::encode(Sha256::digest(&primary_before))
        );
        assert_eq!(latest_verified_snapshot(&primary), Some(fresh));
        assert_eq!(std::fs::read(&primary).unwrap(), primary_before);

        let preserved = preserved_legacy_paths(dir.path());
        assert_eq!(preserved.len(), 1);
        assert_eq!(std::fs::read(&preserved[0]).unwrap(), legacy);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&preserved[0])
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        record_snapshot_metadata(&primary, 43).expect("normal refresh after upgrade");
        assert_eq!(preserved_legacy_paths(dir.path()), preserved);
        assert_eq!(std::fs::read(&preserved[0]).unwrap(), legacy);
        assert_eq!(latest_verified_snapshot(&primary).unwrap().created_us, 43);
    }

    #[test]
    fn legacy_metadata_is_not_replaced_when_backup_verification_fails() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        let meta_path = snapshot_meta_path(&primary);
        let legacy = legacy_metadata_bytes(&primary);
        std::fs::write(&meta_path, &legacy).unwrap();
        std::fs::write(snapshot_bak_path(&primary), b"not a SQLite database").unwrap();

        record_snapshot_metadata(&primary, 42).expect_err("invalid backup must fail");

        assert_eq!(std::fs::read(&meta_path).unwrap(), legacy);
        assert!(preserved_legacy_paths(dir.path()).is_empty());
        assert!(latest_verified_snapshot(&primary).is_none());
    }

    #[test]
    fn legacy_metadata_recognition_rejects_foreign_future_and_partial_records() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta_path = snapshot_meta_path(&primary);
        let original: serde_json::Value =
            serde_json::from_slice(&legacy_metadata_bytes(&primary)).unwrap();
        let invalid_fields = [
            ("source_path", serde_json::json!("/foreign/storage.sqlite3")),
            (
                "snapshot_path",
                serde_json::json!("/foreign/storage.sqlite3.bak"),
            ),
            ("binary_version", serde_json::json!("999.0.0")),
            ("integrity_verified", serde_json::json!(false)),
            ("integrity_kind", serde_json::json!("quick_check")),
            ("schema_version", serde_json::json!(-1)),
            ("row_counts", serde_json::json!({"messages": -1})),
            ("created_us", serde_json::json!(0)),
            ("schema", serde_json::json!(SNAPSHOT_METADATA_SCHEMA)),
            ("snapshot_sha256", serde_json::json!("a".repeat(64))),
            ("operator_note", serde_json::json!("keep this evidence")),
        ];

        for (field, value) in invalid_fields {
            let mut invalid = original.clone();
            invalid[field] = value;
            let bytes = serde_json::to_vec_pretty(&invalid).unwrap();
            std::fs::write(&meta_path, &bytes).unwrap();

            let error = record_snapshot_metadata(&primary, 42).expect_err(field);

            assert!(
                error.to_string().contains("unrecognized regular file"),
                "{field}: {error}"
            );
            assert_eq!(std::fs::read(&meta_path).unwrap(), bytes, "{field}");
            assert!(preserved_legacy_paths(dir.path()).is_empty(), "{field}");
        }
    }

    #[test]
    fn legacy_metadata_preservation_is_restart_safe_and_refuses_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta_path = snapshot_meta_path(&primary);
        let legacy = legacy_metadata_bytes(&primary);
        std::fs::write(&meta_path, &legacy).unwrap();

        // Stand in for a crash after durable preservation but before publish.
        let preserved = preserve_legacy_snapshot_metadata(&meta_path, &legacy).unwrap();
        record_snapshot_metadata(&primary, 42).expect("retry after evidence preservation");
        assert_eq!(preserved_legacy_paths(dir.path()), vec![preserved.clone()]);
        assert_eq!(std::fs::read(&preserved).unwrap(), legacy);

        std::fs::write(&meta_path, &legacy).unwrap();
        let sentinel = b"operator evidence occupying the preserved name";
        std::fs::write(&preserved, sentinel).unwrap();
        let error = record_snapshot_metadata(&primary, 43)
            .expect_err("occupied evidence must not be overwritten");

        assert!(error.to_string().contains("occupied by different bytes"));
        assert_eq!(std::fs::read(&preserved).unwrap(), sentinel);
        assert_eq!(std::fs::read(&meta_path).unwrap(), legacy);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_metadata_evidence_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("storage.sqlite3");
        make_restorable_mailbox_db(&primary);
        std::fs::copy(&primary, snapshot_bak_path(&primary)).unwrap();
        let meta_path = snapshot_meta_path(&primary);
        let legacy = legacy_metadata_bytes(&primary);
        std::fs::write(&meta_path, &legacy).unwrap();
        let preserved = preserve_legacy_snapshot_metadata(&meta_path, &legacy).unwrap();
        let moved = dir.path().join("preserved-original-legacy.json");
        std::fs::rename(&preserved, &moved).unwrap();
        symlink(&moved, &preserved).unwrap();

        record_snapshot_metadata(&primary, 42).expect_err("symlink witness must fail closed");

        assert_eq!(std::fs::read(&meta_path).unwrap(), legacy);
        assert_eq!(std::fs::read(&moved).unwrap(), legacy);
        assert!(
            std::fs::symlink_metadata(&preserved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
