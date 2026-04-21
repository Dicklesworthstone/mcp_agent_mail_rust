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
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{debug, info, warn};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const STARTUP_DELAY_SECS: u64 = 15;
const MIN_INTERVAL_SECS: u64 = 60;

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

/// Run the maintenance tasks on a given archive git directory.
/// This is the core function used by both the background worker and the CLI.
pub fn run_maintenance(git_dir: &Path) -> MaintenanceReport {
    let work_tree = git_dir.parent().unwrap_or(git_dir);
    let loose_before = count_loose_objects(git_dir);
    let pack_before = count_pack_files(git_dir);
    let disk_before = measure_objects_disk_usage(git_dir);

    let result = Command::new("nice")
        .args([
            "-n",
            "19",
            "ionice",
            "-c",
            "3",
            "git",
            "--git-dir",
            &git_dir.display().to_string(),
            "--work-tree",
            &work_tree.display().to_string(),
            "maintenance",
            "run",
            "--task=loose-objects",
            "--task=incremental-repack",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    let (success, error) = match result {
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

fn measure_objects_disk_usage(git_dir: &Path) -> Option<u64> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return None;
    }
    Some(dir_size_recursive(&objects_dir))
}

fn dir_size_recursive(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else if ft.is_dir() {
            total += dir_size_recursive(&entry.path());
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
