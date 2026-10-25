//! Durable non-convergence circuit breaker for AUTOMATIC mailbox recovery
//! (br-acusl).
//!
//! The in-process [`crate::pool`] recovery admission already provides
//! single-flight, backoff, and windowed loop suppression — but its state is
//! `Instant`-based and process-local, so a restarting (or long-looping)
//! daemon re-attempts recovery of a permanently unrepairable database
//! forever, capturing a forensic bundle per attempt (the trj incident
//! refilled 96 GB+ of dumps this way; see br-vdpyv for the capture-side
//! guardrails).
//!
//! This module adds the durable layer: a tiny JSON sidecar next to the
//! database records consecutive automatic-recovery failures **for the same
//! database generation** (on Unix, a stable full-content family fingerprint;
//! other platforms retain the bounded single-file fingerprint). Once the failure
//! count reaches the threshold the breaker trips, and automatic recovery
//! refuses fast — BEFORE any forensic capture — until either
//!
//! * the cooldown elapses (one half-open attempt is then admitted),
//! * the database content changes with no unfinished automatic attempt, or
//! * an operator-invoked path (doctor repair/reconstruct) runs with the
//!   explicit [`RecoveryBreakerBypassGuard`], which is never refused.
//!
//! A successful recovery — automatic or operator — clears the state (the
//! sidecar is overwritten with a cleared record, never deleted).
//!
//! Transitions are pure functions over an explicit state struct so every
//! path is unit-testable without touching the filesystem.

use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::cell::Cell;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Consecutive automatic-recovery failures (same content) before tripping.
pub const DEFAULT_MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Seconds a tripped breaker refuses automatic attempts before admitting a
/// single half-open probe (6 hours).
pub const DEFAULT_COOLDOWN_SECS: u64 = 21_600;

/// Cap stored failure reasons so the sidecar can never bloat.
const MAX_REASON_BYTES: usize = 512;

/// Durable breaker records are tiny; reject unexpectedly large control files
/// before allocating or parsing them.
const MAX_SIDECAR_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct RecoveryBreakerConfig {
    pub max_consecutive_failures: u32,
    pub cooldown_secs: u64,
}

/// Pure parse: missing/empty/garbage/zero fall back to the default so a
/// fat-fingered override can never disable the breaker.
fn parse_positive_u64(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[must_use]
pub fn config_from_env() -> RecoveryBreakerConfig {
    // Read through the shared process-env accessor so the knobs honour the
    // same override layer as every other AM_* setting (including the test
    // override scope); a raw `std::env::var` here made the thresholds
    // unreachable from tests and inconsistent with `Config`.
    let max_failures = parse_positive_u64(
        mcp_agent_mail_core::config::process_env_value(
            "AM_RECOVERY_BREAKER_MAX_CONSECUTIVE_FAILURES",
        )
        .as_deref(),
        u64::from(DEFAULT_MAX_CONSECUTIVE_FAILURES),
    );
    RecoveryBreakerConfig {
        max_consecutive_failures: u32::try_from(max_failures)
            .unwrap_or(DEFAULT_MAX_CONSECUTIVE_FAILURES),
        cooldown_secs: parse_positive_u64(
            mcp_agent_mail_core::config::process_env_value("AM_RECOVERY_BREAKER_COOLDOWN_SECS")
                .as_deref(),
            DEFAULT_COOLDOWN_SECS,
        ),
    }
}

/// Durable breaker state, one JSON object per database sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryBreakerState {
    pub schema: u32,
    /// Content fingerprint of the database the failures were observed on.
    pub db_fingerprint: String,
    pub consecutive_failures: u32,
    /// Unix seconds of the most recent failure.
    pub last_failure_unix: i64,
    /// Truncated human-readable reason for the most recent failure.
    pub last_failure_reason: String,
    pub tripped: bool,
    /// Armed before automatic recovery starts; cleared by a terminal result.
    /// While set, changed bytes may be our own interrupted mutation and must
    /// not reset the failure lineage. Process death cannot clear this marker.
    #[serde(default)]
    pub attempt_in_progress: bool,
}

impl RecoveryBreakerState {
    /// Whether this history governs the current database generation.
    /// An unfinished attempt may have changed the bytes before process death.
    #[must_use]
    pub fn applies_to(&self, fingerprint: &str) -> bool {
        self.attempt_in_progress
            || self.db_fingerprint == fingerprint
            // Failed observation proves no change of generation. Preserve the
            // lineage through both loss and restoration of read access; the
            // cooldown or an explicitly successful repair still clears it.
            || fingerprint == "unreadable"
            || self.db_fingerprint == "unreadable"
            // A former main-only record proves nothing about sidecar changes.
            // Do not erase its history merely because this binary now records
            // the whole family. A genuine main-content change remains visible.
            || family_fingerprint_parts(fingerprint).is_some_and(|(main, _)| {
                self.db_fingerprint == main
                    // pool.rs's live, descriptor-neutral format cannot be
                    // compared to a raw content hash after ownership changes.
                    || self.db_fingerprint.split_once(':').is_some_and(|(_, hash)| {
                        hash.starts_with("fdc10ce1")
                    })
            })
            || (fingerprint
                .split_once(':')
                .is_some_and(|(_, hash)| hash.starts_with("fdc10ce1"))
                && family_fingerprint_parts(&self.db_fingerprint).is_some())
    }
}

/// What the breaker says about an automatic recovery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerVerdict {
    /// No relevant durable history — proceed normally.
    Allow,
    /// Tripped, but the cooldown elapsed — admit ONE half-open probe.
    AllowHalfOpen,
    /// Tripped for this same database content within the cooldown window.
    Refuse {
        consecutive_failures: u32,
        retry_after_secs: u64,
        last_failure_reason: String,
    },
}

/// PURE: evaluate the breaker for the database currently at `fingerprint`.
#[must_use]
pub fn evaluate(
    state: Option<&RecoveryBreakerState>,
    fingerprint: &str,
    config: RecoveryBreakerConfig,
    now_unix: i64,
) -> BreakerVerdict {
    let Some(state) = state else {
        return BreakerVerdict::Allow;
    };
    if !state.applies_to(fingerprint) {
        // Different content ⇒ different problem (operator replaced or
        // quarantined the file, or it changed on its own). Start fresh.
        return BreakerVerdict::Allow;
    }
    if !state.tripped && state.consecutive_failures < config.max_consecutive_failures {
        return BreakerVerdict::Allow;
    }
    // A future timestamp can arise from clock correction or corrupt control
    // state. Treat it as zero elapsed instead of allowing signed subtraction
    // to overflow while computing the retry interval.
    let elapsed = now_unix.saturating_sub(state.last_failure_unix).max(0);
    let cooldown = i64::try_from(config.cooldown_secs).unwrap_or(i64::MAX);
    if elapsed >= cooldown {
        return BreakerVerdict::AllowHalfOpen;
    }
    BreakerVerdict::Refuse {
        consecutive_failures: state.consecutive_failures,
        retry_after_secs: u64::try_from(cooldown.saturating_sub(elapsed)).unwrap_or(0),
        last_failure_reason: state.last_failure_reason.clone(),
    }
}

/// PURE: fold one failed automatic-recovery attempt into the state.
#[must_use]
pub fn record_failure(
    prev: Option<&RecoveryBreakerState>,
    fingerprint: &str,
    reason: &str,
    config: RecoveryBreakerConfig,
    now_unix: i64,
) -> RecoveryBreakerState {
    let consecutive_failures = match prev {
        Some(prev) if prev.applies_to(fingerprint) => prev.consecutive_failures.saturating_add(1),
        _ => 1,
    };
    let mut truncated_reason = reason.to_string();
    if truncated_reason.len() > MAX_REASON_BYTES {
        truncated_reason.truncate(
            (0..=MAX_REASON_BYTES)
                .rev()
                .find(|end| truncated_reason.is_char_boundary(*end))
                .unwrap_or(0),
        );
        truncated_reason.push('…');
    }
    RecoveryBreakerState {
        schema: 1,
        db_fingerprint: fingerprint.to_string(),
        consecutive_failures,
        last_failure_unix: now_unix,
        last_failure_reason: truncated_reason,
        tripped: consecutive_failures >= config.max_consecutive_failures,
        attempt_in_progress: false,
    }
}

/// Arm a counted attempt before entering automatic recovery. A terminal
/// failure replaces this record without counting the same attempt twice.
#[must_use]
pub(crate) fn record_attempt(
    prev: Option<&RecoveryBreakerState>,
    fingerprint: &str,
    config: RecoveryBreakerConfig,
    now_unix: i64,
) -> RecoveryBreakerState {
    let mut state = record_failure(
        prev,
        fingerprint,
        "automatic recovery attempt did not complete",
        config,
        now_unix,
    );
    state.attempt_in_progress = true;
    state
}

/// PURE: the state written after a successful recovery. The sidecar is
/// overwritten (never deleted) so the clearing is itself auditable.
#[must_use]
pub fn cleared_state(fingerprint: &str) -> RecoveryBreakerState {
    RecoveryBreakerState {
        schema: 1,
        db_fingerprint: fingerprint.to_string(),
        consecutive_failures: 0,
        last_failure_unix: 0,
        last_failure_reason: String::new(),
        tripped: false,
        attempt_in_progress: false,
    }
}

/// Sidecar path: `<db>.am-recovery-breaker.json`.
///
/// The name deliberately matches none of the complete published candidate
/// grammars accepted by `classify_sqlite_recovery_candidate_name`, so the
/// breaker file can never become a restorable backup. It also carries none of
/// SQLite's or FrankenSQLite's companion suffixes.
#[must_use]
pub fn breaker_sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(".am-recovery-breaker.json");
    PathBuf::from(name)
}

/// Cross-process recovery-breaker election path: `<db>.am-recovery-breaker.lock`.
#[must_use]
pub fn breaker_lock_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(".am-recovery-breaker.lock");
    PathBuf::from(name)
}

/// Held advisory lock serializing one database's durable breaker transition.
#[derive(Debug)]
pub(crate) struct RecoveryBreakerFileLock {
    file: std::fs::File,
    path: PathBuf,
}

impl RecoveryBreakerFileLock {
    /// A retained flock is authority only while its original name still refers
    /// to the same private, unaliased inode. Retention rechecks this before
    /// reclaiming evidence, so replacement cannot create two recovery owners.
    pub(crate) fn revalidate(&self) -> std::io::Result<()> {
        crate::pool::validate_sqlite_target_path(&self.path, "recovery election lock")
            .map_err(std::io::Error::other)?;
        ensure_exclusive_breaker_authority_file(&self.file, &self.path)?;
        let current = mcp_agent_mail_core::disk::open_regular_file_no_follow(&self.path)?;
        ensure_exclusive_breaker_authority_file(&current, &self.path)?;
        let retained_identity = same_file::Handle::from_file(self.file.try_clone()?)?;
        let current_identity = same_file::Handle::from_file(current)?;
        if retained_identity != current_identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "recovery election lock identity or ownership changed",
            ));
        }
        Ok(())
    }
}

/// Try to become the sole process evaluating and updating this database's
/// breaker state. The lock is held across the admitted recovery operation and
/// terminal state write; process death releases it automatically.
pub(crate) fn try_acquire_file_lock(db_path: &Path) -> std::io::Result<RecoveryBreakerFileLock> {
    let path = std::path::absolute(breaker_lock_path(db_path))?;
    crate::pool::validate_sqlite_target_path(&path, "recovery election lock")
        .map_err(std::io::Error::other)?;
    let file = open_breaker_lock_file(&path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    let guard = RecoveryBreakerFileLock { file, path };
    guard.revalidate()?;
    Ok(guard)
}

fn breaker_authority_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        Ok(file.metadata()?.nlink())
    }

    #[cfg(windows)]
    {
        mcp_agent_mail_core::disk::windows_file_link_count(file)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this platform cannot prove exclusive breaker authority file ownership",
        ))
    }
}

fn ensure_exclusive_breaker_authority_file(
    file: &std::fs::File,
    path: &Path,
) -> std::io::Result<()> {
    let links = breaker_authority_link_count(file)?;
    if links == 1 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} has {links} hard links; refusing to mutate shared breaker authority",
                path.display()
            ),
        ))
    }
}

fn secure_existing_breaker_authority_file(path: &Path) -> std::io::Result<()> {
    let file = mcp_agent_mail_core::disk::open_regular_file_for_permission_change_no_follow(path)?;
    ensure_exclusive_breaker_authority_file(&file, path)?;
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(&file)
}

#[cfg(unix)]
fn open_breaker_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            secure_existing_breaker_authority_file(path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular lock file", path.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular lock file", path.display()),
        ));
    }
    ensure_exclusive_breaker_authority_file(&file, path)?;
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(&file)?;
    Ok(file)
}

#[cfg(windows)]
fn open_breaker_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            secure_existing_breaker_authority_file(path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular lock file", path.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.file_type().is_file()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not a regular non-reparse-point lock file",
                path.display()
            ),
        ));
    }
    ensure_exclusive_breaker_authority_file(&file, path)?;
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(&file)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_breaker_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            secure_existing_breaker_authority_file(path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular lock file", path.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} changed to a non-regular lock file", path.display()),
        ));
    }
    ensure_exclusive_breaker_authority_file(&file, path)?;
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(&file)?;
    Ok(file)
}

/// Content fingerprint of the offline database generation.
///
/// Unix covers the complete known SQLite family, including absent members.
/// Other platforms retain `len:sha256(bytes)`. A missing family is `missing`;
/// an unreadable one is `unreadable` (failures on either still accumulate).
/// Recovery
/// is exceptional and correctness matters more than the cost of hashing the
/// complete generation: a same-length repair beyond the first SQLite page must
/// reset the breaker.
///
/// Hash only an offline/raw-readable source. Live FrankenSQLite callers must
/// use [`crate::pool::recovery_breaker_fingerprint`] to avoid releasing another
/// connection's process-owned file locks by closing a raw file descriptor.
/// Reads are bounded by the observed length and refuse a changed inode,
/// timestamp, length, or pathname. An unstable observation is `unreadable`,
/// never a fabricated content generation or permission to reset the breaker.
#[must_use]
pub fn fingerprint_db(db_path: &Path) -> String {
    fingerprint_db_with_observer(db_path, |_, _| {})
}

fn fingerprint_db_with_observer(db_path: &Path, observed: impl FnMut(&Path, u64)) -> String {
    // Resolve relative paths once; a later process-wide chdir must not make
    // final verification consult a different file with the same basename.
    let path = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        let Ok(cwd) = std::env::current_dir() else {
            return "unreadable".to_string();
        };
        cwd.join(db_path)
    };
    #[cfg(unix)]
    {
        fingerprint_unix_family(&path, observed).unwrap_or_else(|_| "unreadable".to_string())
    }
    #[cfg(not(unix))]
    {
        fingerprint_non_unix_family(&path, observed).unwrap_or_else(|_| "unreadable".to_string())
    }
}

#[cfg(not(unix))]
struct NonUnixFingerprintMember {
    member_path: PathBuf,
    source: Option<(std::fs::File, std::fs::Metadata, same_file::Handle)>,
}

#[cfg(not(unix))]
fn fingerprint_non_unix_family(
    path: &Path,
    mut observed: impl FnMut(&Path, u64),
) -> std::io::Result<String> {
    const SUFFIXES: [&str; 8] = [
        "",
        "-journal",
        "-wal",
        "-shm",
        "-wal-cert",
        "-wal-cert-head",
        "-fsqlite-ns-gate",
        "-fsqlite-ns-use",
    ];
    let parent_path = path
        .parent()
        .ok_or_else(|| std::io::Error::other("fingerprint primary has no parent"))?;
    match std::fs::symlink_metadata(parent_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("missing".to_string());
        }
        Ok(metadata) if metadata.is_dir() => {}
        _ => {
            return Err(std::io::Error::other(
                "fingerprint parent is not a directory",
            ));
        }
    }
    let mut members = Vec::with_capacity(SUFFIXES.len());
    for suffix in SUFFIXES {
        let mut member_os = path.as_os_str().to_os_string();
        member_os.push(suffix);
        let member_path = PathBuf::from(member_os);
        let source = match std::fs::symlink_metadata(&member_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Ok(metadata) if metadata.is_file() => {
                let file = mcp_agent_mail_core::disk::open_regular_file_no_follow(&member_path)?;
                let before = file.metadata()?;
                if !before.is_file() {
                    return Err(std::io::Error::other(
                        "fingerprint family member is not a regular file",
                    ));
                }
                let identity = same_file::Handle::from_file(file.try_clone()?)?;
                Some((file, before, identity))
            }
            _ => {
                return Err(std::io::Error::other(
                    "fingerprint family member is not a regular file",
                ));
            }
        };
        members.push(NonUnixFingerprintMember {
            member_path,
            source,
        });
    }

    let mut family = sha2::Sha256::new();
    family.update(b"mcp-agent-mail-offline-family-v1\0");
    let mut main = "missing".to_string();
    let mut present = false;
    let mut buffer = vec![0_u8; 64 * 1024];
    for (suffix, member) in SUFFIXES.into_iter().zip(&mut members) {
        family.update((suffix.len() as u64).to_le_bytes());
        family.update(suffix.as_bytes());
        let Some((file, before, _)) = member.source.as_mut() else {
            family.update([0]);
            continue;
        };
        present = true;
        family.update([1]);
        family.update(before.len().to_le_bytes());
        let limit = before
            .len()
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("fingerprint member too large"))?;
        let mut reader = (&mut *file).take(limit);
        let mut content = sha2::Sha256::new();
        let mut read_bytes = 0_u64;
        observed(&member.member_path, 0);
        loop {
            let count = match reader.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                break;
            }
            read_bytes = read_bytes
                .checked_add(count as u64)
                .ok_or_else(|| std::io::Error::other("fingerprint length overflow"))?;
            content.update(&buffer[..count]);
            observed(&member.member_path, read_bytes);
        }
        if read_bytes != before.len() || !fingerprint_metadata_matches(before, &file.metadata()?) {
            return Err(std::io::Error::other(
                "fingerprint family changed while streaming",
            ));
        }
        let digest = content.finalize();
        if suffix.is_empty() {
            main = format!("{read_bytes}:{}", hex::encode(digest));
        }
        family.update(digest);
    }

    for member in &members {
        match (
            &member.source,
            std::fs::symlink_metadata(&member.member_path),
        ) {
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            (Some((file, before, identity)), Ok(_)) => {
                let current =
                    mcp_agent_mail_core::disk::open_regular_file_no_follow(&member.member_path)?;
                if !fingerprint_metadata_matches(before, &file.metadata()?)
                    || !fingerprint_metadata_matches(before, &current.metadata()?)
                    || same_file::Handle::from_file(current)? != *identity
                {
                    return Err(std::io::Error::other(
                        "fingerprint family changed after streaming",
                    ));
                }
            }
            _ => {
                return Err(std::io::Error::other(
                    "fingerprint family changed after streaming",
                ));
            }
        }
    }
    if present {
        Ok(format!("family1:{main}:{}", hex::encode(family.finalize())))
    } else {
        Ok("missing".to_string())
    }
}

/// Open the resolved parent component-by-component, retaining its directory
/// descriptor for every family lookup. Canonicalizing only the parent keeps
/// normal platform aliases usable without following a database/sidecar link.
#[cfg(unix)]
fn fingerprint_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    use std::path::Component;
    let canonical = std::fs::canonicalize(path)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = std::fs::File::from(rustix::fs::open("/", flags, Mode::empty())?);
    for component in canonical.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = std::fs::File::from(rustix::fs::openat(
                    &directory,
                    name,
                    flags,
                    Mode::empty(),
                )?);
            }
            _ => return Err(std::io::Error::other("invalid resolved fingerprint parent")),
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn fingerprint_member_file(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let file = std::fs::File::from(rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "fingerprint family member is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
struct FingerprintMember {
    leaf: std::ffi::OsString,
    source: Option<(std::fs::File, std::fs::Metadata)>,
}

#[cfg(unix)]
fn fingerprint_unix_family(
    path: &Path,
    mut observed: impl FnMut(&Path, u64),
) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt as _;
    // Match the recovery family's durable/coordination members, never its
    // breaker sidecar, snapshots, locks, or historical quarantine files.
    const SUFFIXES: [&str; 8] = [
        "",
        "-journal",
        "-wal",
        "-shm",
        "-wal-cert",
        "-wal-cert-head",
        "-fsqlite-ns-gate",
        "-fsqlite-ns-use",
    ];
    let parent_path = path
        .parent()
        .ok_or_else(|| std::io::Error::other("fingerprint primary has no parent"))?;
    let basename = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("fingerprint primary has no basename"))?;
    let parent = match fingerprint_directory(parent_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("missing".to_string());
        }
        result => result?,
    };
    let parent_identity = parent.metadata()?;
    let mut members = Vec::with_capacity(SUFFIXES.len());
    // Retain every present inode BEFORE reading any member. This prevents an
    // inode from being recycled during the multi-file observation and keeps
    // an earlier member's witness alive until the entire family is checked.
    for suffix in SUFFIXES {
        let mut leaf = basename.to_os_string();
        leaf.push(suffix);
        let source = match fingerprint_member_file(&parent, &leaf) {
            Ok(file) => {
                let metadata = file.metadata()?;
                Some((file, metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        members.push(FingerprintMember { leaf, source });
    }

    let mut family = sha2::Sha256::new();
    family.update(b"mcp-agent-mail-offline-family-v1\0");
    let mut main = "missing".to_string();
    let mut present = false;
    let mut buffer = vec![0_u8; 64 * 1024];
    for (suffix, member) in SUFFIXES.into_iter().zip(&mut members) {
        family.update((suffix.len() as u64).to_le_bytes());
        family.update(suffix.as_bytes());
        let Some((file, before)) = member.source.as_mut() else {
            family.update([0]);
            continue;
        };
        present = true;
        family.update([1]);
        family.update(before.len().to_le_bytes());
        family.update(before.dev().to_le_bytes());
        family.update(before.ino().to_le_bytes());
        let limit = before
            .len()
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("fingerprint member too large"))?;
        let mut reader = (&mut *file).take(limit);
        let mut content = sha2::Sha256::new();
        let mut read_bytes = 0_u64;
        let member_path = parent_path.join(&member.leaf);
        observed(&member_path, 0);
        loop {
            let count = match reader.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                break;
            }
            read_bytes = read_bytes
                .checked_add(count as u64)
                .ok_or_else(|| std::io::Error::other("fingerprint length overflow"))?;
            content.update(&buffer[..count]);
            observed(&member_path, read_bytes);
        }
        if read_bytes != before.len() || !fingerprint_metadata_matches(before, &file.metadata()?) {
            return Err(std::io::Error::other(
                "fingerprint family changed while streaming",
            ));
        }
        let digest = content.finalize();
        if suffix.is_empty() {
            main = format!("{read_bytes}:{}", hex::encode(digest));
        }
        family.update(digest);
    }

    // Crucially, recheck ALL members after ALL streams. Verifying a member
    // only when its own read finishes would accept main-old + WAL-new pairs.
    // Absence is part of the witness, not permission to ignore a new member.
    for member in &members {
        match (
            &member.source,
            fingerprint_member_file(&parent, &member.leaf),
        ) {
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            (Some((file, before)), Ok(current))
                if fingerprint_metadata_matches(before, &file.metadata()?)
                    && fingerprint_metadata_matches(before, &current.metadata()?) => {}
            _ => {
                return Err(std::io::Error::other(
                    "fingerprint family changed after streaming",
                ));
            }
        }
    }
    let current_parent = fingerprint_directory(parent_path)?.metadata()?;
    if current_parent.dev() != parent_identity.dev()
        || current_parent.ino() != parent_identity.ino()
    {
        return Err(std::io::Error::other("fingerprint parent was displaced"));
    }
    if present {
        Ok(format!("family1:{main}:{}", hex::encode(family.finalize())))
    } else {
        Ok("missing".to_string())
    }
}

fn fingerprint_metadata_matches(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if !before.is_file()
        || !after.is_file()
        || before.len() != after.len()
        || !before
            .modified()
            .is_ok_and(|modified| after.modified().ok() == Some(modified))
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.nlink() != after.nlink()
            || before.mode() != after.mode()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return false;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if before.creation_time() != after.creation_time()
            || before.file_attributes() != after.file_attributes()
        {
            return false;
        }
    }
    true
}

/// Load the durable sidecar.
///
/// Absence means there is no history. An occupied but unreadable, oversized,
/// malformed, or semantically invalid control file is an error: automatic
/// recovery must not reinterpret corrupted breaker authority as a clean slate.
pub fn load(db_path: &Path) -> std::io::Result<Option<RecoveryBreakerState>> {
    let path = breaker_sidecar_path(db_path);
    let bytes = match mcp_agent_mail_core::disk::read_regular_file_no_follow_bounded(
        &path,
        MAX_SIDECAR_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let state: RecoveryBreakerState = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid recovery-breaker JSON in {}: {error}",
                path.display()
            ),
        )
    })?;
    if !recovery_breaker_state_is_semantically_valid(&state) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "recovery-breaker state in {} violates the schema-1 semantic contract",
                path.display()
            ),
        ));
    }
    Ok(Some(state))
}

fn canonical_fingerprint_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn scalar_fingerprint_valid(fingerprint: &str) -> bool {
    matches!(fingerprint, "missing" | "unreadable")
        || fingerprint.split_once(':').is_some_and(|(len, hash)| {
            len.parse::<u64>()
                .is_ok_and(|parsed| parsed.to_string() == len)
                && canonical_fingerprint_hash(hash)
        })
}

fn family_fingerprint_parts(fingerprint: &str) -> Option<(&str, &str)> {
    let (main, family_hash) = fingerprint.strip_prefix("family1:")?.rsplit_once(':')?;
    (main != "unreadable"
        && scalar_fingerprint_valid(main)
        && canonical_fingerprint_hash(family_hash))
    .then_some((main, family_hash))
}

fn recovery_breaker_state_is_semantically_valid(state: &RecoveryBreakerState) -> bool {
    let fingerprint_is_valid = scalar_fingerprint_valid(&state.db_fingerprint)
        || family_fingerprint_parts(&state.db_fingerprint).is_some();
    state.schema == 1
        && fingerprint_is_valid
        && state.last_failure_unix >= 0
        && state.last_failure_reason.len() <= MAX_REASON_BYTES + '…'.len_utf8()
        && (!state.tripped || state.consecutive_failures > 0)
        && (!state.attempt_in_progress || state.consecutive_failures > 0)
}

/// Persist the sidecar atomically (write-tmp-then-rename).
///
/// Callers decide whether an operator bypass permits best-effort handling.
/// Automatic recovery treats every failure as an admission error so durable
/// circuit authority can never silently lag.
pub fn store(db_path: &Path, state: &RecoveryBreakerState) -> std::io::Result<()> {
    use std::io::Write as _;

    let path = breaker_sidecar_path(db_path);
    if !recovery_breaker_state_is_semantically_valid(state) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to persist semantically invalid recovery-breaker state",
        ));
    }
    let serialized = serde_json::to_vec_pretty(state).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cannot serialize recovery-breaker state: {error}"),
        )
    })?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !std::fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("parent {} is not a real directory", parent.display()),
        ));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            secure_existing_breaker_authority_file(&path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "destination {} is not a regular non-symlink file",
                    path.display()
                ),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut staged = tempfile::Builder::new()
        .prefix(".recovery-breaker-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    mcp_agent_mail_core::disk::set_private_writable_file_permissions(staged.as_file())?;
    staged.write_all(&serialized)?;
    staged.as_file().sync_all()?;

    // Recheck immediately before publish. Replacing a raced-in symlink would
    // destroy that directory entry, so refuse every non-regular target.
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            secure_existing_breaker_authority_file(&path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("destination {} changed type", path.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let persisted = staged.persist(&path).map_err(|error| error.error)?;
    persisted.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(parent).and_then(|directory| directory.sync_all())?;
    Ok(())
}

std::thread_local! {
    static BREAKER_BYPASS_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard operator-invoked recovery surfaces (doctor repair /
/// reconstruct / reset) hold so the breaker never refuses an explicit
/// operator action. Success/failure are still recorded while bypassed.
pub struct RecoveryBreakerBypassGuard;

impl RecoveryBreakerBypassGuard {
    #[must_use]
    pub fn enter() -> Self {
        BREAKER_BYPASS_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }

    #[must_use]
    pub fn is_active() -> bool {
        BREAKER_BYPASS_DEPTH.with(|depth| depth.get() > 0)
    }
}

impl Drop for RecoveryBreakerBypassGuard {
    fn drop(&mut self) {
        BREAKER_BYPASS_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: RecoveryBreakerConfig = RecoveryBreakerConfig {
        max_consecutive_failures: 3,
        cooldown_secs: 100,
    };

    #[test]
    fn parse_positive_u64_never_disables() {
        assert_eq!(parse_positive_u64(None, 9), 9);
        assert_eq!(parse_positive_u64(Some(""), 9), 9);
        assert_eq!(parse_positive_u64(Some("junk"), 9), 9);
        assert_eq!(parse_positive_u64(Some("0"), 9), 9);
        assert_eq!(parse_positive_u64(Some(" 4 "), 9), 4);
    }

    #[test]
    fn failures_accumulate_and_trip_at_threshold() {
        let first = record_failure(None, "fp", "boom", CFG, 1_000);
        assert_eq!(first.consecutive_failures, 1);
        assert!(!first.tripped);
        let second = record_failure(Some(&first), "fp", "boom", CFG, 1_010);
        assert!(!second.tripped);
        let third = record_failure(Some(&second), "fp", "boom", CFG, 1_020);
        assert_eq!(third.consecutive_failures, 3);
        assert!(third.tripped, "threshold reached must trip");
    }

    #[test]
    fn fingerprint_change_restarts_the_count() {
        let tripped = record_failure(
            Some(&record_failure(
                Some(&record_failure(None, "fp-a", "x", CFG, 1)),
                "fp-a",
                "x",
                CFG,
                2,
            )),
            "fp-a",
            "x",
            CFG,
            3,
        );
        assert!(tripped.tripped);
        // Different content: evaluate allows, and a failure restarts at 1.
        assert_eq!(
            evaluate(Some(&tripped), "fp-b", CFG, 4),
            BreakerVerdict::Allow
        );
        let fresh = record_failure(Some(&tripped), "fp-b", "y", CFG, 5);
        assert_eq!(fresh.consecutive_failures, 1);
        assert!(!fresh.tripped);
    }

    #[test]
    fn unfinished_attempt_retains_changed_content_history_and_cooldown() {
        let first = record_attempt(None, "fp-a", CFG, 1_000);
        let second = record_attempt(Some(&first), "fp-b", CFG, 1_010);
        let third = record_attempt(Some(&second), "fp-c", CFG, 1_020);
        assert!(third.attempt_in_progress);
        assert_eq!(third.consecutive_failures, 3);
        assert!(third.tripped);
        assert!(matches!(
            evaluate(Some(&third), "fp-d", CFG, 1_030),
            BreakerVerdict::Refuse {
                consecutive_failures: 3,
                ..
            }
        ));
        assert_eq!(
            evaluate(Some(&third), "fp-d", CFG, 1_120),
            BreakerVerdict::AllowHalfOpen
        );

        let completed = record_failure(Some(&third), "fp-d", "terminal failure", CFG, 1_120);
        assert_eq!(completed.consecutive_failures, 4);
        assert!(!completed.attempt_in_progress);
        assert_eq!(
            evaluate(Some(&completed), "operator-replacement", CFG, 1_121),
            BreakerVerdict::Allow
        );
        assert!(!cleared_state("fp-d").attempt_in_progress);
    }

    #[test]
    fn unfinished_attempt_without_a_count_is_invalid_authority() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let mut invalid = cleared_state(&fingerprint_db(&db));
        invalid.attempt_in_progress = true;
        assert_eq!(
            store(&db, &invalid).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        std::fs::write(
            breaker_sidecar_path(&db),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();
        assert_eq!(
            load(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn tripped_refuses_within_cooldown_and_half_opens_after() {
        let mut state = record_failure(None, "fp", "x", CFG, 0);
        state = record_failure(Some(&state), "fp", "x", CFG, 0);
        state = record_failure(Some(&state), "fp", "x", CFG, 1_000);
        assert!(state.tripped);
        match evaluate(Some(&state), "fp", CFG, 1_050) {
            BreakerVerdict::Refuse {
                consecutive_failures,
                retry_after_secs,
                ..
            } => {
                assert_eq!(consecutive_failures, 3);
                assert_eq!(retry_after_secs, 50);
            }
            other => panic!("expected refuse within cooldown, got {other:?}"),
        }
        assert_eq!(
            evaluate(Some(&state), "fp", CFG, 1_100),
            BreakerVerdict::AllowHalfOpen,
            "cooldown elapsed must admit a half-open probe"
        );
        // A failed half-open probe re-trips with a fresh window.
        let retripped = record_failure(Some(&state), "fp", "x", CFG, 1_100);
        assert!(retripped.tripped);
        assert_eq!(retripped.consecutive_failures, 4);
        assert!(matches!(
            evaluate(Some(&retripped), "fp", CFG, 1_150),
            BreakerVerdict::Refuse { .. }
        ));
    }

    #[test]
    fn future_failure_timestamp_refuses_without_overflow() {
        let state = RecoveryBreakerState {
            schema: 1,
            db_fingerprint: "fp".to_string(),
            consecutive_failures: 3,
            last_failure_unix: i64::MAX,
            last_failure_reason: "future clock".to_string(),
            tripped: true,
            attempt_in_progress: false,
        };
        assert_eq!(
            evaluate(Some(&state), "fp", CFG, 1_000),
            BreakerVerdict::Refuse {
                consecutive_failures: 3,
                retry_after_secs: CFG.cooldown_secs,
                last_failure_reason: "future clock".to_string(),
            }
        );
    }

    #[test]
    fn lowering_threshold_never_grants_an_extra_attempt() {
        let state = RecoveryBreakerState {
            schema: 1,
            db_fingerprint: "fp".to_string(),
            consecutive_failures: 2,
            last_failure_unix: 1_000,
            last_failure_reason: "failed twice".to_string(),
            tripped: false,
            attempt_in_progress: false,
        };
        let lowered = RecoveryBreakerConfig {
            max_consecutive_failures: 2,
            cooldown_secs: 100,
        };
        assert!(matches!(
            evaluate(Some(&state), "fp", lowered, 1_050),
            BreakerVerdict::Refuse {
                consecutive_failures: 2,
                ..
            }
        ));
    }

    #[test]
    fn cleared_state_allows_and_round_trips_through_sidecar() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let fingerprint = fingerprint_db(&db);

        assert!(load(&db).unwrap().is_none(), "no sidecar yet");
        let state = record_failure(None, &fingerprint, "boom", CFG, 42);
        store(&db, &state).unwrap();
        assert_eq!(load(&db).unwrap().as_ref(), Some(&state));

        store(&db, &cleared_state(&fingerprint)).unwrap();
        let cleared = load(&db)
            .expect("load cleared sidecar")
            .expect("cleared sidecar persists");
        assert!(!cleared.tripped);
        assert_eq!(cleared.consecutive_failures, 0);
        assert_eq!(
            evaluate(Some(&cleared), &fingerprint, CFG, 100),
            BreakerVerdict::Allow
        );
    }

    #[test]
    fn sidecar_name_cannot_be_mistaken_for_backup_or_sidecar_artifacts() {
        for path in [
            breaker_sidecar_path(Path::new("/x/storage.sqlite3")),
            breaker_lock_path(Path::new("/x/storage.sqlite3")),
        ] {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(
                mcp_agent_mail_core::disk::classify_sqlite_recovery_candidate_name(
                    std::ffi::OsStr::new("storage.sqlite3"),
                    path.file_name().unwrap(),
                )
                .is_none(),
                "breaker control state must not match any published recovery-candidate grammar"
            );
            for forbidden_suffix in [
                "-journal",
                "-wal",
                "-shm",
                "-wal-cert",
                "-wal-cert-head",
                "-fsqlite-ns-gate",
                "-fsqlite-ns-use",
            ] {
                assert!(!name.ends_with(forbidden_suffix));
            }
        }
    }

    #[test]
    fn breaker_file_lock_is_exclusive_across_processes_and_releases_on_exit() {
        const CHILD_MODE: &str = "AM_TEST_RECOVERY_BREAKER_LOCK_CHILD";
        const DB_PATH: &str = "AM_TEST_RECOVERY_BREAKER_LOCK_DB";
        const READY_PATH: &str = "AM_TEST_RECOVERY_BREAKER_LOCK_READY";
        const RELEASE_PATH: &str = "AM_TEST_RECOVERY_BREAKER_LOCK_RELEASE";

        if std::env::var_os(CHILD_MODE).is_some() {
            let db = PathBuf::from(std::env::var_os(DB_PATH).expect("child DB path"));
            let ready = PathBuf::from(std::env::var_os(READY_PATH).expect("child ready path"));
            let release =
                PathBuf::from(std::env::var_os(RELEASE_PATH).expect("child release path"));
            let _owner = try_acquire_file_lock(&db).expect("child must acquire breaker lock");
            std::fs::write(&ready, b"ready").expect("publish child readiness");
            for _ in 0..500 {
                if release.exists() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("parent never released the breaker-lock child");
        }

        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        let ready = td.path().join("holder-ready");
        let release = td.path().join("holder-release");
        std::fs::write(&db, b"content").unwrap();

        let test_name = "recovery_breaker::tests::breaker_file_lock_is_exclusive_across_processes_and_releases_on_exit";
        let mut holder = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg(test_name)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_MODE, "holder")
            .env(DB_PATH, &db)
            .env(READY_PATH, &ready)
            .env(RELEASE_PATH, &release)
            .spawn()
            .expect("spawn breaker-lock holder");
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            if let Some(status) = holder.try_wait().expect("poll holder") {
                panic!("breaker-lock holder exited before readiness: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !ready.exists() {
            let _ = std::fs::write(&release, b"release after readiness timeout");
            let status = holder.wait().expect("wait for timed-out holder");
            panic!("breaker-lock holder did not become ready; exit status: {status}");
        }

        let contender = try_acquire_file_lock(&db)
            .map(|_unexpected_owner| ())
            .map_err(|error| (error.raw_os_error(), error.to_string()));
        std::fs::write(&release, b"release").expect("release holder");
        let status = holder.wait().expect("wait for breaker-lock holder");
        assert!(status.success(), "breaker-lock holder failed: {status}");
        let (contender_code, contender_message) =
            contender.expect_err("a second process must not enter the breaker transition");
        assert_eq!(
            contender_code,
            fs2::lock_contended_error().raw_os_error(),
            "unexpected lock error: {contender_message}"
        );
        try_acquire_file_lock(&db).expect("lock must release when the owner process exits");
    }

    #[cfg(unix)]
    #[test]
    fn breaker_file_lock_normalizes_existing_permissions_before_locking() {
        use std::os::unix::fs::PermissionsExt as _;

        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        let lock = breaker_lock_path(&db);
        std::fs::write(&lock, b"").expect("plant lock file");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666))
            .expect("make lock file permissive");

        let _owner = try_acquire_file_lock(&db).expect("acquire and secure lock file");

        assert_eq!(
            std::fs::symlink_metadata(&lock)
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "an existing lock authority file must not retain group/world access"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_breaker_lock_refuses_replaced_or_aliased_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let db = root.join("storage.sqlite3");
        let lock = breaker_lock_path(&db);
        let owner = try_acquire_file_lock(&db).unwrap();
        owner.revalidate().unwrap();
        let displaced = root.join("displaced-lock");
        std::fs::rename(&lock, &displaced).unwrap();
        let replacement_owner = try_acquire_file_lock(&db).unwrap();
        assert!(owner.revalidate().is_err());
        replacement_owner.revalidate().unwrap();
        std::fs::hard_link(&lock, root.join("aliased-lock")).unwrap();
        assert!(replacement_owner.revalidate().is_err());
        assert!(displaced.exists());
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_breaker_authority_is_rejected_before_permission_changes() {
        use std::os::unix::fs::PermissionsExt as _;

        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").expect("write database marker");
        let state = cleared_state(&fingerprint_db(&db));

        let lock_sentinel = td.path().join("lock-sentinel");
        std::fs::write(&lock_sentinel, b"lock evidence").expect("write lock sentinel");
        std::fs::set_permissions(&lock_sentinel, std::fs::Permissions::from_mode(0o400))
            .expect("make lock sentinel read-only");
        std::fs::hard_link(&lock_sentinel, breaker_lock_path(&db))
            .expect("plant hard-linked lock authority");

        let error = try_acquire_file_lock(&db)
            .expect_err("multiply-linked lock authority must fail closed");
        assert!(error.to_string().contains("hard links"));
        assert_eq!(std::fs::read(&lock_sentinel).unwrap(), b"lock evidence");
        assert_eq!(
            std::fs::symlink_metadata(&lock_sentinel)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );

        let state_sentinel = td.path().join("state-sentinel");
        std::fs::write(&state_sentinel, b"state evidence").expect("write state sentinel");
        std::fs::set_permissions(&state_sentinel, std::fs::Permissions::from_mode(0o400))
            .expect("make state sentinel read-only");
        std::fs::hard_link(&state_sentinel, breaker_sidecar_path(&db))
            .expect("plant hard-linked state authority");

        let error =
            store(&db, &state).expect_err("multiply-linked state authority must fail closed");
        assert!(error.to_string().contains("hard links"));
        assert_eq!(std::fs::read(&state_sentinel).unwrap(), b"state evidence");
        assert_eq!(
            std::fs::symlink_metadata(&state_sentinel)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_hard_linked_breaker_authority_preserves_sentinels() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let state = cleared_state(&fingerprint_db(&db));

        for (authority, sentinel, contents) in [
            (
                breaker_lock_path(&db),
                td.path().join("lock-sentinel"),
                b"lock evidence".as_slice(),
            ),
            (
                breaker_sidecar_path(&db),
                td.path().join("state-sentinel"),
                b"state evidence".as_slice(),
            ),
        ] {
            std::fs::write(&sentinel, contents).unwrap();
            std::fs::hard_link(&sentinel, &authority).unwrap();
            let mut permissions = std::fs::metadata(&sentinel).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&sentinel, permissions).unwrap();

            let error = if authority == breaker_lock_path(&db) {
                try_acquire_file_lock(&db).unwrap_err()
            } else {
                store(&db, &state).unwrap_err()
            };
            assert!(error.to_string().contains("hard links"), "{error}");
            assert!(
                std::fs::metadata(&sentinel)
                    .unwrap()
                    .permissions()
                    .readonly()
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), contents);
            assert_eq!(std::fs::read(&authority).unwrap(), contents);
        }
    }

    #[test]
    fn unknown_sidecar_schema_is_not_authoritative() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let mut state = cleared_state(&fingerprint_db(&db));
        state.schema = 99;
        std::fs::write(
            breaker_sidecar_path(&db),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        assert_eq!(
            load(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn negative_failure_timestamp_is_rejected_as_invalid_authority() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let mut state = cleared_state(&fingerprint_db(&db));
        state.last_failure_unix = -1;
        std::fs::write(
            breaker_sidecar_path(&db),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        assert_eq!(
            load(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn store_rejects_invalid_state_without_replacing_prior_authority() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let valid = cleared_state(&fingerprint_db(&db));
        store(&db, &valid).expect("store valid authority");
        let sidecar = breaker_sidecar_path(&db);
        let before = std::fs::read(&sidecar).expect("read prior authority");

        let mut invalid = valid;
        invalid.last_failure_unix = -1;
        assert_eq!(
            store(&db, &invalid).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            std::fs::read(&sidecar).expect("read preserved authority"),
            before,
            "invalid state must not replace the last trustworthy sidecar"
        );
    }

    #[test]
    fn store_recovers_a_read_only_regular_sidecar() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let sidecar = breaker_sidecar_path(&db);
        let state = cleared_state(&fingerprint_db(&db));
        store(&db, &state).expect("initial sidecar");

        let mut permissions = std::fs::symlink_metadata(&sidecar)
            .expect("sidecar metadata")
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&sidecar, permissions).expect("make sidecar read-only");

        store(&db, &state).expect("replace read-only authority atomically");

        assert!(
            !std::fs::symlink_metadata(&sidecar)
                .expect("replaced sidecar metadata")
                .permissions()
                .readonly(),
            "the replacement authority must be writable for future transitions"
        );
        assert_eq!(load(&db).expect("load sidecar"), Some(state));
    }

    #[test]
    fn noncanonical_fingerprint_spelling_is_rejected() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let mut state = cleared_state(&fingerprint_db(&db));
        state.db_fingerprint = format!("01:{}", "A".repeat(64));

        assert_eq!(
            store(&db, &state).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn malformed_sidecar_is_an_error_not_empty_history() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        std::fs::write(breaker_sidecar_path(&db), b"{ definitely not JSON").unwrap();

        assert_eq!(
            load(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn oversized_sidecar_is_not_loaded() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        std::fs::write(
            breaker_sidecar_path(&db),
            vec![b' '; usize::try_from(MAX_SIDECAR_BYTES + 1).unwrap()],
        )
        .unwrap();

        assert_eq!(
            load(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_and_database_are_never_followed() {
        use std::os::unix::fs::symlink;

        let td = tempfile::tempdir().expect("tempdir");
        let target_db = td.path().join("target.sqlite3");
        let linked_db = td.path().join("linked.sqlite3");
        std::fs::write(&target_db, b"target content").unwrap();
        symlink(&target_db, &linked_db).unwrap();
        assert_eq!(fingerprint_db(&linked_db), "unreadable");

        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let sentinel = td.path().join("breaker-sentinel.json");
        std::fs::write(&sentinel, b"operator evidence").unwrap();
        symlink(&sentinel, breaker_sidecar_path(&db)).unwrap();
        store(&db, &cleared_state(&fingerprint_db(&db)))
            .expect_err("symlinked breaker state must not be replaced");

        assert!(load(&db).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"operator evidence");
    }

    #[cfg(unix)]
    #[test]
    fn family_fingerprint_observes_real_peer_wal_commit_without_opening_sqlite() {
        const CHILD: &str = "AM_TEST_FAMILY_WAL_WRITER";
        fn wait_marker(path: &Path) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !path.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out awaiting {}",
                    path.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        if let Some(path) = std::env::var_os(CHILD) {
            let db = PathBuf::from(path);
            let root = db.parent().unwrap();
            let conn = crate::CanonicalDbConn::open_file(db.to_str().unwrap()).unwrap();
            conn.execute_raw("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; CREATE TABLE observed (id INTEGER PRIMARY KEY);").unwrap();
            conn.execute_raw("INSERT INTO observed VALUES (1)").unwrap();
            conn.query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[])
                .unwrap();
            conn.execute_raw("INSERT INTO observed VALUES (2)").unwrap();
            std::fs::write(root.join("ready-1"), b"ready").unwrap();
            wait_marker(&root.join("advance"));
            conn.execute_raw("INSERT INTO observed VALUES (3)").unwrap();
            let rows = conn
                .query_sync("SELECT COUNT(*) AS n FROM observed", &[])
                .unwrap();
            assert_eq!(rows[0].get_named::<i64>("n").unwrap(), 3);
            std::fs::write(root.join("ready-2"), b"ready").unwrap();
            wait_marker(&root.join("finish"));
            return;
        }
        struct Peer(std::process::Child);
        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let db = root.join("db");
        let name = std::thread::current().name().unwrap().to_string();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, &db)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let mut peer = Peer(child);
        wait_marker(&root.join("ready-1"));
        let main_bytes = std::fs::read(&db).unwrap();
        let wal = crate::pool::sqlite_path_with_suffix(&db, "-wal");
        let wal_before = std::fs::read(&wal).unwrap();
        assert!(
            wal_before.len() > 32,
            "fixture requires committed WAL frames"
        );
        let before = fingerprint_db(&db);
        assert!(family_fingerprint_parts(&before).is_some());
        let mut injected = false;
        let unstable = fingerprint_db_with_observer(&db, |path, count| {
            if path != wal || count != 0 {
                return;
            }
            injected = true;
            std::fs::write(root.join("advance"), b"commit now").unwrap();
            wait_marker(&root.join("ready-2"));
        });
        assert!(injected);
        assert_eq!(unstable, "unreadable");
        let after = fingerprint_db(&db);
        assert!(family_fingerprint_parts(&after).is_some());
        assert_ne!(
            before, after,
            "WAL-only commit did not change family identity"
        );
        assert_eq!(
            std::fs::read(&db).unwrap(),
            main_bytes,
            "fixture unexpectedly checkpointed the main file"
        );
        assert_ne!(std::fs::read(&wal).unwrap(), wal_before);
        assert!(
            peer.0.try_wait().unwrap().is_none(),
            "peer must still own its live connection"
        );
        let mut state = record_failure(None, &before, "old family failure", CFG, 1_000);
        for _ in 0..2 {
            state = record_failure(Some(&state), &before, "old family failure", CFG, 1_000);
        }
        assert_eq!(
            evaluate(Some(&state), &after, CFG, 1_001),
            BreakerVerdict::Allow
        );
        state.attempt_in_progress = true;
        assert!(matches!(
            evaluate(Some(&state), &after, CFG, 1_001),
            BreakerVerdict::Refuse { .. }
        ));
        std::fs::write(root.join("finish"), b"finish").unwrap();
        assert!(peer.0.wait().unwrap().success());
    }

    #[test]
    fn family_fingerprint_format_transition_preserves_existing_failure_authority() {
        let main = format!("4:{}", "a".repeat(64));
        let changed_main = format!("4:{}", "b".repeat(64));
        let family = format!("family1:{main}:{}", "c".repeat(64));
        let other_family = format!("family1:{main}:{}", "d".repeat(64));
        let mut old = record_failure(None, &main, "old failure", CFG, 1_000);
        for _ in 0..2 {
            old = record_failure(Some(&old), &main, "old failure", CFG, 1_000);
        }
        assert!(matches!(
            evaluate(Some(&old), &family, CFG, 1_001),
            BreakerVerdict::Refuse { .. }
        ));
        assert_eq!(
            record_failure(Some(&old), &family, "still failed", CFG, 1_001).consecutive_failures,
            4
        );
        assert_eq!(
            evaluate(
                Some(&old),
                &format!("family1:{changed_main}:{}", "c".repeat(64)),
                CFG,
                1_001
            ),
            BreakerVerdict::Allow
        );
        let mut upgraded = old.clone();
        upgraded.db_fingerprint.clone_from(&family);
        assert_eq!(
            evaluate(Some(&upgraded), &other_family, CFG, 1_001),
            BreakerVerdict::Allow
        );
        upgraded.attempt_in_progress = true;
        assert!(matches!(
            evaluate(Some(&upgraded), &other_family, CFG, 1_001),
            BreakerVerdict::Refuse { .. }
        ));
        old.db_fingerprint = format!("4:fdc10ce1{}", "e".repeat(56));
        assert!(matches!(
            evaluate(Some(&old), &family, CFG, 1_001),
            BreakerVerdict::Refuse { .. }
        ));

        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("db");
        let valid = cleared_state(&family);
        store(&db, &valid).unwrap();
        let before = std::fs::read(breaker_sidecar_path(&db)).unwrap();
        for invalid in [
            format!("family2:{main}:{}", "c".repeat(64)),
            format!("family1:unreadable:{}", "c".repeat(64)),
            format!("family1:04:{}:{}", "a".repeat(64), "c".repeat(64)),
            format!("family1:{main}:{}", "C".repeat(64)),
            format!("family1:{main}:{}", "c".repeat(63)),
            format!("family1:missing:extra:{}", "c".repeat(64)),
        ] {
            let mut state = valid.clone();
            state.db_fingerprint = invalid;
            assert!(store(&db, &state).is_err());
            assert_eq!(std::fs::read(breaker_sidecar_path(&db)).unwrap(), before);
        }
        assert_eq!(load(&db).unwrap(), Some(valid));
    }

    #[cfg(unix)]
    #[test]
    fn family_fingerprint_refuses_unsafe_members_and_parent_displacement() {
        for shape in ["symlink", "directory", "fifo"] {
            let td = tempfile::tempdir().unwrap();
            let db = td.path().join("db");
            let wal = crate::pool::sqlite_path_with_suffix(&db, "-wal");
            let sentinel = td.path().join("unrelated");
            std::fs::write(&db, b"primary evidence").unwrap();
            std::fs::write(&sentinel, b"unrelated evidence").unwrap();
            match shape {
                "symlink" => std::os::unix::fs::symlink(&sentinel, &wal).unwrap(),
                "directory" => std::fs::create_dir(&wal).unwrap(),
                "fifo" => nix::unistd::mkfifo(
                    &wal,
                    nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
                )
                .unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(fingerprint_db(&db), "unreadable", "{shape}");
            assert_eq!(std::fs::read(&db).unwrap(), b"primary evidence");
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"unrelated evidence");
            assert!(std::fs::symlink_metadata(&wal).is_ok());
        }
        let td = tempfile::tempdir().unwrap();
        let parent = td.path().join("mailbox");
        std::fs::create_dir(&parent).unwrap();
        let db = parent.join("db");
        std::fs::write(&db, b"primary evidence").unwrap();
        let retained = td.path().join("retained-mailbox");
        let mut changed = false;
        let result = fingerprint_db_with_observer(&db, |_, count| {
            if count != 0 {
                return;
            }
            changed = true;
            std::fs::rename(&parent, &retained).unwrap();
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(&db, b"replacement evidence").unwrap();
        });
        assert!(changed);
        assert_eq!(result, "unreadable");
        assert_eq!(
            std::fs::read(retained.join("db")).unwrap(),
            b"primary evidence"
        );
        assert_eq!(std::fs::read(&db).unwrap(), b"replacement evidence");
    }

    #[cfg(unix)]
    #[test]
    fn family_fingerprint_covers_every_sidecar_and_missing_primary() {
        for suffix in [
            "-journal",
            "-wal",
            "-shm",
            "-wal-cert",
            "-wal-cert-head",
            "-fsqlite-ns-gate",
            "-fsqlite-ns-use",
        ] {
            for main_present in [false, true] {
                let td = tempfile::tempdir().unwrap();
                let db = td.path().join("db");
                if main_present {
                    std::fs::write(&db, b"main").unwrap();
                }
                let baseline = fingerprint_db(&db);
                let member = crate::pool::sqlite_path_with_suffix(&db, suffix);
                std::fs::write(&member, b"AAAA").unwrap();
                let first = fingerprint_db(&db);
                assert_ne!(first, baseline, "{suffix}: sidecar existence was omitted");
                assert_ne!(first, "unreadable");
                assert_eq!(first, fingerprint_db(&db));
                std::fs::write(&member, b"BBBB").unwrap();
                let changed = fingerprint_db(&db);
                assert_ne!(
                    first, changed,
                    "{suffix}: same-length sidecar edits were omitted"
                );
                let mut tripped = record_failure(None, &first, "failure", CFG, 1_000);
                for _ in 0..2 {
                    tripped = record_failure(Some(&tripped), &first, "failure", CFG, 1_000);
                }
                assert_eq!(
                    evaluate(Some(&tripped), &changed, CFG, 1_001),
                    BreakerVerdict::Allow
                );
                store(&db, &tripped).unwrap();
                assert_eq!(load(&db).unwrap(), Some(tripped));
                assert_eq!(
                    changed,
                    fingerprint_db(&db),
                    "breaker sidecar fed its own hash"
                );
                std::fs::rename(&member, td.path().join("retained-sidecar")).unwrap();
                assert_eq!(fingerprint_db(&db), baseline);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn family_fingerprint_rechecks_earlier_members_and_missing_entries() {
        for change in [
            "main-after-read",
            "sidecar-after-read",
            "new-sidecar",
            "missing-main",
        ] {
            let td = tempfile::tempdir().unwrap();
            let db = td.path().join("db");
            let wal = crate::pool::sqlite_path_with_suffix(&db, "-wal");
            let journal = crate::pool::sqlite_path_with_suffix(&db, "-journal");
            if change != "missing-main" {
                std::fs::write(&db, b"AAAA").unwrap();
            }
            std::fs::write(&wal, b"WWWW").unwrap();
            std::fs::write(&journal, b"JJJJ").unwrap();
            let mut injected = false;
            let result = fingerprint_db_with_observer(&db, |path, read| {
                if path != wal || read != 0 {
                    return;
                }
                injected = true;
                match change {
                    "main-after-read" => std::fs::write(&db, b"BBBB").unwrap(),
                    "sidecar-after-read" => std::fs::write(&journal, b"KKKK").unwrap(),
                    "new-sidecar" => std::fs::write(
                        crate::pool::sqlite_path_with_suffix(&db, "-wal-cert"),
                        b"new certificate",
                    )
                    .unwrap(),
                    "missing-main" => std::fs::write(&db, b"new main").unwrap(),
                    _ => unreachable!(),
                }
            });
            assert!(injected, "{change}: sidecar reads were never exercised");
            assert_eq!(result, "unreadable", "{change} accepted a mixed family");
        }
    }

    #[cfg(unix)]
    #[test]
    fn family_fingerprint_distinguishes_replaced_sidecar_inode() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("db");
        let wal = crate::pool::sqlite_path_with_suffix(&db, "-wal");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"identical bytes").unwrap();
        let before = fingerprint_db(&db);
        let metadata = std::fs::metadata(&wal).unwrap();
        std::fs::rename(&wal, td.path().join("retained-wal")).unwrap();
        std::fs::write(&wal, b"identical bytes").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&wal)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        assert_ne!(
            before,
            fingerprint_db(&db),
            "a replacement family reused the old generation identity"
        );
    }

    #[test]
    fn fingerprint_read_work_is_bounded_by_the_initial_length() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("growing-db");
        let initial = 64 * 1024_u64;
        std::fs::write(&db, vec![0; usize::try_from(initial).unwrap()]).unwrap();
        let mut largest_read = 0;
        let mut extensions = 0;
        let result = fingerprint_db_with_observer(&db, |path, bytes| {
            largest_read = largest_read.max(bytes);
            if extensions < 4 {
                extensions += 1;
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_len(initial * (extensions + 1))
                    .unwrap();
            }
        });
        assert_eq!(result, "unreadable");
        assert_eq!(
            largest_read,
            initial + 1,
            "the reader followed growth instead of its captured bound"
        );
        assert_eq!(extensions, 3);
    }

    #[test]
    fn fingerprint_stable_vectors_preserve_main_content_identity() {
        let td = tempfile::tempdir().unwrap();
        for (name, bytes, expected) in [
            (
                "empty",
                b"".as_slice(),
                "0:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                b"abc".as_slice(),
                "3:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ] {
            let db = td.path().join(name);
            std::fs::write(&db, bytes).unwrap();
            let fingerprint = fingerprint_db(&db);
            let main = family_fingerprint_parts(&fingerprint)
                .map_or(fingerprint.as_str(), |(main, _)| main);
            assert_eq!(main, expected);
            let state = record_failure(None, &fingerprint, "failure", CFG, 1_000);
            store(&db, &state).unwrap();
            assert_eq!(
                fingerprint_db(&db),
                fingerprint,
                "control state changed content identity"
            );
            assert_eq!(load(&db).unwrap(), Some(state));
        }
    }

    #[test]
    fn fingerprint_uncertainty_preserves_failure_lineage() {
        let mut tripped = record_failure(None, "stable-generation", "failure", CFG, 1_000);
        for _ in 0..2 {
            tripped = record_failure(Some(&tripped), "stable-generation", "failure", CFG, 1_000);
        }
        assert!(
            matches!(
                evaluate(Some(&tripped), "unreadable", CFG, 1_001),
                BreakerVerdict::Refuse { .. }
            ),
            "an unreadable generation is not proof of a repaired database"
        );
        let failed = record_failure(Some(&tripped), "unreadable", "cannot observe", CFG, 1_001);
        assert_eq!(failed.consecutive_failures, 4);
        assert!(matches!(
            evaluate(Some(&failed), "readable-again", CFG, 1_002),
            BreakerVerdict::Refuse { .. }
        ));
        assert_eq!(
            evaluate(Some(&failed), "readable-again", CFG, 1_101),
            BreakerVerdict::AllowHalfOpen
        );
        assert_eq!(
            evaluate(
                Some(&cleared_state("unreadable")),
                "readable-again",
                CFG,
                1_002
            ),
            BreakerVerdict::Allow
        );
    }

    #[cfg(unix)]
    #[test]
    fn fingerprint_refuses_mutation_during_streaming_even_with_restored_mtime() {
        for mutation in ["rewrite", "truncate", "grow", "replace", "symlink"] {
            let td = tempfile::tempdir().unwrap();
            let db = td.path().join("db");
            let bytes = vec![b'a'; 128 * 1024];
            std::fs::write(&db, &bytes).unwrap();
            let before = std::fs::metadata(&db).unwrap();
            let retained = td.path().join("retained-original");
            let fingerprint = fingerprint_db_with_observer(&db, |path, read| {
                if read != 64 * 1024 {
                    return;
                }
                match mutation {
                    "rewrite" => std::fs::write(path, vec![b'b'; bytes.len()]).unwrap(),
                    "truncate" => std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_len(64 * 1024)
                        .unwrap(),
                    "grow" => std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_len(192 * 1024)
                        .unwrap(),
                    "replace" => {
                        std::fs::rename(path, &retained).unwrap();
                        std::fs::write(path, &bytes).unwrap();
                    }
                    "symlink" => {
                        std::fs::rename(path, &retained).unwrap();
                        std::os::unix::fs::symlink(&retained, path).unwrap();
                    }
                    _ => unreachable!(),
                }
                if mutation != "symlink" {
                    std::fs::File::options()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_times(
                            std::fs::FileTimes::new().set_modified(before.modified().unwrap()),
                        )
                        .unwrap();
                }
            });
            assert_eq!(
                fingerprint, "unreadable",
                "{mutation} produced a mixed or displaced fingerprint"
            );
        }
    }

    #[test]
    fn fingerprints_discriminate_and_are_stable() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("db");
        std::fs::write(&db, b"aaaa").unwrap();
        let first = fingerprint_db(&db);
        assert_eq!(first, fingerprint_db(&db), "stable across reads");
        std::fs::write(&db, b"bbbb").unwrap();
        assert_ne!(first, fingerprint_db(&db), "content change must change it");
        assert_eq!(fingerprint_db(&td.path().join("absent")), "missing");
    }

    #[test]
    fn fingerprint_changes_for_same_length_tail_repair() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("db");
        let mut generation = vec![b'a'; 12 * 1024];
        std::fs::write(&db, &generation).unwrap();
        let first = fingerprint_db(&db);

        generation[10 * 1024] = b'b';
        std::fs::write(&db, &generation).unwrap();

        assert_ne!(
            first,
            fingerprint_db(&db),
            "a same-length repair beyond the first page must reset the breaker"
        );
    }

    #[test]
    fn overlong_failure_reasons_are_truncated() {
        let reason = "x".repeat(10_000);
        let state = record_failure(None, "fp", &reason, CFG, 1);
        assert!(state.last_failure_reason.len() <= MAX_REASON_BYTES + '…'.len_utf8());
    }

    #[test]
    fn bypass_guard_nests_and_clears() {
        assert!(!RecoveryBreakerBypassGuard::is_active());
        {
            let _outer = RecoveryBreakerBypassGuard::enter();
            assert!(RecoveryBreakerBypassGuard::is_active());
            {
                let _inner = RecoveryBreakerBypassGuard::enter();
                assert!(RecoveryBreakerBypassGuard::is_active());
            }
            assert!(RecoveryBreakerBypassGuard::is_active());
        }
        assert!(!RecoveryBreakerBypassGuard::is_active());
    }
}
