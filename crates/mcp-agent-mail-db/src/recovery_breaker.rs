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
//! database content** (fingerprinted by size + head hash). Once the failure
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

/// How many bytes of the database head participate in the content
/// fingerprint. 4 KiB covers the SQLite header + first page on default page
/// sizes; combined with the file length it is cheap and discriminating.
const FINGERPRINT_HEAD_BYTES: usize = 4096;

/// Cap stored failure reasons so the sidecar can never bloat.
const MAX_REASON_BYTES: usize = 512;

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
    let max_failures = parse_positive_u64(
        std::env::var("AM_RECOVERY_BREAKER_MAX_CONSECUTIVE_FAILURES")
            .ok()
            .as_deref(),
        u64::from(DEFAULT_MAX_CONSECUTIVE_FAILURES),
    );
    RecoveryBreakerConfig {
        max_consecutive_failures: u32::try_from(max_failures)
            .unwrap_or(DEFAULT_MAX_CONSECUTIVE_FAILURES),
        cooldown_secs: parse_positive_u64(
            std::env::var("AM_RECOVERY_BREAKER_COOLDOWN_SECS")
                .ok()
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
    if !state.tripped {
        return BreakerVerdict::Allow;
    }
    let elapsed = now_unix.saturating_sub(state.last_failure_unix);
    let cooldown = i64::try_from(config.cooldown_secs).unwrap_or(i64::MAX);
    if elapsed >= cooldown {
        return BreakerVerdict::AllowHalfOpen;
    }
    BreakerVerdict::Refuse {
        consecutive_failures: state.consecutive_failures,
        retry_after_secs: u64::try_from(cooldown - elapsed).unwrap_or(0),
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
/// The name deliberately does NOT begin with the `<db>.recovery` /
/// `<db>.bak` / `<db>.backup-` prefixes that `sqlite_backup_candidates`
/// scans, so the breaker file can never be mistaken for a restorable
/// backup, and it carries none of the `-wal`/`-shm`/`-journal` suffixes the
/// sidecar machinery classifies.
#[must_use]
pub fn breaker_sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(".am-recovery-breaker.json");
    PathBuf::from(name)
}

/// Content fingerprint of the database file: `len:sha256(head)`. A missing
/// file fingerprints as `missing`; an unreadable one as `unreadable` (both
/// stable, so failures on them still accumulate).
#[must_use]
pub fn fingerprint_db(db_path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(db_path) else {
        return if db_path.exists() {
            "unreadable".to_string()
        } else {
            "missing".to_string()
        };
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    let mut head = vec![0_u8; FINGERPRINT_HEAD_BYTES];
    let mut filled = 0_usize;
    loop {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if filled == head.len() {
                    break;
                }
            }
            Err(_) => return "unreadable".to_string(),
        }
    }
    let digest = sha2::Sha256::digest(&head[..filled]);
    format!("{len}:{}", hex::encode(digest))
}

/// Load the sidecar; any read/parse failure is treated as no history.
#[must_use]
pub fn load(db_path: &Path) -> Option<RecoveryBreakerState> {
    let bytes = std::fs::read(breaker_sidecar_path(db_path)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the sidecar atomically (write-tmp-then-rename). Best-effort:
/// failures are logged and never block or fail recovery itself.
pub fn store(db_path: &Path, state: &RecoveryBreakerState) {
    let path = breaker_sidecar_path(db_path);
    let Ok(serialized) = serde_json::to_vec_pretty(state) else {
        return;
    };
    let tmp = {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tmp");
        PathBuf::from(name)
    };
    let write_result =
        std::fs::write(&tmp, &serialized).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = write_result {
        tracing::warn!(
            path = %path.display(),
            %error,
            "could not persist recovery-breaker sidecar; durable circuit state may lag"
        );
    }
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
    fn cleared_state_allows_and_round_trips_through_sidecar() {
        let td = tempfile::tempdir().expect("tempdir");
        let db = td.path().join("storage.sqlite3");
        std::fs::write(&db, b"content").unwrap();
        let fingerprint = fingerprint_db(&db);

        assert!(load(&db).is_none(), "no sidecar yet");
        let state = record_failure(None, &fingerprint, "boom", CFG, 42);
        store(&db, &state);
        assert_eq!(load(&db).as_ref(), Some(&state));

        store(&db, &cleared_state(&fingerprint));
        let cleared = load(&db).expect("cleared sidecar persists");
        assert!(!cleared.tripped);
        assert_eq!(cleared.consecutive_failures, 0);
        assert_eq!(
            evaluate(Some(&cleared), &fingerprint, CFG, 100),
            BreakerVerdict::Allow
        );
    }

    #[test]
    fn sidecar_name_cannot_be_mistaken_for_backup_or_sidecar_artifacts() {
        let path = breaker_sidecar_path(Path::new("/x/storage.sqlite3"));
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "storage.sqlite3.am-recovery-breaker.json");
        for forbidden_prefix in [
            "storage.sqlite3.bak",
            "storage.sqlite3.backup-",
            "storage.sqlite3.recovery",
        ] {
            assert!(
                !name.starts_with(forbidden_prefix),
                "{name} must not match backup-candidate prefix {forbidden_prefix}"
            );
        }
        for forbidden_suffix in ["-wal", "-shm", "-journal"] {
            assert!(!name.ends_with(forbidden_suffix));
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
