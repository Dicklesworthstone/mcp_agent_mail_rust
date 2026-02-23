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
        release_reservations_by_ids,
    },
};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Global shutdown flag for the cleanup worker.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Worker handle for join-on-shutdown.
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

/// Start the file reservation cleanup worker (if enabled).
///
/// Must be called at most once. Subsequent calls are no-ops.
pub fn start(config: &Config) {
    if !config.file_reservations_cleanup_enabled {
        return;
    }

    let config = config.clone();
    let _ = WORKER.get_or_init(|| {
        SHUTDOWN.store(false, Ordering::Release);
        std::thread::Builder::new()
            .name("file-res-cleanup".into())
            .spawn(move || {
                let rt = asupersync::runtime::RuntimeBuilder::new()
                    .worker_threads(1)
                    .build()
                    .expect("build cleanup runtime");
                rt.block_on(async move { cleanup_loop(&config) });
            })
            .expect("failed to spawn file reservation cleanup worker")
    });
}

/// Signal the worker to stop and wait for it to finish.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    // We cannot take from OnceLock, so just signal; the thread will exit on
    // the next iteration check.
}

fn cleanup_loop(config: &Config) {
    let interval =
        std::time::Duration::from_secs(config.file_reservations_cleanup_interval_seconds.max(5));

    let pool_config = DbPoolConfig::from_env();
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

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            info!("file reservation cleanup worker shutting down");
            return;
        }

        // Run one cleanup cycle, suppressing all errors (legacy: never crash server).
        match run_cleanup_cycle(config, &pool) {
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

/// Run a single cleanup cycle across all projects.
///
/// Returns `(projects_scanned, total_released)`.
fn run_cleanup_cycle(config: &Config, pool: &DbPool) -> Result<(usize, usize), String> {
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
                _ => Vec::new(), // Suppress per-project errors (legacy: contextlib.suppress).
            };
        total_released += expired_ids.len();

        // Phase 2: detect and release stale.
        let stale_ids = detect_and_release_stale(config, pool, &cx, *pid).unwrap_or_default();
        total_released += stale_ids.len();

        // Write archive artifacts for released reservations.
        if !expired_ids.is_empty() {
            let _ = write_cleanup_artifacts(config, pool, &cx, *pid, &expired_ids);
        }
        if !stale_ids.is_empty() {
            let _ = write_cleanup_artifacts(config, pool, &cx, *pid, &stale_ids);
        }
    }

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
) -> Result<Vec<i64>, String> {
    let inactivity_us = i64::try_from(config.file_reservation_inactivity_seconds)
        .unwrap_or(1800)
        .saturating_mul(1_000_000);
    let grace_us = i64::try_from(config.file_reservation_activity_grace_seconds)
        .unwrap_or(900)
        .saturating_mul(1_000_000);
    let now = now_micros();

    // Get all unreleased reservations for this project.
    let reservations =
        match block_on(async { list_unreleased_file_reservations(cx, pool, project_id).await }) {
            Outcome::Ok(rows) => rows,
            other => return Err(format!("failed to list reservations: {other:?}")),
        };

    // Filter to only non-expired ones (expired were handled in phase 1).
    let active: Vec<&FileReservationRow> =
        reservations.iter().filter(|r| r.expires_ts > now).collect();

    if active.is_empty() {
        return Ok(Vec::new());
    }

    let mut stale_ids = Vec::new();

    for res in &active {
        // Get agent info to check last_active_ts.
        let Outcome::Ok(agent) =
            block_on(async { queries::get_agent_by_id(cx, pool, res.agent_id).await })
        else {
            continue; // Skip if agent lookup fails.
        };

        // Check agent inactivity.
        let agent_inactive = (now - agent.last_active_ts) > inactivity_us;
        if !agent_inactive {
            continue; // Agent is recently active, not stale.
        }

        // Check mail activity grace period.
        let last_mail = match block_on(async {
            get_agent_last_mail_activity(cx, pool, res.agent_id, project_id).await
        }) {
            Outcome::Ok(ts) => ts,
            _ => None,
        };
        let recent_mail = last_mail.is_some_and(|ts| (now - ts) <= grace_us);
        if recent_mail {
            continue; // Recent mail activity, not stale.
        }

        // Check filesystem activity for matched paths.
        let Outcome::Ok(project) =
            block_on(async { queries::get_project_by_id(cx, pool, project_id).await })
        else {
            // Can't determine filesystem activity; treat as stale based on agent+mail.
            if let Some(id) = res.id {
                stale_ids.push(id);
            }
            continue;
        };

        let workspace = Path::new(&project.human_key);
        let recent_fs = check_filesystem_activity(workspace, &res.path_pattern, now, grace_us);
        if recent_fs {
            continue;
        }

        let recent_git = check_git_activity(workspace, &res.path_pattern, now, grace_us);
        if recent_git {
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
    match block_on(async { release_reservations_by_ids(cx, pool, &stale_ids).await }) {
        Outcome::Ok(_) => Ok(stale_ids),
        other => Err(format!("failed to release stale reservations: {other:?}")),
    }
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

    let matches = collect_matching_paths(workspace, path_pattern);
    if matches.is_empty() {
        return false;
    }

    for path in &matches {
        if let Ok(metadata) = path.metadata()
            && let Ok(modified) = metadata.modified()
        {
            let mtime_us = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(0));
            if (now_us - mtime_us) <= grace_us {
                return true;
            }
        }
    }

    false
}

/// Check if any matched files have recent git commit activity.
fn check_git_activity(workspace: &Path, path_pattern: &str, now_us: i64, grace_us: i64) -> bool {
    if !workspace.exists() {
        return false;
    }

    // Use git log with the path pattern directly (git handles pathspecs including globs).
    // This is vastly more efficient than spawning a git process per matched file.
    let Ok(output) = std::process::Command::new("git")
        .args([
            "-C",
            &workspace.to_string_lossy(),
            "log",
            "-1",
            "--format=%ct",
            "--",
            path_pattern,
        ])
        .output()
    else {
        return false;
    };

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Ok(commit_epoch) = stdout.trim().parse::<i64>() {
            let commit_us = commit_epoch * 1_000_000;
            if (now_us - commit_us) <= grace_us {
                return true;
            }
        }
    }

    false
}

/// Collect filesystem paths matching a reservation pattern.
///
/// Mirrors legacy `_collect_matching_paths`: if the pattern contains glob chars,
/// use globbing; otherwise treat as a literal path.
fn collect_matching_paths(base: &Path, pattern: &str) -> Vec<std::path::PathBuf> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Vec::new();
    }

    let has_glob = pattern.contains('*') || pattern.contains('?') || pattern.contains('[');

    if has_glob {
        let base_str = base.to_string_lossy();
        let base_escaped = glob::Pattern::escape(&base_str);
        // We use format! instead of Path::join because base_escaped is a string
        // that may contain glob escape sequences that Path::join could mishandle.
        let full_pattern = if base_str.ends_with('/') || base_str.ends_with('\\') {
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

    let Outcome::Ok(all_reservations) =
        block_on(async { queries::list_file_reservations(cx, pool, project_id, false).await })
    else {
        return Err("failed to list reservations for artifact generation".into());
    };

    let mut res_jsons = Vec::new();
    for row in all_reservations {
        if let Some(id) = row.id {
            if released_ids.contains(&id) {
                // We need the agent name, which isn't in FileReservationRow, so we look it up
                let agent_name = match block_on(async {
                    queries::get_agent_by_id(cx, pool, row.agent_id).await
                }) {
                    Outcome::Ok(agent) => agent.name,
                    _ => format!("agent_{}", row.agent_id),
                };

                res_jsons.push(serde_json::json!({
                    "id": id,
                    "agent": agent_name,
                    "path_pattern": row.path_pattern,
                    "exclusive": row.exclusive != 0,
                    "reason": row.reason,
                    "created_ts": mcp_agent_mail_db::micros_to_iso(row.created_ts),
                    "expires_ts": mcp_agent_mail_db::micros_to_iso(row.expires_ts),
                    "released_ts": mcp_agent_mail_db::micros_to_iso(row.released_ts.unwrap_or(mcp_agent_mail_db::now_micros())),
                }));
            }
        }
    }

    if !res_jsons.is_empty() {
        let op = mcp_agent_mail_storage::WriteOp::FileReservation {
            project_slug: project.slug.clone(),
            config: config.clone(),
            reservations: res_jsons,
        };
        // Best effort
        let _ = mcp_agent_mail_storage::wbq_enqueue(op);
    }

    info!(
        project = %project.slug,
        released_count = released_ids.len(),
        "cleanup: released expired/stale reservations and enqueued archive updates"
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
            queries::register_agent(&cx, &pool, project_id, "RedFox", "test", "test", None, None)
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
            .arg("init")
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
    fn detect_and_release_stale_skips_recent_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, cx, project_id, _agent_id, reservation_id, _human_key, _pattern) =
            seed_active_reservation(&tmp);

        let mut config = Config::from_env();
        config.file_reservation_inactivity_seconds = 86_400; // one day
        config.file_reservation_activity_grace_seconds = 900;

        let released =
            detect_and_release_stale(&config, &pool, &cx, project_id).expect("stale pass");
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

        let released =
            detect_and_release_stale(&config, &pool, &cx, project_id).expect("stale pass");
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
}
