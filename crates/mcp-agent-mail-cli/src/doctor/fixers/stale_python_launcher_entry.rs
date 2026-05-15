//! `fm-mcp-config-files-stale-python-launcher-entry` — P0.
//!
//! **Subsystem**: mcp_config_files.
//!
//! ## What's broken
//!
//! One or more MCP client config files (Claude, Codex, Cursor,
//! Gemini, ...) still declares the `mcp_agent_mail` server using
//! the legacy Python launcher (`python -m mcp_agent_mail` /
//! `uvx mcp-agent-mail` / similar) instead of the current Rust
//! binary at `~/.local/bin/mcp-agent-mail` (or an HTTP URL
//! pointing at `am serve-http`).
//!
//! Concrete failure modes when the Python launcher is left in
//! place:
//! - The Python `mcp_agent_mail` package may not even be
//!   installed, so the client boot fails with a confusing
//!   "command not found" or `ModuleNotFoundError`.
//! - If both binaries are installed, they could be invoked
//!   against the same `storage.sqlite3` and corrupt it (this
//!   is exactly the parent issue of
//!   `fm-db-state-files-python-server-coresident-write`).
//! - Stale launchers also bring stale config schemas — old TEXT
//!   timestamp writes (see
//!   `fm-db-state-files-text-timestamp-contamination`).
//!
//! ## Detection (pure function)
//!
//! Iterate `McpConfigLocation`s with `exists == true`. For each,
//! read the file and call the CLI's existing
//! `classify_mcp_agent_mail_config` helper, which already knows
//! how to parse both TOML and JSON forms and recognize the
//! `Python` shape (any of: `command` starting with `python`,
//! `uvx`/`uv tool`/`pipx` invocations, or `args[0]` referencing
//! the Python module). If the classifier returns
//! `McpAgentMailEntryKind::Python`, emit a finding.
//!
//! ## Fix
//!
//! **Detect-only.** A safe rewrite to the Rust binary path needs
//! byte-exact TOML/JSON preservation across multiple client
//! schemas (Claude JSON, Codex TOML, Cursor JSON5, Gemini
//! JSON, ...). That surface is too broad for this commit; the
//! manual_remediation envelope lists the exact file + tool slug
//! and points operators at the existing
//! `am doctor install-precommit-guard` / `am robot status`
//! flows to manually rewrite via their preferred editor.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use crate::{McpAgentMailEntryKind, classify_mcp_agent_mail_config};
use mcp_agent_mail_core::mcp_config::{McpConfigLocation, McpConfigTool};
use serde::Serialize;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-mcp-config-files-stale-python-launcher-entry";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "mcp_config_files";

#[derive(Debug, Clone, Serialize)]
pub struct StalePythonLauncherEntry {
    pub config_path: PathBuf,
    pub tool: String,
    /// `pub(crate)` because `McpAgentMailEntryKind` itself is
    /// crate-private. External callers consume the finding's
    /// JSON evidence shape via `to_finding()`.
    pub(crate) entry_kind: McpAgentMailEntryKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct StalePythonLauncherEntryFinding {
    pub entries: Vec<StalePythonLauncherEntry>,
}

impl StalePythonLauncherEntryFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n = self.entries.len();
        let title = format!(
            "{} MCP client config{} still uses the Python launcher for mcp_agent_mail (rust binary is canonical)",
            n,
            if n == 1 { "" } else { "s" },
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "stale_entries": self.entries,
                "canonical_rust_binary_hint": "~/.local/bin/mcp-agent-mail",
                "alternative_http_url_hint": "http://127.0.0.1:8765/mcp/",
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only: byte-exact JSON/TOML rewrite is a
                // separate larger surface.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        let mut lines = vec![
            "Replace the Python launcher in each MCP client config below with either \
             the canonical Rust binary at `~/.local/bin/mcp-agent-mail` (stdio) \
             OR an HTTP URL pointing at a running `am serve-http`.\n"
                .to_string(),
        ];
        for e in &self.entries {
            lines.push(format!(
                "  • {} (tool={}, current kind={:?})",
                e.config_path.display(),
                e.tool,
                e.entry_kind
            ));
        }
        lines.push(
            "\nAuto-fix is detect-only because byte-exact rewriting across multiple \
             client config schemas (Claude JSON, Codex TOML, Cursor JSON5, etc.) is \
             a larger surface; we'd rather operators audit each rewrite manually."
                .to_string(),
        );
        lines.join("\n")
    }
}

/// Detector inputs.
#[derive(Debug, Clone)]
pub struct DetectInputs {
    pub locations: Vec<McpConfigLocation>,
    /// Path to the rust binary (passed to the classifier so it can
    /// recognize `command = "<rust-binary>"` as `Rust` and avoid
    /// false-positiving on a correctly-configured entry whose
    /// `command` happens to contain the substring "python" in its
    /// path).
    pub rust_binary_path: PathBuf,
}

/// Detector. PURE w.r.t. inputs.
pub fn detect(inputs: &DetectInputs) -> Vec<StalePythonLauncherEntryFinding> {
    let mut entries = Vec::new();
    for loc in &inputs.locations {
        if !loc.exists {
            continue;
        }
        let contents = match std::fs::read_to_string(&loc.config_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let kind = match classify_mcp_agent_mail_config(
            &loc.config_path,
            &contents,
            &inputs.rust_binary_path,
        ) {
            Some(k) => k,
            None => continue, // entry absent — sibling FM owns "no entry at all"
        };
        if kind == McpAgentMailEntryKind::Python {
            entries.push(StalePythonLauncherEntry {
                config_path: loc.config_path.clone(),
                tool: tool_slug_for(&loc.tool).to_string(),
                entry_kind: kind,
            });
        }
    }
    if entries.is_empty() {
        Vec::new()
    } else {
        vec![StalePythonLauncherEntryFinding { entries }]
    }
}

fn tool_slug_for(tool: &McpConfigTool) -> &'static str {
    tool.slug()
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &StalePythonLauncherEntryFinding,
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

    fn loc(path: PathBuf, tool: McpConfigTool) -> McpConfigLocation {
        McpConfigLocation {
            tool,
            config_path: path,
            exists: true,
        }
    }

    fn rust_binary() -> PathBuf {
        PathBuf::from("/home/op/.local/bin/mcp-agent-mail")
    }

    #[test]
    fn detector_returns_empty_when_no_locations() {
        let inputs = DetectInputs {
            locations: Vec::new(),
            rust_binary_path: rust_binary(),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_flags_python_launcher_in_codex_toml() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers.mcp_agent_mail]
command = "python"
args = ["-m", "mcp_agent_mail"]
"#,
        )
        .unwrap();
        let inputs = DetectInputs {
            locations: vec![loc(p, McpConfigTool::Codex)],
            rust_binary_path: rust_binary(),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 1);
        assert_eq!(
            findings[0].entries[0].entry_kind,
            McpAgentMailEntryKind::Python
        );
        assert_eq!(findings[0].entries[0].tool, "codex");
    }

    #[test]
    fn detector_does_not_flag_rust_binary() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers.mcp_agent_mail]
command = "/home/op/.local/bin/mcp-agent-mail"
"#,
        )
        .unwrap();
        let inputs = DetectInputs {
            locations: vec![loc(p, McpConfigTool::Codex)],
            rust_binary_path: rust_binary(),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_skips_nonexistent_locations() {
        let td = TempDir::new().unwrap();
        let mut l = loc(td.path().join("nope.toml"), McpConfigTool::Codex);
        l.exists = false;
        let inputs = DetectInputs {
            locations: vec![l],
            rust_binary_path: rust_binary(),
        };
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn detector_aggregates_multiple_locations_into_one_finding() {
        let td = TempDir::new().unwrap();
        let p1 = td.path().join("codex.toml");
        let p2 = td.path().join("claude.json");
        fs::write(
            &p1,
            r#"
[mcp_servers.mcp_agent_mail]
command = "python"
args = ["-m", "mcp_agent_mail"]
"#,
        )
        .unwrap();
        fs::write(
            &p2,
            r#"{"mcpServers": {"mcp_agent_mail": {"command": "python", "args": ["-m", "mcp_agent_mail"]}}}"#,
        )
        .unwrap();
        let inputs = DetectInputs {
            locations: vec![
                loc(p1, McpConfigTool::Codex),
                loc(p2, McpConfigTool::Claude),
            ],
            rust_binary_path: rust_binary(),
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1, "one aggregated finding");
        assert_eq!(findings[0].entries.len(), 2);
    }

    #[test]
    fn finding_severity_is_p0_detect_only() {
        let f = StalePythonLauncherEntryFinding {
            entries: vec![StalePythonLauncherEntry {
                config_path: PathBuf::from("/x/config.toml"),
                tool: "codex".to_string(),
                entry_kind: McpAgentMailEntryKind::Python,
            }],
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!(!g.remediation.auto_fixable);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("canonical_rust_binary_hint"));
    }

    #[test]
    fn manual_remediation_lists_each_stale_entry() {
        let f = StalePythonLauncherEntryFinding {
            entries: vec![
                StalePythonLauncherEntry {
                    config_path: PathBuf::from("/x/codex.toml"),
                    tool: "codex".to_string(),
                    entry_kind: McpAgentMailEntryKind::Python,
                },
                StalePythonLauncherEntry {
                    config_path: PathBuf::from("/y/claude.json"),
                    tool: "claude".to_string(),
                    entry_kind: McpAgentMailEntryKind::Python,
                },
            ],
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("/x/codex.toml"));
        assert!(text.contains("/y/claude.json"));
        assert!(text.contains("~/.local/bin/mcp-agent-mail"));
    }
}
