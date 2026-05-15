//! `fm-environment_toolchain-am-git-binary-points-at-missing-file` — P0.
//!
//! **Subsystem**: environment_toolchain (Phase 1 archaeology — HANDOFF
//! P3-C #5 ranking).
//!
//! ## What's broken
//!
//! `AM_GIT_BINARY` is configured (either via process env or via the
//! `$XDG_CONFIG_HOME/mcp-agent-mail/config.env` file) but the value
//! points at a path that:
//! - doesn't exist on disk, OR
//! - exists but isn't a regular file (e.g., a directory left over
//!   from a package uninstall), OR
//! - is a symlink whose target is missing or non-executable, OR
//! - exists as a regular file but lacks the executable bit.
//!
//! This is P0 because Agent Mail's git shell-outs use `AM_GIT_BINARY`
//! when set (to escape known-bad system git versions). If the
//! override is broken, every archive operation either falls back to
//! a potentially-corrupt system git OR fails outright depending on
//! call-site resolution policy.
//!
//! Sibling of `fm-environment_toolchain-known-bad-git-no-override`:
//! that FM flags "system git is bad AND override is missing/invalid".
//! This FM flags "the override is broken on its own merits"
//! regardless of whether the system git is bad — operators may have
//! set the override on a previously-safe system git for forward-
//! compatibility, and the doctor should still warn when the
//! configured path rots.
//!
//! ## Detection (pure function)
//!
//! 1. Resolve the configured value (`DetectInputs::am_git_binary_value`).
//!    `None` (neither config nor env) → not our FM (different one —
//!    `known_bad_git_no_override` handles the "no override" case).
//! 2. Expand `~` to `$HOME` (literal path lookups don't auto-expand).
//! 3. `fs::symlink_metadata(path)`:
//!    - ENOENT → `Reason::Missing`
//!    - is_dir() / is_block() / is_socket() → `Reason::NotAFile`
//!    - is_symlink() → follow once via `canonicalize`:
//!      - canonicalize Err → `Reason::DanglingSymlink`
//!      - target not executable → `Reason::SymlinkTargetNotExecutable`
//!      - target executable → no finding
//!    - is_file() AND mode & 0o111 == 0 → `Reason::NotExecutable`
//!    - is_file() AND executable → no finding
//!
//! ## Fix
//!
//! Detect-only initially. The full fix (rewrite `config.env`'s
//! `AM_GIT_BINARY=...` line in-place via `Op::WriteFile` while
//! preserving every other byte) requires:
//! - safe-git discovery on PATH (via
//!   `mcp_agent_mail_core::git_binary::resolve_git_binary` or PATH
//!   probe),
//! - `match_known_bad` rejection of the discovered candidate,
//! - dotenv-preserving rewriter that keeps comments, blank lines,
//!   and order intact.
//!
//! Those are filed as follow-up work; the detector + manual_remediation
//! envelope already gives operators an actionable signal.
//!
//! `auto_fixable: false` (detect-only); `fix()` is a no-op for API
//! uniformity.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-environment_toolchain-am-git-binary-points-at-missing-file";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "environment_toolchain";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Reason {
    /// File doesn't exist on disk (ENOENT).
    Missing,
    /// Path exists but isn't a regular file or symlink (dir, fifo,
    /// socket, block device, etc.).
    NotAFile,
    /// Symlink whose target can't be resolved (broken link chain).
    DanglingSymlink,
    /// Symlink whose target exists but isn't executable.
    SymlinkTargetNotExecutable,
    /// Regular file that lacks any executable bit (0o111 clear).
    NotExecutable,
}

impl Reason {
    fn as_kebab(self) -> &'static str {
        match self {
            Reason::Missing => "missing",
            Reason::NotAFile => "not_a_file",
            Reason::DanglingSymlink => "dangling_symlink",
            Reason::SymlinkTargetNotExecutable => "symlink_target_not_executable",
            Reason::NotExecutable => "not_executable",
        }
    }
}

/// Where the broken value came from. Surfaced to the operator so
/// they know which surface to fix (config.env vs. shell rc /
/// process env).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Source {
    /// Came from `$XDG_CONFIG_HOME/mcp-agent-mail/config.env`.
    ConfigFile,
    /// Came from the process environment only (e.g., shell rc).
    ProcessEnv,
    /// Set in both, with the same or different values. The detector
    /// reports the config.env value (which the fixer would manage)
    /// and notes both surfaces in the evidence.
    Both,
}

#[derive(Debug, Clone, Serialize)]
pub struct AmGitBinaryMissingFinding {
    /// The configured path that's broken (after `~` expansion).
    pub configured_path: PathBuf,
    /// The raw value as configured, pre-expansion (for evidence).
    pub raw_value: String,
    pub source: Source,
    pub reason: Reason,
}

impl AmGitBinaryMissingFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "AM_GIT_BINARY={} ({}) — {}",
            self.configured_path.display(),
            match self.source {
                Source::ConfigFile => "from config.env",
                Source::ProcessEnv => "from process env",
                Source::Both => "from config.env + process env",
            },
            self.reason.as_kebab(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "configured_path": self.configured_path.to_string_lossy(),
                "raw_value": self.raw_value,
                "source": match self.source {
                    Source::ConfigFile => "config_file",
                    Source::ProcessEnv => "process_env",
                    Source::Both => "both",
                },
                "reason": self.reason.as_kebab(),
                "remediation_doc": "docs/RECOVERY_RUNBOOK.md#git-2-51-0-index-race",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    /// Operator-facing instruction text (manual_remediation envelope).
    pub fn manual_remediation_text(&self) -> String {
        let action = match self.source {
            Source::ConfigFile => {
                "Edit $XDG_CONFIG_HOME/mcp-agent-mail/config.env (or \
                                   `~/.config/mcp-agent-mail/config.env`) and either point \
                                   AM_GIT_BINARY at a working git binary or remove the line."
            }
            Source::ProcessEnv => {
                "AM_GIT_BINARY is set in your shell. Either fix the path \
                                   in your shell rc OR set it explicitly in \
                                   $XDG_CONFIG_HOME/mcp-agent-mail/config.env (the doctor's \
                                   managed surface)."
            }
            Source::Both => {
                "AM_GIT_BINARY is set in BOTH config.env and your shell. Fix or \
                             remove from both surfaces; the config.env value takes precedence \
                             for doctor operations."
            }
        };
        format!(
            "AM_GIT_BINARY={} is {} ({}). {}",
            self.configured_path.display(),
            self.reason.as_kebab(),
            match self.source {
                Source::ConfigFile => "config.env",
                Source::ProcessEnv => "process env",
                Source::Both => "config.env + process env",
            },
            action,
        )
    }
}

/// Input for the detector. Lets tests inject all sources rather
/// than reading the live config.env + process env. Production
/// callers (CLI handlers) populate this from XDG_CONFIG_HOME +
/// `std::env::var("AM_GIT_BINARY")`.
#[derive(Debug, Clone, Default)]
pub struct DetectInputs {
    /// Value of `AM_GIT_BINARY` from the doctor-managed config
    /// file (`$XDG_CONFIG_HOME/mcp-agent-mail/config.env`). `None`
    /// means the file or the key is absent.
    pub config_env_value: Option<String>,
    /// Value of `AM_GIT_BINARY` from the process environment.
    /// `None` means unset.
    pub process_env_value: Option<String>,
    /// Override for `$HOME` expansion. `None` uses `dirs::home_dir()`.
    /// Tests should set this to a temp dir to keep expansions
    /// hermetic.
    pub home_override: Option<PathBuf>,
}

/// Detector. PURE w.r.t. caller-supplied inputs; performs filesystem
/// stat calls on the resolved path but never writes.
///
/// Pass-35-review Codex F2 (P1): validate BOTH surfaces independently
/// when both are present. The runtime git resolution path
/// (`mcp_agent_mail_core::git_binary`) is env-driven; if config.env
/// is valid but the process env points at a broken path, that's
/// the surface that actually breaks `am serve`'s git shell-outs.
/// The pre-fix code only validated config.env when both were set,
/// false-negating that scenario.
///
/// Behavior:
/// - Both surfaces unset: not our FM.
/// - Either surface set with equal values: validate once; if broken,
///   emit a single finding with `Source::Both`.
/// - Surfaces set with different values: validate each independently;
///   emit one finding per broken surface (so an operator can see
///   that config.env is fine but the shell rc / process env is
///   broken, or vice versa).
pub fn detect(inputs: &DetectInputs) -> Vec<AmGitBinaryMissingFinding> {
    let cfg = inputs
        .config_env_value
        .as_deref()
        .filter(|v| !v.trim().is_empty());
    let env = inputs
        .process_env_value
        .as_deref()
        .filter(|v| !v.trim().is_empty());
    match (cfg, env) {
        (None, None) => Vec::new(),
        (Some(v), None) => probe_single(v, Source::ConfigFile, inputs.home_override.as_deref()),
        (None, Some(v)) => probe_single(v, Source::ProcessEnv, inputs.home_override.as_deref()),
        (Some(c), Some(e)) if c == e => {
            // Same value in both surfaces — one finding, source=Both.
            probe_single(c, Source::Both, inputs.home_override.as_deref())
        }
        (Some(c), Some(e)) => {
            // Differing surfaces — validate independently and emit
            // a separate finding per broken side.
            let mut out = probe_single(c, Source::ConfigFile, inputs.home_override.as_deref());
            out.extend(probe_single(
                e,
                Source::ProcessEnv,
                inputs.home_override.as_deref(),
            ));
            out
        }
    }
}

/// Run the path classifier against a single raw value/source pair.
/// Returns `Vec::new()` when the path is healthy.
fn probe_single(
    raw_value: &str,
    source: Source,
    home_override: Option<&Path>,
) -> Vec<AmGitBinaryMissingFinding> {
    let expanded = expand_tilde(raw_value, home_override);
    match classify_path(&expanded) {
        None => Vec::new(),
        Some(reason) => vec![AmGitBinaryMissingFinding {
            configured_path: expanded,
            raw_value: raw_value.to_string(),
            source,
            reason,
        }],
    }
}

fn expand_tilde(value: &str, home_override: Option<&Path>) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        let home = home_override
            .map(Path::to_path_buf)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        home.join(rest)
    } else if value == "~" {
        home_override
            .map(Path::to_path_buf)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    } else {
        PathBuf::from(value)
    }
}

fn classify_path(path: &Path) -> Option<Reason> {
    let lmeta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Reason::Missing),
        // Permission denied stat'ing the configured path means we
        // can't probe; not-our-FM (downstream git shell-out will
        // fail loudly). Don't false-flag.
        Err(_) => return None,
    };
    let ftype = lmeta.file_type();
    if ftype.is_symlink() {
        let real = match std::fs::canonicalize(path) {
            Ok(r) => r,
            Err(_) => return Some(Reason::DanglingSymlink),
        };
        return if is_executable(&real) {
            None
        } else {
            Some(Reason::SymlinkTargetNotExecutable)
        };
    }
    if !ftype.is_file() {
        return Some(Reason::NotAFile);
    }
    if !is_executable(path) {
        return Some(Reason::NotExecutable);
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    // Windows: executability is determined by file extension /
    // PATHEXT. Conservatively report executable (the downstream
    // shell-out will fail loudly if not).
    true
}

/// Detect-only FM. `fix()` is a no-op for API uniformity.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &AmGitBinaryMissingFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
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

    #[cfg(unix)]
    fn make_executable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(p, perms).unwrap();
    }

    #[cfg(unix)]
    fn make_non_executable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn detector_returns_empty_when_neither_source_set() {
        let inputs = DetectInputs::default();
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_returns_empty_when_config_value_is_blank() {
        let inputs = DetectInputs {
            config_env_value: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_flags_missing_path() {
        let td = TempDir::new().unwrap();
        let inputs = DetectInputs {
            config_env_value: Some(td.path().join("nope/git").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::Missing);
        assert_eq!(findings[0].source, Source::ConfigFile);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_non_executable_regular_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("git");
        fs::write(&p, "#!/bin/sh\necho fake").unwrap();
        make_non_executable(&p);
        let inputs = DetectInputs {
            config_env_value: Some(p.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::NotExecutable);
    }

    #[cfg(unix)]
    #[test]
    fn detector_returns_empty_for_executable_regular_file() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("git");
        fs::write(&p, "#!/bin/sh\necho fake").unwrap();
        make_executable(&p);
        let inputs = DetectInputs {
            config_env_value: Some(p.to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_flags_directory_as_not_a_file() {
        let td = TempDir::new().unwrap();
        let inputs = DetectInputs {
            config_env_value: Some(td.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::NotAFile);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_dangling_symlink() {
        let td = TempDir::new().unwrap();
        let link = td.path().join("git-link");
        let target = td.path().join("missing-target");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let inputs = DetectInputs {
            config_env_value: Some(link.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::DanglingSymlink);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_symlink_to_non_executable() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("real");
        fs::write(&target, b"not exec").unwrap();
        make_non_executable(&target);
        let link = td.path().join("git-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let inputs = DetectInputs {
            config_env_value: Some(link.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::SymlinkTargetNotExecutable);
    }

    #[test]
    fn detector_reports_both_source_when_set_twice() {
        let td = TempDir::new().unwrap();
        let bogus = td.path().join("nope/git").to_string_lossy().into_owned();
        let inputs = DetectInputs {
            config_env_value: Some(bogus.clone()),
            process_env_value: Some(bogus),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, Source::Both);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_each_surface_independently_when_values_differ() {
        // Pass-35-review Codex F2 (P1): if config.env is valid but
        // process env points at a broken path, the runtime git
        // shell-outs (env-driven) will fail. The detector must
        // surface the broken process env even when config.env is
        // healthy. Pre-fix the detector only validated config.env
        // when both were set, producing a false negative.
        let td = TempDir::new().unwrap();
        let good = td.path().join("good-git");
        fs::write(&good, "#!/bin/sh\necho").unwrap();
        make_executable(&good);
        let bad = td.path().join("nope/git").to_string_lossy().into_owned();
        let inputs = DetectInputs {
            config_env_value: Some(good.to_string_lossy().into_owned()),
            process_env_value: Some(bad),
            ..Default::default()
        };
        let findings = detect(&inputs);
        // Config.env is fine; process env is broken. Exactly one
        // finding, sourced to ProcessEnv.
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, Source::ProcessEnv);
        assert_eq!(findings[0].reason, Reason::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_both_surfaces_when_each_is_independently_broken() {
        // Pass-35-review Codex F2 (P1): differing surfaces with
        // different brokenness — one finding per side.
        let td = TempDir::new().unwrap();
        // config.env path: a directory (NotAFile).
        let cfg_dir = td.path().join("cfg-dir");
        fs::create_dir(&cfg_dir).unwrap();
        // process env path: doesn't exist (Missing).
        let proc_missing = td.path().join("proc/nope");
        let inputs = DetectInputs {
            config_env_value: Some(cfg_dir.to_string_lossy().into_owned()),
            process_env_value: Some(proc_missing.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 2);
        // Order is deterministic: config.env first, then process_env.
        assert_eq!(findings[0].source, Source::ConfigFile);
        assert_eq!(findings[0].reason, Reason::NotAFile);
        assert_eq!(findings[1].source, Source::ProcessEnv);
        assert_eq!(findings[1].reason, Reason::Missing);
    }

    #[test]
    fn detector_reports_process_env_source_when_only_env_set() {
        let td = TempDir::new().unwrap();
        let bogus = td.path().join("nope/git").to_string_lossy().into_owned();
        let inputs = DetectInputs {
            process_env_value: Some(bogus),
            ..Default::default()
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, Source::ProcessEnv);
    }

    #[cfg(unix)]
    #[test]
    fn detector_expands_tilde_via_home_override() {
        let td = TempDir::new().unwrap();
        // Pretend $HOME = td.path(); ~/git references td/git.
        let p = td.path().join("git");
        fs::write(&p, "#!/bin/sh\necho").unwrap();
        make_executable(&p);
        let inputs = DetectInputs {
            config_env_value: Some("~/git".to_string()),
            home_override: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        assert!(detect(&inputs).is_empty(), "~ expansion should resolve");
    }

    #[test]
    fn finding_is_p0_detect_only() {
        let f = AmGitBinaryMissingFinding {
            configured_path: PathBuf::from("/nonexistent"),
            raw_value: "/nonexistent".to_string(),
            source: Source::ConfigFile,
            reason: Reason::Missing,
        };
        let g = f.to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "environment_toolchain");
        assert!(!g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("missing"));
    }

    #[test]
    fn manual_remediation_text_includes_path_and_source() {
        let f = AmGitBinaryMissingFinding {
            configured_path: PathBuf::from("/opt/git-2.50.2/bin/git"),
            raw_value: "/opt/git-2.50.2/bin/git".to_string(),
            source: Source::Both,
            reason: Reason::Missing,
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("/opt/git-2.50.2/bin/git"));
        assert!(text.contains("config.env"));
    }
}
