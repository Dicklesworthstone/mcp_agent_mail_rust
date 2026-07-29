//! `fm-doctor-state-files-dangling-latest-symlink` — P2.
//!
//! **Subsystem**: doctor_state_files (Phase 1 archaeology — internal
//! doctor bookkeeping, not user state).
//!
//! ## What's broken
//!
//! `<repo>/.doctor/latest` is the canonical "most recent run"
//! symlink. `am doctor explain`, `am doctor health`, and tooling
//! that wants to read `report.json` from the last invocation
//! resolve it. If it points at a `runs/<id>` directory that no
//! longer exists (because the operator manually pruned or moved
//! the run directory after `am doctor undo`),
//! the symlink dangles. Tools that resolve it get
//! `ENOENT` and surface confusing "no recent run" errors even
//! though other runs may exist.
//!
//! Pass-22 fixed the related bypass that wrote `.doctor/` into
//! `.gitignore` outside the chokepoint. This FM is the symmetric
//! recovery surface: when the symlink itself is broken, re-aim
//! it at the newest surviving run-dir through `mutate()` so the
//! operation is reversible.
//!
//! ## Detection (pure function)
//!
//! Given `latest_path` (typically `<repo>/.doctor/latest`):
//! 1. If `fs::symlink_metadata` fails / target is not a symlink → no finding.
//! 2. Read the link target via `fs::read_link`. Resolve it
//!    relative to `latest_path`'s parent.
//! 3. If the resolved target exists → no finding (healthy).
//! 4. Otherwise emit a finding noting the current dangling target.
//!
//! ## Fix (`Op::SymlinkAtomic` — new pattern at FM level)
//!
//! Scan `<doctor_root>/runs/` for the most-recently-modified
//! subdirectory. If found, call `mutate(ctx, latest_path,
//! Op::SymlinkAtomic { target: runs/<newest_id> })`. The
//! chokepoint records before/after hashes of the symlink TARGET
//! STRING (not the file the symlink resolves to), so
//! `am doctor undo` can restore the original (dangling) symlink
//! byte-for-byte — useful for forensics.
//!
//! If no run-dir exists → the fixer returns `actions_skipped: 1`.
//! Operators who want the symlink out of the way in that case can
//! move it aside for forensics (the doctor never deletes per
//! AGENTS.md RULE 1).
//!
//! Demonstrates the sixth canonical write-shape at FM level:
//! - Pass 8/9/10: `Op::Rename` (3 FMs)
//! - Pass 11: detect-only (1 FM)
//! - Pass 12: `Op::Chmod` (2 FMs — token-bak, storage-db)
//! - Pass 13: `Op::WriteFile` (1 FM — mcp-url)
//! - Pass 21: `Op::AppendFile` (1 FM — gitignore)
//! - **Pass 28: `Op::SymlinkAtomic`** ← this
//!
//! Remaining canonical Ops without FM coverage: `Op::DbExec`
//! and `Op::DbMigrate` (both stubbed in the chokepoint, waiting
//! on `DbConn` plumbing).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-doctor-state-files-dangling-latest-symlink";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "doctor_state_files";

#[derive(Debug, Clone, Serialize)]
pub struct DanglingDoctorLatestFinding {
    pub latest_path: PathBuf,
    /// The (resolved) target path that the symlink points at.
    pub dangling_target: PathBuf,
}

impl DanglingDoctorLatestFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} symlink points at non-existent {}",
            self.latest_path.display(),
            self.dangling_target.display()
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "latest_path": self.latest_path.to_string_lossy(),
                "dangling_target": self.dangling_target.to_string_lossy(),
                "remediation_strategy": "re-aim at most-recent surviving runs/<id> via Op::SymlinkAtomic",
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
/// `latest_path` is typically `<repo>/.doctor/latest`. Resolves
/// the symlink target relative to its parent dir; if the resolved
/// target doesn't exist, emits a finding.
pub fn detect(latest_path: &Path) -> Vec<DanglingDoctorLatestFinding> {
    let meta = match fs::symlink_metadata(latest_path) {
        Ok(m) => m,
        Err(_) => return Vec::new(), // no symlink at all → not our problem
    };
    if !meta.file_type().is_symlink() {
        // A regular file at .doctor/latest is a different failure
        // (handled by `runs::update_latest_symlink`'s refusal path);
        // not our concern here.
        return Vec::new();
    }
    let target = match fs::read_link(latest_path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    // Resolve relative to parent.
    let resolved = if target.is_absolute() {
        target.clone()
    } else {
        latest_path
            .parent()
            .map(|p| p.join(&target))
            .unwrap_or(target.clone())
    };
    if resolved.exists() {
        return Vec::new(); // healthy
    }
    vec![DanglingDoctorLatestFinding {
        latest_path: latest_path.to_path_buf(),
        dangling_target: target,
    }]
}

/// Fixer. Routes through `mutate()` with `Op::SymlinkAtomic`.
///
/// Picks the newest existing `runs/<id>` directory by mtime. If
/// no run-dir exists, no-ops (returns actions_skipped: 1).
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &DanglingDoctorLatestFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    // Compute the canonical runs/ dir from the symlink's parent.
    let Some(doctor_root) = finding.latest_path.parent() else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    let runs_dir = doctor_root.join("runs");
    let newest = pick_newest_run_dir(&runs_dir);
    let Some(newest) = newest else {
        // No surviving run-dirs. Per AGENTS.md RULE 1 the doctor
        // never deletes; an operator who wants the broken symlink
        // gone removes it themselves.
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    // The atomic_symlink primitive expects a target Path; using
    // a relative path keeps the symlink portable across moves of
    // the doctor root.
    let relative_target = PathBuf::from("runs").join(&newest);

    mutate(
        ctx,
        &finding.latest_path,
        Op::SymlinkAtomic {
            target: relative_target,
        },
    )?;

    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: Vec::new(),
    })
}

/// Return the file_name of the most-recently-modified
/// subdirectory of `runs_dir`, or `None` if none exist.
fn pick_newest_run_dir(runs_dir: &Path) -> Option<std::ffi::OsString> {
    let entries = fs::read_dir(runs_dir).ok()?;
    let mut best: Option<(std::time::SystemTime, std::ffi::OsString)> = None;
    for entry in entries.flatten() {
        if entry
            .file_type()
            .map_or(true, |file_type| !file_type.is_dir())
        {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        let mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let name = entry.file_name();
        match &best {
            None => best = Some((mtime, name)),
            Some((cur, _)) if mtime > *cur => best = Some((mtime, name)),
            _ => {}
        }
    }
    best.map(|(_, name)| name)
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
    fn detector_returns_empty_when_no_symlink() {
        let td = TempDir::new().unwrap();
        let p = td.path().join(".doctor").join("latest");
        let findings = detect(&p);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_returns_empty_when_target_exists() {
        let td = TempDir::new().unwrap();
        let doctor_root = td.path().join(".doctor");
        let runs = doctor_root.join("runs");
        let run_id = "2026-05-13T00-00-00Z__abc";
        let run_dir = runs.join(run_id);
        fs::create_dir_all(&run_dir).unwrap();
        let latest = doctor_root.join("latest");
        let rel = PathBuf::from("runs").join(run_id);
        std::os::unix::fs::symlink(&rel, &latest).unwrap();

        let findings = detect(&latest);
        assert!(findings.is_empty(), "live target must not flag");
    }

    #[test]
    fn detector_flags_dangling_symlink() {
        let td = TempDir::new().unwrap();
        let doctor_root = td.path().join(".doctor");
        fs::create_dir_all(&doctor_root).unwrap();
        let latest = doctor_root.join("latest");
        let rel = PathBuf::from("runs").join("2026-05-13T00-00-00Z__nope");
        std::os::unix::fs::symlink(&rel, &latest).unwrap();

        let findings = detect(&latest);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].latest_path, latest);
    }

    #[test]
    fn detector_returns_empty_for_regular_file_at_latest() {
        let td = TempDir::new().unwrap();
        let doctor_root = td.path().join(".doctor");
        fs::create_dir_all(&doctor_root).unwrap();
        let latest = doctor_root.join("latest");
        fs::write(&latest, b"not a symlink").unwrap();

        let findings = detect(&latest);
        assert!(
            findings.is_empty(),
            "regular file at .doctor/latest is a separate failure (handled by update_latest_symlink refusal)"
        );
    }

    #[test]
    fn fixer_re_aims_at_newest_surviving_run_dir() {
        let td = TempDir::new().unwrap();
        let doctor_root = td.path().join(".doctor");
        let runs = doctor_root.join("runs");
        // Two run dirs: older and newer. We'll dangling-point at a third.
        let older = "2026-05-13T00-00-00Z__older";
        let newer = "2026-05-13T01-00-00Z__newer";
        fs::create_dir_all(runs.join(older)).unwrap();
        fs::create_dir_all(runs.join(newer)).unwrap();
        // Bump newer's mtime to ensure ordering even on filesystems
        // with second-resolution timestamps.
        let now = std::time::SystemTime::now();
        let later = now + std::time::Duration::from_secs(120);
        let times = fs::FileTimes::new().set_modified(later);
        let f = fs::File::options()
            .read(true)
            .open(runs.join(newer))
            .unwrap();
        f.set_times(times).unwrap();
        drop(f);

        let latest = doctor_root.join("latest");
        let dangling = PathBuf::from("runs").join("2026-05-13T02-00-00Z__nope");
        std::os::unix::fs::symlink(&dangling, &latest).unwrap();

        let findings = detect(&latest);
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-13T03-00-00Z__symlink_fix");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        // Symlink now points at the newer run.
        let target = fs::read_link(&latest).unwrap();
        let target_str = target.to_string_lossy();
        assert!(
            target_str.ends_with(newer),
            "expected symlink to target newer run, got {target_str}"
        );
        // And the symlink resolves (target exists).
        let resolved = doctor_root.join(&target);
        assert!(resolved.exists(), "re-aimed target must exist");
    }

    #[test]
    fn fixer_skips_when_no_surviving_run_dirs() {
        // The fixer's `runs_dir` is derived from
        // `latest_path.parent()`, while `ctx_for` writes its own
        // run-dir under `<ctx-tempdir>/.doctor/runs/`. To honestly
        // exercise the "no surviving runs" branch we isolate the
        // FM's .doctor/ tree under a subdir so the ctx tempdir's
        // run-dir doesn't bleed in.
        let td = TempDir::new().unwrap();
        let isolated_root = td.path().join("isolated_repo");
        let doctor_root = isolated_root.join(".doctor");
        fs::create_dir_all(doctor_root.join("runs")).unwrap();
        let latest = doctor_root.join("latest");
        let dangling = PathBuf::from("runs").join("nope");
        std::os::unix::fs::symlink(&dangling, &latest).unwrap();
        let findings = detect(&latest);
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-13T00-00-00Z__symlink_empty");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // Symlink unchanged (still dangling).
        let target = fs::read_link(&latest).unwrap();
        assert_eq!(target, dangling);
    }

    #[test]
    fn newest_run_picker_ignores_symlinked_directories() {
        let td = TempDir::new().unwrap();
        let runs = td.path().join("runs");
        let outside = td.path().join("outside-run");
        fs::create_dir_all(&runs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, runs.join("linked-run")).unwrap();

        assert_eq!(
            pick_newest_run_dir(&runs),
            None,
            "symlinked directories under .doctor/runs must not be selected"
        );
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = DanglingDoctorLatestFinding {
            latest_path: PathBuf::from("/x/y/.doctor/latest"),
            dangling_target: PathBuf::from("runs/zzz"),
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P2");
        assert_eq!(g.subsystem, "doctor_state_files");
        assert!(g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("runs/zzz"));
    }
}
