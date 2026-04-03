//! Shared mailbox forensic bundle capture for recovery entrypoints.
//!
//! The doctor CLI originally owned forensic bundle creation. This module lifts
//! the bundle contract into the DB layer so startup/runtime recovery paths can
//! preserve the same evidence before any repair or reconstruct logic mutates
//! the live mailbox state.

use crate::{
    pool::{
        inspect_mailbox_db_inventory, inspect_mailbox_recovery_lock, inspect_mailbox_sidecar_state,
    },
    reconstruct::{
        ArchiveMessageInventory, archive_missing_project_identities,
        compute_archive_drift_report, scan_archive_message_inventory,
    },
};
use serde::Serialize;
use serde_json::json;
use sha2::Digest;
use sqlmodel_core::Error as SqlError;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeSet, HashMap},
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

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    major: u32,
    minor: u32,
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
    /// WAL file size in bytes, or `None` if missing.
    pub wal_bytes: Option<u64>,
    /// SHM file size in bytes, or `None` if missing.
    pub shm_bytes: Option<u64>,
    /// SQLite page size read from the DB header (bytes 16..18), or `None` on error.
    pub page_size: Option<u32>,
    /// Total page count read from the DB header (bytes 28..32), or `None` on error.
    pub page_count: Option<u32>,
    /// Processes with open file descriptors on the DB/WAL/SHM files.
    pub process_holders: Vec<ForensicProcessHolder>,
    /// File-level locks held on the DB/WAL/SHM files.
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
    let page_size: u32 = if raw_page_size == 1 {
        65_536
    } else {
        u32::from(raw_page_size)
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
    let db_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    let wal_bytes = std::fs::metadata(&wal_path).ok().map(|m| m.len());
    let shm_bytes = std::fs::metadata(&shm_path).ok().map(|m| m.len());
    let (page_size, page_count) = read_sqlite_header_fields(db_path)
        .map(|(ps, pc)| (Some(ps), Some(pc)))
        .unwrap_or((None, None));

    let family_paths: Vec<(&str, PathBuf)> = vec![
        ("db", db_path.to_path_buf()),
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
    if let Some((scheme, rest)) = url.split_once("://")
        && let Some((_creds, host)) = rest.rsplit_once('@')
    {
        return format!("{scheme}://****@{host}");
    }
    url.to_string()
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
    let db_inventory = inspect_mailbox_db_inventory(capture.db_path);
    let projects_dir = capture.storage_root.join("projects");

    let (db_inventory_json, missing_archive_projects, drift_reasons) = match db_inventory {
        Ok(inventory) => {
            let labels =
                archive_missing_project_identities(&archive, &inventory.project_identities);
            let mut reasons = Vec::new();
            if archive.projects > inventory.projects {
                reasons.push("archive_projects_ahead".to_string());
            }
            if archive.agents > inventory.agents {
                reasons.push("archive_agents_ahead".to_string());
            }
            if archive.unique_message_ids > inventory.messages {
                reasons.push("archive_messages_ahead".to_string());
            }
            if archive.latest_message_id.unwrap_or(0) > inventory.max_message_id {
                reasons.push("archive_latest_id_ahead".to_string());
            }
            if !labels.is_empty() {
                reasons.push("archive_project_identity_ahead".to_string());
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
    };

    json!({
        "schema": { "name": "mcp-agent-mail-mailbox-forensics-archive-drift", "major": 1, "minor": 0 },
        "command": capture.command_name,
        "trigger": capture.trigger,
        "archive": archive_inventory_json(capture.storage_root, &projects_dir, &archive),
        "database_inventory": db_inventory_json,
        "archive_ahead": !drift_reasons.is_empty()
            && !drift_reasons.iter().all(|reason| reason == "database_inventory_unavailable"),
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
    let wal_path = PathBuf::from(format!("{}-wal", capture.db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", capture.db_path.display()));
    let sidecars = inspect_mailbox_sidecar_state(capture.db_path);
    let recovery_lock = inspect_mailbox_recovery_lock(capture.db_path);
    let holders = process_holders_for_paths(&[
        ("db", capture.db_path.to_path_buf()),
        ("wal", wal_path.clone()),
        ("shm", shm_path.clone()),
    ]);
    let locks = file_locks_for_paths(&[
        ("db", capture.db_path.to_path_buf()),
        ("wal", wal_path.clone()),
        ("shm", shm_path.clone()),
    ]);

    json!({
        "schema": { "name": "mcp-agent-mail-mailbox-forensics-live-db-state", "major": 1, "minor": 0 },
        "command": capture.command_name,
        "trigger": capture.trigger,
        "db_family": forensic_db_family_name(capture.db_path),
        "db": source_file_status(capture.db_path),
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
    let major = ((dev >> 8) & 0xfff) as u32;
    let minor = ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u32;
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
            let target_identity = FileIdentity {
                dev: target_meta.dev(),
                ino: target_meta.ino(),
                major: ((target_meta.dev() >> 8) & 0xfff) as u32,
                minor: ((target_meta.dev() & 0xff) | ((target_meta.dev() >> 12) & 0xfff00)) as u32,
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

#[cfg(not(target_os = "linux"))]
fn pid_command_line(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!command.is_empty()).then_some(command)
}

#[cfg(target_os = "linux")]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_executable_path(pid: u32) -> Option<PathBuf> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let argv0 = command.split_whitespace().next()?.trim();
    (!argv0.is_empty()).then(|| PathBuf::from(argv0))
}

/// Capture a mailbox forensic bundle and return the bundle directory.
#[allow(clippy::result_large_err)]
pub fn capture_mailbox_forensic_bundle(
    capture: MailboxForensicCapture<'_>,
) -> Result<PathBuf, SqlError> {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let db_family = forensic_db_family_name(capture.db_path);
    let bundle_name = format!("{}-{timestamp}", capture.command_name);
    let bundle_dir = forensics_root(capture.storage_root, capture.db_path)
        .join(&db_family)
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
    let source_paths = [
        ("db", capture.db_path.to_path_buf()),
        (
            "wal",
            PathBuf::from(format!("{}-wal", capture.db_path.display())),
        ),
        (
            "shm",
            PathBuf::from(format!("{}-shm", capture.db_path.display())),
        ),
    ];

    let mut artifacts = Vec::new();
    let mut sqlite_manifest = serde_json::Map::new();
    let mut copied_paths = Vec::new();
    let mut files = Vec::new();

    for (kind, source_path) in source_paths {
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
        MailboxForensicCapture, capture_mailbox_forensic_bundle, capture_pre_recovery_snapshot,
        read_sqlite_header_fields,
    };

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
        assert!(snap.wal_bytes.is_none());
        assert!(snap.shm_bytes.is_none());
        assert!(snap.page_size.is_none());
        assert!(snap.page_count.is_none());
    }

    #[test]
    fn pre_snapshot_handles_non_sqlite_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("not_sqlite.db");
        std::fs::write(&db_path, b"this is not a sqlite file").expect("write");

        let snap = capture_pre_recovery_snapshot(&db_path, "corrupt");

        assert_eq!(snap.db_bytes, Some(25));
        assert!(snap.page_size.is_none(), "non-sqlite header should yield None");
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
            .with_environment(dir.path(), "sqlite:///secret@host/db.sqlite3");

        assert_eq!(snap.storage_root.as_deref(), Some(dir.path().to_str().unwrap()));
        assert_eq!(
            snap.database_url_redacted.as_deref(),
            Some("sqlite://****@host/db.sqlite3")
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

        assert!(snap.recovery_lock_active, "should detect live recovery lock");
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

        assert!(!snap.recovery_lock_active, "stale lock should not be active");
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
}
