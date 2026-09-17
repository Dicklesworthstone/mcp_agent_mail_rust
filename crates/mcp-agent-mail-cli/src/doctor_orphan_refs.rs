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
//!   deletion. Last 10 backups per project stay in the active backup
//!   directory; older ones are moved into `retired/` rather than
//!   deleted.
//! - **Per-repo flock** held for the full detect+prune+repack
//!   sequence. Apply refuses phantom locks as well as acquisition errors.
//! - **Git ref locks** cover target revalidation and deletion, including
//!   writers that do not participate in the cooperative flock protocol.
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
    DetectionSummary, PrunableRef, PruneRefOutcome, RefCategory, detect_missing_refs,
    prune_missing_ref, ref_backup,
};

use crate::output::CliOutputFormat;
use crate::{CliError, CliResult};

/// Backup retention: keep the last N backups active per project.
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

    if report.projects.iter().any(project_has_errors) {
        return Err(CliError::Other(
            "fix-orphan-refs encountered errors; see report above".to_string(),
        ));
    }

    Ok(())
}

fn project_has_errors(report: &ProjectReport) -> bool {
    report.error.is_some()
        || report
            .apply_result
            .as_ref()
            .is_some_and(|result| result.errors > 0)
}

fn failed_project_report(project: String, error: String) -> ProjectReport {
    ProjectReport {
        project,
        scanned_refs: None,
        actions: Vec::new(),
        summary: DetectionSummary::default(),
        apply_result: None,
        backup_path: None,
        error: Some(error),
    }
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

    // Apply requires actual ownership, not an optional/phantom lock. The
    // per-ref transaction below additionally excludes ordinary Git writers.
    let Some(canonical) = canonicalize_repo(project_path) else {
        return failed_project_report(
            project_display,
            "cannot canonicalize repository for locking".to_string(),
        );
    };
    let _flock = match RepoFlock::acquire(&canonical) {
        Ok(lock) if !apply || lock.is_real() => lock,
        Ok(_) => {
            return failed_project_report(
                project_display,
                "apply refused: repository lock is not held (phantom lock)".to_string(),
            );
        }
        Err(error) => {
            return failed_project_report(
                project_display,
                format!("repository lock acquisition failed: {error}"),
            );
        }
    };

    // Detect.
    let findings = match detect_missing_refs(project_path) {
        Ok(f) => f,
        Err(e) => {
            return failed_project_report(
                project_display,
                format!("detect_missing_refs failed: {e}"),
            );
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
        match prune_missing_ref(project_path, finding, force) {
            Ok(PruneRefOutcome::Pruned) => {
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
            Ok(outcome) => {
                actions.push(ActionRecord {
                    op: "prune_skipped",
                    project: project_display.clone(),
                    ref_name: Some(finding.ref_name.clone()),
                    target_sha: Some(finding.target_sha.clone()),
                    reason: Some(format!(
                        "finding no longer authorizes deletion: {outcome:?}"
                    )),
                    category: Some(finding.category),
                });
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

    if apply && apply_summary.pruned > 0 {
        // br-8ujfs.6.4 (F4): regenerate packed-refs after a successful
        // prune so the remaining refs are consolidated. Writes a
        // backup of packed-refs (if present) in the same backup dir
        // before running. `git pack-refs --all --prune` is atomic
        // (new file + rename) and safe to run while we hold the flock.
        //
        // Note: we rotate AFTER repack_refs (not before) so the
        // packed-refs backup it writes is included in the retention
        // calculation. Otherwise a repack backup would escape the
        // first rotation and only get retired on the next apply.
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

fn write_ref_backup(
    project_path: &Path,
    config: &Config,
    findings: &[PrunableRef],
) -> Result<PathBuf, String> {
    let slug = project_slug(project_path);
    // Time names order backups; no-clobber publication, not clock precision,
    // prevents concurrent attempts from replacing previously saved evidence.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_micros());
    let file = config
        .storage_root
        .join("backups")
        .join("refs")
        .join(&slug)
        .join(format!("{ts}.txt"));
    ref_backup::write_snapshot(project_path, &file, findings)
        .map_err(|error| format!("write complete ref backup {}: {error}", file.display()))?;
    Ok(file)
}

/// Repack refs via `git pack-refs --all --prune` after a successful
/// prune run. br-8ujfs.6.4 (F4).
fn repack_refs(project_path: &Path, config: &Config) -> Result<(), String> {
    // Linked worktrees keep packed-refs in the shared common Git directory.
    let packed_refs = git2::Repository::open(project_path)
        .map_err(|error| format!("open repository for packed-refs backup: {error}"))?
        .commondir()
        .join("packed-refs");
    let has_packed_refs = match fs::symlink_metadata(&packed_refs) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => {
            return Err(format!(
                "packed-refs is not a regular file: {}",
                packed_refs.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "inspect packed-refs {}: {error}",
                packed_refs.display()
            ));
        }
    };
    if has_packed_refs {
        let slug = project_slug(project_path);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_micros());
        let backup_file = config
            .storage_root
            .join("backups")
            .join("refs")
            .join(&slug)
            .join(format!("{ts}-packed-refs.txt"));
        ref_backup::copy_file(&packed_refs, &backup_file)
            .map_err(|error| format!("copy packed-refs backup: {error}"))?;
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
    if files.len() <= BACKUP_RETENTION {
        return Ok(());
    }

    let retired_dir = dir.join("retired");
    fs::create_dir_all(&retired_dir)
        .map_err(|e| format!("mkdir retired backup dir {}: {e}", retired_dir.display()))?;
    for (_, p) in files.iter().skip(BACKUP_RETENTION) {
        let retired_path = unique_retired_backup_path(&retired_dir, p);
        if let Err(e) = fs::rename(p, &retired_path) {
            tracing::warn!(
                target: "mcp_agent_mail::doctor::fix_orphan_refs",
                path = %p.display(),
                retired_path = %retired_path.display(),
                err = %e,
                "backup_rotation_retire_failed"
            );
        } else {
            tracing::debug!(
                target: "mcp_agent_mail::doctor::fix_orphan_refs",
                path = %p.display(),
                retired_path = %retired_path.display(),
                "backup_retired"
            );
        }
    }
    Ok(())
}

fn unique_retired_backup_path(retired_dir: &Path, source: &Path) -> PathBuf {
    let file_name = source
        .file_name()
        .map_or_else(|| "backup".into(), std::ffi::OsStr::to_os_string);
    let candidate = retired_dir.join(&file_name);
    if !candidate.exists() {
        return candidate;
    }

    let display_name = file_name.to_string_lossy();
    for suffix in 1..=u32::MAX {
        let candidate = retired_dir.join(format!("{display_name}.retired-{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }

    retired_dir.join(format!("{display_name}.retired-overflow"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn count_regular_files(dir: &Path) -> usize {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .filter(|entry| entry.metadata().is_ok_and(|meta| meta.is_file()))
            .count()
    }

    #[test]
    fn rotate_backups_retires_old_files_without_deleting_them() {
        let temp = TempDir::new().expect("tempdir");
        let project_path = temp.path().join("repo");
        fs::create_dir_all(&project_path).expect("repo dir");

        let config = Config {
            storage_root: temp.path().join("storage"),
            ..Config::default()
        };
        let backup_dir = config
            .storage_root
            .join("backups")
            .join("refs")
            .join(project_slug(&project_path));
        fs::create_dir_all(&backup_dir).expect("backup dir");

        for idx in 0..(BACKUP_RETENTION + 2) {
            fs::write(backup_dir.join(format!("{idx:02}.txt")), format!("{idx}\n"))
                .expect("backup file");
        }

        rotate_backups(&project_path, &config).expect("rotate backups");

        assert_eq!(count_regular_files(&backup_dir), BACKUP_RETENTION);
        assert_eq!(count_regular_files(&backup_dir.join("retired")), 2);
        assert_eq!(
            count_regular_files(&backup_dir) + count_regular_files(&backup_dir.join("retired")),
            BACKUP_RETENTION + 2,
            "rotation must move old backup artifacts, not delete them"
        );
    }

    #[test]
    fn unique_retired_backup_path_avoids_existing_retired_names() {
        let temp = TempDir::new().expect("tempdir");
        let retired_dir = temp.path().join("retired");
        fs::create_dir_all(&retired_dir).expect("retired dir");
        fs::write(retired_dir.join("123.txt"), b"old").expect("existing retired backup");

        let next = unique_retired_backup_path(&retired_dir, Path::new("/tmp/123.txt"));

        assert_eq!(next, retired_dir.join("123.txt.retired-1"));
    }

    fn orphan_fixture(temp: &TempDir) -> (PathBuf, Config, PathBuf) {
        let path = temp.path().join("repo");
        let repo = git2::Repository::init(&path).unwrap();
        let orphan = repo.path().join("refs/temp/orphan");
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        fs::write(&orphan, b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n").unwrap();
        let config = Config {
            storage_root: temp.path().join("storage"),
            ..Config::default()
        };
        (path, config, orphan)
    }

    #[test]
    fn apply_lock_failure_does_not_prune_or_create_a_backup() {
        let temp = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        let sentinel = mcp_agent_mail_core::git_lock::sentinel_path(&path).unwrap();
        fs::create_dir(&sentinel).unwrap();
        let before = fs::read(&orphan).unwrap();

        let report = scan_one_project(&path, &config, true, false);

        assert!(project_has_errors(&report));
        assert!(report.apply_result.is_none());
        assert!(report.actions.is_empty());
        assert_eq!(fs::read(&orphan).unwrap(), before);
        assert!(!config.storage_root.exists());
    }

    #[test]
    fn backup_failure_is_an_error_and_never_authorizes_pruning() {
        let temp = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        fs::write(&config.storage_root, b"not a directory").unwrap();
        let before = fs::read(&orphan).unwrap();

        let report = scan_one_project(&path, &config, true, false);

        assert!(project_has_errors(&report));
        assert!(
            report
                .actions
                .iter()
                .any(|action| action.op == "backup_failed")
        );
        assert_eq!(report.apply_result.as_ref().unwrap().pruned, 0);
        assert_eq!(report.apply_result.as_ref().unwrap().errors, 1);
        assert_eq!(fs::read(&orphan).unwrap(), before);
        assert!(report.backup_path.is_none());
    }

    #[test]
    fn git_ref_lock_failure_is_reported_without_repacking() {
        let temp = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        let repo = git2::Repository::open(&path).unwrap();
        let mut writer = repo.transaction().unwrap();
        writer.lock_ref("refs/temp/orphan").unwrap();
        let before = fs::read(&orphan).unwrap();

        let report = scan_one_project(&path, &config, true, false);

        assert!(project_has_errors(&report));
        assert!(report.backup_path.as_ref().unwrap().is_file());
        assert_eq!(report.apply_result.as_ref().unwrap().pruned, 0);
        assert!(
            report
                .actions
                .iter()
                .any(|action| action.op == "prune_failed")
        );
        assert!(
            !report
                .actions
                .iter()
                .any(|action| action.op == "repacked_refs")
        );
        assert_eq!(fs::read(&orphan).unwrap(), before);
    }

    #[test]
    fn dry_run_preserves_orphan_refs_and_does_not_create_backups() {
        let temp = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        let before = fs::read(&orphan).unwrap();

        let report = scan_one_project(&path, &config, false, false);

        assert!(!project_has_errors(&report));
        assert_eq!(report.summary.findings, 1);
        assert!(
            report
                .actions
                .iter()
                .any(|action| action.op == "would_prune")
        );
        assert_eq!(fs::read(&orphan).unwrap(), before);
        assert!(!config.storage_root.exists());
    }

    #[test]
    fn failed_dry_run_is_not_a_successful_health_report() {
        let temp = TempDir::new().unwrap();
        let config = Config {
            storage_root: temp.path().join("storage"),
            ..Config::default()
        };
        let report = scan_one_project(&temp.path().join("missing-repo"), &config, false, false);
        assert!(project_has_errors(&report));
        assert!(report.scanned_refs.is_none());
        assert!(!config.storage_root.exists());
    }

    #[test]
    fn successful_apply_keeps_a_complete_pre_repair_snapshot() {
        let temp = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        let report = scan_one_project(&path, &config, true, false);
        assert!(!project_has_errors(&report));
        assert_eq!(report.apply_result.as_ref().unwrap().pruned, 1);
        assert!(!orphan.exists());
        let text = fs::read_to_string(report.backup_path.as_ref().unwrap()).unwrap();
        assert!(text.contains("symref  HEAD  refs/heads/"));
        assert!(text.contains("ref  refs/temp/orphan  deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n"));
        assert!(text.contains("orphan  refs/temp/orphan"));
        assert!(text.ends_with("# END agent-mail ref backup\n"));
    }

    #[test]
    fn corrupt_object_fails_both_dry_run_and_apply_without_pruning() {
        let temp = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        let repo = git2::Repository::open(&path).unwrap();
        let oid = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let directory = repo.path().join("objects").join(&oid[..2]);
        fs::create_dir_all(&directory).unwrap();
        let object = directory.join(&oid[2..]);
        fs::write(&object, b"not a zlib object").unwrap();
        let before = fs::read(&orphan).unwrap();

        for apply in [false, true] {
            let report = scan_one_project(&path, &config, apply, false);
            assert!(project_has_errors(&report));
            assert!(report.apply_result.is_none());
            assert!(report.backup_path.is_none());
        }
        assert_eq!(fs::read(&orphan).unwrap(), before);
        assert_eq!(fs::read(object).unwrap(), b"not a zlib object");
        assert!(!config.storage_root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_backup_root_never_authorizes_doctor_pruning() {
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let (path, config, orphan) = orphan_fixture(&temp);
        std::os::unix::fs::symlink(outside.path(), &config.storage_root).unwrap();
        let before = fs::read(&orphan).unwrap();

        let report = scan_one_project(&path, &config, true, false);

        assert!(project_has_errors(&report));
        assert_eq!(report.apply_result.as_ref().unwrap().pruned, 0);
        assert!(
            report
                .actions
                .iter()
                .any(|action| action.op == "backup_failed")
        );
        assert_eq!(fs::read(orphan).unwrap(), before);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn doctor_repack_refuses_nonregular_packed_refs() {
        let temp = TempDir::new().unwrap();
        let (path, config, _) = orphan_fixture(&temp);
        let repo = git2::Repository::open(&path).unwrap();
        let packed = repo.commondir().join("packed-refs");
        fs::create_dir(&packed).unwrap();

        assert!(repack_refs(&path, &config).is_err());
        assert!(packed.is_dir());
        assert!(!config.storage_root.exists());
    }
}
