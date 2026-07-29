//! `fm-guard_install-plugin-symlink-replacement` — P1 detect-only.
//!
//! **Subsystem**: guard_install.
//!
//! ## What's broken
//!
//! One or more of the four canonical agent-mail guard paths is
//! now a **symlink** instead of the regular file the installer
//! wrote:
//!
//! - `<hooks_dir>/pre-commit`
//! - `<hooks_dir>/pre-push`
//! - `<hooks_dir>/hooks.d/pre-commit/50-agent-mail.py`
//! - `<hooks_dir>/hooks.d/pre-push/50-agent-mail.py`
//!
//! Symlinks here are a security signal: an attacker (or a
//! misconfigured deploy script, or a worktree migration) has
//! redirected the guard at a target that may not validate
//! reservations. The chokepoint's
//! `guard_plugin_not_executable` FM (FM17) deliberately SKIPS
//! symlinked entries — this FM owns them.
//!
//! Severity P1: when the target is outside the project's hooks
//! dir, the link could point at anything (a no-op `/bin/true`, an
//! attacker-controlled script, a stale binary from a prior
//! install). Even a benign-looking target like
//! `<workdir>/scripts/pre-commit` is suspect, because the
//! installer never writes symlinks.
//!
//! ## Detection (pure)
//!
//! 1. Discover the git repo. If not a git repo, return empty.
//! 2. Resolve the active hooks dir via
//!    `mcp_agent_mail_guard::resolve_hooks_dir(repo_root)`.
//! 3. For each of the 4 candidate paths: read `symlink_metadata`.
//!    If the entry is a symlink, record the path + `read_link`
//!    target (best-effort; broken-link `read_link` returns the
//!    target string regardless).
//! 4. Emit one aggregated finding if any symlink is found.
//!
//! Note: this detector pairs with `guard_plugin_not_executable`
//! (FM17). That FM skips symlinks; this FM only flags symlinks.
//! Operators can run both safely — there's no double-emission.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** Repair_spec calls for Op::Rename
//! the SYMLINK ITSELF (not the target) to a quarantine path,
//! then re-render the canonical file via the installer. That's
//! a multi-step plan with a per-symlink quarantine layout +
//! installer-output round-trip test. Deferred. Manual remediation:
//!
//! 1. Inspect each symlink target: `ls -la <path>` and `readlink
//!    <path>`. If the target is unfamiliar (especially outside
//!    `<repo>` or the active hooks dir), treat as a security
//!    incident.
//! 2. Move the symlink aside (NOT the target): `mv <symlink>
//!    .doctor/quarantine/guard-symlinks/<basename>`. Op::Rename
//!    on a symlink relocates the LINK, not the target — `mv`
//!    behaves the same.
//! 3. Re-run `am install-precommit-guard --project <abs-path>` to
//!    write fresh regular files at the canonical paths.
//! 4. Re-run `am doctor fix --only fm-guard_install-plugin-symlink-replacement --list`
//!    to confirm zero residual symlinks.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-guard_install-plugin-symlink-replacement";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "guard_install";

/// Canonical plugin filename — must stay in sync with
/// `mcp_agent_mail_guard::PLUGIN_FILE_NAME` (private to that
/// crate; we duplicate locally with a reminder).
const PLUGIN_FILE_NAME: &str = "50-agent-mail.py";

#[derive(Debug, Clone, Serialize)]
pub struct SymlinkedGuardEntry {
    pub path: PathBuf,
    /// Best-effort symlink target (the raw value, NOT canonical-
    /// ized — operators need the literal target for triage).
    pub target: PathBuf,
    /// Whether the target resolves to an existing path
    /// (`std::fs::metadata` succeeds after the link is followed).
    /// `false` here is a "dangling link" signal: the guard is
    /// completely broken — `git commit` errors with ENOENT.
    pub target_exists: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardPluginSymlinkFinding {
    pub hooks_dir: PathBuf,
    pub entries: Vec<SymlinkedGuardEntry>,
}

impl GuardPluginSymlinkFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} agent-mail guard path(s) under {} are symlinks (installer always writes regular files)",
            self.entries.len(),
            self.hooks_dir.display(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "hooks_dir": self.hooks_dir.to_string_lossy(),
                "entries": self.entries,
                "any_dangling": self.entries.iter().any(|e| !e.target_exists),
                "manual_remediation": {
                    "steps": [
                        "For each entry: `ls -la <path>` and `readlink <path>` to inspect. Treat any target outside `<repo>` or the active hooks dir as a security incident — investigate before rewriting.",
                        "Move the SYMLINK ITSELF (NOT the target) aside: `mkdir -p .doctor/quarantine/guard-symlinks && mv <symlink_path> .doctor/quarantine/guard-symlinks/`. `mv` on a symlink relocates the link, not the target — that's what we want.",
                        "Re-run `am install-precommit-guard --project <abs-path>` to write fresh regular files at the canonical paths.",
                        "Re-run `am doctor fix --only fm-guard_install-plugin-symlink-replacement --list` to confirm zero residual symlinks.",
                    ],
                    "warning": "SECURITY signal: the installer never writes symlinks at these paths. A symlink here means SOMETHING (or someone) substituted the guard. If the target is outside the repo or hooks dir, treat as a potential attacker-aliasing incident and preserve forensics.",
                    "safe_fix_deferred": "Auto-fix via Op::Rename + re-render is intentionally deferred in this first cut. Op::Rename on a symlink relocates the link (not the target) — the chokepoint already supports this — but the re-render step needs the installer's render functions wired into the doctor binary, plus a round-trip test that confirms the installer's output is byte-identical to the original.",
                    "common_causes": [
                        "Operator manually `ln -s` to point the guard at a different script.",
                        "Worktree migration that copied the install dir but rewrote files as symlinks.",
                        "Deploy script that `cp -s` (copy-as-symlink) the install dir.",
                        "ATTACKER substituted the guard to bypass reservations.",
                    ],
                },
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

/// Detector. PURE w.r.t. the supplied `repo_root`.
///
/// Returns at most one aggregated finding per call. Pairs with
/// `guard_plugin_not_executable` (FM17) — that FM skips
/// symlinks; this FM only flags them.
pub fn detect(repo_root: &Path) -> Vec<GuardPluginSymlinkFinding> {
    let Ok(hooks_dir) = mcp_agent_mail_guard::resolve_hooks_dir(repo_root) else {
        return Vec::new();
    };
    let candidates: [PathBuf; 4] = [
        hooks_dir.join("pre-commit"),
        hooks_dir.join("pre-push"),
        hooks_dir
            .join("hooks.d")
            .join("pre-commit")
            .join(PLUGIN_FILE_NAME),
        hooks_dir
            .join("hooks.d")
            .join("pre-push")
            .join(PLUGIN_FILE_NAME),
    ];
    let mut entries: Vec<SymlinkedGuardEntry> = Vec::new();
    for path in candidates {
        let Ok(lmeta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !lmeta.file_type().is_symlink() {
            continue;
        }
        // read_link succeeds on a symlink regardless of target
        // existence (returns the raw target).
        let target = std::fs::read_link(&path).unwrap_or_else(|_| PathBuf::new());
        let target_exists = std::fs::metadata(&path).is_ok();
        entries.push(SymlinkedGuardEntry {
            path,
            target,
            target_exists,
        });
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![GuardPluginSymlinkFinding { hooks_dir, entries }]
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &GuardPluginSymlinkFinding,
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
    use std::fs;
    use tempfile::TempDir;

    fn init_repo(td: &TempDir) -> PathBuf {
        let repo = td.path().to_path_buf();
        git2::Repository::init(&repo).unwrap();
        let hooks = repo.join(".git").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        repo
    }

    /// **NEGATIVE TEST FIRST**: non-git dir → no finding (no
    /// hooks dir to resolve).
    #[test]
    fn detector_returns_empty_for_non_git_directory() {
        let td = TempDir::new().unwrap();
        assert!(detect(td.path()).is_empty());
    }

    /// **NEGATIVE**: git repo, no hook paths exist at all → no
    /// finding.
    #[test]
    fn detector_returns_empty_for_git_repo_with_no_hooks() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        assert!(detect(&repo).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detector_returns_empty_when_all_paths_are_regular_files() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let hooks = repo.join(".git").join("hooks");
        fs::write(hooks.join("pre-commit"), b"#!/bin/sh\n").unwrap();
        fs::write(hooks.join("pre-push"), b"#!/bin/sh\n").unwrap();
        let findings = detect(&repo);
        assert!(
            findings.is_empty(),
            "regular files must not flag this FM (FM17 owns the mode check)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_symlinked_pre_commit_with_existing_target() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let real = repo.join("real_pre_commit.sh");
        fs::write(&real, b"#!/bin/sh\necho hi\n").unwrap();
        symlink(&real, repo.join(".git/hooks/pre-commit")).unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1, "must produce exactly one finding");
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert!(f.entries[0].path.ends_with("pre-commit"));
        assert!(f.entries[0].target_exists, "target must resolve");
        assert_eq!(f.entries[0].target, real);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_dangling_symlink_with_target_exists_false() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        symlink(
            "/nonexistent/dangling/target",
            repo.join(".git/hooks/pre-commit"),
        )
        .unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        let entry = &findings[0].entries[0];
        assert!(
            !entry.target_exists,
            "dangling-link case must record target_exists=false"
        );
        assert_eq!(entry.target, PathBuf::from("/nonexistent/dangling/target"));
    }

    #[cfg(unix)]
    #[test]
    fn detector_aggregates_multiple_symlinks_into_one_finding() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let target = repo.join("decoy.sh");
        fs::write(&target, b"#!/bin/sh\n").unwrap();
        symlink(&target, repo.join(".git/hooks/pre-commit")).unwrap();
        symlink(&target, repo.join(".git/hooks/pre-push")).unwrap();
        // Plugin path: needs the hooks.d/<hook>/ subdir.
        fs::create_dir_all(repo.join(".git/hooks/hooks.d/pre-commit")).unwrap();
        symlink(
            &target,
            repo.join(".git/hooks/hooks.d/pre-commit/50-agent-mail.py"),
        )
        .unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn detector_does_not_double_emit_with_fm17_skip_path() {
        // Layout: pre-commit is a symlink (this FM's territory),
        // pre-push is a regular file with mode 0o644 (FM17's
        // territory). This FM must flag ONLY the symlink — not
        // touch pre-push, which FM17 owns.
        use std::os::unix::fs::{PermissionsExt, symlink};
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let real = repo.join("real_pre_commit.sh");
        fs::write(&real, b"#!/bin/sh\n").unwrap();
        symlink(&real, repo.join(".git/hooks/pre-commit")).unwrap();
        let pre_push = repo.join(".git/hooks/pre-push");
        fs::write(&pre_push, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&pre_push).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&pre_push, perms).unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].entries.len(),
            1,
            "must NOT flag the regular pre-push"
        );
        assert!(findings[0].entries[0].path.ends_with("pre-commit"));
    }

    #[test]
    fn finding_serializes_with_any_dangling_flag_and_remediation() {
        let f = GuardPluginSymlinkFinding {
            hooks_dir: "/tmp/.git/hooks".into(),
            entries: vec![
                SymlinkedGuardEntry {
                    path: "/tmp/.git/hooks/pre-commit".into(),
                    target: "/some/decoy.sh".into(),
                    target_exists: true,
                },
                SymlinkedGuardEntry {
                    path: "/tmp/.git/hooks/pre-push".into(),
                    target: "/dangling/path".into(),
                    target_exists: false,
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"any_dangling\":true"));
        assert!(s.contains("SECURITY signal"));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am install-precommit-guard"));
    }

    #[test]
    fn finding_any_dangling_false_when_all_targets_resolve() {
        let f = GuardPluginSymlinkFinding {
            hooks_dir: "/tmp/.git/hooks".into(),
            entries: vec![SymlinkedGuardEntry {
                path: "/tmp/.git/hooks/pre-commit".into(),
                target: "/some/real.sh".into(),
                target_exists: true,
            }],
        };
        let s = serde_json::to_string(&f.to_finding()).unwrap();
        assert!(s.contains("\"any_dangling\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = GuardPluginSymlinkFinding {
            hooks_dir: td.path().to_path_buf(),
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
