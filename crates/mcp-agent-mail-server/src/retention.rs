//! Background worker for retention/quota reporting.
//!
//! Mirrors legacy Python `_worker_retention_quota` in `http.py`:
//! - Walk `storage_root` to compute per-project statistics
//! - Report old messages, inbox counts, attachment sizes
//! - Emit quota warnings when limits exceeded
//! - Best-effort: suppress all errors, never crash server
//!
//! The worker runs on a dedicated OS thread with `std::thread::sleep` between
//! iterations, matching the pattern in `cleanup.rs` and `ack_ttl.rs`.

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

const ARTIFACT_REPORT_SCHEMA_VERSION: u32 = 1;
const LARGE_ARTIFACT_ROOT_WARN_BYTES: u64 = 512 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Global shutdown flag for the retention worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Start the retention/quota report worker (if enabled).
///
/// Must be called at most once. Subsequent calls are no-ops.
pub fn start(config: &Config) {
    if !config.retention_report_enabled && !config.quota_enabled {
        return;
    }

    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if worker
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
        && let Some(stale) = worker.take()
    {
        let _ = stale.join();
    }
    if worker.is_none() {
        let config = config.clone();
        SHUTDOWN.store(false, Ordering::Release);
        match std::thread::Builder::new()
            .name("retention-quota".into())
            .spawn(move || {
                retention_loop(&config);
            }) {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                warn!(
                    error = %err,
                    "failed to spawn retention/quota worker; continuing without retention background scans"
                );
                return;
            }
        }
    }
    drop(worker);
}

/// Signal the worker to stop.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn retention_loop(config: &Config) {
    let interval = std::time::Duration::from_secs(config.retention_report_interval_seconds.max(60));
    let startup_delay = interval.min(std::time::Duration::from_secs(10));

    info!(
        interval_secs = interval.as_secs(),
        retention_enabled = config.retention_report_enabled,
        quota_enabled = config.quota_enabled,
        storage_root = %config.storage_root.display(),
        "retention/quota report worker started"
    );

    if startup_delay > std::time::Duration::ZERO {
        info!(
            startup_delay_secs = startup_delay.as_secs(),
            "retention/quota worker startup delay engaged"
        );
        if sleep_with_shutdown(startup_delay) {
            return;
        }
    }

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("retention/quota report worker shutting down");
            return;
        }

        match run_retention_cycle(config) {
            Ok(report) => {
                info!(
                    target: "maintenance",
                    event = "retention_quota_report",
                    projects_scanned = report.projects_scanned,
                    total_attachment_bytes = report.total_attachment_bytes,
                    total_inbox_count = report.total_inbox_count,
                    warnings = report.warnings,
                    "retention/quota report completed"
                );
            }
            Err(e) => {
                warn!(error = %e, "retention/quota report cycle failed");
            }
        }

        if sleep_with_shutdown(interval) {
            return;
        }
    }
}

fn sleep_with_shutdown(duration: std::time::Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let chunk = remaining.min(std::time::Duration::from_secs(1));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    false
}

fn path_existing_prefix_has_symlink(path: &Path) -> bool {
    let mut current = if path.is_absolute() {
        std::path::PathBuf::new()
    } else {
        match std::env::current_dir() {
            Ok(dir) => dir,
            Err(_) => return true,
        }
    };

    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                current.push(Path::new(std::path::MAIN_SEPARATOR_STR));
            }
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => current.push(".."),
            std::path::Component::Normal(part) => current.push(part),
        }

        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return true,
        }
    }

    false
}

fn is_real_directory(path: &Path) -> bool {
    !path_existing_prefix_has_symlink(path)
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Summary of a retention/quota report cycle.
struct RetentionReport {
    projects_scanned: usize,
    total_attachment_bytes: u64,
    total_inbox_count: u64,
    warnings: usize,
}

/// Read-only artifact retention inventory for operator-facing doctor output.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactRetentionReport {
    pub schema_version: u32,
    pub generated_at: String,
    pub repo_root: String,
    pub storage_root: String,
    pub totals: ArtifactRetentionTotals,
    pub disk: ArtifactDiskReport,
    pub roots: Vec<ArtifactRootReport>,
    pub largest_roots: Vec<ArtifactRootReport>,
    pub warnings: Vec<ArtifactRetentionWarning>,
    pub safe_remediation: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactRetentionTotals {
    pub roots_reported: usize,
    pub roots_existing: usize,
    pub total_bytes: u64,
    pub total_files: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactDiskReport {
    pub pressure: String,
    pub effective_free_bytes: Option<u64>,
    pub storage_probe_path: String,
    pub db_probe_path: Option<String>,
    pub warning_threshold_bytes: u64,
    pub critical_threshold_bytes: u64,
    pub fatal_threshold_bytes: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactRootReport {
    pub name: String,
    pub path: String,
    pub retention_class: String,
    pub review_after_days: u64,
    pub delete_after_days: Option<u64>,
    pub bytes: u64,
    pub files: u64,
    pub exists: bool,
    pub scan_error: Option<String>,
    pub safe_remediation: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactRetentionWarning {
    pub severity: String,
    pub code: String,
    pub detail: String,
    pub largest_roots: Vec<ArtifactRootReport>,
    pub safe_remediation: String,
}

#[derive(Debug, Clone, Copy)]
struct DirectoryStats {
    bytes: u64,
    files: u64,
}

impl DirectoryStats {
    const fn empty() -> Self {
        Self { bytes: 0, files: 0 }
    }
}

/// Build a read-only artifact retention report for repository and mailbox roots.
#[must_use]
pub fn artifact_retention_report(
    config: &Config,
    repo_root: &Path,
    largest_limit: usize,
) -> ArtifactRetentionReport {
    let mut roots = Vec::new();
    push_repo_artifact_roots(&mut roots, repo_root);
    push_storage_artifact_roots(&mut roots, &config.storage_root);

    let totals = ArtifactRetentionTotals {
        roots_reported: roots.len(),
        roots_existing: roots.iter().filter(|root| root.exists).count(),
        total_bytes: roots
            .iter()
            .fold(0u64, |total, root| total.saturating_add(root.bytes)),
        total_files: roots
            .iter()
            .fold(0u64, |total, root| total.saturating_add(root.files)),
    };

    let mut largest_roots: Vec<ArtifactRootReport> = roots
        .iter()
        .filter(|root| root.exists && root.bytes > 0)
        .cloned()
        .collect();
    largest_roots.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.name.cmp(&right.name))
    });
    largest_roots.truncate(largest_limit.max(1));

    let disk_sample = mcp_agent_mail_core::disk::sample_disk(config);
    let disk = ArtifactDiskReport {
        pressure: disk_sample.pressure.label().to_string(),
        effective_free_bytes: disk_sample.effective_free_bytes,
        storage_probe_path: disk_sample.storage_probe_path.display().to_string(),
        db_probe_path: disk_sample
            .db_probe_path
            .map(|path| path.display().to_string()),
        warning_threshold_bytes: config.disk_space_warning_mb.saturating_mul(MIB),
        critical_threshold_bytes: config.disk_space_critical_mb.saturating_mul(MIB),
        fatal_threshold_bytes: config.disk_space_fatal_mb.saturating_mul(MIB),
        errors: disk_sample.errors,
    };

    let warnings = artifact_retention_warnings(&totals, &disk, &largest_roots);

    ArtifactRetentionReport {
        schema_version: ARTIFACT_REPORT_SCHEMA_VERSION,
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        repo_root: repo_root.display().to_string(),
        storage_root: config.storage_root.display().to_string(),
        totals,
        disk,
        roots,
        largest_roots,
        warnings,
        safe_remediation: vec![
            "No automatic deletion is performed by am doctor artifacts.".to_string(),
            "Archive a root before cleanup, for example: tar -C <parent> -czf /tmp/<root>.tgz <root>".to_string(),
            "After review, manually move stale evidence into a quarantine directory instead of deleting it.".to_string(),
        ],
    }
}

fn push_repo_artifact_roots(roots: &mut Vec<ArtifactRootReport>, repo_root: &Path) {
    push_artifact_root(
        roots,
        "tests_artifacts",
        repo_root.join("tests").join("artifacts"),
        "test_evidence",
        14,
        None,
        "Compress or move reviewed test evidence only after failure triage is complete.",
    );
    push_artifact_root(
        roots,
        "cli_test_artifacts",
        repo_root
            .join("crates")
            .join("mcp-agent-mail-cli")
            .join("tests")
            .join("artifacts"),
        "test_evidence",
        14,
        None,
        "Keep recent CLI proof artifacts; compress stale runs after the owning test is stable.",
    );
    push_artifact_root(
        roots,
        "refactor_artifacts",
        repo_root.join("refactor").join("artifacts"),
        "refactor_evidence",
        30,
        None,
        "Summarize durable findings in docs before moving old refactor ledgers to quarantine.",
    );
    push_artifact_root(
        roots,
        "docs_perf",
        repo_root.join("docs").join("perf"),
        "published_perf_evidence",
        90,
        None,
        "Treat committed performance evidence as long-lived; compress superseded raw bundles only.",
    );
    push_artifact_root(
        roots,
        "server_forensics",
        repo_root
            .join("crates")
            .join("mcp-agent-mail-server")
            .join("doctor")
            .join("forensics"),
        "forensic_evidence",
        90,
        None,
        "Preserve forensic bundles until the incident and follow-up issue are closed.",
    );
}

fn push_storage_artifact_roots(roots: &mut Vec<ArtifactRootReport>, storage_root: &Path) {
    let doctor_root = storage_root.join("doctor");
    push_artifact_root(
        roots,
        "doctor_reports",
        doctor_root.join("reports"),
        "doctor_evidence",
        30,
        None,
        "Keep the latest reports and archive older runs after incident review.",
    );
    push_artifact_root(
        roots,
        "doctor_forensics",
        doctor_root.join("forensics"),
        "forensic_evidence",
        90,
        None,
        "Archive forensic bundles before manual quarantine; never discard active incident evidence.",
    );
    push_artifact_root(
        roots,
        "doctor_archive_quarantine",
        doctor_root.join("archive-quarantine"),
        "quarantine_evidence",
        90,
        None,
        "Inspect quarantine entries, then archive or move them only with operator approval.",
    );
    push_artifact_root(
        roots,
        "doctor_quarantine",
        doctor_root.join("quarantine"),
        "quarantine_evidence",
        90,
        None,
        "Inspect quarantine entries, then archive or move them only with operator approval.",
    );
    push_artifact_root(
        roots,
        "storage_backups",
        storage_root.join("backups"),
        "recovery_backup",
        90,
        None,
        "Retain backups through the recovery window; archive older verified-good backups manually.",
    );
    push_project_attachment_roots(roots, storage_root);
}

fn push_project_attachment_roots(roots: &mut Vec<ArtifactRootReport>, storage_root: &Path) {
    let projects_root = storage_root.join("projects");
    if !is_real_directory(&projects_root) {
        return;
    }

    let Ok(entries) = std::fs::read_dir(projects_root) else {
        return;
    };
    let mut project_dirs: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if file_type.is_dir() && !file_type.is_symlink() {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();
    project_dirs.sort();

    for project_dir in project_dirs {
        let Some(slug) = project_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let attachments_dir = project_dir.join("attachments");
        if attachments_dir.exists() {
            push_artifact_root(
                roots,
                &format!("project_attachments:{slug}"),
                attachments_dir,
                "mail_attachment_evidence",
                90,
                None,
                "Attachments are message evidence; archive externally before any manual pruning.",
            );
        }
    }
}

fn push_artifact_root(
    roots: &mut Vec<ArtifactRootReport>,
    name: &str,
    path: PathBuf,
    retention_class: &str,
    review_after_days: u64,
    delete_after_days: Option<u64>,
    safe_remediation: &str,
) {
    roots.push(scan_artifact_root(
        name,
        path,
        retention_class,
        review_after_days,
        delete_after_days,
        safe_remediation,
    ));
}

fn scan_artifact_root(
    name: &str,
    path: PathBuf,
    retention_class: &str,
    review_after_days: u64,
    delete_after_days: Option<u64>,
    safe_remediation: &str,
) -> ArtifactRootReport {
    let metadata = std::fs::symlink_metadata(&path);
    let exists = metadata.is_ok();
    let scan_error = match metadata {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Some("root is a symlink; skipped for safety".to_string())
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            Some("root exists but is not a directory; skipped".to_string())
        }
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(format!("metadata error: {err}")),
    };
    let stats = if scan_error.is_none() {
        dir_stats(&path)
    } else {
        DirectoryStats::empty()
    };

    ArtifactRootReport {
        name: name.to_string(),
        path: path.display().to_string(),
        retention_class: retention_class.to_string(),
        review_after_days,
        delete_after_days,
        bytes: stats.bytes,
        files: stats.files,
        exists,
        scan_error,
        safe_remediation: safe_remediation.to_string(),
    }
}

fn artifact_retention_warnings(
    totals: &ArtifactRetentionTotals,
    disk: &ArtifactDiskReport,
    largest_roots: &[ArtifactRootReport],
) -> Vec<ArtifactRetentionWarning> {
    let mut warnings = Vec::new();
    let top_roots: Vec<ArtifactRootReport> = largest_roots.iter().take(3).cloned().collect();

    if disk.pressure != "ok" {
        warnings.push(ArtifactRetentionWarning {
            severity: disk.pressure.clone(),
            code: "disk_pressure".to_string(),
            detail: format!(
                "disk pressure is {}; artifact roots total {} bytes across {} files",
                disk.pressure, totals.total_bytes, totals.total_files
            ),
            largest_roots: top_roots,
            safe_remediation: "Archive the listed largest roots, verify the archive, then manually move stale evidence to quarantine. am doctor artifacts does not delete.".to_string(),
        });
    } else if let Some(free_bytes) = disk.effective_free_bytes {
        let near_warning_threshold = disk.warning_threshold_bytes > 0
            && free_bytes < disk.warning_threshold_bytes.saturating_mul(2);
        if near_warning_threshold && totals.total_bytes > 0 {
            warnings.push(ArtifactRetentionWarning {
                severity: "warning".to_string(),
                code: "approaching_disk_warning_threshold".to_string(),
                detail: format!(
                    "{} bytes free is within 2x the warning threshold; artifact roots account for {} bytes",
                    free_bytes, totals.total_bytes
                ),
                largest_roots: top_roots,
                safe_remediation: "Compress or externalize the listed roots before running large swarm or e2e jobs.".to_string(),
            });
        }
    }

    if let Some(root) = largest_roots
        .iter()
        .find(|root| root.bytes >= LARGE_ARTIFACT_ROOT_WARN_BYTES)
    {
        warnings.push(ArtifactRetentionWarning {
            severity: "info".to_string(),
            code: "large_artifact_root".to_string(),
            detail: format!("{} is {} bytes at {}", root.name, root.bytes, root.path),
            largest_roots: vec![root.clone()],
            safe_remediation: root.safe_remediation.clone(),
        });
    }

    warnings
}

/// Run a single retention/quota report cycle.
fn run_retention_cycle(config: &Config) -> Result<RetentionReport, String> {
    // Mailbox archive layout is `{storage_root}/projects/{project_slug}/...`.
    // Retention/quota logic should operate on per-project directories under `projects/`.
    let projects_root = config.storage_root.join("projects");
    if !is_real_directory(&projects_root) {
        return Ok(RetentionReport {
            projects_scanned: 0,
            total_attachment_bytes: 0,
            total_inbox_count: 0,
            warnings: 0,
        });
    }

    let mut report = RetentionReport {
        projects_scanned: 0,
        total_attachment_bytes: 0,
        total_inbox_count: 0,
        warnings: 0,
    };

    // Walk project directories under `{storage_root}/projects`.
    let entries = std::fs::read_dir(&projects_root).map_err(|e| {
        format!(
            "failed to read projects dir: {} ({e})",
            projects_root.display()
        )
    })?;

    for entry in entries {
        let Ok(entry) = entry else { continue };

        let path = entry.path();
        // Skip anything that isn't a real directory (avoid following symlinks).
        if entry
            .file_type()
            .is_ok_and(|ft| !ft.is_dir() || ft.is_symlink())
        {
            continue;
        }

        let project_name = entry.file_name().to_string_lossy().to_string();

        // Check if project matches ignore patterns.
        if should_ignore(&project_name, &config.retention_ignore_project_patterns) {
            continue;
        }

        report.projects_scanned = report.projects_scanned.saturating_add(1);

        // Scan attachments.
        let attachments_dir = path.join("attachments");
        let attachment_bytes = dir_size(&attachments_dir);
        report.total_attachment_bytes = report
            .total_attachment_bytes
            .saturating_add(attachment_bytes);

        // Scan inbox (count .md files under agents/*/inbox/).
        let agents_dir = path.join("agents");
        let inbox_count = count_inbox_files(&agents_dir);
        report.total_inbox_count = report.total_inbox_count.saturating_add(inbox_count);

        // Quota checks.
        if config.quota_enabled {
            if config.quota_attachments_limit_bytes > 0
                && attachment_bytes > config.quota_attachments_limit_bytes
            {
                warn!(
                    target: "maintenance",
                    event = "quota_exceeded",
                    project = %project_name,
                    resource = "attachments",
                    current_bytes = attachment_bytes,
                    limit_bytes = config.quota_attachments_limit_bytes,
                    "attachment quota exceeded"
                );
                report.warnings = report.warnings.saturating_add(1);
            }

            if config.quota_inbox_limit_count > 0 && inbox_count > config.quota_inbox_limit_count {
                warn!(
                    target: "maintenance",
                    event = "quota_exceeded",
                    project = %project_name,
                    resource = "inbox",
                    current_count = inbox_count,
                    limit_count = config.quota_inbox_limit_count,
                    "inbox quota exceeded"
                );
                report.warnings = report.warnings.saturating_add(1);
            }
        }

        // Retention age check (report only, non-destructive).
        if config.retention_report_enabled && config.retention_max_age_days > 0 {
            let old_count = count_old_messages(&agents_dir, config.retention_max_age_days);
            if old_count > 0 {
                info!(
                    target: "maintenance",
                    event = "retention_age_report",
                    project = %project_name,
                    old_message_count = old_count,
                    max_age_days = config.retention_max_age_days,
                    "project has messages older than retention threshold"
                );
            }
        }
    }

    Ok(report)
}

/// Check if a project name matches any ignore pattern.
///
/// Supports simple glob: `*` matches any sequence of characters.
fn should_ignore(name: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        let pat = pattern.trim();
        if pat.is_empty() {
            continue;
        }

        if wildcard_match(pat, name) {
            return true;
        }
    }
    false
}

fn wildcard_match(pattern: &str, name: &str) -> bool {
    // Fast path: no wildcards
    if !pattern.contains('*') {
        return pattern == name;
    }

    // Split pattern by '*' and ensure all segments match in order
    let segments: Vec<&str> = pattern.split('*').collect();

    // If it's just "*"
    if segments.len() == 2 && segments[0].is_empty() && segments[1].is_empty() {
        return true;
    }

    let mut current_name = name;

    for (i, segment) in segments.iter().enumerate() {
        if i == 0 {
            // First segment must match prefix
            if !current_name.starts_with(segment) {
                return false;
            }
            current_name = &current_name[segment.len()..];
        } else if i == segments.len() - 1 {
            // Last segment must match suffix
            if !current_name.ends_with(segment) {
                return false;
            }
        } else {
            // Middle segments must be found in order
            if segment.is_empty() {
                continue;
            }
            if let Some(pos) = current_name.find(segment) {
                current_name = &current_name[pos + segment.len()..];
            } else {
                return false;
            }
        }
    }
    true
}

/// Recursively compute total size of a directory in bytes.
fn dir_size(path: &Path) -> u64 {
    dir_stats(path).bytes
}

fn dir_stats(path: &Path) -> DirectoryStats {
    if !is_real_directory(path) {
        return DirectoryStats::empty();
    }

    let mut stats = DirectoryStats::empty();
    let mut stack = vec![path.to_path_buf()];

    while let Some(current) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(current) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() {
                    continue;
                }

                let p = entry.path();
                if ft.is_file() {
                    stats.bytes = stats
                        .bytes
                        .saturating_add(entry.metadata().map_or(0, |m| m.len()));
                    stats.files = stats.files.saturating_add(1);
                } else if ft.is_dir() {
                    stack.push(p);
                }
            }
        }
    }
    stats
}

/// Count .md files recursively under agents/*/inbox/.
fn count_inbox_files(agents_dir: &Path) -> u64 {
    if !is_real_directory(agents_dir) {
        return 0;
    }

    let mut count = 0u64;
    if let Ok(agents) = std::fs::read_dir(agents_dir) {
        for agent in agents.flatten() {
            let Ok(agent_type) = agent.file_type() else {
                continue;
            };
            if !agent_type.is_dir() || agent_type.is_symlink() {
                continue;
            }
            let inbox = agent.path().join("inbox");
            if is_real_directory(&inbox) {
                count = count.saturating_add(count_md_files_recursive(&inbox));
            }
        }
    }
    count
}

/// Recursively count .md files in a directory.
fn count_md_files_recursive(dir: &Path) -> u64 {
    if !is_real_directory(dir) {
        return 0;
    }

    let mut count = 0u64;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(current) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() {
                    continue;
                }

                let p = entry.path();
                if ft.is_file() {
                    if p.extension().is_some_and(|e| e == "md") {
                        count = count.saturating_add(1);
                    }
                } else if ft.is_dir() {
                    stack.push(p);
                }
            }
        }
    }
    count
}

/// Count messages older than `max_age_days` under agents/*/inbox/.
fn count_old_messages(agents_dir: &Path, max_age_days: u64) -> usize {
    if !is_real_directory(agents_dir) {
        return 0;
    }

    let Ok(max_age_days) = i64::try_from(max_age_days) else {
        return 0;
    };
    let Some(max_age) = chrono::TimeDelta::try_days(max_age_days) else {
        return 0;
    };

    let mut count = 0usize;
    let cutoff = chrono::Utc::now()
        .checked_sub_signed(max_age)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);

    if let Ok(agents) = std::fs::read_dir(agents_dir) {
        for agent in agents.flatten() {
            let Ok(agent_type) = agent.file_type() else {
                continue;
            };
            if !agent_type.is_dir() || agent_type.is_symlink() {
                continue;
            }
            let inbox = agent.path().join("inbox");
            if !is_real_directory(&inbox) {
                continue;
            }

            let mut stack = vec![inbox];

            while let Some(current) = stack.pop() {
                if let Ok(entries) = std::fs::read_dir(current) {
                    for entry in entries.flatten() {
                        let Ok(ft) = entry.file_type() else {
                            continue;
                        };
                        if ft.is_symlink() {
                            continue;
                        }

                        let p = entry.path();
                        if ft.is_file() {
                            if p.extension().is_some_and(|e| e == "md") {
                                if let Ok(meta) = entry.metadata() {
                                    if let Ok(modified) = meta.modified() {
                                        let dt: chrono::DateTime<chrono::Utc> = modified.into();
                                        if dt < cutoff {
                                            count = count.saturating_add(1);
                                        }
                                    }
                                }
                            }
                        } else if ft.is_dir() {
                            stack.push(p);
                        }
                    }
                }
            }
        }
    }
    count
}

/// Count files older than cutoff in a directory tree.
#[allow(dead_code)]
fn count_old_files_recursive(dir: &Path, cutoff: std::time::SystemTime) -> u64 {
    if !is_real_directory(dir) {
        return 0;
    }

    let mut count = 0u64;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(current) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() {
                    continue;
                }

                let p = entry.path();
                if ft.is_file() && p.extension().is_some_and(|e| e == "md") {
                    if let Ok(metadata) = entry.metadata()
                        && let Ok(modified) = metadata.modified()
                        && modified < cutoff
                    {
                        count += 1;
                    }
                } else if ft.is_dir() {
                    stack.push(p);
                }
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_ignore_exact_match() {
        let patterns = vec!["demo".to_string(), "test*".to_string()];
        assert!(should_ignore("demo", &patterns));
        assert!(!should_ignore("production", &patterns));
    }

    #[test]
    fn should_ignore_glob_prefix() {
        let patterns = vec!["test*".to_string(), "testproj*".to_string()];
        assert!(should_ignore("testing", &patterns));
        assert!(should_ignore("testproject-1", &patterns));
        assert!(!should_ignore("mytest", &patterns));
    }

    #[test]
    fn should_ignore_glob_suffix() {
        let patterns = vec!["*-test".to_string()];
        assert!(should_ignore("my-test", &patterns));
        assert!(should_ignore("integration-test", &patterns));
        assert!(!should_ignore("test-my", &patterns));
    }

    #[test]
    fn should_ignore_glob_contains() {
        let patterns = vec!["*ignore*".to_string()];
        assert!(should_ignore("do-ignore-this", &patterns));
        assert!(should_ignore("ignore-this", &patterns));
        assert!(should_ignore("this-ignore", &patterns));
        assert!(!should_ignore("keep-this", &patterns));
    }

    #[test]
    fn should_ignore_empty_patterns() {
        let patterns: Vec<String> = vec![];
        assert!(!should_ignore("anything", &patterns));
    }

    #[test]
    fn dir_size_nonexistent_returns_zero() {
        assert_eq!(dir_size(Path::new("/nonexistent/path")), 0);
    }

    #[test]
    fn dir_size_with_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "world!").unwrap();
        let size = dir_size(tmp.path());
        assert_eq!(size, 11); // "hello" (5) + "world!" (6)
    }

    #[test]
    fn count_inbox_files_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp
            .path()
            .join("GreenCastle")
            .join("inbox")
            .join("2026")
            .join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("msg1.md"), "# Hello").unwrap();
        std::fs::write(agent_dir.join("msg2.md"), "# World").unwrap();
        std::fs::write(agent_dir.join("notes.txt"), "not counted").unwrap();

        let count = count_inbox_files(tmp.path());
        assert_eq!(count, 2);
    }

    #[test]
    fn retention_cycle_empty_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::from_env();
        config.storage_root = tmp.path().to_path_buf();
        config.retention_report_enabled = true;
        config.quota_enabled = true;

        let report = run_retention_cycle(&config).unwrap();
        assert_eq!(report.projects_scanned, 0);
        assert_eq!(report.total_attachment_bytes, 0);
        assert_eq!(report.total_inbox_count, 0);
        assert_eq!(report.warnings, 0);
    }

    #[test]
    fn retention_cycle_with_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects").join("my-project");
        let attach = project.join("attachments");
        let agents = project
            .join("agents")
            .join("BlueBear")
            .join("inbox")
            .join("2026")
            .join("01");
        std::fs::create_dir_all(&attach).unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(attach.join("file.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(agents.join("msg.md"), "# Test").unwrap();

        let mut config = Config::from_env();
        config.storage_root = tmp.path().to_path_buf();
        config.retention_report_enabled = true;
        config.quota_enabled = true;
        config.quota_attachments_limit_bytes = 50; // Low limit to trigger warning.
        config.quota_inbox_limit_count = 0; // Disabled.
        config.retention_ignore_project_patterns = vec![];

        let report = run_retention_cycle(&config).unwrap();
        assert_eq!(report.projects_scanned, 1);
        assert_eq!(report.total_attachment_bytes, 100);
        assert_eq!(report.total_inbox_count, 1);
        assert_eq!(report.warnings, 1); // Attachment quota exceeded.
    }

    #[test]
    fn worker_disabled_by_default() {
        let config = Config::from_env();
        assert!(!config.retention_report_enabled);
        assert!(!config.quota_enabled);
    }

    // ── br-3h13: Additional retention.rs test coverage ──────────────

    #[test]
    fn should_ignore_whitespace_in_patterns() {
        let patterns = vec!["  demo  ".to_string()];
        assert!(should_ignore("demo", &patterns));
    }

    #[test]
    fn should_ignore_empty_pattern_skipped() {
        let patterns = vec![String::new(), "   ".to_string()];
        assert!(!should_ignore("anything", &patterns));
    }

    #[test]
    fn dir_size_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("level1").join("level2");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.path().join("root.txt"), "abc").unwrap(); // 3 bytes
        std::fs::write(sub.join("nested.txt"), "defgh").unwrap(); // 5 bytes
        let size = dir_size(tmp.path());
        assert_eq!(size, 8);
    }

    #[test]
    fn dir_size_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size(tmp.path()), 0);
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_skips_symlink_root_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("payload.bin"), vec![0u8; 64]).unwrap();
        let linked = tmp.path().join("linked");
        symlink(&real, &linked).unwrap();

        assert_eq!(dir_size(&linked), 0);
    }

    #[cfg(unix)]
    #[test]
    fn retention_cycle_skips_symlinked_storage_root_prefix() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real_storage = tmp.path().join("real-storage");
        let project_dir = real_storage.join("projects").join("demo");
        std::fs::create_dir_all(project_dir.join("attachments")).unwrap();
        std::fs::create_dir_all(project_dir.join("agents/RedFox/inbox")).unwrap();
        std::fs::write(project_dir.join("attachments/blob.bin"), vec![0u8; 32]).unwrap();
        std::fs::write(project_dir.join("agents/RedFox/inbox/msg.md"), "message").unwrap();

        let linked_storage = tmp.path().join("linked-storage");
        symlink(&real_storage, &linked_storage).unwrap();

        let mut config = Config::default();
        config.storage_root = linked_storage;
        config.retention_report_enabled = true;
        config.quota_enabled = true;

        let report = run_retention_cycle(&config).unwrap();
        assert_eq!(report.projects_scanned, 0);
        assert_eq!(report.total_attachment_bytes, 0);
        assert_eq!(report.total_inbox_count, 0);
    }

    #[test]
    fn count_inbox_files_no_agents() {
        let tmp = tempfile::tempdir().unwrap();
        // agents_dir exists but empty
        assert_eq!(count_inbox_files(tmp.path()), 0);
    }

    #[test]
    fn count_inbox_files_multiple_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let agent1 = tmp.path().join("RedFox").join("inbox");
        let agent2 = tmp.path().join("BlueBear").join("inbox");
        std::fs::create_dir_all(&agent1).unwrap();
        std::fs::create_dir_all(&agent2).unwrap();
        std::fs::write(agent1.join("a.md"), "msg").unwrap();
        std::fs::write(agent1.join("b.md"), "msg").unwrap();
        std::fs::write(agent2.join("c.md"), "msg").unwrap();
        assert_eq!(count_inbox_files(tmp.path()), 3);
    }

    #[cfg(unix)]
    #[test]
    fn count_inbox_files_skips_symlinked_agent_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real_agent = tmp.path().join("real-agent").join("inbox");
        std::fs::create_dir_all(&real_agent).unwrap();
        std::fs::write(real_agent.join("msg.md"), "msg").unwrap();

        let scan_root = tmp.path().join("agents");
        std::fs::create_dir_all(&scan_root).unwrap();
        symlink(
            tmp.path().join("real-agent"),
            scan_root.join("linked-agent"),
        )
        .unwrap();

        assert_eq!(count_inbox_files(&scan_root), 0);
    }

    #[test]
    fn count_md_files_recursive_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("c.json"), "x").unwrap();
        assert_eq!(count_md_files_recursive(tmp.path()), 1);
    }

    #[test]
    fn count_old_messages_no_old_files() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("Agent").join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("fresh.md"), "new").unwrap();
        // max_age_days = 365000 (~1000 years), so nothing should be old
        assert_eq!(count_old_messages(tmp.path(), 365_000), 0);
    }

    #[test]
    fn count_old_messages_nonexistent_dir() {
        assert_eq!(count_old_messages(Path::new("/nonexistent"), 30), 0);
    }

    #[test]
    fn count_old_messages_extreme_age_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("Agent").join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("fresh.md"), "new").unwrap();

        assert_eq!(count_old_messages(tmp.path(), u64::MAX), 0);
    }

    #[test]
    fn retention_cycle_with_ignored_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects").join("test-proj");
        std::fs::create_dir_all(&project).unwrap();

        let mut config = Config::from_env();
        config.storage_root = tmp.path().to_path_buf();
        config.retention_report_enabled = true;
        config.retention_ignore_project_patterns = vec!["test*".to_string()];

        let report = run_retention_cycle(&config).unwrap();
        assert_eq!(
            report.projects_scanned, 0,
            "ignored project should not be scanned"
        );
    }

    #[test]
    fn retention_cycle_inbox_quota_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects").join("big-proj");
        let inbox = project.join("agents").join("Fox").join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        for i in 0..5 {
            std::fs::write(inbox.join(format!("msg{i}.md")), "hi").unwrap();
        }

        let mut config = Config::from_env();
        config.storage_root = tmp.path().to_path_buf();
        config.quota_enabled = true;
        config.quota_inbox_limit_count = 2; // Low limit
        config.quota_attachments_limit_bytes = 0; // Disabled

        let report = run_retention_cycle(&config).unwrap();
        assert_eq!(report.total_inbox_count, 5);
        assert_eq!(report.warnings, 1, "inbox quota should be exceeded");
    }

    #[test]
    fn retention_cycle_multiple_projects() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["proj-a", "proj-b", "proj-c"] {
            let proj = tmp.path().join("projects").join(name);
            std::fs::create_dir_all(proj.join("attachments")).unwrap();
        }

        let mut config = Config::from_env();
        config.storage_root = tmp.path().to_path_buf();
        config.retention_report_enabled = true;

        let report = run_retention_cycle(&config).unwrap();
        assert_eq!(report.projects_scanned, 3);
    }

    #[test]
    fn artifact_retention_report_inventories_roots_without_deleting() {
        let repo = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();

        let test_artifacts = repo.path().join("tests").join("artifacts").join("perf");
        let cli_artifacts = repo
            .path()
            .join("crates")
            .join("mcp-agent-mail-cli")
            .join("tests")
            .join("artifacts");
        let doctor_reports = storage.path().join("doctor").join("reports");
        let attachments = storage
            .path()
            .join("projects")
            .join("demo-project")
            .join("attachments");
        std::fs::create_dir_all(&test_artifacts).unwrap();
        std::fs::create_dir_all(&cli_artifacts).unwrap();
        std::fs::create_dir_all(&doctor_reports).unwrap();
        std::fs::create_dir_all(&attachments).unwrap();

        let perf_report = test_artifacts.join("report.json");
        let cli_report = cli_artifacts.join("failure_context.json");
        let doctor_report = doctor_reports.join("doctor.json");
        let attachment = attachments.join("payload.bin");
        std::fs::write(&perf_report, "perf").unwrap();
        std::fs::write(&cli_report, "cli").unwrap();
        std::fs::write(&doctor_report, "doctor").unwrap();
        std::fs::write(&attachment, vec![0u8; 16]).unwrap();

        let mut config = Config::default();
        config.storage_root = storage.path().to_path_buf();
        let report = artifact_retention_report(&config, repo.path(), 4);

        assert_eq!(report.schema_version, ARTIFACT_REPORT_SCHEMA_VERSION);
        assert!(report.totals.total_bytes >= 29);
        assert!(report.totals.total_files >= 4);
        assert!(
            report
                .roots
                .iter()
                .any(|root| root.name == "tests_artifacts"
                    && root.retention_class == "test_evidence"
                    && root.bytes == 4)
        );
        assert!(
            report
                .roots
                .iter()
                .any(|root| root.name == "project_attachments:demo-project"
                    && root.retention_class == "mail_attachment_evidence"
                    && root.bytes == 16)
        );

        assert!(
            perf_report.exists(),
            "inventory must not delete perf report"
        );
        assert!(
            cli_report.exists(),
            "inventory must not delete failure context"
        );
        assert!(
            doctor_report.exists(),
            "inventory must not delete doctor report"
        );
        assert!(attachment.exists(), "inventory must not delete attachment");
        assert!(
            report
                .safe_remediation
                .iter()
                .any(|line| line.contains("No automatic deletion"))
        );
    }

    #[test]
    fn artifact_retention_report_limits_largest_roots() {
        let repo = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("tests").join("artifacts")).unwrap();
        std::fs::create_dir_all(repo.path().join("refactor").join("artifacts")).unwrap();
        std::fs::write(
            repo.path()
                .join("tests")
                .join("artifacts")
                .join("small.json"),
            vec![0u8; 3],
        )
        .unwrap();
        std::fs::write(
            repo.path()
                .join("refactor")
                .join("artifacts")
                .join("large.md"),
            vec![0u8; 9],
        )
        .unwrap();

        let mut config = Config::default();
        config.storage_root = storage.path().to_path_buf();
        let report = artifact_retention_report(&config, repo.path(), 1);

        assert_eq!(report.largest_roots.len(), 1);
        assert_eq!(report.largest_roots[0].name, "refactor_artifacts");
        assert_eq!(report.largest_roots[0].bytes, 9);
    }
}
