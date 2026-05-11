//! `fm-mcp-config-files-wrong-http-url-or-scheme` (JSON variant) — P1.
//!
//! **Subsystem**: mcp_config_files (Phase 1 archaeology).
//!
//! ## What's broken
//!
//! Per-client MCP configs (e.g., `~/.claude/.mcp.json`, `~/.cursor/
//! mcp.json`, `~/.gemini/settings.json`) hold a URL like
//! `http://127.0.0.1:8765/mcp/` that the client uses to reach the
//! mcp-agent-mail HTTP transport. If an operator changes the port
//! via `HTTP_PORT` env var, or moves the server behind a different
//! base path, the JSON configs become stale: the client tries the
//! old URL, fails, and the agent thinks Agent Mail is down.
//!
//! ## Detection (pure function)
//!
//! Given a canonical URL (e.g., from `Config::from_env()`) and a list
//! of JSON config files, for each file:
//! 1. Parse as JSON. Skip if malformed.
//! 2. Walk known URL-bearing paths in the JSON tree:
//!    - `mcpServers.<server-name>.url`
//!    - `mcp_servers.<server-name>.url`
//!    - `mcpServers.<server-name>.serverUrl`
//!    - `mcpServers.<server-name>.endpoint`
//!      Where `<server-name>` matches `*agent*mail*` (case-insensitive).
//! 3. Compare each found URL to the canonical. Emit a finding per
//!    mismatch.
//!
//! ## Fix (Op::WriteFile)
//!
//! Parse JSON → mutate the URL field → serialize back with
//! `serde_json::to_string_pretty` → write via `mutate(ctx, path,
//! Op::WriteFile { content, mode: 0o600 })`. The chokepoint handles
//! backup + atomic rename + hash recording.
//!
//! Demonstrates the canonical Op::WriteFile FM pattern alongside
//! passes 8-12's other patterns (Op::Rename, detect-only, Op::Chmod).
//!
//! ## Reversibility
//!
//! Standard via `am doctor undo <run-id>`: the chokepoint's verbatim
//! backup is restored.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-mcp-config-files-wrong-http-url-or-scheme";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// URL-bearing field names we look for in JSON config trees.
const URL_FIELD_NAMES: &[&str] = &["url", "serverUrl", "endpoint"];

/// MCP-servers container key variants (clients differ).
const SERVERS_CONTAINER_KEYS: &[&str] = &["mcpServers", "mcp_servers"];

#[derive(Debug, Clone, Serialize)]
pub struct WrongMcpUrlFinding {
    pub config_path: PathBuf,
    /// JSON-pointer-like path of the field with wrong URL, e.g.
    /// `mcpServers.agent-mail.url`.
    pub json_pointer: String,
    pub current_url: String,
    pub canonical_url: String,
}

impl WrongMcpUrlFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "MCP config {} field {} has URL {} (expected {})",
            self.config_path.display(),
            self.json_pointer,
            self.current_url,
            self.canonical_url,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.99,
            evidence: serde_json::json!({
                "config_path": self.config_path.to_string_lossy(),
                "json_pointer": self.json_pointer,
                "current_url": self.current_url,
                "canonical_url": self.canonical_url,
            }),
            remediation: FindingRemediation {
                command: format!("am doctor --fix --only {} --yes", FM_ID),
                explain_command: format!("am doctor explain {}", FM_ID),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Detector. PURE.
pub fn detect(canonical_url: &str, candidate_configs: &[PathBuf]) -> Vec<WrongMcpUrlFinding> {
    let mut out = Vec::new();
    for path in candidate_configs {
        let body = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue, // malformed JSON — skip
        };
        for container_key in SERVERS_CONTAINER_KEYS {
            let Some(servers) = v.get(container_key).and_then(|x| x.as_object()) else {
                continue;
            };
            for (server_name, server_val) in servers {
                if !is_agent_mail_server_name(server_name) {
                    continue;
                }
                let Some(server_obj) = server_val.as_object() else {
                    continue;
                };
                for field in URL_FIELD_NAMES {
                    let Some(url_val) = server_obj.get(*field).and_then(|x| x.as_str()) else {
                        continue;
                    };
                    if url_val != canonical_url {
                        out.push(WrongMcpUrlFinding {
                            config_path: path.clone(),
                            json_pointer: format!("{container_key}.{server_name}.{field}"),
                            current_url: url_val.to_string(),
                            canonical_url: canonical_url.to_string(),
                        });
                    }
                }
            }
        }
    }
    out
}

fn is_agent_mail_server_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Match "agent-mail", "agent_mail", "mcp_agent_mail", etc.
    lower.contains("agent") && lower.contains("mail")
}

/// Fixer. Routes through `mutate()` with `Op::WriteFile`.
///
/// The function re-parses the file (don't trust the detector's
/// snapshot — defensive against concurrent writers), mutates the
/// single field at the recorded JSON pointer, serializes back with
/// indented JSON (preserves operator-readable layout), and calls
/// `mutate(... Op::WriteFile ...)`.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &WrongMcpUrlFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    if !finding.config_path.exists() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }

    let body =
        fs::read_to_string(&finding.config_path).map_err(crate::doctor::mutate::MutateError::Io)?;
    let mut v: serde_json::Value =
        serde_json::from_str(&body).map_err(crate::doctor::mutate::MutateError::Serde)?;

    // Walk the JSON pointer (3 segments: container.server.field).
    let parts: Vec<&str> = finding.json_pointer.split('.').collect();
    if parts.len() != 3 {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    let (container, server, field) = (parts[0], parts[1], parts[2]);

    let Some(obj) = v.as_object_mut() else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    let Some(servers) = obj.get_mut(container).and_then(|x| x.as_object_mut()) else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    let Some(server_obj) = servers.get_mut(server).and_then(|x| x.as_object_mut()) else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    // Verify current value matches what the detector recorded — if a
    // concurrent writer changed it to something else (not stale, not
    // canonical), refuse rather than clobber. The chokepoint's H3
    // (post-backup re-hash) catches the same class of race for the
    // file as a whole; this catches it at the field level.
    let cur = server_obj.get(field).and_then(|x| x.as_str()).unwrap_or("");
    if cur != finding.current_url {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    server_obj.insert(
        field.to_string(),
        serde_json::Value::String(finding.canonical_url.clone()),
    );

    let new_body =
        serde_json::to_string_pretty(&v).map_err(crate::doctor::mutate::MutateError::Serde)?;
    // Append trailing newline to match common JSON style.
    let mut new_bytes = new_body.into_bytes();
    if !new_bytes.ends_with(b"\n") {
        new_bytes.push(b'\n');
    }

    mutate(
        ctx,
        &finding.config_path,
        Op::WriteFile {
            content: new_bytes,
            // 0o600 — bearer tokens may be nearby in the same file.
            mode: 0o600,
        },
    )?;

    Ok(FixOutcome {
        actions_taken: 1,
        actions_skipped: 0,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::mutate::{Capabilities, MutateContext};
    use crate::doctor::runs::scaffold_run_dir;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    fn ctx_for(td: &TempDir, run_id: &str) -> MutateContext {
        let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        MutateContext {
            run_id: run_id.to_string(),
            run_dir: run_dir.clone(),
            capabilities: Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: Mutex::new(actions),
            fixer_id: FM_ID.to_string(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: Instant::now(),
            extra_locks: Vec::new(),
        }
    }

    const CANONICAL: &str = "http://127.0.0.1:8765/mcp/";

    #[test]
    fn detector_returns_empty_when_url_matches() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "agent-mail": { "url": CANONICAL } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, &[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_wrong_url() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "agent-mail": { "url": "http://127.0.0.1:9999/mcp/" } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].config_path, p);
        assert_eq!(findings[0].json_pointer, "mcpServers.agent-mail.url");
        assert_eq!(findings[0].current_url, "http://127.0.0.1:9999/mcp/");
        assert_eq!(findings[0].canonical_url, CANONICAL);
    }

    #[test]
    fn detector_matches_underscored_servers_container() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcp_servers": { "mcp_agent_mail": { "url": "http://wrong/" } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, &[p]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].json_pointer.starts_with("mcp_servers."));
    }

    #[test]
    fn detector_matches_server_url_field_alias() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "agent-mail": { "serverUrl": "http://wrong/" } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, &[p]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].json_pointer.ends_with(".serverUrl"));
    }

    #[test]
    fn detector_skips_other_servers() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "unrelated-server": { "url": "http://wrong/" } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, &[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_malformed_json() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(&p, "this is not json {{{").unwrap();
        let findings = detect(CANONICAL, &[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn fixer_rewrites_json_url_via_mutate() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "agent-mail": { "url": "http://wrong/" } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, std::slice::from_ref(&p));
        let run_id = "2026-05-11T08-00-00Z__urlfix";
        let ctx = ctx_for(&td, run_id);
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        let body = fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["mcpServers"]["agent-mail"]["url"].as_str().unwrap(),
            CANONICAL
        );
        // Mode is 0o600 (bearer-tokens-in-config defense).
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn fixer_refuses_when_concurrent_writer_changed_value() {
        // Detector recorded current_url="http://wrong/", but by the time
        // fix() runs another process has changed the file to a different
        // (non-canonical) value. Fixer must refuse rather than clobber.
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "agent-mail": { "url": "http://wrong/" } }
            })
            .to_string(),
        )
        .unwrap();
        let findings = detect(CANONICAL, std::slice::from_ref(&p));
        // Concurrent writer changed it to a third value.
        fs::write(
            &p,
            serde_json::json!({
                "mcpServers": { "agent-mail": { "url": "http://something-else/" } }
            })
            .to_string(),
        )
        .unwrap();
        let ctx = ctx_for(&td, "2026-05-11T08-00-01Z__race");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(
            outcome.actions_taken, 0,
            "must NOT clobber the concurrent writer's value"
        );
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("something-else"));
    }

    #[test]
    fn fixer_then_undo_restores_byte_identical_json() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.json");
        let original = serde_json::json!({
            "mcpServers": { "agent-mail": { "url": "http://wrong/" } }
        })
        .to_string();
        fs::write(&p, &original).unwrap();
        let findings = detect(CANONICAL, std::slice::from_ref(&p));
        let run_id = "2026-05-11T08-00-02Z__roundtrip";
        let ctx = ctx_for(&td, run_id);
        let _ = fix(&ctx, &findings[0]).unwrap();
        drop(ctx);
        let summary = crate::doctor::undo::run_undo(td.path(), run_id, false, true).expect("undo");
        assert_eq!(summary.actions_replayed, 1);
        let restored = fs::read_to_string(&p).unwrap();
        assert_eq!(
            restored, original,
            "undo must restore byte-identical to original"
        );
    }

    #[test]
    fn fixer_idempotent_when_file_vanished() {
        let td = TempDir::new().unwrap();
        let finding = WrongMcpUrlFinding {
            config_path: td.path().join("nonexistent.json"),
            json_pointer: "mcpServers.agent-mail.url".into(),
            current_url: "http://wrong/".into(),
            canonical_url: CANONICAL.into(),
        };
        let ctx = ctx_for(&td, "2026-05-11T08-00-03Z__missing");
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    #[test]
    fn finding_serializes_with_required_fields() {
        let f = WrongMcpUrlFinding {
            config_path: "/x/y/config.json".into(),
            json_pointer: "mcpServers.agent-mail.url".into(),
            current_url: "http://wrong/".into(),
            canonical_url: CANONICAL.into(),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("\"severity\":\"P1\""));
        assert!(s.contains("mcpServers.agent-mail.url"));
    }
}
