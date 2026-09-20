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
use mcp_agent_mail_core::{Config, pattern_overlap::CompiledPattern};
use mcp_agent_mail_db::{
    DbPool, DbPoolConfig, FileReservationRow, create_pool, now_micros,
    queries::{
        self, get_agent_last_mail_activity, list_unreleased_file_reservations,
        project_ids_with_active_reservations, prune_released_file_reservations,
        release_expired_reservations, release_reservations_by_ids_returning_ids,
    },
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Global shutdown flag for the cleanup worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const PROBE_CACHE_RETENTION_US: i64 = 6 * 60 * 60 * 1_000_000;

fn normalize_path_pattern_key(path_pattern: &str) -> String {
    CompiledPattern::cached(path_pattern).normalized().to_string()
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
            .stack_size(mcp_agent_mail_core::worker_stack_size())
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
    // #219: reservation release/prune UPDATEs and DELETEs hit the live
    // mailbox; hold the in-process write lease for the whole cycle so a
    // recovery promotion cannot swap the database mid-write.
    let _write_activity = mcp_agent_mail_db::write_barrier::begin_write_activity();
    // This worker runs on a dedicated OS thread outside the async runtime, so
    // there is no parent Cx to derive from. Borrow the runtime-backed ambient
    // Cx<cap::All> that `Runtime::block_on` installs (INFINITE budget, correct
    // for a long-running worker that must never be cancelled by budget
    // exhaustion) instead of the test-only `Cx::for_testing()` constructor,
    // which is gated behind `test-internals`. The Cx is Arc-backed, so the
    // clone returned here stays valid across the later block_on calls below.
    let cx = block_on(async {
        Cx::current().expect("Runtime::block_on installs an ambient Cx for the polled future")
    });

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

    // Phase 3 (GH#154 item 2): retention prune. Released/expired reservations
    // are only ever marked released, never deleted, so `file_reservations` grows
    // unbounded on a long-lived mailbox (one report: ~30k rows / 0 active over
    // ~55 days), inflating every active-reservation scan. Hard-DELETE released
    // rows whose settled timestamp is older than the configured retention
    // horizon. The git archive retains the audit history, so this is
    // non-destructive. `0` days disables the prune (historical behavior).
    if config.file_reservations_retention_days > 0 {
        let retention_us = i64::try_from(config.file_reservations_retention_days)
            .unwrap_or(30)
            .saturating_mul(86_400)
            .saturating_mul(1_000_000);
        let older_than_us = now_micros().saturating_sub(retention_us);
        match block_on(async {
            prune_released_file_reservations(&cx, pool, None, older_than_us).await
        }) {
            Outcome::Ok(deleted) => {
                if deleted > 0 {
                    info!(
                        deleted,
                        retention_days = config.file_reservations_retention_days,
                        "cleanup: pruned released file reservations past retention horizon"
                    );
                }
            }
            Outcome::Err(err) => warn!(
                error = %err,
                "cleanup: file-reservation retention prune failed; will retry next cycle"
            ),
            Outcome::Cancelled(_) => {
                warn!("cleanup: file-reservation retention prune was cancelled");
            }
            Outcome::Panicked(panic) => warn!(
                panic = %panic.message(),
                "cleanup: file-reservation retention prune panicked"
            ),
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
                    // Cleanup may revoke a live claim. A process-local cached
                    // profile cannot establish inactivity after another writer
                    // refreshed the heartbeat or enabled reaper exemption.
                    queries::get_agent_by_id_fresh(cx, pool, res.agent_id).await
                }) {
                    Outcome::Ok(agent) => {
                        agent.id == Some(res.agent_id)
                            && agent.project_id == project_id
                            && agent.reaper_exempt == 0
                            && now.saturating_sub(agent.last_active_ts) > inactivity_us
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
                let computed = match block_on(async {
                    get_agent_last_mail_activity(cx, pool, res.agent_id, project_id).await
                }) {
                    Outcome::Ok(last_mail) => {
                        last_mail.is_some_and(|ts| now.saturating_sub(ts) <= grace_us)
                    }
                    other => {
                        // Unavailable evidence is not a negative activity
                        // signal. Keep this agent's claims for this cycle and
                        // retry the read on the next cycle, including after
                        // cancellation or a database worker panic.
                        warn!(
                            project_id,
                            agent_id = res.agent_id,
                            outcome = ?other,
                            "cleanup: mail activity unavailable; preserving reservations"
                        );
                        true
                    }
                };
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
            if !workspace.is_absolute() || project.id != Some(project_id) {
                return Err(format!(
                    "project {project_id} has an invalid workspace identity; refusing stale cleanup"
                ));
            }
            match workspace.metadata() {
                Ok(metadata) if metadata.is_dir() => Ok(workspace),
                Ok(_) => Err(format!(
                    "project {project_id} workspace is not a directory: {}",
                    workspace.display()
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
                    "project {project_id} workspace does not exist: {}",
                    workspace.display()
                )),
                Err(error) => Err(format!(
                    "project {project_id} workspace cannot be inspected at {}: {error}",
                    workspace.display()
                )),
            }
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
    let key = (project_id, normalized_pattern.clone());
    let entry = cache.path_probes.entry(key).or_default();
    entry.last_used_us = now_us;

    // Only a successful positive observation earns a grace-window cache entry.
    // Incomplete/error observations preserve the lease for this cycle but are
    // retried next time; they are neither inactivity nor fabricated activity.
    if entry.fs_recent_until_us > now_us {
        return true;
    }
    match probe_filesystem_activity(workspace, &normalized_pattern, now_us, grace_us) {
        ActivityProbeResult::Active => {
            entry.fs_recent_until_us = now_us.saturating_add(grace_us);
            return true;
        }
        ActivityProbeResult::Inactive => entry.fs_recent_until_us = 0,
        ActivityProbeResult::Truncated
        | ActivityProbeResult::Unsupported
        | ActivityProbeResult::Unavailable => {
            entry.fs_recent_until_us = 0;
            return true;
        }
    }

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
// Matched-file limits alone do not bound a tree of empty directories or files
// with unrelated extensions. Count every visited entry independently.
const ACTIVITY_PROBE_ENTRY_LIMIT: usize = 40_000;
const ACTIVITY_PROBE_PATH_DEPTH_LIMIT: usize = 256;
const ACTIVITY_PROBE_WALK_TIMEOUT: Duration = Duration::from_millis(250);
const ACTIVITY_PROBE_GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityProbeResult {
    Active,
    Inactive,
    Truncated,
    Unsupported,
    /// Inspection failed. This must never authorize an inactivity release.
    Unavailable,
}

struct ActivityWalkBudget {
    remaining_entries: usize,
    started: Instant,
}

impl ActivityWalkBudget {
    fn new() -> Self {
        Self {
            remaining_entries: ACTIVITY_PROBE_ENTRY_LIMIT,
            started: Instant::now(),
        }
    }

    fn take_entry(&mut self) -> bool {
        if self.remaining_entries == 0 || self.started.elapsed() >= ACTIVITY_PROBE_WALK_TIMEOUT {
            return false;
        }
        self.remaining_entries -= 1;
        true
    }
}

/// Inspect existing prefixes below the selected workspace, not just the leaf.
/// A leaf-only no-follow stat still follows intermediate directory symlinks.
/// These conservative observations are not an atomic filesystem snapshot.
fn probe_metadata(
    workspace: &Path,
    path: &Path,
) -> Result<Option<std::fs::Metadata>, ActivityProbeResult> {
    let relative = path
        .strip_prefix(workspace)
        .map_err(|_| ActivityProbeResult::Unavailable)?;
    let mut candidate = workspace.to_path_buf();
    let mut metadata = std::fs::symlink_metadata(&candidate)
        .map_err(|_| ActivityProbeResult::Unavailable)?;
    if !metadata.is_dir() {
        return Err(ActivityProbeResult::Unavailable);
    }
    for (depth, component) in relative.components().enumerate() {
        if depth >= ACTIVITY_PROBE_PATH_DEPTH_LIMIT
            || !matches!(component, std::path::Component::Normal(_))
            || !metadata.is_dir()
        {
            return Err(ActivityProbeResult::Unavailable);
        }
        candidate.push(component.as_os_str());
        metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ActivityProbeResult::Unavailable),
        };
        if metadata.file_type().is_symlink() {
            return Err(ActivityProbeResult::Unavailable);
        }
    }
    Ok(Some(metadata))
}

fn probe_file_activity(
    workspace: &Path,
    path: &Path,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    let Ok(Some(metadata)) = probe_metadata(workspace, path) else {
        // Includes a file disappearing after enumeration: a deletion/race is
        // not a successful observation of an old unchanged file.
        return ActivityProbeResult::Unavailable;
    };
    if !metadata.is_file() {
        return ActivityProbeResult::Unavailable;
    }
    let Some(modified_us) = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_micros()).ok())
    else {
        return ActivityProbeResult::Unavailable;
    };
    if now_us.saturating_sub(modified_us) <= grace_us {
        ActivityProbeResult::Active
    } else {
        ActivityProbeResult::Inactive
    }
}

fn git_listed_path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(bytes).ok()?);
    // Git's NUL-delimited output is relative to the selected worktree. Never
    // let a malformed record turn a read probe into traversal outside it.
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(path)
}

fn path_matches_probe(
    workspace: &Path,
    path: &Path,
    compiled: Option<&CompiledPattern>,
) -> Result<bool, ActivityProbeResult> {
    let Some(compiled) = compiled else {
        return Ok(true);
    };
    let relative = path
        .strip_prefix(workspace)
        .ok()
        .and_then(Path::to_str)
        .ok_or(ActivityProbeResult::Unavailable)?;
    if cfg!(windows) {
        Ok(compiled.matches(&relative.replace('\\', "/")))
    } else {
        Ok(compiled.matches(relative))
    }
}

fn parse_git_listed_activity(
    workspace: &Path,
    pattern: &str,
    bytes: &[u8],
    now_us: i64,
    grace_us: i64,
    path_limit: usize,
) -> ActivityProbeResult {
    if bytes.is_empty() {
        return ActivityProbeResult::Inactive;
    }
    let Some(records) = bytes.strip_suffix(&[0]) else {
        return ActivityProbeResult::Unavailable;
    };
    let compiled = CompiledPattern::cached(pattern);
    if !compiled.is_matchable() {
        return ActivityProbeResult::Unsupported;
    }
    for (count, record) in records.split(|byte| *byte == 0).enumerate() {
        if count >= path_limit {
            return ActivityProbeResult::Truncated;
        }
        let Some(relative) = git_listed_path(record) else {
            return ActivityProbeResult::Unavailable;
        };
        let path = workspace.join(relative);
        match path_matches_probe(
            workspace,
            &path,
            compiled.is_glob().then_some(compiled.as_ref()),
        ) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => return error,
        }
        match probe_file_activity(workspace, &path, now_us, grace_us) {
            ActivityProbeResult::Inactive => {}
            other => return other,
        }
    }
    ActivityProbeResult::Inactive
}

fn check_git_listed_activity(
    workspace: &Path,
    pattern: &str,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    use mcp_agent_mail_core::git_cmd::{GitCmd, GitRunOutcome};

    let compiled = CompiledPattern::cached(pattern);
    if !compiled.is_matchable() {
        return ActivityProbeResult::Unsupported;
    }
    // Git wildmatch and the reservation matcher differ (notably braces).
    // Enumerate a conservative literal prefix and apply our matcher in Rust.
    let prefix = if compiled.is_glob() {
        compiled.first_literal_segment().unwrap_or(".")
    } else if compiled.normalized().is_empty() {
        "."
    } else {
        compiled.normalized()
    };
    let pathspec = format!(":(literal){prefix}");
    // This is a read-only worktree probe, not archive mutation. Skip both
    // coordination locks so their waits cannot outlive the probe deadline or
    // create a flock sentinel in the user's repository. The shared Unix runner
    // drains/reaps the child and inherited pipes under one bounded deadline
    // and rejects output beyond its configured capture limit. No detached
    // reader is created here. Other platforms retain that runner's guarantees.
    let outcome = GitCmd::new(workspace)
        .args(["ls-files", "-z", "-c", "-o", "--exclude-standard", "--"])
        .arg(pathspec)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .skip_flock()
        .skip_mutex()
        .timeout(ACTIVITY_PROBE_GIT_TIMEOUT)
        .run_once();
    match outcome {
        GitRunOutcome::Finished(output) if output.status.success() => parse_git_listed_activity(
            workspace,
            pattern,
            &output.stdout,
            now_us,
            grace_us,
            ACTIVITY_PROBE_PATH_LIMIT,
        ),
        GitRunOutcome::Finished(_) => ActivityProbeResult::Unsupported,
        _ => ActivityProbeResult::Unavailable,
    }
}

fn walk_activity(
    workspace: &Path,
    root: &Path,
    compiled: Option<&CompiledPattern>,
    now_us: i64,
    grace_us: i64,
    path_limit: usize,
    budget: &mut ActivityWalkBudget,
) -> ActivityProbeResult {
    let mut scanned = 0usize;
    // The elapsed-time bound is cooperative between filesystem operations; it
    // cannot preempt an individual metadata/read-directory call.
    for entry in walkdir::WalkDir::new(root).follow_links(false).max_open(16) {
        if !budget.take_entry() {
            return ActivityProbeResult::Truncated;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return ActivityProbeResult::Unavailable,
        };
        let kind = entry.file_type();
        if kind.is_symlink() {
            return ActivityProbeResult::Unavailable;
        }
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            return ActivityProbeResult::Unavailable;
        }
        match path_matches_probe(workspace, entry.path(), compiled) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => return error,
        }
        if scanned >= path_limit {
            return ActivityProbeResult::Truncated;
        }
        scanned += 1;
        match probe_file_activity(workspace, entry.path(), now_us, grace_us) {
            ActivityProbeResult::Inactive => {}
            other => return other,
        }
    }
    ActivityProbeResult::Inactive
}

fn check_directory_activity_fallback(
    dir: &Path,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    check_directory_activity_fallback_with_limit(dir, now_us, grace_us, ACTIVITY_PROBE_PATH_LIMIT)
}

fn check_directory_activity_fallback_with_limit(
    dir: &Path,
    now_us: i64,
    grace_us: i64,
    path_limit: usize,
) -> ActivityProbeResult {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        _ => return ActivityProbeResult::Unavailable,
    }
    walk_activity(
        dir,
        dir,
        None,
        now_us,
        grace_us,
        path_limit,
        &mut ActivityWalkBudget::new(),
    )
}

fn check_glob_activity_fallback(
    workspace: &Path,
    pattern: &str,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    check_glob_activity_fallback_with_limit(
        workspace,
        pattern,
        now_us,
        grace_us,
        ACTIVITY_PROBE_PATH_LIMIT,
    )
}

fn check_glob_activity_fallback_with_limit(
    workspace: &Path,
    pattern: &str,
    now_us: i64,
    grace_us: i64,
    path_limit: usize,
) -> ActivityProbeResult {
    let compiled = CompiledPattern::cached(pattern);
    if !compiled.is_matchable() {
        return ActivityProbeResult::Unsupported;
    }
    let scan_root = compiled
        .first_literal_segment()
        .map(|segment| workspace.join(segment))
        .unwrap_or_else(|| workspace.to_path_buf());
    match probe_metadata(workspace, &scan_root) {
        Ok(Some(metadata)) if metadata.is_dir() || metadata.is_file() => {}
        Ok(None) => return ActivityProbeResult::Inactive,
        _ => return ActivityProbeResult::Unavailable,
    }
    walk_activity(
        workspace,
        &scan_root,
        Some(&compiled),
        now_us,
        grace_us,
        path_limit,
        &mut ActivityWalkBudget::new(),
    )
}

/// Only a completed negative filesystem observation can support stale release.
/// Git is a fast positive hint: its ignored-file exclusions cannot establish
/// that all files covered by a reservation have been inactive.
fn probe_filesystem_activity(
    workspace: &Path,
    path_pattern: &str,
    now_us: i64,
    grace_us: i64,
) -> ActivityProbeResult {
    match workspace.metadata() {
        Ok(metadata) if metadata.is_dir() => {}
        _ => return ActivityProbeResult::Unavailable,
    }
    let pattern = normalize_path_pattern_key(path_pattern);
    let compiled = CompiledPattern::cached(&pattern);
    if !compiled.is_matchable() {
        return ActivityProbeResult::Unsupported;
    }
    if compiled.is_glob() {
        match check_git_listed_activity(workspace, &pattern, now_us, grace_us) {
            ActivityProbeResult::Inactive | ActivityProbeResult::Unsupported => {}
            other => return other,
        }
        return check_glob_activity_fallback(workspace, &pattern, now_us, grace_us);
    }
    let candidate = workspace.join(&pattern);
    match probe_metadata(workspace, &candidate) {
        Ok(Some(metadata)) if metadata.is_dir() => {
            match check_git_listed_activity(workspace, &pattern, now_us, grace_us) {
                ActivityProbeResult::Inactive | ActivityProbeResult::Unsupported => {}
                other => return other,
            }
            check_directory_activity_fallback(&candidate, now_us, grace_us)
        }
        Ok(Some(metadata)) if metadata.is_file() => {
            probe_file_activity(workspace, &candidate, now_us, grace_us)
        }
        Ok(None) => ActivityProbeResult::Inactive,
        _ => ActivityProbeResult::Unavailable,
    }
}

#[cfg(test)]
fn check_filesystem_activity(
    workspace: &Path,
    path_pattern: &str,
    now_us: i64,
    grace_us: i64,
) -> bool {
    probe_filesystem_activity(workspace, path_pattern, now_us, grace_us)
        != ActivityProbeResult::Inactive
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
    // br-8ujfs.4.1 (D1): route through GitCmd.
    let output = mcp_agent_mail_core::git_cmd::GitCmd::new(workspace)
        .args(["rev-parse", "HEAD"])
        .run()
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

    // br-8ujfs.4.1 (D1): route through GitCmd.
    let output = mcp_agent_mail_core::git_cmd::GitCmd::new(workspace)
        .args(["log", "-1", "--format=%ct", "--"])
        .arg(&pathspec)
        .run()
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
    let found_ids = target_reservations
        .iter()
        .filter_map(|row| row.id)
        .collect::<HashSet<_>>();
    if let Some(missing_id) = released_ids.iter().find(|id| !found_ids.contains(id)) {
        return Err(format!(
            "cleanup artifact reservation lookup failed for released reservation {missing_id}"
        ));
    }

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
        assert_eq!(collect_matching_paths(&tmp, ""), [] as [PathBuf; 0]);
        assert_eq!(collect_matching_paths(&tmp, "  "), [] as [PathBuf; 0]);
    }

    #[test]
    fn collect_matching_nonexistent_base() {
        let fake = Path::new("/nonexistent/path/foo");
        assert_eq!(collect_matching_paths(fake, "*.rs"), [] as [PathBuf; 0]);
    }

    #[test]
    fn collect_matching_invalid_glob_pattern_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            collect_matching_paths(tmp.path(), "[unterminated"),
            [] as [PathBuf; 0]
        );
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
        assert_eq!(collect_matching_paths(&tmp, "   \t  "), [] as [PathBuf; 0]);
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
    fn filesystem_activity_nonexistent_workspace_is_unavailable() {
        let fake = Path::new("/definitely/does/not/exist");
        assert_eq!(
            probe_filesystem_activity(fake, "*.rs", now_micros(), 1_000_000),
            ActivityProbeResult::Unavailable
        );
        assert!(check_filesystem_activity(
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

    fn allow_ephemeral_project_roots_for_test<R>(f: impl FnOnce() -> R) -> R {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ALLOW_EPHEMERAL_PROJECT_ROOTS", "1")],
            f,
        )
    }

    fn seed_active_reservation(
        tmp: &tempfile::TempDir,
    ) -> (DbPool, Cx, i64, i64, i64, String, String) {
        let pool = make_test_pool(tmp);
        let cx = Cx::for_testing();

        let project_root = tmp.path().join("project_root_active");
        std::fs::create_dir_all(&project_root).unwrap();
        let human_key = project_root.to_string_lossy().to_string();

        let project = allow_ephemeral_project_roots_for_test(|| {
            match fastmcp_core::block_on(async {
                queries::ensure_project(&cx, &pool, &human_key).await
            }) {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            }
        });
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

        let project = allow_ephemeral_project_roots_for_test(|| {
            match fastmcp_core::block_on(async {
                queries::ensure_project(&cx, &pool, &human_key).await
            }) {
                Outcome::Ok(p) => p,
                other => panic!("ensure_project failed: {other:?}"),
            }
        });
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
    fn directory_probe_limit_counts_only_files() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let nested_dir = workspace.join("src").join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("lib.rs"), "pub fn nested() {}\n").unwrap();

        let now = now_micros();
        assert_eq!(
            check_directory_activity_fallback_with_limit(&workspace.join("src"), now, 1_000_000, 1),
            ActivityProbeResult::Active,
            "directory entries should not consume the file activity probe budget"
        );
    }

    #[test]
    fn glob_probe_limit_counts_only_files() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let nested_dir = workspace.join("src").join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("lib.rs"), "pub fn nested() {}\n").unwrap();

        let now = now_micros();
        assert_eq!(
            check_glob_activity_fallback_with_limit(workspace, "src/**", now, 1_000_000, 1),
            ActivityProbeResult::Active,
            "glob directory matches should not consume the file activity probe budget"
        );
    }

    #[test]
    fn glob_probe_deduplicates_overlapping_directory_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        let nested_dir = workspace.join("src").join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("lib.rs"), "pub fn nested() {}\n").unwrap();

        let now = now_micros();
        assert_eq!(
            check_glob_activity_fallback_with_limit(workspace, "src/**", now, -1, 1),
            ActivityProbeResult::Inactive,
            "overlapping directory matches should not count the same file more than once"
        );
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
        assert_eq!(released, [] as [i64; 0]);

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
    fn write_cleanup_artifacts_errors_when_released_reservation_lookup_fails() {
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
                .expect_err("missing reservation lookup should fail cleanup artifact generation")
                .contains("reservation lookup failed")
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

    fn stale_cleanup_test_config(tmp: &tempfile::TempDir) -> Config {
        Config {
            storage_root: tmp.path().join("storage"),
            file_reservation_inactivity_seconds: 0,
            file_reservation_activity_grace_seconds: 0,
            ..Config::default()
        }
    }

    fn assert_reservation_unreleased(pool: &DbPool, cx: &Cx, project_id: i64, id: i64) {
        let rows = match block_on(queries::list_file_reservations(cx, pool, project_id, false)) {
            Outcome::Ok(rows) => rows,
            other => panic!("read reservation state: {other:?}"),
        };
        let row = rows
            .iter()
            .find(|row| row.id == Some(id))
            .expect("claim retained");
        assert!(
            row.released_ts.is_none(),
            "uncertain activity must preserve claim {id}"
        );
    }

    #[test]
    fn stale_cleanup_retries_mail_evidence_instead_of_releasing_on_read_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, agent_id, reservation_id, _, _) = seed_active_reservation(&tmp);
        let config = stale_cleanup_test_config(&tmp);
        let mut cache = CleanupProbeCache::default();
        let conn = match block_on(pool.acquire(&cx)) {
            Outcome::Ok(conn) => conn,
            // `Outcome`'s derived `Debug` needs `T: Debug`, and `PooledConnection`
            // is not `Debug`, so report each failure variant on its own.
            Outcome::Err(error) => panic!("acquire fixture connection: {error:?}"),
            Outcome::Cancelled(reason) => {
                panic!("acquire fixture connection cancelled: {reason:?}")
            }
            Outcome::Panicked(payload) => {
                panic!("acquire fixture connection panicked: {payload:?}")
            }
        };
        // Preserve the table and its data while making the actual mail query
        // unavailable. No mocked outcome and no mutation of an operator DB.
        if let Err(error) =
            conn.execute_sync("ALTER TABLE messages RENAME TO cleanup_saved_messages", &[])
        {
            // Preserve the original failure while exposing the engine's
            // validation reason on this same connection, before it is dropped.
            let commit_events = conn.query_sync("PRAGMA fsqlite.commit_events", &[]);
            panic!(
                "hide mail evidence in fixture: {error:?}; engine commit events: {commit_events:?}"
            );
        }
        drop(conn);
        assert!(matches!(
            block_on(get_agent_last_mail_activity(
                &cx, &pool, agent_id, project_id
            )),
            Outcome::Err(_)
        ));

        for _ in 0..2 {
            assert_eq!(
                detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache).unwrap(),
                // Explicit element type: `Vec`'s `PartialEq` is generic over the other
                // side's element, so a bare `Vec::new()` leaves `T` uninferable (E0282).
                Vec::<i64>::new()
            );
            assert_reservation_unreleased(&pool, &cx, project_id, reservation_id);
        }

        let conn = match block_on(pool.acquire(&cx)) {
            Outcome::Ok(conn) => conn,
            Outcome::Err(error) => panic!("reacquire fixture connection: {error:?}"),
            Outcome::Cancelled(reason) => {
                panic!("reacquire fixture connection cancelled: {reason:?}")
            }
            Outcome::Panicked(payload) => {
                panic!("reacquire fixture connection panicked: {payload:?}")
            }
        };
        conn.execute_sync("ALTER TABLE cleanup_saved_messages RENAME TO messages", &[])
            .expect("restore mail evidence");
        drop(conn);
        assert_eq!(
            detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache).unwrap(),
            vec![reservation_id],
            "a later successful inactivity check must still release a stale claim"
        );
    }

    #[test]
    fn stale_cleanup_observes_heartbeat_updates_outside_the_read_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, agent_id, reservation_id, _, _) = seed_active_reservation(&tmp);
        assert!(matches!(
            block_on(queries::get_agent_by_id(&cx, &pool, agent_id)),
            Outcome::Ok(_)
        ));
        let conn = match block_on(pool.acquire(&cx)) {
            Outcome::Ok(conn) => conn,
            // `Outcome`'s derived `Debug` needs `T: Debug`, and `PooledConnection`
            // is not `Debug`, so report each failure variant on its own.
            Outcome::Err(error) => panic!("acquire fixture connection: {error:?}"),
            Outcome::Cancelled(reason) => {
                panic!("acquire fixture connection cancelled: {reason:?}")
            }
            Outcome::Panicked(payload) => {
                panic!("acquire fixture connection panicked: {payload:?}")
            }
        };
        conn.execute_sync(
            "UPDATE agents SET last_active_ts = ? WHERE id = ?",
            &[
                mcp_agent_mail_db::sqlmodel::Value::BigInt(now_micros() + 86_400_000_000),
                mcp_agent_mail_db::sqlmodel::Value::BigInt(agent_id),
            ],
        )
        .expect("refresh heartbeat without process-local invalidation");
        drop(conn);

        let mut cache = CleanupProbeCache::default();
        let config = stale_cleanup_test_config(&tmp);
        assert_eq!(
            detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache).unwrap(),
            // Explicit element type: `Vec`'s `PartialEq` is generic over the other
            // side's element, so a bare `Vec::new()` leaves `T` uninferable (E0282).
            Vec::<i64>::new()
        );
        assert_reservation_unreleased(&pool, &cx, project_id, reservation_id);
    }

    #[test]
    fn stale_cleanup_observes_new_reaper_exemption_outside_the_read_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, agent_id, reservation_id, _, _) = seed_active_reservation(&tmp);
        assert!(matches!(
            block_on(queries::get_agent_by_id(&cx, &pool, agent_id)),
            Outcome::Ok(_)
        ));
        let conn = match block_on(pool.acquire(&cx)) {
            Outcome::Ok(conn) => conn,
            // `Outcome`'s derived `Debug` needs `T: Debug`, and `PooledConnection`
            // is not `Debug`, so report each failure variant on its own.
            Outcome::Err(error) => panic!("acquire fixture connection: {error:?}"),
            Outcome::Cancelled(reason) => {
                panic!("acquire fixture connection cancelled: {reason:?}")
            }
            Outcome::Panicked(payload) => {
                panic!("acquire fixture connection panicked: {payload:?}")
            }
        };
        conn.execute_sync(
            "UPDATE agents SET reaper_exempt = 1, last_active_ts = 0 WHERE id = ?",
            &[mcp_agent_mail_db::sqlmodel::Value::BigInt(agent_id)],
        )
        .expect("protect dormant agent without process-local invalidation");
        drop(conn);

        let mut cache = CleanupProbeCache::default();
        let config = stale_cleanup_test_config(&tmp);
        assert_eq!(
            detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache).unwrap(),
            // Explicit element type: `Vec`'s `PartialEq` is generic over the other
            // side's element, so a bare `Vec::new()` leaves `T` uninferable (E0282).
            Vec::<i64>::new()
        );
        assert_reservation_unreleased(&pool, &cx, project_id, reservation_id);
    }

    #[test]
    fn stale_cleanup_refuses_a_workspace_replaced_by_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _, reservation_id, human_key, _) = seed_active_reservation(&tmp);
        std::fs::rename(&human_key, tmp.path().join("saved-workspace")).unwrap();
        std::fs::write(&human_key, b"not a workspace directory").unwrap();
        let mut cache = CleanupProbeCache::default();
        let config = stale_cleanup_test_config(&tmp);

        let error = detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache)
            .expect_err("a file is not evidence about the reserved workspace paths");
        assert!(error.contains("not a directory"));
        assert_reservation_unreleased(&pool, &cx, project_id, reservation_id);
        assert_eq!(
            std::fs::read(human_key).unwrap(),
            b"not a workspace directory"
        );
    }

    fn init_activity_repo(path: &Path) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["init", "-b", "main"])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    #[test]
    fn git_activity_listing_preserves_quoted_unicode_and_newline_filenames() {
        for name in ["space name.rs", "tab\tname.rs", "line\nbreak.rs", "café.rs"] {
            let tmp = tempfile::tempdir().unwrap();
            init_activity_repo(tmp.path());
            std::fs::write(tmp.path().join(name), b"active work").unwrap();
            let now = now_micros();
            assert_eq!(
                check_git_listed_activity(tmp.path(), "*.rs", now, 60_000_000),
                ActivityProbeResult::Active,
                "{name:?}"
            );
        }
    }

    #[test]
    fn git_listing_uses_reservation_braces_not_git_wildmatch_braces() {
        let tmp = tempfile::tempdir().unwrap();
        init_activity_repo(tmp.path());
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), b"active work").unwrap();
        assert_eq!(
            check_git_listed_activity(tmp.path(), "src/*.{rs,toml}", now_micros(), 60_000_000),
            ActivityProbeResult::Active
        );
    }

    #[test]
    fn ignored_reserved_files_still_protect_their_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        init_activity_repo(tmp.path());
        std::fs::write(tmp.path().join(".gitignore"), b"generated/\n").unwrap();
        std::fs::create_dir(tmp.path().join("generated")).unwrap();
        std::fs::write(tmp.path().join("generated/live.rs"), b"active work").unwrap();
        let now = now_micros();
        for pattern in ["generated", "generated/**", "generated/*.rs"] {
            assert_eq!(
                check_git_listed_activity(tmp.path(), pattern, now, 60_000_000),
                ActivityProbeResult::Inactive
            );
            assert_eq!(
                probe_filesystem_activity(tmp.path(), pattern, now, 60_000_000),
                ActivityProbeResult::Active,
                "Git exclusion is not inactivity: {pattern}"
            );
        }
    }

    #[test]
    fn nul_listing_refuses_partial_or_outside_workspace_records() {
        let tmp = tempfile::tempdir().unwrap();
        for bytes in [
            b"partial.rs".as_slice(),
            b"../outside.rs\0",
            b"/outside.rs\0",
            b"\0",
        ] {
            assert_eq!(
                parse_git_listed_activity(tmp.path(), "**", bytes, now_micros(), 1_000_000, 10),
                ActivityProbeResult::Unavailable
            );
        }
    }

    #[test]
    fn nul_listing_distinguishes_complete_empty_and_truncated_scans() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), b"old work").unwrap();
        let now = now_micros() + 120_000_000;
        assert_eq!(
            parse_git_listed_activity(tmp.path(), "**", b"", now, 1_000_000, 0),
            ActivityProbeResult::Inactive
        );
        assert_eq!(
            parse_git_listed_activity(tmp.path(), "**", b"a.rs\0", now, 1_000_000, 1),
            ActivityProbeResult::Inactive
        );
        assert_eq!(
            parse_git_listed_activity(tmp.path(), "**", b"a.rs\0b.rs\0", now, 1_000_000, 1),
            ActivityProbeResult::Truncated
        );
        assert_eq!(
            parse_git_listed_activity(tmp.path(), "**", b"missing.rs\0", now, 1_000_000, 1),
            ActivityProbeResult::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_git_listing_keeps_non_utf8_paths_without_lossy_substitution() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let tmp = tempfile::tempdir().unwrap();
        init_activity_repo(tmp.path());
        let raw = b"invalid-\xff.rs";
        let name = std::ffi::OsString::from_vec(raw.to_vec());
        std::fs::write(tmp.path().join(&name), b"active work").unwrap();
        let parsed = git_listed_path(raw).unwrap();
        assert_eq!(parsed.as_os_str().as_bytes(), raw);
        // Root requests need no Unicode matcher and can inspect native bytes.
        assert_eq!(
            check_git_listed_activity(tmp.path(), "", now_micros(), 60_000_000),
            ActivityProbeResult::Active
        );
        // A Unicode glob cannot certify that an undecodable path is unrelated.
        assert_eq!(
            check_git_listed_activity(tmp.path(), "*.rs", now_micros(), 60_000_000),
            ActivityProbeResult::Unavailable
        );
    }

    #[test]
    fn walk_errors_and_nonregular_roots_are_not_inactivity() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert_eq!(
            check_directory_activity_fallback(&missing, now_micros(), 1_000_000),
            ActivityProbeResult::Unavailable
        );
        std::fs::write(tmp.path().join("not-directory"), b"preserved").unwrap();
        assert_eq!(
            probe_filesystem_activity(
                tmp.path(),
                "not-directory/child.rs",
                now_micros(),
                1_000_000,
            ),
            ActivityProbeResult::Unavailable
        );
        assert_eq!(
            probe_filesystem_activity(tmp.path(), "[unterminated", now_micros(), 1_000_000),
            ActivityProbeResult::Unsupported
        );
    }

    #[test]
    fn root_aliases_inspect_files_instead_of_reporting_no_activity() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("active.rs"), b"active work").unwrap();
        let now = now_micros();
        for pattern in ["", ".", "./", "/", "src/..", "a\\.."] {
            assert_eq!(normalize_path_pattern_key(pattern), "");
            assert_eq!(
                probe_filesystem_activity(tmp.path(), pattern, now, 60_000_000),
                ActivityProbeResult::Active,
                "{pattern:?}"
            );
        }
    }

    #[test]
    fn traversal_budget_counts_empty_directories_and_unmatched_files() {
        let tmp = tempfile::tempdir().unwrap();
        for index in 0..8 {
            std::fs::create_dir(tmp.path().join(format!("dir{index}"))).unwrap();
            std::fs::write(tmp.path().join(format!("unrelated{index}.txt")), b"x").unwrap();
        }
        let compiled = CompiledPattern::new("*.rs");
        let mut budget = ActivityWalkBudget {
            remaining_entries: 3,
            started: Instant::now(),
        };
        assert_eq!(
            walk_activity(
                tmp.path(),
                tmp.path(),
                Some(&compiled),
                now_micros(),
                1_000_000,
                5000,
                &mut budget,
            ),
            ActivityProbeResult::Truncated
        );
        assert_eq!(budget.remaining_entries, 0);
    }

    #[test]
    fn expired_walk_budget_cannot_authorize_an_empty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let mut budget = ActivityWalkBudget {
            remaining_entries: 10,
            started: Instant::now() - Duration::from_secs(1),
        };
        assert_eq!(
            walk_activity(
                tmp.path(),
                tmp.path(),
                None,
                now_micros(),
                1_000_000,
                10,
                &mut budget,
            ),
            ActivityProbeResult::Truncated
        );
        assert_eq!(budget.remaining_entries, 10);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_activity_is_uncertain_and_does_not_follow_outside_files() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("old.rs"), b"outside work").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();
        for pattern in ["linked", "linked/**"] {
            assert_eq!(
                probe_filesystem_activity(
                    tmp.path(),
                    pattern,
                    now_micros() + 120_000_000,
                    1_000_000,
                ),
                ActivityProbeResult::Unavailable
            );
        }
        assert_eq!(
            std::fs::read(outside.path().join("old.rs")).unwrap(),
            b"outside work"
        );
    }

    #[test]
    fn unavailable_probe_is_retried_without_positive_cache_entitlement() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("src"), b"not a directory").unwrap();
        let now = now_micros();
        let mut cache = CleanupProbeCache::default();
        assert!(path_has_recent_activity_cached(
            &mut cache,
            tmp.path(),
            1,
            "src/missing.rs",
            None,
            now,
            60_000_000,
        ));
        assert_eq!(
            cache.path_probes[&(1, "src/missing.rs".to_string())].fs_recent_until_us,
            0
        );
        std::fs::rename(tmp.path().join("src"), tmp.path().join("saved-src-file")).unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        assert!(!path_has_recent_activity_cached(
            &mut cache,
            tmp.path(),
            1,
            "src/missing.rs",
            None,
            now,
            60_000_000,
        ));
    }

    #[test]
    fn stale_cleanup_preserves_claim_on_filesystem_error_then_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _, reservation_id, human_key, _) = seed_active_reservation(&tmp);
        let workspace = PathBuf::from(human_key);
        // A real ENOTDIR replaces unavailable activity evidence, without
        // chmod assumptions that fail when a test runner has root privileges.
        std::fs::write(workspace.join("src"), b"preserve this evidence").unwrap();
        let config = stale_cleanup_test_config(&tmp);
        let mut cache = CleanupProbeCache::default();
        assert_eq!(
            detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache).unwrap(),
            Vec::<i64>::new()
        );
        assert_reservation_unreleased(&pool, &cx, project_id, reservation_id);
        std::fs::rename(workspace.join("src"), workspace.join("saved-src-file")).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        assert_eq!(
            detect_and_release_stale(&config, &pool, &cx, project_id, &mut cache).unwrap(),
            vec![reservation_id],
            "a complete negative observation still permits ordinary stale cleanup"
        );
        assert_eq!(
            std::fs::read(workspace.join("saved-src-file")).unwrap(),
            b"preserve this evidence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn literal_and_listed_leaf_checks_reject_symlinked_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("nested")).unwrap();
        std::fs::write(outside.path().join("nested/old.rs"), b"outside evidence").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("src")).unwrap();
        let now = now_micros() + 120_000_000;
        assert_eq!(
            probe_filesystem_activity(tmp.path(), "src/nested/old.rs", now, 1_000_000),
            ActivityProbeResult::Unavailable
        );
        assert_eq!(
            parse_git_listed_activity(
                tmp.path(),
                "src/**",
                b"src/nested/old.rs\0",
                now,
                1_000_000,
                10,
            ),
            ActivityProbeResult::Unavailable
        );
        assert_eq!(
            std::fs::read(outside.path().join("nested/old.rs")).unwrap(),
            b"outside evidence"
        );
    }

    #[test]
    fn missing_candidate_and_missing_workspace_are_distinct_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("absent");
        assert!(probe_metadata(tmp.path(), &absent).unwrap().is_none());
        assert!(matches!(
            probe_metadata(&absent, &absent.join("file.rs")),
            Err(ActivityProbeResult::Unavailable)
        ));
        assert!(matches!(
            probe_metadata(tmp.path(), Path::new("/outside-workspace/file.rs")),
            Err(ActivityProbeResult::Unavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn excessive_prefix_depth_is_unknown_not_a_negative_activity_result() {
        let tmp = tempfile::tempdir().unwrap();
        let mut path = tmp.path().to_path_buf();
        for _ in 0..ACTIVITY_PROBE_PATH_DEPTH_LIMIT {
            path.push("d");
            std::fs::create_dir(&path).unwrap();
        }
        let file = path.join("old.rs");
        std::fs::write(&file, b"retained deep work").unwrap();
        assert!(probe_metadata(tmp.path(), &path).unwrap().is_some());
        assert!(matches!(
            probe_metadata(tmp.path(), &file),
            Err(ActivityProbeResult::Unavailable)
        ));
        assert_eq!(std::fs::read(file).unwrap(), b"retained deep work");
    }
}
