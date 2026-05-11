//! `fm-archive-state-files-stale-head-or-ref-update-lock` — P2.
//!
//! **Subsystem**: archive_state_files (Phase 1 archaeology, separate
//! FM from pass-8's stale_archive_lock).
//!
//! ## What's broken
//!
//! Pass-8 covers `.git/index.lock`. Git also writes `.git/HEAD.lock`
//! during `update-ref` operations and `.git/refs/heads/<name>.lock` /
//! `.git/refs/tags/<name>.lock` during ref-creation atomicity. If a
//! writer crashes before the rename-to-final completes, the `.lock`
//! file is left behind and subsequent git operations on that ref
//! refuse:
//!
//! ```text
//! error: cannot lock ref 'refs/heads/main': Unable to create
//! '<repo>/.git/refs/heads/main.lock': File exists.
//! ```
//!
//! Phase 1 archaeology (2026-05-09) found this as a separate FM from
//! the index.lock case because:
//! - The directory walks are different (`refs/` is recursive).
//! - These locks have a built-in mtime-recency window (`core.lockfile-
//!   retain-seconds` defaults to 60s in some git versions) — stricter
//!   threshold than index.lock.
//! - The risk profile differs — a leaked `refs/heads/main.lock` blocks
//!   ALL ref updates on `main`, which is a sharper outage than a stuck
//!   `index.lock` (which only blocks `git commit`).
//!
//! ## Detection (pure function)
//!
//! For each archive root, scan:
//! - `<archive>/.git/HEAD.lock` (single file)
//! - `<archive>/.git/refs/**/*.lock` (recursive, capped depth)
//! - `<archive>/.git/packed-refs.lock` (single file)
//!
//! For each found lock:
//! 1. Must be a regular file (refuse symlinks).
//! 2. mtime older than `stale_seconds` (default 120s = 2 min, stricter
//!    than index.lock's 5 min because ref updates are typically faster).
//! 3. Emit one finding per stale lock.
//!
//! Unlike index.lock, ref locks rarely carry a PID body. So this
//! detector relies purely on mtime, which is why the threshold is
//! tighter.
//!
//! ## Fix (routes through mutate)
//!
//! `mutate(ctx, lock_path, Op::Rename { to: quarantine })`. The
//! quarantine path encodes the original ref-relative path so an
//! operator can identify which ref was affected:
//!
//! `<run-dir>/quarantine/<archive-slug>/refs/<ref-path>.lock.<ns>`
//!
//! Per AGENTS.md RULE 1, no deletion. Reversible via `am doctor undo`.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

const FM_ID: &str = "fm-archive-state-files-stale-head-or-ref-update-lock";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "archive_state_files";

/// Default mtime-based staleness threshold (2 minutes). Stricter than
/// index.lock's 5 min because ref updates complete faster.
pub const DEFAULT_STALE_SECONDS: u64 = 120;

/// Max recursion depth when walking `refs/`. Refs trees are
/// conventionally shallow (refs/heads/<name>, refs/tags/<name>,
/// refs/remotes/<remote>/<name>) but operators can nest. Cap at 5 to
/// bound the scan even on hostile inputs.
const MAX_REFS_DEPTH: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct StaleHeadOrRefLockFinding {
    pub archive_root: PathBuf,
    pub lock_path: PathBuf,
    /// What kind of lock this is, for the `actions_planned` envelope.
    pub kind: LockKind,
    pub age_seconds: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum LockKind {
    Head,
    PackedRefs,
    /// Carries the ref's relative path (e.g. `refs/heads/main`).
    /// We serialize as a tag-only variant for envelope simplicity;
    /// the path is captured in `lock_path` itself.
    Ref,
}

impl StaleHeadOrRefLockFinding {
    pub fn to_finding(&self) -> super::Finding {
        let kind_str = match self.kind {
            LockKind::Head => "HEAD.lock",
            LockKind::PackedRefs => "packed-refs.lock",
            LockKind::Ref => "refs/*/*.lock",
        };
        let title = format!(
            "stale {} at {} (age={}s)",
            kind_str,
            self.lock_path.display(),
            self.age_seconds,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.90, // mtime-only is slightly less certain than dead-PID
            evidence: serde_json::json!({
                "archive_root": self.archive_root.to_string_lossy(),
                "lock_path": self.lock_path.to_string_lossy(),
                "kind": kind_str,
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
pub fn detect(archive_roots: &[PathBuf], stale_seconds: u64) -> Vec<StaleHeadOrRefLockFinding> {
    let mut out = Vec::new();
    let now = std::time::SystemTime::now();
    for archive in archive_roots {
        let git_dir = archive.join(".git");
        if !git_dir.is_dir() {
            continue; // not a git archive
        }
        // 1. HEAD.lock
        check_single_lock(
            &git_dir.join("HEAD.lock"),
            LockKind::Head,
            archive,
            now,
            stale_seconds,
            &mut out,
        );
        // 2. packed-refs.lock
        check_single_lock(
            &git_dir.join("packed-refs.lock"),
            LockKind::PackedRefs,
            archive,
            now,
            stale_seconds,
            &mut out,
        );
        // 3. refs/**/*.lock (recursive)
        let refs_dir = git_dir.join("refs");
        walk_refs(&refs_dir, 0, archive, now, stale_seconds, &mut out);
    }
    out
}

fn check_single_lock(
    lock_path: &Path,
    kind: LockKind,
    archive: &Path,
    now: std::time::SystemTime,
    stale_seconds: u64,
    out: &mut Vec<StaleHeadOrRefLockFinding>,
) {
    let meta = match fs::symlink_metadata(lock_path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if !meta.file_type().is_file() {
        return; // symlink defense
    }
    let age_seconds = meta
        .modified()
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if age_seconds < stale_seconds {
        return; // recent — conservative
    }
    out.push(StaleHeadOrRefLockFinding {
        archive_root: archive.to_path_buf(),
        lock_path: lock_path.to_path_buf(),
        kind,
        age_seconds,
    });
}

fn walk_refs(
    cur: &Path,
    depth: usize,
    archive: &Path,
    now: std::time::SystemTime,
    stale_seconds: u64,
    out: &mut Vec<StaleHeadOrRefLockFinding>,
) {
    if depth > MAX_REFS_DEPTH {
        return;
    }
    let entries = match fs::read_dir(cur) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk_refs(&path, depth + 1, archive, now, stale_seconds, out);
        } else if ft.is_file() && path.extension().map(|e| e == "lock").unwrap_or(false) {
            check_single_lock(&path, LockKind::Ref, archive, now, stale_seconds, out);
        }
    }
}

/// Fixer. Routes the quarantine through `mutate()`.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &StaleHeadOrRefLockFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    let archive_slug = finding
        .archive_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown-archive".to_string());
    // Use the lock's path relative to .git/ for quarantine layout.
    let git_dir = finding.archive_root.join(".git");
    let rel = finding
        .lock_path
        .strip_prefix(&git_dir)
        .unwrap_or(&finding.lock_path)
        .to_path_buf();
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let quarantine = ctx
        .run_dir
        .join("quarantine")
        .join(&archive_slug)
        .join(format!("{}.{now_ns}", rel.display()));

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
        fs::create_dir_all(archive.join(".git/refs/heads")).unwrap();
        archive
    }

    #[test]
    fn detector_skips_non_git_dirs() {
        let td = TempDir::new().unwrap();
        let not_git = td.path().join("alpha");
        fs::create_dir_all(&not_git).unwrap();
        let findings = detect(&[not_git], DEFAULT_STALE_SECONDS);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_stale_head_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // HEAD.lock with threshold 0 → always stale.
        fs::write(archive.join(".git/HEAD.lock"), "").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, LockKind::Head);
    }

    #[test]
    fn detector_flags_stale_packed_refs_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        fs::write(archive.join(".git/packed-refs.lock"), "").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, LockKind::PackedRefs);
    }

    #[test]
    fn detector_flags_stale_branch_ref_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        fs::write(archive.join(".git/refs/heads/main.lock"), "").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, LockKind::Ref);
        assert!(findings[0].lock_path.ends_with("refs/heads/main.lock"));
    }

    #[test]
    fn detector_skips_recent_locks() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        fs::write(archive.join(".git/HEAD.lock"), "").unwrap();
        // High threshold → just-created file is NOT stale.
        let findings = detect(&[archive], 10_000);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_finds_multiple_locks_per_archive() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        fs::write(archive.join(".git/HEAD.lock"), "").unwrap();
        fs::write(archive.join(".git/packed-refs.lock"), "").unwrap();
        fs::write(archive.join(".git/refs/heads/main.lock"), "").unwrap();
        fs::write(archive.join(".git/refs/heads/develop.lock"), "").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        assert_eq!(findings.len(), 4);
    }

    #[test]
    fn detector_refuses_symlink_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // Plant a target file outside, then symlink HEAD.lock to it.
        let target = td.path().join("evil_target.txt");
        fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, archive.join(".git/HEAD.lock")).unwrap();
        let findings = detect(&[archive], 0);
        assert!(
            findings.is_empty(),
            "symlinked HEAD.lock must be refused (symlink-attack defense)"
        );
    }

    #[test]
    fn detector_respects_max_recursion_depth() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        // Build a deeply nested ref tree beyond MAX_REFS_DEPTH.
        let deep = archive.join(".git/refs").join("a/b/c/d/e/f/g/h"); // 8 levels deep
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("locked.lock"), "").unwrap();
        let findings = detect(&[archive], 0);
        // Beyond depth 5 → not found.
        assert!(
            findings.is_empty(),
            "lock beyond MAX_REFS_DEPTH must not be reported"
        );
    }

    #[test]
    fn fixer_quarantines_head_lock_via_mutate() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        let lock_path = archive.join(".git/HEAD.lock");
        fs::write(&lock_path, "").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        assert_eq!(findings.len(), 1);
        let run_id = "2026-05-10T09-00-00Z__headlock";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert!(!lock_path.exists());
        let q = &outcome.quarantined_paths[0];
        assert!(q.exists());
        // Quarantine path should encode the original location.
        assert!(q.to_string_lossy().contains("HEAD.lock"));
    }

    #[test]
    fn fixer_quarantines_branch_ref_lock_with_ref_path_encoded() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        let lock_path = archive.join(".git/refs/heads/main.lock");
        fs::write(&lock_path, "").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        let run_id = "2026-05-10T09-00-01Z__reflock";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        let q = &outcome.quarantined_paths[0];
        // Should contain "refs/heads/main.lock" so operators can identify
        // which ref the leaked lock came from.
        assert!(
            q.to_string_lossy().contains("refs/heads/main.lock"),
            "quarantine path must encode the ref: {}",
            q.display()
        );
    }

    #[test]
    fn fixer_then_undo_restores_ref_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        let lock_path = archive.join(".git/refs/heads/main.lock");
        fs::write(&lock_path, "abc").unwrap();
        let findings = detect(std::slice::from_ref(&archive), 0);
        let run_id = "2026-05-10T09-00-02Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        drop(ctx);
        let summary = crate::doctor::undo::run_undo(td.path(), run_id, false, true).expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        assert!(lock_path.exists());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "abc");
    }

    #[test]
    fn fixer_idempotent_on_already_cleaned_lock() {
        let td = TempDir::new().unwrap();
        let archive = make_archive(&td, "alpha");
        let finding = StaleHeadOrRefLockFinding {
            archive_root: archive.clone(),
            lock_path: archive.join(".git/HEAD.lock"),
            kind: LockKind::Head,
            age_seconds: 300,
        };
        let run_id = "2026-05-10T09-00-03Z__cleanup";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
