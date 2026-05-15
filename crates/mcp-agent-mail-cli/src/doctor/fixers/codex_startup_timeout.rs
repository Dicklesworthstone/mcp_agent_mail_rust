//! `fm-mcp-config-files-codex-startup-timeout-too-short` — P1.
//!
//! **Subsystem**: mcp_config_files.
//!
//! ## What's broken
//!
//! Codex's `config.toml` either omits the
//! `startup_timeout_sec` setting for the `[mcp_servers.mcp_agent_mail]`
//! (or `[mcp_servers."mcp-agent-mail"]`) section, OR sets it below the
//! recommended minimum of `CODEX_STARTUP_TIMEOUT_SECS` (30s).
//!
//! Codex aborts MCP server boot if no response arrives inside the
//! configured window. For `mcp-agent-mail`, normal cold boot
//! (DB pool warm-up + git archive scan) routinely exceeds 10s, so
//! anything below 30 trips a confusing "MCP server didn't respond
//! in time" error.
//!
//! ## Detection (pure function)
//!
//! For each `McpConfigLocation` where `tool == McpConfigTool::Codex`
//! and `exists`:
//!
//! 1. Read the file contents.
//! 2. Call `extract_mcp_agent_mail_toml_startup_timeout`
//!    (the CLI's existing helper) which parses the
//!    `[mcp_servers.mcp_agent_mail]` (or quoted-key variant) section
//!    and returns the configured `startup_timeout_sec` value.
//! 3. Decide:
//!    - `None`  → `TimeoutState::Missing` (no setting at all)
//!    - `Some(t < 30)` → `TimeoutState::TooShort(t)`
//!    - `Some(t >= 30)` → no finding.
//!
//! Files that don't exist or can't be read are silently skipped
//! (a sibling FM owns the "config file missing" surface).
//!
//! ## Fix
//!
//! **Detect-only.** A safe auto-fix requires TOML byte-exact
//! preservation (comments, key ordering, trailing whitespace),
//! which is a substantial engineering surface and out of scope
//! for this commit. The manual remediation envelope gives the
//! operator the exact line to add / change.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use crate::{CODEX_STARTUP_TIMEOUT_SECS, extract_mcp_agent_mail_toml_startup_timeout};
use mcp_agent_mail_core::mcp_config::{McpConfigLocation, McpConfigTool};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-mcp-config-files-codex-startup-timeout-too-short";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "mcp_config_files";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum TimeoutState {
    /// `startup_timeout_sec` is absent from the
    /// `[mcp_servers.mcp_agent_mail]` section (or the section
    /// itself is missing).
    Missing,
    /// `startup_timeout_sec` is set but below `MIN_SECS`.
    TooShort { observed_secs: u64 },
}

impl TimeoutState {
    fn as_kebab(self) -> &'static str {
        match self {
            TimeoutState::Missing => "missing",
            TimeoutState::TooShort { .. } => "too_short",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexStartupTimeoutFinding {
    pub config_path: PathBuf,
    pub state: TimeoutState,
    /// Recommended minimum (matches `CODEX_STARTUP_TIMEOUT_SECS`
    /// in the CLI). Surfaced for evidence so operators don't have
    /// to guess.
    pub min_required_secs: u64,
}

impl CodexStartupTimeoutFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = match self.state {
            TimeoutState::Missing => format!(
                "Codex config {} has no startup_timeout_sec for mcp_agent_mail (need ≥{}s)",
                self.config_path.display(),
                self.min_required_secs,
            ),
            TimeoutState::TooShort { observed_secs } => format!(
                "Codex config {} has startup_timeout_sec={} (need ≥{}s for mcp_agent_mail cold boot)",
                self.config_path.display(),
                observed_secs,
                self.min_required_secs,
            ),
        };
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "config_path": self.config_path.to_string_lossy(),
                "state": self.state.as_kebab(),
                "observed_secs": match self.state {
                    TimeoutState::Missing => None,
                    TimeoutState::TooShort { observed_secs } => Some(observed_secs),
                },
                "min_required_secs": self.min_required_secs,
                "tool": "codex",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only — TOML in-place edit is a separate
                // surface (filed as follow-up).
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "Edit {} and ensure the [mcp_servers.mcp_agent_mail] section contains \
             `startup_timeout_sec = {}` (or quoted variant `[mcp_servers.\"mcp-agent-mail\"]`). \
             Codex cold-boots mcp-agent-mail in ~10s under normal conditions; a smaller \
             timeout produces flaky 'MCP server didn't respond' errors. Auto-fix is \
             detect-only because TOML byte-exact in-place editing is out of scope for this \
             FM revision.",
            self.config_path.display(),
            self.min_required_secs,
        )
    }
}

/// Detector. PURE w.r.t. caller-supplied locations.
pub fn detect(locations: &[McpConfigLocation]) -> Vec<CodexStartupTimeoutFinding> {
    let mut out = Vec::new();
    for loc in locations {
        if loc.tool != McpConfigTool::Codex || !loc.exists {
            continue;
        }
        let contents = match std::fs::read_to_string(&loc.config_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Only target .toml files (Codex's canonical config
        // format). Skip JSON-style codex configs if any operator
        // has put one — those use a different schema.
        if loc.config_path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let state = match extract_mcp_agent_mail_toml_startup_timeout(&contents) {
            None => TimeoutState::Missing,
            Some(t) if t < CODEX_STARTUP_TIMEOUT_SECS => TimeoutState::TooShort { observed_secs: t },
            Some(_) => continue, // healthy — no finding
        };
        out.push(CodexStartupTimeoutFinding {
            config_path: loc.config_path.clone(),
            state,
            min_required_secs: CODEX_STARTUP_TIMEOUT_SECS,
        });
    }
    out
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &CodexStartupTimeoutFinding,
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

    fn loc_for(path: PathBuf, tool: McpConfigTool, exists: bool) -> McpConfigLocation {
        McpConfigLocation {
            tool,
            config_path: path,
            exists,
        }
    }

    #[test]
    fn detector_returns_empty_when_no_codex_locations() {
        let td = TempDir::new().unwrap();
        // Only a Claude config — should not trigger.
        let p = td.path().join("claude.json");
        fs::write(&p, r#"{"mcp_servers":{"mcp-agent-mail":{}}}"#).unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Claude, true)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_missing_timeout_in_codex_toml() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers.mcp_agent_mail]
command = "mcp-agent-mail"
"#,
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].state, TimeoutState::Missing);
        assert_eq!(findings[0].min_required_secs, CODEX_STARTUP_TIMEOUT_SECS);
    }

    #[test]
    fn detector_flags_too_short_timeout() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers.mcp_agent_mail]
command = "mcp-agent-mail"
startup_timeout_sec = 5
"#,
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].state, TimeoutState::TooShort { observed_secs: 5 });
    }

    #[test]
    fn detector_does_not_flag_healthy_timeout() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers.mcp_agent_mail]
command = "mcp-agent-mail"
startup_timeout_sec = 60
"#,
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_accepts_threshold_exactly() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            format!(
                "[mcp_servers.mcp_agent_mail]\nstartup_timeout_sec = {}\n",
                CODEX_STARTUP_TIMEOUT_SECS,
            ),
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert!(findings.is_empty(), "exactly threshold value must not flag");
    }

    #[test]
    fn detector_accepts_quoted_section_variant() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers."mcp-agent-mail"]
startup_timeout_sec = 5
"#,
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].state, TimeoutState::TooShort { observed_secs: 5 });
    }

    #[test]
    fn detector_skips_non_toml_codex_files() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(&p, "{}").unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_nonexistent_locations() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("absent.toml");
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, false)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn finding_severity_is_p1_detect_only() {
        let f = CodexStartupTimeoutFinding {
            config_path: PathBuf::from("/x/config.toml"),
            state: TimeoutState::Missing,
            min_required_secs: 30,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P1");
        assert!(!g.remediation.auto_fixable);
    }

    #[test]
    fn manual_remediation_includes_config_path_and_threshold() {
        let f = CodexStartupTimeoutFinding {
            config_path: PathBuf::from("/home/op/.codex/config.toml"),
            state: TimeoutState::TooShort { observed_secs: 5 },
            min_required_secs: 30,
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("/home/op/.codex/config.toml"));
        assert!(text.contains("startup_timeout_sec = 30"));
    }
}
