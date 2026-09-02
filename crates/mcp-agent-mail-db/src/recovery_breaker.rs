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
//! database content** (fingerprinted by size + full-file hash). Once the failure
//! count reaches the threshold the breaker trips, and automatic recovery
//! refuses fast — BEFORE any forensic capture — until either
//!
//! * the cooldown elapses (one half-open attempt is then admitted),
//! * the database content changes (operator replaced/quarantined it), or
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
    if state.db_fingerprint != fingerprint {
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
        Some(prev) if prev.db_fingerprint == fingerprint => {
            prev.consecutive_failures.saturating_add(1)
        }
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
    }
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
    _file: std::fs::File,
}

/// Try to become the sole process evaluating and updating this database's
/// breaker state. The lock is held across the admitted recovery operation and
/// terminal state write; process death releases it automatically.
pub(crate) fn try_acquire_file_lock(db_path: &Path) -> std::io::Result<RecoveryBreakerFileLock> {
    let path = breaker_lock_path(db_path);
    let file = open_breaker_lock_file(&path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(RecoveryBreakerFileLock { _file: file })
}

fn breaker_authority_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        Ok(file.metadata()?.nlink())
    }

    #[cfg(windows)]
    {
        // Stable Rust does not expose BY_HANDLE_FILE_INFORMATION's link count.
        // Keep the existing Windows breaker path available; a follow-up must
        // add a safe stable wrapper before this platform can reject hard links
        // as strictly as Unix does.
        let _ = file;
        Ok(1)
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

/// Content fingerprint of the database file: `len:sha256(bytes)`.
///
/// A missing file fingerprints as `missing`; an unreadable one as
/// `unreadable` (both stable, so failures on them still accumulate). Recovery
/// is exceptional and correctness matters more than the cost of hashing the
/// complete generation: a same-length repair beyond the first SQLite page must
/// reset the breaker.
#[must_use]
pub fn fingerprint_db(db_path: &Path) -> String {
    let mut file = match mcp_agent_mail_core::disk::open_regular_file_no_follow(db_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return "missing".to_string();
        }
        Err(_) => return "unreadable".to_string(),
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return "unreadable".to_string(),
        }
    }
    format!("{len}:{}", hex::encode(hasher.finalize()))
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

fn recovery_breaker_state_is_semantically_valid(state: &RecoveryBreakerState) -> bool {
    let fingerprint_is_valid = matches!(state.db_fingerprint.as_str(), "missing" | "unreadable")
        || state
            .db_fingerprint
            .split_once(':')
            .is_some_and(|(len, hash)| {
                len.parse::<u64>()
                    .is_ok_and(|parsed| parsed.to_string() == len)
                    && hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
    state.schema == 1
        && fingerprint_is_valid
        && state.last_failure_unix >= 0
        && state.last_failure_reason.len() <= MAX_REASON_BYTES + '…'.len_utf8()
        && (!state.tripped || state.consecutive_failures > 0)
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
            .map_err(|error| (error.kind(), error.to_string()));
        std::fs::write(&release, b"release").expect("release holder");
        let status = holder.wait().expect("wait for breaker-lock holder");
        assert!(status.success(), "breaker-lock holder failed: {status}");
        let (contender_kind, contender_message) =
            contender.expect_err("a second process must not enter the breaker transition");
        assert!(
            matches!(
                contender_kind,
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Other
            ),
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
