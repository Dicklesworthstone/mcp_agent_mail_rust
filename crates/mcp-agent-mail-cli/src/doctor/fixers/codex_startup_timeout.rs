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
//! **Auto-fix via `Op::WriteFile`** (format-preserving). The
//! config is parsed with `toml_edit::DocumentMut`, which keeps
//! comments, key ordering, and whitespace intact. Only the
//! `startup_timeout_sec` scalar under the existing
//! `mcp_agent_mail` server entry is touched — set to the
//! canonical `CODEX_STARTUP_TIMEOUT_SECS` (30). This handles both
//! the `TooShort` state (replace the small value) and the
//! `Missing` state where the entry exists but lacks the key
//! (insert it). The chokepoint backs up the original bytes
//! verbatim, so `am doctor undo <run-id>` restores them
//! byte-identically.
//!
//! Skip (operator-supplied truth required):
//! - The `mcp_agent_mail` server entry doesn't exist at all
//!   (`[mcp_servers]` absent, or no `mcp_agent_mail` /
//!   `"mcp-agent-mail"` key): adding the whole server entry is
//!   `am setup`'s job and requires choosing stdio vs HTTP.
//! - The TOML is malformed (a different FM / the operator owns
//!   broken configs).
//! - The value is already ≥ the minimum (idempotent / a writer
//!   fixed it between detect and fix).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError, Op, mutate};
use crate::{CODEX_STARTUP_TIMEOUT_SECS, extract_mcp_agent_mail_toml_startup_timeout};
use mcp_agent_mail_core::mcp_config::{McpConfigLocation, McpConfigTool};
use std::os::unix::fs::PermissionsExt;
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
                command: format!("am doctor fix --only {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Auto-fix sets startup_timeout_sec on the existing
                // mcp_agent_mail entry via format-preserving
                // toml_edit. Skipped (counted in actions_skipped)
                // when the entry doesn't exist at all.
                auto_fixable: true,
                estimated_actions: 1,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "Auto-fix (preferred): `am doctor fix --only {} --yes` sets \
             `startup_timeout_sec = {}` on the existing mcp_agent_mail entry in {} via \
             format-preserving toml_edit (comments / key order / whitespace untouched), \
             reversible via `am doctor undo`. Manual alternative: edit the file and ensure \
             the [mcp_servers.mcp_agent_mail] section contains `startup_timeout_sec = {}` \
             (or quoted variant `[mcp_servers.\"mcp-agent-mail\"]`). Codex cold-boots \
             mcp-agent-mail in ~10s under normal conditions; a smaller timeout produces \
             flaky 'MCP server didn't respond' errors. If the mcp_agent_mail entry doesn't \
             exist at all, run `am setup` first — the auto-fix won't create the server entry.",
            FM_ID,
            self.min_required_secs,
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
            Some(t) if t < CODEX_STARTUP_TIMEOUT_SECS => {
                TimeoutState::TooShort { observed_secs: t }
            }
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

/// Candidate keys for the agent-mail server entry under
/// `[mcp_servers]`. Codex configs use the snake_case form
/// canonically; the quoted kebab form is also accepted by the
/// detector, so the fixer handles both.
const AGENT_MAIL_KEYS: &[&str] = &["mcp_agent_mail", "mcp-agent-mail"];

/// Set `startup_timeout_sec` on the existing `mcp_agent_mail`
/// entry in a parsed TOML document, preserving all formatting.
///
/// Returns `true` if a value was written, `false` if there was
/// nothing safe to do (entry absent, or already ≥ min). Never
/// creates the server entry itself — that's `am setup`'s job.
fn set_startup_timeout(doc: &mut toml_edit::DocumentMut, min_secs: u64) -> bool {
    let Some(servers) = doc
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return false;
    };
    let Some(key) = AGENT_MAIL_KEYS
        .iter()
        .find(|k| servers.contains_key(k))
        .copied()
    else {
        return false;
    };
    let Some(entry) = servers
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        // Entry exists but isn't a table/inline-table (e.g. a
        // bare string) — not a shape we can safely edit.
        return false;
    };
    // Idempotence / race guard: if already ≥ min, do nothing.
    if let Some(cur) = entry
        .get("startup_timeout_sec")
        .and_then(toml_edit::Item::as_integer)
        && cur >= 0
        && (cur as u64) >= min_secs
    {
        return false;
    }
    entry.insert(
        "startup_timeout_sec",
        toml_edit::value(i64::try_from(min_secs).unwrap_or(i64::MAX)),
    );
    true
}

/// Fixer. Format-preserving TOML edit via `Op::WriteFile`.
pub fn fix(
    ctx: &MutateContext,
    finding: &CodexStartupTimeoutFinding,
) -> Result<FixOutcome, MutateError> {
    let skip = || {
        Ok(FixOutcome {
            actions_taken: 0,
            actions_skipped: 1,
            quarantined_paths: Vec::new(),
        })
    };
    let content = match std::fs::read_to_string(&finding.config_path) {
        Ok(c) => c,
        Err(_) => return skip(), // vanished between detect and fix
    };
    let mut doc = match content.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return skip(), // malformed TOML — operator owns it
    };
    if !set_startup_timeout(&mut doc, finding.min_required_secs) {
        // Entry absent, wrong shape, or already healthy.
        return skip();
    }
    let new_content = doc.to_string();
    let mode = std::fs::symlink_metadata(&finding.config_path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    mutate(
        ctx,
        &finding.config_path,
        Op::WriteFile {
            content: new_content.into_bytes(),
            mode,
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
        assert_eq!(
            findings[0].state,
            TimeoutState::TooShort { observed_secs: 5 }
        );
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
    fn detector_accepts_inline_table_form() {
        // Pass-35-review Codex F1 / Gemini F4 (P1): the
        // inline-table form is valid TOML and operators write
        // it; pre-fix the helper missed it and produced a
        // false `Missing` finding.
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers]
mcp_agent_mail = { command = "mcp-agent-mail", startup_timeout_sec = 60 }
"#,
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert!(findings.is_empty(), "inline-table form must be recognized");
    }

    #[test]
    fn detector_flags_inline_table_too_short() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            r#"
[mcp_servers]
"mcp-agent-mail" = { startup_timeout_sec = 5 }
"#,
        )
        .unwrap();
        let findings = detect(&[loc_for(p, McpConfigTool::Codex, true)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].state,
            TimeoutState::TooShort { observed_secs: 5 }
        );
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
        assert_eq!(
            findings[0].state,
            TimeoutState::TooShort { observed_secs: 5 }
        );
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
    fn finding_severity_is_p1_auto_fixable() {
        let f = CodexStartupTimeoutFinding {
            config_path: PathBuf::from("/x/config.toml"),
            state: TimeoutState::Missing,
            min_required_secs: 30,
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P1");
        assert!(g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 1);
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

    fn finding_for(path: PathBuf, state: TimeoutState) -> CodexStartupTimeoutFinding {
        CodexStartupTimeoutFinding {
            config_path: path,
            state,
            min_required_secs: CODEX_STARTUP_TIMEOUT_SECS,
        }
    }

    /// **NEGATIVE**: config vanished between detect and fix → skip.
    #[test]
    fn fixer_skips_vanished_config() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_vanished");
        let finding = finding_for(td.path().join("absent.toml"), TimeoutState::Missing);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// **NEGATIVE**: the mcp_agent_mail server entry doesn't exist
    /// at all → skip (am setup territory; never create the entry).
    #[test]
    fn fixer_skips_when_entry_absent() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(&p, "[mcp_servers.some_other_server]\ncommand = \"x\"\n").unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_absent");
        let finding = finding_for(p.clone(), TimeoutState::Missing);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        // File untouched.
        assert!(!fs::read_to_string(&p).unwrap().contains("startup_timeout_sec"));
    }

    /// **NEGATIVE**: malformed TOML → skip (operator owns it).
    #[test]
    fn fixer_skips_malformed_toml() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(&p, "[mcp_servers.mcp_agent_mail\nbroken = ").unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_malformed");
        let finding = finding_for(p.clone(), TimeoutState::Missing);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }

    /// Positive: TooShort → bump to 30, preserving a comment and a
    /// sibling key in the same section.
    #[test]
    fn fixer_bumps_too_short_preserving_format() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        let original = "# my codex config\n\
                        [mcp_servers.mcp_agent_mail]\n\
                        command = \"mcp-agent-mail\"  # the rust binary\n\
                        startup_timeout_sec = 5\n";
        fs::write(&p, original).unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_short");
        let finding = finding_for(p.clone(), TimeoutState::TooShort { observed_secs: 5 });
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(outcome.actions_skipped, 0);
        let post = fs::read_to_string(&p).unwrap();
        assert!(post.contains("startup_timeout_sec = 30"));
        assert!(!post.contains("startup_timeout_sec = 5"));
        // Comments + sibling key preserved.
        assert!(post.contains("# my codex config"));
        assert!(post.contains("# the rust binary"));
        assert!(post.contains("command = \"mcp-agent-mail\""));
        // Detector now sees a healthy value.
        assert_eq!(
            extract_mcp_agent_mail_toml_startup_timeout(&post),
            Some(CODEX_STARTUP_TIMEOUT_SECS)
        );
    }

    /// Positive: Missing key on an EXISTING entry → insert it.
    #[test]
    fn fixer_inserts_missing_key_on_existing_entry() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            "[mcp_servers.mcp_agent_mail]\ncommand = \"mcp-agent-mail\"\n",
        )
        .unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_missing");
        let finding = finding_for(p.clone(), TimeoutState::Missing);
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert_eq!(
            extract_mcp_agent_mail_toml_startup_timeout(&fs::read_to_string(&p).unwrap()),
            Some(CODEX_STARTUP_TIMEOUT_SECS)
        );
    }

    /// Positive: quoted kebab key form `"mcp-agent-mail"` is handled.
    #[test]
    fn fixer_handles_quoted_kebab_key() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            "[mcp_servers.\"mcp-agent-mail\"]\nstartup_timeout_sec = 3\n",
        )
        .unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_kebab");
        let finding = finding_for(p.clone(), TimeoutState::TooShort { observed_secs: 3 });
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 1);
        assert!(fs::read_to_string(&p).unwrap().contains("startup_timeout_sec = 30"));
    }

    /// Idempotence: an already-healthy value is left alone.
    #[test]
    fn fixer_skips_when_already_healthy() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        fs::write(
            &p,
            "[mcp_servers.mcp_agent_mail]\nstartup_timeout_sec = 60\n",
        )
        .unwrap();
        let ctx = ctx_for(&td, "2026-05-20T00-00-00Z__codex_healthy");
        // (The detector wouldn't emit this, but fix() must tolerate
        // a stale finding pointing at an already-fixed file.)
        let finding = finding_for(p.clone(), TimeoutState::TooShort { observed_secs: 5 });
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
        assert!(fs::read_to_string(&p).unwrap().contains("startup_timeout_sec = 60"));
    }
}
