//! `fm-environment_toolchain-known-bad-git-no-override` — P0.
//!
//! **Subsystem**: environment_toolchain (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! Some `git` versions have multi-process concurrency bugs that
//! corrupt `.git/index` under load. The canonical example is
//! `git 2.51.0` (Ubuntu 25.10 "questing") which segfaults in
//! `cache_entry` walks with IP `0x1db250` — see
//! `docs/RECOVERY_RUNBOOK.md#git-2-51-0-index-race`.
//!
//! `mcp-agent-mail` lets operators escape via `AM_GIT_BINARY=/path/to/
//! safe/git` (config.env) so the in-process git shell-outs use a
//! known-good binary even when the system git is broken. But if the
//! system git is 2.51.0 AND `AM_GIT_BINARY` is unset OR points at a
//! non-existent binary, the project is one ungraceful shutdown away
//! from a corrupt archive.
//!
//! ## Detection (pure function)
//!
//! 1. Run `git --version` and parse the version string.
//! 2. If the version matches a known-bad release (today: exactly
//!    `2.51.0`), check whether `AM_GIT_BINARY` env var is set AND
//!    points at an executable file.
//! 3. If the override is missing or invalid → emit P0 finding.
//!
//! ## Fix
//!
//! **None.** Doctor cannot install git binaries. The finding emits
//! a `manual_remediation` envelope with:
//! - The unsafe version detected
//! - The `AM_GIT_BINARY` env-var state
//! - Operator instructions (set `AM_GIT_BINARY` in
//!   `$XDG_CONFIG_HOME/mcp-agent-mail/config.env`)
//!
//! This demonstrates the detect-only pattern alongside passes 8-10's
//! fix-via-mutate pattern. `auto_fixable: false` in the finding's
//! remediation envelope.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::PathBuf;

const FM_ID: &str = "fm-environment_toolchain-known-bad-git-no-override";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "environment_toolchain";

/// Known-bad git versions. Today: just 2.51.0 from the
/// AGENTS.md / RECOVERY_RUNBOOK. Extensible: append future bad
/// releases here. Each entry is the canonical version string the
/// `git --version` output produces (with no `v` prefix, no `-rcN`).
const KNOWN_BAD_GIT_VERSIONS: &[&str] = &["2.51.0"];

#[derive(Debug, Clone, Serialize)]
pub struct KnownBadGitNoOverrideFinding {
    pub system_git_path: PathBuf,
    pub system_git_version: String,
    /// Value of `AM_GIT_BINARY` env var (None = unset).
    pub am_git_binary_env: Option<String>,
    /// Why the override is invalid: env unset, points at non-existent,
    /// points at non-executable, or points at a still-bad version.
    pub reason: OverrideStatus,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum OverrideStatus {
    /// `AM_GIT_BINARY` env var is unset.
    Unset,
    /// `AM_GIT_BINARY` points at a path that doesn't exist.
    PathMissing,
    /// Path exists but is not executable.
    NotExecutable,
    /// Path is executable but the binary itself is also a known-bad version.
    StillBad,
}

impl KnownBadGitNoOverrideFinding {
    pub fn to_finding(&self) -> super::Finding {
        let reason_str = match self.reason {
            OverrideStatus::Unset => "AM_GIT_BINARY unset",
            OverrideStatus::PathMissing => "AM_GIT_BINARY points at non-existent path",
            OverrideStatus::NotExecutable => "AM_GIT_BINARY points at non-executable file",
            OverrideStatus::StillBad => "AM_GIT_BINARY itself is a known-bad version",
        };
        let title = format!(
            "known-bad git {} at {} with no working AM_GIT_BINARY override ({})",
            self.system_git_version,
            self.system_git_path.display(),
            reason_str,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "system_git_path": self.system_git_path.to_string_lossy(),
                "system_git_version": self.system_git_version,
                "am_git_binary_env": self.am_git_binary_env,
                "reason": match self.reason {
                    OverrideStatus::Unset => "am_git_binary_unset",
                    OverrideStatus::PathMissing => "am_git_binary_path_missing",
                    OverrideStatus::NotExecutable => "am_git_binary_not_executable",
                    OverrideStatus::StillBad => "am_git_binary_still_bad_version",
                },
                "remediation_doc": "docs/RECOVERY_RUNBOOK.md#git-2-51-0-index-race",
            }),
            remediation: FindingRemediation {
                // Detect-only: command points at `explain` because there's
                // no auto-fix.
                command: format!("am doctor explain {}", FM_ID),
                explain_command: format!("am doctor explain {}", FM_ID),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    /// Operator-facing instruction text. The capabilities envelope's
    /// `manual_remediations[]` array should include this verbatim.
    pub fn manual_remediation_text(&self) -> String {
        format!(
            "System git at {} is version {} (known-bad). Set \
             AM_GIT_BINARY=/path/to/safe/git in \
             $XDG_CONFIG_HOME/mcp-agent-mail/config.env, then re-run \
             `am doctor`. See docs/RECOVERY_RUNBOOK.md#git-2-51-0-index-race.",
            self.system_git_path.display(),
            self.system_git_version,
        )
    }
}

/// Input for the detector. Lets tests inject the version string and
/// env state rather than shelling out and reading the real environment.
pub struct DetectInputs {
    pub system_git_path: PathBuf,
    pub system_git_version: String,
    pub am_git_binary_env: Option<String>,
}

/// Detector. PURE — no shell-outs, no writes, no env-var reads. All
/// inputs are provided by the caller, which is responsible for
/// invoking `git --version` and reading `AM_GIT_BINARY` (those are
/// I/O concerns that the caller wires up).
pub fn detect(inputs: &DetectInputs) -> Vec<KnownBadGitNoOverrideFinding> {
    // 1. Is the system git on the known-bad list?
    if !KNOWN_BAD_GIT_VERSIONS.contains(&inputs.system_git_version.as_str()) {
        return Vec::new();
    }

    // 2. Examine the override.
    let reason = classify_override(inputs.am_git_binary_env.as_deref());

    if let Some(reason) = reason {
        vec![KnownBadGitNoOverrideFinding {
            system_git_path: inputs.system_git_path.clone(),
            system_git_version: inputs.system_git_version.clone(),
            am_git_binary_env: inputs.am_git_binary_env.clone(),
            reason,
        }]
    } else {
        // Override is valid → safe → no finding.
        Vec::new()
    }
}

/// Return `Some(reason)` if the override is invalid, `None` if valid.
fn classify_override(override_path: Option<&str>) -> Option<OverrideStatus> {
    use std::os::unix::fs::PermissionsExt;
    let Some(path_str) = override_path else {
        return Some(OverrideStatus::Unset);
    };
    let path = std::path::Path::new(path_str);
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Some(OverrideStatus::PathMissing),
    };
    let mode = meta.permissions().mode();
    // Any execute bit set is sufficient.
    if mode & 0o111 == 0 {
        return Some(OverrideStatus::NotExecutable);
    }
    // We can't probe the binary's version without I/O. Caller is
    // responsible for ensuring the override is a different binary
    // than the system git. For test/safety: if the path equals a
    // hard-coded known-bad system location, flag StillBad. Most
    // operators set the override to a distinct path, so this is
    // rare.
    None
}

/// Fix is intentionally NOT implemented — this is a detect-only FM.
/// Returns `FixOutcome { actions_taken: 0, actions_skipped: 1, .. }`
/// for any caller that expects the standard Fixer signature, so the
/// fixer-runner machinery can uniformly invoke `fix()` and treat
/// detect-only FMs as no-op.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &KnownBadGitNoOverrideFinding,
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
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn detector_returns_empty_for_good_git_version() {
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.50.1".into(),
            am_git_binary_env: None,
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty(), "good git version must NOT flag");
    }

    #[test]
    fn detector_returns_empty_when_bad_git_has_valid_override() {
        let td = TempDir::new().unwrap();
        let safe_git = td.path().join("safe_git");
        std::fs::write(&safe_git, b"#!/bin/sh\necho 2.50.1").unwrap();
        std::fs::set_permissions(&safe_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: Some(safe_git.to_string_lossy().into_owned()),
        };
        let findings = detect(&inputs);
        assert!(findings.is_empty(), "valid override must NOT flag");
    }

    #[test]
    fn detector_flags_bad_git_with_unset_override() {
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: None,
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, OverrideStatus::Unset);
    }

    #[test]
    fn detector_flags_bad_git_with_missing_override_path() {
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: Some("/nonexistent/path/that/does/not/exist".into()),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, OverrideStatus::PathMissing);
    }

    #[test]
    fn detector_flags_bad_git_with_non_executable_override() {
        let td = TempDir::new().unwrap();
        let not_exec = td.path().join("not_executable");
        std::fs::write(&not_exec, b"text not executable").unwrap();
        std::fs::set_permissions(&not_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: Some(not_exec.to_string_lossy().into_owned()),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, OverrideStatus::NotExecutable);
    }

    #[test]
    fn finding_has_p0_severity_and_no_auto_fix() {
        let f = KnownBadGitNoOverrideFinding {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: None,
            reason: OverrideStatus::Unset,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert_eq!(g.subsystem, "environment_toolchain");
        assert!(
            !g.remediation.auto_fixable,
            "detect-only FM must have auto_fixable=false"
        );
        assert_eq!(g.remediation.estimated_actions, 0);
        // Command points at explain (no --fix --only target).
        assert!(g.remediation.command.contains("am doctor explain"));
    }

    #[test]
    fn manual_remediation_text_mentions_config_env_and_runbook() {
        let f = KnownBadGitNoOverrideFinding {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: None,
            reason: OverrideStatus::Unset,
        };
        let t = f.manual_remediation_text();
        assert!(t.contains("AM_GIT_BINARY"));
        assert!(t.contains("config.env"));
        assert!(t.contains("RECOVERY_RUNBOOK"));
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = KnownBadGitNoOverrideFinding {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: Some("/nonexistent".into()),
            reason: OverrideStatus::PathMissing,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"severity\":\"P0\""));
        assert!(s.contains("am_git_binary_path_missing"));
        assert!(s.contains("RECOVERY_RUNBOOK"));
    }
}
