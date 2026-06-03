//! Background worker for archive git maintenance (loose-object repack).
//!
//! Runs `git maintenance run --task=loose-objects --task=incremental-repack`
//! on the archive's `.git` directory periodically to prevent unbounded
//! loose-object accumulation from high-frequency commit patterns.
//!
//! Respects:
//! - `AM_ARCHIVE_MAINTENANCE_DISABLED=1` — disables the worker entirely
//! - `AM_ARCHIVE_MAINTENANCE_INTERVAL_SECS` — override the 1800s default

use mcp_agent_mail_core::Config;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tracing::{debug, info, warn};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const STARTUP_DELAY_SECS: u64 = 15;
const MIN_INTERVAL_SECS: u64 = 60;
const MAINTENANCE_COMMAND_TIMEOUT_SECS: u64 = 20 * 60;
const PLAN_TOP_LIMIT: usize = 8;
const LOOSE_OBJECTS_WATCH_AT: u64 = 1_000;
const LOOSE_OBJECTS_CRITICAL_AT: u64 = 10_000;
const PACK_FILES_WATCH_AT: u64 = 16;
const PACK_FILES_CRITICAL_AT: u64 = 64;
const GIT_OBJECTS_BYTES_WATCH_AT: u64 = 512 * 1024 * 1024;
const GIT_OBJECTS_BYTES_CRITICAL_AT: u64 = 2 * 1024 * 1024 * 1024;
const GLOBAL_ARCHIVE_BYTES_WATCH_AT: u64 = 2 * 1024 * 1024 * 1024;
const GLOBAL_ARCHIVE_BYTES_CRITICAL_AT: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct MaintenanceReport {
    pub loose_before: Option<u64>,
    pub loose_after: Option<u64>,
    pub pack_count_before: Option<u64>,
    pub pack_count_after: Option<u64>,
    pub disk_bytes_before: Option<u64>,
    pub disk_bytes_after: Option<u64>,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveMaintenanceVerdict {
    Ok,
    Watch,
    MaintenanceRecommended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdVerdict {
    Unknown,
    Ok,
    Watch,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveThresholdVerdict {
    pub metric: String,
    pub value: Option<u64>,
    pub watch_at: u64,
    pub critical_at: u64,
    pub verdict: ThresholdVerdict,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveProjectSize {
    pub project_slug: String,
    pub bytes: u64,
    pub files: u64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveArtifactCategory {
    pub category: String,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveMaintenanceCommand {
    pub purpose: String,
    pub command: String,
    pub mutates_archive: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveMaintenancePlan {
    pub storage_root: String,
    pub git_dir: String,
    pub verdict: ArchiveMaintenanceVerdict,
    pub global_archive_bytes: u64,
    pub git_objects_bytes: Option<u64>,
    pub loose_objects: Option<u64>,
    pub pack_file_count: Option<u64>,
    pub pack_file_bytes: u64,
    pub oldest_pack_age_secs: Option<u64>,
    pub newest_pack_age_secs: Option<u64>,
    pub project_sizes: Vec<ArchiveProjectSize>,
    pub top_artifact_categories: Vec<ArchiveArtifactCategory>,
    pub threshold_verdicts: Vec<ArchiveThresholdVerdict>,
    pub safe_commands: Vec<ArchiveMaintenanceCommand>,
}

#[derive(Debug, Clone, Copy, Default)]
struct PathSummary {
    bytes: u64,
    files: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct PackFileSummary {
    count: u64,
    bytes: u64,
    oldest_age_secs: Option<u64>,
    newest_age_secs: Option<u64>,
}

pub fn start(config: &Config) {
    if !config.archive_maintenance_enabled {
        debug!("archive maintenance worker disabled via AM_ARCHIVE_MAINTENANCE_DISABLED");
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
            .name("archive-maintenance".into())
            .spawn(move || maintenance_loop(&config))
        {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                warn!(
                    error = %err,
                    "failed to spawn archive maintenance worker"
                );
                return;
            }
        }
    }
    drop(worker);
}

pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn maintenance_loop(config: &Config) {
    let interval_secs = config
        .archive_maintenance_interval_secs
        .max(MIN_INTERVAL_SECS);
    let interval = Duration::from_secs(interval_secs);

    info!(
        interval_secs,
        startup_delay_secs = STARTUP_DELAY_SECS,
        "archive maintenance worker started"
    );

    // Initial delay so we don't interfere with cold-start probes.
    if !sleep_interruptible(Duration::from_secs(STARTUP_DELAY_SECS)) {
        return;
    }

    // Run once immediately after startup delay.
    let git_dir = resolve_archive_git_dir(config);
    if let Some(ref dir) = git_dir {
        let report = run_maintenance(dir);
        log_report(&report, dir);
    } else {
        warn!(
            "archive maintenance: could not locate archive .git directory; will retry next cycle"
        );
    }

    loop {
        if !sleep_interruptible(interval) {
            info!("archive maintenance worker shutting down");
            return;
        }

        let git_dir = resolve_archive_git_dir(config);
        if let Some(ref dir) = git_dir {
            let report = run_maintenance(dir);
            log_report(&report, dir);
        }
    }
}

fn sleep_interruptible(duration: Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if SHUTDOWN.load(Ordering::Acquire) {
            return false;
        }
        let chunk = remaining.min(Duration::from_secs(1));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    !SHUTDOWN.load(Ordering::Acquire)
}

pub fn resolve_archive_git_dir(config: &Config) -> Option<PathBuf> {
    let storage_root = &config.storage_root;
    let git_dir = storage_root.join(".git");
    if git_dir.is_dir() {
        return Some(git_dir);
    }
    // Bare repo check.
    if storage_root.join("HEAD").is_file() && storage_root.join("objects").is_dir() {
        return Some(storage_root.clone());
    }
    None
}

/// Build a read-only bloat and safety plan for the archive repository.
pub fn plan_archive_maintenance(storage_root: &Path, git_dir: &Path) -> ArchiveMaintenancePlan {
    let global_archive = summarize_path_recursive(storage_root);
    let git_objects_bytes = measure_objects_disk_usage(git_dir);
    let loose_objects = count_loose_objects(git_dir);
    let pack_summary = summarize_pack_files(git_dir);
    let project_sizes = collect_project_sizes(storage_root);
    let top_artifact_categories = collect_artifact_categories(storage_root);

    let pack_file_count = pack_summary.map(|summary| summary.count);
    let threshold_verdicts = vec![
        threshold_verdict(
            "loose_objects",
            loose_objects,
            LOOSE_OBJECTS_WATCH_AT,
            LOOSE_OBJECTS_CRITICAL_AT,
        ),
        threshold_verdict(
            "pack_file_count",
            pack_file_count,
            PACK_FILES_WATCH_AT,
            PACK_FILES_CRITICAL_AT,
        ),
        threshold_verdict(
            "git_objects_bytes",
            git_objects_bytes,
            GIT_OBJECTS_BYTES_WATCH_AT,
            GIT_OBJECTS_BYTES_CRITICAL_AT,
        ),
        threshold_verdict(
            "global_archive_bytes",
            Some(global_archive.bytes),
            GLOBAL_ARCHIVE_BYTES_WATCH_AT,
            GLOBAL_ARCHIVE_BYTES_CRITICAL_AT,
        ),
    ];
    let verdict = archive_verdict(&threshold_verdicts);

    ArchiveMaintenancePlan {
        storage_root: storage_root.display().to_string(),
        git_dir: git_dir.display().to_string(),
        verdict,
        global_archive_bytes: global_archive.bytes,
        git_objects_bytes,
        loose_objects,
        pack_file_count,
        pack_file_bytes: pack_summary.map_or(0, |summary| summary.bytes),
        oldest_pack_age_secs: pack_summary.and_then(|summary| summary.oldest_age_secs),
        newest_pack_age_secs: pack_summary.and_then(|summary| summary.newest_age_secs),
        project_sizes,
        top_artifact_categories,
        threshold_verdicts,
        safe_commands: safe_maintenance_commands(storage_root),
    }
}

/// Locate an executable on `$PATH`.
///
/// Used to decide whether the low-priority wrappers (`nice`/`ionice`) are
/// available before invoking them, so archive maintenance degrades gracefully
/// instead of exec-failing with exit 127 on hosts that lack them (#137).
fn executable_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
}

/// Build the `git maintenance run` command, wrapping it in `nice`/`ionice` only
/// when those tools actually exist on this host.
///
/// `ionice` is util-linux-only: it is absent on macOS/BSD (#137) and may be
/// stripped from minimal Linux images. Invoking it unconditionally made `nice`
/// exec-fail with exit 127 and broke every archive-maintenance run plus
/// `am doctor pack-archive`. `nice` is POSIX and kept when present; when neither
/// wrapper is available we fall back to a bare `git`, which always exists.
fn build_git_maintenance_command(git_dir: &Path, work_tree: &Path) -> Command {
    let use_ionice = cfg!(target_os = "linux") && executable_on_path("ionice");
    let use_nice = executable_on_path("nice");

    let mut argv: Vec<String> = Vec::with_capacity(14);
    if use_nice {
        argv.extend(["nice".to_string(), "-n".to_string(), "19".to_string()]);
    }
    if use_ionice {
        argv.extend(["ionice".to_string(), "-c".to_string(), "3".to_string()]);
    }
    argv.extend([
        "git".to_string(),
        "--git-dir".to_string(),
        git_dir.display().to_string(),
        "--work-tree".to_string(),
        work_tree.display().to_string(),
        "maintenance".to_string(),
        "run".to_string(),
        "--task=loose-objects".to_string(),
        "--task=incremental-repack".to_string(),
    ]);

    // `argv[0]` is always set (`git` at minimum).
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

/// Run the maintenance tasks on a given archive git directory.
/// This is the core function used by both the background worker and the CLI.
pub fn run_maintenance(git_dir: &Path) -> MaintenanceReport {
    let work_tree = git_dir.parent().unwrap_or(git_dir);
    let loose_before = count_loose_objects(git_dir);
    let pack_before = count_pack_files(git_dir);
    let disk_before = measure_objects_disk_usage(git_dir);

    let mut child = match build_git_maintenance_command(git_dir, work_tree)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return MaintenanceReport {
                loose_before,
                pack_count_before: pack_before,
                disk_bytes_before: disk_before,
                error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };

    // Poll the child process, checking for shutdown signal so we don't
    // block server exit if git maintenance hangs.
    let started = Instant::now();
    let output = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break child.wait_with_output(),
            Ok(None) => {
                if SHUTDOWN.load(Ordering::Acquire) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return MaintenanceReport {
                        loose_before,
                        pack_count_before: pack_before,
                        disk_bytes_before: disk_before,
                        error: Some("interrupted by shutdown".to_string()),
                        ..Default::default()
                    };
                }
                if started.elapsed() >= Duration::from_secs(MAINTENANCE_COMMAND_TIMEOUT_SECS) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return MaintenanceReport {
                        loose_before,
                        pack_count_before: pack_before,
                        disk_bytes_before: disk_before,
                        error: Some(format!(
                            "timed out after {MAINTENANCE_COMMAND_TIMEOUT_SECS}s"
                        )),
                        ..Default::default()
                    };
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                break Err(e);
            }
        }
    };

    let (success, error) = match output {
        Ok(output) if output.status.success() => (true, None),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            (
                false,
                Some(format!("exit {}: {}", output.status, stderr.trim())),
            )
        }
        Err(e) => (false, Some(e.to_string())),
    };

    let loose_after = count_loose_objects(git_dir);
    let pack_after = count_pack_files(git_dir);
    let disk_after = measure_objects_disk_usage(git_dir);

    MaintenanceReport {
        loose_before,
        loose_after,
        pack_count_before: pack_before,
        pack_count_after: pack_after,
        disk_bytes_before: disk_before,
        disk_bytes_after: disk_after,
        success,
        error,
    }
}

fn log_report(report: &MaintenanceReport, git_dir: &Path) {
    if report.success {
        let removed = report
            .loose_before
            .zip(report.loose_after)
            .map(|(b, a)| b.saturating_sub(a));
        let reclaimed = report
            .disk_bytes_before
            .zip(report.disk_bytes_after)
            .map(|(b, a)| b.saturating_sub(a));
        info!(
            git_dir = %git_dir.display(),
            loose_before = report.loose_before,
            loose_after = report.loose_after,
            removed = removed,
            packs_before = report.pack_count_before,
            packs_after = report.pack_count_after,
            bytes_reclaimed = reclaimed,
            "archive maintenance completed"
        );
    } else {
        warn!(
            git_dir = %git_dir.display(),
            error = report.error.as_deref().unwrap_or("unknown"),
            "archive maintenance failed"
        );
    }
}

fn count_loose_objects(git_dir: &Path) -> Option<u64> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return None;
    }
    let mut count = 0u64;
    for entry in std::fs::read_dir(&objects_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name_str = name.to_str().unwrap_or("");
        // Loose objects are in 2-hex-char subdirectories (00..ff).
        if name_str.len() == 2 && name_str.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(subdir) = std::fs::read_dir(entry.path()) {
                count += subdir.count() as u64;
            }
        }
    }
    Some(count)
}

fn count_pack_files(git_dir: &Path) -> Option<u64> {
    let pack_dir = git_dir.join("objects").join("pack");
    if !pack_dir.is_dir() {
        return Some(0);
    }
    let count = std::fs::read_dir(&pack_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
        .count() as u64;
    Some(count)
}

fn summarize_pack_files(git_dir: &Path) -> Option<PackFileSummary> {
    let pack_dir = git_dir.join("objects").join("pack");
    if !pack_dir.is_dir() {
        return Some(PackFileSummary::default());
    }

    let now = SystemTime::now();
    let mut summary = PackFileSummary::default();
    for entry in std::fs::read_dir(&pack_dir).ok()?.flatten() {
        if entry.path().extension().is_none_or(|ext| ext != "pack") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        summary.count += 1;
        summary.bytes = summary.bytes.saturating_add(metadata.len());
        if let Ok(modified) = metadata.modified()
            && let Ok(age) = now.duration_since(modified)
        {
            let age_secs = age.as_secs();
            summary.oldest_age_secs = Some(
                summary
                    .oldest_age_secs
                    .map_or(age_secs, |oldest| oldest.max(age_secs)),
            );
            summary.newest_age_secs = Some(
                summary
                    .newest_age_secs
                    .map_or(age_secs, |newest| newest.min(age_secs)),
            );
        }
    }
    Some(summary)
}

fn measure_objects_disk_usage(git_dir: &Path) -> Option<u64> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return None;
    }
    Some(dir_size_recursive(&objects_dir))
}

fn dir_size_recursive(path: &Path) -> u64 {
    summarize_path_recursive(path).bytes
}

fn summarize_path_recursive(path: &Path) -> PathSummary {
    let mut summary = PathSummary::default();
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return summary;
    };
    if metadata.is_file() {
        return PathSummary {
            bytes: metadata.len(),
            files: 1,
        };
    }
    if !metadata.is_dir() {
        return summary;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return summary;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_file() {
            summary.files = summary.files.saturating_add(1);
            summary.bytes = summary
                .bytes
                .saturating_add(entry.metadata().map(|metadata| metadata.len()).unwrap_or(0));
        } else if ft.is_dir() {
            let child = summarize_path_recursive(&entry.path());
            summary.files = summary.files.saturating_add(child.files);
            summary.bytes = summary.bytes.saturating_add(child.bytes);
        }
    }
    summary
}

fn collect_project_sizes(storage_root: &Path) -> Vec<ArchiveProjectSize> {
    let projects_dir = storage_root.join("projects");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else {
        return Vec::new();
    };

    let mut projects = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }
            let summary = summarize_path_recursive(&entry.path());
            Some(ArchiveProjectSize {
                project_slug: entry.file_name().to_string_lossy().into_owned(),
                bytes: summary.bytes,
                files: summary.files,
                path: entry.path().display().to_string(),
            })
        })
        .collect::<Vec<_>>();
    projects.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.project_slug.cmp(&b.project_slug))
    });
    projects
}

fn collect_artifact_categories(storage_root: &Path) -> Vec<ArchiveArtifactCategory> {
    let mut categories = BTreeMap::<String, PathSummary>::new();
    collect_project_artifact_categories(storage_root, &mut categories);
    collect_root_artifact_categories(storage_root, &mut categories);

    let mut categories = categories
        .into_iter()
        .map(|(category, summary)| ArchiveArtifactCategory {
            category,
            bytes: summary.bytes,
            files: summary.files,
        })
        .collect::<Vec<_>>();
    categories.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.category.cmp(&b.category))
    });
    categories.truncate(PLAN_TOP_LIMIT);
    categories
}

fn collect_project_artifact_categories(
    storage_root: &Path,
    categories: &mut BTreeMap<String, PathSummary>,
) {
    let projects_dir = storage_root.join("projects");
    let Ok(project_entries) = std::fs::read_dir(projects_dir) else {
        return;
    };
    for project in project_entries.flatten() {
        if !project.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let category = if name == "project.json" {
                "project_metadata".to_string()
            } else {
                name
            };
            add_category_summary(
                categories,
                category,
                summarize_path_recursive(&entry.path()),
            );
        }
    }
}

fn collect_root_artifact_categories(
    storage_root: &Path,
    categories: &mut BTreeMap<String, PathSummary>,
) {
    let Ok(entries) = std::fs::read_dir(storage_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "projects") {
            continue;
        }
        let category = match name.as_str() {
            "storage.sqlite3" | "storage.sqlite3-shm" | "storage.sqlite3-wal" => "database",
            ".setup-self-heal" => "setup_self_heal",
            other => other,
        }
        .to_string();
        add_category_summary(
            categories,
            category,
            summarize_path_recursive(&entry.path()),
        );
    }
}

fn add_category_summary(
    categories: &mut BTreeMap<String, PathSummary>,
    category: String,
    summary: PathSummary,
) {
    let entry = categories.entry(category).or_default();
    entry.bytes = entry.bytes.saturating_add(summary.bytes);
    entry.files = entry.files.saturating_add(summary.files);
}

fn threshold_verdict(
    metric: &str,
    value: Option<u64>,
    watch_at: u64,
    critical_at: u64,
) -> ArchiveThresholdVerdict {
    let verdict = match value {
        Some(value) if value >= critical_at => ThresholdVerdict::Critical,
        Some(value) if value >= watch_at => ThresholdVerdict::Watch,
        Some(_) => ThresholdVerdict::Ok,
        None => ThresholdVerdict::Unknown,
    };
    ArchiveThresholdVerdict {
        metric: metric.to_string(),
        value,
        watch_at,
        critical_at,
        verdict,
    }
}

fn archive_verdict(thresholds: &[ArchiveThresholdVerdict]) -> ArchiveMaintenanceVerdict {
    if thresholds
        .iter()
        .any(|threshold| threshold.verdict == ThresholdVerdict::Critical)
    {
        return ArchiveMaintenanceVerdict::MaintenanceRecommended;
    }
    if thresholds
        .iter()
        .any(|threshold| threshold.verdict == ThresholdVerdict::Watch)
    {
        return ArchiveMaintenanceVerdict::Watch;
    }
    ArchiveMaintenanceVerdict::Ok
}

fn safe_maintenance_commands(storage_root: &Path) -> Vec<ArchiveMaintenanceCommand> {
    let storage_root = shell_arg(storage_root);
    vec![
        ArchiveMaintenanceCommand {
            purpose: "Re-run this read-only planner as JSON".to_string(),
            command: format!("STORAGE_ROOT={storage_root} am doctor pack-archive --plan --json"),
            mutates_archive: false,
        },
        ArchiveMaintenanceCommand {
            purpose: "Inspect native Git object counts without changing files".to_string(),
            command: format!("git -C {storage_root} count-objects -vH"),
            mutates_archive: false,
        },
        ArchiveMaintenanceCommand {
            purpose: "Run safe Git maintenance through Agent Mail".to_string(),
            command: format!("STORAGE_ROOT={storage_root} am doctor pack-archive --json"),
            mutates_archive: true,
        },
    ]
}

fn shell_arg(path: &Path) -> String {
    let value = path.display().to_string();
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value;
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn build_git_maintenance_command_is_well_formed_and_guards_ionice() {
        let cmd = build_git_maintenance_command(Path::new("/tmp/x/.git"), Path::new("/tmp/x"));
        let program = cmd.get_program().to_string_lossy().into_owned();
        let argv: Vec<String> = std::iter::once(program.clone())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();

        // git is always invoked, with the maintenance subcommand and both tasks.
        assert!(!program.is_empty());
        assert!(argv.iter().any(|a| a == "git"), "git missing: {argv:?}");
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "maintenance" && w[1] == "run"),
            "maintenance run missing: {argv:?}"
        );
        assert!(argv.iter().any(|a| a == "--task=loose-objects"));
        assert!(argv.iter().any(|a| a == "--task=incremental-repack"));

        // #137: ionice (util-linux only) must never be invoked off Linux.
        if !cfg!(target_os = "linux") {
            assert!(
                !argv.iter().any(|a| a == "ionice"),
                "ionice must not run off Linux: {argv:?}"
            );
        }
    }

    fn create_fake_git_objects_dir(tmp: &Path) -> PathBuf {
        let git_dir = tmp.join(".git");
        let objects_dir = git_dir.join("objects");
        // Create a couple of "loose object" directories.
        let loose_dir = objects_dir.join("ab");
        fs::create_dir_all(&loose_dir).unwrap();
        fs::write(
            loose_dir.join("cdef1234567890abcdef1234567890abcdef12"),
            b"fake",
        )
        .unwrap();
        fs::write(
            loose_dir.join("1111222233334444555566667777888899990000"),
            b"fake2",
        )
        .unwrap();

        let loose_dir2 = objects_dir.join("cd");
        fs::create_dir_all(&loose_dir2).unwrap();
        fs::write(
            loose_dir2.join("ef5678901234567890123456789012345678"),
            b"obj",
        )
        .unwrap();

        // Create pack directory with one pack.
        let pack_dir = objects_dir.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        fs::write(pack_dir.join("pack-abc123.pack"), b"packdata").unwrap();
        fs::write(pack_dir.join("pack-abc123.idx"), b"idxdata").unwrap();

        // Create HEAD for bare-repo detection fallback.
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();

        git_dir
    }

    #[test]
    fn count_loose_objects_finds_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = create_fake_git_objects_dir(tmp.path());
        assert_eq!(count_loose_objects(&git_dir), Some(3));
    }

    #[test]
    fn count_pack_files_finds_packs() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = create_fake_git_objects_dir(tmp.path());
        assert_eq!(count_pack_files(&git_dir), Some(1));
    }

    #[test]
    fn measure_objects_disk_usage_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = create_fake_git_objects_dir(tmp.path());
        let size = measure_objects_disk_usage(&git_dir).unwrap();
        assert!(size > 0);
    }

    #[test]
    fn count_loose_objects_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let objects_dir = git_dir.join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        assert_eq!(count_loose_objects(&git_dir), Some(0));
    }

    #[test]
    fn count_pack_files_no_pack_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let objects_dir = git_dir.join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        assert_eq!(count_pack_files(&git_dir), Some(0));
    }

    #[test]
    fn resolve_archive_git_dir_finds_dotgit() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let config = Config {
            storage_root: tmp.path().to_path_buf(),
            ..Config::default()
        };
        assert_eq!(resolve_archive_git_dir(&config), Some(git_dir));
    }

    #[test]
    fn resolve_archive_git_dir_finds_bare() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(root.join("objects")).unwrap();
        let config = Config {
            storage_root: root.to_path_buf(),
            ..Config::default()
        };
        assert_eq!(resolve_archive_git_dir(&config), Some(root.to_path_buf()));
    }

    #[test]
    fn resolve_archive_git_dir_returns_none_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: tmp.path().join("nonexistent"),
            ..Config::default()
        };
        assert_eq!(resolve_archive_git_dir(&config), None);
    }

    #[test]
    fn maintenance_report_defaults_to_failure() {
        let report = MaintenanceReport::default();
        assert!(!report.success);
    }

    #[test]
    fn sleep_interruptible_returns_true_on_zero() {
        assert!(sleep_interruptible(Duration::ZERO));
    }

    #[test]
    fn start_disabled_does_not_spawn_worker() {
        let config = Config {
            archive_maintenance_enabled: false,
            ..Config::default()
        };
        start(&config);
        let worker = WORKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            worker.is_none(),
            "worker should not be spawned when disabled"
        );
    }

    #[test]
    fn interval_floor_is_enforced() {
        let config = Config {
            archive_maintenance_interval_secs: 10,
            ..Config::default()
        };
        let clamped = config
            .archive_maintenance_interval_secs
            .max(MIN_INTERVAL_SECS);
        assert_eq!(clamped, MIN_INTERVAL_SECS);
    }

    #[test]
    fn run_maintenance_on_empty_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let report = run_maintenance(&git_dir);
        assert_eq!(report.loose_before, Some(0));
        assert_eq!(report.pack_count_before, Some(0));
    }

    #[test]
    fn plan_archive_maintenance_reports_projects_categories_and_verdicts() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = create_fake_git_objects_dir(tmp.path());
        let loose_dir = git_dir.join("objects").join("ef");
        fs::create_dir_all(&loose_dir).unwrap();
        for idx in 0..LOOSE_OBJECTS_WATCH_AT {
            fs::write(loose_dir.join(format!("{idx:038x}")), b"x").unwrap();
        }

        let project = tmp.path().join("projects").join("data-projects-demo");
        fs::create_dir_all(project.join("messages").join("2026")).unwrap();
        fs::create_dir_all(project.join("agents").join("BlueLake")).unwrap();
        fs::create_dir_all(project.join("file_reservations")).unwrap();
        fs::write(project.join("project.json"), b"{\"slug\":\"demo\"}").unwrap();
        fs::write(project.join("messages").join("2026").join("m.md"), b"hello").unwrap();
        fs::write(
            project.join("agents").join("BlueLake").join("profile.json"),
            b"{}",
        )
        .unwrap();
        fs::write(project.join("file_reservations").join("r.json"), b"{}").unwrap();
        fs::write(tmp.path().join("storage.sqlite3"), b"sqlite").unwrap();

        let plan = plan_archive_maintenance(tmp.path(), &git_dir);

        assert!(plan.global_archive_bytes > 0);
        assert_eq!(plan.loose_objects, Some(LOOSE_OBJECTS_WATCH_AT + 3));
        assert_eq!(plan.verdict, ArchiveMaintenanceVerdict::Watch);
        assert!(plan.project_sizes.iter().any(|project| {
            project.project_slug == "data-projects-demo" && project.bytes > 0 && project.files > 0
        }));
        assert!(
            plan.top_artifact_categories
                .iter()
                .any(|category| category.category == "messages" && category.bytes > 0)
        );
        assert!(plan.threshold_verdicts.iter().any(|threshold| {
            threshold.metric == "loose_objects" && threshold.verdict == ThresholdVerdict::Watch
        }));
        assert!(plan.safe_commands.iter().any(|command| {
            !command.mutates_archive && command.command.contains("pack-archive --plan --json")
        }));
        assert!(plan.safe_commands.iter().any(|command| {
            command.mutates_archive && command.command.contains("pack-archive --json")
        }));
    }

    #[test]
    fn plan_archive_maintenance_reports_pack_age_and_is_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = create_fake_git_objects_dir(tmp.path());
        let loose_before = count_loose_objects(&git_dir);

        let plan = plan_archive_maintenance(tmp.path(), &git_dir);

        assert_eq!(plan.pack_file_count, Some(1));
        assert!(plan.pack_file_bytes > 0);
        assert!(plan.oldest_pack_age_secs.is_some());
        assert!(plan.newest_pack_age_secs.is_some());
        assert_eq!(count_loose_objects(&git_dir), loose_before);
    }
}
