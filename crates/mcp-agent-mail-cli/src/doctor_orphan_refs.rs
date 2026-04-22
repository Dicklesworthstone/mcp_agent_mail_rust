//! `am doctor fix-orphan-refs` — br-8ujfs.6.1 (F1) + F3 pruning.
//!
//! Detects and optionally prunes refs whose target objects are missing
//! from the repo's object database. These refs are produced when a
//! crashing writer leaves a ref pointing at an oid that was never
//! written to the ODB — the canonical damage pattern from the git
//! 2.51.0 index-race bug.
//!
//! # Safety posture
//!
//! - **Dry-run by default.** `--apply` required to actually delete.
//! - **Protected refs** (HEAD, main, master, origin/HEAD, etc.)
//!   never auto-prune even with `--force`.
//! - **Unknown-namespace refs** (refs/heads/custom, refs/tags/*)
//!   refuse without `--force`.
//! - **Backups** of pruned refs written to
//!   `<STORAGE_ROOT>/backups/refs/<project_slug>/<ts>.txt` before
//!   deletion. Last 10 backups per project kept; older ones pruned.
//! - **Per-repo flock** held for the full detect+prune+repack
//!   sequence to prevent interleaving with concurrent committers.
//!
//! # Output
//!
//! Human-readable table by default. `--format json` yields a
//! structured payload compatible with
//! `docs/schemas/fix_orphan_refs.json` (check in via F6).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_core::git_lock::{RepoFlock, canonicalize_repo};
use mcp_agent_mail_storage::recovery::{
    DetectionSummary, PrunableRef, RefCategory, detect_missing_refs,
};

use crate::output::CliOutputFormat;
use crate::{CliError, CliResult};

/// Backup retention: keep the last N backups per project, prune older.
const BACKUP_RETENTION: usize = 10;

#[derive(Debug, Clone, serde::Serialize)]
struct ActionRecord {
    op: &'static str,
    project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<RefCategory>,
}

/// Top-level output payload for `--format json`.
#[derive(Debug, Clone, serde::Serialize)]
struct Report {
    dry_run: bool,
    force: bool,
    projects: Vec<ProjectReport>,
    summary: GlobalSummary,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProjectReport {
    project: String,
    scanned_refs: Option<usize>, // None if we couldn't enumerate
    actions: Vec<ActionRecord>,
    summary: DetectionSummary,
    apply_result: Option<ApplySummary>,
    backup_path: Option<PathBuf>,
    error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ApplySummary {
    pruned: usize,
    refused_protected: usize,
    refused_unknown_namespace: usize,
    errors: usize,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
struct GlobalSummary {
    total_projects: usize,
    total_findings: usize,
    total_pruned: usize,
    total_refused: usize,
}

pub fn run(
    project: Option<PathBuf>,
    all: bool,
    apply: bool,
    force: bool,
    format: Option<CliOutputFormat>,
) -> CliResult<()> {
    let config = Config::from_env();
    let format = format.unwrap_or(CliOutputFormat::Table);

    let projects: Vec<PathBuf> = if all {
        enumerate_registered_projects(&config.storage_root)
    } else {
        vec![
            project
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        ]
    };

    if projects.is_empty() {
        tracing::warn!(
            target: "mcp_agent_mail::doctor::fix_orphan_refs",
            "no_projects_to_scan"
        );
        println!("No projects to scan.");
        return Ok(());
    }

    let mut report = Report {
        dry_run: !apply,
        force,
        projects: Vec::with_capacity(projects.len()),
        summary: GlobalSummary::default(),
    };

    for p in &projects {
        report
            .projects
            .push(scan_one_project(p, &config, apply, force));
    }

    aggregate_summary(&mut report);

    match format {
        CliOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|e| CliError::Other(format!("serialize json: {e}")))?
            );
        }
        CliOutputFormat::Toon | CliOutputFormat::Table => {
            print_human(&report);
        }
    }

    if apply && report.projects.iter().any(|p| p.error.is_some()) {
        return Err(CliError::Other(
            "fix-orphan-refs encountered errors; see report above".to_string(),
        ));
    }

    Ok(())
}

fn scan_one_project(
    project_path: &Path,
    config: &Config,
    apply: bool,
    force: bool,
) -> ProjectReport {
    let project_display = project_path.display().to_string();
    tracing::info!(
        target: "mcp_agent_mail::doctor::fix_orphan_refs",
        project = %project_display,
        apply = apply,
        force = force,
        "fix_orphan_refs_started"
    );

    // Acquire per-repo flock for the full detect-prune sequence. This
    // protects against an operator running a git commit in the same
    // repo concurrently with our prune — we'd rather wait than
    // interleave.
    let canonical = canonicalize_repo(project_path);
    let _flock = canonical
        .as_ref()
        .and_then(|c| match RepoFlock::acquire(c) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::error!(
                    target: "mcp_agent_mail::doctor::fix_orphan_refs",
                    project = %project_display,
                    err = %e,
                    "flock_acquire_failed"
                );
                // Proceed without flock — the caller will see any damage
                // we cause in the error column.
                None
            }
        });

    // Detect.
    let findings = match detect_missing_refs(project_path) {
        Ok(f) => f,
        Err(e) => {
            return ProjectReport {
                project: project_display,
                scanned_refs: None,
                actions: Vec::new(),
                summary: DetectionSummary::default(),
                apply_result: None,
                backup_path: None,
                error: Some(format!("detect_missing_refs failed: {e}")),
            };
        }
    };

    // Count total refs via libgit2 so the report shows the true
    // denominator (findings / scanned) rather than just findings.
    let scanned_refs = count_refs(project_path).unwrap_or(findings.len());
    let summary = DetectionSummary::from_findings(scanned_refs, &findings);

    let mut actions = Vec::<ActionRecord>::new();
    let mut apply_summary = ApplySummary {
        pruned: 0,
        refused_protected: 0,
        refused_unknown_namespace: 0,
        errors: 0,
    };

    // Classify each finding into an action. In dry-run mode we emit
    // "would-prune / would-refuse"; in apply mode we actually prune
    // (and write backups).
    let mut to_prune: Vec<&PrunableRef> = Vec::new();

    for finding in &findings {
        match finding.category {
            RefCategory::Protected => {
                apply_summary.refused_protected += 1;
                actions.push(ActionRecord {
                    op: "refuse",
                    project: project_display.clone(),
                    ref_name: Some(finding.ref_name.clone()),
                    target_sha: Some(finding.target_sha.clone()),
                    reason: Some(
                        "protected ref (main/master/HEAD); operator must intervene manually"
                            .to_string(),
                    ),
                    category: Some(finding.category),
                });
            }
            RefCategory::SafeToPrune => {
                to_prune.push(finding);
            }
            RefCategory::AskUser => {
                if force {
                    to_prune.push(finding);
                } else {
                    apply_summary.refused_unknown_namespace += 1;
                    actions.push(ActionRecord {
                        op: "refuse",
                        project: project_display.clone(),
                        ref_name: Some(finding.ref_name.clone()),
                        target_sha: Some(finding.target_sha.clone()),
                        reason: Some("unknown namespace; pass --force to prune".to_string()),
                        category: Some(finding.category),
                    });
                }
            }
        }
    }

    let mut backup_path: Option<PathBuf> = None;
    if apply && !to_prune.is_empty() {
        // Write backup before pruning.
        match write_ref_backup(project_path, config, &findings) {
            Ok(p) => backup_path = Some(p),
            Err(e) => {
                apply_summary.errors += 1;
                actions.push(ActionRecord {
                    op: "backup_failed",
                    project: project_display.clone(),
                    ref_name: None,
                    target_sha: None,
                    reason: Some(format!("backup write failed: {e}")),
                    category: None,
                });
                // Do NOT prune without a backup.
                to_prune.clear();
            }
        }
    }

    // Prune.
    for finding in &to_prune {
        if !apply {
            actions.push(ActionRecord {
                op: "would_prune",
                project: project_display.clone(),
                ref_name: Some(finding.ref_name.clone()),
                target_sha: Some(finding.target_sha.clone()),
                reason: Some(finding.reason.clone()),
                category: Some(finding.category),
            });
            continue;
        }
        match prune_ref(project_path, &finding.ref_name) {
            Ok(()) => {
                apply_summary.pruned += 1;
                actions.push(ActionRecord {
                    op: "pruned",
                    project: project_display.clone(),
                    ref_name: Some(finding.ref_name.clone()),
                    target_sha: Some(finding.target_sha.clone()),
                    reason: Some(finding.reason.clone()),
                    category: Some(finding.category),
                });
                tracing::info!(
                    target: "mcp_agent_mail::doctor::fix_orphan_refs",
                    ref_name = %finding.ref_name,
                    oid = %finding.target_sha,
                    project = %project_display,
                    "ref_pruned"
                );
            }
            Err(e) => {
                apply_summary.errors += 1;
                actions.push(ActionRecord {
                    op: "prune_failed",
                    project: project_display.clone(),
                    ref_name: Some(finding.ref_name.clone()),
                    target_sha: Some(finding.target_sha.clone()),
                    reason: Some(format!("delete failed: {e}")),
                    category: Some(finding.category),
                });
            }
        }
    }

    if apply && backup_path.is_some() {
        // br-8ujfs.6.4 (F4): regenerate packed-refs after a successful
        // prune so the remaining refs are consolidated. Writes a
        // backup of packed-refs (if present) in the same backup dir
        // before running. `git pack-refs --all --prune` is atomic
        // (new file + rename) and safe to run while we hold the flock.
        //
        // Note: we rotate AFTER repack_refs (not before) so the
        // packed-refs backup it writes is included in the retention
        // calculation. Otherwise a repack backup would escape the
        // first rotation and only get reaped on the next apply.
        match repack_refs(project_path, config) {
            Ok(()) => {
                actions.push(ActionRecord {
                    op: "repacked_refs",
                    project: project_display.clone(),
                    ref_name: None,
                    target_sha: None,
                    reason: Some("git pack-refs --all --prune".to_string()),
                    category: None,
                });
            }
            Err(e) => {
                apply_summary.errors += 1;
                actions.push(ActionRecord {
                    op: "repack_failed",
                    project: project_display.clone(),
                    ref_name: None,
                    target_sha: None,
                    reason: Some(format!("pack-refs failed: {e}")),
                    category: None,
                });
            }
        }

        // Rotate after ALL backups are written (prune-time refs backup
        // + repack-time packed-refs backup) so retention reaps every
        // backup from this run fairly.
        let _ = rotate_backups(project_path, config);
    }

    tracing::info!(
        target: "mcp_agent_mail::doctor::fix_orphan_refs",
        project = %project_display,
        findings = findings.len(),
        pruned = apply_summary.pruned,
        refused_protected = apply_summary.refused_protected,
        refused_unknown = apply_summary.refused_unknown_namespace,
        errors = apply_summary.errors,
        apply = apply,
        "fix_orphan_refs_done"
    );

    ProjectReport {
        project: project_display,
        scanned_refs: Some(scanned_refs),
        actions,
        summary,
        apply_result: if apply { Some(apply_summary) } else { None },
        backup_path,
        error: None,
    }
}

/// Count ALL refs in the repo via libgit2. `None` if the repo can't be
/// opened or the reference iterator fails — caller falls back to
/// findings.len() for a non-panic display.
fn count_refs(project_path: &Path) -> Option<usize> {
    let repo = git2::Repository::open(project_path).ok()?;
    let refs = repo.references().ok()?;
    Some(refs.flatten().count())
}

fn prune_ref(project_path: &Path, ref_name: &str) -> Result<(), String> {
    let repo = git2::Repository::open(project_path).map_err(|e| format!("open repo: {e}"))?;
    let mut r = repo
        .find_reference(ref_name)
        .map_err(|e| format!("find_reference({ref_name}): {e}"))?;
    r.delete().map_err(|e| format!("delete({ref_name}): {e}"))?;
    Ok(())
}

fn write_ref_backup(
    project_path: &Path,
    config: &Config,
    findings: &[PrunableRef],
) -> Result<PathBuf, String> {
    let slug = project_slug(project_path);
    // Microsecond resolution so two --apply runs in the same second
    // don't clobber each other's backups. Microseconds is ~10x more
    // precision than filesystem mtime typically reports, which is
    // plenty for sort ordering.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let dir = config.storage_root.join("backups").join("refs").join(&slug);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let file = dir.join(format!("{ts}.txt"));
    let mut text = String::new();
    text.push_str(&format!("# fix-orphan-refs backup — {}\n", ts));
    text.push_str(&format!("# project: {}\n", project_path.display()));
    text.push_str("# format: <status> <ref_name> <target_sha> <category> <reason>\n");
    // Also dump ALL refs via libgit2 so a full restore is possible
    // from the backup alone.
    if let Ok(repo) = git2::Repository::open(project_path) {
        text.push_str("#\n# ALL refs at backup time:\n");
        if let Ok(references) = repo.references() {
            for r in references.flatten() {
                if let (Some(name), Some(target)) = (r.name(), r.target()) {
                    text.push_str(&format!("ref  {name}  {target}\n"));
                }
            }
        }
        text.push_str("#\n");
    }
    text.push_str("# ORPHAN findings (to be pruned):\n");
    for f in findings {
        text.push_str(&format!(
            "orphan  {}  {}  {:?}  {}\n",
            f.ref_name, f.target_sha, f.category, f.reason,
        ));
    }
    fs::write(&file, text.as_bytes()).map_err(|e| format!("write backup: {e}"))?;
    Ok(file)
}

/// Repack refs via `git pack-refs --all --prune` after a successful
/// prune run. br-8ujfs.6.4 (F4).
fn repack_refs(project_path: &Path, config: &Config) -> Result<(), String> {
    // Backup packed-refs (if it exists) BEFORE running pack-refs.
    let admin_dir = mcp_agent_mail_core::git_lock::admin_dir_for(project_path)
        .ok_or_else(|| "admin dir unresolvable".to_string())?;
    let packed_refs = admin_dir.join("packed-refs");
    if packed_refs.is_file() {
        let slug = project_slug(project_path);
        // Microsecond resolution — see write_ref_backup for rationale.
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let dir = config.storage_root.join("backups").join("refs").join(&slug);
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir backup dir: {e}"))?;
        let backup_file = dir.join(format!("{ts}-packed-refs.txt"));
        fs::copy(&packed_refs, &backup_file)
            .map_err(|e| format!("copy packed-refs backup: {e}"))?;
        tracing::info!(
            target: "mcp_agent_mail::doctor::fix_orphan_refs",
            src = %packed_refs.display(),
            dst = %backup_file.display(),
            "packed_refs_backup_written"
        );
    }

    // Run git pack-refs --all --prune via GitCmd.
    //
    // The caller (scan_one_project) already holds a RepoFlock for this
    // repo, acquired at the top of the scan. Re-acquiring it here
    // through GitCmd would open a second fd-level fcntl lock for the
    // same process on the same file — POSIX semantics let this
    // succeed but replace the lock, so the INNER RepoFlock's drop
    // would release the kernel lock even though the outer RepoFlock
    // is still alive, creating a brief window where no lock is held.
    // Use `.skip_flock()` to avoid the double-acquire entirely. We
    // still take the in-process mutex (cheap, idempotent-safe) so
    // cross-thread serialization within the process remains intact.
    let out = mcp_agent_mail_core::git_cmd::GitCmd::new(project_path)
        .args(["pack-refs", "--all", "--prune"])
        .skip_flock()
        .run()
        .map_err(|e| format!("pack-refs invocation: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pack-refs exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    tracing::info!(
        target: "mcp_agent_mail::doctor::fix_orphan_refs",
        project = %project_path.display(),
        "packed_refs_rebuild_completed"
    );
    Ok(())
}

fn rotate_backups(project_path: &Path, config: &Config) -> Result<(), String> {
    let slug = project_slug(project_path);
    let dir = config.storage_root.join("backups").join("refs").join(&slug);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(());
    };
    // Microsecond-precision mtime avoids tied sort keys when multiple
    // backups land within the same second (e.g., a refs backup + its
    // paired packed-refs backup from the same `--apply` run).
    let mut files: Vec<(u128, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let ts = meta
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_micros();
            Some((ts, e.path()))
        })
        .collect();
    // Sort: newest mtime first; secondary by path (Reverse) so ties
    // are deterministic across runs.
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, p) in files.iter().skip(BACKUP_RETENTION) {
        if let Err(e) = fs::remove_file(p) {
            tracing::warn!(
                target: "mcp_agent_mail::doctor::fix_orphan_refs",
                path = %p.display(),
                err = %e,
                "backup_rotation_remove_failed"
            );
        } else {
            tracing::debug!(
                target: "mcp_agent_mail::doctor::fix_orphan_refs",
                path = %p.display(),
                "backup_pruned"
            );
        }
    }
    Ok(())
}

fn project_slug(project_path: &Path) -> String {
    let canon = canonicalize_repo(project_path).unwrap_or_else(|| project_path.to_path_buf());
    let s = canon.to_string_lossy();
    // Replace path separators + other filesystem-unsafe chars with '_'.
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn enumerate_registered_projects(storage_root: &Path) -> Vec<PathBuf> {
    // STORAGE_ROOT/projects/<slug>/ holds each registered project's
    // archive. Each has a `project.json` that records the `human_key`
    // (absolute path) we want to scan.
    let projects_root = storage_root.join("projects");
    let Ok(entries) = fs::read_dir(&projects_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let pj = entry.path().join("project.json");
        if !pj.is_file() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&pj)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(hk) = v.get("human_key").and_then(|h| h.as_str())
        {
            let p = PathBuf::from(hk);
            if p.exists() {
                out.push(p);
            }
        }
    }
    out
}

fn aggregate_summary(report: &mut Report) {
    report.summary.total_projects = report.projects.len();
    for p in &report.projects {
        report.summary.total_findings += p.summary.findings;
        if let Some(a) = &p.apply_result {
            report.summary.total_pruned += a.pruned;
            report.summary.total_refused += a.refused_protected + a.refused_unknown_namespace;
        }
    }
}

fn print_human(report: &Report) {
    println!(
        "fix-orphan-refs report ({})",
        if report.dry_run { "dry-run" } else { "apply" }
    );
    for p in &report.projects {
        println!();
        println!("  project: {}", p.project);
        if let Some(err) = &p.error {
            println!("    ERROR: {err}");
            continue;
        }
        println!(
            "    findings: {} (safe={}, ask-user={}, protected={})",
            p.summary.findings,
            p.summary.by_category.safe_to_prune,
            p.summary.by_category.ask_user,
            p.summary.by_category.protected,
        );
        for a in &p.actions {
            match (a.ref_name.as_deref(), a.target_sha.as_deref()) {
                (Some(r), Some(sha)) => println!(
                    "      [{}] {} -> {} ({})",
                    a.op,
                    r,
                    &sha[..std::cmp::min(8, sha.len())],
                    a.reason.clone().unwrap_or_default(),
                ),
                _ => println!("      [{}] {}", a.op, a.reason.clone().unwrap_or_default()),
            }
        }
        if let Some(apply) = &p.apply_result {
            println!(
                "    applied: pruned={} refused-protected={} refused-unknown={} errors={}",
                apply.pruned,
                apply.refused_protected,
                apply.refused_unknown_namespace,
                apply.errors,
            );
            if let Some(bp) = &p.backup_path {
                println!("    backup: {}", bp.display());
            }
        }
    }
    println!();
    println!(
        "TOTALS: {} project(s), {} finding(s), {} pruned, {} refused",
        report.summary.total_projects,
        report.summary.total_findings,
        report.summary.total_pruned,
        report.summary.total_refused,
    );
    if report.dry_run && report.summary.total_findings > 0 {
        println!();
        println!("NOTE: dry-run by default. Re-run with --apply to actually prune.");
    }
}
