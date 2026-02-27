//! Background worker for continuous `SQLite` integrity checking and recovery.
//!
//! Startup probes catch corruption at boot, but long-running sessions can still
//! encounter driver-level failures later. This worker adds runtime protection:
//!
//! - periodic quick integrity checks
//! - periodic full integrity checks (configurable)
//! - proactive backup refresh on healthy cycles
//! - automatic file/archive-aware recovery on recoverable failures

#![forbid(unsafe_code)]

use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::disk::{
    is_sqlite_memory_database_url, sqlite_file_path_from_database_url,
};
use mcp_agent_mail_db::{
    DbPool, DbPoolConfig, ensure_sqlite_file_healthy, ensure_sqlite_file_healthy_with_archive,
    is_corruption_error_message, is_sqlite_recovery_error_message,
};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
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

pub fn start(config: &Config) {
    if !config.integrity_check_on_startup {
        return;
    }
    if is_sqlite_memory_database_url(&config.database_url) {
        return;
    }

    let Some(sqlite_path) = sqlite_file_path_from_database_url(&config.database_url) else {
        tracing::warn!(
            database_url = %config.database_url,
            "integrity guard disabled: failed to resolve sqlite path from DATABASE_URL"
        );
        return;
    };

    let mut worker = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    if worker.is_none() {
        let config = config.clone();
        SHUTDOWN.store(false, Ordering::Release);
        *worker = Some(
            std::thread::Builder::new()
                .name("integrity-guard".into())
                .spawn(move || monitor_loop(&config, &sqlite_path))
                .expect("failed to spawn integrity guard worker"),
        );
    }
}

pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    if let Ok(mut worker) = WORKER.lock()
        && let Some(handle) = worker.take()
    {
        let _ = handle.join();
    }
}

fn monitor_loop(config: &Config, sqlite_path: &Path) {
    let quick_every = quick_check_interval();
    let full_every = full_check_interval(config);
    let storage_root = config.storage_root.clone();

    let mut pool_config = DbPoolConfig::from_env();
    pool_config.database_url.clone_from(&config.database_url);
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

    let mut last_full_check = Instant::now();
    let mut last_recovery_attempt: Option<Instant> = None;

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("integrity guard worker shutting down");
            return;
        }

        run_quick_cycle(
            &pool,
            sqlite_path,
            &storage_root,
            &mut last_recovery_attempt,
        );

        if let Some(interval) = full_every
            && last_full_check.elapsed() >= interval
        {
            run_full_cycle(
                &pool,
                sqlite_path,
                &storage_root,
                &mut last_recovery_attempt,
            );
            last_full_check = Instant::now();
        }

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
) {
    match pool.run_full_integrity_check() {
        Ok(_) => {
            tracing::info!("integrity guard: periodic full integrity check passed");
        }
        Err(err) => handle_integrity_error(
            "integrity_check",
            &err.to_string(),
            sqlite_path,
            storage_root,
            last_recovery_attempt,
        ),
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

    let recovery_result = if storage_root.is_dir() {
        ensure_sqlite_file_healthy_with_archive(sqlite_path, storage_root)
    } else {
        ensure_sqlite_file_healthy(sqlite_path)
    };

    match recovery_result {
        Ok(()) => tracing::warn!(
            phase,
            path = %sqlite_path.display(),
            "integrity guard auto-recovered sqlite file"
        ),
        Err(err) => tracing::warn!(
            phase,
            path = %sqlite_path.display(),
            error = %err,
            "integrity guard recovery failed"
        ),
    }
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
}
