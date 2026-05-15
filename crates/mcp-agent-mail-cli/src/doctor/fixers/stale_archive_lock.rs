//! `fm-archive-state-files-stale-archive-lock-from-dead-pid` — P1.
//!
//! **Subsystem**: archive_state_files (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! Each Git-backed project archive under `$STORAGE_ROOT/projects/<slug>/`
//! has a `.git/index.lock` that git creates while writing the index. If
//! a writer process crashes / OOMs / gets SIGKILLed between lock-creation
//! and lock-release, the file stays behind. Subsequent git commands
//! refuse with "fatal: Unable to create '<path>/.git/index.lock': File
//! exists." This blocks the commit coalescer and freezes the mailbox
//! archive until someone clears it manually.
//!
//! ## Detection
//!
//! For every project archive root:
//! 1. Check if `<archive>/.git/index.lock` exists.
//! 2. If yes, read the lock body for a PID (some git versions write one;
//!    others leave the file empty). If a PID is present and `kill(pid, 0)`
//!    succeeds, the holder is live — NOT stale. Skip.
//! 3. If no PID is present, fall back to mtime: if the file's mtime is
//!    more than `stale_seconds` (default 300s / 5 min) ago, the lock is
//!    stale.
//! 4. Otherwise (recent, no PID), keep skipping — too risky to remove a
//!    lock that might belong to an in-flight writer.
//!
//! ## Fix
//!
//! `mutate(ctx, lock_path, Op::Rename { to: quarantine })`. The
//! quarantine path is `<run-dir>/quarantine/<archive-slug>/index.lock.<ns>`
//! so a stuck lock is preserved for post-mortem rather than deleted (per
//! AGENTS.md RULE 1 "no file deletion").
//!
//! ## Reversibility
//!
//! `am doctor undo <run-id>` reverses the rename, restoring the lock to
//! its original location. The H6 fix in `undo.rs` refuses to clobber a
//! recreated lock — if a new writer has put a new lock there, the undo
//! refuses rather than destroying the new one.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-archive-state-files-stale-archive-lock-from-dead-pid";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "archive_state_files";

/// Default mtime-based staleness threshold (5 minutes).
pub const DEFAULT_STALE_SECONDS: u64 = 300;

/// Per-finding payload. Distinct from the generic [`super::Finding`] so
/// fixers can read a typed payload without parsing JSON.
#[derive(Debug, Clone, Serialize)]
pub struct StaleArchiveLockFinding {
    pub archive_root: PathBuf,
    pub lock_path: PathBuf,
    pub recorded_pid: Option<u32>,
    pub age_seconds: u64,
}

impl StaleArchiveLockFinding {
    /// Project the typed payload back into the generic `Finding`
    /// envelope used by `report.json::findings[]`.
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "stale archive lock at {} (recorded_pid={:?}, age={}s)",
            self.lock_path.display(),
            self.recorded_pid,
            self.age_seconds,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.99,
            evidence: serde_json::json!({
                "archive_root": self.archive_root.to_string_lossy(),
                "lock_path": self.lock_path.to_string_lossy(),
                "recorded_pid": self.recorded_pid,
                "age_seconds": self.age_seconds,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {} --yes", FM_ID),
                explain_command: format!("am doctor explain {}", FM_ID),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Detector. PURE — no `mutate()` calls, no writes.
///
/// `archive_roots` is the list of project archives to scan (e.g.,
/// `<storage_root>/projects/<slug>/`). `stale_seconds` is the
/// mtime-based threshold; passes `DEFAULT_STALE_SECONDS` by default.
pub fn detect(archive_roots: &[PathBuf], stale_seconds: u64) -> Vec<StaleArchiveLockFinding> {
    let mut out = Vec::new();
    let now = std::time::SystemTime::now();
    for archive in archive_roots {
        let lock_path = archive.join(".git").join("index.lock");
        let meta = match fs::symlink_metadata(&lock_path) {
            Ok(m) => m,
            Err(_) => continue, // no lock or unreadable — not our problem
        };
        if !meta.file_type().is_file() {
            // Symlink-replacement attack — refuse to follow.
            continue;
        }
        // Try to read a PID from the lock body (git writes the PID
        // followed by tab-separated thread/start fields when known).
        let recorded_pid = fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_owned))
            .and_then(|first| first.trim().parse::<u32>().ok());

        if let Some(pid) = recorded_pid
            && super::is_pid_alive(pid)
        {
            continue; // live holder; not stale
        }

        let age_seconds = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if recorded_pid.is_none() && age_seconds < stale_seconds {
            // Conservative: a recent lock with no PID may belong to a
            // writer we couldn't observe (some git builds don't write a
            // PID). Don't remove it.
            continue;
        }

        out.push(StaleArchiveLockFinding {
            archive_root: archive.clone(),
            lock_path,
            recorded_pid,
            age_seconds,
        });
    }
    out
}

/// Fixer. Routes the quarantine through `mutate()`.
///
/// Per AGENTS.md RULE 1, we do NOT delete; we `Op::Rename` the lock to
/// `<run-dir>/quarantine/<archive-slug>/index.lock.<ns>`. The user can
/// inspect or delete it later.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &StaleArchiveLockFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let archive_slug = finding
        .archive_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown-archive".to_string());
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let quarantine = ctx
        .run_dir
        .join("quarantine")
        .join(&archive_slug)
        .join(format!("index.lock.{now_ns}"));

    // Pre-check the live state. If the file vanished between detect and
    // fix (a writer cleaned up after themselves), this is idempotent —
    // no work to do.
    if !finding.lock_path.exists() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }

    mutate(
        ctx,
        &finding.lock_path,
        Op::Rename {
            to: quarantine.clone(),
        },
    )?;

    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: vec![quarantine],
    })
}

// `is_pid_alive` moved to `super::is_pid_alive` (pass-9 dedup).

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

    fn make_archive(td: &TempDir, slug: &str) -> PathBuf {
        let archive = td.path().join(slug);
        fs::create_dir_all(archive.join(".git")).unwrap();
        archive
    }

    #[test]
    fn detector_returns_empty_when_no_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        let findings = detect(&[archive], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_live_pid() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // Write the current process's PID — guaranteed live.
        let live_pid = std::process::id();
        fs::write(archive.join(".git/index.lock"), format!("{live_pid}\n")).unwrap();
        let findings = detect(&[archive], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty(), "live PID must NOT be reported stale");
    }

    #[test]
    fn detector_flags_dead_pid() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // PID 0 is reserved/never-a-real-process on Linux/macOS.
        fs::write(archive.join(".git/index.lock"), "999999999\n").unwrap();
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].recorded_pid, Some(999_999_999));
        assert_eq!(findings[0].archive_root, archive);
    }

    #[test]
    fn detector_conservative_on_recent_empty_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // No PID in body; new file → don't remove.
        fs::write(archive.join(".git/index.lock"), "").unwrap();
        let findings = detect(&[archive], DEFAULT_STALE_SECONDS);
        assert!(
            findings.is_empty(),
            "recent empty-body lock must be left alone"
        );
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = StaleArchiveLockFinding {
            archive_root: PathBuf::from("/x/y"),
            lock_path: PathBuf::from("/x/y/.git/index.lock"),
            recorded_pid: Some(99999),
            age_seconds: 600,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P1");
        assert_eq!(g.subsystem, "archive_state_files");
        assert!(g.title.contains("stale archive lock"));
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 1);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"severity\":\"P1\""));
    }

    #[test]
    fn fixer_quarantines_stale_lock_via_mutate() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // Plant a stale lock with dead PID.
        let lock_path = archive.join(".git/index.lock");
        fs::write(&lock_path, "999999999\n").unwrap();
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        assert_eq!(findings.len(), 1);

        let run_id = "2026-05-10T08-00-00Z__stalelock";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);
        assert_eq!(outcome.quarantined_paths.len(), 1);

        // Lock no longer at its original location.
        assert!(!lock_path.exists(), "lock must be removed from .git/");
        // Quarantined copy exists in run-dir.
        let q = &outcome.quarantined_paths[0];
        assert!(q.exists(), "quarantined lock must exist at {}", q.display());
        assert_eq!(fs::read_to_string(q).unwrap(), "999999999\n");
    }

    #[test]
    fn fixer_idempotent_on_already_cleaned_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // Detector saw a lock; between detect and fix, a writer cleaned up.
        let finding = StaleArchiveLockFinding {
            archive_root: archive.clone(),
            lock_path: archive.join(".git/index.lock"),
            recorded_pid: Some(99999),
            age_seconds: 600,
        };
        // Lock does NOT exist (cleaned up between detect and fix).
        let run_id = "2026-05-10T08-00-01Z__alreadygone";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert!(outcome.quarantined_paths.is_empty());
    }

    #[test]
    fn fixer_then_undo_restores_lock() {
        // End-to-end round-trip: stale lock → quarantine → undo → lock back.
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        let lock_path = archive.join(".git/index.lock");
        fs::write(&lock_path, "999999999\n").unwrap();
        let findings = detect(std::slice::from_ref(&archive), DEFAULT_STALE_SECONDS);
        let run_id = "2026-05-10T08-00-02Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        drop(ctx);

        // Undo should restore the lock to its original location.
        let summary = crate::doctor::undo::run_undo_with_scopes(
            td.path(),
            run_id,
            false,
            true,
            &[td.path().to_path_buf()],
        )
        .expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        assert!(lock_path.exists(), "undo must restore the lock file");
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "999999999\n");
    }
}
