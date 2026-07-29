//! `fm-guard_install-foreign-runner-overwrite` — P0 detect-only.
//!
//! **Subsystem**: guard_install.
//!
//! ## What's broken
//!
//! The active hooks dir contains a `pre-commit` that is NOT the
//! agent-mail chain runner — it carries the signature of a foreign
//! hook manager (husky, lefthook, the `pre-commit` framework), and
//! the project tree has corroborating config artefacts (`.husky/`,
//! `lefthook.yml`, `.pre-commit-config.yaml`).
//!
//! This is the silent-overwrite case: a peer tool ran its own
//! installer AFTER `am install-precommit-guard` and clobbered the
//! agent-mail chain runner. `git commit` now spawns the foreign
//! runner, which has no knowledge of agent-mail reservation gates;
//! reservation violations sail straight to the repo.
//!
//! P0 because:
//!
//! - This is one of the few cases where the guard is COMPLETELY
//!   absent from the commit pipeline (vs. mode-bit drop which can
//!   sometimes still fire on lenient kernels).
//! - The user's other tooling is actively maintaining the foreign
//!   runner, so the agent-mail install will get clobbered AGAIN
//!   the next time the user runs `husky install` / `lefthook install`
//!   / `pre-commit install`.
//!
//! Distinct from `guard_hooks_path_divergence` (orphan install
//! at a NON-active dir while a different dir is active), and from
//! `guard_plugin_not_executable` (the agent-mail install is in
//! place but its mode bits are wrong).
//!
//! ## Detection (pure)
//!
//! 1. Discover the git repo via `git2::Repository::discover`.
//! 2. Resolve the active hooks dir.
//! 3. Read `<hooks_dir>/pre-commit`. If it doesn't exist, return
//!    empty — the "no install at all" case is a separate FM.
//! 4. If body contains the agent-mail sentinel
//!    `# mcp-agent-mail chain-runner (pre-commit)` → return empty
//!    (we ARE the active runner; nothing was overwritten).
//! 5. Otherwise, scan body for foreign manager signatures:
//!    - **husky**: `husky.sh`, `.husky/_/`
//!    - **lefthook**: `lefthook run`, `lefthook hook`
//!    - **pre-commit framework**: `pre-commit-hook`,
//!      `pre-commit framework`, `INSTALL_PYTHON`
//! 6. Scan repo root for corroborating artefacts:
//!    - `.husky/` directory
//!    - `lefthook.yml`, `.lefthook.yml`, `lefthook.yaml` files
//!    - `.pre-commit-config.yaml` file
//! 7. If at least one manager OR artefact is found, emit a P0
//!    finding. Otherwise, return empty (foreign-but-unrecognized
//!    pre-commit content is owned by a different FM).
//!
//! Also reports `orig_present` — whether `<hooks_dir>/pre-commit.orig`
//! exists, since the agent-mail installer preserves the prior hook
//! as `.orig` and a foreign manager that ran AFTER us may have left
//! that file behind, giving an operator a clean recovery path.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair_spec calls for:
//!
//! 1. Op::Rename the foreign pre-commit to
//!    `<hooks_dir>/pre-commit.foreign-<run-id>` (preserves the
//!    foreign content for forensics + lets the operator re-enable
//!    it later).
//! 2. If `<hooks_dir>/pre-commit.orig` exists, restore it OR
//!    re-run `am install-precommit-guard` to write a fresh chain
//!    runner.
//! 3. Verify by re-running the detector.
//!
//! This is multi-step and needs a round-trip test fixture
//! (corrupt → fix → undo → byte-identical). Deferred to a
//! follow-up pass; manual remediation routes the operator through
//! the same sequence with `mv` + `am install-precommit-guard`.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-guard_install-foreign-runner-overwrite";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "guard_install";

const AGENT_MAIL_SENTINEL: &str = "# mcp-agent-mail chain-runner (pre-commit)";

#[derive(Debug, Clone, Serialize)]
pub struct GuardForeignRunnerOverwriteFinding {
    pub hooks_dir: PathBuf,
    pub chain_path: PathBuf,
    /// SHA-256 of the foreign pre-commit body, for forensic
    /// correlation across runs.
    pub chain_path_sha256: String,
    /// Foreign-manager signatures detected in the body
    /// (subset of `["husky", "lefthook", "pre-commit-framework"]`).
    pub detected_managers: Vec<String>,
    /// Repo-side config / dir artefacts that corroborate which
    /// manager owns the pre-commit.
    pub repo_artefacts: Vec<String>,
    /// Whether `<hooks_dir>/pre-commit.orig` exists. The
    /// agent-mail installer preserves the prior hook as `.orig`;
    /// foreign installers don't typically clean this up, so its
    /// presence is a strong signal for a recovery path.
    pub orig_present: bool,
}

impl GuardForeignRunnerOverwriteFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "Agent-mail chain runner clobbered at {} by foreign hook manager(s): {}",
            self.chain_path.display(),
            if self.detected_managers.is_empty() {
                "<unknown> (artefacts only)".to_string()
            } else {
                self.detected_managers.join(", ")
            },
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "hooks_dir": self.hooks_dir.to_string_lossy(),
                "chain_path": self.chain_path.to_string_lossy(),
                "chain_path_sha256": self.chain_path_sha256,
                "detected_managers": self.detected_managers,
                "repo_artefacts": self.repo_artefacts,
                "orig_present": self.orig_present,
                "manual_remediation": {
                    "steps": [
                        "Preserve the foreign pre-commit for forensics: `mv <chain_path> <chain_path>.foreign-$(date +%Y%m%d-%H%M%S)`.",
                        "If `<hooks_dir>/pre-commit.orig` exists (the agent-mail installer's pre-install snapshot), restore it: `mv <chain_path>.orig <chain_path>`. Otherwise re-run `am install-precommit-guard --project <abs-path>` to write a fresh chain runner.",
                        "Coexistence option: chain-run the foreign hook UNDER the agent-mail chain runner by moving it to `<hooks_dir>/hooks.d/pre-commit/99-<manager>.sh`. The chain runner runs each script under `hooks.d/<hook>/` in lexical order — the foreign manager fires after agent-mail's guards.",
                        "If the foreign manager will re-install itself (husky, lefthook, pre-commit-framework) on the next run of its installer, consider disabling the foreign hook-manager invocation in package.json / Makefile until the chain-runner integration is in place.",
                        "Re-run `am doctor fix --only fm-guard_install-foreign-runner-overwrite --list` to confirm the chain runner is back at the active path.",
                    ],
                    "warning": "P0 — when a foreign manager clobbers the chain runner, the agent-mail guard is COMPLETELY absent from `git commit`. Reservation violations land in the repo with no error. Restore the chain runner ASAP.",
                    "common_causes": [
                        "User ran `husky install` / `lefthook install` / `pre-commit install` AFTER `am install-precommit-guard` — the foreign installer overwrote our pre-commit.",
                        "Foreign manager is wired into `npm install` / `yarn install` / `pnpm install` via `prepare` script and re-installs on every dep-tree change.",
                        "Repo's `Makefile` / CI bootstrap reinstalls the foreign manager from a clean slate.",
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
/// Returns at most one finding per call. Returns empty when the
/// agent-mail chain runner is present at the active hooks dir
/// (we own the pre-commit) OR when no pre-commit exists at all
/// (that's a separate "install missing" FM).
pub fn detect(repo_root: &Path) -> Vec<GuardForeignRunnerOverwriteFinding> {
    let Ok(git_repo) = git2::Repository::discover(repo_root) else {
        return Vec::new();
    };
    if git_repo.is_bare() {
        return Vec::new();
    }
    let Some(workdir) = git_repo.workdir().map(Path::to_path_buf) else {
        return Vec::new();
    };
    let Ok(hooks_dir) = mcp_agent_mail_guard::resolve_hooks_dir(repo_root) else {
        return Vec::new();
    };
    let chain_path = hooks_dir.join("pre-commit");
    if !chain_path.is_file() {
        return Vec::new();
    }
    let Ok(body) = std::fs::read_to_string(&chain_path) else {
        return Vec::new();
    };
    if body.contains(AGENT_MAIL_SENTINEL) {
        // The chain runner IS the active pre-commit — we haven't
        // been clobbered. (If the chain runner is degraded — bad
        // shebang, lost exec bit, etc. — that's owned by a
        // different FM.)
        return Vec::new();
    }

    let detected_managers = detect_managers(&body);
    let repo_artefacts = detect_repo_artefacts(&workdir);
    if detected_managers.is_empty() && repo_artefacts.is_empty() {
        // Foreign-but-unrecognized — could be a custom hand-rolled
        // pre-commit. Out of scope for THIS FM; a future
        // "unknown-foreign-pre-commit" FM (or operator inspection)
        // owns it.
        return Vec::new();
    }

    let chain_path_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        hex::encode(hasher.finalize())
    };
    let orig_present = hooks_dir.join("pre-commit.orig").is_file();
    vec![GuardForeignRunnerOverwriteFinding {
        hooks_dir,
        chain_path,
        chain_path_sha256,
        detected_managers,
        repo_artefacts,
        orig_present,
    }]
}

fn detect_managers(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if body.contains("husky.sh") || body.contains(".husky/_/") {
        out.push("husky".to_string());
    }
    if body.contains("lefthook run") || body.contains("lefthook hook") {
        out.push("lefthook".to_string());
    }
    if body.contains("pre-commit-hook")
        || body.contains("pre-commit framework")
        || body.contains("INSTALL_PYTHON")
    {
        out.push("pre-commit-framework".to_string());
    }
    out
}

fn detect_repo_artefacts(workdir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if workdir.join(".husky").is_dir() {
        out.push(".husky/".to_string());
    }
    if workdir.join("lefthook.yml").exists()
        || workdir.join(".lefthook.yml").exists()
        || workdir.join("lefthook.yaml").exists()
    {
        out.push("lefthook config".to_string());
    }
    if workdir.join(".pre-commit-config.yaml").exists() {
        out.push(".pre-commit-config.yaml".to_string());
    }
    out
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &GuardForeignRunnerOverwriteFinding,
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

    fn write_pre_commit(repo: &Path, body: &str) -> PathBuf {
        let hooks = repo.join(".git").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join("pre-commit");
        fs::write(&path, body).unwrap();
        path
    }

    /// **NEGATIVE TEST FIRST**: non-git dir → no finding.
    #[test]
    fn detector_returns_empty_for_non_git_directory() {
        let td = TempDir::new().unwrap();
        assert!(detect(td.path()).is_empty());
    }

    /// **NEGATIVE**: git repo, no pre-commit at all → no finding.
    /// (The "install missing" case is a separate FM; don't double-emit.)
    #[test]
    fn detector_returns_empty_when_no_pre_commit_exists() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        assert!(
            detect(&repo).is_empty(),
            "missing pre-commit must NOT be flagged by this FM"
        );
    }

    /// **NEGATIVE**: agent-mail chain runner present → we own it,
    /// nothing was overwritten.
    #[test]
    fn detector_returns_empty_when_chain_runner_present() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_pre_commit(
            &repo,
            "#!/usr/bin/env python3\n# mcp-agent-mail chain-runner (pre-commit)\nimport sys\n",
        );
        assert!(
            detect(&repo).is_empty(),
            "agent-mail chain runner must NOT be flagged"
        );
    }

    /// **NEGATIVE**: foreign-but-unrecognized content with NO repo
    /// artefacts → out of scope for this FM.
    #[test]
    fn detector_returns_empty_for_unrecognized_foreign_hook_no_artefacts() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_pre_commit(&repo, "#!/bin/sh\necho 'custom hand-rolled hook'\n");
        assert!(
            detect(&repo).is_empty(),
            "unrecognized foreign hook with no artefacts must NOT flag here"
        );
    }

    #[test]
    fn detector_flags_husky_overwrite_with_repo_dir() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_pre_commit(
            &repo,
            "#!/usr/bin/env sh\n. \"$(dirname -- \"$0\")/_/husky.sh\"\nnpm test\n",
        );
        fs::create_dir(repo.join(".husky")).unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.detected_managers, vec!["husky"]);
        assert!(f.repo_artefacts.iter().any(|a| a == ".husky/"));
    }

    #[test]
    fn detector_flags_lefthook_overwrite_by_signature_only() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_pre_commit(&repo, "#!/bin/sh\nlefthook run pre-commit\n");
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].detected_managers, vec!["lefthook"]);
        assert!(findings[0].repo_artefacts.is_empty());
    }

    #[test]
    fn detector_flags_pre_commit_framework_by_config_only() {
        // Pre-commit body that DOESN'T mention any signature
        // string but the repo has the config file. The artefact
        // alone is sufficient.
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_pre_commit(&repo, "#!/bin/sh\necho hi\n");
        fs::write(repo.join(".pre-commit-config.yaml"), "repos: []\n").unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detected_managers.is_empty());
        assert!(
            findings[0]
                .repo_artefacts
                .iter()
                .any(|a| a == ".pre-commit-config.yaml")
        );
    }

    #[test]
    fn detector_reports_orig_present_for_recovery_hint() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        write_pre_commit(&repo, "#!/bin/sh\nlefthook run pre-commit\n");
        // Simulate the agent-mail installer having preserved
        // pre-commit.orig before being clobbered.
        let hooks = repo.join(".git").join("hooks");
        fs::write(hooks.join("pre-commit.orig"), b"prior hook bytes").unwrap();
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].orig_present,
            "orig_present must be true when pre-commit.orig exists"
        );
    }

    #[test]
    fn detector_records_sha256_of_foreign_body() {
        let td = TempDir::new().unwrap();
        let repo = init_repo(&td);
        let body = "#!/bin/sh\nlefthook run pre-commit\n";
        write_pre_commit(&repo, body);
        let findings = detect(&repo);
        assert_eq!(findings.len(), 1);
        // Lower-case hex; 64 chars for SHA-256.
        assert_eq!(findings[0].chain_path_sha256.len(), 64);
        assert!(
            findings[0]
                .chain_path_sha256
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn finding_serializes_with_managers_artefacts_and_remediation() {
        let f = GuardForeignRunnerOverwriteFinding {
            hooks_dir: "/tmp/repo/.git/hooks".into(),
            chain_path: "/tmp/repo/.git/hooks/pre-commit".into(),
            chain_path_sha256: "deadbeef".repeat(8),
            detected_managers: vec!["husky".to_string()],
            repo_artefacts: vec![".husky/".to_string()],
            orig_present: true,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"orig_present\":true"));
        assert!(s.contains("husky"));
        assert!(s.contains(".husky/"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am install-precommit-guard"));
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
        let finding = GuardForeignRunnerOverwriteFinding {
            hooks_dir: td.path().to_path_buf(),
            chain_path: td.path().join("pre-commit"),
            chain_path_sha256: String::new(),
            detected_managers: Vec::new(),
            repo_artefacts: Vec::new(),
            orig_present: false,
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
