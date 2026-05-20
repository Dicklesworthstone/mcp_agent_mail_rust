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
//!    `2.51.0`), check whether `AM_GIT_BINARY` env var is set, points
//!    at an executable file, and reports a safe Git version when probed.
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
use mcp_agent_mail_core::git_binary::{GitVersion, KnownBadEntry, match_known_bad};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-environment_toolchain-known-bad-git-no-override";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "environment_toolchain";

// Pass-20: the canonical known-bad-git registry lives in
// `mcp_agent_mail_core::git_binary` and is data-driven via
// `data/known_bad_git_versions.json` plus the operator-extensible
// `AM_EXTRA_KNOWN_BAD_GIT_JSON` and `AM_IGNORE_KNOWN_BAD_GIT` env
// vars. The fixer used to carry a hardcoded `["2.51.0"]` list,
// which silently drifted away from the registry: an operator
// extending the JSON would see the core path refuse the bad
// binary, but `am doctor --fix --only ...` would still report
// "no findings". Routing detection through `match_known_bad`
// makes that drift structurally impossible — same defect class
// as pass-18 (BACKUP_SUFFIX_HINTS) and pass-19 (stale_seconds).

#[derive(Debug, Clone, Serialize)]
pub struct KnownBadGitNoOverrideFinding {
    pub system_git_path: PathBuf,
    pub system_git_version: String,
    /// Value of `AM_GIT_BINARY` env var (None = unset).
    pub am_git_binary_env: Option<String>,
    /// Parsed version string from `AM_GIT_BINARY --version` when the
    /// caller could probe it. None means the path/env checks failed
    /// first or the probe was unavailable.
    pub am_git_binary_version: Option<String>,
    /// Why the override is invalid: env unset, points at non-existent,
    /// points at non-executable, or points at a still-bad version.
    pub reason: OverrideStatus,
    /// Canonical metadata for the matched known-bad entry (code,
    /// severity, remediation_ref). Pass-20: surfaced from the core
    /// registry so agents see the full operator-extensible match
    /// data, not just a boolean.
    pub matched_entry: Option<KnownBadEntry>,
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
                "am_git_binary_version": self.am_git_binary_version,
                "reason": match self.reason {
                    OverrideStatus::Unset => "am_git_binary_unset",
                    OverrideStatus::PathMissing => "am_git_binary_path_missing",
                    OverrideStatus::NotExecutable => "am_git_binary_not_executable",
                    OverrideStatus::StillBad => "am_git_binary_still_bad_version",
                },
                "remediation_doc": "docs/RECOVERY_RUNBOOK.md#git-2-51-0-index-race",
                // Pass-20: full canonical known-bad-entry metadata —
                // code (e.g. "GIT_2_51_0_INDEX_RACE"), severity
                // ("fail" / "warn"), fingerprint, remediation_ref.
                // Agents that extend AM_EXTRA_KNOWN_BAD_GIT_JSON pick
                // up custom entries here automatically.
                "matched_entry": self.matched_entry,
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
#[derive(Debug, Clone)]
pub struct DetectInputs {
    pub system_git_path: PathBuf,
    pub system_git_version: String,
    pub am_git_binary_env: Option<String>,
    pub am_git_binary_version: Option<String>,
}

/// Detector. PURE w.r.t. caller-supplied version + env state; reads
/// from the process-static known-bad catalog cache in
/// `mcp_agent_mail_core::git_binary`. No shell-outs, no writes here.
///
/// Pass-20: consults `match_known_bad(GitVersion)` instead of the
/// fixer's old local `["2.51.0"]` list. This routes through the
/// canonical operator-extensible registry (embedded JSON +
/// `AM_EXTRA_KNOWN_BAD_GIT_JSON` + `AM_IGNORE_KNOWN_BAD_GIT`
/// suppress list). Same detector, same FM contract — the
/// difference is that operators who add new bad versions via the
/// JSON file get them flagged by `--only` automatically, no
/// fixer-side patch needed.
pub fn detect(inputs: &DetectInputs) -> Vec<KnownBadGitNoOverrideFinding> {
    // 1. Parse the version string. Lax — tolerates `2.51.0`, `2.51.0-rc1`,
    //    `2.51.0.windows.1`, `2.51.0+build.42`.
    let Some(version) = GitVersion::parse_lax(&inputs.system_git_version) else {
        return Vec::new();
    };

    // 2. Is this version in the canonical known-bad catalog (after
    //    suppress-list filtering)?
    let Some(entry) = match_known_bad(version) else {
        return Vec::new();
    };

    // 3. Examine the override.
    let Some(reason) = classify_override(
        inputs.am_git_binary_env.as_deref(),
        inputs.am_git_binary_version.as_deref(),
    ) else {
        // Override is valid → safe → no finding even though the
        // system git is known-bad.
        return Vec::new();
    };

    vec![KnownBadGitNoOverrideFinding {
        system_git_path: inputs.system_git_path.clone(),
        system_git_version: inputs.system_git_version.clone(),
        am_git_binary_env: inputs.am_git_binary_env.clone(),
        am_git_binary_version: inputs.am_git_binary_version.clone(),
        reason,
        matched_entry: Some(entry.clone()),
    }]
}

/// Return `Some(reason)` if the override is invalid, `None` if valid.
fn classify_override(
    override_path: Option<&str>,
    override_version: Option<&str>,
) -> Option<OverrideStatus> {
    let Some(path_str) = override_path else {
        return Some(OverrideStatus::Unset);
    };
    let path = std::path::Path::new(path_str);
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Some(OverrideStatus::PathMissing),
    };
    // The execute-bit check is a POSIX mode-bit concept. On Windows
    // executability is governed by file extension (.exe/.cmd/…) rather than
    // mode bits, so we don't flag a present override as "NotExecutable"
    // there (mirrors `path_order_shadows_am::is_executable`'s
    // `#[cfg(not(unix))]` no-op).
    if !meta_is_executable(&meta) {
        return Some(OverrideStatus::NotExecutable);
    }
    if override_version
        .and_then(GitVersion::parse_lax)
        .and_then(match_known_bad)
        .is_some()
    {
        return Some(OverrideStatus::StillBad);
    }
    None
}

/// Whether the override binary is executable. Unix: any execute bit set.
/// Windows: there are no POSIX execute bits — executability is governed by
/// file extension — so a present file is treated as executable.
#[cfg(unix)]
fn meta_is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn meta_is_executable(_meta: &std::fs::Metadata) -> bool {
    true
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
            am_git_binary_version: None,
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
            am_git_binary_version: Some("2.50.1".into()),
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
            am_git_binary_version: None,
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
            am_git_binary_version: None,
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
            am_git_binary_version: None,
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, OverrideStatus::NotExecutable);
    }

    #[test]
    fn detector_flags_bad_git_with_still_bad_override_version() {
        let td = TempDir::new().unwrap();
        let bad_git = td.path().join("bad_git");
        std::fs::write(&bad_git, b"#!/bin/sh\necho git version 2.51.0\n").unwrap();
        std::fs::set_permissions(&bad_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: Some(bad_git.to_string_lossy().into_owned()),
            am_git_binary_version: Some("2.51.0".into()),
        };

        let findings = detect(&inputs);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, OverrideStatus::StillBad);
    }

    #[test]
    fn finding_has_p0_severity_and_no_auto_fix() {
        let f = KnownBadGitNoOverrideFinding {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: None,
            am_git_binary_version: None,
            reason: OverrideStatus::Unset,
            matched_entry: None,
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
            am_git_binary_version: None,
            reason: OverrideStatus::Unset,
            matched_entry: None,
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
            am_git_binary_version: None,
            reason: OverrideStatus::PathMissing,
            matched_entry: None,
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"severity\":\"P0\""));
        assert!(s.contains("am_git_binary_path_missing"));
        assert!(s.contains("RECOVERY_RUNBOOK"));
    }

    #[test]
    fn detector_routes_through_core_registry_and_surfaces_entry() {
        // Pass-20 invariant. The detector consults
        // `mcp_agent_mail_core::git_binary::match_known_bad`, which
        // returns the canonical KnownBadEntry. Confirm a finding for
        // the embedded-catalog 2.51.0 entry surfaces the entry's
        // `code` field (e.g. "GIT_2_51_0_INDEX_RACE") so agents that
        // call `--only fm-... --list` get the operator-extensible
        // match metadata, not just a boolean.
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "2.51.0".into(),
            am_git_binary_env: None,
            am_git_binary_version: None,
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1, "2.51.0 is in the embedded catalog");
        let entry = findings[0]
            .matched_entry
            .as_ref()
            .expect("detector must populate matched_entry from core registry");
        assert!(
            !entry.code.is_empty(),
            "KnownBadEntry.code must not be empty"
        );
        // The embedded catalog ships the 2.51.0 entry as
        // GIT_2_51_0_INDEX_RACE — pin that so a future entry rename
        // is a deliberate change.
        assert_eq!(entry.code, "GIT_2_51_0_INDEX_RACE");
        assert!(!entry.summary.is_empty());
        assert!(!entry.remediation_ref.is_empty());
        // And the to_finding() evidence must include the entry.
        let g = findings[0].to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(
            s.contains("GIT_2_51_0_INDEX_RACE"),
            "evidence JSON must include the canonical entry code"
        );
    }

    #[test]
    fn detector_returns_empty_for_unparseable_version() {
        // GitVersion::parse_lax requires at least major.minor; a bare
        // word like "garbage" parses to None and the detector returns
        // empty (defensive — corrupt input from a non-git binary).
        let inputs = DetectInputs {
            system_git_path: "/usr/bin/git".into(),
            system_git_version: "garbage".into(),
            am_git_binary_env: None,
            am_git_binary_version: None,
        };
        let findings = detect(&inputs);
        assert!(
            findings.is_empty(),
            "unparseable version must NOT flag (no panic, no false positive)"
        );
    }
}
