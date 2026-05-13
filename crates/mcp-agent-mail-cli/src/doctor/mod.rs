//! World-class `am doctor` surface — the agent-ergonomic upgrade.
//!
//! This module adds the missing world-class verbs to `am doctor`:
//! `capabilities`, `robot-docs`, `undo`, `ls`, `diff`, `gc`, `health`,
//! plus the per-run `.doctor/runs/<run-id>/` artifact layout, the
//! `mutate()` chokepoint, and the agent-ergonomic JSON contract.
//!
//! The existing verbs (`check`, `repair`, `backups`, `restore`,
//! `reconstruct`, `archive-scan`, `archive-verify`, `archive-normalize`, `fix`,
//! `fix-orphan-refs`, `pack-archive`) continue to work while fixers move
//! through the chokepoint.
//!
//! Every public surface here matches CLI-SURFACE.md from the
//! `world-class-doctor-mode-for-cli-tools` skill verbatim. The handbook
//! at `am doctor robot-docs` is the single source of truth for agents.

#![forbid(unsafe_code)]

pub mod capabilities;
pub mod fixers;
pub mod mutate;
pub mod robot_docs;
pub mod runs;
pub mod undo;

use crate::output::CliOutputFormat;
use crate::{CliError, CliResult};
use mcp_agent_mail_core::Config;
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Print `capabilities --json` (or text fallback for `--format toon`).
pub fn handle_capabilities(format: Option<CliOutputFormat>) -> CliResult<()> {
    let tool_version = env!("CARGO_PKG_VERSION").to_string();
    // Existing fixers compute write scopes lazily; expose the canonical set
    // known by the doctor surface.
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
/// `quick=true` is recorded as metadata in the envelope. Detector-level
/// filtering is available once the detector registry is wired; today the
/// `quick_mode_eligible` attribute lives on the capabilities side.
pub fn handle_triage(target: &std::path::Path, quick: bool) -> CliResult<()> {
    let root = runs::doctor_root(target);
    let report_path = latest_doctor_report_path_for_root(&root);

    let report_value: serde_json::Value = if let Some(rp) = report_path.as_ref() {
        read_json_file(rp)?
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
/// Two-stage lookup (pass-23):
/// 1. Try `.doctor/latest/report.json` for a matching finding from the
///    most recent run. If found, emit the full finding (with `evidence`,
///    `remediation`, etc.) in `mode: "latest_run"`.
/// 2. Fall back to `fixers::registry()` lookup. If the id matches a
///    registered FM, emit its static `FixerSpec` (severity, subsystem,
///    `op_pattern`, `auto_fixable`, `source_module`,
///    `one_line_description`) in `mode: "registry"` — useful when no
///    run has happened yet or the FM isn't currently triggering. This
///    keeps `am doctor explain <fm-id>` informative regardless of run
///    history.
/// 3. If neither stage matches, exit 64 with a hint pointing operators
///    at `am doctor fixers` (enumerate registry) and `am doctor --json`
///    (list current findings).
pub fn handle_explain(
    target: &std::path::Path,
    finding_id: &str,
    format: Option<CliOutputFormat>,
) -> CliResult<()> {
    // Stage 1: try the latest-run report. Failures here (no symlink,
    // no report, no matching finding) fall through to stage 2 rather
    // than aborting — silently better UX for `explain` on a registered
    // FM that simply hasn't fired in any run yet.
    let root = runs::doctor_root(target);
    let latest_envelope = latest_doctor_report_path_for_root(&root).and_then(|report_path| {
        let body = std::fs::read_to_string(&report_path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        let findings = v.get("findings")?.as_array()?;
        let matched = findings.iter().find(|f| {
            f.get("id").and_then(|i| i.as_str()) == Some(finding_id)
                || f.get("check").and_then(|i| i.as_str()) == Some(finding_id)
        })?;
        Some(serde_json::json!({
            "schema_version": "1.0",
            "mode": "latest_run",
            "finding_id": finding_id,
            "finding": matched,
            "report_path": report_path.to_string_lossy(),
            "next_actions": [
                format!("am doctor --fix --only {finding_id} --yes"),
                "am doctor capabilities --json".to_string(),
            ],
        }))
    });

    if let Some(envelope) = latest_envelope {
        emit_explain_envelope(&envelope, format)?;
        return Ok(());
    }

    // Stage 2: registry fallback. Useful for `explain <fm-id>` when
    // the FM is registered but hasn't fired in any run.
    let specs = fixers::registry();
    if let Some(spec) = specs.iter().find(|s| s.id == finding_id) {
        let envelope = serde_json::json!({
            "schema_version": "1.0",
            "mode": "registry",
            "finding_id": finding_id,
            "fixer_spec": spec,
            "note": "No matching finding in latest run; showing the FM's static contract from the registry.",
            "next_actions": [
                format!("am doctor fix --only {finding_id} --list --json"),
                format!("am doctor --fix --only {finding_id} --yes"),
                "am doctor fixers --format json".to_string(),
                "am doctor capabilities --json".to_string(),
            ],
        });
        emit_explain_envelope(&envelope, format)?;
        return Ok(());
    }

    // Stage 3: not in latest run, not in registry → truly unknown.
    eprintln!("error: finding `{finding_id}` not found in latest run AND not a registered FM.");
    eprintln!(
        "       Run `am doctor fixers` to enumerate registered FM ids, or `am doctor --json` to list current findings."
    );
    Err(CliError::ExitCode(64))
}

fn emit_explain_envelope(
    envelope: &serde_json::Value,
    format: Option<CliOutputFormat>,
) -> CliResult<()> {
    match format.unwrap_or(CliOutputFormat::Json) {
        CliOutputFormat::Json | CliOutputFormat::Toon | CliOutputFormat::Table => {
            let pretty = serde_json::to_string_pretty(envelope)
                .map_err(|e| CliError::Other(format!("serializing explain: {e}")))?;
            println!("{pretty}");
        }
    }
    Ok(())
}

/// `am doctor selftest` — end-to-end exercise of the chokepoint primitives.
///
/// In an isolated tempdir:
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
/// `am doctor fixers` — pass-14 verb. Lists all registered per-FM
/// detector+fixer pairs in this build with their Op pattern, severity,
/// subsystem, and auto-fixable status.
///
/// JSON output is an array of `FixerSpec` from `fixers::registry()`.
/// Table output is a human-readable table for operator browsing.
pub fn handle_fixers(format: Option<CliOutputFormat>) -> CliResult<()> {
    let specs = fixers::registry();
    let fmt = format.unwrap_or_else(|| {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            CliOutputFormat::Table
        } else {
            CliOutputFormat::Json
        }
    });
    match fmt {
        CliOutputFormat::Json | CliOutputFormat::Toon => {
            let envelope = serde_json::json!({
                "schema_version": "1.0",
                "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
                "tool": "am",
                "tool_version": env!("CARGO_PKG_VERSION"),
                "fixers_count": specs.len(),
                "fixers": specs,
            });
            let s = serde_json::to_string_pretty(&envelope)
                .map_err(|e| CliError::Other(format!("serializing fixers: {e}")))?;
            println!("{s}");
        }
        CliOutputFormat::Table => {
            println!(
                "{:6}  {:9}  {:28}  {:14}  {:6}  FM id",
                "Sev", "Auto-fix", "Subsystem", "Op", "Count"
            );
            println!(
                "{:6}  {:9}  {:28}  {:14}  {:6}  -----",
                "---", "--------", "----------------------------", "--------------", "-----"
            );
            for spec in &specs {
                println!(
                    "{:6}  {:9}  {:28}  {:14}  {:6}  {}",
                    spec.severity,
                    if spec.auto_fixable { "yes" } else { "no" },
                    spec.subsystem,
                    spec.op_pattern,
                    "",
                    spec.id,
                );
                println!(
                    "                                                                              {}",
                    spec.one_line_description
                );
            }
            println!();
            println!("Total: {} FM-level fixers registered", specs.len());
        }
    }
    Ok(())
}

/// `am doctor fix --only <fm-id>` — pass-15 verb.
///
/// Routes a single registered FM through the `mutate()` chokepoint.
/// Validates the id against `fixers::registry()`; unknown ids exit 64
/// with a hint listing valid ids. Builds default `DispatchInputs` from
/// `Config::from_env()` + cwd + the operator's well-known config dirs,
/// scaffolds a `.doctor/runs/<run-id>/` directory, runs the dispatcher,
/// and emits a JSON envelope to stdout. Exit codes follow the doctor
/// contract: 0 (ok), 3 (mutate failed), 4 (refused unsafe / out-of-scope),
/// 64 (unknown id or missing required input).
pub fn handle_fix_only(fm_id: &str, dry_run: bool, yes: bool, _json: bool) -> CliResult<()> {
    use std::sync::Mutex;
    use std::time::Instant;

    let started_at = Instant::now();

    let specs = fixers::registry();
    let Some(spec) = specs.iter().find(|s| s.id == fm_id) else {
        eprintln!("error: unknown FM id `{fm_id}`");
        eprintln!("valid ids (run `am doctor fixers --format json` for the contract):");
        for s in &specs {
            eprintln!("  {} [{}, {}]", s.id, s.severity, s.subsystem);
        }
        return Err(CliError::ExitCode(64));
    };

    if !confirm_mutating_doctor_action_for_only(fm_id, spec.severity, dry_run, yes)? {
        // Suppressing the run on operator decline is *not* an error; emit
        // a structured envelope so wrapper scripts can detect it.
        let envelope = serde_json::json!({
            "schema_version": "1.0",
            "doctor_version": runs::DOCTOR_VERSION,
            "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
            "fm_id": fm_id,
            "skipped": true,
            "reason": "operator declined",
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope)
                .map_err(|e| CliError::Other(format!("serializing fix-only envelope: {e}")))?
        );
        return Ok(());
    }

    let repo_root =
        std::env::current_dir().map_err(|e| CliError::Other(format!("getting cwd: {e}")))?;
    let config = Config::from_env();
    let storage_root = config.storage_root.clone();
    let canonical_mcp_url = canonical_mcp_url_for_config(&config);

    let inputs = fixers::DispatchInputs {
        repo_root: repo_root.clone(),
        archive_roots: enumerate_archive_roots(&storage_root),
        pid_hint_candidates: default_listener_pid_candidates(&storage_root),
        token_backup_candidates: default_token_backup_candidates(&storage_root),
        mcp_config_candidates: default_mcp_config_candidates(),
        canonical_mcp_url: Some(canonical_mcp_url),
        git_detect: build_git_detect_inputs(),
        gitignore_target: Some(repo_root.join(".gitignore")),
        db_file_candidates: default_db_file_candidates(),
        doctor_latest_target: Some(runs::doctor_root(&repo_root).join("latest")),
        // None → each FM falls back to its own canonical DEFAULT_STALE_SECONDS.
        stale_seconds_override: None,
    };

    let run_id = format!(
        "{}__only_{}",
        runs::now_iso_seconds(),
        short_run_suffix(fm_id),
    );
    let run_dir = if dry_run {
        runs::doctor_root(&repo_root).join("dry-run").join(&run_id)
    } else {
        runs::scaffold_run_dir(&repo_root, &run_id)
            .map_err(|e| CliError::Other(format!("scaffolding run dir: {e}")))?
    };
    // Pass-22: the bypassing call to `runs::ensure_gitignore_entry` that
    // used to live here is gone. The pass-21 FM
    // `fm-archive-state-files-missing-doctor-gitignore-entry` now owns
    // that mutation. Operators invoke it explicitly via
    // `am doctor fix --only <id>` and get the full chokepoint
    // guarantees (verbatim backup, hash-witnessed action, reversible
    // via `am doctor undo`). Doing it here would silently mutate
    // `.gitignore` on every unrelated --only run and the change
    // wouldn't be undone by `am doctor undo` of that run-id.
    let actions_file = if dry_run {
        tempfile::tempfile()
            .map_err(|e| CliError::Other(format!("creating dry-run actions sink: {e}")))?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .map_err(|e| CliError::Other(format!("opening actions.jsonl: {e}")))?
    };

    let mut write_scopes = default_write_scopes();
    write_scopes.push(repo_root.clone());
    write_scopes.push(run_dir.clone());

    let ctx = mutate::MutateContext {
        run_id: run_id.clone(),
        run_dir: run_dir.clone(),
        capabilities: mutate::Capabilities { write_scopes },
        actions_file: Mutex::new(actions_file),
        fixer_id: fm_id.to_string(),
        repo_root: repo_root.clone(),
        dry_run,
        start: started_at,
        extra_locks: Vec::new(),
    };

    let outcome = match fixers::dispatch_only(fm_id, &ctx, &inputs) {
        Ok(o) => o,
        Err(fixers::DispatchError::UnknownFm(id)) => {
            // Registry-validated above, so this is genuinely impossible.
            eprintln!("error: dispatcher reported unknown FM id `{id}` after registry check");
            return Err(CliError::ExitCode(64));
        }
        Err(fixers::DispatchError::MissingInput { fm_id, field }) => {
            eprintln!("error: required input `{field}` missing for FM `{fm_id}`");
            return Err(CliError::ExitCode(64));
        }
        Err(fixers::DispatchError::Mutate(me)) => {
            eprintln!("error: mutate() refused or failed for `{fm_id}`: {me}");
            // `OutOfScope` and `RenameDestinationExists` map to 4 (refused unsafe);
            // everything else maps to 3 (fix failed, possibly rolled back).
            let code = match me {
                mutate::MutateError::OutOfScope(_)
                | mutate::MutateError::RenameDestinationExists(_) => 4,
                _ => 3,
            };
            return Err(CliError::ExitCode(code));
        }
    };

    // Pass-16: in dry-run the dispatcher's `actions_taken` is actually
    // "planned" (chokepoint returned success without writing). Surface
    // both fields explicitly so JSON consumers can pick the right one
    // without needing to inspect `dry_run` first.
    let (actions_taken, actions_planned) = if dry_run {
        (0_usize, outcome.actions_taken)
    } else {
        (outcome.actions_taken, outcome.actions_taken)
    };

    let post_detect = if !dry_run && outcome.actions_taken > 0 {
        match fixers::detect_only(fm_id, &inputs) {
            Ok(detected) => Some(detected),
            Err(err) => {
                eprintln!("warning: post-fix detection failed for `{fm_id}`: {err}");
                None
            }
        }
    } else {
        None
    };
    let remaining_findings = post_detect
        .as_ref()
        .map(|detected| detected.findings_count)
        .unwrap_or(outcome.findings_count);
    let ok = !dry_run && remaining_findings == 0 && outcome.actions_skipped == 0;

    let run_dir_json = if dry_run {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(run_dir.to_string_lossy().into_owned())
    };

    let envelope = serde_json::json!({
        "schema_version": "1.0",
        "doctor_version": runs::DOCTOR_VERSION,
        "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "ok": ok,
        "exit_code": if ok { 0 } else { 1 },
        "fm_id": fm_id,
        "severity": spec.severity,
        "subsystem": spec.subsystem,
        "op_pattern": spec.op_pattern,
        "mode": if dry_run { "dry-run" } else { "fix" },
        "dry_run": dry_run,
        "run_id": run_id,
        "run_dir": run_dir_json,
        "duration_ms": started_at.elapsed().as_millis() as u64,
        "actions_taken": actions_taken,
        "actions_planned": actions_planned,
        "summary": {
            "total_findings": remaining_findings,
            "initial_findings": outcome.findings_count,
            "actions_taken": actions_taken,
            "actions_skipped": outcome.actions_skipped,
        },
        "post_fix": post_detect,
        "outcome": outcome,
    });

    if !dry_run && actions_taken > 0 {
        runs::write_run_artifacts(&run_dir, &run_id, &envelope)
            .map_err(|e| CliError::Other(format!("writing doctor run artifacts: {e}")))?;
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&envelope)
            .map_err(|e| CliError::Other(format!("serializing fix-only envelope: {e}")))?
    );

    if !dry_run && outcome.actions_taken > 0 {
        runs::update_latest_symlink(&repo_root, &run_id).ok();
    }
    Ok(())
}

/// Pass-16 verb: `am doctor fix --only <fm-id> --list`.
///
/// Pure-detection variant — runs the FM's detector and prints a JSON
/// envelope of `findings[]` + `actions_planned` without touching the
/// `mutate()` chokepoint. No run-dir is scaffolded, no `actions.jsonl`
/// is written, no advisory locks are taken. Exit codes match
/// `handle_fix_only` for usage errors (64 for unknown id / missing
/// input); the success path always exits 0 — findings ≠ failure.
pub fn handle_fix_only_list(fm_id: &str, _json: bool) -> CliResult<()> {
    use std::time::Instant;

    let started_at = Instant::now();
    let specs = fixers::registry();
    let Some(spec) = specs.iter().find(|s| s.id == fm_id) else {
        eprintln!("error: unknown FM id `{fm_id}`");
        eprintln!("valid ids (run `am doctor fixers --format json` for the contract):");
        for s in &specs {
            eprintln!("  {} [{}, {}]", s.id, s.severity, s.subsystem);
        }
        return Err(CliError::ExitCode(64));
    };

    let repo_root =
        std::env::current_dir().map_err(|e| CliError::Other(format!("getting cwd: {e}")))?;
    let config = Config::from_env();
    let storage_root = config.storage_root.clone();
    let canonical_mcp_url = canonical_mcp_url_for_config(&config);

    let inputs = fixers::DispatchInputs {
        repo_root: repo_root.clone(),
        archive_roots: enumerate_archive_roots(&storage_root),
        pid_hint_candidates: default_listener_pid_candidates(&storage_root),
        token_backup_candidates: default_token_backup_candidates(&storage_root),
        mcp_config_candidates: default_mcp_config_candidates(),
        canonical_mcp_url: Some(canonical_mcp_url),
        git_detect: build_git_detect_inputs(),
        gitignore_target: Some(repo_root.join(".gitignore")),
        db_file_candidates: default_db_file_candidates(),
        doctor_latest_target: Some(runs::doctor_root(&repo_root).join("latest")),
        // None → each FM falls back to its own canonical DEFAULT_STALE_SECONDS.
        stale_seconds_override: None,
    };

    let outcome = match fixers::detect_only(fm_id, &inputs) {
        Ok(o) => o,
        Err(fixers::DispatchError::UnknownFm(id)) => {
            eprintln!("error: detect_only reported unknown FM id `{id}` after registry check");
            return Err(CliError::ExitCode(64));
        }
        Err(fixers::DispatchError::MissingInput { fm_id, field }) => {
            eprintln!("error: required input `{field}` missing for FM `{fm_id}`");
            return Err(CliError::ExitCode(64));
        }
        Err(fixers::DispatchError::Mutate(me)) => {
            // detect_only doesn't call mutate(), so this is structurally
            // impossible. Treat as an internal invariant violation.
            eprintln!("error: internal — detect_only surfaced a MutateError: {me}");
            return Err(CliError::ExitCode(1));
        }
    };

    let envelope = serde_json::json!({
        "schema_version": "1.0",
        "doctor_version": runs::DOCTOR_VERSION,
        "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "fm_id": fm_id,
        "severity": spec.severity,
        "subsystem": spec.subsystem,
        "op_pattern": spec.op_pattern,
        "mode": "list",
        "duration_ms": started_at.elapsed().as_millis() as u64,
        "findings_count": outcome.findings_count,
        "actions_planned": outcome.actions_planned,
        "findings": outcome.findings,
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&envelope)
            .map_err(|e| CliError::Other(format!("serializing fix-only-list envelope: {e}")))?
    );
    Ok(())
}

/// Pass-24 verb: `am doctor fix --list` (without `--only`).
///
/// Single agent-visible "what's broken across the entire FM surface"
/// call. Iterates `fixers::registry()`, runs each FM's detector via
/// `fixers::detect_only`, and emits a combined JSON envelope without
/// touching the `mutate()` chokepoint at all (no run-dir, no
/// actions.jsonl, no advisory locks).
///
/// FMs whose detector hits a `MissingInput` (e.g., `git_detect` for
/// the known-bad-git FM when `git` isn't on PATH) are recorded in
/// the envelope's `skipped[]` array with the missing field name —
/// agents can decide whether the missing input is recoverable.
///
/// Exit 0 on success regardless of finding count (findings ≠
/// failure). Exit 1 only on internal serialization error.
pub fn handle_fix_list_all(_json: bool) -> CliResult<()> {
    use std::time::Instant;

    let started_at = Instant::now();
    let repo_root =
        std::env::current_dir().map_err(|e| CliError::Other(format!("getting cwd: {e}")))?;
    let config = Config::from_env();
    let storage_root = config.storage_root.clone();
    let canonical_mcp_url = canonical_mcp_url_for_config(&config);

    let inputs = fixers::DispatchInputs {
        repo_root: repo_root.clone(),
        archive_roots: enumerate_archive_roots(&storage_root),
        pid_hint_candidates: default_listener_pid_candidates(&storage_root),
        token_backup_candidates: default_token_backup_candidates(&storage_root),
        mcp_config_candidates: default_mcp_config_candidates(),
        canonical_mcp_url: Some(canonical_mcp_url),
        git_detect: build_git_detect_inputs(),
        gitignore_target: Some(repo_root.join(".gitignore")),
        db_file_candidates: default_db_file_candidates(),
        doctor_latest_target: Some(runs::doctor_root(&repo_root).join("latest")),
        stale_seconds_override: None,
    };

    let outcome = match fixers::detect_all(&inputs) {
        Ok(o) => o,
        Err(fixers::DispatchError::Mutate(me)) => {
            // detect_all only calls detect_only(), so this is an
            // internal invariant violation rather than user input.
            eprintln!("error: internal — detect_all surfaced a MutateError: {me}");
            return Err(CliError::ExitCode(1));
        }
        Err(fixers::DispatchError::UnknownFm(id)) => {
            eprintln!("error: internal — registry id was not recognized: {id}");
            return Err(CliError::ExitCode(1));
        }
        Err(fixers::DispatchError::MissingInput { fm_id, field }) => {
            eprintln!("error: internal — unaggregated missing input `{field}` for FM `{fm_id}`");
            return Err(CliError::ExitCode(1));
        }
    };

    let envelope = serde_json::json!({
        "schema_version": "1.0",
        "doctor_version": runs::DOCTOR_VERSION,
        "doctor_contract_version": runs::DOCTOR_CONTRACT_VERSION,
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "mode": "list_all",
        "duration_ms": started_at.elapsed().as_millis() as u64,
        "fm_count": outcome.fm_count,
        "total_findings": outcome.total_findings,
        "total_actions_planned": outcome.total_actions_planned,
        "per_fm": outcome.per_fm,
        "skipped": outcome.skipped,
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&envelope)
            .map_err(|e| CliError::Other(format!("serializing list_all envelope: {e}")))?
    );
    Ok(())
}

/// Prompt-or-bypass helper for `handle_fix_only`. Lifted from
/// `confirm_mutating_doctor_action` in lib.rs so we don't have to
/// expose its internals; matches the same semantics.
fn confirm_mutating_doctor_action_for_only(
    fm_id: &str,
    severity: &str,
    dry_run: bool,
    yes: bool,
) -> CliResult<bool> {
    let prompt = format!(
        "Proceed with `am doctor fix --only {fm_id}` (severity {severity})? This routes mutations through the chokepoint and is reversible via `am doctor undo`.",
    );
    crate::confirm_mutating_doctor_action(&prompt, dry_run, yes)
}

/// One level of children of `<storage_root>/projects/` containing a `.git/`.
fn enumerate_archive_roots(storage_root: &Path) -> Vec<PathBuf> {
    let projects = storage_root.join("projects");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&projects) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(".git").exists() {
            out.push(path);
        }
    }
    out
}

/// Common listener.pid hint locations.
fn default_listener_pid_candidates(storage_root: &Path) -> Vec<PathBuf> {
    let mut v = vec![storage_root.join("listener.pid")];
    if let Some(home) = dirs::home_dir() {
        v.push(
            home.join(".local")
                .join("share")
                .join("mcp-agent-mail")
                .join("listener.pid"),
        );
        v.push(home.join(".mcp_agent_mail").join("listener.pid"));
    }
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        v.push(
            PathBuf::from(xdg_state)
                .join("mcp-agent-mail")
                .join("listener.pid"),
        );
    }
    v
}

/// Top-level (non-recursive) backup-suffixed files under the operator's
/// storage root, Agent Mail config dirs, and common MCP client config
/// dirs. Top-level only — recursion is intentionally avoided to keep
/// latency bounded.
///
/// The accepted suffix set is the canonical
/// `fixers::world_readable_token_bak::BACKUP_SUFFIX_HINTS` — referencing
/// the module's `pub const` directly (instead of duplicating the list)
/// keeps the enumeration here structurally aligned with the detector's
/// accept-set. If the detector broadens the accept-set, this enumeration
/// picks it up automatically.
fn token_backup_candidates(storage_root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![storage_root.to_path_buf()];
    if let Some(home) = home {
        roots.push(home.join(".config").join("mcp-agent-mail"));
        roots.push(home.join(".mcp_agent_mail"));
        roots.push(home.join(".codex"));
        roots.push(home.join(".claude"));
        roots.push(home.join(".cursor"));
        roots.push(home.join(".windsurf"));
        roots.push(home.join(".gemini"));
    }
    let suffixes = fixers::world_readable_token_bak::BACKUP_SUFFIX_HINTS;
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if suffixes.iter().any(|s| name.ends_with(s)) {
                out.push(p);
            }
        }
    }
    out
}

/// Resolve the canonical SQLite DB file path from
/// `DbPoolConfig::from_env().database_url`. Returns an empty list for
/// `:memory:` URLs or anything we can't parse as a filesystem path.
///
/// Accepts `sqlite:///abs/path/db.sqlite3`,
/// `sqlite:///./relative/db.sqlite3`,
/// `sqlite+aiosqlite:///./path` (legacy Python alias),
/// and bare absolute paths.
fn default_db_file_candidates() -> Vec<PathBuf> {
    let url = mcp_agent_mail_db::DbPoolConfig::from_env().database_url;
    if url == ":memory:" || url.ends_with("/:memory:") {
        return Vec::new();
    }
    // Strip the scheme: `sqlite:///`, `sqlite+aiosqlite:///`, etc.
    let path_str = if let Some(rest) = url.strip_prefix("sqlite+aiosqlite:///") {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix("sqlite:///") {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix("sqlite://") {
        // Unusual shape but tolerate.
        rest.to_string()
    } else {
        url.clone()
    };
    if path_str.is_empty() {
        return Vec::new();
    }
    let path = PathBuf::from(path_str);
    if path.exists() {
        vec![path]
    } else {
        // Not a real file (may be `:memory:` in disguise, or the DB
        // hasn't been created yet). Skip rather than emit a spurious
        // candidate.
        Vec::new()
    }
}

fn default_token_backup_candidates(storage_root: &Path) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    token_backup_candidates(storage_root, home.as_deref())
}

/// Common MCP client JSON config paths (per-client, no recursion).
fn default_mcp_config_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".claude").join(".mcp.json"));
        v.push(home.join(".cursor").join("mcp.json"));
        v.push(home.join(".windsurf").join("mcp_config.json"));
        v.push(home.join(".codex").join("mcp.json"));
        v.push(home.join(".gemini").join("settings.json"));
        v.push(home.join(".opencode.json"));
        v.push(home.join(".factory.mcp.json"));
        v.push(home.join(".cline.mcp.json"));
    }
    v
}

fn canonical_mcp_url_for_config(config: &Config) -> String {
    crate::check_inbox_server_url(&config.http_host, config.http_port, &config.http_path)
}

/// Shell out to `git --version`, read `AM_GIT_BINARY`. Returns `None` if
/// `git` isn't on PATH (the known-bad-git FM is unreachable in that case).
fn build_git_detect_inputs() -> Option<fixers::known_bad_git_no_override::DetectInputs> {
    use std::process::Command;
    let which_git = Command::new("sh")
        .arg("-c")
        .arg("command -v git")
        .output()
        .ok()?;
    if !which_git.status.success() {
        return None;
    }
    let system_git_path = PathBuf::from(
        String::from_utf8_lossy(&which_git.stdout)
            .trim()
            .to_string(),
    );
    let out = Command::new(&system_git_path)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let system_git_version = git_version_text_from_stdout(&raw);
    let am_git_binary_env = std::env::var("AM_GIT_BINARY").ok();
    let am_git_binary_version = am_git_binary_env
        .as_deref()
        .and_then(|path| Command::new(path).arg("--version").output().ok())
        .filter(|output| output.status.success())
        .map(|output| git_version_text_from_stdout(&String::from_utf8_lossy(&output.stdout)));
    Some(fixers::known_bad_git_no_override::DetectInputs {
        system_git_path,
        system_git_version,
        am_git_binary_env,
        am_git_binary_version,
    })
}

fn git_version_text_from_stdout(raw: &str) -> String {
    raw.trim()
        .strip_prefix("git version ")
        .unwrap_or(raw.trim())
        .to_string()
}

/// 6-char hex suffix derived from the FM id; keeps run-ids unique when
/// the same FM is invoked multiple times in the same wall-clock second.
fn short_run_suffix(fm_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(fm_id.as_bytes());
    h.update(b"\0");
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(now_ns.to_le_bytes());
    let digest = h.finalize();
    (0..3).map(|i| format!("{:02x}", digest[i])).collect()
}

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
            return Err(CliError::Other(format!(
                "opening actions.jsonl failed: {e}"
            )));
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
    let ok2 = r2.is_ok()
        && fs::read_to_string(&target_a).ok().as_deref() == Some("alpha new\nappended\n");
    all_ok &= ok2;
    checks.push(serde_json::json!({"name": "append_file_mutation", "ok": ok2}));

    // Step 3: Chmod mutation.
    let r3 = mutate::mutate(&ctx, &target_a, mutate::Op::Chmod { mode: 0o600 });
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
        .filter(|e| e.file_name().to_string_lossy().starts_with("seq_"))
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

#[derive(Debug, Clone)]
pub(crate) struct SupportBundleOptions {
    pub(crate) output_dir: Option<PathBuf>,
    pub(crate) stdout_log: Option<PathBuf>,
    pub(crate) stderr_log: Option<PathBuf>,
    pub(crate) redact_subjects: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SupportBundleResult {
    pub(crate) schema_version: &'static str,
    pub(crate) bundle_kind: &'static str,
    pub(crate) bundle_path: String,
    pub(crate) manifest_path: String,
    pub(crate) summary_path: String,
    pub(crate) file_count: usize,
    pub(crate) current_recovery_decision: String,
    pub(crate) observed_recovery_command: Option<String>,
}

#[derive(Debug)]
struct SupportRedactionContext {
    storage_root: PathBuf,
    database_path: PathBuf,
    redact_subjects: bool,
}

/// Build a sanitized, shareable incident bundle for mailbox startup/recovery
/// incidents. Unlike the raw forensic bundle captured by repair/reconstruct,
/// this command never copies SQLite databases, message bodies, or attachment
/// contents.
pub fn handle_support_bundle(
    output_dir: Option<PathBuf>,
    stdout_log: Option<PathBuf>,
    stderr_log: Option<PathBuf>,
    redact_subjects: bool,
    format: Option<CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let config = Config::from_env();
    let database_url = mcp_agent_mail_db::DbPoolConfig::from_env().database_url;
    let result = create_support_bundle(
        &config,
        &database_url,
        SupportBundleOptions {
            output_dir,
            stdout_log,
            stderr_log,
            redact_subjects,
        },
    )?;

    let format = if json {
        CliOutputFormat::Json
    } else {
        format.unwrap_or(CliOutputFormat::Table)
    };
    match format {
        CliOutputFormat::Json => {
            let body = serde_json::to_string_pretty(&result)
                .map_err(|err| CliError::Other(format!("serializing support bundle: {err}")))?;
            println!("{body}");
        }
        CliOutputFormat::Table | CliOutputFormat::Toon => {
            println!("Support bundle: {}", result.bundle_path);
            println!("Manifest: {}", result.manifest_path);
            println!(
                "Decision: current={} observed={}",
                result.current_recovery_decision,
                result
                    .observed_recovery_command
                    .as_deref()
                    .unwrap_or("unknown")
            );
        }
    }
    Ok(())
}

pub(crate) fn create_support_bundle(
    config: &Config,
    database_url: &str,
    options: SupportBundleOptions,
) -> CliResult<SupportBundleResult> {
    let database_path = crate::resolve_mailbox_activity_sqlite_path(database_url)
        .unwrap_or_else(|_| PathBuf::from("<unresolved-database-path>"));
    let redaction = SupportRedactionContext {
        storage_root: config.storage_root.clone(),
        database_path: database_path.clone(),
        redact_subjects: options.redact_subjects,
    };

    let parent = options
        .output_dir
        .clone()
        .unwrap_or_else(|| config.storage_root.join("doctor").join("support-bundles"));
    reject_symlink_ancestor(&parent, "support bundle output root")?;
    fs::create_dir_all(&parent)?;
    reject_symlink_ancestor(&parent, "support bundle output root")?;

    let bundle_name = format!(
        "support-bundle-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        std::process::id()
    );
    let bundle_dir = parent.join(bundle_name);
    reject_symlink_ancestor(&bundle_dir, "support bundle directory")?;
    if bundle_dir.exists() {
        return Err(CliError::Other(format!(
            "support bundle destination already exists: {}",
            bundle_dir.display()
        )));
    }
    fs::create_dir(&bundle_dir)?;

    let mut files = Vec::<serde_json::Value>::new();
    let mut omitted = Vec::<serde_json::Value>::new();

    let decision = support_recovery_decision(database_url, &config.storage_root, &redaction);
    let sidecars = support_sqlite_sidecar_metadata(&database_path);
    let latest_forensic = latest_forensic_manifest(&config.storage_root);
    let observed_recovery_command = latest_forensic
        .as_ref()
        .and_then(|path| read_json_file(path).ok())
        .and_then(|value| {
            value
                .get("command")
                .and_then(|command| command.as_str())
                .map(|command| redact_support_text(command, &redaction))
        });

    if let Some(path) = latest_forensic.as_ref() {
        if let Ok(value) = read_json_file(path) {
            let sanitized = redact_support_json(value, &redaction, None);
            write_support_json_file(
                &bundle_dir,
                "reports/latest-forensic-manifest.json",
                &sanitized,
                "sanitized_metadata",
                "raw_forensic_manifest",
                &mut files,
            )?;
        } else {
            omitted.push(serde_json::json!({
                "source": "latest forensic manifest",
                "source_path_class": "raw_forensic_manifest",
                "reason": "unreadable",
            }));
        }

        let summary = path.parent().map(|dir| dir.join("summary.json"));
        if let Some(summary_path) = summary
            && summary_path.exists()
        {
            if let Ok(value) = read_json_file(&summary_path) {
                let sanitized = redact_support_json(value, &redaction, None);
                write_support_json_file(
                    &bundle_dir,
                    "reports/latest-forensic-summary.json",
                    &sanitized,
                    "sanitized_metadata",
                    "raw_forensic_summary",
                    &mut files,
                )?;
            } else {
                omitted.push(serde_json::json!({
                    "source": "latest forensic summary",
                    "source_path_class": "raw_forensic_summary",
                    "reason": "unreadable",
                }));
            }
        }
    } else {
        omitted.push(serde_json::json!({
            "source": "doctor forensic manifest",
            "source_path_class": "raw_forensic_manifest",
            "reason": "no repair/reconstruct forensic bundle found",
        }));
    }

    if let Some(report_path) = latest_doctor_report_path()
        && report_path.exists()
    {
        if let Ok(value) = read_json_file(&report_path) {
            let sanitized = redact_support_json(value, &redaction, None);
            write_support_json_file(
                &bundle_dir,
                "reports/latest-doctor-report.json",
                &sanitized,
                "sanitized_metadata",
                "doctor_run_report",
                &mut files,
            )?;
        } else {
            omitted.push(serde_json::json!({
                "source": "latest doctor report",
                "source_path_class": "doctor_run_report",
                "reason": "unreadable",
            }));
        }
    }

    if let Some(path) = options.stdout_log.as_ref() {
        write_redacted_operator_log(&bundle_dir, "logs/stdout.log", path, &redaction, &mut files)?;
    } else {
        omitted.push(serde_json::json!({
            "source": "stdout log",
            "source_path_class": "operator_supplied_log",
            "reason": "not supplied; pass --stdout-log",
        }));
    }
    if let Some(path) = options.stderr_log.as_ref() {
        write_redacted_operator_log(&bundle_dir, "logs/stderr.log", path, &redaction, &mut files)?;
    } else {
        omitted.push(serde_json::json!({
            "source": "stderr log",
            "source_path_class": "operator_supplied_log",
            "reason": "not supplied; pass --stderr-log",
        }));
    }

    omitted.push(serde_json::json!({
        "source": "SQLite database and sidecars",
        "source_path_class": "local_database_file",
        "reason": "raw mailbox data; use the raw forensic bundle only for local encrypted escalation",
    }));
    omitted.push(serde_json::json!({
        "source": "message bodies and canonical message files",
        "source_path_class": "mail_archive_content",
        "reason": "private message content is excluded by default",
    }));
    omitted.push(serde_json::json!({
        "source": "attachment contents and attachment filenames",
        "source_path_class": "mail_attachment_content",
        "reason": "attachment data and names are redacted by default",
    }));

    let replay_commands = support_replay_commands();
    let summary = serde_json::json!({
        "schema_version": "1.0",
        "bundle_kind": "doctor_support_bundle",
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "config_shape": {
            "database_url": redact_support_text(database_url, &redaction),
            "database_path": redact_support_text(&database_path.display().to_string(), &redaction),
            "storage_root": redact_support_text(&config.storage_root.display().to_string(), &redaction),
            "http_host": config.http_host,
            "http_port": config.http_port,
            "http_path": config.http_path,
            "http_auth": if config.http_bearer_token.is_some() { "configured" } else { "not_configured" },
            "tui_enabled": config.tui_enabled,
            "interface_mode": std::env::var("AM_INTERFACE_MODE").unwrap_or_else(|_| "unset".to_string()),
        },
        "database": {
            "recovery_decision": decision,
            "sidecars": sidecars,
            "schema_versions": support_schema_versions(database_url, &redaction),
        },
        "latest_forensic": {
            "manifest_found": latest_forensic.is_some(),
            "observed_recovery_command": observed_recovery_command,
        },
        "redaction": support_redaction_policy(options.redact_subjects),
        "replay_commands": replay_commands,
    });
    write_support_json_file(
        &bundle_dir,
        "summary.json",
        &summary,
        "sanitized_metadata",
        "generated",
        &mut files,
    )?;

    write_support_text_file(
        &bundle_dir,
        "README.md",
        support_bundle_readme(),
        "generated_public_guidance",
        "generated",
        &mut files,
    )?;

    let current_recovery_decision = summary["database"]["recovery_decision"]["decision"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let observed_recovery_command = summary["latest_forensic"]["observed_recovery_command"]
        .as_str()
        .map(ToString::to_string);

    let mut manifest_files = files.clone();
    manifest_files.push(serde_json::json!({
        "path": "manifest.json",
        "redaction_mode": "sanitized_metadata",
        "source_path_class": "generated",
        "bytes": "self",
    }));
    let manifest = serde_json::json!({
        "schema_version": "1.0",
        "bundle_kind": "doctor_support_bundle",
        "tool": "am",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "command": {
            "name": "am doctor support-bundle",
            "args": {
                "redact_subjects": options.redact_subjects,
                "stdout_log_supplied": options.stdout_log.is_some(),
                "stderr_log_supplied": options.stderr_log.is_some(),
            },
        },
        "current_recovery_decision": current_recovery_decision,
        "observed_recovery_command": observed_recovery_command,
        "source_path_classes": {
            "generated": "created by support-bundle",
            "local_database_file": "local SQLite path; raw file omitted",
            "raw_forensic_manifest": "doctor repair/reconstruct forensic manifest; sanitized copy only",
            "raw_forensic_summary": "doctor repair/reconstruct forensic summary; sanitized copy only",
            "doctor_run_report": "latest .doctor report; sanitized copy only",
            "operator_supplied_log": "operator-provided stdout/stderr log; redacted and truncated",
            "mail_archive_content": "message archive content; omitted",
            "mail_attachment_content": "attachment content or filename; omitted",
        },
        "redaction": support_redaction_policy(options.redact_subjects),
        "files": manifest_files,
        "omitted": omitted,
        "replay_commands": support_replay_commands(),
        "safe_sharing_limits": [
            "This bundle is designed for maintainer triage, not public posting.",
            "Raw SQLite files, canonical message files, message bodies, and attachments are omitted.",
            "Review the manifest before sharing; paths and secrets are redacted best-effort.",
        ],
    });
    write_support_json_exact(&bundle_dir.join("manifest.json"), &manifest)?;

    Ok(SupportBundleResult {
        schema_version: "1.0",
        bundle_kind: "doctor_support_bundle",
        bundle_path: bundle_dir.display().to_string(),
        manifest_path: bundle_dir.join("manifest.json").display().to_string(),
        summary_path: bundle_dir.join("summary.json").display().to_string(),
        file_count: files.len() + 1,
        current_recovery_decision,
        observed_recovery_command,
    })
}

fn support_recovery_decision(
    database_url: &str,
    storage_root: &Path,
    redaction: &SupportRedactionContext,
) -> serde_json::Value {
    match crate::doctor_database_fix_strategy(database_url, storage_root) {
        Ok(crate::DoctorDatabaseFixStrategy::None(detail)) => serde_json::json!({
            "decision": "none",
            "detail": redact_support_text(&detail, redaction),
        }),
        Ok(crate::DoctorDatabaseFixStrategy::Repair(detail)) => serde_json::json!({
            "decision": "repair",
            "detail": redact_support_text(&detail, redaction),
        }),
        Ok(crate::DoctorDatabaseFixStrategy::Reconstruct(detail)) => serde_json::json!({
            "decision": "reconstruct",
            "detail": redact_support_text(&detail, redaction),
        }),
        Err(err) => serde_json::json!({
            "decision": "unavailable",
            "detail": redact_support_text(&err.to_string(), redaction),
        }),
    }
}

fn support_schema_versions(
    database_url: &str,
    redaction: &SupportRedactionContext,
) -> serde_json::Value {
    let conn = match crate::open_db_for_doctor_check(database_url) {
        Ok(conn) => conn,
        Err(err) => {
            return serde_json::json!({
                "status": "unavailable",
                "detail": redact_support_text(&err.to_string(), redaction),
            });
        }
    };
    let user_version = conn
        .query_sync("PRAGMA user_version", &[])
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|row| row.get_named::<i64>("user_version").ok())
        });
    let sqlite_version = conn
        .query_sync("SELECT sqlite_version() AS sqlite_version", &[])
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|row| row.get_named::<String>("sqlite_version").ok())
        });
    serde_json::json!({
        "status": "captured",
        "database_user_version": user_version,
        "sqlite_version": sqlite_version,
    })
}

fn support_sqlite_sidecar_metadata(database_path: &Path) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (kind, path) in [
        ("db", database_path.to_path_buf()),
        (
            "wal",
            PathBuf::from(format!("{}-wal", database_path.display())),
        ),
        (
            "shm",
            PathBuf::from(format!("{}-shm", database_path.display())),
        ),
        (
            "journal",
            PathBuf::from(format!("{}-journal", database_path.display())),
        ),
    ] {
        let value = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => serde_json::json!({
                "status": "omitted",
                "reason": "symlink refused",
            }),
            Ok(meta) => serde_json::json!({
                "status": "present",
                "bytes": meta.len(),
                "readonly": meta.permissions().readonly(),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => serde_json::json!({
                "status": "missing",
            }),
            Err(err) => serde_json::json!({
                "status": "unavailable",
                "detail": err.to_string(),
            }),
        };
        map.insert(kind.to_string(), value);
    }
    serde_json::Value::Object(map)
}

fn support_redaction_policy(redact_subjects: bool) -> serde_json::Value {
    serde_json::json!({
        "mode": "support_bundle_sanitized",
        "database_url": "credentials_redacted",
        "auth_tokens": "redacted",
        "env_secrets": "redacted",
        "home_paths": "redacted",
        "storage_and_database_paths": "redacted",
        "message_bodies": "redacted_or_omitted",
        "subjects": if redact_subjects { "redacted" } else { "preserved" },
        "attachments": "contents_and_names_redacted_or_omitted",
        "raw_sqlite": "omitted",
    })
}

fn support_replay_commands() -> Vec<&'static str> {
    vec![
        "am doctor check --json",
        "am doctor repair --dry-run",
        "am doctor reconstruct --dry-run --json",
        "am doctor support-bundle --json",
    ]
}

fn support_bundle_readme() -> &'static str {
    "# MCP Agent Mail Doctor Support Bundle\n\n\
This directory is a sanitized incident bundle for maintainer triage.\n\n\
Safe-sharing limits:\n\n\
- Raw SQLite databases, WAL/SHM/journal sidecars, canonical message files, message bodies, and attachments are not included.\n\
- Operator stdout/stderr logs are redacted and truncated when supplied.\n\
- Subjects are preserved by default; rerun with `--redact-subjects` when subjects may be sensitive.\n\
- Review `manifest.json` before sharing. It lists every included file and every omitted source class.\n\n\
Replay commands:\n\n\
```bash\n\
am doctor check --json\n\
am doctor repair --dry-run\n\
am doctor reconstruct --dry-run --json\n\
am doctor support-bundle --json\n\
```\n"
}

fn write_support_json_file(
    bundle_dir: &Path,
    rel: &str,
    value: &serde_json::Value,
    redaction_mode: &str,
    source_path_class: &str,
    files: &mut Vec<serde_json::Value>,
) -> CliResult<()> {
    let path = bundle_dir.join(rel);
    write_support_json_exact(&path, value)?;
    record_support_file(bundle_dir, rel, redaction_mode, source_path_class, files)
}

fn write_support_json_exact(path: &Path, value: &serde_json::Value) -> CliResult<()> {
    if path.exists() {
        return Err(CliError::Other(format!(
            "support bundle refusing to overwrite {}",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(value)
        .map_err(|err| CliError::Other(format!("serializing support bundle JSON: {err}")))?;
    fs::write(path, body)?;
    Ok(())
}

fn write_support_text_file(
    bundle_dir: &Path,
    rel: &str,
    body: &str,
    redaction_mode: &str,
    source_path_class: &str,
    files: &mut Vec<serde_json::Value>,
) -> CliResult<()> {
    let path = bundle_dir.join(rel);
    if path.exists() {
        return Err(CliError::Other(format!(
            "support bundle refusing to overwrite {}",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, body)?;
    record_support_file(bundle_dir, rel, redaction_mode, source_path_class, files)
}

fn record_support_file(
    bundle_dir: &Path,
    rel: &str,
    redaction_mode: &str,
    source_path_class: &str,
    files: &mut Vec<serde_json::Value>,
) -> CliResult<()> {
    let path = bundle_dir.join(rel);
    let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    files.push(serde_json::json!({
        "path": rel,
        "redaction_mode": redaction_mode,
        "source_path_class": source_path_class,
        "bytes": bytes,
    }));
    Ok(())
}

fn write_redacted_operator_log(
    bundle_dir: &Path,
    rel: &str,
    source: &Path,
    redaction: &SupportRedactionContext,
    files: &mut Vec<serde_json::Value>,
) -> CliResult<()> {
    reject_symlink_ancestor(source, "operator log")?;
    let meta = fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        return Err(CliError::Other(format!(
            "operator log is a symlink and will not be followed: {}",
            source.display()
        )));
    }
    let mut file = fs::File::open(source)?;
    let mut bytes = Vec::new();
    let mut limited = file.by_ref().take(512 * 1024 + 1);
    limited.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > 512 * 1024;
    if truncated {
        bytes.truncate(512 * 1024);
    }
    let mut body = String::from_utf8_lossy(&bytes).into_owned();
    body = redact_support_text(&body, redaction);
    if truncated {
        body.push_str("\n<truncated after 512 KiB>\n");
    }
    write_support_text_file(
        bundle_dir,
        rel,
        &body,
        "redacted_truncated_log",
        "operator_supplied_log",
        files,
    )
}

fn read_json_file(path: &Path) -> CliResult<serde_json::Value> {
    reject_symlink_ancestor(path, "JSON evidence")?;
    let body = fs::read_to_string(path)?;
    serde_json::from_str(&body)
        .map_err(|err| CliError::Other(format!("parsing {}: {err}", path.display())))
}

fn latest_forensic_manifest(storage_root: &Path) -> Option<PathBuf> {
    latest_named_file(
        &storage_root.join("doctor").join("forensics"),
        "manifest.json",
    )
}

fn latest_doctor_report_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let doctor_root = runs::doctor_root(&cwd);
    latest_doctor_report_path_for_root(&doctor_root)
}

fn latest_doctor_report_path_for_root(doctor_root: &Path) -> Option<PathBuf> {
    let run_dir = resolve_latest_doctor_run_dir(doctor_root)?;
    let report_path = run_dir.join("report.json");
    fs::symlink_metadata(&report_path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())?;
    Some(report_path)
}

fn resolve_latest_doctor_run_dir(doctor_root: &Path) -> Option<PathBuf> {
    let latest = doctor_root.join("latest");
    let target = fs::read_link(&latest).ok()?;
    let mut components = target.components();
    match components.next()? {
        std::path::Component::Normal(segment) if segment == "runs" => {}
        _ => return None,
    }
    let run_id = match components.next()? {
        std::path::Component::Normal(segment) => segment,
        _ => return None,
    };
    if components.next().is_some() {
        return None;
    }
    let run_dir = doctor_root.join("runs").join(run_id);
    reject_symlink_ancestor(&run_dir, "doctor latest target").ok()?;
    fs::symlink_metadata(&run_dir)
        .ok()
        .filter(|metadata| metadata.file_type().is_dir())?;
    Some(run_dir)
}

fn path_absent_without_following_symlink(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

fn latest_named_file(root: &Path, file_name: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() || entry.file_name() != file_name {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .map(|(seen, _)| modified > *seen)
            .unwrap_or(true)
        {
            latest = Some((modified, entry.path().to_path_buf()));
        }
    }
    latest.map(|(_, path)| path)
}

fn reject_symlink_ancestor(path: &Path, label: &str) -> CliResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CliError::Other(format!(
                    "{label} contains a symlink component and will not be followed: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => {
                return Err(CliError::Other(format!(
                    "checking {label} {}: {err}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

fn redact_support_json(
    value: serde_json::Value,
    ctx: &SupportRedactionContext,
    key: Option<&str>,
) -> serde_json::Value {
    if let Some(key) = key {
        if support_key_is_body(key) {
            return serde_json::Value::String("<redacted-message-body>".to_string());
        }
        if support_key_is_subject(key) && ctx.redact_subjects {
            return serde_json::Value::String("<redacted-subject>".to_string());
        }
        if support_key_is_attachment(key) {
            return match value {
                serde_json::Value::Array(values) if values.is_empty() => {
                    serde_json::Value::Array(vec![])
                }
                serde_json::Value::Null => serde_json::Value::Null,
                _ => serde_json::json!("<redacted-attachment-metadata>"),
            };
        }
        if support_key_is_secret(key) {
            return serde_json::Value::String("<redacted-secret>".to_string());
        }
    }

    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(redact_support_text(&text, ctx))
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| redact_support_json(value, ctx, key))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let redacted = redact_support_json(value, ctx, Some(&key));
                    (key, redacted)
                })
                .collect(),
        ),
        other => other,
    }
}

fn support_key_is_body(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "body" | "body_md" | "message_body" | "content"
    )
}

fn support_key_is_subject(key: &str) -> bool {
    key.eq_ignore_ascii_case("subject")
}

fn support_key_is_attachment(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "attachment" || key == "attachments" || key.contains("attachment_path")
}

fn support_key_is_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("authorization")
        || key.contains("bearer")
        || key == "database_url"
        || key == "http_bearer_token"
}

fn redact_support_text(input: &str, ctx: &SupportRedactionContext) -> String {
    let mut out = input.to_string();
    for raw in [
        ctx.database_path.display().to_string(),
        ctx.storage_root.display().to_string(),
    ] {
        if !raw.is_empty() && raw != "." {
            out = out.replace(&raw, "<redacted-path>");
        }
    }
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        if !home.is_empty() {
            out = out.replace(&home, "<home>");
        }
    }

    for (pattern, replacement) in [
        (
            r#"(?i)Bearer\s+[A-Za-z0-9._~+/\-=]+"#,
            "Bearer <redacted-token>",
        ),
        (
            r#"(?i)\b([A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|PASS|KEY|AUTH)[A-Z0-9_]*)\s*(?:=|:|\x{ff1a})\s*([^\s'"]+)"#,
            "$1=<redacted-secret>",
        ),
        (
            r#"(?i)\bDATABASE_URL\s*(?:=|:|\x{ff1a})\s*([^\s'"]+)"#,
            "DATABASE_URL=<redacted-database-url>",
        ),
        (
            r#"(?i)"([^"]*(?:token|secret|password|authorization|bearer)[^"]*)"\s*:\s*"[^"]*""#,
            "\"$1\":\"<redacted-secret>\"",
        ),
        (
            r#"(?i)"database_url"\s*:\s*"[^"]*""#,
            "\"database_url\":\"<redacted-database-url>\"",
        ),
        (
            r#"(?i)"body(?:_md)?"\s*:\s*"[^"]*""#,
            "\"body\":\"<redacted-message-body>\"",
        ),
        (
            r#"(?im)^(body|body_md|message_body)\s*[:=]\s*.*$"#,
            "$1=<redacted-message-body>",
        ),
        (
            r#"(?i)\b(body|body_md|message_body)=\S+"#,
            "$1=<redacted-message-body>",
        ),
        (
            r#"(?is)"attachments?"\s*:\s*\[[^\]]*\]"#,
            "\"attachments\":[\"<redacted-attachment-metadata>\"]",
        ),
        (
            r#"(?i)\battachments?=\S+"#,
            "attachments=<redacted-attachment-metadata>",
        ),
        (r#"([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@\s]+@"#, "$1****@"),
    ] {
        let re = regex::Regex::new(pattern).expect("valid support-bundle redaction regex");
        out = re.replace_all(&out, replacement).into_owned();
    }

    if ctx.redact_subjects {
        for (pattern, replacement) in [
            (
                r#"(?i)"subject"\s*:\s*"[^"]*""#,
                "\"subject\":\"<redacted-subject>\"",
            ),
            (
                r#"(?im)^subject\s*[:=]\s*.*$"#,
                "subject=<redacted-subject>",
            ),
            (r#"(?i)\bsubject=\S+"#, "subject=<redacted-subject>"),
        ] {
            let re = regex::Regex::new(pattern).expect("valid support-bundle subject regex");
            out = re.replace_all(&out, replacement).into_owned();
        }
    }

    out
}

/// Print `am doctor health` — one-line liveness summary + exit 0/1.
///
/// Cheap. For CI scheduling. Reads `.doctor/latest/report.json` if present.
pub fn handle_health(target: &std::path::Path) -> CliResult<()> {
    let root = runs::doctor_root(target);
    let latest = root.join("latest");
    let runs_dir = root.join("runs");

    if path_absent_without_following_symlink(&latest)
        && path_absent_without_following_symlink(&runs_dir)
    {
        // No prior run; doctor itself is healthy (no findings to report).
        println!("ok: no prior runs");
        return Ok(());
    }

    let report_path = latest_doctor_report_path_for_root(&root);

    let Some(report_path) = report_path else {
        println!("warn: no report.json in latest run");
        // Explicit exit 1: findings are present and no fix was run.
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
        // Explicit exit 1: findings are present and no fix was run.
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
        // Undo I/O failures use exit 3 (`fix_failed_rolled_back`).
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
            // Avoid unwraps in the user-facing JSON path.
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
        // Undo failures use exit 3 (`fix_failed_rolled_back`).
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

    #[cfg(unix)]
    #[test]
    fn latest_doctor_report_path_accepts_canonical_relative_latest_symlink() {
        let root = tempfile::tempdir().unwrap();
        let doctor_root = root.path().join(".doctor");
        let run_dir = doctor_root.join("runs/2026-05-13T00-00-00Z__abc123");
        fs::create_dir_all(&run_dir).unwrap();
        let report_path = run_dir.join("report.json");
        fs::write(&report_path, "{}").unwrap();
        std::os::unix::fs::symlink(
            Path::new("runs/2026-05-13T00-00-00Z__abc123"),
            doctor_root.join("latest"),
        )
        .unwrap();

        assert_eq!(
            latest_doctor_report_path_for_root(&doctor_root).as_deref(),
            Some(report_path.as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn latest_doctor_report_path_rejects_absolute_latest_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let doctor_root = root.path().join(".doctor");
        let outside_run = outside.path().join("runs/2026-05-13T00-00-00Z__abc123");
        fs::create_dir_all(&doctor_root).unwrap();
        fs::create_dir_all(&outside_run).unwrap();
        fs::write(outside_run.join("report.json"), r#"{"ok":true}"#).unwrap();
        std::os::unix::fs::symlink(&outside_run, doctor_root.join("latest")).unwrap();

        assert_eq!(latest_doctor_report_path_for_root(&doctor_root), None);
    }

    #[cfg(unix)]
    #[test]
    fn latest_doctor_report_path_rejects_parent_traversal_latest_symlink() {
        let root = tempfile::tempdir().unwrap();
        let doctor_root = root.path().join(".doctor");
        let outside_run = root.path().join("outside-run");
        fs::create_dir_all(&doctor_root).unwrap();
        fs::create_dir_all(&outside_run).unwrap();
        fs::write(outside_run.join("report.json"), r#"{"ok":true}"#).unwrap();
        std::os::unix::fs::symlink(Path::new("../outside-run"), doctor_root.join("latest"))
            .unwrap();

        assert_eq!(latest_doctor_report_path_for_root(&doctor_root), None);
    }

    #[cfg(unix)]
    #[test]
    fn path_absent_without_following_symlink_treats_dangling_symlink_as_present() {
        let root = tempfile::tempdir().unwrap();
        let dangling = root.path().join("dangling");
        let missing = root.path().join("missing");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();

        assert!(path_absent_without_following_symlink(&missing));
        assert!(
            !path_absent_without_following_symlink(&dangling),
            "dangling symlink should count as present doctor state"
        );
    }

    #[test]
    fn canonical_mcp_url_uses_client_connect_host_for_wildcard_bind() {
        let config = Config {
            http_host: "0.0.0.0".to_string(),
            http_port: 7777,
            http_path: "/api/".to_string(),
            ..Default::default()
        };

        assert_eq!(
            canonical_mcp_url_for_config(&config),
            "http://127.0.0.1:7777/api/"
        );
    }

    #[test]
    fn canonical_mcp_url_normalizes_unbracketed_ipv6_and_path() {
        let config = Config {
            http_host: "2001:db8::42".to_string(),
            http_port: 7777,
            http_path: "api".to_string(),
            ..Default::default()
        };

        assert_eq!(
            canonical_mcp_url_for_config(&config),
            "http://[2001:db8::42]:7777/api/"
        );
    }

    #[test]
    fn token_backup_candidates_references_canonical_suffix_list() {
        // Pass-18: the handler must enumerate via the module's canonical
        // `BACKUP_SUFFIX_HINTS` so widening the detector's accept-set
        // automatically widens the enumeration. Plant one file per
        // canonical suffix; assert every one is returned.
        let root = tempfile::tempdir().unwrap();
        for suffix in fixers::world_readable_token_bak::BACKUP_SUFFIX_HINTS {
            // Strip the leading dot for the filename stem.
            let name = format!("config.toml{suffix}");
            fs::write(root.path().join(&name), "HTTP_BEARER_TOKEN=secret").unwrap();
        }
        let candidates = token_backup_candidates(root.path(), None);
        let names = candidates
            .iter()
            .filter(|p| p.starts_with(root.path()))
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect::<std::collections::BTreeSet<_>>();
        for suffix in fixers::world_readable_token_bak::BACKUP_SUFFIX_HINTS {
            let expected = format!("config.toml{suffix}");
            assert!(
                names.contains(expected.as_str()),
                "handler enumeration must cover canonical suffix `{suffix}` (got: {names:?})"
            );
        }
    }

    #[test]
    fn default_token_backup_candidates_covers_detector_suffixes() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "config.toml.bak",
            "config.toml.tmp",
            "config.toml.backup",
            "config.toml.orig",
            "config.toml.old",
        ] {
            fs::write(root.path().join(name), "HTTP_BEARER_TOKEN=secret").unwrap();
        }
        fs::write(root.path().join("config.toml"), "HTTP_BEARER_TOKEN=secret").unwrap();

        let candidates = token_backup_candidates(root.path(), None);
        let names = candidates
            .iter()
            .filter(|path| path.starts_with(root.path()))
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<std::collections::BTreeSet<_>>();

        for expected in [
            "config.toml.bak",
            "config.toml.tmp",
            "config.toml.backup",
            "config.toml.orig",
            "config.toml.old",
        ] {
            assert!(
                names.contains(expected),
                "missing backup candidate {expected}: {names:?}"
            );
        }
        assert!(!names.contains("config.toml"));
    }

    #[test]
    fn default_token_backup_candidates_scans_common_client_config_dirs() {
        let storage_root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let client_dirs = [".codex", ".claude", ".cursor", ".windsurf", ".gemini"];
        for dir in client_dirs {
            let root = home.path().join(dir);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("mcp.json.bak"), "HTTP_BEARER_TOKEN=secret").unwrap();
        }

        let candidates = token_backup_candidates(storage_root.path(), Some(home.path()));
        let candidate_strings = candidates
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        for dir in client_dirs {
            let expected = home.path().join(dir).join("mcp.json.bak");
            assert!(
                candidates.contains(&expected),
                "missing backup candidate {} in:\n{}",
                expected.display(),
                candidate_strings
            );
        }
    }

    #[test]
    fn support_bundle_redacts_sensitive_text_classes() {
        let ctx = SupportRedactionContext {
            storage_root: PathBuf::from("/home/ubuntu/.mcp_agent_mail_git_mailbox_repo"),
            database_path: PathBuf::from(
                "/home/ubuntu/.mcp_agent_mail_git_mailbox_repo/storage.sqlite3",
            ),
            redact_subjects: true,
        };
        let input = r#"HTTP_BEARER_TOKEN=abc123
Authorization: Bearer secret.jwt.token
OPENAI_API_KEY=sk-test
DATABASE_URL=sqlite://user:pass@example.invalid/mail
subject=Sensitive incident title
body_md=private message body phrase
attachments=screenshot-secret.png
path=/home/ubuntu/.mcp_agent_mail_git_mailbox_repo/storage.sqlite3
"#;

        let redacted = redact_support_text(input, &ctx);
        for forbidden in [
            "abc123",
            "secret.jwt.token",
            "sk-test",
            "user:pass",
            "Sensitive",
            "private message body phrase",
            "screenshot-secret.png",
            "/home/ubuntu/.mcp_agent_mail_git_mailbox_repo",
        ] {
            assert!(
                !redacted.contains(forbidden),
                "support bundle text leaked {forbidden}: {redacted}"
            );
        }
        assert!(redacted.contains("<redacted-secret>"));
        assert!(redacted.contains("<redacted-message-body>"));
        assert!(redacted.contains("<redacted-attachment-metadata>"));
        assert!(redacted.contains("<redacted-path>"));
    }

    #[test]
    fn support_bundle_redaction_corpus_covers_field_and_content_variants() {
        let ctx = SupportRedactionContext {
            storage_root: PathBuf::from("/tmp/mail-storage"),
            database_path: PathBuf::from("/tmp/mail-storage/storage.sqlite3"),
            redact_subjects: true,
        };
        struct CorpusCase {
            name: &'static str,
            value: serde_json::Value,
            forbidden: &'static [&'static str],
            retained: &'static [&'static str],
        }
        let cases = vec![
            CorpusCase {
                name: "field-name secrets inside nested JSON",
                value: serde_json::json!({
                    "OPENAI_API_KEY": "sk-field-secret",
                    "bearer_header": "Bearer field-token",
                    "nested": {
                        "password": "hunter2",
                        "reason_code": "foreign_key_integrity",
                        "artifact_path_kind": "doctor_forensic_manifest"
                    }
                }),
                forbidden: &["sk-field-secret", "field-token", "hunter2"],
                retained: &["foreign_key_integrity", "doctor_forensic_manifest"],
            },
            CorpusCase {
                name: "safe command and query params redact values but keep command shape",
                value: serde_json::json!({
                    "safe_command": "am doctor support-bundle --bearer-token=command-secret --database-url sqlite://user:pass@example.invalid/mail?token=url-secret",
                    "category": "recovery",
                    "reason": "operator asked for sanitized bundle"
                }),
                forbidden: &["command-secret", "user:pass", "url-secret"],
                retained: &["am doctor support-bundle", "recovery", "sanitized bundle"],
            },
            CorpusCase {
                name: "free text logs with mixed separators",
                value: serde_json::Value::String(
                    "TOKEN\u{ff1a}unicode-secret\nDATABASE_URL: sqlite://user:pass@example.invalid/mail\nAuthorization: Bearer log-secret\nsubject=Sensitive title\nbody_md=Private body\nattachments=secret.png\npath=/tmp/mail-storage/storage.sqlite3\nsource_path_class=operator_supplied_log"
                        .to_string(),
                ),
                forbidden: &[
                    "unicode-secret",
                    "user:pass",
                    "log-secret",
                    "Sensitive title",
                    "Private body",
                    "secret.png",
                    "/tmp/mail-storage",
                ],
                retained: &["source_path_class=operator_supplied_log"],
            },
        ];

        for case in cases {
            let redacted = redact_support_json(case.value, &ctx, None);
            let encoded = serde_json::to_string(&redacted).unwrap();
            for forbidden in case.forbidden {
                assert!(
                    !encoded.contains(forbidden),
                    "{} leaked forbidden value {forbidden}: {encoded}",
                    case.name
                );
            }
            for retained in case.retained {
                assert!(
                    encoded.contains(retained),
                    "{} lost non-sensitive detail {retained}: {encoded}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn support_bundle_redacts_json_bodies_subjects_and_attachments() {
        let ctx = SupportRedactionContext {
            storage_root: PathBuf::from("/tmp/mail-storage"),
            database_path: PathBuf::from("/tmp/mail-storage/storage.sqlite3"),
            redact_subjects: true,
        };
        let value = serde_json::json!({
            "subject": "Sensitive subject",
            "body_md": "Private body text",
            "attachments": ["secret-attachment.png"],
            "database_url": "sqlite://user:pass@example.invalid/mail",
            "nested": {
                "authorization": "Bearer abc123"
            }
        });

        let redacted = redact_support_json(value, &ctx, None);
        let encoded = serde_json::to_string(&redacted).unwrap();
        for forbidden in [
            "Sensitive subject",
            "Private body text",
            "secret-attachment.png",
            "user:pass",
            "abc123",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "support bundle JSON leaked {forbidden}: {encoded}"
            );
        }
        assert!(encoded.contains("<redacted-subject>"));
        assert!(encoded.contains("<redacted-message-body>"));
        assert!(encoded.contains("<redacted-attachment-metadata>"));
    }

    #[test]
    fn support_bundle_manifest_lists_inclusions_and_omissions() {
        let root = tempfile::tempdir().unwrap();
        let storage_root = root.path().join("storage");
        let forensics = storage_root
            .join("doctor")
            .join("forensics")
            .join("storage")
            .join("repair-20260511");
        fs::create_dir_all(&forensics).unwrap();
        fs::write(
            forensics.join("manifest.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "command": "repair",
                "source": {
                    "database_url": "sqlite://user:pass@example.invalid/mail",
                    "db_path": storage_root.join("storage.sqlite3").display().to_string()
                },
                "subject": "Sensitive support subject",
                "body_md": "Private support body",
                "attachments": ["private-evidence.png"]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            forensics.join("summary.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "command": "repair",
                "body": "Private summary body"
            }))
            .unwrap(),
        )
        .unwrap();
        let stdout_log = root.path().join("stdout.log");
        fs::write(
            &stdout_log,
            "Bearer abc123\nsubject=Sensitive log subject\nbody=Private log body\n",
        )
        .unwrap();

        let mut config = Config {
            storage_root: storage_root.clone(),
            ..Default::default()
        };
        config.http_bearer_token = Some("not-written".to_string());
        let result = create_support_bundle(
            &config,
            &format!("sqlite:///{}/storage.sqlite3", storage_root.display()),
            SupportBundleOptions {
                output_dir: Some(root.path().join("bundles")),
                stdout_log: Some(stdout_log),
                stderr_log: None,
                redact_subjects: true,
            },
        )
        .unwrap();

        assert_eq!(result.observed_recovery_command.as_deref(), Some("repair"));
        let manifest = fs::read_to_string(&result.manifest_path).unwrap();
        for required in [
            "\"manifest.json\"",
            "\"summary.json\"",
            "\"reports/latest-forensic-manifest.json\"",
            "\"logs/stdout.log\"",
            "\"redaction_mode\"",
            "\"source_path_class\"",
            "\"SQLite database and sidecars\"",
            "\"message bodies and canonical message files\"",
            "\"attachment contents and attachment filenames\"",
        ] {
            assert!(
                manifest.contains(required),
                "support bundle manifest missing {required}: {manifest}"
            );
        }
        for forbidden in [
            "user:pass",
            "abc123",
            "Sensitive support subject",
            "Private support body",
            "private-evidence.png",
            storage_root.to_string_lossy().as_ref(),
        ] {
            assert!(
                !manifest.contains(forbidden),
                "support bundle manifest leaked {forbidden}: {manifest}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn support_bundle_refuses_symlink_output_root() {
        let root = tempfile::tempdir().unwrap();
        let storage_root = root.path().join("storage");
        fs::create_dir_all(&storage_root).unwrap();
        let real_output = root.path().join("real-output");
        fs::create_dir_all(&real_output).unwrap();
        let symlink_output = root.path().join("linked-output");
        std::os::unix::fs::symlink(&real_output, &symlink_output).unwrap();

        let config = Config {
            storage_root,
            ..Default::default()
        };
        let err = create_support_bundle(
            &config,
            "sqlite:///missing.sqlite3",
            SupportBundleOptions {
                output_dir: Some(symlink_output),
                stdout_log: None,
                stderr_log: None,
                redact_subjects: false,
            },
        )
        .expect_err("support bundle should refuse symlinked output roots");
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink refusal, got: {err}"
        );
    }
}
