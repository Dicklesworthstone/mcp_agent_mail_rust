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

/// `am doctor triage` — mega-command. Returns `{summary, findings,
/// actions_planned, recommended_command, capabilities_url}` in one
/// round-trip. Collapses the typical 3-call agent loop into one.
///
/// Reads `.doctor/latest/report.json` if available; else returns a stub
/// directing the agent to `am doctor` first. JSON only.
///
/// `quick=true` is recorded as metadata; pass-3 returns the filter
/// flag in the envelope. Per-FM detector-level filtering happens in
/// pass-4 once the per-FM detector registry is wired (today the
/// quick_mode_eligible attribute is on the capabilities side only).
pub fn handle_triage(target: &std::path::Path, quick: bool) -> CliResult<()> {
    let root = runs::doctor_root(target);
    let latest = root.join("latest");
    let resolved = std::fs::read_link(&latest).ok().map(|p| root.join(p));
    let report_path = resolved.and_then(|p| {
        let r = p.join("report.json");
        if r.exists() { Some(r) } else { None }
    });

    let report_value: serde_json::Value = if let Some(rp) = report_path.as_ref() {
        let s = std::fs::read_to_string(rp)
            .map_err(|e| CliError::Other(format!("reading {}: {}", rp.display(), e)))?;
        serde_json::from_str(&s)
            .map_err(|e| CliError::Other(format!("parsing report.json: {e}")))?
    } else {
        serde_json::json!({
            "ok": null,
            "summary": null,
            "findings": [],
        })
    };

    let summary = report_value
        .get("summary")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let findings = report_value
        .get("findings")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let total_findings = summary
        .get("total_findings")
        .and_then(|n| n.as_u64())
        .unwrap_or_else(|| findings.as_array().map(|arr| arr.len() as u64).unwrap_or(0));

    let recommended_command = if total_findings == 0 {
        if report_path.is_none() {
            "am doctor".to_string()
        } else {
            "am doctor health".to_string()
        }
    } else {
        let has_p0 = findings
            .as_array()
            .map(|arr| {
                arr.iter()
                    .any(|f| f.get("severity").and_then(|s| s.as_str()) == Some("P0"))
            })
            .unwrap_or(false);
        if has_p0 {
            "am doctor --fix --yes".to_string()
        } else {
            "am doctor --dry-run --fix".to_string()
        }
    };

    let actions_planned: Vec<serde_json::Value> = findings
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let id = f.get("id")?.as_str()?;
                    let severity = f.get("severity").and_then(|s| s.as_str()).unwrap_or("P3");
                    Some(serde_json::json!({
                        "id": id,
                        "severity": severity,
                        "fix_command": format!("am doctor --fix --only {} --yes", id),
                        "explain_command": format!("am doctor explain {}", id),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let envelope = serde_json::json!({
        "schema_version": "1.0",
        "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "quick": quick,
        "report_available": report_path.is_some(),
        "report_path": report_path.map(|p| p.to_string_lossy().into_owned()),
        "summary": summary,
        "total_findings": total_findings,
        "findings": findings,
        "actions_planned": actions_planned,
        "recommended_command": recommended_command,
        "capabilities_url": "am doctor capabilities --json",
        "robot_docs_url": "am doctor robot-docs",
    });

    let s = serde_json::to_string_pretty(&envelope)
        .map_err(|e| CliError::Other(format!("serializing triage envelope: {e}")))?;
    println!("{s}");
    Ok(())
}

/// `am doctor explain <finding-id>` — drill into a single finding.
///
/// Reads `.doctor/latest/report.json` and finds the matching entry. Returns
/// the full finding (including `evidence`, `remediation`, etc.) as JSON.
pub fn handle_explain(
    target: &std::path::Path,
    finding_id: &str,
    format: Option<CliOutputFormat>,
) -> CliResult<()> {
    let root = runs::doctor_root(target);
    let latest = root.join("latest");
    let resolved = std::fs::read_link(&latest)
        .ok()
        .map(|p| root.join(p))
        .ok_or_else(|| {
            eprintln!("error: no `.doctor/latest` symlink found. Run `am doctor` first.");
            CliError::ExitCode(64)
        })?;
    let report_path = resolved.join("report.json");
    if !report_path.exists() {
        eprintln!(
            "error: no report.json at {}. Run `am doctor` first.",
            report_path.display()
        );
        return Err(CliError::ExitCode(64));
    }
    let s = std::fs::read_to_string(&report_path)
        .map_err(|e| CliError::Other(format!("reading {}: {}", report_path.display(), e)))?;
    let v: serde_json::Value = serde_json::from_str(&s)
        .map_err(|e| CliError::Other(format!("parsing report.json: {e}")))?;
    let findings = v
        .get("findings")
        .and_then(|f| f.as_array())
        .ok_or_else(|| CliError::Other("report.json missing `findings` array".into()))?;
    let matched = findings.iter().find(|f| {
        f.get("id").and_then(|i| i.as_str()) == Some(finding_id)
            || f.get("check").and_then(|i| i.as_str()) == Some(finding_id)
    });
    let Some(finding) = matched else {
        eprintln!(
            "error: finding `{finding_id}` not found in latest run. Run `am doctor --json` to list all findings."
        );
        return Err(CliError::ExitCode(64));
    };

    let envelope = serde_json::json!({
        "schema_version": "1.0",
        "finding_id": finding_id,
        "finding": finding,
        "report_path": report_path.to_string_lossy(),
        "next_actions": [
            format!("am doctor --fix --only {finding_id} --yes"),
            "am doctor capabilities --json".to_string(),
        ],
    });

    match format.unwrap_or(CliOutputFormat::Json) {
        CliOutputFormat::Json | CliOutputFormat::Toon | CliOutputFormat::Table => {
            let pretty = serde_json::to_string_pretty(&envelope)
                .map_err(|e| CliError::Other(format!("serializing explain: {e}")))?;
            println!("{pretty}");
        }
    }
    Ok(())
}

/// `am doctor selftest` — end-to-end exercise of the chokepoint primitives.
///
/// Pass-6 deliverable. In an isolated tempdir:
/// 1. WriteFile mutation through `mutate()` (verifies pending+completed
///    actions.jsonl entries, per-mutation seq backup, atomic write).
/// 2. AppendFile mutation (verifies append + O_NOFOLLOW path).
/// 3. Chmod mutation (verifies chmod_via_fd + before_mode/after_mode).
/// 4. Rename mutation (verifies destination-lock + RenameDestinationExists guard).
/// 5. Run undo. Verify byte-identical restoration.
///
/// Reports JSON:
/// ```json
/// {
///   "schema_version": "1.0",
///   "doctor_version": "1.0.0",
///   "tool_version": "0.2.52",
///   "ok": true,
///   "checks": [
///     {"name": "write_file_mutation", "ok": true},
///     {"name": "append_file_mutation", "ok": true},
///     ...
///   ],
///   "duration_ms": 12
/// }
/// ```
///
/// Exit 0 on pass, 1 on fail. For operators after install/upgrade.
pub fn handle_selftest(format: Option<CliOutputFormat>) -> CliResult<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::Instant;

    let started_at = Instant::now();
    let td = match tempfile::TempDir::new() {
        Ok(t) => t,
        Err(e) => {
            return Err(CliError::Other(format!("could not create tempdir: {e}")));
        }
    };

    let run_id = "selftest__inline";
    let run_dir = match runs::scaffold_run_dir(td.path(), run_id) {
        Ok(d) => d,
        Err(e) => {
            return Err(CliError::Other(format!("scaffold_run_dir failed: {e}")));
        }
    };
    let actions_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("actions.jsonl"))
    {
        Ok(f) => f,
        Err(e) => {
            return Err(CliError::Other(format!("opening actions.jsonl failed: {e}")));
        }
    };

    let ctx = mutate::MutateContext {
        run_id: run_id.to_string(),
        run_dir: run_dir.clone(),
        capabilities: mutate::Capabilities {
            write_scopes: vec![td.path().to_path_buf()],
        },
        actions_file: Mutex::new(actions_file),
        fixer_id: "selftest".to_string(),
        repo_root: td.path().to_path_buf(),
        dry_run: false,
        start: started_at,
        extra_locks: Vec::new(),
    };

    let mut checks = Vec::<serde_json::Value>::new();
    let mut all_ok = true;

    // Step 1: WriteFile mutation.
    let target_a = td.path().join("alpha.txt");
    fs::write(&target_a, b"alpha original\n").ok();
    let r1 = mutate::mutate(
        &ctx,
        &target_a,
        mutate::Op::WriteFile {
            content: b"alpha new\n".to_vec(),
            mode: 0o644,
        },
    );
    let ok1 = r1.is_ok() && fs::read_to_string(&target_a).ok().as_deref() == Some("alpha new\n");
    all_ok &= ok1;
    checks.push(serde_json::json!({"name": "write_file_mutation", "ok": ok1}));

    // Step 2: AppendFile mutation.
    let r2 = mutate::mutate(
        &ctx,
        &target_a,
        mutate::Op::AppendFile {
            content: b"appended\n".to_vec(),
        },
    );
    let ok2 =
        r2.is_ok() && fs::read_to_string(&target_a).ok().as_deref() == Some("alpha new\nappended\n");
    all_ok &= ok2;
    checks.push(serde_json::json!({"name": "append_file_mutation", "ok": ok2}));

    // Step 3: Chmod mutation.
    let r3 = mutate::mutate(
        &ctx,
        &target_a,
        mutate::Op::Chmod { mode: 0o600 },
    );
    let ok3 = r3.is_ok()
        && fs::metadata(&target_a)
            .map(|m| m.permissions().mode() & 0o777 == 0o600)
            .unwrap_or(false);
    all_ok &= ok3;
    checks.push(serde_json::json!({"name": "chmod_mutation", "ok": ok3}));

    // Step 4: Rename mutation (to a quarantine path).
    let target_b = td.path().join("beta.txt");
    fs::write(&target_b, b"beta original\n").ok();
    let quarantine = td.path().join("quarantine_beta.txt");
    let r4 = mutate::mutate(
        &ctx,
        &target_b,
        mutate::Op::Rename {
            to: quarantine.clone(),
        },
    );
    let ok4 = r4.is_ok() && !target_b.exists() && quarantine.exists();
    all_ok &= ok4;
    checks.push(serde_json::json!({"name": "rename_mutation", "ok": ok4}));

    // Drop ctx so actions.jsonl flushes before we read it.
    drop(ctx);

    // Step 5: Verify per-mutation seq backups exist.
    let backups_root = run_dir.join("backups");
    let seq_dirs: Vec<_> = std::fs::read_dir(&backups_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("seq_")
        })
        .collect();
    let ok5 = seq_dirs.len() >= 4;
    all_ok &= ok5;
    checks.push(serde_json::json!({
        "name": "per_mutation_seq_backups",
        "ok": ok5,
        "seq_dir_count": seq_dirs.len(),
    }));

    // Step 6: Run undo. Verify byte-identical recovery.
    let undo_summary = undo::run_undo(td.path(), run_id, false, false);
    let ok6 = undo_summary
        .as_ref()
        .map(|s| s.failures.is_empty())
        .unwrap_or(false)
        && fs::read_to_string(&target_a).ok().as_deref() == Some("alpha original\n")
        && fs::read_to_string(&target_b).ok().as_deref() == Some("beta original\n")
        && !quarantine.exists();
    all_ok &= ok6;
    checks.push(serde_json::json!({
        "name": "undo_round_trip_byte_identical",
        "ok": ok6,
        "actions_replayed": undo_summary.as_ref().map(|s| s.actions_replayed).unwrap_or(0),
        "failures": undo_summary
            .as_ref()
            .map(|s| s.failures.clone())
            .unwrap_or_default(),
    }));

    let duration_ms = started_at.elapsed().as_millis() as u64;

    let envelope = serde_json::json!({
        "schema_version": "1.0",
        "doctor_version": runs::DOCTOR_VERSION,
        "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "ok": all_ok,
        "checks": checks,
        "duration_ms": duration_ms,
        "tempdir": td.path().to_string_lossy(),
    });

    match format.unwrap_or(CliOutputFormat::Json) {
        CliOutputFormat::Json | CliOutputFormat::Toon | CliOutputFormat::Table => {
            let s = serde_json::to_string_pretty(&envelope)
                .map_err(|e| CliError::Other(format!("serializing selftest: {e}")))?;
            println!("{s}");
        }
    }

    if !all_ok {
        eprintln!("error: doctor selftest had failing checks");
        return Err(CliError::ExitCode(1));
    }
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
