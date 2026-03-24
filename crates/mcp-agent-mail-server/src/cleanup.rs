//! Background worker for file reservation cleanup.
//!
//! Mirrors legacy Python `_worker_cleanup` in `http.py`:
//! - Phase 1: release expired reservations (`expires_ts <= now`)
//! - Phase 2: release stale reservations by inactivity heuristics
//! - Logs via structlog + optional rich panel
//!
//! The worker runs on a dedicated OS thread with `std::thread::sleep` between
//! iterations, matching the WBQ pattern in `mcp-agent-mail-storage`.

#![forbid(unsafe_code)]

use asupersync::{Cx, Outcome};
use fastmcp_core::block_on;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{
    DbPool, DbPoolConfig, FileReservationRow, create_pool, now_micros,
    queries::{
        self, get_agent_last_mail_activity, list_unreleased_file_reservations,
        project_ids_with_active_reservations, release_expired_reservations,
        release_reservations_by_ids_returning_ids,
    },
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Global shutdown flag for the cleanup worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const PROBE_CACHE_RETENTION_US: i64 = 6 * 60 * 60 * 1_000_000;

fn normalize_path_pattern_key(path_pattern: &str) -> String {
    path_pattern.trim().trim_start_matches('/').to_string()
}

#[derive(Debug, Default)]
struct PathProbeCacheEntry {
    /// Upper bound (epoch micros) where filesystem activity is still known
    /// recent for this pattern.
    fs_recent_until_us: i64,
    /// Git HEAD seen when `git_latest_commit_us` was last computed.
    git_head_oid: Option<String>,
    /// Latest commit touching this path pattern at `git_head_oid`.
    git_latest_commit_us: Option<i64>,
    /// Last cycle timestamp where this entry was touched.
    last_used_us: i64,
}

#[derive(Debug, Default)]
struct CleanupProbeCache {
    path_probes: HashMap<(i64, String), PathProbeCacheEntry>,
}

impl CleanupProbeCache {
    fn prune_stale(&mut self, now_us: i64) {
        self.path_probes.retain(|_, entry| {
            now_us.saturating_sub(entry.last_used_us) <= PROBE_CACHE_RETENTION_US
        });
    }
}

/// Start the file reservation cleanup worker (if enabled).
///
/// Must be called at most once. Subsequent calls are no-ops.
pub fn start(config: &Config) {
    if !config.file_reservations_cleanup_enabled {
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
            .name("file-res-cleanup".into())
            .spawn(move || {
                cleanup_loop(&config);
            }) {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                warn!(
                    error = %err,
                    "failed to spawn file reservation cleanup worker; continuing without cleanup background scans"
                );
                return;
            }
        }
    }
    drop(worker);
}

/// Signal the worker to stop and wait for it to finish.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(handle) = worker.take() {
        let _ = handle.join();
    }
}

fn cleanup_loop(config: &Config) {
    let interval =
        std::time::Duration::from_secs(config.file_reservations_cleanup_interval_seconds.max(5));
    let startup_delay = interval.min(std::time::Duration::from_secs(8));

    let mut pool_config = DbPoolConfig::from_env();
    pool_config.database_url.clone_from(&config.database_url);
    pool_config.min_connections = 1;
    pool_config.max_connections = 1;
    pool_config.warmup_connections = 0;
    // HTTP/TUI startup already runs readiness_check with migrations before
    // this worker starts, so keep the worker path lean.
    pool_config.run_migrations = false;
    let pool = match create_pool(&pool_config) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "cleanup worker: failed to create DB pool, exiting");
            return;
        }
    };

    info!(
        interval_secs = interval.as_secs(),
        "file reservation cleanup worker started"
    );

    if startup_delay > std::time::Duration::ZERO {
        info!(
            startup_delay_secs = startup_delay.as_secs(),
            "file reservation cleanup worker startup delay engaged"
        );
        if sleep_with_shutdown(startup_delay) {
            return;
        }
    }

    let mut probe_cache = CleanupProbeCache::default();

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("file reservation cleanup worker shutting down");
            return;
        }

        // Run one cleanup cycle, suppressing all errors (legacy: never crash server).
        match run_cleanup_cycle_with_cache(config, &pool, &mut probe_cache) {
            Ok((projects_scanned, released)) => {
                info!(
                    event = "file_reservations_cleanup",
                    projects_scanned,
                    stale_released = released,
                    "file reservation cleanup completed"
                );
            }
            Err(e) => {
                warn!(error = %e, "file reservation cleanup cycle failed");
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

/// Run a single cleanup cycle across all projects.
///
/// Returns `(projects_scanned, total_released)`.
#[cfg(test)]
fn run_cleanup_cycle(config: &Config, pool: &DbPool) -> Result<(usize, usize), String> {
    let mut probe_cache = CleanupProbeCache::default();
    run_cleanup_cycle_with_cache(config, pool, &mut probe_cache)
}

fn run_cleanup_cycle_with_cache(
    config: &Config,
    pool: &DbPool,
    probe_cache: &mut CleanupProbeCache,
) -> Result<(usize, usize), String> {
    // This worker runs on a dedicated OS thread outside the async runtime,
    // so there is no parent Cx to derive from. Cx::for_testing() provides
    // Budget::INFINITE which is correct for a long-running background worker
    // that should never be cancelled by budget exhaustion.
    let cx = Cx::for_testing();

    // Get all project IDs with active reservations.
    let project_ids =
        match block_on(async { project_ids_with_active_reservations(&cx, pool).await }) {
            Outcome::Ok(ids) => ids,
            other => return Err(format!("failed to list projects: {other:?}")),
        };

    let mut total_released = 0usize;

    for pid in &project_ids {
        // Phase 1: release expired.
        let expired_ids =
            match block_on(async { release_expired_reservations(&cx, pool, *pid).await }) {
                Outcome::Ok(ids) => ids,
                Outcome::Err(err) => {
                    warn!(
                        project_id = *pid,
                        error = %err,
                        "cleanup: failed to release expired reservations for project"
                    );
                    Vec::new()
                }
                Outcome::Cancelled(_) => {
                    warn!(
                        project_id = *pid,
                        "cleanup: expired reservation release was cancelled for project"
                    );
                    Vec::new()
                }
                Outcome::Panicked(panic) => {
                    warn!(
                        project_id = *pid,
                        panic = %panic.message(),
                        "cleanup: expired reservation release panicked for project"
                    );
                    Vec::new()
                }
            };
        total_released += expired_ids.len();

        // Phase 2: detect and release stale.
        let stale_ids = match detect_and_release_stale(config, pool, &cx, *pid, probe_cache) {
            Ok(ids) => ids,
            Err(err) => {
                warn!(
                    project_id = *pid,
                    error = %err,
                    "cleanup: failed to detect and release stale reservations for project"
                );
                Vec::new()
            }
        };
        total_released += stale_ids.len();

        // Write archive artifacts for released reservations.
        if !expired_ids.is_empty()
            && let Err(err) = write_cleanup_artifacts(config, pool, &cx, *pid, &expired_ids)
        {
            warn!(
                project_id = *pid,
                phase = "expired",
                released_count = expired_ids.len(),
                error = %err,
                "cleanup: failed to enqueue archive updates for released reservations"
            );
        }
        if !stale_ids.is_empty()
            && let Err(err) = write_cleanup_artifacts(config, pool, &cx, *pid, &stale_ids)
        {
            warn!(
                project_id = *pid,
                phase = "stale",
                released_count = stale_ids.len(),
                error = %err,
                "cleanup: failed to enqueue archive updates for released reservations"
            );
        }
    }

    probe_cache.prune_stale(now_micros());
    Ok((project_ids.len(), total_released))
}

/// Phase 2: Detect stale reservations by inactivity heuristics and release them.
///
/// A reservation is stale when ALL of:
/// - Not already released
/// - Agent is inactive (`last_active_ts` > `inactivity_seconds` ago)
/// - No recent mail activity within `activity_grace_seconds`
/// - No recent filesystem activity within `activity_grace_seconds`
/// - No recent git activity within `activity_grace_seconds`
fn detect_and_release_stale(
    config: &Config,
    pool: &DbPool,
    cx: &Cx,
    project_id: i64,
    probe_cache: &mut CleanupProbeCache,
) -> Result<Vec<i64>, String> {
    let inactivity_us = i64::try_from(config.file_reservation_inactivity_seconds)
        .unwrap_or(1800)
        .saturating_mul(1_000_000);
    let grace_us = i64::try_from(config.file_reservation_activity_grace_seconds)
        .unwrap_or(900)
        .saturating_mul(1_000_000);
    let now = now_micros();

    let active = active_reservations_for_stale_cleanup(cx, pool, project_id, now)?;
    if active.is_empty() {
        return Ok(Vec::new());
    }

    // Project workspace is identical for every reservation in this cycle.
    let workspace = stale_cleanup_workspace(cx, pool, project_id)?;
    let git_head_oid = git_head_oid_for_workspace(&workspace);

    // Many reservations share the same agent and/or path pattern. Cache activity
    // checks within a cycle to avoid repeated DB + git process work.
    let mut inactive_agent_cache: HashMap<i64, bool> = HashMap::new();
    let mut recent_mail_cache: HashMap<i64, bool> = HashMap::new();
    let mut recent_path_activity_cache: HashMap<String, bool> = HashMap::new();
    let mut stale_ids = Vec::new();

    for res in &active {
        // Check agent inactivity, cached by agent id.
        let agent_inactive = inactive_agent_cache
            .get(&res.agent_id)
            .copied()
            .unwrap_or_else(|| {
                let computed = match block_on(async {
                    queries::get_agent_by_id(cx, pool, res.agent_id).await
                }) {
                    Outcome::Ok(agent) => {
                        // Reaper-exempt agents are never classified as inactive.
                        if agent.reaper_exempt != 0 {
                            false
                        } else {
                            now.saturating_sub(agent.last_active_ts) > inactivity_us
                        }
                    }
                    _ => false, // Skip stale classification when agent lookup fails.
                };
                inactive_agent_cache.insert(res.agent_id, computed);
                computed
            });
        if !agent_inactive {
            continue; // Agent is recently active, not stale.
        }

        // Check mail activity grace period, cached by agent id.
        let recent_mail = recent_mail_cache
            .get(&res.agent_id)
            .copied()
            .unwrap_or_else(|| {
                let last_mail = match block_on(async {
                    get_agent_last_mail_activity(cx, pool, res.agent_id, project_id).await
                }) {
                    Outcome::Ok(ts) => ts,
                    _ => None,
                };
                let computed = last_mail.is_some_and(|ts| now.saturating_sub(ts) <= grace_us);
                recent_mail_cache.insert(res.agent_id, computed);
                computed
            });
        if recent_mail {
            continue; // Recent mail activity, not stale.
        }

        // Check filesystem/git activity, cached by path pattern.
        let normalized_pattern = normalize_path_pattern_key(&res.path_pattern);
        let has_recent_path_activity = recent_path_activity_cache
            .get(&normalized_pattern)
            .copied()
            .unwrap_or_else(|| {
                let computed = path_has_recent_activity_cached(
                    probe_cache,
                    &workspace,
                    project_id,
                    &normalized_pattern,
                    git_head_oid.as_deref(),
                    now,
                    grace_us,
                );
                recent_path_activity_cache.insert(normalized_pattern.clone(), computed);
                computed
            });
        if has_recent_path_activity {
            continue;
        }

        // All checks negative — reservation is stale.
        if let Some(id) = res.id {
            stale_ids.push(id);
        }
    }

    if stale_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Bulk-release stale reservations.
    match block_on(async { release_reservations_by_ids_returning_ids(cx, pool, &stale_ids).await })
    {
        Outcome::Ok(released_ids) => Ok(released_ids),
        other => Err(format!("failed to release stale reservations: {other:?}")),
    }
}

fn active_reservations_for_stale_cleanup(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    now: i64,
) -> Result<Vec<FileReservationRow>, String> {
    match block_on(async { list_unreleased_file_reservations(cx, pool, project_id).await }) {
        Outcome::Ok(rows) => Ok(rows
            .into_iter()
            .filter(|row| row.expires_ts > now)
            .collect()),
        other => Err(format!("failed to list reservations: {other:?}")),
    }
}

fn stale_cleanup_workspace(cx: &Cx, pool: &DbPool, project_id: i64) -> Result<PathBuf, String> {
    match block_on(async { queries::get_project_by_id(cx, pool, project_id).await }) {
        Outcome::Ok(project) => {
            let workspace = PathBuf::from(project.human_key);
            if workspace.as_os_str().is_empty() {
                return Err(format!(
                    "project {project_id} has no workspace path; refusing stale cleanup without filesystem evidence"
                ));
            }
            if !workspace.exists() {
                return Err(format!(
                    "project {project_id} workspace does not exist: {}",
                    workspace.display()
                ));
            }
            Ok(workspace)
        }
        Outcome::Err(err) => Err(format!(
            "project lookup failed for stale cleanup on project {project_id}: {err}"
        )),
        Outcome::Cancelled(reason) => Err(format!(
            "project lookup cancelled for stale cleanup on project {project_id}: {reason:?}"
        )),
        Outcome::Panicked(panic) => Err(format!(
            "project lookup panicked for stale cleanup on project {project_id}: {}",
            panic.message()
        )),
    }
}

fn path_has_recent_activity_cached(
    cache: &mut CleanupProbeCache,
    workspace: &Path,
    project_id: i64,
    path_pattern: &str,
    git_head_oid: Option<&str>,
    now_us: i64,
    grace_us: i64,
) -> bool {
    let normalized_pattern = normalize_path_pattern_key(path_pattern);
    if normalized_pattern.is_empty() {
        return false;
    }
    let key = (project_id, normalized_pattern.clone());
    let entry = cache.path_probes.entry(key).or_default();
    entry.last_used_us = now_us;

    // Filesystem side: only reuse known-positive activity through grace window.
    if entry.fs_recent_until_us > now_us {
        return true;
    }
    let recent_fs = check_filesystem_activity(workspace, &normalized_pattern, now_us, grace_us);
    if recent_fs {
        entry.fs_recent_until_us = now_us.saturating_add(grace_us);
        return true;
    }
    entry.fs_recent_until_us = 0;

    // Git side: cache latest matching commit at a specific HEAD.
    let Some(current_head) = git_head_oid else {
        entry.git_head_oid = None;
        entry.git_latest_commit_us = None;
        return false;
    };

    if entry.git_head_oid.as_deref() != Some(current_head) {
        entry.git_head_oid = Some(current_head.to_string());
        entry.git_latest_commit_us = git_latest_commit_us(workspace, &normalized_pattern);
    }
    entry
        .git_latest_commit_us
        .is_some_and(|commit_us| now_us.saturating_sub(commit_us) <= grace_us)
}

const ACTIVITY_PROBE_PATH_LIMIT: usize = 5_000;

fn path_modified_within_grace(path: &Path, now_us: i64, grace_us: i64) -> bool {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .is_some_and(|modified| {
            let mtime_us = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(0));
            now_us.saturating_sub(mtime_us) <= grace_us
        })
}

enum ActivityProbeResult {
    Active,
    Inactive,
    Truncated,
    Unsupported,
}

fn check_git_listed_activity(
    workspace: &Path,
    pathspec: &str,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new("git")
        .args([
            "-C",
            &workspace.to_string_lossy(),
            "ls-files",
            "-c",
            "-o",
            "--exclude-standard",
            "--",
            pathspec,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return ActivityProbeResult::Unsupported;
    };

    let Some(stdout) = child.stdout.take() else {
        return ActivityProbeResult::Inactive;
    };

    let reader = BufReader::new(stdout);
    let mut count = 0;
    for line in reader.lines().map_while(Result::ok) {
        if path_modified_within_grace(&workspace.join(line), now_us, grace_us) {
            let _ = child.kill();
            let _ = child.wait();
            return ActivityProbeResult::Active;
        }
        count += 1;
        if count >= ACTIVITY_PROBE_PATH_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return ActivityProbeResult::Truncated;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    ActivityProbeResult::Inactive
}

fn check_directory_activity_fallback(
    dir: &Path,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    for (scanned, entry) in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .enumerate()
    {
        if scanned >= ACTIVITY_PROBE_PATH_LIMIT {
            return ActivityProbeResult::Truncated;
        }
        if path_modified_within_grace(entry.path(), now_us, grace_us) {
            return ActivityProbeResult::Active;
        }
    }
    ActivityProbeResult::Inactive
}

fn check_glob_activity_fallback(
    workspace: &Path,
    pattern: &str,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    let base_str = workspace.to_string_lossy().replace('\\', "/");
    let base_escaped = glob::Pattern::escape(&base_str);
    // We use format! instead of Path::join because base_escaped is a string
    // that may contain glob escape sequences that Path::join could mishandle.
    let full_pattern = if base_str.ends_with('/') {
        format!("{base_escaped}{pattern}")
    } else {
        format!("{base_escaped}/{pattern}")
    };

    let Ok(paths) = glob::glob(&full_pattern) else {
        return ActivityProbeResult::Unsupported;
    };

    for (scanned, entry) in paths.flatten().enumerate() {
        if scanned >= ACTIVITY_PROBE_PATH_LIMIT {
            return ActivityProbeResult::Truncated;
        }
        if path_modified_within_grace(&entry, now_us, grace_us) {
            return ActivityProbeResult::Active;
        }
    }
    ActivityProbeResult::Inactive
}

/// Check if any matched files have recent filesystem activity.
fn check_filesystem_activity(
    workspace: &Path,
    path_pattern: &str,
    now_us: i64,
    grace_us: i64,
) -> bool {
    if !workspace.exists() {
        return false;
    }

    let pattern = normalize_path_pattern_key(path_pattern);
    if pattern.is_empty() {
        return false;
    }

    let has_glob = pattern.contains('*')
        || pattern.contains('?')
        || pattern.contains('[')
        || pattern.contains('{');

    if has_glob {
        let pathspec = format!(":(glob){pattern}");
        // Fast path: let git enumerate matching files so ignored trees such as
        // `target/` do not explode synchronous traversal cost.
        match check_git_listed_activity(workspace, &pathspec, now_us, grace_us) {
            ActivityProbeResult::Inactive => return false,
            ActivityProbeResult::Active | ActivityProbeResult::Truncated => return true, // Fail closed to preserve reservation
            ActivityProbeResult::Unsupported => {} // Fall through to glob
        }

        // Fallback: glob traversal for non-git workspaces or truncated git scans.
        // If the fallback also truncates, fail closed and keep the reservation.
        return matches!(
            check_glob_activity_fallback(workspace, &pattern, now_us, grace_us),
            ActivityProbeResult::Active | ActivityProbeResult::Truncated
        );
    }

    let candidate = workspace.join(&pattern);
    if candidate.is_dir() {
        match check_git_listed_activity(workspace, &pattern, now_us, grace_us) {
            ActivityProbeResult::Inactive => return false,
            ActivityProbeResult::Active | ActivityProbeResult::Truncated => return true, // Fail closed
            ActivityProbeResult::Unsupported => {}
        }
        return matches!(
            check_directory_activity_fallback(&candidate, now_us, grace_us),
            ActivityProbeResult::Active | ActivityProbeResult::Truncated
        );
    }
    if candidate.exists() && path_modified_within_grace(&candidate, now_us, grace_us) {
        return true;
    }

    false
}

/// Check if any matched files have recent git commit activity.
#[cfg(test)]
fn check_git_activity(workspace: &Path, path_pattern: &str, now_us: i64, grace_us: i64) -> bool {
    git_latest_commit_us(workspace, path_pattern)
        .is_some_and(|commit_us| now_us.saturating_sub(commit_us) <= grace_us)
}

fn git_head_oid_for_workspace(workspace: &Path) -> Option<String> {
    if !workspace.exists() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["-C", &workspace.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head.is_empty() { None } else { Some(head) }
}

fn git_latest_commit_us(workspace: &Path, path_pattern: &str) -> Option<i64> {
    if !workspace.exists() {
        return None;
    }

    let pattern = normalize_path_pattern_key(path_pattern);
    if pattern.is_empty() {
        return None;
    }

    let has_glob = pattern.contains('*')
        || pattern.contains('?')
        || pattern.contains('[')
        || pattern.contains('{');
    let pathspec = if has_glob {
        format!(":(glob){pattern}")
    } else {
        pattern
    };

    let output = std::process::Command::new("git")
        .args([
            "-C",
            &workspace.to_string_lossy(),
            "log",
            "-1",
            "--format=%ct",
            "--",
            &pathspec,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit_epoch = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<i64>()
        .ok()?;
    Some(commit_epoch.saturating_mul(1_000_000))
}

/// Collect filesystem paths matching a reservation pattern.
///
/// Mirrors legacy `_collect_matching_paths`: if the pattern contains glob chars,
/// use globbing; otherwise treat as a literal path.
#[cfg(test)]
fn collect_matching_paths(base: &Path, pattern: &str) -> Vec<std::path::PathBuf> {
    let pattern = normalize_path_pattern_key(pattern);
    if pattern.is_empty() {
        return Vec::new();
    }

    let has_glob = pattern.contains('*')
        || pattern.contains('?')
        || pattern.contains('[')
        || pattern.contains('{');

    if has_glob {
        let base_str = base.to_string_lossy().replace('\\', "/");
        let base_escaped = glob::Pattern::escape(&base_str);
        // We use format! instead of Path::join because base_escaped is a string
        // that may contain glob escape sequences that Path::join could mishandle.
        let full_pattern = if base_str.ends_with('/') {
            format!("{base_escaped}{pattern}")
        } else {
            format!("{base_escaped}/{pattern}")
        };
        glob::glob(&full_pattern)
            .map(|paths| paths.filter_map(Result::ok).collect())
            .unwrap_or_default()
    } else {
        let candidate = base.join(pattern);
        if candidate.exists() {
            vec![candidate]
        } else {
            Vec::new()
        }
    }
}

/// Record cleanup releases to logs (best-effort).
fn write_cleanup_artifacts(
    config: &Config,
    pool: &DbPool,
    cx: &Cx,
    project_id: i64,
    released_ids: &[i64],
) -> Result<(), String> {
    let Outcome::Ok(project) =
        block_on(async { queries::get_project_by_id(cx, pool, project_id).await })
    else {
        return Err("project lookup failed".into());
    };

    let Outcome::Ok(target_reservations) =
        block_on(async { queries::get_reservations_by_ids(cx, pool, released_ids).await })
    else {
        return Err("failed to list reservations for artifact generation".into());
    };

    let mut res_jsons = Vec::new();
    for row in target_reservations {
        if let Some(id) = row.id {
            // We need the agent name, which isn't in FileReservationRow, so we look it up
            let agent_name = match block_on(async {
                queries::get_agent_by_id_fresh(cx, pool, row.agent_id).await
            }) {
                Outcome::Ok(agent) => agent.name,
                Outcome::Err(error) => {
                    return Err(format!(
                        "cleanup artifact agent lookup failed for reservation {id}: {error}"
                    ));
                }
                Outcome::Cancelled(_) => {
                    return Err(format!(
                        "cleanup artifact agent lookup cancelled for reservation {id}"
                    ));
                }
                Outcome::Panicked(panic) => {
                    return Err(format!(
                        "cleanup artifact agent lookup panicked for reservation {id}: {}",
                        panic.message()
                    ));
                }
            };
            let released_ts = row.released_ts.ok_or_else(|| {
                format!("cleanup artifact generation requires released_ts for reservation {id}")
            })?;

            res_jsons.push(serde_json::json!({
                "id": id,
                "agent": agent_name,
                "path_pattern": row.path_pattern,
                "exclusive": row.exclusive != 0,
                "reason": row.reason,
                "created_ts": mcp_agent_mail_db::micros_to_iso(row.created_ts),
                "expires_ts": mcp_agent_mail_db::micros_to_iso(row.expires_ts),
                "released_ts": mcp_agent_mail_db::micros_to_iso(released_ts),
            }));
        }
    }

    if !res_jsons.is_empty() {
        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: config.clone(),
            reservations: res_jsons,
        };
        match mcp_agent_mail_storage::wbq_enqueue(op.clone()) {
            mcp_agent_mail_storage::WbqEnqueueResult::Enqueued => {
                info!(
                    project = %project.slug,
                    released_count = released_ids.len(),
                    "cleanup: released expired/stale reservations and enqueued archive updates"
                );
                return Ok(());
            }
            mcp_agent_mail_storage::WbqEnqueueResult::QueueUnavailable => {
                mcp_agent_mail_storage::write_op_sync(&op).map_err(|error| {
                    format!(
                        "cleanup archive fallback write failed after queue unavailability: {error}"
                    )
                })?;
                info!(
                    project = %project.slug,
                    released_count = released_ids.len(),
                    "cleanup: WBQ unavailable; wrote released reservation archive updates synchronously"
                );
                return Ok(());
            }
            mcp_agent_mail_storage::WbqEnqueueResult::SkippedDiskCritical => {
                return Err("cleanup archive enqueue skipped due to critical disk pressure".into());
            }
        }
    }

    info!(
        project = %project.slug,
        released_count = released_ids.len(),
        "cleanup: released reservations but found no archive rows to update"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{Cx, Outcome};
    use mcp_agent_mail_core::Config;
    use mcp_agent_mail_db::{DbPoolConfig, create_pool, queries};

    #[test]
    fn collect_matching_literal_path() {
        let tmp = std::env::temp_dir().join("cleanup_test_literal");
        let _ = std::fs::create_dir_all(&tmp);
        let test_file = tmp.join("foo.rs");
        std::fs::write(&test_file, "test").unwrap();

        let matches = collect_matching_paths(&tmp, "foo.rs");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], test_file);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_matching_glob_pattern() {
        let tmp = std::env::temp_dir().join("cleanup_test_glob");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("a.rs"), "").unwrap();
        std::fs::write(tmp.join("b.rs"), "").unwrap();
        std::fs::write(tmp.join("c.txt"), "").unwrap();

        let matches = collect_matching_paths(&tmp, "*.rs");
        assert!(matches.len() >= 2, "expected >=2 .rs files: {matches:?}");
        assert!(
            matches
                .iter()
                .all(|p| p.extension().is_some_and(|e| e == "rs"))
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_matching_empty_pattern() {
        let tmp = std::env::temp_dir();
        assert!(collect_matching_paths(&tmp, "").is_empty());
        assert!(collect_matching_paths(&tmp, "  ").is_empty());
    }

    #[test]
    fn collect_matching_nonexistent_base() {
        let fake = Path::new("/nonexistent/path/foo");
        assert!(collect_matching_paths(fake, "*.rs").is_empty());
    }

    #[test]
    fn collect_matching_invalid_glob_pattern_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(collect_matching_paths(tmp.path(), "[unterminated").is_empty());
    }

    #[test]
    fn collect_matching_question_mark_glob() {
        let tmp = std::env::temp_dir().join("cleanup_test_qmark");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("a.rs"), "").unwrap();
        std::fs::write(tmp.join("b.rs"), "").unwrap();
        std::fs::write(tmp.join("ab.rs"), "").unwrap(); // Won't match ?.rs

        let matches = collect_matching_paths(&tmp, "?.rs");
        assert!(
            matches.len() >= 2,
            "?.rs should match single-char filenames: {matches:?}"
        );
        // ab.rs should NOT match ?.rs (two chars before extension).
        assert!(
            !matches
                .iter()
                .any(|p| p.file_name().is_some_and(|f| f == "ab.rs")),
            "ab.rs should not match ?.rs"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_matching_whitespace_only_pattern() {
        let tmp = std::env::temp_dir();
        assert!(collect_matching_paths(&tmp, "   \t  ").is_empty());
    }

    #[test]
    fn collect_matching_nested_glob() {
        let tmp = std::env::temp_dir().join("cleanup_test_nested");
        let sub = tmp.join("sub");
        let _ = std::fs::create_dir_all(&sub);
        std::fs::write(sub.join("deep.rs"), "").unwrap();
        std::fs::write(tmp.join("shallow.rs"), "").unwrap();

        let matches = collect_matching_paths(&tmp, "**/*.rs");
        assert!(
            matches.len() >= 2,
            "**/*.rs should match files in subdirectories too: {matches:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filesystem_activity_nonexistent_workspace() {
        let fake = Path::new("/definitely/does/not/exist");
        assert!(!check_filesystem_activity(
            fake,
            "*.rs",
            now_micros(),
            1_000_000
        ));
    }

    #[test]
    fn filesystem_activity_no_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Workspace exists but no files match the pattern.
        assert!(!check_filesystem_activity(
            tmp.path(),
            "nonexistent.rs",
            now_micros(),
            1_000_000
        ));
    }

    #[test]
    fn git_activity_nonexistent_workspace() {
        let fake = Path::new("/definitely/does/not/exist");
        assert!(!check_git_activity(fake, "*.rs", now_micros(), 1_000_000));
    }

    fn make_test_pool(tmp: &tempfile::TempDir) -> DbPool {
        // Use the standard pool setup to mirror production initialization
        // semantics under FrankenSQLite.
        let db_path = tmp.path().join("db.sqlite3");
        let db_url = format!(
            "sqlite:////{}",
            db_path.to_string_lossy().trim_start_matches('/')
        );
        let pool_config = DbPoolConfig {
            database_url: db_url,
            min_connections: 1,
            max_connections: 1,
            ..Default::default()
        };
        create_pool(&pool_config).expect("create pool")
    }

    fn seed_active_reservation(
        tmp: &tempfile::TempDir,
    ) -> (DbPool, Cx, i64, i64, i64, String, String) {
        let pool = make_test_pool(tmp);
        let cx = Cx::for_testing();

        let project_root = tmp.path().join("project_root_active");
        std::fs::create_dir_all(&project_root).unwrap();
        let human_key = project_root.to_string_lossy().to_string();

        let project = match fastmcp_core::block_on(async {
            queries::ensure_project(&cx, &pool, &human_key).await
        }) {
            Outcome::Ok(p) => p,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let project_id = project.id.expect("project id");

        let agent = match fastmcp_core::block_on(async {
            queries::register_agent(
                &cx,
                &pool,
                project_id,
                "GreenLake",
                "test",
                "test",
                None,
                None,
                None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent failed: {other:?}"),
        };
        let agent_id = agent.id.expect("agent id");

        let path_pattern = "src/missing_file.rs".to_string();
        let created = match fastmcp_core::block_on(async {
            queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &[path_pattern.as_str()],
                3_600, // active reservation (1h)
                true,
                "test-active",
            )
            .await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("create_file_reservations failed: {other:?}"),
        };
        let reservation_id = created[0].id.expect("reservation id");

        (
            pool,
            cx,
            project_id,
            agent_id,
            reservation_id,
            human_key,
            path_pattern,
        )
    }

    #[test]
    fn cleanup_cycle_releases_expired_reservations() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = make_test_pool(&tmp);
        let cx = Cx::for_testing();

        let project_root = tmp.path().join("project_root");
        std::fs::create_dir_all(&project_root).unwrap();
        let human_key = project_root.to_string_lossy().to_string();

        let project = match fastmcp_core::block_on(async {
            queries::ensure_project(&cx, &pool, &human_key).await
        }) {
            Outcome::Ok(p) => p,
            other => panic!("ensure_project failed: {other:?}"),
        };
        let project_id = project.id.expect("project id");

        let agent = match fastmcp_core::block_on(async {
            queries::register_agent(
                &cx, &pool, project_id, "RedFox", "test", "test", None, None, None,
            )
            .await
        }) {
            Outcome::Ok(a) => a,
            other => panic!("register_agent failed: {other:?}"),
        };
        let agent_id = agent.id.expect("agent id");

        let created = match fastmcp_core::block_on(async {
            queries::create_file_reservations(
                &cx,
                &pool,
                project_id,
                agent_id,
                &["src/**"],
                -1, // already expired
                true,
                "test-expired",
            )
            .await
        }) {
            Outcome::Ok(rows) => rows,
            other => panic!("create_file_reservations failed: {other:?}"),
        };
        assert_eq!(created.len(), 1);
        let id = created[0].id.expect("reservation id");

        let config = Config::from_env();
        let (projects_scanned, released) = run_cleanup_cycle(&config, &pool).expect("run cleanup");
        assert_eq!(projects_scanned, 1);
        assert_eq!(released, 1);

        let rows = match fastmcp_core::block_on(async {
            queries::list_file_reservations(&cx, &pool, project_id, false).await
        }) {
            Outcome::Ok(r) => r,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        let row = rows
            .iter()
            .find(|r| r.id.is_some_and(|rid| rid == id))
            .expect("reservation should exist");
        assert!(
            row.released_ts.is_some(),
            "expired reservation should be released"
        );
    }

    #[test]
    fn cleanup_cycle_with_no_active_reservations_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = make_test_pool(&tmp);
        let config = Config::from_env();

        let (projects_scanned, released) = run_cleanup_cycle(&config, &pool).expect("run cleanup");
        assert_eq!(projects_scanned, 0);
        assert_eq!(released, 0);
    }

    #[test]
    fn check_filesystem_activity_detects_recent_then_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let file = workspace.join("active.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let now = now_micros();
        assert!(check_filesystem_activity(
            workspace,
            "active.rs",
            now,
            1_000_000
        ));
        assert!(!check_filesystem_activity(
            workspace,
            "active.rs",
            now + 10_000_000,
            1_000_000
        ));
    }

    #[test]
    fn check_filesystem_activity_detects_recent_nested_file_for_literal_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let nested_dir = workspace.join("src");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("lib.rs");
        std::fs::write(&nested_file, "pub fn nested() {}\n").unwrap();

        let now = now_micros();
        assert!(check_filesystem_activity(workspace, "src", now, 1_000_000));
        assert!(!check_filesystem_activity(
            workspace,
            "src",
            now + 10_000_000,
            1_000_000
        ));
    }

    #[test]
    fn check_filesystem_activity_falls_back_when_git_glob_probe_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let nested_dir = workspace.join("src");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("lib.rs");
        std::fs::write(&nested_file, "pub fn nested() {}\n").unwrap();

        let now = now_micros();
        assert!(
            check_filesystem_activity(workspace, "src/**/*.rs", now, 1_000_000),
            "non-git workspaces should fall back to glob scanning when git ls-files fails"
        );
    }

    #[test]
    fn check_filesystem_activity_falls_back_when_git_directory_probe_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let nested_dir = workspace.join("src");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("lib.rs");
        std::fs::write(&nested_file, "pub fn nested() {}\n").unwrap();

        let now = now_micros();
        assert!(
            check_filesystem_activity(workspace, "src", now, 1_000_000),
            "non-git workspaces should fall back to directory scanning when git ls-files fails"
        );
    }

    #[test]
    fn check_filesystem_activity_glob_probe_limit_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let src = workspace.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for idx in 0..=ACTIVITY_PROBE_PATH_LIMIT {
            std::fs::write(src.join(format!("file_{idx:04}.rs")), "pub fn stale() {}\n").unwrap();
        }

        let now = now_micros().saturating_add(10_000_000);
        assert!(
            check_filesystem_activity(workspace, "src/**/*.rs", now, 1_000_000),
            "truncated glob scans must fail closed to avoid releasing active reservations unsafely"
        );
    }

    #[test]
    fn check_filesystem_activity_directory_probe_limit_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let src = workspace.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for idx in 0..=ACTIVITY_PROBE_PATH_LIMIT {
            std::fs::write(src.join(format!("file_{idx:04}.rs")), "pub fn stale() {}\n").unwrap();
        }

        let now = now_micros().saturating_add(10_000_000);
        assert!(
            check_filesystem_activity(workspace, "src", now, 1_000_000),
            "truncated directory scans must fail closed to avoid releasing active reservations unsafely"
        );
    }

    #[test]
    fn check_git_activity_returns_false_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "fn x() {}").unwrap();

        let now = now_micros();
        assert!(!check_git_activity(tmp.path(), "file.rs", now, 1_000_000));
    }

    #[test]
    fn check_git_activity_detects_recent_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let file = repo.join("tracked.rs");
        std::fs::write(&file, "fn tracked() {}\n").unwrap();

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["init", "-b", "main"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "user.email", "cleanup-test@example.com"])
            .status()
            .expect("git config user.email should run");
        assert!(status.success(), "git config user.email should succeed");

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "user.name", "Cleanup Test"])
            .status()
            .expect("git config user.name should run");
        assert!(status.success(), "git config user.name should succeed");

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "tracked.rs"])
            .status()
            .expect("git add should run");
        assert!(status.success(), "git add should succeed");

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "-m", "seed commit"])
            .status()
            .expect("git commit should run");
        assert!(status.success(), "git commit should succeed");

        let now = now_micros();
        assert!(
            check_git_activity(repo, "tracked.rs", now, 120_000_000),
            "recently committed file should be treated as recently active"
        );
        assert!(
            !check_git_activity(repo, "tracked.rs", now + 10_000_000_000, 1_000_000),
            "old commit should fall outside a short grace window"
        );
    }

    #[test]
    fn check_git_activity_glob_respects_directory_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("src/nested")).unwrap();
        std::fs::write(repo.join("src/nested/lib.rs"), "fn nested() {}\n").unwrap();

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["init", "-b", "main"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "user.email", "cleanup-test@example.com"])
            .status()
            .expect("git config user.email should run");
        assert!(status.success(), "git config user.email should succeed");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "user.name", "Cleanup Test"])
            .status()
            .expect("git config user.name should run");
        assert!(status.success(), "git config user.name should succeed");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "."])
            .status()
            .expect("git add should run");
        assert!(status.success(), "git add should succeed");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "-m", "nested"])
            .status()
            .expect("git commit should run");
        assert!(status.success(), "git commit should succeed");

        let now = now_micros();
        assert!(
            !check_git_activity(repo, "src/*.rs", now, 120_000_000),
            "nested file should not match shallow glob"
        );
    }

    #[test]
    fn path_probe_cache_normalizes_leading_slash_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_micros();
        let mut probe_cache = CleanupProbeCache::default();

        let without_leading_slash = path_has_recent_activity_cached(
            &mut probe_cache,
            tmp.path(),
            7,
            "src/lib.rs",
            None,
            now,
            1_000_000,
        );
        let with_leading_slash = path_has_recent_activity_cached(
            &mut probe_cache,
            tmp.path(),
            7,
            "/src/lib.rs",
            None,
            now,
            1_000_000,
        );

        assert!(!without_leading_slash);
        assert!(!with_leading_slash);
        assert_eq!(probe_cache.path_probes.len(), 1);
    }

    #[test]
    fn detect_and_release_stale_skips_recent_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _agent_id, reservation_id, _human_key, _pattern) =
            seed_active_reservation(&tmp);

        let mut config = Config::from_env();
        config.file_reservation_inactivity_seconds = 86_400; // one day
        config.file_reservation_activity_grace_seconds = 900;

        let mut probe_cache = CleanupProbeCache::default();
        let released = detect_and_release_stale(&config, &pool, &cx, project_id, &mut probe_cache)
            .expect("stale pass");
        assert!(released.is_empty());

        let rows = match fastmcp_core::block_on(async {
            queries::list_file_reservations(&cx, &pool, project_id, false).await
        }) {
            Outcome::Ok(r) => r,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        let row = rows
            .iter()
            .find(|r| r.id.is_some_and(|rid| rid == reservation_id))
            .expect("reservation should exist");
        assert!(
            row.released_ts.is_none(),
            "recently active agent reservation should not be released"
        );
    }

    #[test]
    fn detect_and_release_stale_releases_inactive_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _agent_id, reservation_id, _human_key, _pattern) =
            seed_active_reservation(&tmp);

        let mut config = Config::from_env();
        config.file_reservation_inactivity_seconds = 0;
        config.file_reservation_activity_grace_seconds = 0;

        let mut probe_cache = CleanupProbeCache::default();
        let released = detect_and_release_stale(&config, &pool, &cx, project_id, &mut probe_cache)
            .expect("stale pass");
        assert_eq!(released.len(), 1);
        assert_eq!(released[0], reservation_id);

        let rows = match fastmcp_core::block_on(async {
            queries::list_file_reservations(&cx, &pool, project_id, false).await
        }) {
            Outcome::Ok(r) => r,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        let row = rows
            .iter()
            .find(|r| r.id.is_some_and(|rid| rid == reservation_id))
            .expect("reservation should exist");
        assert!(
            row.released_ts.is_some(),
            "inactive agent reservation should be released"
        );
    }

    #[test]
    fn detect_and_release_stale_errors_when_workspace_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _agent_id, reservation_id, human_key, _pattern) =
            seed_active_reservation(&tmp);

        std::fs::remove_dir_all(&human_key).expect("remove workspace");

        let mut config = Config::from_env();
        config.file_reservation_inactivity_seconds = 0;
        config.file_reservation_activity_grace_seconds = 0;

        let mut probe_cache = CleanupProbeCache::default();
        let err = detect_and_release_stale(&config, &pool, &cx, project_id, &mut probe_cache)
            .expect_err("missing workspace must not trigger stale release");
        assert!(err.contains("workspace does not exist"));

        let rows = match fastmcp_core::block_on(async {
            queries::list_file_reservations(&cx, &pool, project_id, false).await
        }) {
            Outcome::Ok(r) => r,
            other => panic!("list_file_reservations failed: {other:?}"),
        };
        let row = rows
            .iter()
            .find(|r| r.id.is_some_and(|rid| rid == reservation_id))
            .expect("reservation should exist");
        assert!(
            row.released_ts.is_none(),
            "workspace lookup failure must not release reservations"
        );
    }

    #[test]
    fn write_cleanup_artifacts_errors_when_archive_enqueue_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _agent_id, reservation_id, _human_key, _pattern) =
            seed_active_reservation(&tmp);

        match fastmcp_core::block_on(async {
            queries::release_reservations_by_ids(&cx, &pool, &[reservation_id]).await
        }) {
            Outcome::Ok(1) => {}
            other => panic!("release_reservations_by_ids failed: {other:?}"),
        }

        let config = Config::from_env();
        let metrics = mcp_agent_mail_core::global_metrics();
        let previous_pressure = metrics.system.disk_pressure_level.load();
        metrics
            .system
            .disk_pressure_level
            .set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
        let result = write_cleanup_artifacts(&config, &pool, &cx, project_id, &[reservation_id]);
        metrics.system.disk_pressure_level.set(previous_pressure);

        assert_eq!(
            result,
            Err("cleanup archive enqueue skipped due to critical disk pressure".into())
        );
    }

    #[test]
    fn write_cleanup_artifacts_errors_when_agent_lookup_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, agent_id, reservation_id, _human_key, _pattern) =
            seed_active_reservation(&tmp);

        match fastmcp_core::block_on(async {
            queries::release_reservations_by_ids(&cx, &pool, &[reservation_id]).await
        }) {
            Outcome::Ok(1) => {}
            other => panic!("release_reservations_by_ids failed: {other:?}"),
        }

        let conn = match fastmcp_core::block_on(async { pool.acquire(&cx).await }) {
            Outcome::Ok(conn) => conn,
            Outcome::Err(err) => panic!("pool acquire failed: {err}"),
            Outcome::Cancelled(_) => panic!("pool acquire cancelled"),
            Outcome::Panicked(panic) => panic!("pool acquire panicked: {}", panic.message()),
        };
        conn.execute_sync(
            "DELETE FROM agents WHERE id = ?",
            &[mcp_agent_mail_db::sqlmodel::Value::BigInt(agent_id)],
        )
        .expect("delete agent");
        drop(conn);

        let config = Config::from_env();
        let result = write_cleanup_artifacts(&config, &pool, &cx, project_id, &[reservation_id]);
        assert!(
            result
                .expect_err("missing agent lookup should fail cleanup artifact generation")
                .contains("agent lookup failed")
        );
    }

    #[test]
    fn write_cleanup_artifacts_errors_when_release_timestamp_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _agent_id, reservation_id, _human_key, _pattern) =
            seed_active_reservation(&tmp);

        let config = Config::from_env();
        let result = write_cleanup_artifacts(&config, &pool, &cx, project_id, &[reservation_id]);
        assert!(
            result
                .expect_err("active reservation should not fabricate a release timestamp")
                .contains("requires released_ts")
        );
    }
}
