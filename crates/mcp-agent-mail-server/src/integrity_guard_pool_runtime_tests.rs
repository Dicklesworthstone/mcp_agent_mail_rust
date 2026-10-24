//! Real retirement/re-admission plus preservation of the worker's evidence gates.
//! Global admission, pool residency and integrity evidence require child isolation.

use super::super::{run_live_integrity_followups, schedule};
use super::*;
use mcp_agent_mail_db::write_barrier::{self, DrainOutcome};
use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn isolated() -> bool {
    const CHILD: &str = "AM_TEST_INTEGRITY_RUNTIME_READMISSION";
    let thread = std::thread::current();
    let name = thread.name().expect("named test");
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let mut log = tempfile::tempfile().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(log.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut expired = false;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            expired = true;
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().unwrap();
    log.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    log.read_to_string(&mut output).unwrap();
    assert!(
        !expired && status.success() && output.contains("1 passed; 0 failed"),
        "isolated {name} failed (timeout={expired}): {output}"
    );
    true
}

fn mailbox() -> (tempfile::TempDir, Config, DbPool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let config = Config {
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&root.join("mail.sqlite3")),
        storage_root: root.join("archive"),
        ..Default::default()
    };
    std::fs::create_dir(&config.storage_root).unwrap();
    let pool = open(&config, &AtomicBool::new(false), |_| {
        panic!("fresh fixture admission")
    })
    .expect("fixture pool");
    let cx = asupersync::Cx::for_testing();
    let conn = fastmcp_core::block_on(pool.acquire(&cx))
        .into_result()
        .unwrap();
    conn.execute_raw("CREATE TABLE runtime_guard_sentinel (value INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute_raw("INSERT INTO runtime_guard_sentinel VALUES (73)")
        .unwrap();
    drop(conn);
    (temp, config, pool)
}

fn assert_sentinel(pool: &DbPool) {
    let cx = asupersync::Cx::for_testing();
    let conn = fastmcp_core::block_on(pool.acquire(&cx))
        .into_result()
        .unwrap();
    let rows = conn
        .query_sync("SELECT value FROM runtime_guard_sentinel", &[])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_named::<i64>("value").unwrap(), 73);
}

fn retire(pool: &DbPool) {
    let (_barrier, drain) =
        write_barrier::acquire_promotion_barrier_draining(Duration::from_secs(2));
    assert!(!matches!(drain, DrainOutcome::TimedOut { .. }));
    // The existing healthy-file recovery regression exercises real retirement
    // before the parser-only recovery trigger is rejected. No corrupt fixture,
    // forced deletion or mocked is_closed flag is needed.
    assert!(
        pool.try_recover_from_corruption("integrity guard healthy-file retirement test")
            .is_err()
    );
    assert!(pool.is_closed(), "fixture must actually retire the engine");
}

fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink());
            let bytes = if kind.is_dir() {
                directories.push(path.clone());
                None
            } else {
                assert!(kind.is_file());
                Some(std::fs::read(&path).unwrap())
            };
            result.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
        }
    }
    result
}

#[test]
fn healthy_engine_and_existing_verification_history_are_kept_without_reopening() {
    if isolated() {
        return;
    }
    let (_temp, config, mut pool) = mailbox();
    let engine = pool.engine_identity_key();
    let now = Instant::now();
    let mut gate = schedule::FullVerificationGate::default();
    gate.complete(now, false);
    let before = format!("{gate:?}");
    let mut skip = true;
    for _ in 0..8 {
        assert!(ensure_current(
            &config,
            &AtomicBool::new(false),
            &mut pool,
            &mut gate,
            &mut skip,
            |_| panic!("a live engine must not enter re-admission"),
        ));
    }
    assert_eq!(pool.engine_identity_key(), engine);
    assert_eq!(format!("{gate:?}"), before);
    assert!(skip);
    assert_sentinel(&pool);
}

#[test]
fn retired_engine_resumes_real_sql_without_erasing_full_or_backup_backoff() {
    if isolated() {
        return;
    }
    let (_temp, config, mut pool) = mailbox();
    let old = pool.engine_identity_key();
    retire(&pool);
    let now = Instant::now();
    let mut gate = schedule::FullVerificationGate::default();
    gate.complete(now, false);
    gate.complete(now, false);
    let full_delay = gate.retry_remaining(now);
    let mut backup = schedule::AutomaticBackupSchedule::default();
    backup.request_verified();
    backup.complete(
        schedule::BackupKind::Verified,
        schedule::BackupCompletion::Failed,
        now,
    );
    let backup_before = format!("{backup:?}");
    let mut skip = true;
    assert!(ensure_current(
        &config,
        &AtomicBool::new(false),
        &mut pool,
        &mut gate,
        &mut skip,
        |_| panic!("idle healthy replacement must be admitted"),
    ));
    assert!(!pool.is_closed());
    assert_ne!(pool.engine_identity_key(), old);
    assert_sentinel(&pool);
    assert!(gate.is_required());
    assert!(
        !skip,
        "a pre-recovery startup probe cannot authorize this engine"
    );
    assert_eq!(gate.retry_remaining(now), full_delay);
    assert_eq!(format!("{backup:?}"), backup_before);

    assert!(!run_live_integrity_followups(
        &pool,
        true,
        false,
        &mut gate,
        || now,
        || panic!("source replacement must not bypass full retry backoff"),
        |_| panic!("old evidence cannot authorize writes"),
    ));
    assert!(run_live_integrity_followups(
        &pool,
        true,
        false,
        &mut gate,
        || now + full_delay,
        || true,
        |full_completed| {
            assert!(
                full_completed,
                "the replacement requires a fresh full probe"
            );
            assert!(backup.next_attempt(now + full_delay).is_none());
            true
        },
    ));
    assert_eq!(backup.consecutive_failures(), 1);
}

#[test]
fn active_promotion_defers_reopening_without_touching_source_and_releases_before_retry() {
    if isolated() {
        return;
    }
    let (temp, config, mut pool) = mailbox();
    retire(&pool);
    let before = tree_bytes(temp.path());
    let mut promotion = Some(write_barrier::try_acquire_promotion_barrier_if_idle().unwrap());
    let mut gate = schedule::FullVerificationGate::default();
    let mut skip = true;
    let mut delays = Vec::new();
    assert!(ensure_current(
        &config,
        &AtomicBool::new(false),
        &mut pool,
        &mut gate,
        &mut skip,
        |delay| {
            delays.push(delay.as_secs());
            assert_eq!(
                delays,
                [1],
                "the external promotion must be the only refusal"
            );
            assert_eq!(write_barrier::active_writer_count(), 0);
            assert_eq!(tree_bytes(temp.path()), before);
            drop(promotion.take());
            true
        },
    ));
    assert!(gate.is_required());
    assert!(!skip);
    assert_sentinel(&pool);
    assert!(write_barrier::try_acquire_promotion_barrier_if_idle().is_some());
}

#[test]
fn hot_successor_is_reused_even_while_other_writers_are_active() {
    if isolated() {
        return;
    }
    let (_temp, config, mut old) = mailbox();
    retire(&old);
    let successor = open(&config, &AtomicBool::new(false), |_| {
        panic!("successor open")
    })
    .unwrap();
    let engine = successor.engine_identity_key();
    let _writer = write_barrier::begin_write_activity();
    let mut gate = schedule::FullVerificationGate::default();
    let mut skip = true;
    assert!(ensure_current(
        &config,
        &AtomicBool::new(false),
        &mut old,
        &mut gate,
        &mut skip,
        |_| panic!("a cached successor must not drain active writers"),
    ));
    assert_eq!(old.engine_identity_key(), engine);
    assert_eq!(write_barrier::active_writer_count(), 1);
    assert_sentinel(&old);
}

#[test]
fn cold_reopening_cannot_bypass_an_unsuccessful_writer_drain() {
    if isolated() {
        return;
    }
    let (temp, config, pool) = mailbox();
    retire(&pool);
    let before = tree_bytes(temp.path());
    let writer = write_barrier::begin_write_activity();
    let Err(error) = runtime_candidate(&pool_config(&config)) else {
        panic!("cold bootstrap must not open a source before writers drain");
    };
    assert!(error.to_string().contains("writers did not drain"));
    assert_eq!(write_barrier::active_writer_count(), 1);
    assert_eq!(tree_bytes(temp.path()), before);
    drop(writer);
    let replacement = runtime_candidate(&pool_config(&config)).unwrap();
    assert!(!replacement.is_closed());
    assert_sentinel(&replacement);
}

#[test]
fn shutdown_during_readmission_preserves_source_and_does_not_reset_failed_verification() {
    if isolated() {
        return;
    }
    let (temp, config, mut pool) = mailbox();
    retire(&pool);
    let before = tree_bytes(temp.path());
    let _promotion = write_barrier::try_acquire_promotion_barrier_if_idle().unwrap();
    let stop = AtomicBool::new(false);
    let now = Instant::now();
    let mut gate = schedule::FullVerificationGate::default();
    gate.complete(now, false);
    let before_gate = format!("{gate:?}");
    let mut skip = true;
    let mut waits = 0;
    assert!(!ensure_current(
        &config,
        &stop,
        &mut pool,
        &mut gate,
        &mut skip,
        |delay| {
            waits += 1;
            assert_eq!(delay, Duration::from_secs(1));
            stop.store(true, Ordering::Release);
            true
        },
    ));
    assert_eq!(waits, 1);
    assert!(pool.is_closed());
    assert!(!skip);
    assert_eq!(format!("{gate:?}"), before_gate);
    assert_eq!(tree_bytes(temp.path()), before);
}

#[test]
fn critical_pressure_blocks_cold_readmission_but_keeps_an_admitted_successor_usable() {
    if isolated() {
        return;
    }
    struct RestorePressure(u64);
    impl Drop for RestorePressure {
        fn drop(&mut self) {
            mcp_agent_mail_core::global_metrics()
                .system
                .disk_pressure_level
                .set(self.0);
        }
    }
    let (temp, config, pool) = mailbox();
    retire(&pool);
    let metric = &mcp_agent_mail_core::global_metrics()
        .system
        .disk_pressure_level;
    let _restore = RestorePressure(metric.load());
    let before = tree_bytes(temp.path());
    metric.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
    let Err(error) = runtime_candidate(&pool_config(&config)) else {
        panic!("critical pressure must stop cold source creation");
    };
    assert!(error.to_string().contains("critical disk pressure"));
    assert_eq!(tree_bytes(temp.path()), before);
    assert_eq!(write_barrier::active_writer_count(), 0);
    metric.set(0);
    let successor = runtime_candidate(&pool_config(&config)).unwrap();
    metric.set(mcp_agent_mail_core::disk::DiskPressure::Critical.as_u64());
    let cached = runtime_candidate(&pool_config(&config)).unwrap();
    assert_eq!(
        cached.engine_identity_key(),
        successor.engine_identity_key()
    );
    assert_sentinel(&cached);
}

#[test]
fn retired_probe_results_never_authorize_followups_even_when_a_full_probe_returned_true() {
    if isolated() {
        return;
    }
    for retire_during_probe in [false, true] {
        let (_temp, _config, pool) = mailbox();
        if !retire_during_probe {
            retire(&pool);
        }
        let mut gate = schedule::FullVerificationGate::default();
        assert!(!run_live_integrity_followups(
            &pool,
            true,
            retire_during_probe,
            &mut gate,
            Instant::now,
            || {
                assert!(
                    retire_during_probe,
                    "already retired source must not run a probe"
                );
                retire(&pool);
                true
            },
            |_| panic!("retired engine evidence cannot authorize a mutation"),
        ));
        assert!(pool.is_closed());
        assert!(gate.is_required());
    }
}
