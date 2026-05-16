//! `fm-guard_install-plugin-not-executable` — P1 detect-only first cut.
//!
//! **Subsystem**: guard_install.
//!
//! ## What's broken
//!
//! `am install-precommit-guard` writes a small Python plugin and a
//! shell shim into the project's git hooks dir. Both need the
//! POSIX user-exec bit (`0o100`) so `git commit` can spawn them.
//! When that bit is missing — typically because:
//!
//! - the user ran `chmod -x` (or a deploy script did),
//! - `git checkout` materialized the hooks via a worktree that
//!   strips mode bits,
//! - a system umask quirk dropped the exec bit on write, or
//! - an archive extraction (tar / zip / `cp`) lost the bit —
//!
//! `git commit` silently bypasses the pre-commit guard. The user
//! sees no error; reservation violations sail straight to the
//! repo. This is a P1 because the guard is the project's
//! defense-in-depth against reservation bypass.
//!
//! Distinct from `fm-guard_install-plugin-symlink-replacement`
//! (which handles the case where someone replaced the plugin
//! with a symlink to a different binary) and from
//! `fm-guard_install-foreign-runner-overwrite` (which handles
//! the case where a different tool installed its own
//! `pre-commit` shim on top of ours).
//!
//! ## Detection (pure)
//!
//! 1. Windows: return empty (POSIX exec bits don't apply).
//! 2. Resolve the hooks dir via
//!    `mcp_agent_mail_guard::resolve_hooks_dir(repo_root)`. If
//!    the resolve fails (not a git repo, bare, missing workdir,
//!    etc.) return empty — there's no install to check.
//! 3. Enumerate the four canonical hook paths:
//!    - `<hooks_dir>/pre-commit`
//!    - `<hooks_dir>/pre-push`
//!    - `<hooks_dir>/hooks.d/pre-commit/50-agent-mail.py`
//!    - `<hooks_dir>/hooks.d/pre-push/50-agent-mail.py`
//! 4. For each existing path that is a **regular file** (not a
//!    symlink — that's a different FM): if the mode is missing
//!    the user-exec bit (`mode & 0o100 == 0`), record it.
//! 5. Emit one aggregated finding if any path is non-executable.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair_spec calls for an
//! `Op::Chmod { mode: 0o755 }` per affected path through the
//! chokepoint, which is straightforward once a per-FM round-trip
//! test is wired (corrupt → fix → undo → byte-identical-mode).
//! That's deferred to a follow-up pass. Manual remediation:
//! `chmod 755 <path>` per affected entry.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-guard_install-plugin-not-executable";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "guard_install";

/// Target mode for the future auto-fix. Surfaced in the finding
/// so operators running `chmod` manually know the canonical
/// value without consulting docs.
const EXPECTED_MODE: u32 = 0o755;

/// Canonical filename of the agent-mail plugin under
/// `<hooks_dir>/hooks.d/<hook>/<PLUGIN_FILE_NAME>`. Must match
/// `mcp_agent_mail_guard::PLUGIN_FILE_NAME` (which is private to
/// the guard crate, so we duplicate the constant here with a
/// reminder to keep them in sync).
const PLUGIN_FILE_NAME: &str = "50-agent-mail.py";

#[derive(Debug, Clone, Serialize)]
pub struct NonExecutableEntry {
    pub path: PathBuf,
    /// Current POSIX mode (masked to 0o7777).
    pub current_mode: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardPluginNotExecutableFinding {
    pub hooks_dir: PathBuf,
    pub entries: Vec<NonExecutableEntry>,
    pub expected_mode: u32,
}

impl GuardPluginNotExecutableFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} guard hook(s) under {} are missing user-exec bit (expected 0o{:o})",
            self.entries.len(),
            self.hooks_dir.display(),
            self.expected_mode,
        );
        let entries_json: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "path": e.path.to_string_lossy(),
                    "current_mode_octal": format!("0o{:o}", e.current_mode),
                })
            })
            .collect();
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "hooks_dir": self.hooks_dir.to_string_lossy(),
                "entries": entries_json,
                "expected_mode_octal": format!("0o{:o}", self.expected_mode),
                "manual_remediation": {
                    "steps": [
                        "For each entry, restore the user-exec bit: `chmod 755 <path>` (the canonical mode the installer writes).",
                        "Re-run `am doctor fix --only fm-guard_install-plugin-not-executable --list` to confirm zero residual hooks.",
                        "Confirm `git commit` triggers the agent-mail guard end-to-end: `git commit --allow-empty -m smoke` should report the guard's banner on stderr.",
                    ],
                    "warning": "When the user-exec bit is missing, `git commit` silently bypasses the guard — reservation violations land in the repo without any error. Treat as P1.",
                    "safe_fix_deferred": "Auto-fix via Op::Chmod { mode: 0o755 } is straightforward but intentionally deferred in this first cut. The chokepoint already implements Op::Chmod (see `world_readable_storage_db` and `world_readable_token_bak`); a follow-up pass wires it for these hook paths with a round-trip test (corrupt → fix → undo → byte-identical-mode).",
                    "common_causes": [
                        "Manual `chmod -x` (or `chmod 644`) on a hook path.",
                        "`git checkout` from a worktree filesystem that strips mode bits.",
                        "Restrictive system umask that dropped the exec bit on write.",
                        "Tar / zip / `cp` extraction that lost the bit.",
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
/// Returns at most one finding per call (multi-path entries are
/// aggregated). Windows: returns empty unconditionally.
pub fn detect(repo_root: &Path) -> Vec<GuardPluginNotExecutableFinding> {
    if cfg!(windows) {
        return Vec::new();
    }
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
    let mut entries: Vec<NonExecutableEntry> = Vec::new();
    for path in candidates {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // Symlinks are out of scope — the symlink-replacement FM
        // owns that case.
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o7777;
            if (mode & 0o100) == 0 {
                entries.push(NonExecutableEntry {
                    path,
                    current_mode: mode,
                });
            }
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![GuardPluginNotExecutableFinding {
        hooks_dir,
        entries,
        expected_mode: EXPECTED_MODE,
    }]
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &GuardPluginNotExecutableFinding,
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

    /// **NEGATIVE TEST FIRST**: a non-git directory returns empty
    /// (resolve_hooks_dir fails on a plain dir).
    #[test]
    fn detector_returns_empty_for_non_git_directory() {
        let td = TempDir::new().unwrap();
        let findings = detect(td.path());
        assert!(findings.is_empty(), "non-git dir must not flag");
    }

    /// **NEGATIVE**: a git repo with no installed hooks returns
    /// empty (the four candidate paths don't exist).
    #[test]
    fn detector_returns_empty_for_git_repo_without_installed_hooks() {
        let td = TempDir::new().unwrap();
        let repo = td.path();
        git2::Repository::init(repo).unwrap();
        // .git/hooks dir exists (git2 creates it) but is bare —
        // no pre-commit / pre-push / hooks.d structure.
        let findings = detect(repo);
        assert!(
            findings.is_empty(),
            "fresh git repo without our hooks must not flag: {findings:?}"
        );
    }

    #[cfg(unix)]
    fn install_hook(hooks_dir: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = hooks_dir.join(name);
        fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(mode);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    fn install_plugin(hooks_dir: &Path, hook_name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let sub = hooks_dir.join("hooks.d").join(hook_name);
        fs::create_dir_all(&sub).unwrap();
        let path = sub.join(PLUGIN_FILE_NAME);
        fs::write(&path, b"#!/usr/bin/env python3\nimport sys; sys.exit(0)\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(mode);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    fn init_repo_with_hooks_dir(td: &TempDir) -> PathBuf {
        let repo = td.path().to_path_buf();
        git2::Repository::init(&repo).unwrap();
        let hooks_dir = repo.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        hooks_dir
    }

    #[cfg(unix)]
    #[test]
    fn detector_returns_empty_when_all_hooks_executable() {
        let td = TempDir::new().unwrap();
        let hooks_dir = init_repo_with_hooks_dir(&td);
        install_hook(&hooks_dir, "pre-commit", 0o755);
        install_hook(&hooks_dir, "pre-push", 0o755);
        install_plugin(&hooks_dir, "pre-commit", 0o755);
        install_plugin(&hooks_dir, "pre-push", 0o755);
        let findings = detect(td.path());
        assert!(
            findings.is_empty(),
            "all-executable layout must not flag: {findings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_pre_commit_missing_user_exec() {
        let td = TempDir::new().unwrap();
        let hooks_dir = init_repo_with_hooks_dir(&td);
        // Drop user-exec bit on pre-commit ONLY.
        install_hook(&hooks_dir, "pre-commit", 0o644);
        install_hook(&hooks_dir, "pre-push", 0o755);
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1, "must produce exactly one finding");
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert!(f.entries[0].path.ends_with("pre-commit"));
        assert_eq!(f.entries[0].current_mode, 0o644);
        assert_eq!(f.expected_mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn detector_aggregates_multiple_non_executable_entries() {
        let td = TempDir::new().unwrap();
        let hooks_dir = init_repo_with_hooks_dir(&td);
        install_hook(&hooks_dir, "pre-commit", 0o644);
        install_hook(&hooks_dir, "pre-push", 0o600);
        install_plugin(&hooks_dir, "pre-commit", 0o400);
        install_plugin(&hooks_dir, "pre-push", 0o755); // this one OK
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1, "must aggregate into one finding");
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn detector_skips_symlinked_entries() {
        // Symlinks are owned by `plugin-symlink-replacement` FM,
        // not this one. Pin that we don't double-emit.
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let hooks_dir = init_repo_with_hooks_dir(&td);
        // Real target lives elsewhere with mode 0o644.
        let real = td.path().join("real_pre_commit");
        fs::write(&real, b"#!/bin/sh\n").unwrap();
        symlink(&real, hooks_dir.join("pre-commit")).unwrap();
        let findings = detect(td.path());
        assert!(
            findings.is_empty(),
            "symlinked pre-commit must NOT flag in this FM: {findings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn finding_serializes_with_expected_mode_and_remediation() {
        let f = GuardPluginNotExecutableFinding {
            hooks_dir: "/tmp/.git/hooks".into(),
            entries: vec![NonExecutableEntry {
                path: "/tmp/.git/hooks/pre-commit".into(),
                current_mode: 0o644,
            }],
            expected_mode: EXPECTED_MODE,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"current_mode_octal\":\"0o644\""));
        assert!(s.contains("\"expected_mode_octal\":\"0o755\""));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("chmod 755"));
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
        let finding = GuardPluginNotExecutableFinding {
            hooks_dir: td.path().to_path_buf(),
            entries: Vec::new(),
            expected_mode: EXPECTED_MODE,
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
