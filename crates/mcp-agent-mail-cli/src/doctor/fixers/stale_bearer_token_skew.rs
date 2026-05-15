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
//! Re-parse JSON, rewrite the token field at the recorded JSON
//! pointer, serialize back with `serde_json::to_string_pretty`,
//! call `mutate(... Op::WriteFile { content, mode: 0o600 })`.
//! Same shape as pass-13's wrong_mcp_url_json fixer.
//!
//! ## Reversibility
//!
//! Standard via `am doctor undo <run-id>`: the chokepoint's
//! verbatim backup is restored.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{Op, mutate};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

pub const FM_ID: &str = "fm-mcp-config-files-stale-bearer-token-skew";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "mcp_config_files";

/// MCP-servers container key variants (clients differ).
const SERVERS_CONTAINER_KEYS: &[&str] = &["mcpServers", "mcp_servers"];

/// Token-bearing field names. The Authorization header has both
/// canonical-case (`Authorization`) and lower-case (`authorization`)
/// variants depending on client JSON style. The `bearer` and
/// `token` standalone fields are non-RFC but common in client
/// configs that opt out of full HTTP-header verbiage.
const TOKEN_FIELDS: &[&str] = &["Authorization", "authorization", "bearer", "token"];

/// Structured pointer to the token field. Keep this separate from
/// `display_pointer()` so server names containing `.` remain addressable.
#[derive(Debug, Clone, Serialize)]
pub struct TokenLocation {
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
        if loc.inside_headers {
            format!(
                "{}.{}.headers.{}",
                loc.container_key, loc.server_name, loc.field
            )
        } else {
            format!("{}.{}.{}", loc.container_key, loc.server_name, loc.field)
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
                command: format!("am doctor --fix --only {FM_ID} --yes"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }
}

/// Token redactor: first 4 + last 4 chars, with length in the
/// middle. Sufficient for diff/triage; never exposes the full
/// secret in JSON logs.
fn redact(s: &str) -> String {
    if s.len() <= 12 {
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
                // Walk `headers.*` and direct top-level token fields.
                for token_field in TOKEN_FIELDS {
                    // Direct field on the server object.
                    if let Some(val) = server_obj.get(*token_field).and_then(|x| x.as_str())
                        && strip_bearer_prefix(val) != canonical_token
                    {
                        out.push(StaleBearerTokenSkewFinding {
                            config_path: path.clone(),
                            location: TokenLocation {
                                container_key: (*container_key).to_string(),
                                server_name: server_name.clone(),
                                inside_headers: false,
                                field: (*token_field).to_string(),
                            },
                            current_token: val.to_string(),
                            canonical_token: canonical_token.to_string(),
                        });
                    }
                    // Inside `headers.<field>`.
                    if let Some(headers) = server_obj.get("headers").and_then(|x| x.as_object())
                        && let Some(val) = headers.get(*token_field).and_then(|x| x.as_str())
                        && strip_bearer_prefix(val) != canonical_token
                    {
                        out.push(StaleBearerTokenSkewFinding {
                            config_path: path.clone(),
                            location: TokenLocation {
                                container_key: (*container_key).to_string(),
                                server_name: server_name.clone(),
                                inside_headers: true,
                                field: (*token_field).to_string(),
                            },
                            current_token: val.to_string(),
                            canonical_token: canonical_token.to_string(),
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
    lower.contains("agent") && lower.contains("mail")
}

/// Fixer. Routes through `mutate()` with `Op::WriteFile`.
///
/// Preserves the `"Bearer "` prefix iff the original value had
/// one. Re-parses the file before writing (defensive against
/// concurrent writers). Refuses if the current value no longer
/// matches what the detector recorded.
pub fn fix(
    ctx: &crate::doctor::mutate::MutateContext,
    finding: &StaleBearerTokenSkewFinding,
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

    // Walk the structured location directly; dotted display strings
    // are ambiguous for server names such as "agent-mail.prod".
    let loc = &finding.location;
    let Some(obj) = v.as_object_mut() else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    let Some(servers) = obj
        .get_mut(&loc.container_key)
        .and_then(|x| x.as_object_mut())
    else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };
    let Some(server_obj) = servers
        .get_mut(&loc.server_name)
        .and_then(|x| x.as_object_mut())
    else {
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    };

    let (target_obj, field): (&mut serde_json::Map<String, serde_json::Value>, &str) =
        if loc.inside_headers {
            let Some(headers) = server_obj
                .get_mut("headers")
                .and_then(|x| x.as_object_mut())
            else {
                return Ok(FixOutcome {
                    actions_taken: 0,
                    actions_skipped: 1,
                    quarantined_paths: Vec::new(),
                });
            };
            (headers, loc.field.as_str())
        } else {
            (server_obj, loc.field.as_str())
        };

    let cur = target_obj.get(field).and_then(|x| x.as_str()).unwrap_or("");
    if cur != finding.current_token {
        // Concurrent writer changed it; refuse to clobber.
        return Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        });
    }
    // Preserve `Bearer ` prefix iff the original had one.
    // Use `get(..7)` to avoid panicking on non-ASCII UTF-8 boundaries.
    let had_bearer_prefix = cur
        .get(..7)
        .is_some_and(|h| h.eq_ignore_ascii_case("bearer "));
    let new_value = if had_bearer_prefix {
        format!("Bearer {}", finding.canonical_token)
    } else {
        finding.canonical_token.clone()
    };
    target_obj.insert(field.to_string(), serde_json::Value::String(new_value));

    let new_body =
        serde_json::to_string_pretty(&v).map_err(crate::doctor::mutate::MutateError::Serde)?;
    let mut new_bytes = new_body.into_bytes();
    if !new_bytes.ends_with(b"\n") {
        new_bytes.push(b'\n');
    }
    mutate(
        ctx,
        &finding.config_path,
        Op::WriteFile {
            content: new_bytes,
            mode: 0o600, // bearer tokens — owner-only
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
        assert_eq!(
            outcome.actions_taken, 1,
            "structured TokenLocation must fix tokens for dotted server names"
        );
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
    fn evidence_redacts_token_values() {
        let f = StaleBearerTokenSkewFinding {
            config_path: PathBuf::from("/x/mcp.json"),
            location: TokenLocation {
                container_key: "mcpServers".into(),
                server_name: "agent-mail".into(),
                inside_headers: true,
                field: "Authorization".into(),
            },
            current_token: "Bearer super-secret-token-abc123".into(),
            canonical_token: "super-canonical-token-xyz789".into(),
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
}
