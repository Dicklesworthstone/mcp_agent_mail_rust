//! `fm-db-state-files-sqlite-sidecar-symlink` — P0.
//!
//! **Subsystem**: db_state_files.
//!
//! ## What's broken
//!
//! One or more of the SQLite sidecar files (`storage.sqlite3-wal`,
//! `storage.sqlite3-shm`) is a **symlink** rather than a regular
//! file. SQLite follows symlinks on these auxiliary files, which
//! means an attacker (or a misconfigured deployment script) can
//! redirect WAL writes to an arbitrary path on disk — corrupting
//! whatever lives at the symlink target and exfiltrating page
//! contents in the process.
//!
//! The doctor's job:
//! - Detect symlinks on the sidecar paths (and on the main DB path
//!   for diagnostic surfacing — auto-fix refuses for the main DB).
//! - Auto-fix the sidecars by `Op::Rename`-ing them into the
//!   per-run quarantine directory. SQLite will recreate fresh
//!   sidecars on next open.
//! - For the main DB file itself: refuse with a manual remediation
//!   pointer (operator must reconfigure `storage_root`).
//!
//! ## Detection (pure function)
//!
//! For each candidate path:
//! 1. `fs::symlink_metadata(path)` — never follow.
//! 2. If `file_type().is_symlink()`: emit a finding with the
//!    symlink target (read via `fs::read_link`).
//! 3. Otherwise (or if ENOENT): no finding.
//!
//! Three classes:
//! - `Sidecar::Wal` — `storage.sqlite3-wal`
//! - `Sidecar::Shm` — `storage.sqlite3-shm`
//! - `Sidecar::MainDb` — `storage.sqlite3` (advisory only;
//!   auto-fix refuses)
//!
//! ## Fix
//!
//! **Detect-only.** The chokepoint's `reject_unexpected_symlink`
//! guard (mutate.rs) refuses every `Op` variant against a symlink
//! target *except* `Op::SymlinkAtomic`, which is the wrong shape
//! for a quarantine-rename. Implementing a true auto-fix requires
//! adding a new `Op::QuarantineSymlink { to }` variant to the
//! chokepoint — out of scope for this commit (filed as
//! follow-up).
//!
//! The detector + manual_remediation envelope already gives
//! operators a precise actionable signal: the path of the
//! symlink, the target, and a copy-paste shell command to
//! quarantine it manually:
//!
//! ```sh
//! mv -v <symlink> <symlink>.bak.$(date +%s)
//! ```
//!
//! After the operator runs that, `am doctor health` will return
//! to green; SQLite will recreate fresh -wal/-shm on next open.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-sqlite-sidecar-symlink";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Sidecar {
    /// `storage.sqlite3-wal` — Write-Ahead Log.
    Wal,
    /// `storage.sqlite3-shm` — shared-memory file.
    Shm,
    /// `storage.sqlite3` itself — the main DB file. Detect-only
    /// (auto-fix refuses; reconfiguring `storage_root` is the
    /// operator-side remediation).
    MainDb,
}

impl Sidecar {
    fn as_kebab(self) -> &'static str {
        match self {
            Sidecar::Wal => "wal",
            Sidecar::Shm => "shm",
            Sidecar::MainDb => "main_db",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SqliteSidecarSymlinkFinding {
    /// The symlink path itself.
    pub link_path: PathBuf,
    /// Where the symlink points. May be empty if `read_link` failed
    /// (e.g., truncated link content).
    pub link_target: PathBuf,
    pub sidecar: Sidecar,
}

impl SqliteSidecarSymlinkFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "SQLite {} sidecar at {} is a symlink → {}",
            self.sidecar.as_kebab(),
            self.link_path.display(),
            self.link_target.display(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "link_path": self.link_path.to_string_lossy(),
                "link_target": self.link_target.to_string_lossy(),
                "sidecar": self.sidecar.as_kebab(),
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only: the chokepoint refuses every Op
                // variant against a symlink target (security
                // defense). Auto-fix requires a new
                // Op::QuarantineSymlink — out of scope here.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        match self.sidecar {
            Sidecar::Wal | Sidecar::Shm => format!(
                "Sidecar at {} is a symlink → {}. After stopping `am serve`, quarantine it \
                 manually: `mv -v {} {}.bak.$(date +%s)`. SQLite will recreate fresh -wal / -shm \
                 files on next open; the symlink target is left untouched. Auto-fix is detect-only \
                 because the doctor chokepoint refuses every Op against a symlink target as a \
                 security defense.",
                self.link_path.display(),
                self.link_target.display(),
                self.link_path.display(),
                self.link_path.display(),
            ),
            Sidecar::MainDb => format!(
                "Main DB file {} is a symlink → {}. Reconfigure `STORAGE_ROOT` to point at \
                 the real file path rather than the symlink. The doctor refuses to auto-rename \
                 the main DB because that would break the deployment.",
                self.link_path.display(),
                self.link_target.display()
            ),
        }
    }
}

/// One candidate path tagged with its expected role. Pass-35-review
/// Codex F3 / Gemini F3 (P2): the pre-fix detector inferred the
/// role from a `-wal` / `-shm` filename-suffix match, but an
/// operator whose main DB filename ends with those suffixes
/// (e.g., `mailbox-shm.sqlite3`) would have been misclassified
/// and shown the wrong remediation text. The caller — who knows
/// whether each path was the main DB or a sidecar — now passes
/// the role explicitly.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub role: Sidecar,
}

impl Candidate {
    pub fn main_db(path: PathBuf) -> Self {
        Self {
            path,
            role: Sidecar::MainDb,
        }
    }
    pub fn wal(path: PathBuf) -> Self {
        Self {
            path,
            role: Sidecar::Wal,
        }
    }
    pub fn shm(path: PathBuf) -> Self {
        Self {
            path,
            role: Sidecar::Shm,
        }
    }
}

/// Detector. PURE — `symlink_metadata` + `read_link` (never follow).
///
/// `candidates`: each path tagged with its `Sidecar` role by the
/// caller — typically three entries (main DB + -wal + -shm) per
/// configured storage root. The detector no longer guesses the
/// role from the filename suffix.
pub fn detect(candidates: &[Candidate]) -> Vec<SqliteSidecarSymlinkFinding> {
    let mut out = Vec::new();
    for cand in candidates {
        let meta = match std::fs::symlink_metadata(&cand.path) {
            Ok(m) => m,
            Err(_) => continue, // ENOENT / EACCES — not our finding
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let target = std::fs::read_link(&cand.path).unwrap_or_default();
        out.push(SqliteSidecarSymlinkFinding {
            link_path: cand.path.clone(),
            link_target: target,
            sidecar: cand.role,
        });
    }
    out
}

/// Detect-only FM. `fix()` is a no-op — the chokepoint's
/// `reject_unexpected_symlink` guard refuses every `Op` variant
/// against a symlink target (security defense). A future
/// `Op::QuarantineSymlink` variant could land an auto-fix; for now
/// the finding's manual_remediation envelope guides the operator
/// to a single-line shell rename.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &SqliteSidecarSymlinkFinding,
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
    use crate::doctor::mutate::Capabilities;
    use std::fs;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn make_ctx(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir,
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    #[test]
    fn detector_returns_empty_when_no_symlinks() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let wal = td.path().join("storage.sqlite3-wal");
        let shm = td.path().join("storage.sqlite3-shm");
        fs::write(&db, b"SQLite format 3\0").unwrap();
        fs::write(&wal, b"wal").unwrap();
        fs::write(&shm, b"shm").unwrap();
        let findings = detect(&[
            Candidate::main_db(db),
            Candidate::wal(wal),
            Candidate::shm(shm),
        ]);
        assert!(findings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_wal_symlink() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("attacker-target.bin");
        fs::write(&target, b"victim data").unwrap();
        let wal = td.path().join("storage.sqlite3-wal");
        std::os::unix::fs::symlink(&target, &wal).unwrap();
        let findings = detect(&[Candidate::wal(wal)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].sidecar, Sidecar::Wal);
        assert_eq!(findings[0].link_target, target);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_shm_symlink() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("attacker-target.bin");
        fs::write(&target, b"victim data").unwrap();
        let shm = td.path().join("storage.sqlite3-shm");
        std::os::unix::fs::symlink(&target, &shm).unwrap();
        let findings = detect(&[Candidate::shm(shm)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].sidecar, Sidecar::Shm);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_main_db_symlink_with_detect_only_remediation() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("real-db");
        fs::write(&target, b"SQLite format 3\0").unwrap();
        let db = td.path().join("storage.sqlite3");
        std::os::unix::fs::symlink(&target, &db).unwrap();
        let findings = detect(&[Candidate::main_db(db)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].sidecar, Sidecar::MainDb);
        let g = findings[0].to_finding();
        assert!(
            !g.remediation.auto_fixable,
            "main DB symlink must NOT be auto-fixable"
        );
        assert_eq!(g.remediation.estimated_actions, 0);
    }

    #[cfg(unix)]
    #[test]
    fn detector_classifies_main_db_correctly_even_when_filename_ends_with_wal_suffix() {
        // Pass-35-review Codex F3 / Gemini F3 (P2): an operator
        // whose main DB filename happens to end with `-wal` (or
        // `-shm`) must NOT be misclassified as a sidecar. The
        // caller passes the role explicitly via `Candidate::main_db`.
        let td = TempDir::new().unwrap();
        let target = td.path().join("real-db");
        fs::write(&target, b"SQLite format 3\0").unwrap();
        let db = td.path().join("mailbox-wal"); // ends with `-wal`!
        std::os::unix::fs::symlink(&target, &db).unwrap();
        let findings = detect(&[Candidate::main_db(db)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].sidecar,
            Sidecar::MainDb,
            "main DB suffix must come from caller, not filename"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_returns_empty_for_missing_paths() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[Candidate::wal(td.path().join("nope/storage.sqlite3-wal"))]);
        assert!(findings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fixer_is_no_op_for_wal_symlink_chokepoint_refuses() {
        // Detect-only contract: the chokepoint refuses every Op
        // against a symlink target, so the fixer is a no-op that
        // records actions_skipped: 1. The symlink and its target
        // are untouched; the operator follows the
        // manual_remediation pointer to quarantine via shell.
        let td = TempDir::new().unwrap();
        let target = td.path().join("attacker-target.bin");
        fs::write(&target, b"victim data").unwrap();
        let wal = td.path().join("storage.sqlite3-wal");
        std::os::unix::fs::symlink(&target, &wal).unwrap();
        let run_id = "2026-05-15T05-00-00Z__sidecar-wal";
        let ctx = make_ctx(&td, run_id);
        let findings = detect(&[Candidate::wal(wal.clone())]);
        assert_eq!(findings.len(), 1);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // Symlink unchanged; target untouched.
        let meta = fs::symlink_metadata(&wal).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"victim data");
    }

    #[cfg(unix)]
    #[test]
    fn fixer_is_no_op_for_main_db_symlink() {
        // Same detect-only contract for the main DB; manual
        // remediation steers the operator at STORAGE_ROOT
        // reconfiguration rather than quarantine.
        let td = TempDir::new().unwrap();
        let target = td.path().join("real-db");
        fs::write(&target, b"SQLite format 3\0").unwrap();
        let db = td.path().join("storage.sqlite3");
        std::os::unix::fs::symlink(&target, &db).unwrap();
        let run_id = "2026-05-15T05-00-01Z__sidecar-main";
        let ctx = make_ctx(&td, run_id);
        let findings = detect(&[Candidate::main_db(db.clone())]);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        let meta = fs::symlink_metadata(&db).unwrap();
        assert!(meta.file_type().is_symlink());
    }

    #[test]
    fn finding_severity_is_p0_detect_only() {
        let f = SqliteSidecarSymlinkFinding {
            link_path: PathBuf::from("/x/storage.sqlite3-wal"),
            link_target: PathBuf::from("/etc/passwd"),
            sidecar: Sidecar::Wal,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!(
            !g.remediation.auto_fixable,
            "detect-only — chokepoint refuses Op-against-symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn manual_remediation_distinguishes_sidecar_vs_main_db() {
        let wal_f = SqliteSidecarSymlinkFinding {
            link_path: PathBuf::from("/x/storage.sqlite3-wal"),
            link_target: PathBuf::from("/x/elsewhere"),
            sidecar: Sidecar::Wal,
        };
        let main_f = SqliteSidecarSymlinkFinding {
            link_path: PathBuf::from("/x/storage.sqlite3"),
            link_target: PathBuf::from("/x/realdb"),
            sidecar: Sidecar::MainDb,
        };
        assert!(wal_f.manual_remediation_text().contains("quarantine"));
        assert!(main_f.manual_remediation_text().contains("STORAGE_ROOT"));
    }
}
