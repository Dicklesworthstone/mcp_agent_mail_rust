//! `fm-mcp-config-files-stale-bearer-token-skew` — P1.
//!
//! **Subsystem**: mcp_config_files (Phase 1 archaeology — HANDOFF
//! P3-C #7 ranking).
//!
//! ## What's broken
//!
//! Per-client MCP configs hold the bearer token the client sends
//! with each request, e.g.:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "agent-mail": {
//!       "url": "http://127.0.0.1:8765/mcp/",
//!       "headers": { "Authorization": "Bearer abc123..." }
//!     }
//!   }
//! }
//! ```
//!
//! When the server's `HTTP_BEARER_TOKEN` is rotated (operator
//! intervention, post-incident response, etc.) the client
//! configs become stale: requests fail with 401 and the agent
//! thinks Agent Mail is down. Mirror of pass-13's
//! `wrong_mcp_url_json` FM but for the token instead of the URL.
//!
//! ## Detection (pure function)
//!
//! Given a canonical token (from `Config::http_bearer_token`) and
//! a list of JSON config files, walk known token-bearing paths:
//!
//! - `mcpServers.<server>.headers.Authorization` (case-insensitive)
//! - `mcpServers.<server>.headers.authorization`
//! - `mcpServers.<server>.bearer`
//! - `mcpServers.<server>.token`
//! - same for `mcp_servers` (underscore) container.
//!
//! Where `<server>` matches `*agent*mail*` (case-insensitive).
//! Compare extracted token to canonical (stripping any
//! `"Bearer "` prefix for the Authorization-header case). Emit
//! a finding per mismatch.
//!
//! ## Fix (`Op::WriteFile`)
//!
//! `transform_config_file` reads and renders the current JSON under the
//! canonical mutation locks. Only the detected token field changes;
//! removed fields and newer operator values are not overwritten.
//! Backup, hash verification, owner-only publication, and undo records
//! remain in the mutation chokepoint.
//!
//! ## Reversibility
//!
//! Standard via `am doctor undo <run-id>`: the chokepoint's
//! verbatim backup is restored.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::transform_config_file;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-mcp-config-files-stale-bearer-token-skew";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// MCP-servers container key variants (clients differ).
const SERVERS_CONTAINER_KEYS: &[&str] = &["mcpServers", "mcp_servers"];

/// JSON token aliases retain their spelling; Authorization header names
/// are matched case-insensitively and their actual key is preserved.
fn is_token_field(field: &str) -> bool {
    field.eq_ignore_ascii_case("authorization") || matches!(field, "bearer" | "token")
}

/// Structured pointer to the token field. Keep this separate from
/// `display_pointer()` so server names containing `.` remain addressable.
#[derive(Debug, Clone, Serialize)]
pub struct TokenLocation {
    /// Project-scoped container path, when the MCP server entry lives
    /// under `projects.<abs-path>.<container_key>` instead of the
    /// top-level container. This is the shape `claude mcp add` (local
    /// scope) writes into `~/.claude.json`. `None` means the entry is
    /// at the top level. The string is the project key as-is (an
    /// absolute path), including any `.` characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// `"mcpServers"` or `"mcp_servers"`.
    pub container_key: String,
    /// The server name as-is, including any `.` characters.
    pub server_name: String,
    /// True if the token lives at
    /// `<container>.<server>.headers.<field>`; false if it's a
    /// direct field on the server object.
    pub inside_headers: bool,
    /// The token-bearing field name (`Authorization`,
    /// `authorization`, `bearer`, `token`).
    pub field: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleBearerTokenSkewFinding {
    pub config_path: PathBuf,
    pub location: TokenLocation,
    /// Current token value as it appears in the file (with any
    /// `Bearer ` prefix preserved for fidelity).
    pub current_token: String,
    /// Canonical token from `Config::http_bearer_token`.
    pub canonical_token: String,
}

impl StaleBearerTokenSkewFinding {
    /// Human-readable JSON-pointer-style string for titles and
    /// evidence display. Built from the structured `TokenLocation`;
    /// only used for display, never for fix() navigation (the
    /// fixer consumes `self.location` directly to avoid the
    /// pre-pass-34 dotted-string ambiguity).
    pub fn display_pointer(&self) -> String {
        let loc = &self.location;
        let prefix = match &loc.project_path {
            Some(project) => format!("projects.{project}."),
            None => String::new(),
        };
        if loc.inside_headers {
            format!(
                "{}{}.{}.headers.{}",
                prefix, loc.container_key, loc.server_name, loc.field
            )
        } else {
            format!(
                "{}{}.{}.{}",
                prefix, loc.container_key, loc.server_name, loc.field
            )
        }
    }

    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "MCP config {} field {} has stale bearer token (rotated)",
            self.config_path.display(),
            self.display_pointer()
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 0.95,
            evidence: serde_json::json!({
                "config_path": self.config_path.to_string_lossy(),
                "location": self.location,
                "display_pointer": self.display_pointer(),
                // Token values redacted in evidence — never echo to
                // logs / report.json. Only the existence + path is
                // surfaced. Operators who need the actual values open
                // the file.
                "current_token_redacted": redact(&self.current_token),
                "canonical_token_redacted": redact(&self.canonical_token),
            }),
            remediation: FindingRemediation {
                command: super::fix_only_command(FM_ID),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Token redactor: first 4 + last 4 chars, with length in the
/// middle. Short values are completely masked, including multibyte
/// values whose byte length is greater than their character count.
fn redact(s: &str) -> String {
    if s.chars().take(13).count() <= 12 {
        return format!("<redacted len={}>", s.len());
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}...{tail} (len={})", s.len())
}

/// Strip a leading `Bearer ` prefix (case-insensitive) for value
/// comparison. The Authorization header's value is conventionally
/// `Bearer <token>`; we compare just the `<token>` part.
///
/// Use `str::get` rather than byte slicing so malformed-looking
/// non-ASCII token values cannot panic at a UTF-8 boundary.
fn strip_bearer_prefix(s: &str) -> &str {
    if let Some(head) = s.get(..7)
        && head.eq_ignore_ascii_case("bearer ")
    {
        return s.get(7..).unwrap_or(s).trim_start();
    }
    s
}

/// Detector. PURE.
///
/// `canonical_token` is the server's current `HTTP_BEARER_TOKEN`.
/// `candidate_configs` is the same list of well-known MCP client
/// config paths the `wrong_mcp_url_json` FM uses — the handler's
/// `default_mcp_config_candidates()` helper.
pub fn detect(
    canonical_token: &str,
    candidate_configs: &[PathBuf],
) -> Vec<StaleBearerTokenSkewFinding> {
    let mut out = Vec::new();
    if canonical_token.is_empty() {
        // Nothing to compare against — no canonical token configured.
        return out;
    }
    for path in candidate_configs {
        let body = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Top-level `mcpServers` / `mcp_servers` containers (the
        // common shape and `claude mcp add -s user` writes into
        // `~/.claude.json`'s top-level container).
        scan_value_containers(path, None, &v, canonical_token, &mut out);
        // Project-scoped containers: `projects.<abs-path>.mcpServers`.
        // This is what the DEFAULT (local-scope) `claude mcp add`
        // writes into `~/.claude.json` — the exact file/shape that
        // drifted in the ts1 401 incident (br-5gfrd).
        if let Some(projects) = v.get("projects").and_then(|x| x.as_object()) {
            for (project_path, project_val) in projects {
                scan_value_containers(
                    path,
                    Some(project_path.as_str()),
                    project_val,
                    canonical_token,
                    &mut out,
                );
            }
        }
    }
    out
}

/// Scan the `mcpServers` / `mcp_servers` containers directly on
/// `parent` for agent-mail entries whose token differs from
/// `canonical_token`. `project_path` records the project scope so the
/// fixer can navigate back to `projects.<project_path>.<container>`;
/// pass `None` for a top-level container.
fn scan_value_containers(
    path: &std::path::Path,
    project_path: Option<&str>,
    parent: &serde_json::Value,
    canonical_token: &str,
    out: &mut Vec<StaleBearerTokenSkewFinding>,
) {
    for container_key in SERVERS_CONTAINER_KEYS {
        let Some(servers) = parent.get(container_key).and_then(|x| x.as_object()) else {
            continue;
        };
        for (server_name, server_val) in servers {
            if !is_agent_mail_server_name(server_name) {
                continue;
            }
            let Some(server_obj) = server_val.as_object() else {
                continue;
            };
            // Iterate actual keys rather than two hard-coded header
            // spellings so mixed/upper-case Authorization is repaired too.
            for (inside_headers, fields) in [
                (false, Some(server_obj)),
                (
                    true,
                    server_obj
                        .get("headers")
                        .and_then(serde_json::Value::as_object),
                ),
            ] {
                let Some(fields) = fields else {
                    continue;
                };
                for (field, value) in fields {
                    if !is_token_field(field) {
                        continue;
                    }
                    let Some(token) = value.as_str() else {
                        continue;
                    };
                    if mcp_agent_mail_core::setup::constant_time_str_eq(
                        strip_bearer_prefix(token),
                        canonical_token,
                    ) {
                        continue;
                    }
                    out.push(StaleBearerTokenSkewFinding {
                        config_path: path.to_path_buf(),
                        location: TokenLocation {
                            project_path: project_path.map(str::to_string),
                            container_key: (*container_key).to_string(),
                            server_name: server_name.clone(),
                            inside_headers,
                            field: field.clone(),
                        },
                        current_token: token.to_string(),
                        canonical_token: canonical_token.to_string(),
                    });
                }
            }
        }
    }
}

fn is_agent_mail_server_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("agent") && lower.contains("mail")
}

/// Rotate the detected field in the current, locked snapshot. Preserve the
/// Bearer prefix when present, but never recreate a removed credential or
/// overwrite a value changed since detection.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &StaleBearerTokenSkewFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    if finding.canonical_token.is_empty() {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    let result = transform_config_file(ctx, &finding.config_path, 0o600, true, |source| {
        let mut value: serde_json::Value = serde_json::from_slice(source)?;
        let loc = &finding.location;
        if !SERVERS_CONTAINER_KEYS.contains(&loc.container_key.as_str())
            || !is_agent_mail_server_name(&loc.server_name)
            || !is_token_field(&loc.field)
        {
            return Ok(None);
        }
        // Structured locations preserve literal dots/slashes in both
        // project keys and server names; never parse the display string.
        let parent = match &loc.project_path {
            None => &mut value,
            Some(project) => {
                let Some(scoped) = value
                    .get_mut("projects")
                    .and_then(|projects| projects.get_mut(project.as_str()))
                else {
                    return Ok(None);
                };
                scoped
            }
        };
        let Some(server) = parent
            .get_mut(loc.container_key.as_str())
            .and_then(|servers| servers.get_mut(loc.server_name.as_str()))
        else {
            return Ok(None);
        };
        let fields = if loc.inside_headers {
            let Some(headers) = server.get_mut("headers") else {
                return Ok(None);
            };
            headers
        } else {
            server
        };
        let Some(slot) = fields.get_mut(loc.field.as_str()) else {
            return Ok(None);
        };
        let Some(current) = slot.as_str() else {
            return Ok(None);
        };
        if !mcp_agent_mail_core::setup::constant_time_str_eq(current, &finding.current_token)
            || mcp_agent_mail_core::setup::constant_time_str_eq(
                strip_bearer_prefix(current),
                &finding.canonical_token,
            )
        {
            return Ok(None);
        }
        let had_bearer_prefix = current
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("bearer "));
        let replacement = if had_bearer_prefix {
            format!("Bearer {}", finding.canonical_token)
        } else {
            finding.canonical_token.clone()
        };
        *slot = serde_json::Value::String(replacement);
        let mut bytes = serde_json::to_vec_pretty(&value)?;
        bytes.push(b'\n');
        Ok(Some(bytes))
    })?;
    let (actions_taken, actions_skipped) = if result.is_some() { (1, 0) } else { (0, 1) };
    Ok(FixOutcome {
        actions_taken,
        actions_skipped,
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
        crate::doctor::ensure_private_test_git_root(td.path());
        let ignore = td.path().join(".gitignore");
        if !ignore.exists() {
            fs::write(
                &ignore,
                b"/mcp.json\n/claude.json\n/.mcp.json.*\n/.claude.json.*\n/mcp.json.bak*\n/claude.json.bak*\n/.doctor/\n",
            )
            .unwrap();
        }
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

    #[test]
    fn detector_returns_empty_when_token_matches() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"headers":{"Authorization":"Bearer canonical-xyz"}}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", &[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_flags_authorization_header_skew() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"headers":{"Authorization":"Bearer stale-old"}}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].display_pointer().contains("Authorization"));
    }

    #[test]
    fn detector_flags_lowercase_authorization() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"headers":{"authorization":"Bearer stale-old"}}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn detector_flags_direct_bearer_field() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"bearer":"stale-old"}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].display_pointer().ends_with("bearer"));
    }

    #[test]
    fn detector_skips_other_server_names() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"some-other-server":{"headers":{"Authorization":"Bearer whatever"}}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", &[p]);
        assert!(findings.is_empty());
    }

    #[test]
    fn detector_skips_when_canonical_empty() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(&p, r#"{"mcpServers":{"agent-mail":{"bearer":"anything"}}}"#).unwrap();
        let findings = detect("", &[p]);
        assert!(
            findings.is_empty(),
            "empty canonical means 'unconfigured' — never flag"
        );
    }

    #[test]
    fn fixer_rewrites_with_bearer_prefix_preserved() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"headers":{"Authorization":"Bearer stale-old"}}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-14T02-00-00Z__token_rewrite");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        let body = fs::read_to_string(&p).unwrap();
        assert!(
            body.contains(r#""Bearer canonical-xyz""#),
            "post-fix must preserve Bearer prefix; got:\n{body}"
        );
        assert!(!body.contains("stale-old"));
    }

    #[test]
    fn fixer_rewrites_direct_field_without_bearer_prefix() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"bearer":"stale-old"}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-05-14T02-00-00Z__token_direct");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        let body = fs::read_to_string(&p).unwrap();
        // Direct field doesn't get a Bearer prefix added.
        assert!(body.contains(r#""bearer": "canonical-xyz""#));
        assert!(!body.contains("stale-old"));
    }

    #[test]
    fn fixer_refuses_when_concurrent_writer_changed_value() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"bearer":"stale-old"}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));

        // Simulate concurrent writer.
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail":{"bearer":"some-third-value"}}}"#,
        )
        .unwrap();

        let ctx = ctx_for(&td, "2026-05-14T02-00-00Z__token_concurrent");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        // Refused.
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        let body = fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("some-third-value"),
            "concurrent-writer value preserved"
        );
    }

    #[test]
    fn detector_handles_server_names_with_dots() {
        // Structured TokenLocation must round-trip even when the MCP
        // server key contains literal `.` characters.
        let td = TempDir::new().unwrap();
        let p = td.path().join("mcp.json");
        fs::write(
            &p,
            r#"{"mcpServers":{"agent-mail.prod":{"bearer":"stale-old"}}}"#,
        )
        .unwrap();
        let findings = detect("canonical-xyz", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].location.server_name, "agent-mail.prod");

        let ctx = ctx_for(&td, "2026-05-14T03-00-00Z__token_dotted_server");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains(r#""bearer": "canonical-xyz""#));
    }

    #[test]
    fn strip_bearer_prefix_does_not_panic_on_multibyte_utf8() {
        // `strip_bearer_prefix` must not panic on non-ASCII token values.
        let inputs = [
            "ééééé",           // 5 chars × 2 bytes = 10 bytes
            "éBearer test",    // multi-byte at start
            "🔒 secret-token", // 4-byte codepoint at start
            "Bearer ééé",      // Bearer prefix + multi-byte token
        ];
        for s in inputs {
            // Just call it — should never panic.
            let _stripped = strip_bearer_prefix(s);
        }
    }

    #[test]
    fn detector_flags_project_scoped_claude_json_skew() {
        // `~/.claude.json` shape: default (local) `claude mcp add`
        // writes the server under `projects.<abs-path>.mcpServers`,
        // NOT the top-level container. This is the exact ts1 401
        // incident shape (br-5gfrd).
        let td = TempDir::new().unwrap();
        let p = td.path().join("claude.json");
        fs::write(
            &p,
            r#"{
  "numStartups": 7,
  "projects": {
    "/abs/path/to/repo": {
      "mcpServers": {
        "agent-mail": {
          "type": "http",
          "url": "http://127.0.0.1:8765/mcp/",
          "headers": { "Authorization": "Bearer stale-956458c9" }
        }
      }
    }
  }
}"#,
        )
        .unwrap();
        let findings = detect("a9d26a73-canonical", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1, "must flag the project-scoped entry");
        assert_eq!(
            findings[0].location.project_path.as_deref(),
            Some("/abs/path/to/repo")
        );
        assert!(
            findings[0]
                .display_pointer()
                .starts_with("projects./abs/path/to/repo.mcpServers.agent-mail")
        );
    }

    #[test]
    fn fixer_rewrites_project_scoped_claude_json_in_place() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("claude.json");
        fs::write(
            &p,
            r#"{
  "numStartups": 7,
  "projects": {
    "/abs/path/to/repo": {
      "mcpServers": {
        "agent-mail": {
          "url": "http://127.0.0.1:8765/mcp/",
          "headers": { "Authorization": "Bearer stale-956458c9" }
        }
      }
    }
  }
}"#,
        )
        .unwrap();
        let findings = detect("a9d26a73-canonical", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 1);

        let ctx = ctx_for(&td, "2026-06-19T00-00-00Z__claude_json_scoped");
        let outcome = fix(&ctx, &findings[0]).expect("fix");
        assert_eq!(outcome.actions_taken, 1);

        let body = fs::read_to_string(&p).unwrap();
        assert!(
            body.contains(r#""Bearer a9d26a73-canonical""#),
            "post-fix must reconcile the project-scoped token; got:\n{body}"
        );
        assert!(!body.contains("stale-956458c9"));
        // Unrelated keys must survive the whole-file rewrite (preserve_order).
        assert!(body.contains(r#""numStartups": 7"#));
    }

    #[test]
    fn detector_flags_both_top_level_and_project_scoped() {
        // A `~/.claude.json` can carry BOTH a user-scoped (top-level)
        // and a local-scoped (project) agent-mail entry; flag each.
        let td = TempDir::new().unwrap();
        let p = td.path().join("claude.json");
        fs::write(
            &p,
            r#"{
  "mcpServers": {
    "agent-mail": { "headers": { "Authorization": "Bearer stale-top" } }
  },
  "projects": {
    "/repo": {
      "mcpServers": {
        "agent-mail": { "headers": { "Authorization": "Bearer stale-scoped" } }
      }
    }
  }
}"#,
        )
        .unwrap();
        let findings = detect("canon", std::slice::from_ref(&p));
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|f| f.location.project_path.is_none()));
        assert!(
            findings
                .iter()
                .any(|f| f.location.project_path.as_deref() == Some("/repo"))
        );
    }

    #[test]
    fn evidence_redacts_token_values() {
        let f = StaleBearerTokenSkewFinding {
            config_path: PathBuf::from("/x/mcp.json"),
            location: TokenLocation {
                project_path: None,
                container_key: "mcpServers".into(),
                server_name: "agent-mail".into(),
                inside_headers: true,
                field: "Authorization".into(),
            },
            current_token: "Bearer super-secret-token-abc123".into(), // ubs:ignore — synthetic cfg(test) canary must be absent from serialized finding evidence
            canonical_token: "super-canonical-token-xyz789".into(), // ubs:ignore — synthetic cfg(test) canonical canary verifies evidence redaction
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(
            !s.contains("super-secret-token-abc123"),
            "evidence must NOT include the raw token: {s}"
        );
        assert!(
            !s.contains("super-canonical-token-xyz789"),
            "evidence must NOT include the raw canonical: {s}"
        );
    }

    #[test]
    fn fixer_locks_before_parsing_token_configuration() {
        use fs2::FileExt;
        let td = TempDir::new().unwrap();
        let path = td.path().join("mcp.json");
        fs::write(&path, r#"{"mcpServers":{"agent-mail":{"token":"old"}}}"#).unwrap();
        let findings = detect("new", std::slice::from_ref(&path));
        let ctx = ctx_for(&td, "2026-10-14T00-02-00Z__token_lock");
        let writer = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(td.path().join(".mcp.json.doctor-lock"))
            .unwrap();
        writer.try_lock_exclusive().unwrap();
        fs::write(&path, b"{in-progress operator edit").unwrap();
        let error = fix(&ctx, &findings[0]).unwrap_err();
        assert!(matches!(
            error,
            crate::doctor::mutate::MutateError::LockHeld(_)
        ));
        assert_eq!(fs::read(&path).unwrap(), b"{in-progress operator edit");
        assert!(
            fs::read(ctx.run_dir.join("actions.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fixer_never_recreates_a_removed_empty_token_field() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("mcp.json");
        fs::write(&path, r#"{"mcpServers":{"agent-mail":{"token":""}}}"#).unwrap();
        let findings = detect("new-secret", std::slice::from_ref(&path));
        assert_eq!(findings.len(), 1);
        let edited = br#"{"mcpServers":{"agent-mail":{"url":"http://keep/"}}}"#;
        fs::write(&path, edited).unwrap();
        let ctx = ctx_for(&td, "2026-10-14T00-02-01Z__token_removed");
        let result = fix(&ctx, &findings[0]).unwrap();
        assert_eq!(result.actions_taken, 0);
        assert_eq!(result.actions_skipped, 1);
        assert_eq!(fs::read(&path).unwrap(), edited);
        assert!(
            fs::read(ctx.run_dir.join("actions.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn detector_and_fixer_preserve_arbitrary_authorization_header_case() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("mcp.json");
        for (index, field) in ["AUTHORIZATION", "AuThOrIzAtIoN"].iter().enumerate() {
            let mut headers = serde_json::Map::new();
            headers.insert((*field).to_string(), serde_json::json!("Bearer old"));
            headers.insert("X-Keep".to_string(), serde_json::json!("unchanged"));
            let config = serde_json::json!({
                "mcp_servers": { "agent-mail.prod": { "headers": headers } }
            });
            fs::write(&path, config.to_string()).unwrap();
            let findings = detect("new", std::slice::from_ref(&path));
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].location.field, *field);
            let ctx = ctx_for(&td, &format!("2026-10-14T00-02-02Z__case_{index}"));
            assert_eq!(fix(&ctx, &findings[0]).unwrap().actions_taken, 1);
            let repaired: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let headers = &repaired["mcp_servers"]["agent-mail.prod"]["headers"];
            assert_eq!(headers[*field], "Bearer new");
            assert_eq!(headers["X-Keep"], "unchanged");
            assert_eq!(fix(&ctx, &findings[0]).unwrap().actions_skipped, 1);
        }
    }

    #[test]
    fn short_multibyte_tokens_are_completely_redacted() {
        for token in ["🔐🔒🔑🗝", "ééééééé", "漢字秘密"] {
            assert!(redact(token).starts_with("<redacted len="));
            assert!(!redact(token).contains(token));
        }
    }
}
