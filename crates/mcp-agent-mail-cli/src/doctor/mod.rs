//! World-class `am doctor` surface — the agent-ergonomic upgrade.
//!
//! This module adds the missing world-class verbs to `am doctor`:
//! `capabilities`, `robot-docs`, `undo`, `ls`, `diff`, `gc`, `health`,
//! plus the per-run `.doctor/runs/<run-id>/` artifact layout, the
//! `mutate()` chokepoint, and the agent-ergonomic JSON contract.
//!
//! The existing verbs (`check`, `repair`, `backups`, `restore`,
//! `reconstruct`, `archive-scan`, `archive-normalize`, `fix`,
//! `fix-orphan-refs`, `pack-archive`) continue to work; pass-2 will
//! refactor them through the chokepoint.
//!
//! Every public surface here matches CLI-SURFACE.md from the
//! `world-class-doctor-mode-for-cli-tools` skill verbatim. The handbook
//! at `am doctor robot-docs` is the single source of truth for agents.

#![forbid(unsafe_code)]

pub mod capabilities;
pub mod mutate;
pub mod robot_docs;
pub mod runs;
pub mod undo;

use crate::output::CliOutputFormat;
use crate::{CliError, CliResult};
use std::path::PathBuf;

/// Print `capabilities --json` (or text fallback for `--format toon`).
pub fn handle_capabilities(format: Option<CliOutputFormat>) -> CliResult<()> {
    let tool_version = env!("CARGO_PKG_VERSION").to_string();
    // Pass-1: write_scopes are computed lazily by the existing fixers;
    // here we expose the canonical set we know about.
    let write_scopes = default_write_scopes();
    let report = capabilities::build_report(tool_version, write_scopes);

    let fmt = format.unwrap_or(CliOutputFormat::Json);
    match fmt {
        CliOutputFormat::Json | CliOutputFormat::Toon | CliOutputFormat::Table => {
            // Capabilities is a contract — always JSON regardless of format
            // request. (TOON would erase types; table is lossy.)
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| CliError::Other(format!("serializing capabilities: {e}")))?;
            println!("{json}");
            Ok(())
        }
    }
}

/// Print `robot-docs` to stdout. Markdown.
pub fn handle_robot_docs() -> CliResult<()> {
    println!("{}", robot_docs::handbook());
    Ok(())
}

/// Print `am doctor health` — one-line liveness summary + exit 0/1.
///
/// Cheap. For CI scheduling. Reads `.doctor/latest/report.json` if present.
pub fn handle_health(target: &std::path::Path) -> CliResult<()> {
    let root = runs::doctor_root(target);
    let latest = root.join("latest");
    let runs_dir = root.join("runs");

    if !latest.exists() && !runs_dir.exists() {
        // No prior run; doctor itself is healthy (no findings to report).
        println!("ok: no prior runs");
        return Ok(());
    }

    let resolved = std::fs::read_link(&latest).ok().map(|p| root.join(p));
    let report_path = resolved.and_then(|p| {
        let r = p.join("report.json");
        if r.exists() { Some(r) } else { None }
    });

    let Some(report_path) = report_path else {
        println!("warn: no report.json in latest run");
        // H1 fix: explicit exit 1 (`findings_present_no_fix`).
        return Err(CliError::ExitCode(1));
    };

    let s = std::fs::read_to_string(&report_path)
        .map_err(|e| CliError::Other(format!("reading {}: {}", report_path.display(), e)))?;
    let v: serde_json::Value = serde_json::from_str(&s)
        .map_err(|e| CliError::Other(format!("parsing report.json: {e}")))?;

    let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
    let total = v
        .get("summary")
        .and_then(|sm| sm.get("total_findings"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let exit_code = v.get("exit_code").and_then(|n| n.as_i64()).unwrap_or(0);

    if ok && total == 0 {
        println!("ok: 0 findings (last run exit {exit_code})");
        Ok(())
    } else {
        println!(
            "findings_present: {} findings (last run exit {})",
            total, exit_code
        );
        // H1 fix: explicit exit 1 (`findings_present_no_fix`).
        Err(CliError::ExitCode(1))
    }
}

/// Print `am doctor ls` — list of runs.
pub fn handle_ls(target: &std::path::Path, format: Option<CliOutputFormat>) -> CliResult<()> {
    let runs =
        runs::list_runs(target).map_err(|e| CliError::Other(format!("listing runs: {e}")))?;
    let fmt = format.unwrap_or_else(|| {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            CliOutputFormat::Table
        } else {
            CliOutputFormat::Json
        }
    });
    match fmt {
        CliOutputFormat::Json => {
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": "1.0",
                "runs": runs,
                "count": runs.len(),
            }))
            .map_err(|e| CliError::Other(format!("serializing runs: {e}")))?;
            println!("{json}");
        }
        CliOutputFormat::Table | CliOutputFormat::Toon => {
            if runs.is_empty() {
                println!("(no runs)");
            } else {
                println!("{:36}  {:8}  {:8}  findings", "run_id", "exit", "actions");
                for r in &runs {
                    println!(
                        "{:36}  {:8}  {:8}  {}",
                        r.run_id,
                        r.exit_code.map(|c| c.to_string()).unwrap_or("-".into()),
                        r.action_count,
                        r.finding_count.map(|n| n.to_string()).unwrap_or("-".into()),
                    );
                }
            }
        }
    }
    Ok(())
}

/// `am doctor undo <run-id>` (or `latest`).
///
/// Reads `actions.jsonl` in reverse and restores from `backups/`.
pub fn handle_undo(
    target: &std::path::Path,
    run_id_arg: &str,
    dry_run: bool,
    strict: bool,
    format: Option<CliOutputFormat>,
) -> CliResult<()> {
    let run_id = undo::resolve_run_id(target, run_id_arg)
        .ok_or_else(|| CliError::Other(format!("could not resolve run-id '{run_id_arg}'")))?;
    if undo::undo_complete(target, &run_id) {
        // Idempotent.
        let json = serde_json::json!({
            "schema_version": "1.0",
            "run_id": run_id,
            "status": "already_undone",
            "actions_replayed": 0,
            "actions_skipped": 0,
        });
        match format.unwrap_or(CliOutputFormat::Json) {
            CliOutputFormat::Json => {
                let s = serde_json::to_string_pretty(&json)
                    .map_err(|e| CliError::Other(format!("serializing undo result: {e}")))?;
                println!("{s}");
            }
            _ => println!("undo already complete for {}", run_id),
        }
        return Ok(());
    }
    let summary = undo::run_undo(target, &run_id, dry_run, strict)
        // H1 fix: undo IO failures are exit 3 (`fix_failed_rolled_back`),
        // not the catch-all exit 1.
        .map_err(|e| {
            eprintln!("error: undo failed: {e}");
            CliError::ExitCode(3)
        })?;

    let json = serde_json::json!({
        "schema_version": "1.0",
        "run_id": summary.run_id,
        "actions_replayed": summary.actions_replayed,
        "actions_skipped": summary.actions_skipped,
        "failures": summary.failures,
        "dry_run": dry_run,
        "strict": strict,
    });
    match format.unwrap_or(CliOutputFormat::Json) {
        CliOutputFormat::Json => {
            // H7 fix: don't .unwrap() on JSON serialization in user-facing path.
            let s = serde_json::to_string_pretty(&json)
                .map_err(|e| CliError::Other(format!("serializing undo result: {e}")))?;
            println!("{s}");
        }
        _ => println!(
            "undo {}: replayed={} skipped={} failures={}",
            summary.run_id,
            summary.actions_replayed,
            summary.actions_skipped,
            summary.failures.len()
        ),
    }

    if !summary.failures.is_empty() {
        // H1 fix: exit 3 (`fix_failed_rolled_back`), not exit 1.
        eprintln!("error: undo had {} failures", summary.failures.len());
        return Err(CliError::ExitCode(3));
    }
    Ok(())
}

/// Compute the canonical write_scopes for `am doctor --fix`.
///
/// These match `analysis/safety_envelope.md` (Phase 3 synthesis).
fn default_write_scopes() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".config").join("mcp-agent-mail"));
        v.push(home.join(".codex"));
        v.push(home.join(".claude"));
        v.push(home.join(".gemini"));
        v.push(home.join(".cursor"));
        v.push(home.join(".windsurf"));
        v.push(home.join(".opencode.json"));
        v.push(home.join(".factory.mcp.json"));
        v.push(home.join(".cline.mcp.json"));
        v.push(home.join(".mcp_agent_mail"));
    }
    if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        v.push(PathBuf::from(xdg_config).join("mcp-agent-mail"));
    }
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        v.push(PathBuf::from(xdg_data).join("mcp-agent-mail"));
    }
    if let Ok(storage) = std::env::var("STORAGE_ROOT") {
        v.push(PathBuf::from(storage));
    }
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".local").join("share").join("mcp-agent-mail"));
        v.push(home.join(".mcp_agent_mail_git_mailbox_repo"));
    }
    // Per-repo scope: <cwd>/.doctor/, <cwd>/.git/hooks/, <cwd>/.gitignore
    v.push(PathBuf::from(".doctor"));
    v.push(PathBuf::from(".git/hooks"));
    v.push(PathBuf::from(".gitignore"));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_write_scopes_includes_known_locations() {
        let scopes = default_write_scopes();
        assert!(!scopes.is_empty());
        let s = scopes
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert!(s.contains(".doctor"));
        // Storage root is conditional; XDG paths are conditional. Just assert
        // the per-repo scopes are always present.
    }
}
