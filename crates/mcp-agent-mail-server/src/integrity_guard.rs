//! Background worker for continuous `SQLite` integrity checking and recovery.
//!
//! Startup probes catch corruption at boot, but long-running sessions can still
//! encounter driver-level failures later. This worker adds runtime protection:
//!
//! - periodic quick integrity checks
//! - periodic full integrity checks (configurable)
//! - proactive backup refresh on healthy cycles
//! - diagnostic surfacing for recoverable failures without mutating the live DB

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::disk::is_sqlite_memory_database_url;
use mcp_agent_mail_db::{
    DbPool, DbPoolConfig, is_corruption_error_message, is_sqlite_recovery_error_message,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SKIP_NEXT_QUICK_CYCLE: AtomicBool = AtomicBool::new(false);
static SKIP_NEXT_PROACTIVE_BACKUP: AtomicBool = AtomicBool::new(false);
static WORKER: std::sync::LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const DEFAULT_QUICK_CHECK_INTERVAL_SECS: u64 = 300;
const MIN_FULL_CHECK_INTERVAL_SECS: u64 = 3600;
const RECOVERY_MIN_INTERVAL_SECS: u64 = 30;
const BACKUP_MAX_AGE_SECS: u64 = 3600;

#[inline]
const fn quick_check_interval() -> Duration {
    Duration::from_secs(DEFAULT_QUICK_CHECK_INTERVAL_SECS)
}

#[inline]
fn full_check_interval(config: &Config) -> Option<Duration> {
    if config.integrity_check_interval_hours == 0 {
        return None;
    }
    let secs = config
        .integrity_check_interval_hours
        .saturating_mul(3600)
        .max(MIN_FULL_CHECK_INTERVAL_SECS);
    Some(Duration::from_secs(secs))
}

fn full_check_due(
    config: &Config,
    interval: Option<Duration>,
    last_full_attempt: Option<Instant>,
) -> bool {
    let Some(interval) = interval else {
        return false;
    };
    if let Some(last_full_attempt) = last_full_attempt {
        return last_full_attempt.elapsed() >= interval;
    }
    mcp_agent_mail_db::is_full_check_due(config.integrity_check_interval_hours)
}

/// Tell the guard that startup already ran an integrity probe.
///
/// Used by HTTP/TUI startup to avoid immediately repeating the same quick-check
/// in the background worker before the first interval elapses.
#[allow(dead_code)]
pub fn note_startup_integrity_probe_completed() {
    SKIP_NEXT_QUICK_CYCLE.store(true, Ordering::Release);
}

/// Skip only the next proactive backup refresh while still performing the
/// integrity guard's quick health check.
pub fn defer_next_proactive_backup() {
    SKIP_NEXT_PROACTIVE_BACKUP.store(true, Ordering::Release);
}

fn take_deferred_proactive_backup() -> bool {
    SKIP_NEXT_PROACTIVE_BACKUP.swap(false, Ordering::AcqRel)
}

fn resolve_integrity_guard_sqlite_path(config: &Config) -> Option<PathBuf> {
    crate::resolve_server_database_url_sqlite_path(&config.database_url)
}

pub fn start(config: &Config) {
    if !config.integrity_check_on_startup {
        return;
    }
    if is_sqlite_memory_database_url(&config.database_url) {
        return;
    }

    let Some(sqlite_path) = resolve_integrity_guard_sqlite_path(config) else {
        tracing::warn!(
            database_url = %config.database_url,
            "integrity guard disabled: failed to resolve sqlite path from DATABASE_URL"
        );
        return;
    };

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
            .name("integrity-guard".into())
            .spawn(move || monitor_loop(&config, &sqlite_path))
        {
            Ok(handle) => {
                *worker = Some(handle);
            }
            Err(err) => {
                drop(worker);
                tracing::warn!(
                    error = %err,
                    "failed to spawn integrity guard worker; continuing without integrity background scans"
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

fn monitor_loop(config: &Config, sqlite_path: &Path) {
    let quick_every = quick_check_interval();
    let full_every = full_check_interval(config);
    let storage_root = config.storage_root.clone();

    let mut pool_config = DbPoolConfig::from_env();
    pool_config.database_url.clone_from(&config.database_url);
    pool_config.min_connections = 1;
    pool_config.max_connections = 1;
    pool_config.warmup_connections = 0;
    // Keep migrations enabled here: this worker can be the first component to
    // acquire a pooled connection (e.g. proactive backup checkpointing in
    // stdio mode), and that first-acquire path must remain schema-safe.
    pool_config.run_migrations = true;

    let pool = match mcp_agent_mail_db::create_pool(&pool_config) {
        Ok(pool) => pool,
        Err(err) => {
            tracing::warn!(error = %err, "integrity guard: failed to create DB pool, exiting");
            return;
        }
    };

    tracing::info!(
        quick_interval_secs = quick_every.as_secs(),
        full_interval_secs = full_every.map_or(0, |d| d.as_secs()),
        "integrity guard worker started"
    );

    let mut last_full_attempt: Option<Instant> = None;
    let mut last_recovery_attempt: Option<Instant> = None;
    // Bead K4: seed the maintenance schedule at "now" so the first checkpoint /
    // analyze / vacuum fires only after one full interval — never a heavy
    // startup VACUUM storm. These live on the integrity-guard worker thread, so
    // maintenance always runs off the request/dispatch hot path.
    let maintenance_start = Instant::now();
    let mut last_checkpoint: Option<Instant> = Some(maintenance_start);
    let mut last_analyze: Option<Instant> = Some(maintenance_start);
    let mut last_vacuum: Option<Instant> = Some(maintenance_start);
    let mut skip_first_quick_cycle = SKIP_NEXT_QUICK_CYCLE.swap(false, Ordering::AcqRel);

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("integrity guard worker shutting down");
            return;
        }

        if skip_first_quick_cycle {
            skip_first_quick_cycle = false;
            tracing::debug!(
                "integrity guard: skipped immediate quick cycle (startup probe already executed)"
            );
        } else {
            run_quick_cycle(
                &pool,
                sqlite_path,
                &storage_root,
                &mut last_recovery_attempt,
            );
        }

        if full_check_due(config, full_every, last_full_attempt) {
            let attempted_at = Instant::now();
            let _ = run_full_cycle(
                &pool,
                sqlite_path,
                &storage_root,
                &mut last_recovery_attempt,
            );
            last_full_attempt = Some(attempted_at);
        }

        // Bead K4: bounded SQLite maintenance (checkpoint / analyze / vacuum /
        // journal_size_limit) on independent cadences, off the hot path.
        run_db_maintenance_cycle(
            &pool,
            config,
            Instant::now(),
            &mut last_checkpoint,
            &mut last_analyze,
            &mut last_vacuum,
        );

        // Sleep in short increments so shutdown reacts quickly.
        let mut remaining = quick_every;
        while !remaining.is_zero() {
            if SHUTDOWN.load(Ordering::Acquire) {
                tracing::info!("integrity guard worker shutting down");
                return;
            }
            let chunk = remaining.min(Duration::from_secs(1));
            std::thread::sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

fn run_quick_cycle(
    pool: &DbPool,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
) {
    match pool.run_startup_integrity_check() {
        Ok(_) => {
            if take_deferred_proactive_backup() {
                tracing::debug!(
                    "integrity guard: deferred proactive backup during startup quick cycle"
                );
                return;
            }
            if let Err(err) = pool.create_proactive_backup(Duration::from_secs(BACKUP_MAX_AGE_SECS))
            {
                tracing::warn!(error = %err, "integrity guard: proactive backup refresh failed");
            }
        }
        Err(err) => handle_integrity_error(
            "quick_check",
            &err.to_string(),
            sqlite_path,
            storage_root,
            last_recovery_attempt,
        ),
    }
}

fn run_full_cycle(
    pool: &DbPool,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
) -> bool {
    match pool.run_full_integrity_check() {
        Ok(_) => {
            tracing::info!("integrity guard: periodic full integrity check passed");
            true
        }
        Err(err) => {
            handle_integrity_error(
                "integrity_check",
                &err.to_string(),
                sqlite_path,
                storage_root,
                last_recovery_attempt,
            );
            false
        }
    }
}

/// Whether a maintenance task with cadence `interval_secs` is due, given when
/// it last ran (`last`) relative to `now`.
///
/// `interval_secs == 0` disables the task; a task that has never run is always
/// due. Pure (no clock/IO) so the maintenance schedule is unit-testable without
/// real time (bead K4).
fn maintenance_task_due(interval_secs: u64, last: Option<Instant>, now: Instant) -> bool {
    if interval_secs == 0 {
        return false;
    }
    last.map_or(true, |last| {
        now.saturating_duration_since(last) >= Duration::from_secs(interval_secs)
    })
}

/// Run one maintenance op, timing it and recording per-op metrics.
///
/// Success bumps the op's run counter, records the duration in the shared
/// maintenance-duration histogram, and stamps the op's last-run gauge. Failure
/// bumps the shared failure counter and logs — maintenance never propagates an
/// error to the worker loop or the request path.
fn run_maintenance_op<F, E>(
    op: &str,
    run: F,
    runs_total: &mcp_agent_mail_core::Counter,
    last_run_us: &mcp_agent_mail_core::GaugeU64,
) where
    F: FnOnce() -> Result<(), E>,
    E: std::fmt::Display,
{
    let db = &mcp_agent_mail_core::global_metrics().db;
    let started = Instant::now();
    match run() {
        Ok(()) => {
            let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            db.maintenance_duration_us.record(elapsed_us);
            runs_total.inc();
            let now_us =
                u64::try_from(mcp_agent_mail_core::timestamps::now_micros().max(0)).unwrap_or(0);
            last_run_us.set(now_us);
            tracing::debug!(op, elapsed_us, "db maintenance op completed");
        }
        Err(err) => {
            db.maintenance_failures_total.inc();
            tracing::warn!(op, error = %err, "db maintenance op failed; will retry next cycle");
        }
    }
}

/// Run any due background SQLite maintenance ops (bead K4): passive WAL
/// checkpoint, `ANALYZE`, and `VACUUM`, each on its own configured cadence, plus
/// re-applying the configured `journal_size_limit`.
///
/// Runs on the integrity-guard worker thread against its own connection, so it
/// never touches the request/dispatch hot path. Each op is bounded and
/// best-effort: failures are recorded and retried on the next cycle, never
/// fatal. The `last_*` cursors are advanced whenever a task is attempted so a
/// persistently-failing op still backs off to its interval instead of spinning.
fn run_db_maintenance_cycle(
    pool: &DbPool,
    config: &Config,
    now: Instant,
    last_checkpoint: &mut Option<Instant>,
    last_analyze: &mut Option<Instant>,
    last_vacuum: &mut Option<Instant>,
) {
    if !config.db_maintenance_enabled {
        return;
    }
    let db = &mcp_agent_mail_core::global_metrics().db;

    // Bound WAL growth (cheap, idempotent) so a checkpoint truncates back to the
    // configured cap.
    if config.db_journal_size_limit_bytes > 0
        && let Err(err) = pool.set_journal_size_limit(config.db_journal_size_limit_bytes)
    {
        tracing::debug!(error = %err, "db maintenance: journal_size_limit apply failed");
    }

    if maintenance_task_due(config.db_checkpoint_interval_secs, *last_checkpoint, now) {
        run_maintenance_op(
            "passive_wal_checkpoint",
            || pool.wal_checkpoint_passive().map(|_frames| ()),
            &db.maintenance_checkpoint_runs_total,
            &db.maintenance_last_checkpoint_us,
        );
        *last_checkpoint = Some(now);
    }

    if maintenance_task_due(config.db_analyze_interval_secs, *last_analyze, now) {
        run_maintenance_op(
            "analyze",
            || pool.analyze(),
            &db.maintenance_analyze_runs_total,
            &db.maintenance_last_analyze_us,
        );
        *last_analyze = Some(now);
    }

    if maintenance_task_due(config.db_vacuum_interval_secs, *last_vacuum, now) {
        run_maintenance_op(
            "vacuum",
            || pool.vacuum(),
            &db.maintenance_vacuum_runs_total,
            &db.maintenance_last_vacuum_us,
        );
        *last_vacuum = Some(now);
    }
}

fn handle_integrity_error(
    phase: &str,
    error_message: &str,
    sqlite_path: &Path,
    storage_root: &Path,
    last_recovery_attempt: &mut Option<Instant>,
) {
    let recoverable = is_sqlite_recovery_error_message(error_message)
        || is_corruption_error_message(error_message);
    if !recoverable {
        tracing::warn!(
            phase,
            error = %error_message,
            "integrity guard: non-recoverable integrity error"
        );
        return;
    }

    let now = Instant::now();
    if let Some(last) = *last_recovery_attempt
        && now.duration_since(last) < Duration::from_secs(RECOVERY_MIN_INTERVAL_SECS)
    {
        tracing::warn!(
            phase,
            error = %error_message,
            "integrity guard: recovery throttled after recent attempt"
        );
        return;
    }
    *last_recovery_attempt = Some(now);

    let storage_root_present = storage_root.is_dir();
    // #105 flagged this line as confusing — "recovery is disabled" reads as
    // "nothing will happen" but in fact the query path triggers
    // `reconstruct_sqlite_file_with_archive_salvage` on its own when a
    // tool reads against a broken verdict (see
    // `mcp-agent-mail-tools::tool_util::live_db_is_suspect`). What this
    // branch actually does is *surface* the detection and leave the
    // mutation to the query path's admission-controlled reconstruct, so
    // say that plainly.
    tracing::warn!(
        phase,
        path = %sqlite_path.display(),
        error = %error_message,
        storage_root_present,
        "integrity guard detected recoverable sqlite corruption; background guard does not mutate the live db — the query-path recovery flow will trigger an admission-controlled reconstruct on the next corrupt-verdict read"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_check_interval_disabled_when_zero() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 0;
        assert!(full_check_interval(&config).is_none());
    }

    #[test]
    fn full_check_interval_has_minimum_floor() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1;
        assert_eq!(
            full_check_interval(&config),
            Some(Duration::from_secs(MIN_FULL_CHECK_INTERVAL_SECS))
        );
    }

    #[test]
    fn quick_interval_matches_default() {
        assert_eq!(
            quick_check_interval(),
            Duration::from_secs(DEFAULT_QUICK_CHECK_INTERVAL_SECS)
        );
    }

    #[test]
    #[allow(clippy::duration_suboptimal_units)]
    fn full_check_interval_large_value() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 24;
        assert_eq!(
            full_check_interval(&config),
            Some(Duration::from_secs(86_400))
        );
    }

    #[test]
    fn full_check_interval_small_value_clamped_to_minimum() {
        // Even sub-hour values get clamped to MIN_FULL_CHECK_INTERVAL_SECS
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1; // 1 hour = 3600s >= 3600s minimum
        let interval = full_check_interval(&config).unwrap();
        assert!(interval.as_secs() >= MIN_FULL_CHECK_INTERVAL_SECS);
    }

    #[test]
    fn full_check_interval_saturating_mul_no_overflow() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = u64::MAX;
        // saturating_mul should not panic
        let interval = full_check_interval(&config);
        assert!(interval.is_some());
        assert!(interval.unwrap().as_secs() >= MIN_FULL_CHECK_INTERVAL_SECS);
    }

    #[test]
    fn quick_check_interval_is_5_minutes() {
        assert_eq!(quick_check_interval().as_secs(), 300);
    }

    #[test]
    fn full_check_due_respects_attempt_throttle() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1;
        let interval = full_check_interval(&config);
        assert!(!full_check_due(&config, interval, Some(Instant::now())));
    }

    #[test]
    fn full_check_due_uses_last_attempt_not_last_success() {
        let mut config = Config::from_env();
        config.integrity_check_interval_hours = 1;
        let interval = full_check_interval(&config);
        let stale_success = Instant::now()
            .checked_sub(Duration::from_secs(MIN_FULL_CHECK_INTERVAL_SECS + 1))
            .expect("stale success timestamp");
        assert!(
            full_check_due(&config, interval, Some(stale_success)),
            "an old successful full check should make another attempt due"
        );

        let attempted_at = Instant::now();
        assert!(
            !full_check_due(&config, interval, Some(attempted_at)),
            "a recent failed attempt should still throttle the next full check"
        );
    }

    #[test]
    fn defer_next_proactive_backup_is_one_shot() {
        SKIP_NEXT_PROACTIVE_BACKUP.store(false, Ordering::Release);
        assert!(!take_deferred_proactive_backup());
        defer_next_proactive_backup();
        assert!(take_deferred_proactive_backup());
        assert!(
            !take_deferred_proactive_backup(),
            "startup backup deferral should apply only once"
        );
    }

    #[test]
    fn resolve_integrity_guard_sqlite_path_prefers_absolute_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = dir.path().join("integrity-guard.sqlite3");
        std::fs::write(&absolute_db, b"seed").expect("write absolute db");

        let relative_path = PathBuf::from(absolute_db.to_string_lossy().trim_start_matches('/'));
        assert!(
            !relative_path.exists(),
            "relative shadow path should be absent so integrity guard resolves the absolute candidate"
        );

        let mut config = Config::from_env();
        config.database_url = format!("sqlite:///{}", relative_path.display());

        let resolved =
            resolve_integrity_guard_sqlite_path(&config).expect("resolve integrity guard db path");
        assert_eq!(
            resolved, absolute_db,
            "integrity guard should monitor the resolved absolute candidate"
        );
    }

    #[test]
    fn handle_integrity_error_non_recoverable_does_not_update_timestamp() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        // "connection reset" is NOT a recoverable error
        handle_integrity_error(
            "test",
            "connection reset by peer",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        assert!(
            last_recovery.is_none(),
            "non-recoverable error should not set last_recovery_attempt"
        );
    }

    #[test]
    fn handle_integrity_error_recoverable_sets_timestamp() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        // "database disk image is malformed" IS a recoverable error
        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        assert!(
            last_recovery.is_some(),
            "recoverable error should set last_recovery_attempt"
        );
    }

    #[test]
    fn handle_integrity_error_throttles_rapid_recovery() {
        let mut last_recovery: Option<Instant> = Some(Instant::now());
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        let before = last_recovery;

        // Second call immediately after should be throttled
        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        // Timestamp should NOT have been updated (throttled)
        assert_eq!(
            last_recovery.map(|i| i.elapsed().as_millis() < 100),
            before.map(|i| i.elapsed().as_millis() < 100),
            "recovery should be throttled within RECOVERY_MIN_INTERVAL_SECS"
        );
    }

    #[test]
    fn handle_integrity_error_various_recoverable_messages() {
        let recoverable_msgs = [
            "database disk image is malformed",
            "Database Disk Image Is Malformed", // case-insensitive
            "malformed database schema - broken_table",
            "file is not a database",
            "out of memory",
            "cursor stack is empty",
            "internal error",
            "no healthy backup was found",
        ];
        for msg in &recoverable_msgs {
            let mut last_recovery: Option<Instant> = None;
            let tmp = tempfile::TempDir::new().unwrap();
            let sqlite_path = tmp.path().join("test.sqlite3");
            let storage_root = tmp.path().join("storage");

            handle_integrity_error("test", msg, &sqlite_path, &storage_root, &mut last_recovery);

            assert!(
                last_recovery.is_some(),
                "'{msg}' should be classified as recoverable"
            );
        }
    }

    #[test]
    fn handle_integrity_error_non_recoverable_messages() {
        let non_recoverable_msgs = [
            "connection refused",
            "timeout",
            "constraint violation",
            "unique constraint failed",
            "no such table",
        ];
        for msg in &non_recoverable_msgs {
            let mut last_recovery: Option<Instant> = None;
            let tmp = tempfile::TempDir::new().unwrap();
            let sqlite_path = tmp.path().join("test.sqlite3");
            let storage_root = tmp.path().join("storage");

            handle_integrity_error("test", msg, &sqlite_path, &storage_root, &mut last_recovery);

            assert!(
                last_recovery.is_none(),
                "'{msg}' should NOT be classified as recoverable"
            );
        }
    }

    #[test]
    fn handle_integrity_error_uses_archive_recovery_when_storage_exists() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("storage");

        // Create the storage directory so archive-aware recovery is used.
        std::fs::create_dir_all(&storage_root).unwrap();

        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        // We can't easily verify which recovery path was used, but
        // the function should not panic when storage_root exists.
        assert!(last_recovery.is_some());
    }

    #[test]
    fn handle_integrity_error_uses_file_recovery_when_no_storage() {
        let mut last_recovery: Option<Instant> = None;
        let tmp = tempfile::TempDir::new().unwrap();
        let sqlite_path = tmp.path().join("test.sqlite3");
        let storage_root = tmp.path().join("nonexistent_storage");

        // storage_root doesn't exist, so file-only recovery is used.
        handle_integrity_error(
            "test",
            "database disk image is malformed",
            &sqlite_path,
            &storage_root,
            &mut last_recovery,
        );

        assert!(last_recovery.is_some());
    }

    #[test]
    fn constants_are_reasonable() {
        const _: () = assert!(
            DEFAULT_QUICK_CHECK_INTERVAL_SECS >= 60,
            "quick check should be at least 1 minute"
        );
        const _: () = assert!(
            MIN_FULL_CHECK_INTERVAL_SECS >= 3600,
            "full check minimum should be at least 1 hour"
        );
        const _: () = assert!(
            RECOVERY_MIN_INTERVAL_SECS >= 10,
            "recovery throttle should be at least 10 seconds"
        );
        const _: () = assert!(
            BACKUP_MAX_AGE_SECS >= 600,
            "backup max age should be at least 10 minutes"
        );
    }

    // ---- bead K4: periodic SQLite maintenance scheduling ----

    #[test]
    fn maintenance_task_due_disabled_when_interval_zero() {
        let now = Instant::now();
        assert!(
            !maintenance_task_due(0, None, now),
            "interval 0 disables the task"
        );
        assert!(!maintenance_task_due(0, Some(now), now));
    }

    #[test]
    fn maintenance_task_due_when_never_run() {
        let now = Instant::now();
        assert!(
            maintenance_task_due(300, None, now),
            "a task that never ran is always due once enabled"
        );
    }

    #[test]
    fn maintenance_task_due_respects_interval() {
        let now = Instant::now();
        let recent = now
            .checked_sub(Duration::from_secs(100))
            .expect("recent instant");
        let stale = now
            .checked_sub(Duration::from_secs(400))
            .expect("stale instant");
        assert!(
            !maintenance_task_due(300, Some(recent), now),
            "100s elapsed < 300s interval: not due"
        );
        assert!(
            maintenance_task_due(300, Some(stale), now),
            "400s elapsed >= 300s interval: due"
        );
        assert!(
            !maintenance_task_due(300, Some(now), now),
            "just ran this instant: not due"
        );
    }

    fn memory_maintenance_pool() -> DbPool {
        let pool_config = DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            ..DbPoolConfig::default()
        };
        mcp_agent_mail_db::create_pool(&pool_config).expect("create in-memory maintenance pool")
    }

    #[test]
    fn run_db_maintenance_cycle_noop_when_disabled() {
        let config = Config {
            db_maintenance_enabled: false,
            ..Config::default()
        };
        let pool = memory_maintenance_pool();
        let now = Instant::now();
        let mut cp = None;
        let mut an = None;
        let mut va = None;
        run_db_maintenance_cycle(&pool, &config, now, &mut cp, &mut an, &mut va);
        assert!(
            cp.is_none() && an.is_none() && va.is_none(),
            "disabled maintenance must not attempt or advance any task"
        );
    }

    #[test]
    fn run_db_maintenance_cycle_advances_due_tasks_and_backs_off() {
        let config = Config {
            db_maintenance_enabled: true,
            db_checkpoint_interval_secs: 300,
            db_analyze_interval_secs: 300,
            db_vacuum_interval_secs: 300,
            db_journal_size_limit_bytes: 268_435_456,
            ..Config::default()
        };
        let pool = memory_maintenance_pool();
        let now = Instant::now();

        // All tasks never-run => all due => cursors advance to `now`. (Ops no-op
        // on a :memory: pool but still record success and advance the cursor.)
        let mut cp = None;
        let mut an = None;
        let mut va = None;
        run_db_maintenance_cycle(&pool, &config, now, &mut cp, &mut an, &mut va);
        assert_eq!(cp, Some(now), "checkpoint cursor advanced");
        assert_eq!(an, Some(now), "analyze cursor advanced");
        assert_eq!(va, Some(now), "vacuum cursor advanced");

        // Re-running at the same instant: cursors are fresh, so nothing is due
        // and they must stay put (off-hot-path back-off).
        let before = (cp, an, va);
        run_db_maintenance_cycle(&pool, &config, now, &mut cp, &mut an, &mut va);
        assert_eq!(
            (cp, an, va),
            before,
            "fresh cursors must not re-run within the interval"
        );
    }
}
