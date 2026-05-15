//! `fm-db-state-files-empty-or-truncated-db` — P0.
//!
//! **Subsystem**: db_state_files (Phase 1 archaeology — HANDOFF
//! P3-C #4 ranking).
//!
//! ## What's broken
//!
//! `storage.sqlite3` is too small to be a valid SQLite database
//! OR fails `PRAGMA quick_check`. Indicates partial-write
//! corruption (truncated by `fs::write` mid-stream, a kernel
//! crash during DB grow, or a manual `> storage.sqlite3` shell
//! redirect that wiped the file). Either case loses every
//! message body, agent identity, and contact graph in the DB.
//!
//! This is P0 — the DB is the canonical store. The Rust pool
//! refuses to open a malformed DB; the doctor's job is to
//! detect the state and point the operator at recovery.
//!
//! ## Detection (pure function)
//!
//! Bypasses `SqliteConnection` entirely (round-5 Gemini F1 + F2):
//! even with `OpenFlags::read_only()`, opening a WAL-mode DB
//! through SQLite mutates the `-shm` sidecar (or refuses to open
//! if `-shm` is absent), and the metadata-then-open dance has a
//! TOCTOU window where a symlink can be swapped to `/dev/zero`
//! or a named pipe. Reading the SQLite header bytes directly off
//! a held file descriptor closes both.
//!
//! 1. Open `path` for reading with `O_NONBLOCK` (cheap DoS
//!    defense — opening a FIFO with no writer would otherwise
//!    block indefinitely). Symlinks ARE followed, matching the
//!    behavior SQLite would have at runtime, so a symlinked DB
//!    file gets probed against its target (round-4 Gemini F1).
//!    ENOENT → `Reason::Missing`. Any other open error →
//!    `Reason::OpenFailed`.
//! 2. `fd.metadata().file_type().is_file()` on the **open fd** —
//!    this closes the round-4/round-5 TOCTOU window because the
//!    fd is locked to whatever inode was resolved at open time.
//!    If the fd is a dir/device/fifo/socket, skip silently (not
//!    our domain).
//! 3. If `meta.len() < SQLITE_HEADER_BYTES` (100), emit
//!    `Reason::TooSmall { size }`.
//! 4. `read_exact` the 100-byte SQLite header off the fd. The
//!    first 16 bytes MUST equal the magic
//!    `b"SQLite format 3\0"`; else `Reason::HeaderMagicFailed`.
//!    Past-magic deeper validation (page-size sanity,
//!    schema_cookie parse) lives in a dedicated
//!    `am doctor deep-check` verb (future) — this FM only
//!    catches the gross "file isn't even a SQLite header" case
//!    that blocks startup.
//! 5. Otherwise no finding.
//!
//! ## Fix
//!
//! **None.** Doctor cannot rebuild a corrupted SQLite file
//! deterministically without operator intervention. The finding
//! emits a `manual_remediation` envelope pointing operators at
//! `am doctor reconstruct --json` which walks the Git archive
//! and rebuilds the DB from message files.
//!
//! `auto_fixable: false` (detect-only); fix() is a no-op for
//! API uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-empty-or-truncated-db";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

/// SQLite file format requires at least a 100-byte header.
/// Anything smaller is necessarily corrupt or empty.
pub const SQLITE_HEADER_BYTES: u64 = 100;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Reason {
    /// File doesn't exist on disk.
    Missing,
    /// File exists but is smaller than the SQLite header.
    TooSmall { size: u64 },
    /// `open(2)` failed (permission, EISDIR, etc.). Round-5
    /// Gemini F1+F2: open is `O_RDONLY | O_NONBLOCK` and follows
    /// symlinks; `O_NONBLOCK` defeats the FIFO-blocks-open DoS,
    /// and the subsequent `fd.metadata()` check ensures the held
    /// inode is a regular file before any bytes are read.
    OpenFailed { message: String },
    /// The first 16 bytes of `path` are not the SQLite magic
    /// `"SQLite format 3\0"`. The header bytes that WERE read are
    /// surfaced as a hex string for diagnostics. Round-5 Gemini F1:
    /// replaces the pre-round-5 `SchemaProbeFailed` reason, which
    /// required opening through SQLite (which mutated `-shm` on
    /// healthy WAL DBs and false-positived on missing `-shm`).
    HeaderMagicFailed { observed_prefix_hex: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct EmptyOrTruncatedDbFinding {
    pub db_path: PathBuf,
    pub reason: Reason,
}

impl EmptyOrTruncatedDbFinding {
    pub fn to_finding(&self) -> super::Finding {
        let reason_str = match &self.reason {
            Reason::Missing => "missing".to_string(),
            Reason::TooSmall { size } => format!("too_small (size={size})"),
            Reason::OpenFailed { message } => format!("open_failed ({message})"),
            Reason::HeaderMagicFailed {
                observed_prefix_hex,
            } => format!("header_magic_failed (prefix={observed_prefix_hex})"),
        };
        let title = format!(
            "DB {} is empty or corrupted ({reason_str}); recover via `am doctor reconstruct`",
            self.db_path.display()
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "reason": self.reason,
                "sqlite_header_min_bytes": SQLITE_HEADER_BYTES,
                "recovery_command": "am doctor reconstruct --json",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// SQLite file format magic — the first 16 bytes of every valid
/// SQLite database file. Documented at <https://sqlite.org/fileformat.html>.
pub const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Detector. PURE w.r.t. caller-supplied paths.
///
/// Round-5 (Gemini F1 + F2): the detector opens the candidate
/// file directly, checks `metadata()` on the held fd, then reads
/// the 100-byte SQLite header off that same fd. Bypasses
/// `SqliteConnection` entirely so the probe cannot create
/// `-shm` sidecars, cannot fail with `SQLITE_READONLY_CANTINIT`
/// on a healthy WAL DB whose `-shm` is missing, and is not racy
/// against a symlink swap between `fs::metadata` and a later
/// open.
///
/// Skips silently when the resolved fd is a dir/device/fifo
/// (not our domain).
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<EmptyOrTruncatedDbFinding> {
    let mut out = Vec::new();
    for path in candidate_paths {
        match detect_one(path) {
            Ok(Some(reason)) => out.push(EmptyOrTruncatedDbFinding {
                db_path: path.clone(),
                reason,
            }),
            Ok(None) => {}
            Err(reason) => out.push(EmptyOrTruncatedDbFinding {
                db_path: path.clone(),
                reason,
            }),
        }
    }
    out
}

/// Probe one candidate. Returns:
/// - `Ok(None)` — healthy or not-our-domain (dir/device/fifo).
/// - `Ok(Some(reason))` — a corruption finding to surface.
/// - `Err(reason)` — same shape; matches `Reason::OpenFailed`
///   convenience in the open path.
fn detect_one(path: &std::path::Path) -> Result<Option<Reason>, Reason> {
    use std::io::Read as _;

    // Stage 1: O_NONBLOCK open. Symlinks are followed (so a
    // symlinked DB is probed against its target — round-4 G1),
    // but O_NONBLOCK defeats the FIFO-blocks-open DoS. ENOENT is
    // surfaced as Missing; anything else is OpenFailed.
    let mut f = match open_nonblock_for_read(path) {
        Ok(f) => f,
        Err(e) => {
            return Ok(Some(if e.kind() == std::io::ErrorKind::NotFound {
                Reason::Missing
            } else {
                Reason::OpenFailed {
                    message: format!("{e}"),
                }
            }));
        }
    };

    // Stage 2: metadata() on the OPEN fd. This is what closes the
    // round-5 Gemini F2 TOCTOU window — the fd we read from is
    // bound to the inode that existed at open time, and the
    // file_type check below decides whether to read from it.
    let meta = f.metadata().map_err(|e| Reason::OpenFailed {
        message: format!("metadata: {e}"),
    })?;
    if !meta.file_type().is_file() {
        // Dir / device / fifo / socket — not our domain.
        return Ok(None);
    }
    if meta.len() < SQLITE_HEADER_BYTES {
        return Ok(Some(Reason::TooSmall { size: meta.len() }));
    }

    // Stage 3: read first 100 bytes; verify SQLite magic.
    let mut header = [0u8; SQLITE_HEADER_BYTES as usize];
    f.read_exact(&mut header).map_err(|e| Reason::OpenFailed {
        message: format!("header read: {e}"),
    })?;
    if &header[..16] != SQLITE_MAGIC {
        return Ok(Some(Reason::HeaderMagicFailed {
            observed_prefix_hex: hex::encode(&header[..16]),
        }));
    }
    Ok(None)
}

#[cfg(unix)]
fn open_nonblock_for_read(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_nonblock_for_read(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Detect-only FM. `fix()` is a no-op for API uniformity.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &EmptyOrTruncatedDbFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlmodel_sqlite::SqliteConnection;
    use std::fs;
    use tempfile::TempDir;

    fn make_healthy_db(td: &TempDir, name: &str) -> PathBuf {
        let p = td.path().join(name);
        let conn = SqliteConnection::open_file(p.to_string_lossy().into_owned())
            .expect("open new sqlite db");
        conn.execute_raw("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .expect("create + insert");
        drop(conn);
        p
    }

    #[test]
    fn detector_returns_empty_for_healthy_db() {
        let td = TempDir::new().unwrap();
        let db = make_healthy_db(&td, "good.sqlite3");
        let findings = detect(&[db]);
        assert!(findings.is_empty(), "healthy DB must not flag");
    }

    #[test]
    fn detector_flags_missing_file() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert_eq!(findings.len(), 1);
        assert!(matches!(findings[0].reason, Reason::Missing));
    }

    #[test]
    fn detector_flags_truncated_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("truncated.sqlite3");
        // Smaller than the 100-byte SQLite header.
        fs::write(&p, b"not a real sqlite header").unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        match &findings[0].reason {
            Reason::TooSmall { size } => assert!(*size < SQLITE_HEADER_BYTES),
            other => panic!("expected TooSmall, got {other:?}"),
        }
    }

    #[test]
    fn detector_flags_invalid_sqlite_header_above_min_size() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("garbage.sqlite3");
        // Above the 100-byte minimum but not a real SQLite header.
        // Round-5: the detector reads bytes directly and verifies
        // the SQLite magic, so this MUST land in HeaderMagicFailed.
        fs::write(&p, vec![0xFF_u8; 200]).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(
            findings.len(),
            1,
            "garbage-content file must flag (got: {findings:?})"
        );
        match &findings[0].reason {
            Reason::HeaderMagicFailed {
                observed_prefix_hex,
            } => assert!(observed_prefix_hex.starts_with("ff")),
            other => panic!("expected HeaderMagicFailed, got {other:?}"),
        }
    }

    #[test]
    fn detector_skips_fifo_silently_without_blocking() {
        // Round-5 Gemini F2 defense: O_NONBLOCK | post-open
        // fd.metadata() means a FIFO at the candidate path opens
        // immediately (no DoS hang waiting for a writer) and is
        // skipped silently as not-our-domain. The probe must
        // return WITHOUT blocking and WITHOUT emitting a finding.
        use std::os::unix::fs::FileTypeExt as _;
        let td = TempDir::new().unwrap();
        let fifo = td.path().join("fifo_db");
        // mkfifo via nix
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(
            fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
            "test setup: must be a FIFO"
        );
        let findings = detect(std::slice::from_ref(&fifo));
        assert!(
            findings.is_empty(),
            "FIFO must be skipped silently (got: {findings:?})"
        );
    }

    #[test]
    fn detector_follows_symlinks_to_healthy_db() {
        // Pass-34 round-4 (Gemini F1): a symlink to a healthy
        // DB must NOT silently bypass detection. Pre-fix
        // `symlink_metadata` reported `Symlink` and the loop
        // continued past it; `fs::metadata` resolves to the
        // target so a corrupt target is flagged and a healthy
        // target leaves no finding.
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let target = make_healthy_db(&td, "real.sqlite3");
        let link = td.path().join("link.sqlite3");
        symlink(&target, &link).unwrap();
        let findings = detect(std::slice::from_ref(&link));
        assert!(
            findings.is_empty(),
            "symlink to healthy DB must not flag (got: {findings:?})"
        );
    }

    #[test]
    fn detector_flags_symlink_to_corrupt_db() {
        // Pass-34 round-4 (Gemini F1): symlink to a corrupt DB
        // must still flag through the symlink. Verifies the
        // is_file() check on resolved metadata still hits the
        // corruption-flagging branches.
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let target = td.path().join("corrupt.sqlite3");
        fs::write(&target, vec![0xFF_u8; 200]).unwrap();
        let link = td.path().join("link.sqlite3");
        symlink(&target, &link).unwrap();
        let findings = detect(std::slice::from_ref(&link));
        assert_eq!(findings.len(), 1, "symlink to corrupt DB must flag");
    }

    #[test]
    fn detector_is_pure_no_state_mutation_on_healthy_db() {
        // Pass-34 round-4 (Codex F1 + Gemini F3): detect()
        // must not mutate the DB file or create -wal/-shm
        // siblings even on a freshly-closed DB. Read-only
        // open suppresses journal replay and WAL creation.
        use sha2::{Digest, Sha256};
        let td = TempDir::new().unwrap();
        let db = make_healthy_db(&td, "pure.sqlite3");
        let before_bytes = fs::read(&db).unwrap();
        let before_hash = {
            let mut h = Sha256::new();
            h.update(&before_bytes);
            hex::encode(h.finalize())
        };
        let before_dir: Vec<String> = fs::read_dir(td.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let _ = detect(std::slice::from_ref(&db));
        let after_bytes = fs::read(&db).unwrap();
        let after_hash = {
            let mut h = Sha256::new();
            h.update(&after_bytes);
            hex::encode(h.finalize())
        };
        let after_dir: Vec<String> = fs::read_dir(td.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            before_hash, after_hash,
            "detect() mutated DB bytes (before={before_hash}, after={after_hash})"
        );
        assert_eq!(
            before_dir, after_dir,
            "detect() created sibling files (before={before_dir:?}, after={after_dir:?})"
        );
    }

    #[test]
    fn finding_is_p0_detect_only_with_recovery_command() {
        let f = EmptyOrTruncatedDbFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            reason: Reason::TooSmall { size: 0 },
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "db_state_files");
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("am doctor reconstruct"));
    }
}
