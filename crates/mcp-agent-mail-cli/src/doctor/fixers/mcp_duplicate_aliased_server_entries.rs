//! `fm-mcp-config-files-duplicate-aliased-server-entries` — P2
//! detect-only.
//!
//! **Subsystem**: mcp_config_files.
//!
//! ## What's broken
//!
//! Many MCP client configs accept the agent-mail server under
//! ANY combination of a "container key" and an "alias key":
//!
//! - **Container keys** (where the server map lives in the JSON):
//!   `mcpServers`, `servers`, `mcp`, `mcp_servers`.
//! - **Alias keys** (the per-server entry name):
//!   `mcp-agent-mail`, `mcp_agent_mail`.
//!
//! That's 8 (container, alias) combinations. Most users have
//! exactly ONE — but a config file that's been hand-edited or
//! migrated across MCP-client versions can end up with TWO OR
//! MORE entries pointing at this server.
//!
//! Duplicates are a P2 because:
//!
//! - The MCP client typically picks the first-match (or sometimes
//!   the last-match), so users see ONE entry working and the
//!   others silently dead — they update the wrong one and wonder
//!   why nothing changes.
//! - Two entries with different URLs / tokens silently disagree
//!   about which to talk to.
//! - `am setup --rotate-token` may rewrite one entry and miss the
//!   others, leaving stale tokens on disk.
//!
//! Distinct from:
//!
//! - `stale_bearer_token_skew`: a single entry has a stale token.
//! - `wrong_mcp_url_json`: a single entry has a wrong URL.
//! - `stale_python_launcher_entry`: a single entry uses the
//!   Python launcher instead of the Rust binary.
//!
//! ## Detection (pure)
//!
//! For each config file in `mcp_config_candidates` (existing
//! DispatchInputs field):
//!
//! 1. Read the file. If unreadable, skip.
//! 2. Detect file format from the extension: `.json` /
//!    `.jsonc` / `.json5` → JSON or JSON5-compatible input;
//!    anything else → skip (TOML support is a
//!    follow-up — first cut limits to JSON / JSON5 because
//!    that's the vast majority of MCP client configs and the
//!    most-common duplicate-aliased shape).
//! 3. Parse JSON. If unparseable, skip — a different FM owns
//!    malformed configs.
//! 4. For each of `SERVER_CONTAINERS × TARGET_ALIASES` (8 pairs),
//!    check whether `doc[container][alias]` is an object (the
//!    typical server-entry shape). Record every hit as a
//!    `(container, alias)` registration.
//! 5. If > 1 registration in the same file, emit a per-file
//!    record. The set of distinct registrations is part of the
//!    finding evidence so operators triaging via
//!    `am doctor explain` can see which entries duplicate.
//!
//! ## Fix
//!
//! **Detect-only (first cut).** The repair_spec calls for a
//! "coalesce to canonical" rewrite (`mcpServers.mcp-agent-mail`
//! is the winner; drop the others; preserve URL/token from the
//! winner). That's substantial JSON-rewrite work with serde and
//! requires careful preservation of comments, formatting, and
//! sibling keys — plus a per-config round-trip test fixture.
//! Deferred.
//!
//! Manual remediation: hand-edit each config to leave exactly
//! one entry under `(mcpServers, mcp-agent-mail)`; or re-run
//! `am setup` which writes a fresh canonical entry but doesn't
//! delete the others.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-mcp-config-files-duplicate-aliased-server-entries";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// Container keys we recognize as housing the per-server map.
/// Source of truth: `mcp_agent_mail_core::mcp_config::SERVER_CONTAINER_KEYS`
/// (private). Keep in sync with the canonical install path.
const SERVER_CONTAINERS: &[&str] = &["mcpServers", "servers", "mcp", "mcp_servers"];

/// Alias keys we recognize as the agent-mail server entry.
/// Source of truth: `mcp_agent_mail_core::mcp_config::TARGET_SERVER_ALIASES`
/// (private).
const TARGET_ALIASES: &[&str] = &["mcp-agent-mail", "mcp_agent_mail"];

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateConfigEntry {
    pub config_path: PathBuf,
    /// All `(container, alias)` pairs the detector found in this
    /// config. By construction the list has ≥ 2 elements (single
    /// hits are healthy and not flagged).
    pub registrations: Vec<RegistrationPair>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistrationPair {
    pub container: String,
    pub alias: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpDuplicateAliasedServerEntriesFinding {
    pub entries: Vec<DuplicateConfigEntry>,
}

impl McpDuplicateAliasedServerEntriesFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} MCP client config file(s) have MORE than one agent-mail server registration across `(container, alias)` pairs",
            self.entries.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "entries": self.entries,
                "containers_scanned": SERVER_CONTAINERS,
                "aliases_scanned": TARGET_ALIASES,
                "canonical_pair": ["mcpServers", "mcp-agent-mail"],
                "manual_remediation": {
                    "steps": [
                        "For each config file, hand-edit to leave exactly one entry under `mcpServers.mcp-agent-mail` (the canonical pair). Preserve the URL, bearer token, env block, and args from the entry you keep.",
                        "Quick alternative: re-run `am setup` to (re)write a fresh canonical entry. NOTE: `am setup` writes the canonical entry but does NOT remove the duplicates — after re-running it, you still need to hand-delete the non-canonical entries from the config.",
                        "If you don't know which is the live entry (the MCP client picks one and the others are silent): start `am serve-http --no-tui` in a terminal, then ask the MCP client to call any agent-mail tool — only the live registration's URL will reach the server.",
                        "After hand-editing, restart your MCP client (or the editor / agent process that owns it) so the updated config is reloaded.",
                        "Re-run `am doctor fix --only fm-mcp-config-files-duplicate-aliased-server-entries --list` to confirm a single registration per file.",
                    ],
                    "warning": "Duplicate registrations can silently disagree on URL or bearer token — the MCP client picks one (typically first-match) and the rest become dead. Token rotations and URL edits applied to the wrong entry have no effect.",
                    "safe_fix_deferred": "Auto-fix via Op::WriteFile of a coalesced config is intentionally deferred in this first cut. Faithful JSON rewriting (preserving comments / formatting / sibling keys) requires per-format work and a per-config round-trip test; the chokepoint already supports Op::WriteFile but the rewrite logic doesn't ship yet.",
                    "first_cut_scope": "This detector currently inspects JSON / JSON5 configs only. TOML configs (e.g. Codex `config.toml`) are NOT yet scanned — a follow-up FM adds TOML parsing. If a Codex config has duplicate `[mcp_servers.mcp-agent-mail]` and `[mcp_servers.mcp_agent_mail]` blocks, this FM will not flag it.",
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

/// Detector. PURE w.r.t. the supplied list of candidate config
/// paths.
///
/// Returns at most one aggregated finding per call. Returns
/// empty when every candidate has ≤ 1 registration or is
/// unreadable / unparseable / not a JSON-shaped config.
pub fn detect(candidate_configs: &[PathBuf]) -> Vec<McpDuplicateAliasedServerEntriesFinding> {
    let mut entries: Vec<DuplicateConfigEntry> = Vec::new();
    for path in candidate_configs {
        let Some(entry) = inspect_one(path) else {
            continue;
        };
        entries.push(entry);
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![McpDuplicateAliasedServerEntriesFinding { entries }]
}

fn inspect_one(path: &Path) -> Option<DuplicateConfigEntry> {
    if !is_json_extension(path) {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    let doc = parse_json_or_json5(&body)?;
    let registrations = enumerate_registrations(&doc);
    if registrations.len() < 2 {
        return None;
    }
    Some(DuplicateConfigEntry {
        config_path: path.to_path_buf(),
        registrations,
    })
}

fn is_json_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|s| {
        let s = s.to_ascii_lowercase();
        s == "json" || s == "jsonc" || s == "json5"
    })
}

fn parse_json_or_json5(body: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .or_else(|| json5::from_str::<serde_json::Value>(body).ok())
}

fn enumerate_registrations(doc: &serde_json::Value) -> Vec<RegistrationPair> {
    let mut out: Vec<RegistrationPair> = Vec::new();
    for container in SERVER_CONTAINERS {
        let Some(servers) = doc.get(container).and_then(|v| v.as_object()) else {
            continue;
        };
        for alias in TARGET_ALIASES {
            if servers
                .get(*alias)
                .is_some_and(serde_json::Value::is_object)
            {
                out.push(RegistrationPair {
                    container: (*container).to_string(),
                    alias: (*alias).to_string(),
                });
            }
        }
    }
    out
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &McpDuplicateAliasedServerEntriesFinding,
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

    fn write_config(td: &TempDir, name: &str, body: &str) -> PathBuf {
        let p = td.path().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    /// **NEGATIVE TEST FIRST**: empty input → no finding.
    #[test]
    fn detector_returns_empty_for_no_candidates() {
        assert!(detect(&[]).is_empty());
    }

    /// **NEGATIVE**: missing files → silently skipped.
    #[test]
    fn detector_skips_missing_file() {
        let td = TempDir::new().unwrap();
        let findings = detect(&[td.path().join("nope.json")]);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: non-JSON extension (`.toml`) → skipped
    /// (first-cut scope). TOML support is a follow-up FM.
    #[test]
    fn detector_skips_toml_extension_in_first_cut() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "config.toml",
            "[mcp_servers.mcp-agent-mail]\nurl = \"x\"\n[mcp_servers.mcp_agent_mail]\nurl = \"y\"\n",
        );
        assert!(detect(&[path]).is_empty());
    }

    /// **NEGATIVE**: malformed JSON → skipped silently.
    #[test]
    fn detector_skips_malformed_json() {
        let td = TempDir::new().unwrap();
        let path = write_config(&td, "broken.json", "not json at all{");
        assert!(detect(&[path]).is_empty());
    }

    /// **NEGATIVE**: a healthy single-entry config → no finding.
    #[test]
    fn detector_returns_empty_for_single_canonical_entry() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "single.json",
            r#"{"mcpServers":{"mcp-agent-mail":{"url":"http://127.0.0.1:8765/mcp/"}}}"#,
        );
        let findings = detect(&[path]);
        assert!(findings.is_empty());
    }

    /// **NEGATIVE**: a config with no agent-mail entries at all → no finding.
    /// (Different FM would handle "install missing".)
    #[test]
    fn detector_returns_empty_for_config_without_any_agent_mail_entries() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "other.json",
            r#"{"mcpServers":{"some-other-server":{"url":"x"}}}"#,
        );
        assert!(detect(&[path]).is_empty());
    }

    #[test]
    fn detector_flags_canonical_plus_alt_container_duplicate() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "dup.json",
            r#"{
              "mcpServers": {"mcp-agent-mail": {"url": "http://127.0.0.1:8765/mcp/"}},
              "servers":    {"mcp-agent-mail": {"url": "http://127.0.0.1:8766/mcp/"}}
            }"#,
        );
        let findings = detect(&[path]);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].registrations.len(), 2);
        let containers: std::collections::HashSet<String> = f.entries[0]
            .registrations
            .iter()
            .map(|r| r.container.clone())
            .collect();
        assert!(containers.contains("mcpServers"));
        assert!(containers.contains("servers"));
    }

    #[test]
    fn detector_flags_alias_dash_vs_underscore_duplicate() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "alias-dup.json",
            r#"{"mcpServers":{
              "mcp-agent-mail": {"url": "http://127.0.0.1:8765/mcp/"},
              "mcp_agent_mail": {"url": "http://127.0.0.1:8765/mcp/"}
            }}"#,
        );
        let findings = detect(&[path]);
        assert_eq!(findings.len(), 1);
        let aliases: std::collections::HashSet<String> = findings[0].entries[0]
            .registrations
            .iter()
            .map(|r| r.alias.clone())
            .collect();
        assert!(aliases.contains("mcp-agent-mail"));
        assert!(aliases.contains("mcp_agent_mail"));
    }

    #[test]
    fn detector_parses_jsonc_comments_and_trailing_commas() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "cursor.jsonc",
            r#"{
              // Real user configs frequently contain comments.
              "mcpServers": {"mcp-agent-mail": {"url": "http://127.0.0.1:8765/mcp/"}},
              "servers": {"mcp_agent_mail": {"url": "http://127.0.0.1:8766/mcp/"}},
            }"#,
        );
        let findings = detect(&[path]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].registrations.len(), 2);
    }

    #[test]
    fn detector_flags_three_way_duplicate() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "triple.json",
            r#"{
              "mcpServers": {"mcp-agent-mail": {"url":"a"}},
              "servers":    {"mcp_agent_mail": {"url":"b"}},
              "mcp":        {"mcp-agent-mail": {"url":"c"}}
            }"#,
        );
        let findings = detect(&[path]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].registrations.len(), 3);
    }

    /// Pin that entries which AREN'T objects (e.g., `null`,
    /// boolean stub, string) don't count as registrations. This
    /// prevents false-positives when a key is present but the
    /// value isn't a real server entry.
    #[test]
    fn detector_ignores_non_object_entries() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "stub.json",
            r#"{
              "mcpServers": {"mcp-agent-mail": {"url":"a"}},
              "servers": {"mcp-agent-mail": null},
              "mcp": {"mcp-agent-mail": "disabled"}
            }"#,
        );
        // Only the mcpServers entry counts; the other two are
        // stubs / nulls — must NOT flag as duplicate.
        assert!(detect(&[path]).is_empty());
    }

    #[test]
    fn detector_aggregates_multiple_configs_into_one_finding() {
        let td = TempDir::new().unwrap();
        let p1 = write_config(
            &td,
            "cursor.json",
            r#"{"mcpServers":{"mcp-agent-mail":{},"mcp_agent_mail":{}}}"#,
        );
        let p2 = write_config(
            &td,
            "claude.json",
            r#"{
              "mcpServers": {"mcp-agent-mail": {}},
              "servers":    {"mcp-agent-mail": {}}
            }"#,
        );
        let findings = detect(&[p1, p2]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 2);
    }

    #[test]
    fn is_json_extension_matches_json_jsonc_json5() {
        assert!(is_json_extension(Path::new("/x/cursor.json")));
        assert!(is_json_extension(Path::new("/x/vscode.jsonc")));
        assert!(is_json_extension(Path::new("/x/zed.json5")));
        // Case-insensitive
        assert!(is_json_extension(Path::new("/x/X.JSON")));
        // Negative
        assert!(!is_json_extension(Path::new("/x/codex.toml")));
        assert!(!is_json_extension(Path::new("/x/no_ext")));
    }

    #[test]
    fn finding_serializes_with_canonical_pair_and_remediation() {
        let f = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: "/tmp/cursor.json".into(),
                registrations: vec![
                    RegistrationPair {
                        container: "mcpServers".to_string(),
                        alias: "mcp-agent-mail".to_string(),
                    },
                    RegistrationPair {
                        container: "servers".to_string(),
                        alias: "mcp_agent_mail".to_string(),
                    },
                ],
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"canonical_pair\":[\"mcpServers\",\"mcp-agent-mail\"]"));
        assert!(s.contains("first_cut_scope"));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("\"auto_fixable\":false"));
        assert!(s.contains("am setup"));
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
        let finding = McpDuplicateAliasedServerEntriesFinding {
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
