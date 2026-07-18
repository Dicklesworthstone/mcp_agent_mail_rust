//! Shared mailbox forensic bundle capture for recovery entrypoints.
//!
//! The doctor CLI originally owned forensic bundle creation. This module lifts
//! the bundle contract into the DB layer so startup/runtime recovery paths can
//! preserve the same evidence before any repair or reconstruct logic mutates
//! the live mailbox state.

use crate::{
    pool::{
        archive_metadata_advantage_is_decisive, inspect_mailbox_db_inventory,
        inspect_mailbox_recovery_lock, inspect_mailbox_sidecar_state, sqlite_path_with_suffix,
    },
    reconstruct::{
        ArchiveMessageInventory, archive_missing_project_identities, compute_archive_drift_report,
        scan_archive_message_inventory,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;
use sqlmodel_core::{Error as SqlError, Value};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Write as _,
    path::{Path, PathBuf},
};

/// Request to capture a mailbox forensic bundle.
#[derive(Debug, Clone, Copy)]
pub struct MailboxForensicCapture<'a> {
    pub command_name: &'a str,
    pub trigger: &'a str,
    pub database_url: &'a str,
    pub db_path: &'a Path,
    pub storage_root: &'a Path,
    pub integrity_detail: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForensicProcessHolder {
    pub pid: u32,
    pub roles: Vec<String>,
    pub cmdline: Option<String>,
    pub exe_path: Option<String>,
    pub exe_deleted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForensicFileLock {
    pub role: String,
    pub pid: u32,
    pub lock_type: String,
    pub access: String,
    pub range_start: String,
    pub range_end: String,
}

// ============================================================================
// Hash-linked recovery receipts
// ============================================================================

/// A prepared recovery receipt which has been durably written but does not yet
/// claim that its candidate was promoted.
///
/// Promotion is committed by atomically renaming `pending_path` to
/// `final_path` *after* the candidate has replaced the live database. This
/// filename transition is deliberately the only promotion marker: a crash in
/// the gap leaves a `.pending` file that the next startup treats as a hard
/// readiness failure.
#[derive(Debug)]
pub(crate) struct PreparedRecoveryReceipt {
    pending_path: PathBuf,
    final_path: PathBuf,
    receipt_bytes_sha256: String,
}

/// Finalization error annotated with whether the durable `.json` promotion
/// marker has already replaced `.pending`.
///
/// Callers may restore the old database only when this is `false`. Once the
/// marker rename succeeds, restoring the old database would make a valid
/// finalized receipt attest the wrong live generation.
#[derive(Debug)]
pub(crate) struct RecoveryReceiptFinalizeError {
    error: SqlError,
    promotion_marker_committed: bool,
}

impl RecoveryReceiptFinalizeError {
    #[must_use]
    pub(crate) const fn promotion_marker_committed(&self) -> bool {
        self.promotion_marker_committed
    }
}

impl std::fmt::Display for RecoveryReceiptFinalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecoveryReceiptCategory {
    count: usize,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecoveryContinuitySnapshot {
    projects: RecoveryReceiptCategory,
    products: RecoveryReceiptCategory,
    product_project_links: RecoveryReceiptCategory,
    agents: RecoveryReceiptCategory,
    contacts: RecoveryReceiptCategory,
    reservations: RecoveryReceiptCategory,
    messages: RecoveryReceiptCategory,
    message_recipients: RecoveryReceiptCategory,
    proof_gate_consumed_nonces: RecoveryReceiptCategory,
    aggregate_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecoveryReceiptDeltaCategory {
    added_count: usize,
    lost_count: usize,
    added_sha256: String,
    lost_sha256: String,
    /// Canonical JSON stable keys present only in the candidate.
    added: Vec<String>,
    /// Canonical JSON stable keys present only in the recovery source.
    lost: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecoveryContinuityDelta {
    projects: RecoveryReceiptDeltaCategory,
    products: RecoveryReceiptDeltaCategory,
    product_project_links: RecoveryReceiptDeltaCategory,
    agents: RecoveryReceiptDeltaCategory,
    contacts: RecoveryReceiptDeltaCategory,
    reservations: RecoveryReceiptDeltaCategory,
    messages: RecoveryReceiptDeltaCategory,
    message_recipients: RecoveryReceiptDeltaCategory,
    proof_gate_consumed_nonces: RecoveryReceiptDeltaCategory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecoveryReceiptBody {
    schema: String,
    receipt_id: String,
    /// The body is only a promotion intent while its file ends in `.pending`.
    /// Renaming the exact bytes to `.json` after activation commits the claim.
    promotion_commit_marker: String,
    prepared_at_us: i64,
    db_path: String,
    storage_root: String,
    source_path: Option<String>,
    /// Present only when the source was confirmed corrupt and could not be
    /// semantically inventoried. The digest records the exact probe failure
    /// without leaking paths or row contents into the receipt. In this case
    /// `source` is the empty snapshot and `delta` is candidate-relative; the
    /// receipt attests the promoted generation but cannot claim losslessness
    /// against unreadable bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_snapshot_failure_sha256: Option<String>,
    candidate_path: String,
    source: RecoveryContinuitySnapshot,
    candidate: RecoveryContinuitySnapshot,
    delta: RecoveryContinuityDelta,
    previous_receipt_bytes_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecoveryReceiptDocument {
    body: RecoveryReceiptBody,
    /// SHA-256 of the canonical compact JSON serialization of `body`.
    self_sha256: String,
}

#[derive(Debug)]
struct VerifiedRecoveryReceiptChain {
    tip_bytes_sha256: String,
    latest: RecoveryReceiptDocument,
}

#[derive(Debug, Default)]
struct RecoveryContinuitySets {
    projects: BTreeSet<String>,
    products: BTreeSet<String>,
    product_project_links: BTreeSet<String>,
    agents: BTreeSet<String>,
    contacts: BTreeSet<String>,
    reservations: BTreeSet<String>,
    /// SHA-256 semantic fingerprints mapped to their multiplicity. Message ids
    /// are deliberately excluded so collision-safe id remaps do not look like
    /// continuity loss, while the multiset still detects drops and duplicates.
    messages: BTreeMap<String, usize>,
    /// Recipient state fingerprints use their parent message's semantic digest
    /// rather than its numeric id for the same remap-invariance property.
    message_recipients: BTreeMap<String, usize>,
    proof_gate_consumed_nonces: BTreeSet<String>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    major: u32,
    minor: u32,
}

#[cfg(target_os = "linux")]
fn linux_device_numbers(dev: u64) -> (u32, u32) {
    let major = u32::try_from((dev >> 8) & 0xfff).unwrap_or(u32::MAX);
    let minor = u32::try_from((dev & 0xff) | ((dev >> 12) & 0xfff00)).unwrap_or(u32::MAX);
    (major, minor)
}

// ============================================================================
// Pre-recovery snapshot
// ============================================================================

/// Lightweight snapshot of live DB state captured immediately before recovery.
///
/// This is cheaper than a full forensic bundle — it reads only file metadata
/// and `/proc` state, never opens the SQLite file or walks the archive.
/// Recovery callers should capture this *before* any mutation, close, or
/// rename so that the evidence reflects the state that triggered recovery.
#[derive(Debug, Clone, Serialize)]
pub struct ForensicPreSnapshot {
    /// Trigger that caused the snapshot (e.g. "startup-integrity", "runtime-corruption").
    pub trigger: String,
    /// Canonical path to the primary DB file.
    pub db_path: String,
    /// Database family name (e.g. "storage.sqlite3"), derived from the DB path.
    pub db_family: String,
    /// Primary DB file size in bytes, or `None` if missing.
    pub db_bytes: Option<u64>,
    /// Rollback-journal sidecar size in bytes, or `None` if missing.
    pub journal_bytes: Option<u64>,
    /// WAL file size in bytes, or `None` if missing.
    pub wal_bytes: Option<u64>,
    /// SHM file size in bytes, or `None` if missing.
    pub shm_bytes: Option<u64>,
    /// SQLite page size read from the DB header (bytes 16..18), or `None` on error.
    pub page_size: Option<u32>,
    /// Total page count read from the DB header (bytes 28..32), or `None` on error.
    pub page_count: Option<u32>,
    /// Processes with open file descriptors on the DB/journal/WAL/SHM files.
    pub process_holders: Vec<ForensicProcessHolder>,
    /// File-level locks held on the DB/journal/WAL/SHM files.
    pub file_locks: Vec<ForensicFileLock>,
    /// Whether a `.recovery.lock` file exists and is held by a live process.
    pub recovery_lock_active: bool,
    /// PID recorded in the recovery lock file, if any.
    pub recovery_lock_pid: Option<u32>,
    /// PID of the current process (for cross-reference with holders).
    pub self_pid: u32,
    /// Microsecond timestamp when the snapshot was taken.
    pub captured_at_us: i64,
    /// Storage root path, if provided via [`with_environment`](Self::with_environment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_root: Option<String>,
    /// Redacted `DATABASE_URL`, if provided via [`with_environment`](Self::with_environment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_url_redacted: Option<String>,
}

impl ForensicPreSnapshot {
    /// Attach environment/config context to the snapshot.
    ///
    /// Call this after [`capture_pre_recovery_snapshot`] when you have access
    /// to the storage root and database URL.  The URL is automatically redacted
    /// to strip credentials.
    #[must_use]
    pub fn with_environment(mut self, storage_root: &Path, database_url: &str) -> Self {
        self.storage_root = Some(storage_root.display().to_string());
        self.database_url_redacted = Some(redact_database_url(database_url));
        self
    }
}

/// Read SQLite page size and page count from the database file header.
///
/// The header format is fixed: bytes 16..18 hold the page size as a big-endian
/// u16 (with 1 meaning 65536), and bytes 28..32 hold the page count as a
/// big-endian u32.  Returns `(page_size, page_count)` or `None` on any error.
fn read_sqlite_header_fields(db_path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;

    let mut file = std::fs::File::open(db_path).ok()?;
    let mut header = [0u8; 32];
    file.read_exact(&mut header).ok()?;

    // Bytes 0..16 are the magic string "SQLite format 3\000".
    if &header[..16] != b"SQLite format 3\0" {
        return None;
    }

    let raw_page_size = u16::from_be_bytes([header[16], header[17]]);
    let page_size: u32 = match raw_page_size {
        1 => 65_536,
        512 | 1024 | 2048 | 4096 | 8192 | 16_384 | 32_768 => u32::from(raw_page_size),
        _ => return None,
    };
    let page_count = u32::from_be_bytes([header[28], header[29], header[30], header[31]]);
    Some((page_size, page_count))
}

/// Capture a lightweight pre-recovery snapshot of the live DB state.
///
/// This reads file metadata and `/proc` state without opening the SQLite
/// connection, so it is safe to call even when the DB is corrupt or locked.
#[must_use]
pub fn capture_pre_recovery_snapshot(db_path: &Path, trigger: &str) -> ForensicPreSnapshot {
    if is_in_memory_db_path(db_path) {
        let recovery_lock = inspect_mailbox_recovery_lock(db_path);
        let snapshot = ForensicPreSnapshot {
            trigger: trigger.to_string(),
            db_path: db_path.display().to_string(),
            db_family: forensic_db_family_name(db_path),
            db_bytes: None,
            journal_bytes: None,
            wal_bytes: None,
            shm_bytes: None,
            page_size: None,
            page_count: None,
            process_holders: Vec::new(),
            file_locks: Vec::new(),
            recovery_lock_active: recovery_lock.active,
            recovery_lock_pid: recovery_lock.pid,
            self_pid: std::process::id(),
            captured_at_us: mcp_agent_mail_core::timestamps::now_micros(),
            storage_root: None,
            database_url_redacted: None,
        };

        tracing::info!(
            db_path = %snapshot.db_path,
            db_family = %snapshot.db_family,
            trigger = %snapshot.trigger,
            recovery_lock_active = snapshot.recovery_lock_active,
            recovery_lock_pid = ?snapshot.recovery_lock_pid,
            "captured pre-recovery forensic snapshot for in-memory database"
        );

        return snapshot;
    }

    let db_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());
    let journal_path = sqlite_path_with_suffix(db_path, "-journal");
    let wal_path = sqlite_path_with_suffix(db_path, "-wal");
    let shm_path = sqlite_path_with_suffix(db_path, "-shm");
    let journal_bytes = std::fs::metadata(&journal_path).ok().map(|m| m.len());
    let wal_bytes = std::fs::metadata(&wal_path).ok().map(|m| m.len());
    let shm_bytes = std::fs::metadata(&shm_path).ok().map(|m| m.len());
    let (page_size, page_count) =
        read_sqlite_header_fields(db_path).map_or((None, None), |(ps, pc)| (Some(ps), Some(pc)));

    let family_paths: Vec<(&str, PathBuf)> = vec![
        ("db", db_path.to_path_buf()),
        ("journal", journal_path),
        ("wal", wal_path),
        ("shm", shm_path),
    ];
    let process_holders = process_holders_for_paths(&family_paths);
    let file_locks = file_locks_for_paths(&family_paths);

    // Recovery lock state — derived from the well-known sidecar path.
    let recovery_lock = inspect_mailbox_recovery_lock(db_path);
    let recovery_lock_active = recovery_lock.active;
    let recovery_lock_pid = recovery_lock.pid;

    let snapshot = ForensicPreSnapshot {
        trigger: trigger.to_string(),
        db_path: db_path.display().to_string(),
        db_family: forensic_db_family_name(db_path),
        db_bytes,
        journal_bytes,
        wal_bytes,
        shm_bytes,
        page_size,
        page_count,
        process_holders,
        file_locks,
        recovery_lock_active,
        recovery_lock_pid,
        self_pid: std::process::id(),
        captured_at_us: mcp_agent_mail_core::timestamps::now_micros(),
        storage_root: None,
        database_url_redacted: None,
    };

    tracing::info!(
        db_path = %snapshot.db_path,
        db_family = %snapshot.db_family,
        trigger = %snapshot.trigger,
        db_bytes = ?snapshot.db_bytes,
        journal_bytes = ?snapshot.journal_bytes,
        wal_bytes = ?snapshot.wal_bytes,
        page_size = ?snapshot.page_size,
        page_count = ?snapshot.page_count,
        holders = snapshot.process_holders.len(),
        locks = snapshot.file_locks.len(),
        recovery_lock_active = snapshot.recovery_lock_active,
        recovery_lock_pid = ?snapshot.recovery_lock_pid,
        "captured pre-recovery forensic snapshot"
    );

    snapshot
}

fn redact_database_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |offset| authority_start + offset);
    let authority = &url[authority_start..authority_end];
    let Some(at_pos) = authority.rfind('@') else {
        return url.to_string();
    };
    format!(
        "{}****{}",
        &url[..authority_start],
        &url[authority_start + at_pos..]
    )
}

fn forensics_root(storage_root: &Path, db_path: &Path) -> PathBuf {
    if storage_root.is_dir() {
        storage_root.join("doctor").join("forensics")
    } else {
        db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("doctor")
            .join("forensics")
    }
}

fn forensic_db_family_name(db_path: &Path) -> String {
    db_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("database.sqlite3")
        .to_string()
}

fn is_in_memory_db_path(path: &Path) -> bool {
    path.as_os_str() == ":memory:"
}

fn forensic_bundle_dir_component(db_path: &Path) -> String {
    if is_in_memory_db_path(db_path) {
        return "in-memory.sqlite3".to_string();
    }

    let family = forensic_db_family_name(db_path);
    let mut sanitized = String::with_capacity(family.len());
    for ch in family.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }

    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "database.sqlite3".to_string()
    } else {
        sanitized.to_string()
    }
}

fn bundle_rel_path(bundle_dir: &Path, path: &Path) -> Result<String, SqlError> {
    path.strip_prefix(bundle_dir)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|_| {
            SqlError::Custom(format!(
                "failed to compute forensic bundle relative path for {} under {}",
                path.display(),
                bundle_dir.display()
            ))
        })
}

fn bundle_sha256(path: &Path) -> Result<String, SqlError> {
    let bytes = std::fs::read(path).map_err(|error| {
        SqlError::Custom(format!(
            "failed to read forensic artifact {} for hashing: {error}",
            path.display()
        ))
    })?;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

fn recovery_sha256(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

fn recovery_receipt_db_authority_path(db_path: &Path) -> Result<PathBuf, SqlError> {
    match std::fs::symlink_metadata(db_path) {
        Ok(_) => std::fs::canonicalize(db_path).map_err(|error| {
            recovery_receipt_error("database path canonicalization", db_path, error)
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file_name = db_path.file_name().ok_or_else(|| {
                recovery_receipt_error(
                    "database path canonicalization",
                    db_path,
                    "database path has no file name",
                )
            })?;
            let parent = db_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let canonical_parent = std::fs::canonicalize(parent).map_err(|parent_error| {
                recovery_receipt_error("database parent canonicalization", parent, parent_error)
            })?;
            Ok(canonical_parent.join(file_name))
        }
        Err(error) => Err(recovery_receipt_error(
            "database path inspection",
            db_path,
            error,
        )),
    }
}

fn recovery_receipts_dir(_storage_root: &Path, db_path: &Path) -> Result<PathBuf, SqlError> {
    // Receipt authority is anchored beside the database, never beneath the
    // configured archive root. Archive-aware and plain recovery can be routed
    // with different storage-root context, but they must observe the same
    // pending marker and hash chain for a given live DB file. Canonicalizing
    // the existing file (or its parent before first creation) also prevents a
    // relative path or symlink alias from creating a second authority chain.
    let authority_path = recovery_receipt_db_authority_path(db_path)?;
    Ok(authority_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(".mcp-agent-mail-recovery-receipts")
        .join(forensic_bundle_dir_component(&authority_path)))
}

#[cfg(unix)]
fn sync_recovery_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_recovery_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    // FILE_FLAG_BACKUP_SEMANTICS is required to obtain a directory handle.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_recovery_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn recovery_receipt_error(context: &str, path: &Path, error: impl std::fmt::Display) -> SqlError {
    SqlError::Custom(format!(
        "recovery receipt {context} failed for {}: {error}",
        path.display()
    ))
}

fn receipt_table_names(
    conn: &crate::CanonicalDbConn,
    db_path: &Path,
) -> Result<BTreeSet<String>, SqlError> {
    conn.query_sync(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        &[],
    )
    .map_err(|error| recovery_receipt_error("schema inventory", db_path, error))?
    .into_iter()
    .map(|row| {
        row.get_named::<String>("name")
            .map_err(|error| recovery_receipt_error("schema inventory row decode", db_path, error))
    })
    .collect()
}

fn receipt_required_text(
    row: &sqlmodel_core::Row,
    column: &str,
    context: &str,
    db_path: &Path,
) -> Result<String, SqlError> {
    let value = row
        .get_named::<String>(column)
        .map_err(|error| recovery_receipt_error(context, db_path, error))?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(recovery_receipt_error(
            context,
            db_path,
            format!("column {column} is empty; no stable key can be formed"),
        ));
    }
    // Validate against empty/whitespace-only identities, but hash the exact
    // stored bytes. Trimming here would make semantically distinct project,
    // agent, product, or reservation values compare equal across recovery.
    Ok(value)
}

fn receipt_optional_text(
    row: &sqlmodel_core::Row,
    column: &str,
    context: &str,
    db_path: &Path,
) -> Result<Option<String>, SqlError> {
    row.get_named::<Option<String>>(column)
        .map_err(|error| recovery_receipt_error(context, db_path, error))
}

fn receipt_required_i64(
    row: &sqlmodel_core::Row,
    column: &str,
    context: &str,
    db_path: &Path,
) -> Result<i64, SqlError> {
    row.get_named::<i64>(column)
        .map_err(|error| recovery_receipt_error(context, db_path, error))
}

fn receipt_optional_i64(
    row: &sqlmodel_core::Row,
    column: &str,
    context: &str,
    db_path: &Path,
) -> Result<Option<i64>, SqlError> {
    row.get_named::<Option<i64>>(column)
        .map_err(|error| recovery_receipt_error(context, db_path, error))
}

fn receipt_text(
    row: &sqlmodel_core::Row,
    column: &str,
    context: &str,
    db_path: &Path,
) -> Result<String, SqlError> {
    row.get_named::<String>(column)
        .map_err(|error| recovery_receipt_error(context, db_path, error))
}

fn receipt_canonical_key(value: serde_json::Value, db_path: &Path) -> Result<String, SqlError> {
    serde_json::to_string(&value)
        .map_err(|error| recovery_receipt_error("stable-key serialization", db_path, error))
}

fn receipt_sort_json_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(receipt_sort_json_value).collect())
        }
        serde_json::Value::Object(values) => {
            let sorted = values
                .into_iter()
                .map(|(key, value)| (key, receipt_sort_json_value(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}

fn receipt_json_value(
    row: &sqlmodel_core::Row,
    column: &str,
    context: &str,
    db_path: &Path,
) -> Result<serde_json::Value, SqlError> {
    let raw = receipt_text(row, column, context, db_path)?;
    Ok(match serde_json::from_str(&raw) {
        Ok(value) => json!({"json": receipt_sort_json_value(value)}),
        Err(_) => json!({"invalid_json_sha256": recovery_sha256(raw.as_bytes())}),
    })
}

fn receipt_semantic_fingerprint(
    value: serde_json::Value,
    db_path: &Path,
) -> Result<String, SqlError> {
    let canonical = receipt_canonical_key(value, db_path)?;
    Ok(recovery_sha256(canonical.as_bytes()))
}

fn insert_receipt_multiset(
    multiset: &mut BTreeMap<String, usize>,
    fingerprint: String,
    db_path: &Path,
) -> Result<(), SqlError> {
    let count = multiset.entry(fingerprint).or_default();
    *count = count.checked_add(1).ok_or_else(|| {
        recovery_receipt_error(
            "semantic multiset count",
            db_path,
            "record multiplicity exceeded usize",
        )
    })?;
    Ok(())
}

fn require_unique_receipt_keys(
    keys: &BTreeSet<String>,
    row_count: usize,
    category: &str,
    db_path: &Path,
) -> Result<(), SqlError> {
    if keys.len() != row_count {
        return Err(recovery_receipt_error(
            "stable-key collision check",
            db_path,
            format!(
                "{category} produced {row_count} rows but only {} unique stable keys; refusing ambiguous recovery",
                keys.len()
            ),
        ));
    }
    Ok(())
}

fn collect_recovery_continuity_sets(db_path: &Path) -> Result<RecoveryContinuitySets, SqlError> {
    if !db_path.is_file() {
        return Err(recovery_receipt_error(
            "snapshot open",
            db_path,
            "source is missing or is not a regular file",
        ));
    }
    let config = sqlmodel_sqlite::SqliteConfig::file(db_path.to_string_lossy().into_owned())
        .flags(sqlmodel_sqlite::OpenFlags::read_only());
    let conn = crate::CanonicalDbConn::open(&config)
        .map_err(|error| recovery_receipt_error("read-only snapshot open", db_path, error))?;
    conn.execute_raw("PRAGMA query_only = ON;")
        .map_err(|error| recovery_receipt_error("enforce query-only snapshot", db_path, error))?;
    // Recovery evidence must not depend on a possibly corrupt secondary index.
    // Sorting is performed in Rust over canonical stable keys.
    conn.execute_raw("PRAGMA automatic_index = OFF;")
        .map_err(|error| recovery_receipt_error("disable automatic indexes", db_path, error))?;
    let tables = receipt_table_names(&conn, db_path)?;
    let mut sets = RecoveryContinuitySets::default();
    let mut message_fingerprints_by_id = BTreeMap::<i64, String>::new();

    if tables.contains("projects") {
        let rows = conn
            .query_sync("SELECT slug, human_key FROM projects AS p NOT INDEXED", &[])
            .map_err(|error| recovery_receipt_error("project snapshot query", db_path, error))?;
        let row_count = rows.len();
        for row in rows {
            let slug = receipt_required_text(&row, "slug", "project slug decode", db_path)?;
            let human_key =
                receipt_required_text(&row, "human_key", "project human_key decode", db_path)?;
            sets.projects.insert(receipt_canonical_key(
                json!({"slug": slug, "human_key": human_key}),
                db_path,
            )?);
        }
        require_unique_receipt_keys(&sets.projects, row_count, "projects", db_path)?;
    }

    if tables.contains("products") {
        let rows = conn
            .query_sync(
                "SELECT product_uid, name, CAST(created_at AS INTEGER) AS created_at \
                 FROM products AS product NOT INDEXED",
                &[],
            )
            .map_err(|error| recovery_receipt_error("product snapshot query", db_path, error))?;
        let row_count = rows.len();
        for row in rows {
            let product_uid =
                receipt_required_text(&row, "product_uid", "product uid decode", db_path)?;
            let name = receipt_required_text(&row, "name", "product name decode", db_path)?;
            let created_at =
                receipt_required_i64(&row, "created_at", "product created_at decode", db_path)?;
            sets.products.insert(receipt_canonical_key(
                json!({
                    "product_uid": product_uid,
                    "name": name,
                    "created_at": created_at,
                }),
                db_path,
            )?);
        }
        require_unique_receipt_keys(&sets.products, row_count, "products", db_path)?;
    }

    if tables.contains("product_project_links") {
        if !(tables.contains("products") && tables.contains("projects")) {
            return Err(recovery_receipt_error(
                "product-project link snapshot query",
                db_path,
                "product_project_links table exists without products and projects tables",
            ));
        }
        let rows = conn
            .query_sync(
                "SELECT product.product_uid AS product_uid, product.name AS product_name, \
                        project.slug AS project_slug, project.human_key AS project_human_key, \
                        CAST(link.created_at AS INTEGER) AS created_at \
                 FROM product_project_links AS link NOT INDEXED \
                 JOIN products AS product NOT INDEXED ON product.id = link.product_id \
                 JOIN projects AS project NOT INDEXED ON project.id = link.project_id",
                &[],
            )
            .map_err(|error| {
                recovery_receipt_error("product-project link snapshot query", db_path, error)
            })?;
        let table_count = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count FROM product_project_links AS link NOT INDEXED",
                &[],
            )
            .map_err(|error| {
                recovery_receipt_error("product-project link count query", db_path, error)
            })?
            .first()
            .ok_or_else(|| {
                recovery_receipt_error(
                    "product-project link count query",
                    db_path,
                    "missing count row",
                )
            })
            .and_then(|row| {
                receipt_required_i64(
                    row,
                    "row_count",
                    "product-project link count decode",
                    db_path,
                )
            })?;
        let table_count = usize::try_from(table_count).map_err(|error| {
            recovery_receipt_error("product-project link count decode", db_path, error)
        })?;
        if rows.len() != table_count {
            return Err(recovery_receipt_error(
                "product-project link ownership join",
                db_path,
                format!(
                    "joined {} of {table_count} product-project rows; refusing orphaned ownership",
                    rows.len()
                ),
            ));
        }
        for row in rows {
            let product_uid = receipt_required_text(
                &row,
                "product_uid",
                "product-project product uid decode",
                db_path,
            )?;
            let product_name = receipt_required_text(
                &row,
                "product_name",
                "product-project product name decode",
                db_path,
            )?;
            let project_slug = receipt_required_text(
                &row,
                "project_slug",
                "product-project project slug decode",
                db_path,
            )?;
            let project_human_key = receipt_required_text(
                &row,
                "project_human_key",
                "product-project project human_key decode",
                db_path,
            )?;
            let created_at = receipt_required_i64(
                &row,
                "created_at",
                "product-project created_at decode",
                db_path,
            )?;
            sets.product_project_links.insert(receipt_canonical_key(
                json!({
                    "product": {"product_uid": product_uid, "name": product_name},
                    "project": {"slug": project_slug, "human_key": project_human_key},
                    "created_at": created_at,
                }),
                db_path,
            )?);
        }
        require_unique_receipt_keys(
            &sets.product_project_links,
            table_count,
            "product_project_links",
            db_path,
        )?;
    }

    if tables.contains("agents") {
        if !tables.contains("projects") {
            return Err(recovery_receipt_error(
                "agent snapshot query",
                db_path,
                "agents table exists without projects table",
            ));
        }
        let rows = conn
            .query_sync(
                "SELECT p.slug AS project_slug, p.human_key AS project_human_key, a.name AS agent_name, \
                        CAST(a.reaper_exempt AS INTEGER) AS reaper_exempt, \
                        a.registration_token AS registration_token \
                 FROM agents AS a NOT INDEXED \
                 JOIN projects AS p NOT INDEXED ON p.id = a.project_id",
                &[],
            )
            .map_err(|error| recovery_receipt_error("agent snapshot query", db_path, error))?;
        let table_count = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count FROM agents AS a NOT INDEXED",
                &[],
            )
            .map_err(|error| recovery_receipt_error("agent count query", db_path, error))?
            .first()
            .ok_or_else(|| {
                recovery_receipt_error("agent count query", db_path, "missing count row")
            })
            .and_then(|row| {
                receipt_required_i64(row, "row_count", "agent count decode", db_path)
            })?;
        let table_count = usize::try_from(table_count)
            .map_err(|error| recovery_receipt_error("agent count decode", db_path, error))?;
        if rows.len() != table_count {
            return Err(recovery_receipt_error(
                "agent ownership join",
                db_path,
                format!(
                    "joined {} of {table_count} agent rows; refusing orphaned or cross-schema identities",
                    rows.len()
                ),
            ));
        }
        for row in rows {
            let project_slug =
                receipt_required_text(&row, "project_slug", "agent project slug decode", db_path)?;
            let project_human_key = receipt_required_text(
                &row,
                "project_human_key",
                "agent project human_key decode",
                db_path,
            )?;
            let agent_name =
                receipt_required_text(&row, "agent_name", "agent name decode", db_path)?;
            let reaper_exempt =
                receipt_required_i64(&row, "reaper_exempt", "agent reaper_exempt decode", db_path)?;
            let registration_token = receipt_optional_text(
                &row,
                "registration_token",
                "agent registration token decode",
                db_path,
            )?;
            let registration_token_present = registration_token.is_some();
            let registration_token_sha256 = registration_token
                .as_deref()
                .map(|token| recovery_sha256(token.as_bytes()));
            sets.agents.insert(receipt_canonical_key(
                json!({
                    "project": {"slug": project_slug, "human_key": project_human_key},
                    "agent": agent_name,
                    "reaper_exempt": reaper_exempt,
                    "registration_token_present": registration_token_present,
                    "registration_token_sha256": registration_token_sha256,
                }),
                db_path,
            )?);
        }
        require_unique_receipt_keys(&sets.agents, table_count, "agents", db_path)?;
    }

    if tables.contains("agent_links") {
        if !(tables.contains("projects") && tables.contains("agents")) {
            return Err(recovery_receipt_error(
                "contact snapshot query",
                db_path,
                "agent_links table exists without projects and agents tables",
            ));
        }
        let rows = conn
            .query_sync(
                "SELECT ap.slug AS a_slug, ap.human_key AS a_human_key, aa.name AS a_name, \
                        bp.slug AS b_slug, bp.human_key AS b_human_key, ba.name AS b_name, \
                        al.status AS status, al.reason AS reason, \
                        CAST(al.created_ts AS INTEGER) AS created_ts, \
                        CAST(al.updated_ts AS INTEGER) AS updated_ts, \
                        CAST(al.expires_ts AS INTEGER) AS expires_ts \
                 FROM agent_links AS al NOT INDEXED \
                 JOIN projects AS ap NOT INDEXED ON ap.id = al.a_project_id \
                 JOIN agents AS aa NOT INDEXED ON aa.id = al.a_agent_id AND aa.project_id = al.a_project_id \
                 JOIN projects AS bp NOT INDEXED ON bp.id = al.b_project_id \
                 JOIN agents AS ba NOT INDEXED ON ba.id = al.b_agent_id AND ba.project_id = al.b_project_id",
                &[],
            )
            .map_err(|error| recovery_receipt_error("contact snapshot query", db_path, error))?;
        let table_count = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count FROM agent_links AS al NOT INDEXED",
                &[],
            )
            .map_err(|error| recovery_receipt_error("contact count query", db_path, error))?
            .first()
            .ok_or_else(|| {
                recovery_receipt_error("contact count query", db_path, "missing count row")
            })
            .and_then(|row| {
                receipt_required_i64(row, "row_count", "contact count decode", db_path)
            })?;
        let table_count = usize::try_from(table_count)
            .map_err(|error| recovery_receipt_error("contact count decode", db_path, error))?;
        if rows.len() != table_count {
            return Err(recovery_receipt_error(
                "contact ownership join",
                db_path,
                format!(
                    "joined {} of {table_count} contact rows; refusing orphaned or cross-project endpoints",
                    rows.len()
                ),
            ));
        }
        for row in rows {
            let a_slug = receipt_required_text(&row, "a_slug", "contact a slug decode", db_path)?;
            let a_human_key =
                receipt_required_text(&row, "a_human_key", "contact a human_key decode", db_path)?;
            let a_name = receipt_required_text(&row, "a_name", "contact a name decode", db_path)?;
            let b_slug = receipt_required_text(&row, "b_slug", "contact b slug decode", db_path)?;
            let b_human_key =
                receipt_required_text(&row, "b_human_key", "contact b human_key decode", db_path)?;
            let b_name = receipt_required_text(&row, "b_name", "contact b name decode", db_path)?;
            let status = receipt_required_text(&row, "status", "contact status decode", db_path)?;
            let reason = receipt_text(&row, "reason", "contact reason decode", db_path)?;
            let created_ts =
                receipt_required_i64(&row, "created_ts", "contact created_ts decode", db_path)?;
            let updated_ts =
                receipt_required_i64(&row, "updated_ts", "contact updated_ts decode", db_path)?;
            let expires_ts =
                receipt_optional_i64(&row, "expires_ts", "contact expires_ts decode", db_path)?;
            sets.contacts.insert(receipt_canonical_key(
                json!({
                    "a": {"project": {"slug": a_slug, "human_key": a_human_key}, "agent": a_name},
                    "b": {"project": {"slug": b_slug, "human_key": b_human_key}, "agent": b_name},
                    "status": status,
                    "reason": reason,
                    "created_ts": created_ts,
                    "updated_ts": updated_ts,
                    "expires_ts": expires_ts,
                }),
                db_path,
            )?);
        }
        require_unique_receipt_keys(&sets.contacts, table_count, "contacts", db_path)?;
    }

    if tables.contains("file_reservations") {
        if !(tables.contains("projects") && tables.contains("agents")) {
            return Err(recovery_receipt_error(
                "reservation snapshot query",
                db_path,
                "file_reservations table exists without projects and agents tables",
            ));
        }
        let (release_join, released_ts_expr) = if tables.contains("file_reservation_releases") {
            (
                "LEFT JOIN file_reservation_releases AS rr NOT INDEXED ON rr.reservation_id = fr.id",
                "COALESCE(rr.released_ts, fr.released_ts)",
            )
        } else {
            ("", "fr.released_ts")
        };
        let reservation_snapshot_sql = format!(
            "SELECT p.slug AS project_slug, p.human_key AS project_human_key, \
                    a.name AS agent_name, fr.path_pattern AS path_pattern, \
                    CAST(fr.exclusive AS INTEGER) AS exclusive, fr.reason AS reason, \
                    CAST(fr.created_ts AS INTEGER) AS created_ts, \
                    CAST(fr.expires_ts AS INTEGER) AS expires_ts, \
                    CAST({released_ts_expr} AS INTEGER) AS released_ts \
             FROM file_reservations AS fr NOT INDEXED \
             JOIN projects AS p NOT INDEXED ON p.id = fr.project_id \
             JOIN agents AS a NOT INDEXED ON a.id = fr.agent_id AND a.project_id = fr.project_id \
             {release_join}"
        );
        let rows = conn
            .query_sync(&reservation_snapshot_sql, &[])
            .map_err(|error| {
                recovery_receipt_error("reservation snapshot query", db_path, error)
            })?;
        let table_count = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count FROM file_reservations AS fr NOT INDEXED",
                &[],
            )
            .map_err(|error| recovery_receipt_error("reservation count query", db_path, error))?
            .first()
            .ok_or_else(|| {
                recovery_receipt_error("reservation count query", db_path, "missing count row")
            })
            .and_then(|row| {
                receipt_required_i64(row, "row_count", "reservation count decode", db_path)
            })?;
        let table_count = usize::try_from(table_count)
            .map_err(|error| recovery_receipt_error("reservation count decode", db_path, error))?;
        if rows.len() != table_count {
            return Err(recovery_receipt_error(
                "reservation ownership join",
                db_path,
                format!(
                    "joined {} of {table_count} reservation rows; refusing orphaned or cross-project ownership",
                    rows.len()
                ),
            ));
        }
        for row in rows {
            let project_slug = receipt_required_text(
                &row,
                "project_slug",
                "reservation project slug decode",
                db_path,
            )?;
            let project_human_key = receipt_required_text(
                &row,
                "project_human_key",
                "reservation project human_key decode",
                db_path,
            )?;
            let agent_name = receipt_required_text(
                &row,
                "agent_name",
                "reservation agent name decode",
                db_path,
            )?;
            let path_pattern =
                receipt_required_text(&row, "path_pattern", "reservation path decode", db_path)?;
            let exclusive =
                receipt_required_i64(&row, "exclusive", "reservation exclusive decode", db_path)?;
            let reason = receipt_text(&row, "reason", "reservation reason decode", db_path)?;
            let created_ts =
                receipt_required_i64(&row, "created_ts", "reservation created_ts decode", db_path)?;
            let expires_ts =
                receipt_required_i64(&row, "expires_ts", "reservation expires_ts decode", db_path)?;
            let released_ts = receipt_optional_i64(
                &row,
                "released_ts",
                "reservation released_ts decode",
                db_path,
            )?;
            sets.reservations.insert(receipt_canonical_key(
                json!({
                    "project": {"slug": project_slug, "human_key": project_human_key},
                    "agent": agent_name,
                    "path_pattern": path_pattern,
                    "exclusive": exclusive,
                    "reason": reason,
                    "created_ts": created_ts,
                    "expires_ts": expires_ts,
                    "released_ts": released_ts,
                }),
                db_path,
            )?);
        }
        require_unique_receipt_keys(&sets.reservations, table_count, "reservations", db_path)?;
    }

    if tables.contains("messages") {
        if !(tables.contains("projects") && tables.contains("agents")) {
            return Err(recovery_receipt_error(
                "message snapshot query",
                db_path,
                "messages table exists without projects and agents tables",
            ));
        }
        let rows = conn
            .query_sync(
                "SELECT m.id AS message_id, \
                        p.slug AS project_slug, p.human_key AS project_human_key, \
                        sender.name AS sender_name, m.thread_id AS thread_id, \
                        m.subject AS subject, m.body_md AS body_md, \
                        m.importance AS importance, \
                        CAST(m.ack_required AS INTEGER) AS ack_required, \
                        CAST(m.created_ts AS INTEGER) AS created_ts, \
                        m.recipients_json AS recipients_json, m.attachments AS attachments \
                 FROM messages AS m NOT INDEXED \
                 JOIN projects AS p NOT INDEXED ON p.id = m.project_id \
                 JOIN agents AS sender NOT INDEXED \
                   ON sender.id = m.sender_id AND sender.project_id = m.project_id",
                &[],
            )
            .map_err(|error| recovery_receipt_error("message snapshot query", db_path, error))?;
        let table_count = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count FROM messages AS m NOT INDEXED",
                &[],
            )
            .map_err(|error| recovery_receipt_error("message count query", db_path, error))?
            .first()
            .ok_or_else(|| {
                recovery_receipt_error("message count query", db_path, "missing count row")
            })
            .and_then(|row| {
                receipt_required_i64(row, "row_count", "message count decode", db_path)
            })?;
        let table_count = usize::try_from(table_count)
            .map_err(|error| recovery_receipt_error("message count decode", db_path, error))?;
        if rows.len() != table_count {
            return Err(recovery_receipt_error(
                "message ownership join",
                db_path,
                format!(
                    "joined {} of {table_count} message rows; refusing orphaned or cross-project senders",
                    rows.len()
                ),
            ));
        }
        for row in rows {
            let message_id =
                receipt_required_i64(&row, "message_id", "message id decode", db_path)?;
            let project_slug = receipt_required_text(
                &row,
                "project_slug",
                "message project slug decode",
                db_path,
            )?;
            let project_human_key = receipt_required_text(
                &row,
                "project_human_key",
                "message project human_key decode",
                db_path,
            )?;
            let sender_name =
                receipt_required_text(&row, "sender_name", "message sender decode", db_path)?;
            let thread_id =
                receipt_optional_text(&row, "thread_id", "message thread decode", db_path)?;
            let subject = receipt_text(&row, "subject", "message subject decode", db_path)?;
            let body_md = receipt_text(&row, "body_md", "message body decode", db_path)?;
            let importance =
                receipt_text(&row, "importance", "message importance decode", db_path)?;
            let ack_required =
                receipt_required_i64(&row, "ack_required", "message ack_required decode", db_path)?;
            let created_ts =
                receipt_required_i64(&row, "created_ts", "message created_ts decode", db_path)?;
            let recipients_json = receipt_json_value(
                &row,
                "recipients_json",
                "message recipients_json decode",
                db_path,
            )?;
            let attachments =
                receipt_json_value(&row, "attachments", "message attachments decode", db_path)?;
            let fingerprint = receipt_semantic_fingerprint(
                json!({
                    "project": {"slug": project_slug, "human_key": project_human_key},
                    "sender": sender_name,
                    "thread_id": thread_id,
                    "subject": subject,
                    "body_md": body_md,
                    "importance": importance,
                    "ack_required": ack_required,
                    "created_ts": created_ts,
                    "recipients_json": recipients_json,
                    "attachments": attachments,
                }),
                db_path,
            )?;
            if message_fingerprints_by_id
                .insert(message_id, fingerprint.clone())
                .is_some()
            {
                return Err(recovery_receipt_error(
                    "message identity snapshot",
                    db_path,
                    format!("duplicate numeric message id {message_id}"),
                ));
            }
            insert_receipt_multiset(&mut sets.messages, fingerprint, db_path)?;
        }
    }

    if tables.contains("message_recipients") {
        if !(tables.contains("messages")
            && tables.contains("projects")
            && tables.contains("agents"))
        {
            return Err(recovery_receipt_error(
                "message-recipient snapshot query",
                db_path,
                "message_recipients table exists without messages, projects, and agents tables",
            ));
        }
        let rows = conn
            .query_sync(
                "SELECT mr.message_id AS message_id, \
                        p.slug AS project_slug, p.human_key AS project_human_key, \
                        recipient.name AS recipient_name, mr.kind AS kind, \
                        CAST(mr.read_ts AS INTEGER) AS read_ts, \
                        CAST(mr.ack_ts AS INTEGER) AS ack_ts \
                 FROM message_recipients AS mr NOT INDEXED \
                 JOIN messages AS m NOT INDEXED ON m.id = mr.message_id \
                 JOIN projects AS p NOT INDEXED ON p.id = m.project_id \
                 JOIN agents AS recipient NOT INDEXED \
                   ON recipient.id = mr.agent_id AND recipient.project_id = m.project_id",
                &[],
            )
            .map_err(|error| {
                recovery_receipt_error("message-recipient snapshot query", db_path, error)
            })?;
        let table_count = conn
            .query_sync(
                "SELECT COUNT(*) AS row_count FROM message_recipients AS mr NOT INDEXED",
                &[],
            )
            .map_err(|error| {
                recovery_receipt_error("message-recipient count query", db_path, error)
            })?
            .first()
            .ok_or_else(|| {
                recovery_receipt_error(
                    "message-recipient count query",
                    db_path,
                    "missing count row",
                )
            })
            .and_then(|row| {
                receipt_required_i64(row, "row_count", "message-recipient count decode", db_path)
            })?;
        let table_count = usize::try_from(table_count).map_err(|error| {
            recovery_receipt_error("message-recipient count decode", db_path, error)
        })?;
        if rows.len() != table_count {
            return Err(recovery_receipt_error(
                "message-recipient ownership join",
                db_path,
                format!(
                    "joined {} of {table_count} recipient rows; refusing orphaned or cross-project recipients",
                    rows.len()
                ),
            ));
        }
        for row in rows {
            let message_id = receipt_required_i64(
                &row,
                "message_id",
                "message-recipient message id decode",
                db_path,
            )?;
            let message_fingerprint = message_fingerprints_by_id.get(&message_id).ok_or_else(|| {
                recovery_receipt_error(
                    "message-recipient parent identity",
                    db_path,
                    format!("recipient references message {message_id} without a semantic fingerprint"),
                )
            })?;
            let project_slug = receipt_required_text(
                &row,
                "project_slug",
                "message-recipient project slug decode",
                db_path,
            )?;
            let project_human_key = receipt_required_text(
                &row,
                "project_human_key",
                "message-recipient project human_key decode",
                db_path,
            )?;
            let recipient_name = receipt_required_text(
                &row,
                "recipient_name",
                "message-recipient agent decode",
                db_path,
            )?;
            let kind =
                receipt_required_text(&row, "kind", "message-recipient kind decode", db_path)?;
            let read_ts =
                receipt_optional_i64(&row, "read_ts", "message-recipient read_ts decode", db_path)?;
            let ack_ts =
                receipt_optional_i64(&row, "ack_ts", "message-recipient ack_ts decode", db_path)?;
            let fingerprint = receipt_semantic_fingerprint(
                json!({
                    "message_sha256": message_fingerprint,
                    "project": {"slug": project_slug, "human_key": project_human_key},
                    "recipient": recipient_name,
                    "kind": kind,
                    "read_ts": read_ts,
                    "ack_ts": ack_ts,
                }),
                db_path,
            )?;
            insert_receipt_multiset(&mut sets.message_recipients, fingerprint, db_path)?;
        }
    }

    if tables.contains("proof_gate_consumed_nonces") {
        let rows = conn
            .query_sync(
                "SELECT issuer_key, nonce, \
                        CAST(retain_until AS INTEGER) AS retain_until, \
                        CAST(consumed_at AS INTEGER) AS consumed_at \
                 FROM proof_gate_consumed_nonces AS consumed_nonce NOT INDEXED",
                &[],
            )
            .map_err(|error| {
                recovery_receipt_error("proof-gate nonce snapshot query", db_path, error)
            })?;
        let row_count = rows.len();
        for row in rows {
            let issuer_key = receipt_required_text(
                &row,
                "issuer_key",
                "proof-gate nonce issuer decode",
                db_path,
            )?;
            let nonce = receipt_required_text(&row, "nonce", "proof-gate nonce decode", db_path)?;
            let retain_until = receipt_required_i64(
                &row,
                "retain_until",
                "proof-gate nonce retain_until decode",
                db_path,
            )?;
            let consumed_at = receipt_required_i64(
                &row,
                "consumed_at",
                "proof-gate nonce consumed_at decode",
                db_path,
            )?;
            sets.proof_gate_consumed_nonces
                .insert(receipt_canonical_key(
                    json!({
                        "issuer_key": issuer_key,
                        "nonce": nonce,
                        "retain_until": retain_until,
                        "consumed_at": consumed_at,
                    }),
                    db_path,
                )?);
        }
        require_unique_receipt_keys(
            &sets.proof_gate_consumed_nonces,
            row_count,
            "proof_gate_consumed_nonces",
            db_path,
        )?;
    }

    Ok(sets)
}

fn recovery_category(keys: &BTreeSet<String>) -> Result<RecoveryReceiptCategory, SqlError> {
    let entries = keys.iter().collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&entries).map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt category serialization failed: {error}"
        ))
    })?;
    Ok(RecoveryReceiptCategory {
        count: keys.len(),
        sha256: recovery_sha256(&bytes),
    })
}

fn recovery_multiset_category(
    keys: &BTreeMap<String, usize>,
) -> Result<RecoveryReceiptCategory, SqlError> {
    let count = keys.values().try_fold(0usize, |total, count| {
        total.checked_add(*count).ok_or_else(|| {
            SqlError::Custom("recovery receipt semantic multiset count exceeded usize".to_string())
        })
    })?;
    let entries = keys.iter().collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&entries).map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt semantic multiset serialization failed: {error}"
        ))
    })?;
    Ok(RecoveryReceiptCategory {
        count,
        sha256: recovery_sha256(&bytes),
    })
}

fn recovery_snapshot(
    sets: &RecoveryContinuitySets,
) -> Result<RecoveryContinuitySnapshot, SqlError> {
    let projects = recovery_category(&sets.projects)?;
    let products = recovery_category(&sets.products)?;
    let product_project_links = recovery_category(&sets.product_project_links)?;
    let agents = recovery_category(&sets.agents)?;
    let contacts = recovery_category(&sets.contacts)?;
    let reservations = recovery_category(&sets.reservations)?;
    let messages = recovery_multiset_category(&sets.messages)?;
    let message_recipients = recovery_multiset_category(&sets.message_recipients)?;
    let proof_gate_consumed_nonces = recovery_category(&sets.proof_gate_consumed_nonces)?;
    let aggregate_bytes = serde_json::to_vec(&(
        &projects,
        &products,
        &product_project_links,
        &agents,
        &contacts,
        &reservations,
        &messages,
        &message_recipients,
        &proof_gate_consumed_nonces,
    ))
    .map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt aggregate serialization failed: {error}"
        ))
    })?;
    Ok(RecoveryContinuitySnapshot {
        projects,
        products,
        product_project_links,
        agents,
        contacts,
        reservations,
        messages,
        message_recipients,
        proof_gate_consumed_nonces,
        aggregate_sha256: recovery_sha256(&aggregate_bytes),
    })
}

fn recovery_delta_category(
    source: &BTreeSet<String>,
    candidate: &BTreeSet<String>,
) -> Result<RecoveryReceiptDeltaCategory, SqlError> {
    let added = candidate.difference(source).cloned().collect::<Vec<_>>();
    let lost = source.difference(candidate).cloned().collect::<Vec<_>>();
    let added_bytes = serde_json::to_vec(&added).map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt added-delta serialization failed: {error}"
        ))
    })?;
    let lost_bytes = serde_json::to_vec(&lost).map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt lost-delta serialization failed: {error}"
        ))
    })?;
    Ok(RecoveryReceiptDeltaCategory {
        added_count: added.len(),
        lost_count: lost.len(),
        added_sha256: recovery_sha256(&added_bytes),
        lost_sha256: recovery_sha256(&lost_bytes),
        added,
        lost,
    })
}

fn recovery_multiset_delta_category(
    source: &BTreeMap<String, usize>,
    candidate: &BTreeMap<String, usize>,
) -> Result<RecoveryReceiptDeltaCategory, SqlError> {
    let mut added = Vec::new();
    let mut lost = Vec::new();
    for (fingerprint, candidate_count) in candidate {
        let source_count = source.get(fingerprint).copied().unwrap_or(0);
        added.extend(std::iter::repeat_n(
            fingerprint.clone(),
            candidate_count.saturating_sub(source_count),
        ));
    }
    for (fingerprint, source_count) in source {
        let candidate_count = candidate.get(fingerprint).copied().unwrap_or(0);
        lost.extend(std::iter::repeat_n(
            fingerprint.clone(),
            source_count.saturating_sub(candidate_count),
        ));
    }
    let added_bytes = serde_json::to_vec(&added).map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt semantic multiset added-delta serialization failed: {error}"
        ))
    })?;
    let lost_bytes = serde_json::to_vec(&lost).map_err(|error| {
        SqlError::Custom(format!(
            "recovery receipt semantic multiset lost-delta serialization failed: {error}"
        ))
    })?;
    Ok(RecoveryReceiptDeltaCategory {
        added_count: added.len(),
        lost_count: lost.len(),
        added_sha256: recovery_sha256(&added_bytes),
        lost_sha256: recovery_sha256(&lost_bytes),
        added,
        lost,
    })
}

fn recovery_multiset_duplicate_inflation(
    source: &BTreeMap<String, usize>,
    candidate: &BTreeMap<String, usize>,
) -> usize {
    candidate
        .iter()
        .map(|(fingerprint, candidate_count)| {
            let permitted = source.get(fingerprint).copied().unwrap_or(1);
            candidate_count.saturating_sub(permitted)
        })
        .sum()
}

fn recovery_delta(
    source: &RecoveryContinuitySets,
    candidate: &RecoveryContinuitySets,
) -> Result<RecoveryContinuityDelta, SqlError> {
    Ok(RecoveryContinuityDelta {
        projects: recovery_delta_category(&source.projects, &candidate.projects)?,
        products: recovery_delta_category(&source.products, &candidate.products)?,
        product_project_links: recovery_delta_category(
            &source.product_project_links,
            &candidate.product_project_links,
        )?,
        agents: recovery_delta_category(&source.agents, &candidate.agents)?,
        contacts: recovery_delta_category(&source.contacts, &candidate.contacts)?,
        reservations: recovery_delta_category(&source.reservations, &candidate.reservations)?,
        messages: recovery_multiset_delta_category(&source.messages, &candidate.messages)?,
        message_recipients: recovery_multiset_delta_category(
            &source.message_recipients,
            &candidate.message_recipients,
        )?,
        proof_gate_consumed_nonces: recovery_delta_category(
            &source.proof_gate_consumed_nonces,
            &candidate.proof_gate_consumed_nonces,
        )?,
    })
}

fn recovery_delta_has_loss(delta: &RecoveryContinuityDelta) -> bool {
    delta.projects.lost_count > 0
        || delta.products.lost_count > 0
        || delta.product_project_links.lost_count > 0
        || delta.agents.lost_count > 0
        || delta.contacts.lost_count > 0
        || delta.reservations.lost_count > 0
        || delta.messages.lost_count > 0
        || delta.message_recipients.lost_count > 0
        || delta.proof_gate_consumed_nonces.lost_count > 0
}

fn finalized_recovery_receipt_paths(receipts_dir: &Path) -> Result<Vec<PathBuf>, SqlError> {
    if !receipts_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(receipts_dir)
        .map_err(|error| recovery_receipt_error("directory scan", receipts_dir, error))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| recovery_receipt_error("directory entry read", receipts_dir, error))?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| recovery_receipt_error("artifact inspect", &path, error))?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(recovery_receipt_error(
                    "artifact type check",
                    &path,
                    "finalized receipt is not a regular non-symlink file",
                ));
            }
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn pending_recovery_receipt_paths(receipts_dir: &Path) -> Result<Vec<PathBuf>, SqlError> {
    if !receipts_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(receipts_dir)
        .map_err(|error| recovery_receipt_error("pending scan", receipts_dir, error))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| recovery_receipt_error("pending entry read", receipts_dir, error))?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "pending")
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Move an uncommitted singleton intent to an explicit terminal artifact.
///
/// A pending filename means that activation may have reached an indeterminate
/// point, so startup must fail closed while it exists. Once the caller has
/// proved that the old generation is authoritative (or preparation failed
/// before activation), an atomic rename to `.aborted` records that terminal
/// outcome without deleting forensic evidence and releases singleton
/// admission for a future recovery.
fn abort_pending_recovery_receipt_path(pending_path: &Path) -> Result<PathBuf, SqlError> {
    let receipts_dir = pending_path.parent().ok_or_else(|| {
        recovery_receipt_error(
            "abort parent resolution",
            pending_path,
            "pending receipt has no parent directory",
        )
    })?;
    let aborted_at_us = mcp_agent_mail_core::timestamps::now_micros();
    let aborted_path = (0_u32..10_000)
        .find_map(|suffix| {
            let candidate = receipts_dir.join(format!(
                "recovery-aborted-{aborted_at_us:020}-{:010}-{suffix:04}.receipt.aborted",
                std::process::id()
            ));
            (!candidate.exists()).then_some(candidate)
        })
        .ok_or_else(|| {
            recovery_receipt_error(
                "abort filename allocation",
                receipts_dir,
                "exhausted 10,000 aborted receipt suffixes",
            )
        })?;
    std::fs::rename(pending_path, &aborted_path)
        .map_err(|error| recovery_receipt_error("abort rename", &aborted_path, error))?;
    sync_recovery_directory(receipts_dir)
        .map_err(|error| recovery_receipt_error("abort directory sync", receipts_dir, error))?;
    Ok(aborted_path)
}

fn verify_recovery_receipt_document(
    path: &Path,
    bytes: &[u8],
) -> Result<RecoveryReceiptDocument, SqlError> {
    let document: RecoveryReceiptDocument = serde_json::from_slice(bytes)
        .map_err(|error| recovery_receipt_error("JSON decode", path, error))?;
    if document.body.schema != "mcp-agent-mail-recovery-receipt.v1" {
        return Err(recovery_receipt_error(
            "schema check",
            path,
            format!("unsupported schema {}", document.body.schema),
        ));
    }
    let body_bytes = serde_json::to_vec(&document.body)
        .map_err(|error| recovery_receipt_error("canonical body serialization", path, error))?;
    let expected_self_hash = recovery_sha256(&body_bytes);
    if document.self_sha256 != expected_self_hash {
        return Err(recovery_receipt_error(
            "self-hash check",
            path,
            format!(
                "stored {}, computed {expected_self_hash}",
                document.self_sha256
            ),
        ));
    }
    Ok(document)
}

fn verify_finalized_recovery_receipt_chain(
    receipts_dir: &Path,
) -> Result<Option<VerifiedRecoveryReceiptChain>, SqlError> {
    let mut documents_by_bytes_sha256 = BTreeMap::new();
    let mut receipt_ids = BTreeSet::new();
    for path in finalized_recovery_receipt_paths(receipts_dir)? {
        let bytes =
            std::fs::read(&path).map_err(|error| recovery_receipt_error("read", &path, error))?;
        let document = verify_recovery_receipt_document(&path, &bytes)?;
        if !receipt_ids.insert(document.body.receipt_id.clone()) {
            return Err(recovery_receipt_error(
                "receipt id uniqueness check",
                &path,
                format!("duplicate receipt id {}", document.body.receipt_id),
            ));
        }
        let bytes_sha256 = recovery_sha256(&bytes);
        if documents_by_bytes_sha256
            .insert(bytes_sha256.clone(), (path.clone(), document))
            .is_some()
        {
            return Err(recovery_receipt_error(
                "receipt byte-hash uniqueness check",
                &path,
                format!("duplicate finalized receipt bytes {bytes_sha256}"),
            ));
        }
    }
    if documents_by_bytes_sha256.is_empty() {
        return Ok(None);
    }

    let roots = documents_by_bytes_sha256
        .iter()
        .filter(|(_, (_, document))| document.body.previous_receipt_bytes_sha256.is_none())
        .map(|(bytes_sha256, _)| bytes_sha256.clone())
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err(recovery_receipt_error(
            "chain root check",
            receipts_dir,
            format!("expected exactly one root receipt, found {}", roots.len()),
        ));
    }

    let mut successor_by_predecessor = BTreeMap::new();
    for (bytes_sha256, (path, document)) in &documents_by_bytes_sha256 {
        let Some(predecessor) = document.body.previous_receipt_bytes_sha256.as_ref() else {
            continue;
        };
        if !documents_by_bytes_sha256.contains_key(predecessor) {
            return Err(recovery_receipt_error(
                "chain-link check",
                path,
                format!("predecessor {predecessor} is not present in the finalized receipt set"),
            ));
        }
        if let Some(existing_successor) =
            successor_by_predecessor.insert(predecessor.clone(), bytes_sha256.clone())
        {
            return Err(recovery_receipt_error(
                "chain fork check",
                path,
                format!(
                    "predecessor {predecessor} has multiple successors: {existing_successor} and {bytes_sha256}"
                ),
            ));
        }
    }

    let mut visited = BTreeSet::new();
    let mut tip_bytes_sha256 = roots[0].clone();
    loop {
        if !visited.insert(tip_bytes_sha256.clone()) {
            return Err(recovery_receipt_error(
                "chain cycle check",
                receipts_dir,
                format!("cycle encountered at receipt {tip_bytes_sha256}"),
            ));
        }
        let Some(successor) = successor_by_predecessor.get(&tip_bytes_sha256) else {
            break;
        };
        tip_bytes_sha256.clone_from(successor);
    }
    if visited.len() != documents_by_bytes_sha256.len() {
        return Err(recovery_receipt_error(
            "chain connectivity check",
            receipts_dir,
            format!(
                "traversed {} of {} finalized receipts",
                visited.len(),
                documents_by_bytes_sha256.len()
            ),
        ));
    }
    let latest = documents_by_bytes_sha256
        .get(&tip_bytes_sha256)
        .map(|(_, document)| document.clone())
        .ok_or_else(|| {
            recovery_receipt_error(
                "chain tip lookup",
                receipts_dir,
                format!("missing traversed tip {tip_bytes_sha256}"),
            )
        })?;
    Ok(Some(VerifiedRecoveryReceiptChain {
        tip_bytes_sha256,
        latest,
    }))
}

const RECOVERY_RECEIPT_MARKERS_TABLE: &str = "mailbox_recovery_receipt_markers";

fn install_candidate_recovery_marker(
    candidate_path: &Path,
    document: &RecoveryReceiptDocument,
) -> Result<(), SqlError> {
    let conn = crate::CanonicalDbConn::open_file(candidate_path.to_string_lossy().as_ref())
        .map_err(|error| recovery_receipt_error("candidate marker open", candidate_path, error))?;
    conn.execute_raw(&format!(
        "CREATE TABLE IF NOT EXISTS {RECOVERY_RECEIPT_MARKERS_TABLE} (\
             receipt_id TEXT PRIMARY KEY, \
             receipt_self_sha256 TEXT NOT NULL, \
             candidate_snapshot_sha256 TEXT NOT NULL, \
             prepared_at_us INTEGER NOT NULL\
         )"
    ))
    .map_err(|error| recovery_receipt_error("candidate marker schema", candidate_path, error))?;
    conn.execute_sync(
        &format!(
            "INSERT INTO {RECOVERY_RECEIPT_MARKERS_TABLE} (\
                 receipt_id, receipt_self_sha256, candidate_snapshot_sha256, prepared_at_us\
             ) VALUES (?, ?, ?, ?)"
        ),
        &[
            Value::Text(document.body.receipt_id.clone()),
            Value::Text(document.self_sha256.clone()),
            Value::Text(document.body.candidate.aggregate_sha256.clone()),
            Value::BigInt(document.body.prepared_at_us),
        ],
    )
    .map_err(|error| recovery_receipt_error("candidate marker insert", candidate_path, error))?;
    let rows = conn
        .query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[])
        .map_err(|error| {
            recovery_receipt_error("candidate marker checkpoint", candidate_path, error)
        })?;
    if rows
        .first()
        .and_then(|row| row.get_named::<i64>("busy").ok())
        .is_some_and(|busy| busy != 0)
    {
        return Err(recovery_receipt_error(
            "candidate marker checkpoint",
            candidate_path,
            "checkpoint reported busy",
        ));
    }
    drop(conn);
    verify_live_recovery_marker(candidate_path, document)
}

fn verify_live_recovery_marker(
    db_path: &Path,
    document: &RecoveryReceiptDocument,
) -> Result<(), SqlError> {
    if !db_path.is_file() {
        return Err(recovery_receipt_error(
            "live marker open",
            db_path,
            "finalized recovery receipt exists but the live database is missing or not a regular file",
        ));
    }
    let conn = crate::CanonicalDbConn::open_file(db_path.to_string_lossy().as_ref())
        .map_err(|error| recovery_receipt_error("live marker open", db_path, error))?;
    let rows = conn
        .query_sync(
            &format!(
                "SELECT receipt_self_sha256, candidate_snapshot_sha256 \
                 FROM {RECOVERY_RECEIPT_MARKERS_TABLE} \
                 WHERE receipt_id = ? LIMIT 2"
            ),
            &[Value::Text(document.body.receipt_id.clone())],
        )
        .map_err(|error| recovery_receipt_error("live marker query", db_path, error))?;
    if rows.len() != 1 {
        return Err(recovery_receipt_error(
            "live marker cardinality",
            db_path,
            format!(
                "finalized receipt {} has {} matching live marker rows; expected exactly one",
                document.body.receipt_id,
                rows.len()
            ),
        ));
    }
    let row = &rows[0];
    let marker_self_sha256 = receipt_required_text(
        row,
        "receipt_self_sha256",
        "live marker self-hash decode",
        db_path,
    )?;
    let marker_candidate_sha256 = receipt_required_text(
        row,
        "candidate_snapshot_sha256",
        "live marker candidate-hash decode",
        db_path,
    )?;
    if marker_self_sha256 != document.self_sha256
        || marker_candidate_sha256 != document.body.candidate.aggregate_sha256
    {
        return Err(recovery_receipt_error(
            "live marker consistency",
            db_path,
            format!(
                "finalized receipt {} does not match the live database generation marker",
                document.body.receipt_id
            ),
        ));
    }
    Ok(())
}

fn verify_live_recovery_candidate_snapshot(
    db_path: &Path,
    document: &RecoveryReceiptDocument,
) -> Result<(), SqlError> {
    let actual_sets = collect_recovery_continuity_sets(db_path)?;
    let actual_snapshot = recovery_snapshot(&actual_sets)?;
    if actual_snapshot != document.body.candidate {
        return Err(recovery_receipt_error(
            "live candidate continuity check",
            db_path,
            format!(
                "receipt {} prepared candidate snapshot {}, but the activated database now has {}; refusing to commit a stale recovery receipt",
                document.body.receipt_id,
                document.body.candidate.aggregate_sha256,
                actual_snapshot.aggregate_sha256
            ),
        ));
    }
    Ok(())
}

/// Make an activated recovery database and its directory entry durable before
/// the receipt promotion marker can be committed.
pub(crate) fn sync_activated_recovery_database(db_path: &Path) -> Result<(), SqlError> {
    let file = std::fs::File::open(db_path)
        .map_err(|error| recovery_receipt_error("activated database open", db_path, error))?;
    file.sync_all()
        .map_err(|error| recovery_receipt_error("activated database file sync", db_path, error))?;
    let parent = db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_recovery_directory(parent).map_err(|error| {
        recovery_receipt_error("activated database parent directory sync", parent, error)
    })
}

/// Refuse startup/recovery if a prior promotion was not durably receipted, or
/// if any finalized receipt's self-hash/chain link is invalid.
pub(crate) fn verify_recovery_receipt_state(
    storage_root: &Path,
    db_path: &Path,
) -> Result<(), SqlError> {
    let receipts_dir = recovery_receipts_dir(storage_root, db_path)?;
    let pending = pending_recovery_receipt_paths(&receipts_dir)?;
    if !pending.is_empty() {
        return Err(SqlError::Custom(format!(
            "unfinalized recovery receipt intent(s) exist for {}: {}; refusing readiness because a prior candidate promotion may have crashed before its receipt was committed",
            db_path.display(),
            pending
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if let Some(chain) = verify_finalized_recovery_receipt_chain(&receipts_dir)? {
        verify_live_recovery_marker(db_path, &chain.latest)?;
    }
    Ok(())
}

/// Verify receipt admission before replacing a known-unhealthy live database.
///
/// A finalized chain must remain structurally valid and no pending intent may
/// exist. A readable source must also carry the chain-tip marker. The sole
/// exception is a source independently confirmed unhealthy: corruption can
/// make its marker unreadable, and blocking recovery in that state would make
/// a previously receipted mailbox impossible to repair after a later incident.
pub(crate) fn verify_recovery_receipt_state_for_promotion(
    storage_root: &Path,
    db_path: &Path,
) -> Result<(), SqlError> {
    let receipts_dir = recovery_receipts_dir(storage_root, db_path)?;
    let pending = pending_recovery_receipt_paths(&receipts_dir)?;
    if !pending.is_empty() {
        return Err(SqlError::Custom(format!(
            "unfinalized recovery receipt intent(s) exist for {}: {}; refusing promotion because a prior candidate activation may be indeterminate",
            db_path.display(),
            pending
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let Some(chain) = verify_finalized_recovery_receipt_chain(&receipts_dir)? else {
        return Ok(());
    };
    match verify_live_recovery_marker(db_path, &chain.latest) {
        Ok(()) => Ok(()),
        Err(marker_error) => match crate::pool::sqlite_file_is_healthy(db_path) {
            Ok(false) => {
                tracing::warn!(
                    db_path = %db_path.display(),
                    receipt_id = %chain.latest.body.receipt_id,
                    error = %marker_error,
                    "confirmed-corrupt source cannot expose its prior recovery marker; preserving the verified chain as the predecessor for a new recovery"
                );
                Ok(())
            }
            Ok(true) => Err(marker_error),
            Err(health_error) => Err(recovery_receipt_error(
                "promotion source health classification",
                db_path,
                format!(
                    "live marker verification failed ({marker_error}); health classification also failed ({health_error})"
                ),
            )),
        },
    }
}

#[derive(Debug)]
struct RecoveryReceiptEvidence {
    source: RecoveryContinuitySnapshot,
    candidate: RecoveryContinuitySnapshot,
    delta: RecoveryContinuityDelta,
    source_snapshot_failure_sha256: Option<String>,
    previous_receipt_bytes_sha256: Option<String>,
}

fn collect_recovery_receipt_evidence(
    receipts_dir: &Path,
    source_path: Option<&Path>,
    candidate_path: &Path,
) -> Result<RecoveryReceiptEvidence, SqlError> {
    let chain = verify_finalized_recovery_receipt_chain(receipts_dir)?;
    let (source_sets, source_snapshot_failure_sha256) = match source_path {
        Some(path) => match collect_recovery_continuity_sets(path) {
            Ok(sets) => (sets, None),
            Err(snapshot_error) => match crate::pool::sqlite_file_is_healthy(path) {
                Ok(false) => (
                    RecoveryContinuitySets::default(),
                    Some(recovery_sha256(snapshot_error.to_string().as_bytes())),
                ),
                Ok(true) => return Err(snapshot_error),
                Err(health_error) => {
                    return Err(recovery_receipt_error(
                        "source generation health classification",
                        path,
                        format!(
                            "semantic snapshot failed ({snapshot_error}); health classification also failed ({health_error})"
                        ),
                    ));
                }
            },
        },
        None => (RecoveryContinuitySets::default(), None),
    };
    if let Some(existing_chain) = chain.as_ref() {
        let source_path = source_path.ok_or_else(|| {
            recovery_receipt_error(
                "source generation continuity check",
                receipts_dir,
                "a finalized receipt chain exists but this recovery has no source database generation",
            )
        })?;
        // A readable source must carry the marker for the current chain tip.
        // When corruption has made the source unreadable, the verified chain
        // remains the durable predecessor and this receipt explicitly records
        // that source continuity could not be re-inventoried.
        if source_snapshot_failure_sha256.is_none() {
            verify_live_recovery_marker(source_path, &existing_chain.latest)?;
        }
    }
    let candidate_sets = collect_recovery_continuity_sets(candidate_path)?;
    let source = recovery_snapshot(&source_sets)?;
    let candidate = recovery_snapshot(&candidate_sets)?;
    let delta = recovery_delta(&source_sets, &candidate_sets)?;
    if source_snapshot_failure_sha256.is_none() && recovery_delta_has_loss(&delta) {
        return Err(SqlError::Custom(format!(
            "recovery candidate {} would lose stable coordination keys from {} (projects={}, products={}, product_project_links={}, agents={}, contacts={}, reservations={}, messages={}, message_recipients={}, proof_gate_consumed_nonces={}); refusing promotion",
            candidate_path.display(),
            source_path.map_or_else(
                || "an empty source".to_string(),
                |path| path.display().to_string()
            ),
            delta.projects.lost_count,
            delta.products.lost_count,
            delta.product_project_links.lost_count,
            delta.agents.lost_count,
            delta.contacts.lost_count,
            delta.reservations.lost_count,
            delta.messages.lost_count,
            delta.message_recipients.lost_count,
            delta.proof_gate_consumed_nonces.lost_count,
        )));
    }
    // Multiplicity inflation is meaningful only relative to a readable source.
    // When the source is corrupt, preserve the candidate's full multiset and
    // let the explicit unreadable-source attestation carry that uncertainty.
    if source_snapshot_failure_sha256.is_none() {
        let duplicate_messages =
            recovery_multiset_duplicate_inflation(&source_sets.messages, &candidate_sets.messages);
        let duplicate_recipients = recovery_multiset_duplicate_inflation(
            &source_sets.message_recipients,
            &candidate_sets.message_recipients,
        );
        if duplicate_messages > 0 || duplicate_recipients > 0 {
            return Err(SqlError::Custom(format!(
                "recovery candidate {} inflates an existing semantic multiplicity (messages={duplicate_messages}, message_recipients={duplicate_recipients}); refusing possible duplicate replay",
                candidate_path.display()
            )));
        }
    }
    Ok(RecoveryReceiptEvidence {
        source,
        candidate,
        delta,
        source_snapshot_failure_sha256,
        previous_receipt_bytes_sha256: chain.map(|chain| chain.tip_bytes_sha256),
    })
}

const RECOVERY_RECEIPT_SINGLETON_PENDING_FILE: &str = "recovery-admission.receipt.pending";

/// Build and durably persist a promotion intent from deterministic stable-key
/// snapshots. Any lost coordination/security key aborts before the live path
/// can be replaced.
pub(crate) fn prepare_recovery_receipt(
    storage_root: &Path,
    db_path: &Path,
    source_path: Option<&Path>,
    candidate_path: &Path,
) -> Result<PreparedRecoveryReceipt, SqlError> {
    let authority_path = recovery_receipt_db_authority_path(db_path)?;
    let receipts_dir = recovery_receipts_dir(storage_root, &authority_path)?;
    std::fs::create_dir_all(&receipts_dir)
        .map_err(|error| recovery_receipt_error("directory create", &receipts_dir, error))?;
    if let Some(receipts_root) = receipts_dir.parent() {
        // `create_dir_all` may have created both the authority root beside the
        // database and this database-specific child. Sync each namespace that
        // gained a directory entry before any live candidate can be activated.
        if let Some(db_parent) = receipts_root.parent() {
            sync_recovery_directory(db_parent).map_err(|error| {
                recovery_receipt_error("database parent directory sync", db_parent, error)
            })?;
        }
        sync_recovery_directory(receipts_root).map_err(|error| {
            recovery_receipt_error("receipt root directory sync", receipts_root, error)
        })?;
    }
    sync_recovery_directory(&receipts_dir)
        .map_err(|error| recovery_receipt_error("directory sync", &receipts_dir, error))?;

    let pending = pending_recovery_receipt_paths(&receipts_dir)?;
    if !pending.is_empty() {
        return Err(SqlError::Custom(format!(
            "cannot prepare recovery receipt for {} while unfinalized intent(s) exist: {}",
            db_path.display(),
            pending
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    // Validate deterministic loss before acquiring the singleton intent so a
    // rejected candidate does not leave an admission marker behind.
    let _prevalidated_evidence =
        collect_recovery_receipt_evidence(&receipts_dir, source_path, candidate_path)?;

    // `create_new` on one fixed pathname is the cross-process compare/exchange.
    // Unique per-receipt pending names would let two processes both pass the
    // preceding empty-directory scan and prepare competing promotions.
    let pending_path = receipts_dir.join(RECOVERY_RECEIPT_SINGLETON_PENDING_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending_path)
        .map_err(|error| recovery_receipt_error("singleton admission", &pending_path, error))?;

    let prepare_after_admission = (|| {
        // Re-read the chain and both generations after winning admission.
        // Another process may have finalized between prevalidation and our
        // create_new.
        let evidence =
            collect_recovery_receipt_evidence(&receipts_dir, source_path, candidate_path)?;
        let prepared_at_us = mcp_agent_mail_core::timestamps::now_micros();
        let (receipt_id, final_path) = (0_u32..10_000)
            .find_map(|suffix| {
                let receipt_id = format!(
                    "recovery-{prepared_at_us:020}-{:010}-{suffix:04}",
                    std::process::id()
                );
                let final_path = receipts_dir.join(format!("{receipt_id}.receipt.json"));
                (!final_path.exists()).then_some((receipt_id, final_path))
            })
            .ok_or_else(|| {
                recovery_receipt_error(
                    "collision-free filename allocation",
                    &receipts_dir,
                    "exhausted 10,000 suffixes",
                )
            })?;
        let body = RecoveryReceiptBody {
            schema: "mcp-agent-mail-recovery-receipt.v1".to_string(),
            receipt_id,
            promotion_commit_marker: "singleton pending filename means intent only; atomic rename to the receipt's .json pathname occurs only after candidate activation".to_string(),
            prepared_at_us,
            db_path: authority_path.display().to_string(),
            storage_root: storage_root.display().to_string(),
            source_path: source_path.map(|path| path.display().to_string()),
            source_snapshot_failure_sha256: evidence.source_snapshot_failure_sha256,
            candidate_path: candidate_path.display().to_string(),
            source: evidence.source,
            candidate: evidence.candidate,
            delta: evidence.delta,
            previous_receipt_bytes_sha256: evidence.previous_receipt_bytes_sha256,
        };
        let body_bytes = serde_json::to_vec(&body).map_err(|error| {
            recovery_receipt_error("canonical body serialization", &pending_path, error)
        })?;
        let document = RecoveryReceiptDocument {
            body,
            self_sha256: recovery_sha256(&body_bytes),
        };
        let mut bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| recovery_receipt_error("serialization", &pending_path, error))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .map_err(|error| recovery_receipt_error("write", &pending_path, error))?;
        file.sync_all()
            .map_err(|error| recovery_receipt_error("file sync", &pending_path, error))?;
        drop(file);
        sync_recovery_directory(&receipts_dir)
            .map_err(|error| recovery_receipt_error("directory sync", &receipts_dir, error))?;
        // Bind the receipt to the candidate generation itself. Finalization
        // also recomputes the candidate's semantic snapshot before committing
        // `.json`.
        install_candidate_recovery_marker(candidate_path, &document)?;
        Ok(PreparedRecoveryReceipt {
            pending_path: pending_path.clone(),
            final_path,
            receipt_bytes_sha256: recovery_sha256(&bytes),
        })
    })();

    match prepare_after_admission {
        Ok(prepared) => Ok(prepared),
        Err(error) => match abort_pending_recovery_receipt_path(&pending_path) {
            Ok(aborted_path) => {
                tracing::warn!(
                    pending = %pending_path.display(),
                    aborted = %aborted_path.display(),
                    error = %error,
                    "recovery receipt preparation failed after singleton admission; durably aborted the intent"
                );
                Err(error)
            }
            Err(abort_error) => Err(SqlError::Custom(format!(
                "{error}; additionally failed to durably abort singleton recovery intent {}: {abort_error}; readiness remains fail-closed",
                pending_path.display()
            ))),
        },
    }
}

/// Durably mark a prepared, uncommitted promotion as aborted.
///
/// Call this only after proving the old database generation is again the live
/// authority. If this operation fails, the `.pending` intent deliberately
/// remains and readiness stays fail-closed.
pub(crate) fn abort_recovery_receipt(
    prepared: &PreparedRecoveryReceipt,
) -> Result<PathBuf, SqlError> {
    abort_pending_recovery_receipt_path(&prepared.pending_path)
}

/// Commit a prepared receipt after the reconstructed/backup candidate has been
/// activated. The exact pending bytes are verified before the atomic rename.
pub(crate) fn finalize_recovery_receipt(
    prepared: &PreparedRecoveryReceipt,
) -> Result<(), RecoveryReceiptFinalizeError> {
    finalize_recovery_receipt_with_post_rename(prepared, |receipts_dir, document| {
        sync_recovery_directory(receipts_dir)
            .map_err(|error| recovery_receipt_error("directory sync", receipts_dir, error))?;
        let chain = verify_finalized_recovery_receipt_chain(receipts_dir)?.ok_or_else(|| {
            recovery_receipt_error(
                "final chain-tip check",
                receipts_dir,
                "finalized receipt chain is unexpectedly empty",
            )
        })?;
        if chain.tip_bytes_sha256 != prepared.receipt_bytes_sha256
            || chain.latest.body.receipt_id != document.body.receipt_id
        {
            return Err(recovery_receipt_error(
                "final chain-tip check",
                &prepared.final_path,
                format!(
                    "chain tip {} / {} does not match finalized receipt {} / {}",
                    chain.tip_bytes_sha256,
                    chain.latest.body.receipt_id,
                    prepared.receipt_bytes_sha256,
                    document.body.receipt_id
                ),
            ));
        }
        verify_live_recovery_marker(Path::new(&document.body.db_path), document)
    })
}

fn recovery_finalize_error(
    error: SqlError,
    promotion_marker_committed: bool,
) -> RecoveryReceiptFinalizeError {
    RecoveryReceiptFinalizeError {
        error,
        promotion_marker_committed,
    }
}

fn finalize_recovery_receipt_with_post_rename<F>(
    prepared: &PreparedRecoveryReceipt,
    post_rename: F,
) -> Result<(), RecoveryReceiptFinalizeError>
where
    F: FnOnce(&Path, &RecoveryReceiptDocument) -> Result<(), SqlError>,
{
    let bytes = std::fs::read(&prepared.pending_path).map_err(|error| {
        recovery_finalize_error(
            recovery_receipt_error("pending read", &prepared.pending_path, error),
            false,
        )
    })?;
    let actual_hash = recovery_sha256(&bytes);
    if actual_hash != prepared.receipt_bytes_sha256 {
        return Err(recovery_finalize_error(
            recovery_receipt_error(
                "pending byte-hash check",
                &prepared.pending_path,
                format!(
                    "stored intent hash {}, observed {actual_hash}",
                    prepared.receipt_bytes_sha256
                ),
            ),
            false,
        ));
    }
    let document = verify_recovery_receipt_document(&prepared.pending_path, &bytes)
        .map_err(|error| recovery_finalize_error(error, false))?;
    if document.body.receipt_id.is_empty() {
        return Err(recovery_finalize_error(
            recovery_receipt_error(
                "pending receipt id check",
                &prepared.pending_path,
                "empty receipt id",
            ),
            false,
        ));
    }
    let receipts_dir = prepared.final_path.parent().ok_or_else(|| {
        recovery_finalize_error(
            recovery_receipt_error(
                "parent resolution",
                &prepared.final_path,
                "final receipt has no parent directory",
            ),
            false,
        )
    })?;
    let current_chain = verify_finalized_recovery_receipt_chain(receipts_dir)
        .map_err(|error| recovery_finalize_error(error, false))?;
    let current_tip = current_chain
        .as_ref()
        .map(|chain| chain.tip_bytes_sha256.as_str());
    if current_tip != document.body.previous_receipt_bytes_sha256.as_deref() {
        return Err(recovery_finalize_error(
            recovery_receipt_error(
                "pre-rename chain-tip check",
                &prepared.pending_path,
                format!(
                    "pending predecessor {:?} does not match current chain tip {:?}",
                    document.body.previous_receipt_bytes_sha256, current_tip
                ),
            ),
            false,
        ));
    }
    // The candidate has already been activated by the caller. Prove its
    // in-database marker matches these exact receipt bytes before committing
    // the filename transition.
    verify_live_recovery_marker(Path::new(&document.body.db_path), &document)
        .map_err(|error| recovery_finalize_error(error, false))?;
    verify_live_recovery_candidate_snapshot(Path::new(&document.body.db_path), &document)
        .map_err(|error| recovery_finalize_error(error, false))?;
    if prepared.final_path.exists() {
        return Err(recovery_finalize_error(
            recovery_receipt_error(
                "final collision check",
                &prepared.final_path,
                "destination already exists",
            ),
            false,
        ));
    }
    std::fs::rename(&prepared.pending_path, &prepared.final_path).map_err(|error| {
        recovery_finalize_error(
            recovery_receipt_error("promotion-marker rename", &prepared.final_path, error),
            false,
        )
    })?;
    // The atomic rename is the point of no return. Any subsequent error must
    // leave the activated candidate in place; callers use the typed flag to
    // avoid rolling it back beneath a finalized receipt.
    post_rename(receipts_dir, &document).map_err(|error| recovery_finalize_error(error, true))
}

#[cfg(test)]
pub(crate) fn finalize_recovery_receipt_with_injected_post_rename_failure(
    prepared: &PreparedRecoveryReceipt,
) -> Result<(), RecoveryReceiptFinalizeError> {
    finalize_recovery_receipt_with_post_rename(prepared, |_receipts_dir, _document| {
        Err(SqlError::Custom(
            "injected post-rename receipt durability failure".to_string(),
        ))
    })
}

#[cfg(test)]
pub(crate) fn finalize_recovery_receipt_with_injected_pre_rename_failure(
    prepared: &PreparedRecoveryReceipt,
) -> Result<(), RecoveryReceiptFinalizeError> {
    Err(recovery_finalize_error(
        recovery_receipt_error(
            "injected pre-rename failure",
            &prepared.pending_path,
            "injected before promotion marker commit",
        ),
        false,
    ))
}

fn write_json_report<T: Serialize>(report_path: &Path, payload: &T) -> Result<(), SqlError> {
    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            SqlError::Custom(format!(
                "failed to create forensic report directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let report = serde_json::to_vec_pretty(payload).map_err(|error| {
        SqlError::Custom(format!("failed to serialize forensic report: {error}"))
    })?;
    std::fs::write(report_path, report).map_err(|error| {
        SqlError::Custom(format!(
            "failed to write forensic report {}: {error}",
            report_path.display()
        ))
    })?;
    Ok(())
}

fn file_inventory(
    bundle_dir: &Path,
    path: &Path,
    kind: &str,
    role: &str,
    schema: Option<&str>,
    contains_raw_mailbox_data: bool,
) -> Result<serde_json::Value, SqlError> {
    Ok(json!({
        "path": bundle_rel_path(bundle_dir, path)?,
        "sha256": bundle_sha256(path)?,
        "bytes": path.metadata().map_err(|error| {
            SqlError::Custom(format!(
                "failed to inspect forensic artifact {}: {error}",
                path.display()
            ))
        })?.len(),
        "kind": kind,
        "role": role,
        "schema": schema,
        "contains_raw_mailbox_data": contains_raw_mailbox_data,
    }))
}

fn add_report_artifact<T: Serialize>(
    bundle_dir: &Path,
    files: &mut Vec<serde_json::Value>,
    path: &Path,
    kind: &str,
    role: &str,
    schema: &str,
    payload: &T,
) -> Result<serde_json::Value, SqlError> {
    write_json_report(path, payload)?;
    files.push(file_inventory(
        bundle_dir,
        path,
        kind,
        role,
        Some(schema),
        false,
    )?);
    Ok(json!({
        "path": bundle_rel_path(bundle_dir, path)?,
        "schema": schema,
    }))
}

fn source_file_status(path: &Path) -> serde_json::Value {
    if is_in_memory_db_path(path) {
        return json!({
            "path": path.display().to_string(),
            "exists": false,
            "bytes": serde_json::Value::Null,
            "status": "in_memory",
        });
    }

    match std::fs::metadata(path) {
        Ok(metadata) => json!({
            "path": path.display().to_string(),
            "exists": metadata.is_file(),
            "bytes": metadata.is_file().then_some(metadata.len()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({
            "path": path.display().to_string(),
            "exists": false,
            "bytes": serde_json::Value::Null,
        }),
        Err(error) => json!({
            "path": path.display().to_string(),
            "exists": false,
            "bytes": serde_json::Value::Null,
            "error": error.to_string(),
        }),
    }
}

fn not_applicable_source_status(detail: &str) -> serde_json::Value {
    json!({
        "path": serde_json::Value::Null,
        "exists": false,
        "bytes": serde_json::Value::Null,
        "status": "not_applicable",
        "detail": detail,
    })
}

fn inventory_identity_labels(
    identities: &BTreeSet<crate::reconstruct::MailboxProjectIdentity>,
) -> Vec<String> {
    identities
        .iter()
        .map(crate::reconstruct::MailboxProjectIdentity::display_label)
        .collect()
}

fn build_archive_drift_reference(capture: MailboxForensicCapture<'_>) -> serde_json::Value {
    let archive = scan_archive_message_inventory(capture.storage_root);
    let projects_dir = capture.storage_root.join("projects");

    let (db_inventory_json, missing_archive_projects, drift_reasons) = if capture
        .db_path
        .as_os_str()
        == ":memory:"
    {
        (
            json!({
                "status": "skipped",
                "detail": "DB inventory comparison skipped for in-memory database",
            }),
            Vec::new(),
            vec!["database_inventory_skipped_in_memory".to_string()],
        )
    } else {
        match inspect_mailbox_db_inventory(capture.db_path) {
            Ok(inventory) => {
                let labels =
                    archive_missing_project_identities(&archive, &inventory.project_identities);
                let mut reasons = Vec::new();
                let archive_message_count = archive.unique_message_ids;
                let db_message_count = inventory.messages;
                let archive_messages_ahead = archive_message_count > db_message_count;
                let archive_latest_id_ahead =
                    archive.latest_message_id.unwrap_or(0) > inventory.max_message_id;
                let archive_metadata_ahead = archive_metadata_advantage_is_decisive(
                    archive.projects,
                    archive.agents,
                    archive_message_count,
                    archive.latest_message_id,
                    inventory.projects,
                    inventory.agents,
                    db_message_count,
                    inventory.max_message_id,
                    &labels,
                );

                if archive_messages_ahead {
                    reasons.push("archive_messages_ahead".to_string());
                }
                if archive_latest_id_ahead {
                    reasons.push("archive_latest_id_ahead".to_string());
                }
                if archive_metadata_ahead {
                    if archive.projects > inventory.projects {
                        reasons.push("archive_projects_ahead".to_string());
                    }
                    if archive.agents > inventory.agents {
                        reasons.push("archive_agents_ahead".to_string());
                    }
                    if !labels.is_empty() {
                        reasons.push("archive_project_identity_ahead".to_string());
                    }
                }
                (
                    json!({
                        "status": "ok",
                        "projects": inventory.projects,
                        "agents": inventory.agents,
                        "messages": inventory.messages,
                        "max_message_id": inventory.max_message_id,
                        "project_identities": inventory_identity_labels(&inventory.project_identities),
                    }),
                    labels,
                    reasons,
                )
            }
            Err(error) => (
                json!({
                    "status": "error",
                    "detail": error.to_string(),
                }),
                Vec::new(),
                vec!["database_inventory_unavailable".to_string()],
            ),
        }
    };

    json!({
        "schema": { "name": "mcp-agent-mail-mailbox-forensics-archive-drift", "major": 1, "minor": 0 },
        "command": capture.command_name,
        "trigger": capture.trigger,
        "archive": archive_inventory_json(capture.storage_root, &projects_dir, &archive),
        "database_inventory": db_inventory_json,
        "archive_ahead": !drift_reasons.is_empty()
            && !drift_reasons.iter().all(|reason| {
                reason == "database_inventory_unavailable"
                    || reason == "database_inventory_skipped_in_memory"
            }),
        "archive_drift_reasons": drift_reasons,
        "missing_archive_projects": missing_archive_projects,
        "candidate_validation": {
            "planned_checks": [
                "sqlite_file_is_healthy",
                "candidate_quarantine_on_failure",
                "activate_only_after_validation",
            ],
            "promotion_guard": "Recovery may only promote a reconstructed candidate after validation succeeds and the live path is safe to replace.",
        },
    })
}

fn archive_inventory_json(
    storage_root: &Path,
    projects_dir: &Path,
    archive: &ArchiveMessageInventory,
) -> serde_json::Value {
    json!({
        "storage_root": storage_root.display().to_string(),
        "storage_root_exists": storage_root.exists(),
        "storage_root_is_directory": storage_root.is_dir(),
        "projects_dir_exists": projects_dir.is_dir(),
        "projects": archive.projects,
        "agents": archive.agents,
        "canonical_message_files": archive.canonical_message_files,
        "unique_message_ids": archive.unique_message_ids,
        "duplicate_canonical_message_files": archive.duplicate_canonical_message_files,
        "duplicate_canonical_message_ids": archive.duplicate_canonical_message_ids,
        "latest_message_id": archive.latest_message_id,
        "parse_errors": archive.parse_errors,
        "project_identities": inventory_identity_labels(&archive.project_identities),
    })
}

fn build_environment_reference(capture: MailboxForensicCapture<'_>) -> serde_json::Value {
    let current_dir = std::env::current_dir()
        .map(|path| path.display().to_string())
        .ok();
    json!({
        "schema": { "name": "mcp-agent-mail-mailbox-forensics-environment", "major": 1, "minor": 0 },
        "command": capture.command_name,
        "trigger": capture.trigger,
        "process_id": std::process::id(),
        "current_dir": current_dir,
        "database_url": redact_database_url(capture.database_url),
        "db_path": capture.db_path.display().to_string(),
        "storage_root": capture.storage_root.display().to_string(),
        "storage_root_exists": capture.storage_root.exists(),
        "storage_root_is_directory": capture.storage_root.is_dir(),
        "integrity_detail_present": capture.integrity_detail.is_some(),
    })
}

fn build_live_db_reference(capture: MailboxForensicCapture<'_>) -> serde_json::Value {
    if is_in_memory_db_path(capture.db_path) {
        let sidecars = inspect_mailbox_sidecar_state(capture.db_path);
        let recovery_lock = inspect_mailbox_recovery_lock(capture.db_path);
        return json!({
            "schema": { "name": "mcp-agent-mail-mailbox-forensics-live-db-state", "major": 1, "minor": 0 },
            "command": capture.command_name,
            "trigger": capture.trigger,
            "db_family": forensic_db_family_name(capture.db_path),
            "db": source_file_status(capture.db_path),
            "journal": not_applicable_source_status("In-memory database has no rollback-journal sidecar file"),
            "wal": not_applicable_source_status("In-memory database has no WAL sidecar file"),
            "shm": not_applicable_source_status("In-memory database has no SHM sidecar file"),
            "sidecars": sidecars,
            "recovery_lock": recovery_lock,
            "process_inventory": {
                "platform": std::env::consts::OS,
                "holders": Vec::<ForensicProcessHolder>::new(),
            },
            "file_locks": {
                "platform": std::env::consts::OS,
                "locks": Vec::<ForensicFileLock>::new(),
            },
        });
    }

    let journal_path = sqlite_path_with_suffix(capture.db_path, "-journal");
    let wal_path = sqlite_path_with_suffix(capture.db_path, "-wal");
    let shm_path = sqlite_path_with_suffix(capture.db_path, "-shm");
    let sidecars = inspect_mailbox_sidecar_state(capture.db_path);
    let recovery_lock = inspect_mailbox_recovery_lock(capture.db_path);
    let holders = process_holders_for_paths(&[
        ("db", capture.db_path.to_path_buf()),
        ("journal", journal_path.clone()),
        ("wal", wal_path.clone()),
        ("shm", shm_path.clone()),
    ]);
    let locks = file_locks_for_paths(&[
        ("db", capture.db_path.to_path_buf()),
        ("journal", journal_path.clone()),
        ("wal", wal_path.clone()),
        ("shm", shm_path.clone()),
    ]);

    json!({
        "schema": { "name": "mcp-agent-mail-mailbox-forensics-live-db-state", "major": 1, "minor": 0 },
        "command": capture.command_name,
        "trigger": capture.trigger,
        "db_family": forensic_db_family_name(capture.db_path),
        "db": source_file_status(capture.db_path),
        "journal": source_file_status(&journal_path),
        "wal": source_file_status(&wal_path),
        "shm": source_file_status(&shm_path),
        "sidecars": sidecars,
        "recovery_lock": recovery_lock,
        "process_inventory": {
            "platform": std::env::consts::OS,
            "holders": holders,
        },
        "file_locks": {
            "platform": std::env::consts::OS,
            "locks": locks,
        },
    })
}

#[cfg(target_os = "linux")]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    let dev = metadata.dev();
    let (major, minor) = linux_device_numbers(dev);
    Some(FileIdentity {
        dev,
        ino: metadata.ino(),
        major,
        minor,
    })
}

#[cfg(target_os = "linux")]
fn process_holders_for_paths(paths: &[(&str, PathBuf)]) -> Vec<ForensicProcessHolder> {
    let mut identities = Vec::new();
    for (role, path) in paths {
        if let Some(identity) = file_identity(path) {
            identities.push(((*role).to_string(), identity));
        }
    }
    if identities.is_empty() {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut holders: HashMap<u32, BTreeSet<String>> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid_text) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = pid_text.parse::<u32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Ok(target_meta) = std::fs::metadata(&target) else {
                continue;
            };
            let (major, minor) = linux_device_numbers(target_meta.dev());
            let target_identity = FileIdentity {
                dev: target_meta.dev(),
                ino: target_meta.ino(),
                major,
                minor,
            };
            for (role, identity) in &identities {
                if target_identity.dev == identity.dev && target_identity.ino == identity.ino {
                    holders.entry(pid).or_default().insert(role.clone());
                }
            }
        }
    }

    let mut results = holders
        .into_iter()
        .map(|(pid, roles)| {
            let exe_path = pid_executable_path(pid).map(|path| path.to_string_lossy().into_owned());
            let exe_deleted = exe_path
                .as_deref()
                .is_some_and(|path| path.ends_with(" (deleted)"));
            ForensicProcessHolder {
                pid,
                roles: roles.into_iter().collect(),
                cmdline: pid_command_line(pid),
                exe_path,
                exe_deleted,
            }
        })
        .collect::<Vec<_>>();
    results.sort_by_key(|holder| holder.pid);
    results
}

#[cfg(not(target_os = "linux"))]
fn process_holders_for_paths(_paths: &[(&str, PathBuf)]) -> Vec<ForensicProcessHolder> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn file_locks_for_paths(paths: &[(&str, PathBuf)]) -> Vec<ForensicFileLock> {
    let identities = paths
        .iter()
        .filter_map(|(role, path)| {
            file_identity(path).map(|identity| ((*role).to_string(), identity))
        })
        .collect::<Vec<_>>();
    if identities.is_empty() {
        return Vec::new();
    }
    let Ok(locks_content) = std::fs::read_to_string("/proc/locks") else {
        return Vec::new();
    };
    let mut locks = Vec::new();
    for line in locks_content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        // Blocking lock lines have "->" at fields[1], shifting all
        // subsequent field indices by 1:
        //   Normal:   "1: POSIX  ADVISORY  WRITE 12345 fd:01:123456 0 EOF"
        //   Blocking: "2: -> FLOCK  ADVISORY  WRITE 12345 fd:01:123456 0 EOF"
        let offset: usize = if fields.get(1) == Some(&"->") { 1 } else { 0 };
        if fields.len() < 8 + offset {
            continue;
        }
        let parts: Vec<&str> = fields[5 + offset].split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(major) = u32::from_str_radix(parts[0], 16) else {
            continue;
        };
        let Ok(minor) = u32::from_str_radix(parts[1], 16) else {
            continue;
        };
        let Ok(ino) = parts[2].parse::<u64>() else {
            continue;
        };
        let Ok(pid) = fields[4 + offset].parse::<u32>() else {
            continue;
        };
        for (role, identity) in &identities {
            if identity.major == major && identity.minor == minor && identity.ino == ino {
                locks.push(ForensicFileLock {
                    role: role.clone(),
                    pid,
                    lock_type: fields[1 + offset].to_string(),
                    access: fields[3 + offset].to_string(),
                    range_start: fields[6 + offset].to_string(),
                    range_end: fields[7 + offset].to_string(),
                });
            }
        }
    }
    locks.sort_by(|left, right| {
        left.pid
            .cmp(&right.pid)
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.lock_type.cmp(&right.lock_type))
    });
    locks
}

#[cfg(not(target_os = "linux"))]
fn file_locks_for_paths(_paths: &[(&str, PathBuf)]) -> Vec<ForensicFileLock> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn pid_command_line(pid: u32) -> Option<String> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let segments = cmdline
        .split(|byte| *byte == 0)
        .filter(|segment| !segment.is_empty())
        .map(|segment| String::from_utf8_lossy(segment).into_owned())
        .collect::<Vec<_>>();
    (!segments.is_empty()).then(|| segments.join(" "))
}

#[cfg(any(test, not(target_os = "linux")))]
fn parse_ps_output_value(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(not(target_os = "linux"))]
fn ps_output_value(pid: u32, column: &str) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", column])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ps_output_value(&output.stdout)
}

#[cfg(not(target_os = "linux"))]
fn pid_command_line(pid: u32) -> Option<String> {
    ps_output_value(pid, "command=")
}

#[cfg(target_os = "linux")]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    ps_output_value(pid, "comm=").map(PathBuf::from)
}

/// Capture a mailbox forensic bundle and return the bundle directory.
#[allow(clippy::result_large_err)]
pub fn capture_mailbox_forensic_bundle(
    capture: MailboxForensicCapture<'_>,
) -> Result<PathBuf, SqlError> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let db_family = forensic_db_family_name(capture.db_path);
    let bundle_dir_family = forensic_bundle_dir_component(capture.db_path);
    let bundle_name = format!("{}-{timestamp}", capture.command_name);
    let bundle_dir = forensics_root(capture.storage_root, capture.db_path)
        .join(&bundle_dir_family)
        .join(&bundle_name);
    std::fs::create_dir_all(&bundle_dir).map_err(|error| {
        SqlError::Custom(format!(
            "failed to create mailbox forensic bundle {}: {error}",
            bundle_dir.display()
        ))
    })?;
    let sqlite_dir = bundle_dir.join("sqlite");
    std::fs::create_dir_all(&sqlite_dir).map_err(|error| {
        SqlError::Custom(format!(
            "failed to create mailbox forensic sqlite directory {}: {error}",
            sqlite_dir.display()
        ))
    })?;

    let created_at = chrono::Utc::now().to_rfc3339();
    let in_memory_db = is_in_memory_db_path(capture.db_path);
    let source_paths = [
        ("db", capture.db_path.to_path_buf()),
        (
            "journal",
            sqlite_path_with_suffix(capture.db_path, "-journal"),
        ),
        ("wal", sqlite_path_with_suffix(capture.db_path, "-wal")),
        ("shm", sqlite_path_with_suffix(capture.db_path, "-shm")),
    ];

    let mut artifacts = Vec::new();
    let mut sqlite_manifest = serde_json::Map::new();
    let mut copied_paths = Vec::new();
    let mut files = Vec::new();

    for (kind, source_path) in source_paths {
        if in_memory_db {
            let detail = match kind {
                "db" => "In-memory database has no SQLite file artifact",
                "journal" => "In-memory database has no rollback-journal sidecar artifact",
                "wal" => "In-memory database has no WAL sidecar artifact",
                "shm" => "In-memory database has no SHM sidecar artifact",
                _ => "In-memory database has no file artifact",
            };
            artifacts.push(json!({
                "kind": kind,
                "source_path": serde_json::Value::Null,
                "captured_path": serde_json::Value::Null,
                "size_bytes": serde_json::Value::Null,
                "status": "not_applicable",
                "error": serde_json::Value::Null,
                "detail": detail,
            }));
            sqlite_manifest.insert(
                kind.to_string(),
                json!({
                    "path": serde_json::Value::Null,
                    "status": "not_applicable",
                    "required": false,
                    "contains_raw_mailbox_data": false,
                    "detail": detail,
                }),
            );
            continue;
        }

        let destination = sqlite_dir.join(
            source_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(kind),
        );
        let captured_rel_path = bundle_rel_path(&bundle_dir, &destination)?;
        if !source_path.exists() {
            let required = kind == "db";
            artifacts.push(json!({
                "kind": kind,
                "source_path": source_path.display().to_string(),
                "captured_path": captured_rel_path,
                "size_bytes": serde_json::Value::Null,
                "status": if required { "missing_required" } else { "missing" },
                "error": serde_json::Value::Null,
            }));
            sqlite_manifest.insert(
                kind.to_string(),
                json!({
                    "path": captured_rel_path,
                    "status": if required { "missing_required" } else { "missing" },
                    "required": required,
                    "contains_raw_mailbox_data": true,
                }),
            );
            continue;
        }

        let copy_result = std::fs::copy(&source_path, &destination);
        let copied_ok = copy_result.is_ok();
        let size_bytes = destination
            .metadata()
            .ok()
            .map(|metadata| metadata.len())
            .or_else(|| source_path.metadata().ok().map(|metadata| metadata.len()));
        let sha256 = if copied_ok {
            Some(bundle_sha256(&destination)?)
        } else {
            None
        };
        if copied_ok {
            copied_paths.push(captured_rel_path.clone());
            files.push(file_inventory(
                &bundle_dir,
                &destination,
                "sqlite",
                kind,
                None,
                true,
            )?);
        }

        artifacts.push(json!({
            "kind": kind,
            "source_path": source_path.display().to_string(),
            "captured_path": captured_rel_path.clone(),
            "size_bytes": size_bytes,
            "sha256": sha256,
            "status": if copied_ok { "captured" } else { "error" },
            "error": copy_result.err().map(|error| error.to_string()),
        }));
        sqlite_manifest.insert(
            kind.to_string(),
            json!({
                "path": captured_rel_path,
                "status": if copied_ok { "captured" } else { "error" },
                "required": kind == "db",
                "bytes": size_bytes,
                "sha256": sha256,
                "contains_raw_mailbox_data": true,
            }),
        );
    }

    let references_dir = bundle_dir.join("references");
    let live_db_state = build_live_db_reference(capture);
    let archive_drift = build_archive_drift_reference(capture);
    let archive_drift_report = compute_archive_drift_report(capture.storage_root, capture.db_path);
    let environment = build_environment_reference(capture);

    let mut reference_artifacts = serde_json::Map::new();
    reference_artifacts.insert(
        "live_db_state".to_string(),
        add_report_artifact(
            &bundle_dir,
            &mut files,
            &references_dir.join("live-db-state.json"),
            "report",
            "live_db_state",
            "mailbox-forensics-live-db-state.v1",
            &live_db_state,
        )?,
    );
    reference_artifacts.insert(
        "archive_drift".to_string(),
        add_report_artifact(
            &bundle_dir,
            &mut files,
            &references_dir.join("archive-drift.json"),
            "report",
            "archive_drift",
            "mailbox-forensics-archive-drift.v1",
            &archive_drift,
        )?,
    );
    match archive_drift_report {
        Ok(drift_report) => {
            reference_artifacts.insert(
                "archive_drift_report".to_string(),
                add_report_artifact(
                    &bundle_dir,
                    &mut files,
                    &references_dir.join("archive-drift-report.json"),
                    "report",
                    "archive_drift_report",
                    "mcp-agent-mail-archive-drift-report.v1",
                    &drift_report,
                )?,
            );
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to compute archive drift report for forensic bundle"
            );
        }
    }

    reference_artifacts.insert(
        "environment".to_string(),
        add_report_artifact(
            &bundle_dir,
            &mut files,
            &references_dir.join("environment.json"),
            "report",
            "environment",
            "mailbox-forensics-environment.v1",
            &environment,
        )?,
    );

    let summary_path = bundle_dir.join("summary.json");
    let summary = json!({
        "schema": { "name": "mcp-agent-mail-doctor-forensics-summary", "major": 1, "minor": 1 },
        "command": capture.command_name,
        "trigger": capture.trigger,
        "bundle_name": bundle_name,
        "timestamp": timestamp,
        "created_at": created_at,
        "database_url": redact_database_url(capture.database_url),
        "db_path": capture.db_path.display().to_string(),
        "storage_root": capture.storage_root.display().to_string(),
        "integrity_detail": capture.integrity_detail,
        "archive_scan": archive_inventory_json(
            capture.storage_root,
            &capture.storage_root.join("projects"),
            &scan_archive_message_inventory(capture.storage_root),
        ),
        "artifacts": artifacts,
        "references": {
            "live_db_state": "references/live-db-state.json",
            "archive_drift": "references/archive-drift.json",
            "archive_drift_report": "references/archive-drift-report.json",
            "environment": "references/environment.json",
        },
    });
    write_json_report(&summary_path, &summary)?;
    files.push(file_inventory(
        &bundle_dir,
        &summary_path,
        "report",
        "summary",
        Some("doctor-forensics-summary.v1"),
        false,
    )?);

    copied_paths.sort();
    files.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["path"].as_str().unwrap_or_default())
    });

    let mut referenced_evidence = BTreeSet::from([
        "archive_drift".to_string(),
        "archive_drift_report".to_string(),
        "environment_summary".to_string(),
        "live_db_state".to_string(),
    ]);
    if capture.integrity_detail.is_some() {
        referenced_evidence.insert("integrity_detail".to_string());
    }

    let manifest_path = bundle_dir.join("manifest.json");
    let manifest = json!({
        "schema": { "name": "mcp-agent-mail-doctor-forensics", "major": 1, "minor": 1 },
        "bundle_kind": "mailbox-doctor-forensics",
        "bundle_name": bundle_name,
        "command": capture.command_name,
        "trigger": capture.trigger,
        "timestamp": timestamp,
        "generated_at": created_at,
        "source": {
            "database_url": redact_database_url(capture.database_url),
            "db_path": capture.db_path.display().to_string(),
            "db_family": db_family,
            "storage_root": capture.storage_root.display().to_string(),
        },
        "layout": {
            "sqlite_dir": "sqlite",
            "summary_path": "summary.json",
            "manifest_path": "manifest.json",
            "copied_before_mutation": copied_paths,
            "referenced_evidence": referenced_evidence.into_iter().collect::<Vec<_>>(),
            "reserved_paths": ["references/", "receipts/"],
        },
        "retention": {
            "policy": "manual_review",
            "review_after_days": 14,
            "delete_after_days": serde_json::Value::Null,
            "automatic_deletion": false,
            "deletion_requires_explicit_operator_action": true,
            "note": "No automatic forensic bundle deletion is allowed until storage-budget guardrails land.",
        },
        "redaction": {
            "database_url": "credentials_redacted",
            "sqlite_family": "raw_local_only",
            "manifest_and_summary": "shareable_after_human_review",
            "raw_sqlite_export": "requires_explicit_redaction_or_encrypted_export",
        },
        "artifacts": {
            "summary": { "path": "summary.json", "schema": "doctor-forensics-summary.v1" },
            "sqlite": serde_json::Value::Object(sqlite_manifest),
            "references": serde_json::Value::Object(reference_artifacts),
        },
        "files": files,
    });
    write_json_report(&manifest_path, &manifest)?;

    Ok(bundle_dir)
}

#[cfg(test)]
mod tests {
    use super::{
        MailboxForensicCapture, build_archive_drift_reference, build_live_db_reference,
        capture_mailbox_forensic_bundle, capture_pre_recovery_snapshot,
        collect_recovery_continuity_sets, finalize_recovery_receipt,
        finalize_recovery_receipt_with_injected_post_rename_failure,
        finalized_recovery_receipt_paths, parse_ps_output_value, pending_recovery_receipt_paths,
        prepare_recovery_receipt, read_sqlite_header_fields, redact_database_url,
        verify_recovery_receipt_state,
    };
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    fn seed_recovery_receipt_db(path: &std::path::Path, include_coordination: bool) {
        let conn = crate::CanonicalDbConn::open_file(path.to_string_lossy().as_ref())
            .expect("open receipt fixture database");
        for statement in [
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT NOT NULL)",
            "CREATE TABLE products (id INTEGER PRIMARY KEY, product_uid TEXT NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL)",
            "CREATE TABLE product_project_links (id INTEGER PRIMARY KEY, product_id INTEGER NOT NULL, project_id INTEGER NOT NULL, created_at INTEGER NOT NULL)",
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL, reaper_exempt INTEGER NOT NULL, registration_token TEXT)",
            "CREATE TABLE agent_links (id INTEGER PRIMARY KEY, a_project_id INTEGER NOT NULL, a_agent_id INTEGER NOT NULL, b_project_id INTEGER NOT NULL, b_agent_id INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT NOT NULL, created_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL, expires_ts INTEGER)",
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL, ack_required INTEGER NOT NULL, created_ts INTEGER NOT NULL, recipients_json TEXT NOT NULL, attachments TEXT NOT NULL)",
            "CREATE TABLE message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL, read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY (message_id, agent_id))",
            "CREATE TABLE file_reservations (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL, reason TEXT NOT NULL, created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL, released_ts INTEGER)",
            "CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL)",
            "CREATE TABLE proof_gate_consumed_nonces (issuer_key TEXT NOT NULL, nonce TEXT NOT NULL, retain_until INTEGER NOT NULL, consumed_at INTEGER NOT NULL, PRIMARY KEY (issuer_key, nonce))",
            "INSERT INTO projects (id, slug, human_key) VALUES (17, 'alpha', '/srv/alpha')",
            "INSERT INTO agents (id, project_id, name, reaper_exempt, registration_token) VALUES (41, 17, 'BlueFox', 1, 'fixture-registration-secret')",
            "INSERT INTO agents (id, project_id, name, reaper_exempt, registration_token) VALUES (42, 17, 'RedLake', 0, NULL)",
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) VALUES (73, 17, 41, 'receipt-thread', 'Recovery proof', 'Preserve this body', 'high', 1, 345678, '{\"to\":[\"RedLake\"]}', '[{\"name\":\"proof.txt\"}]')",
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (73, 42, 'to', 456789, NULL)",
        ] {
            conn.execute_raw(statement)
                .expect("seed receipt fixture schema");
        }
        if include_coordination {
            conn.execute_raw(
                "INSERT INTO products (id, product_uid, name, created_at) VALUES (5, 'product-alpha', 'Alpha Product', 50)",
            )
            .expect("seed receipt fixture product");
            conn.execute_raw(
                "INSERT INTO product_project_links (id, product_id, project_id, created_at) VALUES (6, 5, 17, 60)",
            )
            .expect("seed receipt fixture product-project link");
            conn.execute_raw(
                "INSERT INTO agent_links (id, a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts, expires_ts) VALUES (3, 17, 41, 17, 42, 'approved', 'same-team', 100, 200, 900000)",
            )
            .expect("seed receipt fixture contact");
            conn.execute_raw(
                "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) VALUES (9, 17, 41, 'src/**', 1, 'editing', 123456, 999999, NULL)",
            )
            .expect("seed receipt fixture reservation");
            conn.execute_raw(
                "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (9, 777777)",
            )
            .expect("seed receipt fixture release marker");
            conn.execute_raw(
                "INSERT INTO proof_gate_consumed_nonces (issuer_key, nonce, retain_until, consumed_at) VALUES ('issuer-alpha', 'nonce-alpha', 1230000, 456000)",
            )
            .expect("seed receipt fixture proof-gate nonce");
        }
        drop(conn);
    }

    #[test]
    fn recovery_receipt_is_pending_until_atomic_finalize_and_chains_exact_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        let alternate_storage_root = dir.path().join("alternate-mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);

        let candidate_sets = collect_recovery_continuity_sets(&candidate)
            .expect("collect candidate continuity evidence");
        let candidate_agent_keys = candidate_sets.agents.iter().cloned().collect::<String>();
        assert!(
            !candidate_agent_keys.contains("fixture-registration-secret"),
            "stable agent keys must never retain registration-token plaintext"
        );
        assert!(
            candidate_agent_keys.contains(&super::recovery_sha256(b"fixture-registration-secret")),
            "stable agent keys must bind the registration token by digest"
        );

        let first = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect("prepare first recovery receipt");
        assert!(first.pending_path.exists());
        assert!(!first.final_path.exists());
        let pending_text =
            std::fs::read_to_string(&first.pending_path).expect("read pending receipt");
        assert!(
            !pending_text.contains("fixture-registration-secret"),
            "registration tokens must never appear in plaintext receipts"
        );
        assert!(
            !pending_text.contains("Preserve this body")
                && !pending_text.contains("Recovery proof"),
            "message contents must be represented only by semantic digests"
        );
        let pending_error = verify_recovery_receipt_state(&alternate_storage_root, &primary)
            .expect_err("pending promotion intent must block readiness");
        assert!(
            pending_error
                .to_string()
                .contains("unfinalized recovery receipt")
        );

        std::fs::rename(&candidate, &primary).expect("activate first candidate");
        finalize_recovery_receipt(&first).expect("finalize first recovery receipt");
        assert!(!first.pending_path.exists());
        assert!(first.final_path.exists());
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("finalized first receipt chain");

        let second_candidate = dir.path().join("candidate-second.sqlite3");
        seed_recovery_receipt_db(&second_candidate, true);
        let second =
            prepare_recovery_receipt(&storage_root, &primary, Some(&primary), &second_candidate)
                .expect("prepare second recovery receipt");
        let prior_primary = dir.path().join("storage.sqlite3.prior-generation");
        std::fs::rename(&primary, &prior_primary).expect("preserve prior generation");
        std::fs::rename(&second_candidate, &primary).expect("activate second candidate");
        finalize_recovery_receipt(&second).expect("finalize second recovery receipt");
        verify_recovery_receipt_state(&storage_root, &primary).expect("two-receipt chain is valid");

        let receipts_dir = first.final_path.parent().expect("receipt parent");
        let paths = finalized_recovery_receipt_paths(receipts_dir).expect("list receipts");
        assert_eq!(paths.len(), 2);
        let first_bytes = std::fs::read(&paths[0]).expect("read first receipt");
        let second_document: super::RecoveryReceiptDocument =
            serde_json::from_slice(&std::fs::read(&paths[1]).expect("read second receipt"))
                .expect("decode second receipt");
        let first_bytes_sha256 = super::recovery_sha256(&first_bytes);
        assert_eq!(
            second_document
                .body
                .previous_receipt_bytes_sha256
                .as_deref(),
            Some(first_bytes_sha256.as_str())
        );
        assert_eq!(second_document.body.delta.projects.lost_count, 0);
        assert_eq!(second_document.body.delta.products.lost_count, 0);
        assert_eq!(
            second_document.body.delta.product_project_links.lost_count,
            0
        );
        assert_eq!(second_document.body.delta.agents.lost_count, 0);
        assert_eq!(second_document.body.delta.contacts.lost_count, 0);
        assert_eq!(second_document.body.delta.reservations.lost_count, 0);
        assert_eq!(second_document.body.delta.messages.lost_count, 0);
        assert_eq!(second_document.body.delta.message_recipients.lost_count, 0);
        assert_eq!(
            second_document
                .body
                .delta
                .proof_gate_consumed_nonces
                .lost_count,
            0
        );

        let finalized_primary = dir.path().join("storage.sqlite3.finalized-generation");
        std::fs::rename(&primary, &finalized_primary).expect("preserve finalized generation");
        seed_recovery_receipt_db(&primary, true);
        let marker_error = verify_recovery_receipt_state(&alternate_storage_root, &primary)
            .expect_err("old/unmarked live database must not satisfy a finalized receipt");
        assert!(marker_error.to_string().contains("live marker"));

        let tampered = String::from_utf8(first_bytes)
            .expect("receipt is UTF-8 JSON")
            .replacen(
                "mcp-agent-mail-recovery-receipt.v1",
                "tampered-receipt.v1",
                1,
            );
        std::fs::write(&paths[0], tampered).expect("tamper first receipt");
        let chain_error = verify_recovery_receipt_state(&storage_root, &primary)
            .expect_err("tampered receipt chain must block readiness");
        assert!(
            chain_error.to_string().contains("schema check")
                || chain_error.to_string().contains("self-hash check")
                || chain_error.to_string().contains("chain-link check")
        );
    }

    #[test]
    fn recovery_receipt_post_rename_failure_is_a_point_of_no_return() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);

        let prepared = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect("prepare receipt");
        std::fs::rename(&candidate, &primary).expect("activate candidate");
        let error = finalize_recovery_receipt_with_injected_post_rename_failure(&prepared)
            .expect_err("post-rename failure must be surfaced");
        assert!(error.promotion_marker_committed());
        assert!(prepared.final_path.exists());
        assert!(!prepared.pending_path.exists());
        assert!(primary.exists(), "activated candidate must remain live");
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("finalized bytes and live marker remain consistent after injected failure");
    }

    #[test]
    fn recovery_receipt_preparation_failure_durably_aborts_singleton_intent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);
        let candidate_conn =
            crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref()).unwrap();
        candidate_conn
            .execute_raw(
                "CREATE TABLE mailbox_recovery_receipt_markers (wrong_column TEXT NOT NULL)",
            )
            .unwrap();
        drop(candidate_conn);

        let error = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect_err("candidate marker failure must abort receipt preparation");
        assert!(
            error.to_string().contains("candidate marker insert"),
            "unexpected error: {error}"
        );

        let receipts_dir = super::recovery_receipts_dir(&storage_root, &primary).unwrap();
        assert!(
            pending_recovery_receipt_paths(&receipts_dir)
                .unwrap()
                .is_empty(),
            "a proven pre-activation failure must not wedge future recovery admission"
        );
        assert!(
            std::fs::read_dir(&receipts_dir)
                .unwrap()
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".receipt.aborted")),
            "the failed singleton intent must be retained as an explicit durable aborted artifact"
        );
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("aborted intent must not block a future recovery");
    }

    #[test]
    fn recovery_receipt_rechecks_candidate_semantics_at_finalize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);

        let prepared = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect("prepare receipt");
        let conn = crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref())
            .expect("open candidate after receipt preparation");
        conn.execute_raw("UPDATE agent_links SET status = 'blocked' WHERE id = 3")
            .expect("mutate prepared candidate");
        drop(conn);
        std::fs::rename(&candidate, &primary).expect("activate drifted candidate");

        let error = finalize_recovery_receipt(&prepared)
            .expect_err("candidate drift after prepare must refuse receipt commit");
        assert!(!error.promotion_marker_committed());
        assert!(error.to_string().contains("continuity check"));
        assert!(prepared.pending_path.exists());
        assert!(!prepared.final_path.exists());
    }

    #[test]
    fn recovery_receipt_chain_order_comes_from_hash_links_not_filenames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first_candidate = dir.path().join("candidate-first.sqlite3");
        let second_candidate = dir.path().join("candidate-second.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&first_candidate, true);
        seed_recovery_receipt_db(&second_candidate, true);

        let first = prepare_recovery_receipt(&storage_root, &primary, None, &first_candidate)
            .expect("prepare first receipt");
        std::fs::rename(&first_candidate, &primary).expect("activate first candidate");
        finalize_recovery_receipt(&first).expect("finalize first receipt");
        let second =
            prepare_recovery_receipt(&storage_root, &primary, Some(&primary), &second_candidate)
                .expect("prepare second receipt");
        let prior = dir.path().join("prior.sqlite3");
        std::fs::rename(&primary, &prior).expect("preserve first generation");
        std::fs::rename(&second_candidate, &primary).expect("activate second candidate");
        finalize_recovery_receipt(&second).expect("finalize second receipt");

        let receipts_dir = first.final_path.parent().expect("receipt parent");
        let paths = finalized_recovery_receipt_paths(receipts_dir).expect("list receipts");
        assert_eq!(paths.len(), 2);
        let mut root = None;
        let mut successor = None;
        for path in paths {
            let document: super::RecoveryReceiptDocument =
                serde_json::from_slice(&std::fs::read(&path).expect("read receipt"))
                    .expect("decode receipt");
            if document.body.previous_receipt_bytes_sha256.is_none() {
                root = Some(path);
            } else {
                successor = Some(path);
            }
        }
        std::fs::rename(
            root.expect("root receipt"),
            receipts_dir.join("zzzz-root.receipt.json"),
        )
        .expect("rename root after successor lexically");
        std::fs::rename(
            successor.expect("successor receipt"),
            receipts_dir.join("aaaa-successor.receipt.json"),
        )
        .expect("rename successor before root lexically");

        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("hash-linked traversal must ignore filename order");
    }

    #[test]
    fn recovery_receipt_singleton_pending_admission_is_cross_thread_atomic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate_a = dir.path().join("candidate-a.sqlite3");
        let candidate_b = dir.path().join("candidate-b.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate_a, true);
        seed_recovery_receipt_db(&candidate_b, true);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let handles = [candidate_a, candidate_b].map(|candidate| {
            let barrier = std::sync::Arc::clone(&barrier);
            let source = source.clone();
            let primary = primary.clone();
            let storage_root = storage_root.clone();
            std::thread::spawn(move || {
                barrier.wait();
                prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate).is_ok()
            })
        });
        let successes = handles
            .into_iter()
            .map(|handle| handle.join().expect("prepare thread"))
            .filter(|success| *success)
            .count();
        assert_eq!(
            successes, 1,
            "exactly one process-equivalent caller may prepare"
        );
    }

    #[test]
    fn recovery_receipt_refuses_lost_contact_and_reservation_before_writing_intent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, false);

        let error = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect_err("coordination loss must fail before promotion intent");
        let error_text = error.to_string();
        assert!(error_text.contains("would lose stable coordination keys"));
        assert!(error_text.contains("products=1"));
        assert!(error_text.contains("product_project_links=1"));
        assert!(error_text.contains("contacts=1"));
        assert!(error_text.contains("reservations=1"));
        assert!(error_text.contains("proof_gate_consumed_nonces=1"));
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("loss rejection must not leave a pending intent");
    }

    #[test]
    fn recovery_receipt_refuses_state_only_contact_and_reservation_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);

        let conn = crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref())
            .expect("open candidate for semantic drift");
        conn.execute_raw(
            "UPDATE agents SET reaper_exempt = 0, registration_token = 'replacement-secret' WHERE id = 41",
        )
        .expect("change agent security state without changing identity");
        conn.execute_raw("UPDATE products SET name = 'Renamed Product' WHERE id = 5")
            .expect("change product state without changing product uid");
        conn.execute_raw("UPDATE product_project_links SET created_at = 61 WHERE id = 6")
            .expect("change product-project link state without changing ownership");
        conn.execute_raw(
            "UPDATE agent_links SET status = 'blocked', reason = 'policy', created_ts = 101, updated_ts = 201, expires_ts = 800000 WHERE id = 3",
        )
        .expect("change contact state without changing endpoints");
        conn.execute_raw(
            "UPDATE file_reservations SET exclusive = 0, reason = 'shared', expires_ts = 888888 WHERE id = 9",
        )
        .expect("change reservation state without changing ownership");
        conn.execute_raw(
            "UPDATE file_reservation_releases SET released_ts = 666666 WHERE reservation_id = 9",
        )
        .expect("change authoritative reservation release state");
        conn.execute_raw(
            "UPDATE proof_gate_consumed_nonces SET retain_until = 1230001, consumed_at = 456001 WHERE issuer_key = 'issuer-alpha' AND nonce = 'nonce-alpha'",
        )
        .expect("change proof-gate replay state without changing nonce identity");
        drop(conn);

        let error = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect_err("semantic coordination drift must fail before promotion intent");
        let error_text = error.to_string();
        assert!(error_text.contains("would lose stable coordination keys"));
        assert!(error_text.contains("products=1"));
        assert!(error_text.contains("product_project_links=1"));
        assert!(error_text.contains("agents=1"));
        assert!(error_text.contains("contacts=1"));
        assert!(error_text.contains("reservations=1"));
        assert!(error_text.contains("proof_gate_consumed_nonces=1"));
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("state-drift rejection must not leave a pending intent");
    }

    #[test]
    fn recovery_receipt_message_fingerprints_are_numeric_id_remap_invariant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);

        let conn = crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref())
            .expect("open candidate for id remap");
        conn.execute_raw("UPDATE messages SET id = 731 WHERE id = 73")
            .expect("remap message id");
        conn.execute_raw("UPDATE message_recipients SET message_id = 731 WHERE message_id = 73")
            .expect("remap recipient parent id");
        drop(conn);

        let prepared = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect("numeric id-only remap must preserve semantic continuity");
        let document: super::RecoveryReceiptDocument = serde_json::from_slice(
            &std::fs::read(&prepared.pending_path).expect("read pending receipt"),
        )
        .expect("decode pending receipt");
        assert_eq!(document.body.delta.messages.lost_count, 0);
        assert_eq!(document.body.delta.messages.added_count, 0);
        assert_eq!(document.body.delta.message_recipients.lost_count, 0);
        assert_eq!(document.body.delta.message_recipients.added_count, 0);
    }

    #[test]
    fn recovery_receipt_refuses_message_and_recipient_loss_or_semantic_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let lost_candidate = dir.path().join("lost-candidate.sqlite3");
        let drifted_candidate = dir.path().join("drifted-candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&lost_candidate, true);
        seed_recovery_receipt_db(&drifted_candidate, true);

        let lost_conn =
            crate::CanonicalDbConn::open_file(lost_candidate.to_string_lossy().as_ref())
                .expect("open candidate for message loss");
        lost_conn
            .execute_raw("DELETE FROM message_recipients WHERE message_id = 73")
            .expect("remove recipient state");
        lost_conn
            .execute_raw("DELETE FROM messages WHERE id = 73")
            .expect("remove message");
        drop(lost_conn);

        let loss =
            prepare_recovery_receipt(&storage_root, &primary, Some(&source), &lost_candidate)
                .expect_err("message and recipient loss must fail before promotion intent");
        assert!(loss.to_string().contains("messages=1"));
        assert!(loss.to_string().contains("message_recipients=1"));

        let drifted_conn =
            crate::CanonicalDbConn::open_file(drifted_candidate.to_string_lossy().as_ref())
                .expect("open candidate for message drift");
        drifted_conn
            .execute_raw("UPDATE messages SET subject = 'Rewritten subject' WHERE id = 73")
            .expect("rewrite message semantics");
        drifted_conn
            .execute_raw("UPDATE message_recipients SET read_ts = 456790 WHERE message_id = 73")
            .expect("rewrite recipient state");
        drop(drifted_conn);

        let drift =
            prepare_recovery_receipt(&storage_root, &primary, Some(&source), &drifted_candidate)
                .expect_err(
                    "message and recipient semantic drift must fail before promotion intent",
                );
        assert!(drift.to_string().contains("messages=1"));
        assert!(drift.to_string().contains("message_recipients=1"));
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("message continuity rejections must not leave a pending intent");
    }

    #[test]
    fn recovery_receipt_refuses_semantic_message_and_recipient_duplicate_inflation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);

        let conn = crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref())
            .expect("open candidate for duplicate replay");
        conn.execute_raw(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             SELECT 74, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments \
             FROM messages WHERE id = 73",
        )
        .expect("duplicate message semantics under a new id");
        conn.execute_raw(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
             SELECT 74, agent_id, kind, read_ts, ack_ts \
             FROM message_recipients WHERE message_id = 73",
        )
        .expect("duplicate recipient semantics under the new message id");
        drop(conn);

        let error = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect_err("duplicate semantic replay must fail before promotion intent");
        let error_text = error.to_string();
        assert!(error_text.contains("possible duplicate replay"));
        assert!(error_text.contains("messages=1"));
        assert!(error_text.contains("message_recipients=1"));
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("duplicate rejection must not leave a pending intent");
    }

    #[test]
    fn recovery_receipt_refuses_cross_project_message_or_recipient_ownership() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let message_candidate = dir.path().join("message-candidate.sqlite3");
        let recipient_candidate = dir.path().join("recipient-candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&message_candidate, true);
        seed_recovery_receipt_db(&recipient_candidate, true);

        for candidate in [&message_candidate, &recipient_candidate] {
            let conn = crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref())
                .expect("open candidate for cross-project fixture");
            conn.execute_raw(
                "INSERT INTO projects (id, slug, human_key) VALUES (18, 'beta', '/srv/beta')",
            )
            .expect("insert second project");
            conn.execute_raw(
                "INSERT INTO agents (id, project_id, name, reaper_exempt, registration_token) VALUES (43, 18, 'GreenField', 0, NULL)",
            )
            .expect("insert second-project agent");
            drop(conn);
        }

        let message_conn =
            crate::CanonicalDbConn::open_file(message_candidate.to_string_lossy().as_ref())
                .expect("open message candidate");
        message_conn
            .execute_raw("UPDATE messages SET project_id = 18 WHERE id = 73")
            .expect("cross-link message to a project that does not own its sender");
        drop(message_conn);
        let message_error =
            prepare_recovery_receipt(&storage_root, &primary, Some(&source), &message_candidate)
                .expect_err("cross-project sender ownership must fail closed");
        assert!(message_error.to_string().contains("message ownership join"));

        let recipient_conn =
            crate::CanonicalDbConn::open_file(recipient_candidate.to_string_lossy().as_ref())
                .expect("open recipient candidate");
        recipient_conn
            .execute_raw("UPDATE message_recipients SET agent_id = 43 WHERE message_id = 73")
            .expect("cross-link recipient to another project");
        drop(recipient_conn);
        let recipient_error =
            prepare_recovery_receipt(&storage_root, &primary, Some(&source), &recipient_candidate)
                .expect_err("cross-project recipient ownership must fail closed");
        assert!(
            recipient_error
                .to_string()
                .contains("message-recipient ownership join")
        );
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("ownership rejection must not leave a pending intent");
    }

    #[test]
    fn recovery_receipt_refuses_contact_timestamp_only_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&source, true);
        seed_recovery_receipt_db(&candidate, true);
        let conn = crate::CanonicalDbConn::open_file(candidate.to_string_lossy().as_ref())
            .expect("open candidate for contact timestamp drift");
        conn.execute_raw("UPDATE agent_links SET created_ts = 101, updated_ts = 201 WHERE id = 3")
            .expect("change only contact timestamps");
        drop(conn);

        let error = prepare_recovery_receipt(&storage_root, &primary, Some(&source), &candidate)
            .expect_err("contact timestamp drift must fail before promotion intent");
        assert!(error.to_string().contains("contacts=1"));
        verify_recovery_receipt_state(&storage_root, &primary)
            .expect("timestamp-drift rejection must not leave a pending intent");
    }

    #[test]
    fn recovery_receipt_refuses_to_extend_chain_from_wrong_source_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first_candidate = dir.path().join("candidate-first.sqlite3");
        let second_candidate = dir.path().join("candidate-second.sqlite3");
        let primary = dir.path().join("storage.sqlite3");
        let storage_root = dir.path().join("mail-root");
        seed_recovery_receipt_db(&first_candidate, true);
        seed_recovery_receipt_db(&second_candidate, true);
        let first = prepare_recovery_receipt(&storage_root, &primary, None, &first_candidate)
            .expect("prepare first receipt");
        std::fs::rename(&first_candidate, &primary).expect("activate first candidate");
        finalize_recovery_receipt(&first).expect("finalize first receipt");

        let marked_generation = dir.path().join("marked-generation.sqlite3");
        std::fs::rename(&primary, &marked_generation).expect("preserve marked generation");
        seed_recovery_receipt_db(&primary, true);
        let error =
            prepare_recovery_receipt(&storage_root, &primary, Some(&primary), &second_candidate)
                .expect_err("wrong source generation must not extend finalized chain");
        assert!(error.to_string().contains("live marker"));
        assert!(
            pending_recovery_receipt_paths(first.final_path.parent().expect("receipt parent"))
                .expect("scan pending receipts")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_receipt_authority_is_shared_by_lexical_and_symlink_db_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("storage.sqlite3");
        let candidate = dir.path().join("candidate.sqlite3");
        let storage_root = dir.path().join("mail-root");
        let alternate_storage_root = dir.path().join("alternate-mail-root");
        seed_recovery_receipt_db(&primary, true);
        seed_recovery_receipt_db(&candidate, true);

        let lexical_alias = dir.path().join(".").join("storage.sqlite3");
        let symlink_alias = dir.path().join("mailbox-alias.sqlite3");
        std::os::unix::fs::symlink(&primary, &symlink_alias)
            .expect("create database symlink alias");

        let prepared =
            prepare_recovery_receipt(&storage_root, &lexical_alias, Some(&primary), &candidate)
                .expect("prepare receipt through lexical alias");
        assert!(prepared.pending_path.exists());

        let symlink_error = verify_recovery_receipt_state(&alternate_storage_root, &symlink_alias)
            .expect_err("symlink alias must observe the same pending receipt authority");
        assert!(
            symlink_error
                .to_string()
                .contains("unfinalized recovery receipt")
        );
    }

    #[test]
    fn parse_ps_output_value_uses_first_nonempty_trimmed_line() {
        let stdout = b"\n  /Applications/Agent Mail/bin/mcp-agent-mail --stdio  \nsecond line\n";

        let parsed = parse_ps_output_value(stdout);

        assert_eq!(
            parsed.as_deref(),
            Some("/Applications/Agent Mail/bin/mcp-agent-mail --stdio")
        );
    }

    #[test]
    fn redact_database_url_masks_userinfo_inside_authority_only() {
        assert_eq!(
            redact_database_url("postgres://user:p@ss@host:5432/mail?sslmode=require"),
            "postgres://****@host:5432/mail?sslmode=require"
        );
        assert_eq!(
            redact_database_url("sqlite://admin:pass123@/data/test.db"),
            "sqlite://****@/data/test.db"
        );
    }

    #[test]
    fn redact_database_url_preserves_at_signs_in_sqlite_paths() {
        assert_eq!(
            redact_database_url("sqlite:///tmp/mail@box.sqlite3"),
            "sqlite:///tmp/mail@box.sqlite3"
        );
        assert_eq!(
            redact_database_url("sqlite+aiosqlite:///tmp/mail@box.sqlite3?mode=rwc"),
            "sqlite+aiosqlite:///tmp/mail@box.sqlite3?mode=rwc"
        );
    }

    #[test]
    fn capture_mailbox_forensic_bundle_records_reference_reports() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let storage_root = tempdir.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("demo")).expect("storage");
        let db_path = tempdir.path().join("storage.sqlite3");
        std::fs::write(&db_path, b"sqlite-bytes").expect("db");
        std::fs::write(tempdir.path().join("storage.sqlite3-wal"), b"wal").expect("wal");

        let bundle_dir = capture_mailbox_forensic_bundle(MailboxForensicCapture {
            command_name: "repair",
            trigger: "doctor",
            database_url: "sqlite:///tmp/storage.sqlite3",
            db_path: &db_path,
            storage_root: &storage_root,
            integrity_detail: Some("integrity failed"),
        })
        .expect("bundle");

        assert!(bundle_dir.join("manifest.json").exists());
        assert!(bundle_dir.join("summary.json").exists());
        assert!(
            bundle_dir
                .join("references")
                .join("live-db-state.json")
                .exists()
        );
        assert!(
            bundle_dir
                .join("references")
                .join("archive-drift.json")
                .exists()
        );
        assert!(
            bundle_dir
                .join("references")
                .join("environment.json")
                .exists()
        );

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bundle_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["trigger"], "doctor");
        assert_eq!(
            manifest["artifacts"]["references"]["live_db_state"]["path"],
            "references/live-db-state.json"
        );
    }

    #[test]
    fn archive_drift_reference_skips_in_memory_db_inventory_without_error_state() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let storage_root = tempdir.path().join("storage");
        let project_dir = storage_root.join("projects").join("demo");
        std::fs::create_dir_all(project_dir.join("messages").join("2026").join("04"))
            .expect("storage");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"demo","human_key":"/demo"}"#,
        )
        .expect("project metadata");

        let drift = build_archive_drift_reference(MailboxForensicCapture {
            command_name: "doctor",
            trigger: "test",
            database_url: "sqlite:///:memory:",
            db_path: std::path::Path::new(":memory:"),
            storage_root: &storage_root,
            integrity_detail: None,
        });

        assert_eq!(drift["database_inventory"]["status"], "skipped");
        assert_eq!(drift["archive_ahead"], false);
        assert!(
            drift["archive_drift_reasons"]
                .as_array()
                .expect("drift reasons")
                .iter()
                .any(|value| value == "database_inventory_skipped_in_memory")
        );
    }

    #[test]
    fn archive_drift_reference_suppresses_metadata_only_archive_ahead_when_db_has_newer_messages() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("storage.sqlite3");
        let storage_root = tempdir.path().join("storage");

        let proj_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = proj_dir.join("agents").join("Alice");
        let msg_dir = proj_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&msg_dir).expect("create message dir");
        std::fs::write(
            proj_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-03-22T00:00:00Z","last_active_ts":"2026-03-22T00:00:01Z"}"#,
        )
        .expect("write agent metadata");
        std::fs::write(
            msg_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            "---json\n{\"id\":1,\"from\":\"Alice\",\"to\":[\"Bob\"],\"subject\":\"First\",\"importance\":\"normal\",\"ack_required\":false,\"created_ts\":\"2026-03-22T12:00:00Z\",\"attachments\":[]}\n---\n\nfirst body\n",
        )
        .expect("write archive message");

        crate::reconstruct::reconstruct_from_archive(&db_path, &storage_root)
            .expect("seed live db from archive");

        let conn = crate::DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                crate::sqlmodel::Value::BigInt(3),
                crate::sqlmodel::Value::BigInt(1),
                crate::sqlmodel::Value::Text("BlueLake".to_string()),
                crate::sqlmodel::Value::Text("coder".to_string()),
                crate::sqlmodel::Value::Text("test".to_string()),
                crate::sqlmodel::Value::Text(String::new()),
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::Text("auto".to_string()),
                crate::sqlmodel::Value::Text("auto".to_string()),
            ],
        )
        .expect("insert recipient");
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::BigInt(1),
                crate::sqlmodel::Value::BigInt(1),
                crate::sqlmodel::Value::Text("t2".to_string()),
                crate::sqlmodel::Value::Text("Second".to_string()),
                crate::sqlmodel::Value::Text("second body".to_string()),
                crate::sqlmodel::Value::Text("normal".to_string()),
                crate::sqlmodel::Value::BigInt(0),
                crate::sqlmodel::Value::BigInt(2_000_000),
                crate::sqlmodel::Value::Text("[]".to_string()),
                crate::sqlmodel::Value::Text(r#"{"to":["BlueLake"],"cc":[],"bcc":[]}"#.to_string()),
            ],
        )
        .expect("insert newer live message");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, ack_ts, read_ts) VALUES (?, ?, ?, NULL, NULL)",
            &[
                crate::sqlmodel::Value::BigInt(2),
                crate::sqlmodel::Value::BigInt(3),
                crate::sqlmodel::Value::Text("to".to_string()),
            ],
        )
        .expect("insert message recipient");
        drop(conn);

        let archive_only_project = storage_root.join("projects").join("archive-only-project");
        let archive_only_agent = archive_only_project.join("agents").join("ArchiveGhost");
        std::fs::create_dir_all(&archive_only_agent).expect("create archive-only project dir");
        std::fs::write(
            archive_only_project.join("project.json"),
            r#"{"slug":"archive-only-project","human_key":"/archive-only-project","created_at":0}"#,
        )
        .expect("write archive-only project metadata");
        std::fs::write(
            archive_only_agent.join("profile.json"),
            r#"{"agent_name":"ArchiveGhost","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write archive-only agent metadata");

        let drift = build_archive_drift_reference(MailboxForensicCapture {
            command_name: "doctor",
            trigger: "test",
            database_url: "sqlite://placeholder",
            db_path: &db_path,
            storage_root: &storage_root,
            integrity_detail: None,
        });

        assert_eq!(drift["archive_ahead"], false);
        let reasons = drift["archive_drift_reasons"]
            .as_array()
            .expect("drift reasons");
        assert!(
            !reasons
                .iter()
                .any(|value| value == "archive_projects_ahead"),
            "metadata-only archive project drift should not be reported as decisive archive-ahead drift when the DB has newer messages: {reasons:?}"
        );
        assert!(
            !reasons
                .iter()
                .any(|value| value == "archive_project_identity_ahead"),
            "metadata-only project identity drift should not be reported as decisive archive-ahead drift when the DB has newer messages: {reasons:?}"
        );
    }

    #[test]
    fn live_db_reference_marks_in_memory_artifacts_not_applicable() {
        let live_db = build_live_db_reference(MailboxForensicCapture {
            command_name: "doctor",
            trigger: "test",
            database_url: "sqlite:///:memory:",
            db_path: std::path::Path::new(":memory:"),
            storage_root: std::path::Path::new("/tmp"),
            integrity_detail: None,
        });

        assert_eq!(live_db["db"]["status"], "in_memory");
        assert_eq!(live_db["journal"]["status"], "not_applicable");
        assert_eq!(live_db["wal"]["status"], "not_applicable");
        assert_eq!(live_db["shm"]["status"], "not_applicable");
        assert_eq!(
            live_db["process_inventory"]["holders"]
                .as_array()
                .expect("holders")
                .len(),
            0
        );
        assert_eq!(
            live_db["file_locks"]["locks"]
                .as_array()
                .expect("locks")
                .len(),
            0
        );
    }

    #[test]
    fn live_db_reference_reports_rollback_journal_artifacts() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("mailbox.sqlite3");
        let journal_path = tempdir.path().join("mailbox.sqlite3-journal");
        std::fs::write(&db_path, b"SQLite format 3\0").expect("write db header");
        std::fs::write(&journal_path, b"rollback-journal").expect("write journal");

        let live_db = build_live_db_reference(MailboxForensicCapture {
            command_name: "doctor",
            trigger: "test",
            database_url: "sqlite:///tmp/mailbox.sqlite3",
            db_path: &db_path,
            storage_root: tempdir.path(),
            integrity_detail: None,
        });

        assert_eq!(live_db["journal"]["exists"], true);
        assert_eq!(live_db["journal"]["bytes"], 16);
        assert_eq!(live_db["sidecars"]["journal_exists"], true);
        assert_eq!(live_db["sidecars"]["journal_bytes"], 16);
    }

    #[test]
    fn capture_mailbox_forensic_bundle_preserves_missing_db_as_evidence() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let storage_root = tempdir.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("demo")).expect("storage");
        let db_path = tempdir.path().join("missing.sqlite3");

        let bundle_dir = capture_mailbox_forensic_bundle(MailboxForensicCapture {
            command_name: "reconstruct",
            trigger: "automatic-recovery",
            database_url: "sqlite:///tmp/missing.sqlite3",
            db_path: &db_path,
            storage_root: &storage_root,
            integrity_detail: None,
        })
        .expect("bundle");

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bundle_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["artifacts"]["sqlite"]["db"]["status"],
            "missing_required"
        );
        assert!(
            bundle_dir
                .join("references")
                .join("archive-drift.json")
                .exists(),
            "archive drift evidence should still be recorded"
        );
    }

    #[test]
    fn capture_mailbox_forensic_bundle_marks_in_memory_sqlite_artifacts_not_applicable() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let storage_root = tempdir.path().join("storage");
        std::fs::create_dir_all(storage_root.join("projects").join("demo")).expect("storage");

        let bundle_dir = capture_mailbox_forensic_bundle(MailboxForensicCapture {
            command_name: "reconstruct",
            trigger: "automatic-recovery",
            database_url: "sqlite:///:memory:",
            db_path: std::path::Path::new(":memory:"),
            storage_root: &storage_root,
            integrity_detail: None,
        })
        .expect("bundle");

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bundle_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["artifacts"]["sqlite"]["db"]["status"],
            "not_applicable"
        );
        assert_eq!(
            manifest["artifacts"]["sqlite"]["journal"]["status"],
            "not_applicable"
        );
        assert_eq!(
            manifest["artifacts"]["sqlite"]["wal"]["status"],
            "not_applicable"
        );
        assert_eq!(
            manifest["artifacts"]["sqlite"]["shm"]["status"],
            "not_applicable"
        );
        assert_eq!(manifest["artifacts"]["sqlite"]["db"]["required"], false);
        assert!(
            bundle_dir
                .components()
                .any(|component| component.as_os_str() == "in-memory.sqlite3"),
            "bundle path should use sanitized in-memory directory name: {}",
            bundle_dir.display()
        );
    }

    #[test]
    fn forensic_bundle_dir_component_sanitizes_invalid_path_characters() {
        assert_eq!(
            super::forensic_bundle_dir_component(std::path::Path::new(":memory:")),
            "in-memory.sqlite3"
        );
        assert_eq!(
            super::forensic_bundle_dir_component(std::path::Path::new("mail:box?.sqlite3")),
            "mail_box_.sqlite3"
        );
    }

    // ── ForensicPreSnapshot tests ────────────────────────────────────

    #[test]
    fn pre_snapshot_captures_existing_db_family() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.sqlite3");
        // Write a minimal valid SQLite header (100 bytes).
        let mut header = vec![0u8; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        // Page size = 4096 (big-endian u16 at offset 16).
        header[16] = 0x10;
        header[17] = 0x00;
        // Page count = 42 (big-endian u32 at offset 28).
        header[28] = 0;
        header[29] = 0;
        header[30] = 0;
        header[31] = 42;
        std::fs::write(&db_path, &header).expect("write db");

        // Create a WAL sidecar.
        let wal_path = dir.path().join("test.sqlite3-wal");
        std::fs::write(&wal_path, vec![0u8; 512]).expect("write wal");

        let snap = capture_pre_recovery_snapshot(&db_path, "test-trigger");

        assert_eq!(snap.trigger, "test-trigger");
        assert_eq!(snap.db_family, "test.sqlite3");
        assert_eq!(snap.db_bytes, Some(100));
        assert!(snap.journal_bytes.is_none());
        assert_eq!(snap.wal_bytes, Some(512));
        assert!(snap.shm_bytes.is_none());
        assert_eq!(snap.page_size, Some(4096));
        assert_eq!(snap.page_count, Some(42));
        assert!(!snap.recovery_lock_active);
        assert!(snap.recovery_lock_pid.is_none());
        assert_eq!(snap.self_pid, std::process::id());
        assert!(snap.captured_at_us > 0);
        // Environment fields are None until with_environment is called.
        assert!(snap.storage_root.is_none());
        assert!(snap.database_url_redacted.is_none());
    }

    #[test]
    fn pre_snapshot_handles_missing_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("nonexistent.sqlite3");

        let snap = capture_pre_recovery_snapshot(&db_path, "missing-db");

        assert!(snap.db_bytes.is_none());
        assert!(snap.journal_bytes.is_none());
        assert!(snap.wal_bytes.is_none());
        assert!(snap.shm_bytes.is_none());
        assert!(snap.page_size.is_none());
        assert!(snap.page_count.is_none());
    }

    #[test]
    fn pre_snapshot_in_memory_database_avoids_fake_sidecar_evidence() {
        let snap = capture_pre_recovery_snapshot(std::path::Path::new(":memory:"), "memory");

        assert_eq!(snap.db_family, ":memory:");
        assert!(snap.db_bytes.is_none());
        assert!(snap.journal_bytes.is_none());
        assert!(snap.wal_bytes.is_none());
        assert!(snap.shm_bytes.is_none());
        assert!(snap.page_size.is_none());
        assert!(snap.page_count.is_none());
        assert!(snap.process_holders.is_empty());
        assert!(snap.file_locks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pre_snapshot_preserves_non_utf8_sidecar_and_lock_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_name = OsString::from_vec(b"mailbox-\xFF.sqlite3".to_vec());
        let db_path = dir.path().join(std::path::PathBuf::from(db_name));
        let journal_path = crate::pool::sqlite_path_with_suffix(&db_path, "-journal");

        let mut header = vec![0u8; 100];
        header[0..16].copy_from_slice(b"SQLite format 3\0");
        header[16] = 0x10;
        header[17] = 0x00;
        std::fs::write(&db_path, &header).expect("write db");
        std::fs::write(&journal_path, b"rollback").expect("write journal");

        let wal_path = crate::pool::sqlite_path_with_suffix(&db_path, "-wal");
        std::fs::write(&wal_path, vec![0u8; 512]).expect("write wal");

        let lock_path = crate::pool::sqlite_path_with_suffix(&db_path, ".recovery.lock");
        std::fs::write(&lock_path, std::process::id().to_string()).expect("write lock");

        let snap = capture_pre_recovery_snapshot(&db_path, "non-utf8");

        assert_eq!(snap.journal_bytes, Some(8));
        assert_eq!(snap.wal_bytes, Some(512));
        assert!(snap.recovery_lock_active);
        assert_eq!(snap.recovery_lock_pid, Some(std::process::id()));
    }

    #[test]
    fn pre_snapshot_handles_non_sqlite_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("not_sqlite.db");
        std::fs::write(&db_path, b"this is not a sqlite file").expect("write");

        let snap = capture_pre_recovery_snapshot(&db_path, "corrupt");

        assert_eq!(snap.db_bytes, Some(25));
        assert!(snap.journal_bytes.is_none());
        assert!(
            snap.page_size.is_none(),
            "non-sqlite header should yield None"
        );
        assert!(snap.page_count.is_none());
    }

    #[test]
    fn pre_snapshot_serializes_to_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("serial.sqlite3");
        std::fs::write(&db_path, b"short").expect("write");

        let snap = capture_pre_recovery_snapshot(&db_path, "json-test");
        let json = serde_json::to_value(&snap).expect("serialize");

        assert_eq!(json["trigger"], "json-test");
        assert_eq!(json["db_family"], "serial.sqlite3");
        assert_eq!(json["journal_bytes"], serde_json::Value::Null);
        assert!(json["self_pid"].is_number());
        assert!(json["captured_at_us"].is_number());
        assert!(json["process_holders"].is_array());
        assert!(json["file_locks"].is_array());
        assert_eq!(json["recovery_lock_active"], false);
        // Optional env fields should be absent when not set.
        assert!(json.get("storage_root").is_none());
        assert!(json.get("database_url_redacted").is_none());
    }

    #[test]
    fn pre_snapshot_with_environment_attaches_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("env.sqlite3");
        std::fs::write(&db_path, b"short").expect("write");

        let snap = capture_pre_recovery_snapshot(&db_path, "env-test")
            .with_environment(dir.path(), "sqlite://user:secret@/db.sqlite3");

        assert_eq!(
            snap.storage_root.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
        assert_eq!(
            snap.database_url_redacted.as_deref(),
            Some("sqlite://****@/db.sqlite3")
        );
        // Verify JSON includes the environment fields.
        let json = serde_json::to_value(&snap).expect("serialize");
        assert!(json["storage_root"].is_string());
        assert!(json["database_url_redacted"].is_string());
    }

    #[test]
    fn pre_snapshot_detects_active_recovery_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("locked.sqlite3");
        std::fs::write(&db_path, b"data").expect("write db");

        // Write a recovery lock file with our own PID (guaranteed alive).
        let lock_path = dir.path().join("locked.sqlite3.recovery.lock");
        std::fs::write(&lock_path, std::process::id().to_string()).expect("write lock");

        let snap = capture_pre_recovery_snapshot(&db_path, "lock-test");

        assert!(
            snap.recovery_lock_active,
            "should detect live recovery lock"
        );
        assert_eq!(snap.recovery_lock_pid, Some(std::process::id()));
    }

    #[test]
    fn pre_snapshot_detects_stale_recovery_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("stale.sqlite3");
        std::fs::write(&db_path, b"data").expect("write db");

        // PID 999999999 almost certainly doesn't exist.
        let lock_path = dir.path().join("stale.sqlite3.recovery.lock");
        std::fs::write(&lock_path, "999999999").expect("write lock");

        let snap = capture_pre_recovery_snapshot(&db_path, "stale-lock");

        assert!(
            !snap.recovery_lock_active,
            "stale lock should not be active"
        );
        assert_eq!(snap.recovery_lock_pid, Some(999_999_999));
    }

    #[test]
    fn read_sqlite_header_fields_page_size_1_means_65536() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("big_page.sqlite3");
        let mut header = vec![0u8; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        // Page size = 1 means 65536.
        header[16] = 0x00;
        header[17] = 0x01;
        // Page count = 1.
        header[31] = 1;
        std::fs::write(&db_path, &header).expect("write");

        let (ps, pc) = read_sqlite_header_fields(&db_path).expect("valid header");
        assert_eq!(ps, 65_536);
        assert_eq!(pc, 1);
    }

    #[test]
    fn read_sqlite_header_fields_rejects_truncated_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("truncated.sqlite3");
        std::fs::write(&db_path, b"SQLite format 3\0").expect("write");

        assert!(
            read_sqlite_header_fields(&db_path).is_none(),
            "16-byte file should fail (need 32 bytes)"
        );
    }

    #[test]
    fn read_sqlite_header_fields_rejects_invalid_page_sizes() {
        for raw_page_size in [0_u16, 256, 513, 65_535] {
            let dir = tempfile::tempdir().expect("tempdir");
            let db_path = dir
                .path()
                .join(format!("page_size_{raw_page_size}.sqlite3"));
            let mut header = vec![0u8; 100];
            header[..16].copy_from_slice(b"SQLite format 3\0");
            header[16..18].copy_from_slice(&raw_page_size.to_be_bytes());
            header[31] = 1;
            std::fs::write(&db_path, &header).expect("write");

            assert!(
                read_sqlite_header_fields(&db_path).is_none(),
                "page size {raw_page_size} should be rejected"
            );
        }
    }
}
