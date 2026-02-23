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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

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

    let config = config.clone();
    let _ = WORKER.get_or_init(|| {
        SHUTDOWN.store(false, Ordering::Release);
        std::thread::Builder::new()
            .name("integrity-guard".into())
            .spawn(move || monitor_loop(&config, &sqlite_path))
            .expect("failed to spawn integrity guard worker")
    });
}

pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

fn monitor_loop(config: &Config, sqlite_path: &Path) {
    let quick_every = quick_check_interval();
    let full_every = full_check_interval(config);
    let storage_root = config.storage_root.clone();

    let mut pool_config = DbPoolConfig::from_env();
    pool_config.database_url.clone_from(&config.database_url);
    pool_config.run_migrations = false;

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
}
