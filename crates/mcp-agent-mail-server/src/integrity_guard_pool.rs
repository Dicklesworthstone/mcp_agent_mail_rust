//! Startup admission for the integrity worker's resolved mailbox.
//!
//! A failed pool open is not a reason to permanently lose integrity checks and
//! backups. Retry on the existing owned worker thread, without restarting the
//! monitor or constructing its verification/backup schedules more than once.
//! These retries grant no recovery authority: every attempt uses the ordinary
//! pool factory and all of its source, ownership and corruption checks.

use mcp_agent_mail_core::Config;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

fn pool_config(config: &Config) -> DbPoolConfig {
    let mut selected = DbPoolConfig::from_env();
    selected.database_url.clone_from(&config.database_url);
    // The server has already resolved this mailbox. An ambient STORAGE_ROOT
    // must not redirect a later retry's source/archive authority.
    selected.storage_root = Some(config.storage_root.clone());
    selected.min_connections = 1;
    selected.max_connections = 1;
    selected.warmup_connections = 0;
    // This worker can be the first pooled checkout (including in stdio mode).
    selected.run_migrations = true;
    selected
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1_u64 << failures.saturating_sub(1).min(6)).min(MAX_RETRY_DELAY)
}

/// Keep the worker alive until its source is admitted or shutdown is requested.
/// `wait` returns true after a completed wait, false when shutdown interrupts it.
/// No connection, admission permit or registry mutex is held across a wait.
pub(super) fn open(
    config: &Config,
    stop: &AtomicBool,
    wait: impl FnMut(Duration) -> bool,
) -> Option<DbPool> {
    let selected = pool_config(config);
    retry(
        || mcp_agent_mail_db::create_pool(&selected),
        || stop.load(Ordering::Acquire),
        wait,
        |error, failures, delay| {
            tracing::warn!(
                target: "maintenance",
                event = "integrity_guard_source_admission_deferred",
                error = %error,
                failures,
                retry_in_secs = delay.as_secs(),
                "integrity guard source unavailable; retaining worker and retrying admission"
            );
        },
    )
}

fn retry<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    mut stopped: impl FnMut() -> bool,
    mut wait: impl FnMut(Duration) -> bool,
    mut refused: impl FnMut(&E, u32, Duration),
) -> Option<T> {
    let mut failures = 0_u32;
    loop {
        if stopped() {
            return None;
        }
        let error = match attempt() {
            Ok(source) => {
                // A slow constructor may complete after shutdown was signaled.
                // Do not start integrity/backup work from that late result.
                return if stopped() { None } else { Some(source) };
            }
            Err(error) => error,
        };
        failures = failures.saturating_add(1);
        let delay = retry_delay(failures);
        refused(&error, failures, delay);
        // Dispose of the failed attempt before sleeping. In particular, no
        // failed source handle or error-owned resources survive the backoff.
        drop(error);
        if stopped() || !wait(delay) {
            return None;
        }
        // Check shutdown again at the loop head even when wait returned true.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retry_pacing_is_bounded_and_success_stops_attempts() {
        let calls = Cell::new(0_u32);
        let mut delays = Vec::new();
        let mut errors = Vec::new();
        let source = retry(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 10 {
                    Err("admission contention")
                } else {
                    Ok(42)
                }
            },
            || false,
            |delay| {
                delays.push(delay.as_secs());
                true
            },
            |error, failures, delay| errors.push((*error, failures, delay.as_secs())),
        );
        assert_eq!(source, Some(42));
        assert_eq!(calls.get(), 10);
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(errors.len(), delays.len());
        for (index, (error, failures, seconds)) in errors.into_iter().enumerate() {
            assert_eq!(error, "admission contention");
            assert_eq!(usize::try_from(failures).unwrap(), index + 1);
            assert_eq!(seconds, delays[index]);
        }
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY_DELAY);
        assert_eq!(retry_delay(0), Duration::from_secs(1));
    }

    #[test]
    fn shutdown_before_or_during_wait_prevents_another_source_attempt() {
        for already_stopped in [false, true] {
            let stop = Cell::new(already_stopped);
            let calls = Cell::new(0);
            let waits = Cell::new(0);
            let source = retry::<(), _>(
                || {
                    calls.set(calls.get() + 1);
                    Err("source unavailable")
                },
                || stop.get(),
                |_| {
                    waits.set(waits.get() + 1);
                    stop.set(true);
                    // Cover a stop that races a completed wait, not only a
                    // waiter that explicitly reports interruption.
                    true
                },
                |_, _, _| {},
            );
            assert!(source.is_none());
            assert_eq!(calls.get(), i32::from(!already_stopped));
            assert_eq!(waits.get(), i32::from(!already_stopped));
        }
        assert!(retry::<(), _>(
            || Err("source unavailable"),
            || false,
            |_| false,
            |_, _, _| {},
        ).is_none());
    }

    #[test]
    fn shutdown_during_a_successful_open_discards_the_late_source() {
        struct Source<'a>(&'a Cell<bool>);
        impl Drop for Source<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let stop = Cell::new(false);
        let dropped = Cell::new(false);
        assert!(retry::<_, ()>(
            || {
                stop.set(true);
                Ok(Source(&dropped))
            },
            || stop.get(),
            |_| panic!("a successful late open must not wait"),
            |_, _, _| panic!("a successful late open is not a failure"),
        ).is_none());
        assert!(dropped.get());
    }

    fn config(root: &std::path::Path) -> Config {
        Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &root.join("mail.sqlite3"),
            ),
            storage_root: root.join("archive"),
            ..Default::default()
        }
    }

    #[test]
    fn retries_keep_the_resolved_mailbox_and_startup_schema_policy() {
        let scratch = tempfile::tempdir().unwrap();
        let config = config(scratch.path());
        let selected = pool_config(&config);
        assert_eq!(selected.database_url, config.database_url);
        assert_eq!(selected.storage_root.as_deref(), Some(config.storage_root.as_path()));
        assert_eq!(selected.min_connections, 1);
        assert_eq!(selected.max_connections, 1);
        assert_eq!(selected.warmup_connections, 0);
        assert!(selected.run_migrations);
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test]
    fn real_source_recovers_after_admission_errors_without_restarting_the_worker() {
        let scratch = tempfile::tempdir().unwrap();
        let blocked = scratch.path().join("blocked");
        let preserved = scratch.path().join("preserved-obstruction");
        std::fs::write(&blocked, b"preserve source evidence").unwrap();
        let config = config(&blocked);
        let mut delays = Vec::new();
        // Run the production entrypoint and real pool factory. Only the wait
        // is deterministic: an operator repairs the inaccessible parent after
        // three failed admissions. No database or source bytes are deleted.
        let pool = open(&config, &AtomicBool::new(false), |delay| {
            delays.push(delay.as_secs());
            assert_eq!(std::fs::read(&blocked).unwrap(), b"preserve source evidence");
            if delays.len() == 3 {
                std::fs::rename(&blocked, &preserved).unwrap();
                std::fs::create_dir(&blocked).unwrap();
                std::fs::create_dir(&config.storage_root).unwrap();
            }
            assert!(delays.len() <= 3, "repaired source must be admitted");
            true
        }).expect("source eventually admitted on the same worker invocation");
        assert_eq!(delays, [1, 2, 4]);
        assert_eq!(std::fs::read(&preserved).unwrap(), b"preserve source evidence");
        let cx = asupersync::Cx::for_testing();
        let conn = fastmcp_core::block_on(pool.acquire(&cx)).into_result().unwrap();
        let rows = conn.query_sync("SELECT COUNT(*) AS c FROM messages", &[]).unwrap();
        assert_eq!(rows[0].get_named::<i64>("c").unwrap(), 0);
        assert!(blocked.join("mail.sqlite3").is_file());
    }

    #[test]
    fn permanent_real_source_failure_stays_paced_and_shutdown_preserves_evidence() {
        let scratch = tempfile::tempdir().unwrap();
        let blocked = scratch.path().join("blocked");
        std::fs::write(&blocked, b"keep this obstruction").unwrap();
        let config = config(&blocked);
        let stop = AtomicBool::new(false);
        let mut delays = Vec::new();
        let source = open(&config, &stop, |delay| {
            delays.push(delay.as_secs());
            assert_eq!(std::fs::read(&blocked).unwrap(), b"keep this obstruction");
            if delays.len() == 3 {
                stop.store(true, Ordering::Release);
            }
            true
        });
        assert!(source.is_none());
        assert_eq!(delays, [1, 2, 4]);
        assert_eq!(std::fs::read(&blocked).unwrap(), b"keep this obstruction");
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 1);
    }
}
