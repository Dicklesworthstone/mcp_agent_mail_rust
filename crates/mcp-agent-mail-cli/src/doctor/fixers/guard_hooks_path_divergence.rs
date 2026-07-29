//! `fm-guard_install-hooks-path-divergence` — P1 detect-only first cut.
//!
//! **Subsystem**: guard_install.
//!
//! ## What's broken
//!
//! `am install-precommit-guard` resolves the active hooks dir at
//! install time (typically `<git_dir>/hooks` or whatever
//! `core.hooksPath` pointed at). If the user later **changes**
//! `core.hooksPath` (or removes it), the installed chain runner
//! is still present at the OLD path — but `git commit` now spawns
//! hooks from the NEW path, so the orphan install silently
//! stops firing. The guard is effectively bypassed; reservation
//! violations sail straight to the repo.
//!
//! Distinct from `guard_plugin_not_executable` (which detects a
//! mode-bit drop at the **active** hooks dir) and from
//! `guard_install-plugin-symlink-replacement` (which detects an
//! intentional swap at the active path). This FM specifically
//! detects **orphan** installs at NON-active candidate locations.
//!
//! ## Detection (pure)
//!
//! 1. Discover the git repo via `git2::Repository::discover`. If
//!    not a git repo, return empty.
//! 2. Resolve the active hooks dir via
//!    `mcp_agent_mail_guard::resolve_hooks_dir(repo_root)`.
//! 3. Build the candidate hooks-dir set:
//!    - `<common_git_dir>/hooks` (default location)
//!    - `<workdir>/.githooks` (worktree convention)
//!    - `<workdir>/githooks` (alt convention)
//! 4. For each candidate `<dir>/pre-commit` AND `<dir>/pre-push`
//!    whose contents include the chain-runner sentinel
//!    (`# mcp-agent-mail chain-runner (<hook>)`):
//!    - If the dir equals the active hooks dir → active install
//!      confirmed (no action).
//!    - Otherwise → orphan install (record).
//! 5. Emit one aggregated finding if any orphan is recorded.
//!    Note: when NO install exists anywhere, that's a different
//!    failure mode (`guard install missing`) which this FM
//!    deliberately does NOT flag.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair_spec calls for an
//! `Op::Rename` of each orphan hook file to a quarantine path
//! under `<run-dir>/quarantine/`, plus per-orphan
//! `actions.jsonl` rows so `am doctor undo <run-id>` can restore
//! byte-identically. That's substantial harness work (per-orphan
//! quarantine directory + round-trip test) — deferred. Manual
//! remediation: `git config core.hooksPath` to confirm the
//! intended dir, then either (a) move the orphans into the
//! active dir, or (b) move them aside into a private archive
//! (`mkdir -p .doctor/quarantine/orphan-hooks && mv <orphan> .doctor/quarantine/orphan-hooks/`).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-guard_install-hooks-path-divergence";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "guard_install";

/// Sentinel string the chain-runner script writes as a comment.
/// Source of truth: `crates/mcp-agent-mail-guard/src/lib.rs:206`,
/// `render_chain_runner_script(hook_name)`. If the guard crate
/// renames the sentinel, this detector must be updated in lock
/// step — there is no public constant to import (yet).
fn sentinel_for(hook_name: &str) -> String {
    format!("# mcp-agent-mail chain-runner ({hook_name})")
}

#[derive(Debug, Clone, Serialize)]
pub struct OrphanInstallEntry {
    /// The candidate hooks-dir that contains an orphan install
    /// (NOT the active dir).
    pub hooks_dir: PathBuf,
    /// Subset of `["pre-commit", "pre-push"]` for which an
    /// orphan was found at this dir.
    pub orphan_hooks: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardHooksPathDivergenceFinding {
    pub active_hooks_dir: PathBuf,
    pub active_install_present: bool,
    pub core_hooks_path: Option<String>,
    pub orphan_installs: Vec<OrphanInstallEntry>,
}

impl GuardHooksPathDivergenceFinding {
    pub fn total_orphan_hooks(&self) -> usize {
        self.orphan_installs
            .iter()
            .map(|e| e.orphan_hooks.len())
            .sum()
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} orphan agent-mail hook install(s) at {} candidate dir(s) outside the active hooks dir {}",
            self.total_orphan_hooks(),
            self.orphan_installs.len(),
            self.active_hooks_dir.display(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "active_hooks_dir": self.active_hooks_dir.to_string_lossy(),
                "active_install_present": self.active_install_present,
                "core_hooks_path": self.core_hooks_path,
                "orphan_installs": self.orphan_installs,
                "total_orphan_hooks": self.total_orphan_hooks(),
                "manual_remediation": {
                    "steps": [
                        "Confirm the intended hooks dir: `git config --show-origin --get core.hooksPath` (empty output means the default <git_dir>/hooks is canonical).",
                        "If the active hooks dir is intended: move each orphan into the active dir, OR move them aside into a private archive — `mkdir -p .doctor/quarantine/orphan-hooks && mv <orphan> .doctor/quarantine/orphan-hooks/`.",
                        "If an orphan dir is actually intended (e.g., the user wants `.githooks/` as canonical): `git config core.hooksPath .githooks` (relative to workdir) to make it active, then move the contents from the old active dir as above.",
                        "Re-run `am install-precommit-guard --project <abs-path>` to ensure the install lives at the current active dir.",
                        "Re-run `am doctor fix --only fm-guard_install-hooks-path-divergence --list` to confirm zero orphans.",
                    ],
                    "warning": "When orphan installs exist, the user's `git commit` may spawn hooks from a different dir than the one the chain-runner sits at — the guard is silently bypassed. Treat as P1.",
                    "safe_fix_deferred": "Auto-fix via Op::Rename to quarantine is intentionally deferred in this first cut. The chokepoint already implements Op::Rename (see `stale_archive_lock` and `stale_head_or_ref_lock`); a follow-up pass wires per-orphan quarantine with a round-trip test.",
                    "common_causes": [
                        "Operator ran `git config core.hooksPath <new_dir>` after `am install-precommit-guard` had already installed at the default location.",
                        "Husky / lefthook / pre-commit (the python tool) installed a competing dir and updated `core.hooksPath`, but the agent-mail chain-runner is still at the default.",
                        "Worktree migration: the install lives at `<workdir>/.githooks/` but `core.hooksPath` was unset, reverting to `<git_dir>/hooks`.",
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
/// Returns at most one finding per call. Emits no finding when
/// the install is wholly absent (a separate FM covers that case).
pub fn detect(repo_root: &Path) -> Vec<GuardHooksPathDivergenceFinding> {
    // Non-git directory → no install to diverge from.
    let Ok(git_repo) = git2::Repository::discover(repo_root) else {
        return Vec::new();
    };
    if git_repo.is_bare() {
        return Vec::new();
    }
    let Some(workdir) = git_repo.workdir().map(Path::to_path_buf) else {
        return Vec::new();
    };
    let Ok(active_hooks_dir) = mcp_agent_mail_guard::resolve_hooks_dir(repo_root) else {
        return Vec::new();
    };
    let core_hooks_path = git_repo
        .config()
        .ok()
        .and_then(|c| c.get_string("core.hooksPath").ok());

    // Build candidate dirs. Deduplicate — depending on the repo
    // shape, the default `<git_dir>/hooks` and the active dir
    // may coincide.
    let common_git_dir = git_repo.commondir().to_path_buf();
    let mut candidates: Vec<PathBuf> = vec![
        common_git_dir.join("hooks"),
        workdir.join(".githooks"),
        workdir.join("githooks"),
    ];
    candidates.sort();
    candidates.dedup();

    let canonical_active = canonicalize_or_clone(&active_hooks_dir);
    let mut active_install_present = false;
    let mut orphan_installs: Vec<OrphanInstallEntry> = Vec::new();
    for c in &candidates {
        let canonical_c = canonicalize_or_clone(c);
        let mut found: Vec<String> = Vec::new();
        for hook in ["pre-commit", "pre-push"] {
            let hook_path = c.join(hook);
            if !hook_path.is_file() {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&hook_path) else {
                continue;
            };
            if body.contains(&sentinel_for(hook)) {
                found.push(hook.to_string());
            }
        }
        if found.is_empty() {
            continue;
        }
        if canonical_c == canonical_active {
            active_install_present = true;
        } else {
            orphan_installs.push(OrphanInstallEntry {
                hooks_dir: c.clone(),
                orphan_hooks: found,
            });
        }
    }

    if orphan_installs.is_empty() {
        return Vec::new();
    }
    vec![GuardHooksPathDivergenceFinding {
        active_hooks_dir,
        active_install_present,
        core_hooks_path,
        orphan_installs,
    }]
}

/// Best-effort canonicalization for path-equality checks.
/// `fs::canonicalize` requires the path exist; for non-existent
/// candidates (e.g., `.githooks` on a repo that doesn't use it)
/// fall back to the input path.
fn canonicalize_or_clone(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &GuardHooksPathDivergenceFinding,
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
        repo
    }

    fn write_chain_runner(dir: &Path, hook: &str) {
        fs::create_dir_all(dir).unwrap();
        let body = format!(
            "#!/usr/bin/env python3\n# mcp-agent-mail chain-runner ({hook})\nimport sys; sys.exit(0)\n"
        );
        fs::write(dir.join(hook), body).unwrap();
    }

    fn write_foreign_hook(dir: &Path, hook: &str) {
        fs::create_dir_all(dir).unwrap();
        // Looks like a hook but lacks our sentinel — should be
        // ignored by the detector.
        fs::write(
            dir.join(hook),
            "#!/bin/sh\necho 'some other project hook'\n",
        )
        .unwrap();
    }

    /// **NEGATIVE TEST FIRST**: non-git dir → no finding.
    #[test]
    fn detector_returns_empty_for_non_git_directory() {
        let td = TempDir::new().unwrap();
        assert!(detect(td.path()).is_empty());
    }

    /// **NEGATIVE**: git repo, no installs anywhere → no finding
    /// (the "install missing" case is owned by a separate FM).
    #[test]
    fn detector_returns_empty_for_git_repo_with_no_installs() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        assert!(
            detect(&repo).is_empty(),
            "fresh git repo without our hooks must not flag this FM"
        );
    }

    #[test]
    fn detector_returns_empty_when_only_active_install_present() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let active = repo.join(".git").join("hooks");
        write_chain_runner(&active, "pre-commit");
        write_chain_runner(&active, "pre-push");
        let findings = detect(&repo);
        assert!(
            findings.is_empty(),
            "active install at canonical location must not flag: {findings:?}"
        );
    }

    /// **NEGATIVE**: a foreign (non-agent-mail) hook at an alt
    /// dir must NOT flag — sentinel mismatch is the gate.
    #[test]
    fn detector_skips_foreign_hooks_at_alt_dirs() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let active = repo.join(".git").join("hooks");
        write_chain_runner(&active, "pre-commit");
        write_foreign_hook(&repo.join(".githooks"), "pre-commit");
        write_foreign_hook(&repo.join("githooks"), "pre-commit");
        let findings = detect(&repo);
        assert!(
            findings.is_empty(),
            "foreign hooks without our sentinel must not flag: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_orphan_install_at_dot_githooks() {
        // Active is default (<git>/hooks); also install at
        // <workdir>/.githooks/ — that's an orphan.
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let active = repo.join(".git").join("hooks");
        write_chain_runner(&active, "pre-commit");
        let orphan_dir = repo.join(".githooks");
        write_chain_runner(&orphan_dir, "pre-commit");
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1, "must produce exactly one finding");
        let f = &findings[0];
        assert!(f.active_install_present);
        assert_eq!(f.orphan_installs.len(), 1);
        assert!(f.orphan_installs[0].hooks_dir.ends_with(".githooks"));
        assert_eq!(f.orphan_installs[0].orphan_hooks, vec!["pre-commit"]);
        assert_eq!(f.total_orphan_hooks(), 1);
    }

    #[test]
    fn detector_flags_orphans_at_multiple_dirs_and_both_hook_kinds() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let active = repo.join(".git").join("hooks");
        write_chain_runner(&active, "pre-commit");
        // Orphans: .githooks has BOTH pre-commit + pre-push;
        // githooks has only pre-push.
        write_chain_runner(&repo.join(".githooks"), "pre-commit");
        write_chain_runner(&repo.join(".githooks"), "pre-push");
        write_chain_runner(&repo.join("githooks"), "pre-push");
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert!(f.active_install_present);
        assert_eq!(f.orphan_installs.len(), 2);
        assert_eq!(f.total_orphan_hooks(), 3);
    }

    #[test]
    fn detector_records_active_install_absent_when_only_orphans_exist() {
        // Active hooks dir is empty; orphan at .githooks. Spec:
        // this is still a finding — the operator may have moved
        // the install to an alt dir without updating
        // core.hooksPath. The doctor surfaces both signals so
        // the operator can decide.
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_chain_runner(&repo.join(".githooks"), "pre-commit");
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert!(
            !f.active_install_present,
            "active install absent must be recorded as false"
        );
        assert_eq!(f.orphan_installs.len(), 1);
    }

    #[test]
    fn finding_serializes_with_active_dir_and_remediation() {
        let f = GuardHooksPathDivergenceFinding {
            active_hooks_dir: "/tmp/repo/.git/hooks".into(),
            active_install_present: true,
            core_hooks_path: Some(".githooks".to_string()),
            orphan_installs: vec![OrphanInstallEntry {
                hooks_dir: "/tmp/repo/.githooks".into(),
                orphan_hooks: vec!["pre-commit".to_string()],
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"active_install_present\":true"));
        assert!(s.contains("\"total_orphan_hooks\":1"));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("git config core.hooksPath"));
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
        let finding = GuardHooksPathDivergenceFinding {
            active_hooks_dir: td.path().to_path_buf(),
            active_install_present: false,
            core_hooks_path: None,
            orphan_installs: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
