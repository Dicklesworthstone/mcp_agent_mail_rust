//! GH#353: exercise the real pool, engine, writer admission and retirement.

use super::{DbPoolConfig, WRITE_POOL_RESIDENT_CAPACITY, WRITE_POOL_RESIDENTS};
use super::{create_pool, get_cached_pool, get_or_create_pool};
use crate::write_barrier::{self, DrainOutcome};
use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

// Both admission and residency are process-global. A child isolates this test
// from unrelated libtests without mutating the multithreaded parent's env.
fn isolated() -> bool {
    const CHILD: &str = "AM_TEST_POOL_RESIDENCY_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest thread");
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let mut log = tempfile::tempfile().expect("child log");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(log.try_clone().unwrap())
        .spawn()
        .expect("spawn isolated pool workload");
    let deadline = Instant::now() + Duration::from_secs(180);
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

#[test]
fn steady_writes_reuse_one_engine_and_recovery_invalidates_resident_owners() {
    if isolated() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("archive");
    std::fs::create_dir_all(&root).unwrap();
    let config = DbPoolConfig {
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
            &temp.path().join("mail.sqlite3"),
        ),
        storage_root: Some(root),
        min_connections: 1,
        max_connections: 1,
        warmup_connections: 0,
        ..Default::default()
    };
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = Cx::for_testing();
    let generation = {
        let (barrier, drain) =
            write_barrier::acquire_promotion_barrier_draining(Duration::from_secs(2));
        assert!(!matches!(drain, DrainOutcome::TimedOut { .. }));
        let pool = create_pool(&config).unwrap();
        let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
        conn.execute_raw("CREATE TABLE gh353_writes (id INTEGER PRIMARY KEY)")
            .unwrap();
        let generation = pool.sqlite_identity_key();
        drop(conn);
        drop(pool);
        drop(barrier);
        generation
    };

    // No request or maintenance wrapper survives any iteration. Before the
    // fix this very first lookup misses and forces recovery-style bootstrap.
    for _ in 0..128 {
        let pool = get_cached_pool(&config).expect("idle request gap retained the engine");
        assert_eq!(pool.sqlite_identity_key(), generation);
        let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
        conn.query_sync("SELECT COUNT(*) FROM gh353_writes", &[])
            .unwrap();
        drop(conn);
        drop(pool);
    }

    // Keep admission continuously non-idle, as overlapping agent operations
    // do, while four real writers repeatedly acquire/drop the same live pool.
    // Maintenance must still obtain its nonblocking writer lease and read.
    let steady_activity = write_barrier::begin_write_activity();
    let start = Arc::new(Barrier::new(5));
    let mut writers = Vec::new();
    for writer in 0..4 {
        let config = config.clone();
        let generation = generation.clone();
        let start = Arc::clone(&start);
        writers.push(std::thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread().build().unwrap();
            let cx = Cx::for_testing();
            start.wait();
            for sequence in 0..64 {
                let _activity = write_barrier::begin_write_activity();
                let pool = get_cached_pool(&config).expect("steady write must stay hot");
                assert_eq!(pool.sqlite_identity_key(), generation);
                let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
                let id = writer * 64 + sequence + 1;
                conn.execute_raw(&format!("INSERT INTO gh353_writes VALUES ({id})"))
                    .unwrap();
            }
        }));
    }
    start.wait();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut passes = 0;
    while passes < 64 {
        assert!(Instant::now() < deadline, "maintenance admission starved");
        let Some(_activity) = write_barrier::try_begin_write_activity() else {
            std::thread::yield_now();
            continue;
        };
        let pool = get_cached_pool(&config).expect("maintenance shares the live engine");
        assert_eq!(pool.sqlite_identity_key(), generation);
        let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
        conn.query_sync("SELECT COUNT(*) FROM gh353_writes", &[])
            .unwrap();
        passes += 1;
    }
    for writer in writers {
        writer.join().expect("writer completed without a cold bootstrap");
    }
    assert!(write_barrier::try_acquire_promotion_barrier_if_idle().is_none());
    let (failed, drain) =
        write_barrier::acquire_promotion_barrier_draining(Duration::from_millis(2));
    assert!(matches!(drain, DrainOutcome::TimedOut { .. }));
    assert!(write_barrier::try_begin_write_activity().is_none());
    drop(failed);
    drop(steady_activity);

    let pool = get_cached_pool(&config).unwrap();
    let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
    let rows = conn
        .query_sync("SELECT COUNT(*) AS total FROM gh353_writes", &[])
        .unwrap();
    assert_eq!(rows[0].get_named::<i64>("total").unwrap(), 256);
    drop(conn);

    // A real recovery closes and unindexes all shapes. Resident strong owners
    // must not turn that explicit retirement into a cache hit on stale state.
    // A parser-only trigger on a canonically healthy file returns a terminal
    // error AFTER successful runtime retirement (the existing recovery policy).
    let (barrier, drain) =
        write_barrier::acquire_promotion_barrier_draining(Duration::from_secs(2));
    assert!(!matches!(drain, DrainOutcome::TimedOut { .. }));
    assert!(write_barrier::try_begin_write_activity().is_none());
    let recovery = pool.try_recover_from_corruption("GH353 healthy-file retirement regression");
    assert!(recovery.is_err(), "healthy parser-only recovery remains terminal");
    assert!(get_cached_pool(&config).is_none(), "retired generation was resurrected");
    assert!(runtime.block_on(pool.acquire(&cx)).into_result().is_err());
    let replacement = get_or_create_pool(&config).unwrap();
    assert_ne!(replacement.sqlite_identity_key(), generation);
    let conn = runtime
        .block_on(replacement.acquire(&cx))
        .into_result()
        .unwrap();
    let rows = conn
        .query_sync("SELECT COUNT(*) AS total FROM gh353_writes", &[])
        .unwrap();
    assert_eq!(rows[0].get_named::<i64>("total").unwrap(), 256);
    drop(conn);
    drop(barrier);

    // Churning mailbox identities bounds resident owners, while a live worker
    // holding an evicted pool keeps its original engine usable and discoverable.
    let replacement_generation = replacement.sqlite_identity_key();
    for index in 0..WRITE_POOL_RESIDENT_CAPACITY + 2 {
        let mut other = config.clone();
        other.database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(
            &temp.path().join(format!("other-{index}.sqlite3")),
        );
        drop(create_pool(&other).unwrap());
    }
    assert!(
        WRITE_POOL_RESIDENTS.get().unwrap().lock().unwrap().owners.len()
            <= WRITE_POOL_RESIDENT_CAPACITY
    );
    assert_eq!(
        get_cached_pool(&config).unwrap().sqlite_identity_key(),
        replacement_generation
    );
    let conn = runtime
        .block_on(replacement.acquire(&cx))
        .into_result()
        .expect("eviction must not close a worker's shared pool");
    drop(conn);
}

#[test]
fn anonymous_memory_pools_remain_caller_owned() {
    if isolated() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let config = DbPoolConfig {
        database_url: "sqlite:///:memory:".into(),
        storage_root: Some(temp.path().to_path_buf()),
        min_connections: 1,
        max_connections: 1,
        warmup_connections: 0,
        ..Default::default()
    };
    let pool = create_pool(&config).unwrap();
    assert!(get_cached_pool(&config).is_some());
    drop(pool);
    assert!(get_cached_pool(&config).is_none());
}
