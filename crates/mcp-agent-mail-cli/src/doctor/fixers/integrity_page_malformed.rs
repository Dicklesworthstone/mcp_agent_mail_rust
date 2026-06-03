//! `fm-db-state-files-integrity-page-malformed` — P0.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! `storage.sqlite3` has malformed page(s) — `PRAGMA integrity_check`
//! returns an error string (or a list of errors) instead of `"ok"`.
//! This is the canonical signal that on-disk B-tree pages have
//! drifted from their indexes, cells overflow incorrectly, or a
//! page boundary marker is wrong. Causes range from:
//!
//! - storage media failure (bad sectors, FS-level corruption),
//! - kernel crash mid-page-write before fsync completed,
//! - concurrent writes from two SQLite processes (the canonical
//!   reason for `fm-db-state-files-python-server-coresident-write`),
//! - a partial restore from backup that mismatched WAL + main.
//!
//! Once integrity is broken, every query that touches a malformed
//! page either errors or — worse — returns silently-wrong rows.
//! Recovery is non-trivial: SQLite's recovery extension or
//! `am doctor reconstruct` against the git archive.
//!
//! ## Detection (pure function)
//!
//! Opens the DB read-only with URI `?immutable=1` (no -shm
//! creation, no locking). Runs:
//!
//! ```sql
//! PRAGMA integrity_check(1)
//! ```
//!
//! The `1` limits the result to the first error — full
//! integrity check on a multi-GB DB can take minutes; we only
//! need to know whether the DB is corrupt, not enumerate every
//! page that's broken. If the column value is the literal
//! string `"ok"`, no finding. Otherwise emit a P0 finding with
//! the error text as evidence.
//!
//! ### Performance note
//!
//! `PRAGMA integrity_check` reads every page in the DB. On
//! large mailbox DBs (multi-GB) this can run for several
//! minutes. The detector is intentionally **NOT** part of the
//! default `am doctor` sweep; agents wanting to run it must
//! invoke `am doctor fix --only fm-db-state-files-integrity-page-malformed
//! --list` explicitly. The default sweep relies on cheaper
//! detectors (`empty_or_truncated_db`, `wal_mode_disabled`,
//! `world_readable_storage_db`) for sub-200ms turnaround.
//!
//! ## Fix
//!
//! **Detect-only.** Auto-repair would require the
//! `am doctor reconstruct` path (Op::Rename the corrupt DB to
//! quarantine, then INSERT...SELECT from the git archive into
//! a fresh DB), which is a separate, already-implemented
//! command with its own UI. The manual_remediation envelope
//! routes operators there.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-integrity-page-malformed";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct IntegrityPageMalformedFinding {
    pub db_path: PathBuf,
    /// The exact text returned by `PRAGMA integrity_check(1)`.
    /// For `"ok"` DBs the detector emits no finding; for
    /// non-`"ok"` results this carries SQLite's error
    /// description (e.g., `"*** in database main *** Page 42:
    /// ..."`).
    pub integrity_check_result: String,
    /// Size of the DB file in bytes — useful for operators
    /// deciding whether the corruption is whole-file (likely
    /// truncation, but caught by `empty_or_truncated_db` first)
    /// or page-level (likely media / concurrent-writer fault).
    pub db_size_bytes: u64,
}

impl IntegrityPageMalformedFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "DB {} failed PRAGMA integrity_check: {}",
            self.db_path.display(),
            // Truncate the result for the title; full result is
            // in evidence.
            self.integrity_check_result
                .chars()
                .take(120)
                .collect::<String>(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "db_path": self.db_path.to_string_lossy(),
                "integrity_check_result": self.integrity_check_result,
                "db_size_bytes": self.db_size_bytes,
                "recovery_paths": [
                    "`am doctor reconstruct --yes` (rebuilds DB from git archive — destructive on the corrupt file but reversible via undo).",
                    "Restore from backup: `am doctor undo <prior-run-id>` if the corruption appeared after a recent doctor run.",
                ],
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only — `am doctor reconstruct` is the
                // canonical fix path.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "DB {} failed SQLite's integrity_check ({} bytes). Recovery requires \
             `am doctor reconstruct --yes` which Op::Rename's the corrupt file to \
             quarantine, then rebuilds a fresh DB by INSERT...SELECT from the git \
             archive. If the corruption appeared right after a doctor run, \
             `am doctor undo <run-id>` may be faster (restores byte-identical from \
             backup). Auto-fix is detect-only because reconstruct is a separate \
             chokepoint-managed surface with its own --yes gate.",
            self.db_path.display(),
            self.db_size_bytes,
        )
    }
}

/// Detector. PURE w.r.t. caller-supplied DB paths.
///
/// **Performance**: `PRAGMA integrity_check(1)` reads every
/// page. On a multi-GB DB this can take minutes. Callers should
/// gate this FM behind explicit operator opt-in
/// (`--only fm-db-state-files-integrity-page-malformed`) rather
/// than bundling it into a sub-200ms health probe.
pub fn detect(candidate_dbs: &[PathBuf]) -> Vec<IntegrityPageMalformedFinding> {
    let mut out = Vec::new();
    for db in candidate_dbs {
        if let Some(f) = detect_one(db) {
            out.push(f);
        }
    }
    out
}

fn detect_one(db_path: &std::path::Path) -> Option<IntegrityPageMalformedFinding> {
    if !has_sqlite_header(db_path) {
        return None;
    }
    // URI + immutable=1: read-only, no -shm creation, no
    // locking. Matches the pass-35H pattern.
    let uri = super::sqlite_immutable_uri(db_path);
    let mut flags = OpenFlags::read_only();
    flags.uri = true;
    let config = SqliteConfig::file(uri).flags(flags);
    let conn = match SqliteConnection::open(&config) {
        Ok(conn) => conn,
        Err(error) => {
            let detail = error.to_string();
            if !mcp_agent_mail_db::is_corruption_error_message(&detail) {
                return None;
            }
            let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
            return Some(IntegrityPageMalformedFinding {
                db_path: db_path.to_path_buf(),
                integrity_check_result: format!("open failed before integrity_check: {detail}"),
                db_size_bytes,
            });
        }
    };
    let result = match conn.query_sync("PRAGMA integrity_check(1)", &[]) {
        Ok(rows) => rows.first()?.get_named::<String>("integrity_check").ok()?,
        Err(error) => {
            let detail = error.to_string();
            if !mcp_agent_mail_db::is_corruption_error_message(&detail) {
                return None;
            }
            format!("PRAGMA integrity_check(1) failed: {detail}")
        }
    };
    if result == "ok" {
        return None;
    }
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    Some(IntegrityPageMalformedFinding {
        db_path: db_path.to_path_buf(),
        integrity_check_result: result,
        db_size_bytes,
    })
}

fn has_sqlite_header(path: &std::path::Path) -> bool {
    use std::io::Read as _;

    let Ok(mut file) = open_nonblock_for_read(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if !meta.file_type().is_file() || meta.len() < super::empty_or_truncated_db::SQLITE_HEADER_BYTES
    {
        return false;
    }
    let mut header = [0u8; 16];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    header == *super::empty_or_truncated_db::SQLITE_MAGIC
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

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &IntegrityPageMalformedFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_healthy_db(td: &TempDir) -> PathBuf {
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn detector_returns_empty_for_healthy_db() {
        let td = TempDir::new().unwrap();
        let db = make_healthy_db(&td);
        let findings = detect(std::slice::from_ref(&db));
        assert!(findings.is_empty(), "healthy DB must not flag");
    }

    #[test]
    fn detector_skips_missing_db() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.sqlite3")]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_non_sqlite_file() {
        // A non-SQLite file fails the direct SQLite-header probe
        // and is silently skipped (sibling FM
        // `empty_or_truncated_db` owns this surface).
        let td = TempDir::new().unwrap();
        let p = td.path().join("garbage.sqlite3");
        std::fs::write(&p, b"not a sqlite db").unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_header_only_truncated_file() {
        // A file with only SQLite's 16-byte magic is not a page-level
        // integrity failure. The empty/truncated FM owns sub-100-byte
        // files because SQLite's database header itself is incomplete.
        let td = TempDir::new().unwrap();
        let p = td.path().join("truncated.sqlite3");
        std::fs::write(&p, super::super::empty_or_truncated_db::SQLITE_MAGIC).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert!(findings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detector_skips_fifo_without_blocking() {
        use std::os::unix::fs::FileTypeExt as _;

        let td = TempDir::new().unwrap();
        let fifo = td.path().join("storage.sqlite3");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(&fifo)
                .unwrap()
                .file_type()
                .is_fifo()
        );

        let findings = detect(std::slice::from_ref(&fifo));
        assert!(findings.is_empty(), "FIFO must not block or flag");
    }

    #[test]
    fn corruption_query_error_becomes_p0_finding() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let conn = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        conn.execute_raw("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);

        let bytes = std::fs::read(&db).unwrap();
        let page_size = 4096;
        assert!(
            bytes.len() > page_size,
            "fixture DB should include a second page"
        );
        let mut corrupted = bytes;
        corrupted[page_size] ^= 0x7f;
        std::fs::write(&db, corrupted).unwrap();

        let findings = detect(std::slice::from_ref(&db));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .integrity_check_result
                .contains("PRAGMA integrity_check(1) failed")
                || findings[0]
                    .integrity_check_result
                    .contains("*** in database main ***")
                || findings[0].integrity_check_result.contains("malformed")
        );
    }

    #[test]
    fn finding_severity_is_p0_detect_only() {
        let f = IntegrityPageMalformedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            integrity_check_result: "*** in database main *** Page 42: corrupt".to_string(),
            db_size_bytes: 1_234_567,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!(!g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("am doctor reconstruct"));
        assert!(s.contains("integrity_check_result"));
    }

    #[test]
    fn manual_remediation_includes_db_size_and_reconstruct_pointer() {
        let f = IntegrityPageMalformedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            integrity_check_result: "***corrupt***".to_string(),
            db_size_bytes: 2_000_000,
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("2000000"));
        assert!(text.contains("am doctor reconstruct"));
    }

    #[test]
    fn finding_title_truncates_long_integrity_results() {
        let long_result = "x".repeat(500);
        let f = IntegrityPageMalformedFinding {
            db_path: PathBuf::from("/x/storage.sqlite3"),
            integrity_check_result: long_result,
            db_size_bytes: 0,
        };
        let g = f.to_finding();
        // Title carries first 120 chars; evidence carries full.
        assert!(g.title.len() < 200);
    }
}
