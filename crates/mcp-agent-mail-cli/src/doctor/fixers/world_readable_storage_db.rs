//! `fm-db-state-files-world-readable-storage-db` — P0.
//!
//! **Subsystem**: db_state_files (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! Agent Mail's `storage.sqlite3` holds every message body, every
//! agent identity, every contact graph entry. If the file is left
//! with group- or world-readable permissions (often 0o644 — what the
//! default umask produces on first-time-write), any local user
//! account can read the entire mailbox.
//!
//! This is P0 (not P1 like the token-backup chmod FM): the DB is
//! the canonical store, not a backup of one. A leak there exposes
//! all message bodies, not just a previous bearer token.
//!
//! ## Detection (pure function)
//!
//! Given a list of candidate DB file paths (typically just
//! `<storage_root>/storage.sqlite3`):
//! 1. `fs::symlink_metadata` — refuse to follow symlinks (defense
//!    against symlink-swap attack on the chmod target).
//! 2. Skip if not a regular file.
//! 3. Skip if mode bits 0o077 are clear (file is already 0o600 /
//!    0o400 / 0o700 etc.).
//! 4. Otherwise emit a finding.
//!
//! ## Fix (`Op::Chmod` — same pattern as
//! `world_readable_token_bak`)
//!
//! `mutate(ctx, path, Op::Chmod { mode: 0o600 })`. The chokepoint
//! uses `chmod_via_fd` with `O_NOFOLLOW` (pass-3 hardening), so
//! the chmod is symlink-safe even if an attacker plants one
//! between detect and fix.
//!
//! ## Reversibility
//!
//! Standard via `am doctor undo <run-id>`: the chokepoint records
//! `before_mode`/`after_mode` in `actions.jsonl` and restores the
//! original mode on undo. (Whether restoring the original
//! world-readable mode is *desirable* is a separate question, but
//! the chokepoint guarantees byte-identical reversibility.)
//!
//! ## Why a separate FM from `world_readable_token_bak`?
//!
//! Different file shape (binary SQLite vs. text token-backup),
//! different severity (P0 vs P1), different evidence detail. The
//! token-bak detector requires a token-shape body match
//! (`"authorization"`, `"bearer"`, etc.) before flagging — that
//! filter would never pass on a binary SQLite header. Two FMs is
//! clearer than one with conditional logic.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-db-state-files-world-readable-storage-db";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "db_state_files";

/// Safe permission mode: rw for owner only.
pub const SAFE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Serialize)]
pub struct WorldReadableStorageDbFinding {
    pub path: PathBuf,
    pub current_mode: u32,
}

impl WorldReadableStorageDbFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "DB file {} has world/group-readable mode 0o{:o} (target: 0o600); contains all message bodies",
            self.path.display(),
            self.current_mode & 0o777,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "path": self.path.to_string_lossy(),
                "current_mode_octal": format!("0o{:o}", self.current_mode & 0o777),
                "current_mode_decimal": self.current_mode,
                "target_mode_octal": format!("0o{:o}", SAFE_MODE),
                "risk": "DB holds all message bodies, agent identities, contact graphs",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Detector. PURE.
///
/// `candidate_paths` is a small list (typically just one entry —
/// `<storage_root>/storage.sqlite3`). Caller resolves the DB path
/// from `DbPoolConfig::database_url` and passes it in.
pub fn detect(candidate_paths: &[PathBuf]) -> Vec<WorldReadableStorageDbFinding> {
    let mut out = Vec::new();
    for path in candidate_paths {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue, // not present / unreadable — skip
        };
        if !meta.file_type().is_file() {
            continue; // symlink-attack defense
        }
        let mode = meta.permissions().mode();
        if mode & 0o077 == 0 {
            // Already safe (0o600 or stricter — 0o400 / 0o700 / etc.).
            continue;
        }
        out.push(WorldReadableStorageDbFinding {
            path: path.clone(),
            current_mode: mode,
        });
    }
    out
}

/// Fixer. Routes through `mutate()` with `Op::Chmod`.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &WorldReadableStorageDbFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    if !finding.path.exists() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    mutate(ctx, &finding.path, Op::Chmod { mode: SAFE_MODE })?;
    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{Capabilities, MutateContext};
    use crate::doctor::runs::scaffold_run_dir;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir: run_dir.clone(),
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: FM_ID.to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    #[test]
    fn detector_returns_empty_for_safe_db_mode() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("storage.sqlite3");
        fs::write(&p, b"sqlite header").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        let findings = detect(&[p]);
        assert!(findings.is_empty(), "0o600 must not flag");
    }

    #[test]
    fn detector_flags_world_readable_db() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("storage.sqlite3");
        fs::write(&p, b"sqlite header").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, p);
        assert_eq!(findings[0].current_mode & 0o777, 0o644);
    }

    #[test]
    fn detector_flags_group_readable_db() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("storage.sqlite3");
        fs::write(&p, b"sqlite header").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].current_mode & 0o777, 0o640);
    }

    #[test]
    fn detector_skips_missing_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("nope.sqlite3");
        let findings = detect(&[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_refuses_symlink() {
        let td = TempDir::new().unwrap();
        let real = td.path().join("real.sqlite3");
        fs::write(&real, b"data").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        let link = td.path().join("link.sqlite3");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let findings = detect(std::slice::from_ref(&link));
        assert!(
            findings.is_empty(),
            "must NOT follow symlinks (symlink-swap attack defense)"
        );
    }

    #[test]
    fn fixer_chmods_to_0o600_via_mutate() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("storage.sqlite3");
        fs::write(&p, b"sqlite header").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let findings = detect(std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-13T00-00-00Z__db_chmod");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "DB mode must be 0o600 post-fix (got 0o{mode:o})"
        );
    }

    #[test]
    fn fixer_idempotent_on_already_safe_db() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("storage.sqlite3");
        fs::write(&p, b"sqlite header").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let findings = detect(std::slice::from_ref(&p));

        // Simulate: between detect and fix, another agent already
        // chmod'd to 0o600. Fix should still succeed (chmod to 0o600
        // is a no-op at the FS level).
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();

        let ctx = ctx_for(&td, "2026-05-13T00-00-00Z__db_idemp");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        // The fixer always returns actions_taken=1 when the file
        // exists — the chokepoint records before/after hashes;
        // re-chmod is harmless.
        assert_eq!(outcome.actions_taken, 1);
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn fixer_skips_when_file_vanished() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("storage.sqlite3");
        // Don't create it.
        let finding = WorldReadableStorageDbFinding {
            path: p,
            current_mode: 0o644,
        };
        let ctx = ctx_for(&td, "2026-05-13T00-00-00Z__db_vanish");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn finding_severity_is_p0_and_subsystem_is_db_state_files() {
        let f = WorldReadableStorageDbFinding {
            path: PathBuf::from("/x/y/storage.sqlite3"),
            current_mode: 0o644,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "db_state_files");
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 1);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("0o644"));
    }
}
