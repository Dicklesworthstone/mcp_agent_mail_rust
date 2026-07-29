//! `fm-guard_install-chain-runner-missing-or-stale` — P1 detect-only.
//!
//! **Subsystem**: guard_install.
//!
//! ## What's broken
//!
//! The agent-mail plugin (`<hooks_dir>/hooks.d/<hook>/50-agent-mail.py`)
//! exists — proof that `am install-precommit-guard` ran at some
//! point — but the chain runner (`<hooks_dir>/<hook>`) is either:
//!
//! - **missing entirely** — the plugin lives at `hooks.d/<hook>/`
//!   but no top-level dispatcher hook is in place. `git commit`
//!   never invokes the plugin because git only spawns
//!   `<hooks_dir>/<hook>`, not the per-plugin scripts beneath
//!   `hooks.d/`. The plugin is dead code.
//! - **lacking our sentinel** — the chain runner exists but its
//!   contents don't carry `# mcp-agent-mail chain-runner (<hook>)`.
//!   Some other writer rewrote the file (manual edit, a half-run
//!   installer for another tool, an old install left behind).
//!   The agent-mail plugin still sits in `hooks.d/` but the
//!   dispatcher no longer routes to it.
//!
//! Both cases mean `git commit` silently bypasses agent-mail
//! reservation gates. Distinct from:
//!
//! - **`guard_foreign_runner_overwrite` (FM19)**: foreign hook
//!   manager (husky/lefthook/pre-commit) signature in the chain
//!   runner + repo artefacts. THIS FM has no such signature OR
//!   artefact requirement; we flag ANY non-canonical chain runner
//!   when our plugin is present.
//! - **`guard_hooks_path_divergence` (FM18)**: chain runner sits
//!   at a NON-active hooks dir (orphan install at `core.hooksPath`
//!   change). THIS FM only looks at the ACTIVE hooks dir.
//! - **`guard_plugin_not_executable` (FM17) / `_symlink_replacement`
//!   (FM20)**: the plugin/runner file IS the agent-mail version
//!   but mode/symlink-shape is wrong.
//!
//! ## Detection (pure)
//!
//! For each hook ∈ {pre-commit, pre-push}:
//!
//! 1. Resolve active hooks dir via `mcp_agent_mail_guard::resolve_hooks_dir`.
//! 2. Check `<hooks_dir>/hooks.d/<hook>/50-agent-mail.py`. If
//!    the plugin doesn't exist, this hook is out of scope —
//!    the "install never happened OR was fully uninstalled" case
//!    is a separate FM.
//! 3. Check `<hooks_dir>/<hook>`:
//!    - **missing** → record `Reason::ChainRunnerMissing`.
//!    - **exists, contains** `# mcp-agent-mail chain-runner (<hook>)`
//!      → considered current; no flag. (Bit-identical "is this
//!      exactly the version we'd render today?" gating is a
//!      follow-up — needs `render_chain_runner_script` to be
//!      pub from the guard crate, which is private today.)
//!    - **exists, missing sentinel** → record
//!      `Reason::SentinelMismatch`.
//! 4. Symlinks: if the chain runner is a symlink, defer to
//!    `guard_plugin_symlink_replacement` (FM20) — record nothing
//!    here to avoid double-emission.
//!
//! Emit one aggregated finding when at least one hook is missing
//! or stale.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair_spec calls for two
//! distinct write actions:
//!
//! 1. If foreign content present, Op::Rename it aside to
//!    `<hooks_dir>/<hook>.orig` (if no `.orig` exists yet) OR
//!    `<run-dir>/quarantine/`.
//! 2. Op::WriteFile the freshly-rendered chain runner from
//!    `render_chain_runner_script(hook)` (requires the guard
//!    crate's render fn to be pub or duplicated; deferred to
//!    avoid bit-rot between this FM and the installer).
//!
//! Manual remediation: re-run `am install-precommit-guard
//! --project <abs-path>` — the installer handles the per-hook
//! `.orig` quarantine + write cycle correctly and the doctor
//! shouldn't re-implement it from scratch.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-guard_install-chain-runner-missing-or-stale";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "guard_install";

const PLUGIN_FILE_NAME: &str = "50-agent-mail.py";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The chain runner file is absent at the canonical path
    /// while the plugin sits in `hooks.d/`. `git commit` never
    /// invokes the plugin.
    ChainRunnerMissing,
    /// The chain runner file exists but its content lacks our
    /// sentinel `# mcp-agent-mail chain-runner (<hook>)`.
    SentinelMismatch,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleHookEntry {
    /// `pre-commit` or `pre-push`.
    pub hook: String,
    /// The chain runner path that should carry the sentinel.
    pub chain_path: PathBuf,
    /// The plugin path that PROVES the install happened (without
    /// this, the whole FM doesn't apply).
    pub plugin_path: PathBuf,
    pub reason: Reason,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuardChainRunnerStaleFinding {
    pub hooks_dir: PathBuf,
    pub entries: Vec<StaleHookEntry>,
}

impl GuardChainRunnerStaleFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} agent-mail plugin(s) under {} have a missing or stale chain runner — `git commit` skips the guard",
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
                "manual_remediation": {
                    "steps": [
                        "Re-run `am install-precommit-guard --project <abs-path>` — the installer correctly handles the per-hook quarantine + write cycle (preserves any prior hook content as `<hook>.orig` and writes a fresh chain runner with the canonical sentinel).",
                        "Verify after install: `cat <hooks_dir>/pre-commit | head -3` should show `#!/usr/bin/env python3` and `# mcp-agent-mail chain-runner (pre-commit)`.",
                        "Re-run `am doctor fix --only fm-guard_install-chain-runner-missing-or-stale --list` to confirm zero residual entries.",
                    ],
                    "warning": "When the chain runner is missing or stale, `git commit` SILENTLY bypasses the agent-mail guard — reservation violations land in the repo with no error.",
                    "safe_fix_deferred": "Auto-fix via Op::Rename + Op::WriteFile is deferred. The render-step needs `mcp_agent_mail_guard::render_chain_runner_script` to be pub (it is private today) OR the chain-runner content needs to be duplicated in the doctor crate (risky — bit-rot vs. the installer). The installer (`am install-precommit-guard`) is the canonical write path; the doctor routes there for now.",
                    "distinct_from": {
                        "fm-guard_install-foreign-runner-overwrite": "That FM requires a foreign manager signature (husky/lefthook/pre-commit) in the chain runner AND repo artefacts. THIS FM flags ANY non-canonical chain runner when the agent-mail plugin is present.",
                        "fm-guard_install-hooks-path-divergence": "That FM looks for orphan installs at NON-active hooks dirs (core.hooksPath changed). THIS FM only looks at the ACTIVE hooks dir.",
                        "fm-guard_install-plugin-symlink-replacement": "That FM owns the symlinked-runner case; this FM deliberately skips symlinks to avoid double-emission.",
                    },
                    "common_causes": [
                        "Operator manually `rm`-ed pre-commit (perhaps after a husky uninstall) but left `hooks.d/pre-commit/50-agent-mail.py` in place.",
                        "An older `am install-precommit-guard` version wrote a chain runner with a different sentinel string and was never re-installed.",
                        "Manual edit of pre-commit stripped the sentinel comment (formatter, hand edit, IDE rewrite).",
                        "A worktree migration copied `hooks.d/` but skipped the top-level dispatcher hook.",
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

fn sentinel_for(hook: &str) -> String {
    format!("# mcp-agent-mail chain-runner ({hook})")
}

/// Detector. PURE w.r.t. the supplied `repo_root`.
///
/// Returns at most one aggregated finding per call. Returns
/// empty when the agent-mail plugin is absent (out of scope —
/// the "uninstalled" case is owned elsewhere) or when both
/// chain runners are present and carry the sentinel.
pub fn detect(repo_root: &Path) -> Vec<GuardChainRunnerStaleFinding> {
    let Ok(hooks_dir) = mcp_agent_mail_guard::resolve_hooks_dir(repo_root) else {
        return Vec::new();
    };
    let mut entries: Vec<StaleHookEntry> = Vec::new();
    for hook in ["pre-commit", "pre-push"] {
        let plugin_path = hooks_dir.join("hooks.d").join(hook).join(PLUGIN_FILE_NAME);
        if !plugin_path.is_file() {
            // Plugin absent — install never happened OR was fully
            // uninstalled for this hook. Out of scope.
            continue;
        }
        let chain_path = hooks_dir.join(hook);
        let Ok(lmeta) = std::fs::symlink_metadata(&chain_path) else {
            // Plugin exists, chain runner missing — that's the
            // ChainRunnerMissing case.
            entries.push(StaleHookEntry {
                hook: hook.to_string(),
                chain_path: chain_path.clone(),
                plugin_path: plugin_path.clone(),
                reason: Reason::ChainRunnerMissing,
            });
            continue;
        };
        if lmeta.file_type().is_symlink() {
            // Owned by `guard_plugin_symlink_replacement` (FM20).
            // Don't double-emit.
            continue;
        }
        if !lmeta.file_type().is_file() {
            // Directory or other special file — exotic. Skip.
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&chain_path) else {
            continue;
        };
        if !body.contains(&sentinel_for(hook)) {
            entries.push(StaleHookEntry {
                hook: hook.to_string(),
                chain_path,
                plugin_path,
                reason: Reason::SentinelMismatch,
            });
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![GuardChainRunnerStaleFinding { hooks_dir, entries }]
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &GuardChainRunnerStaleFinding,
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

    fn install_plugin(repo: &Path, hook: &str) -> PathBuf {
        let dir = repo.join(".git").join("hooks").join("hooks.d").join(hook);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(PLUGIN_FILE_NAME);
        fs::write(&p, b"#!/usr/bin/env python3\nimport sys; sys.exit(0)\n").unwrap();
        p
    }

    fn write_chain_runner(repo: &Path, hook: &str, body: &str) -> PathBuf {
        let p = repo.join(".git").join("hooks").join(hook);
        fs::write(&p, body).unwrap();
        p
    }

    fn canonical_runner_body(hook: &str) -> String {
        format!(
            "#!/usr/bin/env python3\n# mcp-agent-mail chain-runner ({hook})\nimport sys; sys.exit(0)\n"
        )
    }

    /// **NEGATIVE TEST FIRST**: non-git dir → no finding.
    #[test]
    fn detector_returns_empty_for_non_git_directory() {
        let td = TempDir::new().unwrap();
        assert!(detect(td.path()).is_empty());
    }

    /// **NEGATIVE**: plugin absent → no finding (out of scope).
    #[test]
    fn detector_returns_empty_when_plugin_absent() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        // No plugin, no chain runner. The "install missing"
        // case is a different FM.
        assert!(detect(&repo).is_empty());
    }

    /// **NEGATIVE**: plugin present, chain runner present with
    /// sentinel → no finding (healthy state).
    #[test]
    fn detector_returns_empty_when_chain_runner_has_sentinel() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        install_plugin(&repo, "pre-commit");
        install_plugin(&repo, "pre-push");
        write_chain_runner(&repo, "pre-commit", &canonical_runner_body("pre-commit"));
        write_chain_runner(&repo, "pre-push", &canonical_runner_body("pre-push"));
        let findings = detect(&repo);
        assert!(
            findings.is_empty(),
            "healthy install must not flag: {findings:?}"
        );
    }

    #[test]
    fn detector_flags_missing_chain_runner_when_plugin_present() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        install_plugin(&repo, "pre-commit");
        // No write_chain_runner call — pre-commit chain runner is missing.
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].hook, "pre-commit");
        assert_eq!(f.entries[0].reason, Reason::ChainRunnerMissing);
    }

    #[test]
    fn detector_flags_sentinel_mismatch_when_chain_runner_was_clobbered() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        install_plugin(&repo, "pre-commit");
        // Some other writer overwrote pre-commit with something
        // that doesn't carry our sentinel.
        write_chain_runner(&repo, "pre-commit", "#!/bin/sh\necho 'manual edit'\n");
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].reason, Reason::SentinelMismatch);
    }

    #[test]
    fn detector_aggregates_both_hooks_in_one_finding() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        install_plugin(&repo, "pre-commit");
        install_plugin(&repo, "pre-push");
        // pre-commit missing; pre-push present but sentinel stripped.
        write_chain_runner(&repo, "pre-push", "#!/bin/sh\necho hi\n");
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 2);
        let kinds: std::collections::HashSet<Reason> =
            findings[0].entries.iter().map(|e| e.reason).collect();
        assert!(kinds.contains(&Reason::ChainRunnerMissing));
        assert!(kinds.contains(&Reason::SentinelMismatch));
    }

    /// Pin the no-double-emit invariant with FM20: when the
    /// chain runner is a symlink, defer to that FM.
    #[cfg(unix)]
    #[test]
    fn detector_skips_symlinked_chain_runner_owned_by_fm20() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        install_plugin(&repo, "pre-commit");
        // Plant a symlink with broken target.
        symlink(
            "/some/where/else",
            repo.join(".git").join("hooks").join("pre-commit"),
        )
        .unwrap();
        let findings = detect(&repo);
        assert!(
            findings.is_empty(),
            "symlinked chain runner is FM20's territory, not FM21"
        );
    }

    #[test]
    fn detector_skips_partial_install_where_only_one_hook_has_plugin() {
        // Only pre-commit plugin installed; pre-push has nothing.
        // pre-commit chain runner is also present with sentinel.
        // Result: clean.
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        install_plugin(&repo, "pre-commit");
        write_chain_runner(&repo, "pre-commit", &canonical_runner_body("pre-commit"));
        let findings = detect(&repo);
        assert!(
            findings.is_empty(),
            "partial install with healthy pre-commit must not flag pre-push as missing"
        );
    }

    #[test]
    fn finding_serializes_with_reason_distinct_strings() {
        let f = GuardChainRunnerStaleFinding {
            hooks_dir: "/tmp/.git/hooks".into(),
            entries: vec![
                StaleHookEntry {
                    hook: "pre-commit".to_string(),
                    chain_path: "/tmp/.git/hooks/pre-commit".into(),
                    plugin_path: "/tmp/.git/hooks/hooks.d/pre-commit/50-agent-mail.py".into(),
                    reason: Reason::ChainRunnerMissing,
                },
                StaleHookEntry {
                    hook: "pre-push".to_string(),
                    chain_path: "/tmp/.git/hooks/pre-push".into(),
                    plugin_path: "/tmp/.git/hooks/hooks.d/pre-push/50-agent-mail.py".into(),
                    reason: Reason::SentinelMismatch,
                },
            ],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        // serde rename_all = "snake_case" → string values lowercase.
        assert!(s.contains("\"chain_runner_missing\""));
        assert!(s.contains("\"sentinel_mismatch\""));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("distinct_from"));
        assert!(s.contains("am install-precommit-guard"));
        assert!(s.contains("\"auto_fixable\":false"));
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
        let finding = GuardChainRunnerStaleFinding {
            hooks_dir: td.path().to_path_buf(),
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
