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
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use crate::{McpAgentMailEntryKind, classify_mcp_agent_mail_config};
use crate::doctor::platform;
use mcp_agent_mail_core::mcp_config::{McpConfigLocation, McpConfigTool};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-mcp-config-files-stale-python-launcher-entry";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// JSON container keys that may hold the per-server map. Mirrors
/// `lib.rs::find_mcp_agent_mail_entry`'s `SERVER_CONTAINER_KEYS`.
const SERVER_CONTAINER_KEYS: &[&str] = &["mcpServers", "servers", "mcp", "mcp_servers"];

/// Per-server alias keys for the agent-mail entry (both JSON and
/// the TOML quoted/snake forms). Mirrors the classifier.
const AGENT_MAIL_ALIAS_KEYS: &[&str] = &["mcp-agent-mail", "mcp_agent_mail", "agent-mail"];

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
    /// The validated Rust binary path the detector resolved. The
    /// auto-fix writes this as the entry's `command`, preserving
    /// the operator's stdio transport choice (a Python *launcher*
    /// is always a command/stdio entry — never an HTTP url — so
    /// swapping the command to the Rust binary keeps the transport
    /// the operator chose).
    pub rust_binary_path: PathBuf,
}

/// File extensions the auto-fix can rewrite without data loss.
/// `.json` (strict serde_json) + `.toml` (format-preserving
/// toml_edit). `.jsonc` / `.json5` are intentionally excluded —
/// a serde_json round-trip would strip their comments.
fn is_fixable_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("json") | Some("toml")
    )
}

/// Count entries whose config the auto-fix can rewrite.
fn count_fixable(finding: &StalePythonLauncherEntryFinding) -> usize {
    finding
        .entries
        .iter()
        .filter(|e| is_fixable_extension(&e.config_path))
        .count()
}

impl StalePythonLauncherEntryFinding {
    pub fn to_finding(&self) -> super::Finding {
        let n = self.entries.len();
        let fixable = count_fixable(self);
        let unfixable = n - fixable;
        let title = format!(
            "{} MCP client config{} still uses the Python launcher for mcp_agent_mail (rust binary is canonical; {} auto-fixable, {} need manual edit)",
            n,
            if n == 1 { "" } else { "s" },
            fixable,
            unfixable,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "stale_entries": self.entries,
                "canonical_rust_binary": self.rust_binary_path.to_string_lossy(),
                "alternative_http_url_hint": "http://127.0.0.1:8765/mcp/",
                "fixable_count": fixable,
                "unfixable_count": unfixable,
                "auto_fix_summary": format!(
                    "`am doctor fix --only {FM_ID} --yes` rewrites {fixable} `.json`/`.toml` config(s): the Python launcher's `command` is swapped to the canonical Rust binary and its `args` cleared, PRESERVING the stdio transport + all sibling keys (env, etc.). The remaining {unfixable} (`.jsonc`/`.json5` or absent-entry) stay manual. Reversible via `am doctor undo <run-id>`."
                ),
            }),
            remediation: FindingRemediation {
                command: if fixable > 0 {
                    format!("am doctor fix --only {FM_ID}")
                } else {
                    format!("am doctor explain {FM_ID}")
                },
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: fixable > 0,
                estimated_actions: fixable,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        let mut lines = vec![format!(
            "Auto-fix (preferred for `.json`/`.toml` configs): \
             `am doctor fix --only {FM_ID} --yes` swaps the Python launcher's \
             `command` to the canonical Rust binary ({}) and clears its `args`, \
             preserving the stdio transport + all sibling keys, reversible via \
             `am doctor undo`. For `.jsonc`/`.json5` configs (comment-bearing), \
             edit manually: replace the Python launcher with the Rust binary (stdio) \
             OR an HTTP URL pointing at a running `am serve-http`.\n",
            self.rust_binary_path.display(),
        )];
        for e in &self.entries {
            lines.push(format!(
                "  • {} (tool={}, current kind={:?})",
                e.config_path.display(),
                e.tool,
                e.entry_kind
            ));
        }
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
        vec![StalePythonLauncherEntryFinding {
            entries,
            rust_binary_path: inputs.rust_binary_path.clone(),
        }]
    }
}

fn tool_slug_for(tool: &McpConfigTool) -> &'static str {
    tool.slug()
}

/// Rewrite a strict-JSON MCP config: find the agent-mail server
/// entry, swap its `command` to `rust_binary` and clear `args`,
/// preserving every other key (env, headers, etc.). Returns the
/// new file content, or `None` if there's nothing safe to rewrite
/// (entry absent, not an object, or already pointing at a non-
/// command/stdio shape).
fn rewrite_json_launcher(content: &str, rust_binary: &str) -> Option<String> {
    let mut doc: serde_json::Value = serde_json::from_str(content).ok()?;
    let root = doc.as_object_mut()?;
    // Find the (container, alias) that holds the entry.
    let mut found: Option<(String, String)> = None;
    for ck in SERVER_CONTAINER_KEYS {
        let Some(container) = root.get(*ck).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for ak in AGENT_MAIL_ALIAS_KEYS {
            if container.contains_key(*ak) {
                found = Some(((*ck).to_string(), (*ak).to_string()));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let (ck, ak) = found?;
    let entry = root
        .get_mut(&ck)?
        .as_object_mut()?
        .get_mut(&ak)?
        .as_object_mut()?;
    // Only rewrite a command/stdio launcher. If the entry has a
    // `url`/`httpUrl` (HTTP transport) we must NOT clobber it —
    // that's a different transport the operator chose, and the
    // detector wouldn't classify it as Python anyway. Defensive.
    if entry.contains_key("url") || entry.contains_key("httpUrl") {
        return None;
    }
    entry.insert(
        "command".to_string(),
        serde_json::Value::String(rust_binary.to_string()),
    );
    entry.insert("args".to_string(), serde_json::Value::Array(Vec::new()));
    serde_json::to_string_pretty(&doc).ok()
}

/// Rewrite a TOML MCP config (Codex) via format-preserving
/// toml_edit: find `mcp_servers.{mcp_agent_mail | "mcp-agent-mail"}`,
/// swap `command` to `rust_binary` and clear `args`. Returns the
/// new content, or `None` if there's nothing safe to rewrite.
fn rewrite_toml_launcher(content: &str, rust_binary: &str) -> Option<String> {
    let mut doc = content.parse::<toml_edit::DocumentMut>().ok()?;
    let servers = doc.get_mut("mcp_servers")?.as_table_like_mut()?;
    let key = AGENT_MAIL_ALIAS_KEYS
        .iter()
        .find(|k| servers.contains_key(k))
        .copied()?;
    let entry = servers.get_mut(key)?.as_table_like_mut()?;
    // Don't clobber an HTTP-url entry (different transport).
    if entry.contains_key("url") || entry.contains_key("httpUrl") {
        return None;
    }
    entry.insert("command", toml_edit::value(rust_binary));
    entry.insert("args", toml_edit::value(toml_edit::Array::new()));
    Some(doc.to_string())
}

/// Fixer. For each `.json`/`.toml` entry, surgically swap the
/// Python launcher to the canonical Rust stdio command via
/// `Op::WriteFile`, preserving the operator's transport + sibling
/// keys. `.jsonc`/`.json5` and entry-absent configs are skipped.
pub fn fix(
    ctx: &MutateContext,
    finding: &StalePythonLauncherEntryFinding,
) -> Result<FixOutcome, MutateError> {
    let rust_binary = finding.rust_binary_path.to_string_lossy().into_owned();
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    for entry in &finding.entries {
        let path = &entry.config_path;
        if !is_fixable_extension(path) {
            actions_skipped += 1;
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        let ext = path.extension().and_then(|e| e.to_str());
        let new_content = match ext {
            Some("toml") => rewrite_toml_launcher(&content, &rust_binary),
            Some("json") => rewrite_json_launcher(&content, &rust_binary),
            _ => None,
        };
        let Some(new_content) = new_content else {
            // Entry absent, malformed, or HTTP-url shape we won't
            // clobber — nothing safe to do here.
            actions_skipped += 1;
            continue;
        };
        let mode = std::fs::symlink_metadata(path)
            .ok()
            .map(|m| platform::permission_mode(&m))
            .unwrap_or(0o644);
        mutate(
            ctx,
            path,
            Op::WriteFile {
                content: new_content.into_bytes(),
                mode,
            },
        )?;
        actions_taken += 1;
    }
    Ok(FixOutcome {
        actions_taken,
        actions_skipped,
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
    fn finding_severity_is_p0_auto_fixable_for_toml() {
        let f = StalePythonLauncherEntryFinding {
            entries: vec![StalePythonLauncherEntry {
                config_path: PathBuf::from("/x/config.toml"),
                tool: "codex".to_string(),
                entry_kind: McpAgentMailEntryKind::Python,
            }],
            rust_binary_path: rust_binary(),
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 1);
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("canonical_rust_binary"));
        assert!(s.contains("\"fixable_count\":1"));
    }

    #[test]
    fn finding_unfixable_for_jsonc_only() {
        // A .jsonc config can't be auto-fixed (comment loss).
        let f = StalePythonLauncherEntryFinding {
            entries: vec![StalePythonLauncherEntry {
                config_path: PathBuf::from("/x/mcp.jsonc"),
                tool: "cursor".to_string(),
                entry_kind: McpAgentMailEntryKind::Python,
            }],
            rust_binary_path: rust_binary(),
        };
        let g = f.to_finding();
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
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
            rust_binary_path: rust_binary(),
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("/x/codex.toml"));
        assert!(text.contains("/y/claude.json"));
        assert!(text.contains(&rust_binary().display().to_string()));
    }

    // ---- fix() unit tests ----

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.into(),
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
        }
    }

    fn finding_for(path: PathBuf, tool: &str) -> StalePythonLauncherEntryFinding {
        StalePythonLauncherEntryFinding {
            entries: vec![StalePythonLauncherEntry {
                config_path: path,
                tool: tool.to_string(),
                entry_kind: McpAgentMailEntryKind::Python,
            }],
            rust_binary_path: rust_binary(),
        }
    }

    /// **NEGATIVE**: a `.jsonc` config is skipped (comment loss).
    #[test]
    fn fixer_skips_jsonc_extension() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.jsonc");
        let body = "{\n  // operator comment\n  \"mcpServers\": { \"mcp-agent-mail\": { \"command\": \"uvx\", \"args\": [\"mcp-agent-mail\", \"run_server\"] } }\n}\n";
        fs::write(&p, body).unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__pylaunch_jsonc");
        let outcome = fix(&ctx, &finding_for(p.clone(), "cursor")).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert_eq!(fs::read_to_string(&p).unwrap(), body, "jsonc untouched");
    }

    /// **NEGATIVE**: entry absent → skip (never create an entry).
    #[test]
    fn fixer_skips_when_entry_absent_json() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("claude.json");
        fs::write(&p, r#"{"mcpServers":{"some-other":{"command":"x"}}}"#).unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__pylaunch_absent");
        let outcome = fix(&ctx, &finding_for(p.clone(), "claude")).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// **NEGATIVE**: an HTTP-url entry is never clobbered.
    #[test]
    fn fixer_skips_http_url_entry_json() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("claude.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"mcp-agent-mail":{"url":"http://127.0.0.1:8765/mcp/"}}}"#,
        )
        .unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__pylaunch_http");
        let outcome = fix(&ctx, &finding_for(p.clone(), "claude")).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert!(fs::read_to_string(&p).unwrap().contains("http://127.0.0.1:8765/mcp/"));
    }

    /// Positive (JSON): a uvx Python launcher is swapped to the
    /// canonical Rust command, args cleared, sibling `env` preserved.
    #[test]
    fn fixer_rewrites_json_python_launcher_preserving_env() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("claude.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"mcp-agent-mail":{"command":"uvx","args":["mcp-agent-mail","run_server"],"env":{"FOO":"bar"}}}}"#,
        )
        .unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__pylaunch_json_fix");
        let outcome = fix(&ctx, &finding_for(p.clone(), "claude")).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let post: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            post.pointer("/mcpServers/mcp-agent-mail/command")
                .and_then(|v| v.as_str()),
            Some(rust_binary().to_string_lossy().as_ref())
        );
        assert_eq!(
            post.pointer("/mcpServers/mcp-agent-mail/args")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0),
            "python args cleared"
        );
        // Sibling env preserved.
        assert_eq!(
            post.pointer("/mcpServers/mcp-agent-mail/env/FOO")
                .and_then(|v| v.as_str()),
            Some("bar")
        );
        // Classifier now sees a Rust entry → detector would clear.
        let kind = classify_mcp_agent_mail_config(
            &p,
            &fs::read_to_string(&p).unwrap(),
            &rust_binary(),
        );
        assert_eq!(kind, Some(McpAgentMailEntryKind::Rust));
    }

    /// Positive (TOML): a python -m launcher is swapped to the Rust
    /// command, preserving a comment + sibling key.
    #[test]
    fn fixer_rewrites_toml_python_launcher_preserving_format() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        let original = "# codex config\n\
                        [mcp_servers.mcp_agent_mail]\n\
                        command = \"python\"  # legacy launcher\n\
                        args = [\"-m\", \"mcp_agent_mail\", \"run_server\"]\n\
                        startup_timeout_sec = 30\n";
        fs::write(&p, original).unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__pylaunch_toml_fix");
        let outcome = fix(&ctx, &finding_for(p.clone(), "codex")).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        let post = fs::read_to_string(&p).unwrap();
        assert!(post.contains(&format!("command = \"{}\"", rust_binary().display())));
        assert!(post.contains("args = []"));
        // Comment + sibling key preserved.
        assert!(post.contains("# codex config"));
        assert!(post.contains("startup_timeout_sec = 30"));
        // No longer a python launcher.
        let kind = classify_mcp_agent_mail_config(&p, &post, &rust_binary());
        assert_eq!(kind, Some(McpAgentMailEntryKind::Rust));
    }

    /// Round-trip: corrupt (python launcher) → fix → undo →
    /// byte-identical. The `entry_kind` field is `pub(crate)`, so
    /// this lives as a unit test (not in the integration
    /// round-trip suite). Proves the chokepoint reverses the
    /// transport-preserving TOML rewrite byte-for-byte (comment +
    /// sibling key restored).
    #[test]
    fn round_trip_toml_python_launcher() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        let original = "# codex config (operator comment)\n\
                        [mcp_servers.mcp_agent_mail]\n\
                        command = \"python\"  # legacy launcher\n\
                        args = [\"-m\", \"mcp_agent_mail\", \"run_server\"]\n\
                        startup_timeout_sec = 30\n";
        fs::write(&p, original).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();

        let run_id = "2026-05-20T00-00-00Z__pylaunch_rt";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &finding_for(p.clone(), "codex")).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        // Post-fix differs from original (python → rust).
        let post_fix = fs::read_to_string(&p).unwrap();
        assert_ne!(post_fix, original);
        assert!(post_fix.contains("args = []"));

        drop(ctx);

        let summary = crate::doctor::undo::run_undo_with_scopes(
            td.path(),
            run_id,
            false,
            true,
            &[td.path().to_path_buf()],
        )
        .expect("run_undo");
        assert!(summary.failures.is_empty(), "undo failures: {:?}", summary.failures);

        // Undo restores the python launcher bytes exactly.
        assert_eq!(fs::read_to_string(&p).unwrap(), original);
    }
}
