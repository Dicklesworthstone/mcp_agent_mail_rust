//! `fm-mcp-config-files-duplicate-aliased-server-entries` — P2
//! auto-fix via `Op::WriteFile`.
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
//!   `mcp-agent-mail`, `mcp_agent_mail`, `agent-mail`.
//!
//! That's 12 (container, alias) combinations. Most users have
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
//! 4. For each of `SERVER_CONTAINERS × TARGET_ALIASES` (12 pairs),
//!    check whether `doc[container][alias]` is an object (the
//!    typical server-entry shape). Record every hit as a
//!    `(container, alias)` registration.
//! 5. If > 1 registration in the same file, emit a per-file
//!    record. The set of distinct registrations is part of the
//!    finding evidence so operators triaging via
//!    `am doctor explain` can see which entries duplicate.
//!
//! ## Fix (partial auto-fix)
//!
//! **Auto-fix scope** is bounded by what we can rewrite without
//! data loss:
//!
//! - **`.json` configs where canonical `(mcpServers, mcp-agent-mail)`
//!   already exists**: rewritten via `Op::WriteFile` with each
//!   non-canonical `(container, alias)` entry removed. The
//!   canonical entry is preserved verbatim (URL, bearer token,
//!   env block, args — every key under it stays). Sibling
//!   servers within a container (e.g. `mcpServers.other-server`)
//!   and unrelated top-level keys are preserved. The chokepoint
//!   backs up the original bytes verbatim, so `am doctor undo
//!   <run-id>` restores the pre-fix state byte-identically.
//! - **`.jsonc` / `.json5` configs**: skipped. Auto-fix would
//!   strip comments (serde_json doesn't round-trip JSON5
//!   comments). Operator-supplied truth required.
//! - **Configs missing canonical `(mcpServers, mcp-agent-mail)`**:
//!   skipped. Auto-fix would have to PROMOTE one of the
//!   non-canonical entries, but the choice depends on which one
//!   the operator actually wants live. Operator must run `am
//!   setup` first (which writes a fresh canonical entry), then
//!   re-run the auto-fix to drop the duplicates.
//!
//! Byte fidelity: rewrites use `serde_json::to_string_pretty`
//! with no trailing newline. Mode mirrors the live file's mode
//! at fix-time. Atomic write via the chokepoint.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use crate::doctor::platform;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-mcp-config-files-duplicate-aliased-server-entries";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// Fallback mode when the live file's mode can't be read (e.g.
/// race between detect and fix). 0o644 is the typical config-file
/// mode — and the canonical entry on disk before fix() ran would
/// already have it. In practice this constant is unreachable
/// because vanished paths return early via `actions_skipped`.
const FALLBACK_CONFIG_MODE: u32 = 0o644;

const CANONICAL_CONTAINER: &str = "mcpServers";
const CANONICAL_ALIAS: &str = "mcp-agent-mail";

/// Container keys we recognize as housing the per-server map.
/// Source of truth: `mcp_agent_mail_core::mcp_config::SERVER_CONTAINER_KEYS`
/// (private). Keep in sync with the canonical install path.
const SERVER_CONTAINERS: &[&str] = &["mcpServers", "servers", "mcp", "mcp_servers"];

/// Alias keys we recognize as the agent-mail server entry.
/// Keep in sync with `JSON_MCP_AGENT_MAIL_ENTRY_KEYS` in the CLI
/// setup/doctor helpers.
const TARGET_ALIASES: &[&str] = &["mcp-agent-mail", "mcp_agent_mail", "agent-mail"];

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

/// Whether this entry is in-scope for auto-fix: file extension is
/// strict `.json` (no .jsonc/.json5 — comment loss risk) AND the
/// canonical `(mcpServers, mcp-agent-mail)` pair is present.
fn is_auto_fixable(entry: &DuplicateConfigEntry) -> bool {
    let strict_json = entry
        .config_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("json"));
    if !strict_json {
        return false;
    }
    entry
        .registrations
        .iter()
        .any(|r| r.container == CANONICAL_CONTAINER && r.alias == CANONICAL_ALIAS)
}

fn count_fixable(entries: &[DuplicateConfigEntry]) -> usize {
    entries.iter().filter(|e| is_auto_fixable(e)).count()
}

impl McpDuplicateAliasedServerEntriesFinding {
    pub fn to_finding(&self) -> super::Finding {
        let fixable = count_fixable(&self.entries);
        let unfixable = self.entries.len() - fixable;
        let title = format!(
            "{} MCP client config file(s) have MORE than one agent-mail server registration across `(container, alias)` pairs ({} auto-fixable, {} need operator input)",
            self.entries.len(),
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
                "entries": self.entries,
                "containers_scanned": SERVER_CONTAINERS,
                "aliases_scanned": TARGET_ALIASES,
                "canonical_pair": [CANONICAL_CONTAINER, CANONICAL_ALIAS],
                "fixable_count": fixable,
                "unfixable_count": unfixable,
                "auto_fix_summary": format!(
                    "`am doctor fix --only {FM_ID} --yes` rewrites {fixable} `.json` config file(s) where the canonical `(mcpServers, mcp-agent-mail)` pair is already present — non-canonical duplicates are removed; canonical entry preserved verbatim. The remaining {unfixable} (`.jsonc`/`.json5` configs OR configs without a canonical entry) stay in `actions_skipped` and need operator input. Reversible via `am doctor undo <run-id>` — the chokepoint backs up the original bytes verbatim."
                ),
                "manual_remediation": {
                    "steps": [
                        "Auto-fix (preferred for `.json` configs with canonical present): `am doctor fix --only fm-mcp-config-files-duplicate-aliased-server-entries --yes`. Drops every non-canonical `(container, alias)` entry; canonical `(mcpServers, mcp-agent-mail)` is preserved verbatim including URL/token/args.",
                        "For `.jsonc` / `.json5` configs: hand-edit to leave exactly one entry under `mcpServers.mcp-agent-mail`. Auto-fix is skipped to avoid stripping comments. Preserve the URL, bearer token, env block, and args from the entry you keep.",
                        "For configs without a canonical entry: re-run `am setup` to write a fresh canonical `(mcpServers, mcp-agent-mail)` entry, THEN re-run `am doctor fix --only fm-mcp-config-files-duplicate-aliased-server-entries --yes` to drop the duplicates.",
                        "If you don't know which is the live entry (the MCP client picks one and the others are silent): start `am serve-http --no-tui` in a terminal, then ask the MCP client to call any agent-mail tool — only the live registration's URL will reach the server.",
                        "After fixing, restart your MCP client (or the editor / agent process that owns it) so the updated config is reloaded.",
                        "Re-run `am doctor fix --only fm-mcp-config-files-duplicate-aliased-server-entries --list` to confirm a single registration per file.",
                    ],
                    "warning": "Duplicate registrations can silently disagree on URL or bearer token — the MCP client picks one (typically first-match) and the rest become dead. Token rotations and URL edits applied to the wrong entry have no effect.",
                    "first_cut_scope": "This detector currently inspects JSON / JSON5 configs only. TOML configs (e.g. Codex `config.toml`) are NOT yet scanned — a follow-up FM adds TOML parsing. If a Codex config has duplicate `[mcp_servers.mcp-agent-mail]` and `[mcp_servers.mcp_agent_mail]` blocks, this FM will not flag it.",
                },
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

/// Fixer. For each `is_auto_fixable` entry, route an `Op::WriteFile`
/// through the chokepoint with non-canonical `(container, alias)`
/// entries removed and the canonical entry + every sibling key
/// preserved verbatim.
///
/// Skip semantics:
/// - `.jsonc` / `.json5` configs → skip (comment loss).
/// - Configs without canonical `(mcpServers, mcp-agent-mail)` →
///   skip (auto-fix can't choose which non-canonical to promote;
///   operator must run `am setup` first).
/// - Vanished/unreadable config files → skip.
/// - Configs that re-parse with `serde_json::from_str` but the
///   live JSON no longer contains any non-canonical registration
///   (sibling agent / manual edit won the race between detect
///   and fix) → skip with no-op idempotence.
pub fn fix(
    ctx: &MutateContext,
    finding: &McpDuplicateAliasedServerEntriesFinding,
) -> Result<FixOutcome, MutateError> {
    let mut actions_taken = 0;
    let mut actions_skipped = 0;
    for entry in &finding.entries {
        if !is_auto_fixable(entry) {
            actions_skipped += 1;
            continue;
        }
        let body = match std::fs::read_to_string(&entry.config_path) {
            Ok(b) => b,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        // Strict JSON only (the `is_auto_fixable` gate restricts
        // to `.json` already, but defense-in-depth: don't fall
        // back to json5 here — we don't want to silently strip
        // comments if the gate is ever relaxed).
        let mut value: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        // Drop every non-canonical (container, alias) entry from
        // the parsed document.
        let mut removed_any = false;
        for reg in &entry.registrations {
            if reg.container == CANONICAL_CONTAINER && reg.alias == CANONICAL_ALIAS {
                continue;
            }
            if let Some(container_obj) = value
                .get_mut(&reg.container)
                .and_then(|c| c.as_object_mut())
                && container_obj.shift_remove(&reg.alias).is_some()
            {
                removed_any = true;
            }
        }
        if !removed_any {
            // Race: between detect and fix, something else
            // removed the duplicates. Idempotent no-op.
            actions_skipped += 1;
            continue;
        }
        // Serialize back. `to_string_pretty` matches the typical
        // config-writer convention (no trailing newline; same
        // serialization shape as `serde_json::json!`-built docs).
        let new_body = match serde_json::to_string_pretty(&value) {
            Ok(s) => s,
            Err(_) => {
                actions_skipped += 1;
                continue;
            }
        };
        // Mirror the live file's mode so fix() doesn't surprise-
        // change permissions.
        let mode = std::fs::symlink_metadata(&entry.config_path)
            .ok()
            .map(|m| platform::permission_mode(&m))
            .unwrap_or(FALLBACK_CONFIG_MODE);
        mutate(
            ctx,
            &entry.config_path,
            Op::WriteFile {
                content: new_body.into_bytes(),
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
    fn detector_flags_agent_mail_short_alias_duplicate() {
        let td = TempDir::new().unwrap();
        let path = write_config(
            &td,
            "short-alias-dup.json",
            r#"{"mcpServers":{
              "mcp-agent-mail": {"url": "http://127.0.0.1:8765/mcp/"},
              "agent-mail": {"url": "http://127.0.0.1:8765/mcp/"}
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
        assert!(aliases.contains("agent-mail"));
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
    fn finding_serializes_as_fixable_when_canonical_present_in_strict_json() {
        // .json + canonical (mcpServers, mcp-agent-mail) present
        // → auto_fixable: true with estimated_actions: 1.
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
        assert!(s.contains("auto_fix_summary"));
        assert!(s.contains("\"auto_fixable\":true"));
        assert!(s.contains("\"estimated_actions\":1"));
        assert!(s.contains("\"fixable_count\":1"));
        assert!(s.contains("\"unfixable_count\":0"));
        assert!(s.contains("am setup"));
    }

    #[test]
    fn finding_serializes_as_unfixable_when_canonical_missing() {
        // Two non-canonical entries, no canonical → unfixable.
        let f = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: "/tmp/cursor.json".into(),
                registrations: vec![
                    RegistrationPair {
                        container: "servers".to_string(),
                        alias: "mcp_agent_mail".to_string(),
                    },
                    RegistrationPair {
                        container: "mcp".to_string(),
                        alias: "agent-mail".to_string(),
                    },
                ],
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("\"fixable_count\":0"));
        assert!(s.contains("\"unfixable_count\":1"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn finding_serializes_as_unfixable_for_jsonc_extension() {
        // .jsonc → unfixable even with canonical present (comment
        // loss risk).
        let f = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: "/tmp/cursor.jsonc".into(),
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
        assert!(s.contains("\"fixable_count\":0"));
        assert!(s.contains("\"unfixable_count\":1"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

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

    /// **NEGATIVE TEST FIRST**: empty entries → no-op.
    #[test]
    fn fixer_with_empty_entries_is_a_no_op() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-19T00-00-00Z__dup_empty");
        let finding = McpDuplicateAliasedServerEntriesFinding {
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 0);
    }

    /// **NEGATIVE**: `.jsonc` config is skipped (comment loss risk),
    /// even when canonical is present.
    #[test]
    fn fixer_skips_jsonc_extension() {
        let td = TempDir::new().unwrap();
        let p = write_config(
            &td,
            "claude.jsonc",
            r#"{"mcpServers":{"mcp-agent-mail":{"url":"http://x"}},"servers":{"mcp_agent_mail":{"url":"http://y"}}}"#,
        );
        let ctx = ctx_for(&td, "2026-05-19T00-00-00Z__dup_jsonc");
        let finding = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: p.clone(),
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
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // File untouched.
        let post = fs::read_to_string(&p).unwrap();
        assert!(post.contains("mcp_agent_mail"));
    }

    /// **NEGATIVE**: a config without the canonical
    /// `(mcpServers, mcp-agent-mail)` pair is skipped — the
    /// fixer refuses to choose which non-canonical to promote.
    #[test]
    fn fixer_skips_when_canonical_pair_absent() {
        let td = TempDir::new().unwrap();
        let p = write_config(
            &td,
            "claude.json",
            r#"{"servers":{"mcp_agent_mail":{"url":"http://y"}},"mcp":{"agent-mail":{"url":"http://z"}}}"#,
        );
        let ctx = ctx_for(&td, "2026-05-19T00-00-00Z__dup_no_canon");
        let finding = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: p.clone(),
                registrations: vec![
                    RegistrationPair {
                        container: "servers".to_string(),
                        alias: "mcp_agent_mail".to_string(),
                    },
                    RegistrationPair {
                        container: "mcp".to_string(),
                        alias: "agent-mail".to_string(),
                    },
                ],
            }],
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // File untouched.
        let post = fs::read_to_string(&p).unwrap();
        assert!(post.contains("mcp_agent_mail"));
        assert!(post.contains("agent-mail"));
    }

    /// Positive: canonical present + non-canonical → fix removes
    /// non-canonical, preserves canonical verbatim including
    /// nested URL / token / args.
    #[test]
    fn fixer_drops_non_canonical_preserving_canonical_content() {
        let td = TempDir::new().unwrap();
        let p = write_config(
            &td,
            "claude.json",
            r#"{"mcpServers":{"mcp-agent-mail":{"url":"http://canonical","args":["serve"]},"other-server":{"url":"http://other"}},"servers":{"mcp_agent_mail":{"url":"http://stale"}}}"#,
        );
        let ctx = ctx_for(&td, "2026-05-19T00-00-00Z__dup_fix");
        let finding = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: p.clone(),
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
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);

        let post: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        // Canonical entry preserved verbatim.
        assert_eq!(
            post.pointer("/mcpServers/mcp-agent-mail/url")
                .and_then(|v| v.as_str()),
            Some("http://canonical")
        );
        assert_eq!(
            post.pointer("/mcpServers/mcp-agent-mail/args/0")
                .and_then(|v| v.as_str()),
            Some("serve")
        );
        // Sibling server preserved.
        assert_eq!(
            post.pointer("/mcpServers/other-server/url")
                .and_then(|v| v.as_str()),
            Some("http://other")
        );
        // Non-canonical entry removed.
        assert!(
            post.pointer("/servers/mcp_agent_mail").is_none(),
            "non-canonical (servers, mcp_agent_mail) should have been removed"
        );
    }

    /// Race idempotence: between detect and fix, the duplicate
    /// disappeared (operator hand-edited, or a sibling FM ran).
    /// fix() detects there's nothing to remove and skips.
    #[test]
    fn fixer_skips_when_duplicate_already_removed_between_detect_and_fix() {
        let td = TempDir::new().unwrap();
        let p = write_config(
            &td,
            "claude.json",
            r#"{"mcpServers":{"mcp-agent-mail":{"url":"http://canonical"}}}"#,
        );
        let ctx = ctx_for(&td, "2026-05-19T00-00-00Z__dup_race");
        // The finding was built when the duplicate was still
        // there, but by the time fix() reads the file, it's gone.
        let finding = McpDuplicateAliasedServerEntriesFinding {
            entries: vec![DuplicateConfigEntry {
                config_path: p.clone(),
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
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
