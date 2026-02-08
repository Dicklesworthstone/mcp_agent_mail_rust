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
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Global shutdown flag for the retention worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

/// Start the retention/quota report worker (if enabled).
///
/// Must be called at most once. Subsequent calls are no-ops.
pub fn start(config: &Config) {
    if !config.retention_report_enabled && !config.quota_enabled {
        return;
    }

    let config = config.clone();
    let _ = WORKER.get_or_init(|| {
        SHUTDOWN.store(false, Ordering::Release);
        std::thread::Builder::new()
            .name("retention-quota".into())
            .spawn(move || retention_loop(&config))
            .expect("failed to spawn retention/quota worker")
    });
}

/// Signal the worker to stop.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

fn retention_loop(config: &Config) {
    let interval = std::time::Duration::from_secs(config.retention_report_interval_seconds.max(60));

    info!(
        interval_secs = interval.as_secs(),
        retention_enabled = config.retention_report_enabled,
        quota_enabled = config.quota_enabled,
        storage_root = %config.storage_root.display(),
        "retention/quota report worker started"
    );

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

        // Sleep in small increments to allow quick shutdown.
        let mut remaining = interval;
        while !remaining.is_zero() {
            if SHUTDOWN.load(Ordering::Acquire) {
                return;
            }
            let chunk = remaining.min(std::time::Duration::from_secs(1));
            std::thread::sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

/// Summary of a retention/quota report cycle.
struct RetentionReport {
    projects_scanned: usize,
    total_attachment_bytes: u64,
    total_inbox_count: u64,
    warnings: usize,
}

/// Run a single retention/quota report cycle.
fn run_retention_cycle(config: &Config) -> Result<RetentionReport, String> {
    // Mailbox archive layout is `{storage_root}/projects/{project_slug}/...`.
    // Retention/quota logic should operate on per-project directories under `projects/`.
    let projects_root = config.storage_root.join("projects");
    if !projects_root.is_dir() {
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

        report.projects_scanned += 1;

        // Scan attachments.
        let attachments_dir = path.join("attachments");
        let attachment_bytes = dir_size(&attachments_dir);
        report.total_attachment_bytes += attachment_bytes;

        // Scan inbox (count .md files under agents/*/inbox/).
        let agents_dir = path.join("agents");
        let inbox_count = count_inbox_files(&agents_dir);
        report.total_inbox_count += inbox_count;

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
                report.warnings += 1;
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
                report.warnings += 1;
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
        if let Some(prefix) = pat.strip_suffix('*') {
            if name.starts_with(prefix) {
                return true;
            }
        } else if name == pat {
            return true;
        }
    }
    false
}

/// Recursively compute total size of a directory in bytes.
fn dir_size(path: &Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }

    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }

            let p = entry.path();
            if ft.is_file() {
                total += p.metadata().map_or(0, |m| m.len());
            } else if ft.is_dir() {
                total += dir_size(&p);
            }
        }
    }
    total
}

/// Count .md files recursively under agents/*/inbox/.
fn count_inbox_files(agents_dir: &Path) -> u64 {
    if !agents_dir.is_dir() {
        return 0;
    }

    let mut count = 0u64;
    if let Ok(agents) = std::fs::read_dir(agents_dir) {
        for agent in agents.flatten() {
            let inbox = agent.path().join("inbox");
            if inbox.is_dir() {
                count += count_md_files_recursive(&inbox);
            }
        }
    }
    count
}

/// Recursively count .md files in a directory.
fn count_md_files_recursive(dir: &Path) -> u64 {
    let mut count = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }

            let p = entry.path();
            if ft.is_file() && p.extension().is_some_and(|e| e == "md") {
                count += 1;
            } else if ft.is_dir() {
                count += count_md_files_recursive(&p);
            }
        }
    }
    count
}

/// Count messages older than `max_age_days` under agents/*/inbox/.
fn count_old_messages(agents_dir: &Path, max_age_days: u64) -> u64 {
    if !agents_dir.is_dir() {
        return 0;
    }

    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(max_age_days * 86400);

    let mut count = 0u64;
    if let Ok(agents) = std::fs::read_dir(agents_dir) {
        for agent in agents.flatten() {
            let inbox = agent.path().join("inbox");
            if inbox.is_dir() {
                count += count_old_files_recursive(&inbox, cutoff);
            }
        }
    }
    count
}

/// Count files older than cutoff in a directory tree.
fn count_old_files_recursive(dir: &Path, cutoff: std::time::SystemTime) -> u64 {
    let mut count = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }

            let p = entry.path();
            if ft.is_file() {
                if let Ok(metadata) = p.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if modified < cutoff {
                            count += 1;
                        }
                    }
                }
            } else if ft.is_dir() {
                count += count_old_files_recursive(&p, cutoff);
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
}
