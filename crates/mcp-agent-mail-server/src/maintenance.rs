//! Background worker for archive git maintenance (loose-object repack).
//!
//! Runs `git maintenance run --task=loose-objects` and, when packs already
//! exist, `--task=incremental-repack` on the archive's `.git` directory
//! periodically to prevent unbounded loose-object accumulation from
//! high-frequency commit patterns.
//!
//! Respects:
//! - `AM_ARCHIVE_MAINTENANCE_DISABLED=1` — disables the worker entirely
//! - `AM_ARCHIVE_MAINTENANCE_INTERVAL_SECS` — override the 1800s default

use mcp_agent_mail_core::Config;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
static MAINTENANCE_LOCK_QUARANTINE_COUNTER: AtomicU64 = AtomicU64::new(0);

const STARTUP_DELAY_SECS: u64 = 15;
const MIN_INTERVAL_SECS: u64 = 60;
const MAINTENANCE_COMMAND_TIMEOUT_SECS: u64 = 20 * 60;
const MAINTENANCE_TERMINATION_GRACE_SECS: u64 = 5;
const MAX_MAINTENANCE_LOCK_EVIDENCE_BYTES: u64 = 16 * 1024;
const MAINTENANCE_LOCK_EVIDENCE_PREFIX: &str = "maintenance.lock.agent-mail-stale-";
const MAINTENANCE_ACTIVE_EVIDENCE_PREFIX: &str = "maintenance.lock.agent-mail-active-";
const MAINTENANCE_COMPLETED_EVIDENCE_PREFIX: &str = "maintenance.lock.agent-mail-completed-";
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
    pub observed_effect: MaintenanceObservedEffect,
    pub lock_reaped: bool,
    pub lock_evidence: Option<String>,
    pub success: bool,
    pub error: Option<String>,
}

/// What the maintenance run measurably accomplished.
///
/// A zero exit status alone is not sufficient evidence: Git returns success
/// when `objects/maintenance.lock` makes it skip every requested task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MaintenanceObservedEffect {
    #[default]
    Unknown,
    NoWorkRequired,
    ProgressObserved,
    NoProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordedProcessLiveness {
    Alive,
    Dead,
    Reused,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct MaintenanceLockFingerprint {
    bytes: u64,
    modified_unix_nanos: u64,
}

/// Durable evidence that this process terminated a Git maintenance child while
/// its existence-based lock remained. The record lets a later worker distinguish
/// that known-dead child from an unrelated, currently-running Git invocation.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct TerminatedMaintenanceLockEvidence {
    schema_version: u8,
    git_pid: u32,
    git_process_start_ticks: Option<u64>,
    termination_reason: String,
    observed_at_unix_micros: u64,
    lock_fingerprint: Option<MaintenanceLockFingerprint>,
}

#[derive(Debug, Clone)]
struct ActiveMaintenanceEvidence {
    path: PathBuf,
}

#[derive(Debug, Default)]
struct MaintenanceLockPreflight {
    reaped: bool,
    evidence: Option<String>,
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
            .stack_size(mcp_agent_mail_core::worker_stack_size())
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
fn build_git_maintenance_command(
    git_dir: &Path,
    work_tree: &Path,
    include_incremental_repack: bool,
) -> Command {
    let use_ionice = cfg!(target_os = "linux") && executable_on_path("ionice");
    let use_nice = executable_on_path("nice");

    let mut argv: Vec<String> = Vec::with_capacity(15);
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
    ]);
    if include_incremental_repack {
        argv.push("--task=incremental-repack".to_string());
    }
    // Git otherwise emits no diagnostic while a maintenance.lock makes it
    // skip every task but still exits successfully (GH#234).
    argv.push("--no-quiet".to_string());

    // `argv[0]` is always set (`git` at minimum).
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

fn maintenance_lock_path(git_dir: &Path) -> PathBuf {
    git_dir.join("objects").join("maintenance.lock")
}

fn maintenance_lock_fingerprint(path: &Path) -> Option<MaintenanceLockFingerprint> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let modified_unix_nanos = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()?;
    Some(MaintenanceLockFingerprint {
        bytes: metadata.len(),
        modified_unix_nanos,
    })
}

fn maintenance_lock_evidence_path(
    objects_dir: &Path,
    pid: u32,
    observed_at_micros: u64,
) -> PathBuf {
    let nonce = MAINTENANCE_LOCK_QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed);
    objects_dir.join(format!(
        "{MAINTENANCE_LOCK_EVIDENCE_PREFIX}{observed_at_micros}-pid-{pid}-{nonce:06}.json"
    ))
}

fn active_maintenance_evidence_path(
    objects_dir: &Path,
    pid: u32,
    observed_at_micros: u64,
) -> PathBuf {
    let nonce = MAINTENANCE_LOCK_QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed);
    objects_dir.join(format!(
        "{MAINTENANCE_ACTIVE_EVIDENCE_PREFIX}{observed_at_micros}-pid-{pid}-{nonce:06}.json"
    ))
}

fn maintenance_lock_quarantine_path(lock_path: &Path) -> Option<PathBuf> {
    let observed_at_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    for _ in 0..1024 {
        let nonce = MAINTENANCE_LOCK_QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let file_name = format!(
            "maintenance.lock.agent-mail-quarantine-{observed_at_micros}-{}-{nonce:06}",
            std::process::id()
        );
        let candidate = lock_path.with_file_name(file_name);
        if matches!(fs::symlink_metadata(&candidate), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            return Some(candidate);
        }
    }
    None
}

fn process_start_ticks(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let close_paren = stat.rfind(')')?;
        let tail = stat.get(close_paren + 2..)?;
        // Fields after `comm` start with field 3. Process start time is field
        // 22, therefore position 19 in this tail segment.
        return tail.split_whitespace().nth(19)?.parse().ok();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn recorded_process_liveness(
    evidence: &TerminatedMaintenanceLockEvidence,
) -> RecordedProcessLiveness {
    if evidence.git_pid == 0 {
        return RecordedProcessLiveness::Unknown;
    }

    #[cfg(target_os = "linux")]
    {
        let proc_path = Path::new("/proc").join(evidence.git_pid.to_string());
        if !proc_path.is_dir() {
            return RecordedProcessLiveness::Dead;
        }
        if let (Some(expected), Some(actual)) = (
            evidence.git_process_start_ticks,
            process_start_ticks(evidence.git_pid),
        ) && expected != actual
        {
            return RecordedProcessLiveness::Reused;
        }
        return RecordedProcessLiveness::Alive;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let status = Command::new("ps")
            .args(["-p", &evidence.git_pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return match status {
            Ok(status) if status.success() => RecordedProcessLiveness::Alive,
            Ok(_) => RecordedProcessLiveness::Dead,
            Err(_) => RecordedProcessLiveness::Unknown,
        };
    }
}

/// Returns `Some` only when process inspection is available. A true value
/// means a related Git maintenance process still exists, so the lock is not
/// safe to reap even if its original parent was killed.
fn related_git_maintenance_process_running(git_dir: &Path) -> Option<bool> {
    let git_dir = git_dir.to_string_lossy();

    #[cfg(target_os = "linux")]
    {
        let proc_dir = fs::read_dir("/proc").ok()?;
        for entry in proc_dir.flatten() {
            let pid = entry.file_name();
            if pid.to_string_lossy().parse::<u32>().is_err() {
                continue;
            }
            let Ok(command) = fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let command = String::from_utf8_lossy(&command).replace('\0', " ");
            if command.contains(git_dir.as_ref())
                && command.contains("git")
                && command.contains("maintenance")
            {
                return Some(true);
            }
        }
        return Some(false);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-ax", "-o", "command="])
            .output()
            .ok()?;
        let commands = String::from_utf8_lossy(&output.stdout);
        Some(commands.lines().any(|command| {
            command.contains(git_dir.as_ref())
                && command.contains("git")
                && command.contains("maintenance")
        }))
    }
}

fn read_matching_maintenance_lock_evidence(
    git_dir: &Path,
    fingerprint: MaintenanceLockFingerprint,
) -> Option<TerminatedMaintenanceLockEvidence> {
    let objects_dir = git_dir.join("objects");
    fs::read_dir(objects_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy();
            if !name.starts_with(MAINTENANCE_LOCK_EVIDENCE_PREFIX)
                || path.extension().is_none_or(|extension| extension != "json")
            {
                return None;
            }
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_file()
                || metadata.len() > MAX_MAINTENANCE_LOCK_EVIDENCE_BYTES
            {
                return None;
            }
            let evidence = serde_json::from_str::<TerminatedMaintenanceLockEvidence>(
                &fs::read_to_string(path).ok()?,
            )
            .ok()?;
            (evidence.lock_fingerprint == Some(fingerprint)).then_some(evidence)
        })
        .max_by_key(|evidence| evidence.observed_at_unix_micros)
}

fn read_active_maintenance_evidence(git_dir: &Path) -> Option<TerminatedMaintenanceLockEvidence> {
    let objects_dir = git_dir.join("objects");
    fs::read_dir(objects_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy();
            if !name.starts_with(MAINTENANCE_ACTIVE_EVIDENCE_PREFIX)
                || path.extension().is_none_or(|extension| extension != "json")
            {
                return None;
            }
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_file()
                || metadata.len() > MAX_MAINTENANCE_LOCK_EVIDENCE_BYTES
            {
                return None;
            }
            let evidence = serde_json::from_str::<TerminatedMaintenanceLockEvidence>(
                &fs::read_to_string(path).ok()?,
            )
            .ok()?;
            evidence.lock_fingerprint.is_none().then_some(evidence)
        })
        .max_by_key(|evidence| evidence.observed_at_unix_micros)
}

fn write_maintenance_lock_evidence(
    evidence_path: &Path,
    evidence: &TerminatedMaintenanceLockEvidence,
) -> bool {
    let Ok(payload) = serde_json::to_vec_pretty(evidence) else {
        return false;
    };
    let Ok(mut file) = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(evidence_path)
    else {
        return false;
    };
    file.write_all(&payload).is_ok() && file.write_all(b"\n").is_ok() && file.flush().is_ok()
}

fn record_active_maintenance_evidence(
    git_dir: &Path,
    git_pid: u32,
    git_process_start_ticks: Option<u64>,
) -> Option<ActiveMaintenanceEvidence> {
    let objects_dir = git_dir.join("objects");
    let observed_at_unix_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .try_into()
        .ok()?;
    let evidence = TerminatedMaintenanceLockEvidence {
        schema_version: 1,
        git_pid,
        git_process_start_ticks,
        termination_reason: "maintenance child started".to_string(),
        observed_at_unix_micros,
        lock_fingerprint: None,
    };
    let path = active_maintenance_evidence_path(&objects_dir, git_pid, observed_at_unix_micros);
    write_maintenance_lock_evidence(&path, &evidence).then_some(ActiveMaintenanceEvidence { path })
}

fn mark_maintenance_evidence_completed(evidence: &ActiveMaintenanceEvidence) -> Result<(), String> {
    let Some(file_name) = evidence.path.file_name().and_then(|name| name.to_str()) else {
        return Err(format!(
            "active maintenance evidence has no UTF-8 file name: {}",
            evidence.path.display()
        ));
    };
    let Some(completed_name) = file_name.strip_prefix(MAINTENANCE_ACTIVE_EVIDENCE_PREFIX) else {
        return Err(format!(
            "active maintenance evidence has unexpected name: {}",
            evidence.path.display()
        ));
    };
    let completed_path = evidence.path.with_file_name(format!(
        "{MAINTENANCE_COMPLETED_EVIDENCE_PREFIX}{completed_name}"
    ));
    if fs::symlink_metadata(&completed_path).is_ok() {
        return Err(format!(
            "refusing to overwrite completed maintenance evidence at {}",
            completed_path.display()
        ));
    }
    fs::rename(&evidence.path, &completed_path).map_err(|error| {
        format!(
            "could not retain completed maintenance evidence at {}: {error}",
            completed_path.display()
        )
    })
}

fn record_terminated_maintenance_lock(
    git_dir: &Path,
    git_pid: u32,
    git_process_start_ticks: Option<u64>,
    termination_reason: &str,
) -> Option<String> {
    let lock_path = maintenance_lock_path(git_dir);
    let fingerprint = maintenance_lock_fingerprint(&lock_path)?;
    let objects_dir = lock_path.parent()?;
    let observed_at_unix_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .try_into()
        .ok()?;
    let evidence = TerminatedMaintenanceLockEvidence {
        schema_version: 1,
        git_pid,
        git_process_start_ticks,
        termination_reason: termination_reason.to_string(),
        observed_at_unix_micros,
        lock_fingerprint: Some(fingerprint),
    };
    let evidence_path =
        maintenance_lock_evidence_path(objects_dir, git_pid, observed_at_unix_micros);
    write_maintenance_lock_evidence(&evidence_path, &evidence).then_some(())?;
    Some(format!(
        "recorded stale maintenance lock evidence at {} for git pid {git_pid}",
        evidence_path.display()
    ))
}

/// Reap only a lock left by a maintenance child that this worker previously
/// terminated. A matching fingerprint prevents an older dead PID record from
/// being applied to a newer external Git invocation.
fn preflight_maintenance_lock(git_dir: &Path) -> MaintenanceLockPreflight {
    let lock_path = maintenance_lock_path(git_dir);
    let Some(fingerprint) = maintenance_lock_fingerprint(&lock_path) else {
        if lock_path.exists() {
            return MaintenanceLockPreflight {
                evidence: Some(format!(
                    "refusing maintenance lock at {} because it is not a regular file",
                    lock_path.display()
                )),
                ..Default::default()
            };
        }
        return MaintenanceLockPreflight::default();
    };

    let (evidence, evidence_kind) = if let Some(evidence) =
        read_matching_maintenance_lock_evidence(git_dir, fingerprint)
    {
        (evidence, "matching terminated-child")
    } else if let Some(evidence) = read_active_maintenance_evidence(git_dir) {
        // A server SIGKILL prevents the controlled termination path from
        // fingerprinting the lock. Its active record still proves which Git
        // child was launched; process-tree liveness below is the safety gate.
        (evidence, "active-child")
    } else {
        return MaintenanceLockPreflight {
            evidence: Some(format!(
                "maintenance lock at {} has no terminated-child PID evidence; refusing to run a silently skipped maintenance command",
                lock_path.display()
            )),
            ..Default::default()
        };
    };

    let liveness = recorded_process_liveness(&evidence);
    if !matches!(
        liveness,
        RecordedProcessLiveness::Dead | RecordedProcessLiveness::Reused
    ) {
        return MaintenanceLockPreflight {
            evidence: Some(format!(
                "maintenance lock at {} belongs to {evidence_kind} git pid {} ({liveness:?}); not reaping",
                lock_path.display(),
                evidence.git_pid
            )),
            ..Default::default()
        };
    }
    match related_git_maintenance_process_running(git_dir) {
        Some(false) => {}
        Some(true) => {
            return MaintenanceLockPreflight {
                evidence: Some(format!(
                    "maintenance lock at {} has dead/reused {evidence_kind} owner pid {} but a related Git maintenance process remains alive; not reaping",
                    lock_path.display(),
                    evidence.git_pid
                )),
                ..Default::default()
            };
        }
        None => {
            return MaintenanceLockPreflight {
                evidence: Some(format!(
                    "maintenance lock at {} has dead/reused {evidence_kind} owner pid {} but process-tree liveness is unavailable; not reaping",
                    lock_path.display(),
                    evidence.git_pid
                )),
                ..Default::default()
            };
        }
    }

    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(error) => {
            return MaintenanceLockPreflight {
                evidence: Some(format!(
                    "could not open stale maintenance lock {} for an exclusive safety probe: {error}",
                    lock_path.display()
                )),
                ..Default::default()
            };
        }
    };
    if fs2::FileExt::try_lock_exclusive(&file).is_err()
        || maintenance_lock_fingerprint(&lock_path) != Some(fingerprint)
    {
        let _ = fs2::FileExt::unlock(&file);
        return MaintenanceLockPreflight {
            evidence: Some(format!(
                "maintenance lock at {} changed or is advisory-lock busy during stale-lock verification; not reaping",
                lock_path.display()
            )),
            ..Default::default()
        };
    }

    let Some(quarantine_path) = maintenance_lock_quarantine_path(&lock_path) else {
        let _ = fs2::FileExt::unlock(&file);
        return MaintenanceLockPreflight {
            evidence: Some(format!(
                "could not allocate a non-overwriting quarantine path for stale maintenance lock {}",
                lock_path.display()
            )),
            ..Default::default()
        };
    };
    let reaped = fs::rename(&lock_path, &quarantine_path).is_ok();
    let _ = fs2::FileExt::unlock(&file);
    if reaped {
        MaintenanceLockPreflight {
            reaped: true,
            evidence: Some(format!(
                "quarantined stale maintenance lock at {} after {evidence_kind} git pid {} was {liveness:?}; retained lock artifact at {}",
                lock_path.display(),
                evidence.git_pid,
                quarantine_path.display()
            )),
        }
    } else {
        MaintenanceLockPreflight {
            evidence: Some(format!(
                "failed to quarantine stale maintenance lock at {} after {evidence_kind} git pid {} was {liveness:?}",
                lock_path.display(),
                evidence.git_pid
            )),
            ..Default::default()
        }
    }
}

fn maintenance_observed_effect(
    loose_before: Option<u64>,
    loose_after: Option<u64>,
    pack_before: Option<u64>,
    pack_after: Option<u64>,
    disk_before: Option<u64>,
    disk_after: Option<u64>,
) -> MaintenanceObservedEffect {
    let Some(loose_before) = loose_before else {
        return MaintenanceObservedEffect::Unknown;
    };
    if loose_before < LOOSE_OBJECTS_WATCH_AT {
        return MaintenanceObservedEffect::NoWorkRequired;
    }
    let (
        Some(loose_after),
        Some(pack_before),
        Some(pack_after),
        Some(disk_before),
        Some(disk_after),
    ) = (
        loose_after,
        pack_before,
        pack_after,
        disk_before,
        disk_after,
    )
    else {
        return MaintenanceObservedEffect::Unknown;
    };
    if loose_after < loose_before || pack_after != pack_before || disk_after < disk_before {
        MaintenanceObservedEffect::ProgressObserved
    } else {
        MaintenanceObservedEffect::NoProgress
    }
}

fn git_reported_maintenance_lock(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("maintenance.lock") || stderr.contains("maintenance is already running")
}

fn terminate_maintenance_child(
    child: &mut std::process::Child,
    git_dir: &Path,
    termination_reason: &str,
) -> Option<String> {
    let git_pid = child.id();
    let git_process_start_ticks = process_start_ticks(git_pid);

    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &git_pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let grace_deadline = Instant::now() + Duration::from_secs(MAINTENANCE_TERMINATION_GRACE_SECS);
    while Instant::now() < grace_deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                return record_terminated_maintenance_lock(
                    git_dir,
                    git_pid,
                    git_process_start_ticks,
                    termination_reason,
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    record_terminated_maintenance_lock(
        git_dir,
        git_pid,
        git_process_start_ticks,
        termination_reason,
    )
}

/// Run the maintenance tasks on a given archive git directory.
/// This is the core function used by both the background worker and the CLI.
pub fn run_maintenance(git_dir: &Path) -> MaintenanceReport {
    let work_tree = git_dir.parent().unwrap_or(git_dir);
    let loose_before = count_loose_objects(git_dir);
    let pack_before = count_pack_files(git_dir);
    let disk_before = measure_objects_disk_usage(git_dir);
    let lock_preflight = preflight_maintenance_lock(git_dir);
    if lock_preflight.evidence.is_some() && !lock_preflight.reaped {
        return MaintenanceReport {
            loose_before,
            loose_after: count_loose_objects(git_dir),
            pack_count_before: pack_before,
            pack_count_after: count_pack_files(git_dir),
            disk_bytes_before: disk_before,
            disk_bytes_after: measure_objects_disk_usage(git_dir),
            lock_evidence: lock_preflight.evidence.clone(),
            error: lock_preflight.evidence,
            ..Default::default()
        };
    }
    // A fresh archive can have loose objects but no `.pack` file. Git's
    // incremental-repack task treats that as an error (GH#233), so it must
    // only be requested after the loose-objects task has created a pack.
    let include_incremental_repack = pack_before.is_some_and(|count| count > 0);

    let mut child =
        match build_git_maintenance_command(git_dir, work_tree, include_incremental_repack)
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
                    lock_reaped: lock_preflight.reaped,
                    lock_evidence: lock_preflight.evidence,
                    error: Some(e.to_string()),
                    ..Default::default()
                };
            }
        };
    let active_evidence = match record_active_maintenance_evidence(
        git_dir,
        child.id(),
        process_start_ticks(child.id()),
    ) {
        Some(evidence) => evidence,
        None => {
            let lock_evidence = terminate_maintenance_child(
                &mut child,
                git_dir,
                "maintenance launch could not persist active PID evidence",
            );
            return MaintenanceReport {
                loose_before,
                loose_after: count_loose_objects(git_dir),
                pack_count_before: pack_before,
                pack_count_after: count_pack_files(git_dir),
                disk_bytes_before: disk_before,
                disk_bytes_after: measure_objects_disk_usage(git_dir),
                lock_reaped: lock_preflight.reaped,
                lock_evidence: lock_evidence.clone().or(lock_preflight.evidence),
                error: Some(lock_evidence.map_or_else(
                    || {
                        "could not persist active Git maintenance PID evidence; maintenance child was terminated"
                            .to_string()
                    },
                    |evidence| format!(
                        "could not persist active Git maintenance PID evidence; maintenance child was terminated; {evidence}"
                    ),
                )),
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
                    let lock_evidence =
                        terminate_maintenance_child(&mut child, git_dir, "interrupted by shutdown");
                    return MaintenanceReport {
                        loose_before,
                        loose_after: count_loose_objects(git_dir),
                        pack_count_before: pack_before,
                        pack_count_after: count_pack_files(git_dir),
                        disk_bytes_before: disk_before,
                        disk_bytes_after: measure_objects_disk_usage(git_dir),
                        lock_reaped: lock_preflight.reaped,
                        lock_evidence: lock_evidence.clone().or(lock_preflight.evidence),
                        error: Some(lock_evidence.map_or_else(
                            || "interrupted by shutdown".to_string(),
                            |evidence| format!("interrupted by shutdown; {evidence}"),
                        )),
                        ..Default::default()
                    };
                }
                if started.elapsed() >= Duration::from_secs(MAINTENANCE_COMMAND_TIMEOUT_SECS) {
                    let timeout_error =
                        format!("timed out after {MAINTENANCE_COMMAND_TIMEOUT_SECS}s");
                    let lock_evidence =
                        terminate_maintenance_child(&mut child, git_dir, &timeout_error);
                    return MaintenanceReport {
                        loose_before,
                        loose_after: count_loose_objects(git_dir),
                        pack_count_before: pack_before,
                        pack_count_after: count_pack_files(git_dir),
                        disk_bytes_before: disk_before,
                        disk_bytes_after: measure_objects_disk_usage(git_dir),
                        lock_reaped: lock_preflight.reaped,
                        lock_evidence: lock_evidence.clone().or(lock_preflight.evidence),
                        error: Some(lock_evidence.map_or_else(
                            || timeout_error.clone(),
                            |evidence| format!("{timeout_error}; {evidence}"),
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

    let evidence_completion_error = mark_maintenance_evidence_completed(&active_evidence).err();

    let loose_after = count_loose_objects(git_dir);
    let pack_after = count_pack_files(git_dir);
    let disk_after = measure_objects_disk_usage(git_dir);
    let observed_effect = maintenance_observed_effect(
        loose_before,
        loose_after,
        pack_before,
        pack_after,
        disk_before,
        disk_after,
    );
    let lock_after = maintenance_lock_path(git_dir).exists();
    let (success, error) = match output {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            (
                false,
                Some(format!("exit {}: {}", output.status, stderr.trim())),
            )
        }
        Ok(output) if git_reported_maintenance_lock(&String::from_utf8_lossy(&output.stderr)) => (
            false,
            Some(
                "git maintenance reported an existing maintenance lock; no task effect was accepted"
                    .to_string(),
            ),
        ),
        Ok(_) if lock_after => (
            false,
            Some(
                "git maintenance returned success but objects/maintenance.lock remains; refusing to accept a silent no-op"
                    .to_string(),
            ),
        ),
        Ok(_) if observed_effect == MaintenanceObservedEffect::NoProgress => (
            false,
            Some(
                "git maintenance returned success but high loose-object pressure had no observed effect"
                    .to_string(),
            ),
        ),
        Ok(_) if observed_effect == MaintenanceObservedEffect::Unknown => (
            false,
            Some(
                "git maintenance returned success but object metrics were unavailable, so its effect could not be verified"
                    .to_string(),
            ),
        ),
        Ok(_) if evidence_completion_error.is_some() => (
            false,
            evidence_completion_error,
        ),
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };

    MaintenanceReport {
        loose_before,
        loose_after,
        pack_count_before: pack_before,
        pack_count_after: pack_after,
        disk_bytes_before: disk_before,
        disk_bytes_after: disk_after,
        observed_effect,
        lock_reaped: lock_preflight.reaped,
        lock_evidence: lock_preflight.evidence,
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
            observed_effect = ?report.observed_effect,
            lock_reaped = report.lock_reaped,
            lock_evidence = report.lock_evidence.as_deref().unwrap_or("none"),
            "archive maintenance completed"
        );
    } else {
        warn!(
            git_dir = %git_dir.display(),
            error = report.error.as_deref().unwrap_or("unknown"),
            observed_effect = ?report.observed_effect,
            lock_reaped = report.lock_reaped,
            lock_evidence = report.lock_evidence.as_deref().unwrap_or("none"),
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
        let cmd =
            build_git_maintenance_command(Path::new("/tmp/x/.git"), Path::new("/tmp/x"), true);
        let program = cmd.get_program().to_string_lossy().into_owned();
        let argv: Vec<String> = std::iter::once(program.clone())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();

        // git is always invoked, with the maintenance subcommand and both tasks
        // when a pack already exists.
        assert!(!program.is_empty());
        assert!(argv.iter().any(|a| a == "git"), "git missing: {argv:?}");
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "maintenance" && w[1] == "run"),
            "maintenance run missing: {argv:?}"
        );
        assert!(argv.iter().any(|a| a == "--task=loose-objects"));
        assert!(argv.iter().any(|a| a == "--task=incremental-repack"));
        assert!(argv.iter().any(|a| a == "--no-quiet"));

        let zero_pack =
            build_git_maintenance_command(Path::new("/tmp/x/.git"), Path::new("/tmp/x"), false);
        let zero_pack_argv: Vec<String> =
            std::iter::once(zero_pack.get_program().to_string_lossy().into_owned())
                .chain(
                    zero_pack
                        .get_args()
                        .map(|argument| argument.to_string_lossy().into_owned()),
                )
                .collect();
        assert!(zero_pack_argv.iter().any(|a| a == "--task=loose-objects"));
        assert!(
            !zero_pack_argv
                .iter()
                .any(|a| a == "--task=incremental-repack"),
            "a fresh zero-pack archive must not request incremental-repack: {zero_pack_argv:?}"
        );

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
        let archive_root = tmp.path().join("archive");
        let init = Command::new("git")
            .args(["init", "--quiet", &archive_root.display().to_string()])
            .output()
            .expect("spawn git init");
        assert!(init.status.success(), "git init failed: {init:?}");
        let git_dir = archive_root.join(".git");

        SHUTDOWN.store(false, Ordering::Release);
        let report = run_maintenance(&git_dir);
        assert_eq!(report.loose_before, Some(0));
        assert_eq!(report.pack_count_before, Some(0));
        assert_eq!(
            report.observed_effect,
            MaintenanceObservedEffect::NoWorkRequired
        );
        assert!(
            report.success,
            "a fresh zero-pack archive must not invoke incremental-repack: {report:?}"
        );
    }

    #[test]
    fn stale_maintenance_lock_with_dead_recorded_pid_is_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let lock_path = maintenance_lock_path(&git_dir);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, b"stale maintenance sentinel").unwrap();
        let evidence = record_terminated_maintenance_lock(
            &git_dir,
            u32::MAX,
            None,
            "test killed maintenance child",
        );
        assert!(evidence.is_some(), "must record stale-lock PID evidence");

        let preflight = preflight_maintenance_lock(&git_dir);
        assert!(
            preflight.reaped,
            "dead child lock must be reaped: {preflight:?}"
        );
        assert!(
            !lock_path.exists(),
            "stale maintenance.lock must be moved aside"
        );
        assert!(
            preflight
                .evidence
                .as_deref()
                .is_some_and(|evidence| evidence.contains("retained lock artifact")),
            "reap result must retain a durable audit trail: {preflight:?}"
        );
    }

    #[test]
    fn stale_maintenance_lock_from_abruptly_killed_worker_is_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let lock_path = maintenance_lock_path(&git_dir);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, b"stale lock after worker kill").unwrap();
        let active = record_active_maintenance_evidence(&git_dir, u32::MAX, None);
        assert!(
            active.is_some(),
            "a launched child must have durable PID evidence before it can be killed"
        );

        let preflight = preflight_maintenance_lock(&git_dir);
        assert!(
            preflight.reaped,
            "a dead active child record must make its stale lock recoverable: {preflight:?}"
        );
        assert!(!lock_path.exists());
    }

    #[test]
    fn maintenance_lock_with_live_recorded_pid_is_not_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let lock_path = maintenance_lock_path(&git_dir);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, b"possibly live maintenance sentinel").unwrap();
        let evidence = record_terminated_maintenance_lock(
            &git_dir,
            std::process::id(),
            process_start_ticks(std::process::id()),
            "test live process",
        );
        assert!(evidence.is_some(), "must record stale-lock PID evidence");

        let preflight = preflight_maintenance_lock(&git_dir);
        assert!(!preflight.reaped, "live PID lock must not be reaped");
        assert!(lock_path.exists(), "live PID lock must remain in place");
        assert!(
            preflight
                .evidence
                .as_deref()
                .is_some_and(|evidence| evidence.contains("Alive")),
            "the refusal must disclose PID liveness evidence: {preflight:?}"
        );
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
