//! GH#353: exercise the real pool, engine, writer admission and retirement.

use super::{DbPoolConfig, WRITE_POOL_RESIDENT_CAPACITY, WRITE_POOL_RESIDENTS};
use super::{
    RUNTIME_POOL_CONSTRUCTIONS, RUNTIME_POOL_INITIALIZERS, RuntimePoolInitialization,
    create_runtime_pool, get_or_reuse_compatible_memory_pool, resolve_mailbox_sqlite_path,
};
use super::{create_pool, get_cached_pool, get_or_create_pool};
use crate::DbPool;
use crate::error::{DbError, DbResult};
use crate::write_barrier::{self, DrainOutcome};
use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::sync::atomic::Ordering;
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
        writer
            .join()
            .expect("writer completed without a cold bootstrap");
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
    assert!(
        recovery.is_err(),
        "healthy parser-only recovery remains terminal"
    );
    assert!(
        get_cached_pool(&config).is_none(),
        "retired generation was resurrected"
    );
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
        WRITE_POOL_RESIDENTS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .owners
            .len()
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

fn factory_config(temp: &tempfile::TempDir) -> DbPoolConfig {
    let root = temp.path().join("archive");
    std::fs::create_dir_all(&root).unwrap();
    DbPoolConfig {
        database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
            &temp.path().join("factory.sqlite3"),
        ),
        storage_root: Some(root),
        min_connections: 1,
        max_connections: 1,
        warmup_connections: 0,
        ..Default::default()
    }
}

fn runtime_factory(index: usize) -> fn(&DbPoolConfig) -> DbResult<DbPool> {
    match index % 3 {
        0 => create_pool,
        1 => get_or_create_pool,
        _ => get_or_reuse_compatible_memory_pool,
    }
}

fn hold_initializer(config: &DbPoolConfig) -> RuntimePoolInitialization<'static> {
    let identity = resolve_mailbox_sqlite_path(&config.database_url)
        .unwrap()
        .canonical_path;
    let registry = RUNTIME_POOL_INITIALIZERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    RuntimePoolInitialization::try_begin(registry, identity).unwrap()
}

#[test]
fn cold_factories_refuse_aliases_and_other_shapes_before_opening_an_engine() {
    if isolated() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let config = factory_config(&temp);
    let mut alias = config.clone();
    alias.database_url = mcp_agent_mail_core::disk::sqlite_url_from_path(
        &temp.path().join(".").join("factory.sqlite3"),
    );
    let mut other_shape = config.clone();
    other_shape.max_connections = 3;
    let owner = hold_initializer(&config);
    let before = RUNTIME_POOL_CONSTRUCTIONS.load(Ordering::Relaxed);
    let activity = write_barrier::begin_write_activity();
    for selected in [&config, &alias, &other_shape] {
        for factory in 0..3 {
            let result = runtime_factory(factory)(selected);
            assert!(matches!(result, Err(DbError::ResourceBusy(_))));
        }
    }
    assert_eq!(RUNTIME_POOL_CONSTRUCTIONS.load(Ordering::Relaxed), before);
    assert!(!temp.path().join("factory.sqlite3").exists());
    assert_eq!(write_barrier::active_writer_count(), 1);
    // Initialization ownership is not promotion exclusion or permission:
    // maintenance can enter, and a live writer still prevents promotion.
    assert!(write_barrier::try_begin_write_activity().is_some());
    assert!(write_barrier::try_acquire_promotion_barrier_if_idle().is_none());
    drop(activity);

    let mut independent = config.clone();
    independent.database_url =
        mcp_agent_mail_core::disk::sqlite_url_from_path(&temp.path().join("independent.sqlite3"));
    drop(create_pool(&independent).expect("another mailbox must remain available"));
    assert!(!temp.path().join("factory.sqlite3").exists());
    drop(owner);
    drop(create_pool(&config).expect("released admission must permit retry"));
    assert!(
        RUNTIME_POOL_INITIALIZERS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn factory_errors_and_unwinds_release_admission_before_a_real_retry() {
    if isolated() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let config = factory_config(&temp);
    // Inject at the construction boundary, after the real authority lookup
    // and permit acquisition, not in a replacement admission implementation.
    let failed = create_runtime_pool(&config, |_| {
        Err(DbError::Pool("injected factory failure".into()))
    });
    assert!(matches!(failed, Err(DbError::Pool(_))));
    assert!(
        RUNTIME_POOL_INITIALIZERS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty()
    );
    let unwound = std::panic::catch_unwind(|| {
        let _ = create_runtime_pool(&config, |_| panic!("injected factory unwind"));
    });
    assert!(unwound.is_err());
    assert!(
        RUNTIME_POOL_INITIALIZERS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty()
    );
    assert!(!temp.path().join("factory.sqlite3").exists());
    let pool = create_pool(&config).expect("a failed initializer stranded its mailbox");
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = Cx::for_testing();
    let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
    conn.execute_raw("CREATE TABLE gh353_retry (id INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute_raw("INSERT INTO gh353_retry VALUES (1)")
        .unwrap();
    assert_eq!(
        conn.query_sync("SELECT id FROM gh353_retry", &[])
            .unwrap()
            .len(),
        1
    );
}

const FACTORY_WRITERS: usize = 4;
const FACTORY_WRITES: usize = 32;
const FACTORY_BODY_BYTES: usize = 16 * 1024;

fn assert_factory_payloads(pool: &DbPool) {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = Cx::for_testing();
    let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
    let rows = conn
        .query_sync("SELECT id, body FROM gh353_factory_load ORDER BY id", &[])
        .unwrap();
    assert_eq!(rows.len(), FACTORY_WRITERS * FACTORY_WRITES);
    let expected = "x".repeat(FACTORY_BODY_BYTES);
    for (offset, row) in rows.iter().enumerate() {
        assert_eq!(
            row.get_named::<i64>("id").unwrap(),
            i64::try_from(offset + 1).unwrap()
        );
        assert_eq!(row.get_named::<String>("body").unwrap(), expected);
    }
}

#[test]
fn mixed_cold_factories_and_steady_writes_keep_one_engine_until_recovery() {
    if isolated() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let config = factory_config(&temp);
    let before = RUNTIME_POOL_CONSTRUCTIONS.load(Ordering::Relaxed);
    let start = Arc::new(Barrier::new(8));
    let pools = std::thread::scope(|scope| {
        // Collecting spawns all eight workers before the first join; joining
        // lazily would leave the first worker waiting on the barrier forever.
        #[allow(clippy::needless_collect)]
        let tasks = (0..8)
            .map(|worker| {
                let config = config.clone();
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    let deadline = Instant::now() + Duration::from_secs(60);
                    loop {
                        match runtime_factory(worker)(&config) {
                            Ok(pool) => return pool,
                            Err(DbError::ResourceBusy(_)) => {
                                assert!(Instant::now() < deadline, "cold factory starved");
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("cold factory failed: {error}"),
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    let engine = pools[0].engine_identity_key();
    assert!(
        pools
            .iter()
            .all(|pool| pool.engine_identity_key() == engine)
    );
    // Equal returned identities alone would miss redundant, losing engine
    // constructions. Count actual calls into the original factories too.
    assert_eq!(
        RUNTIME_POOL_CONSTRUCTIONS.load(Ordering::Relaxed) - before,
        1
    );
    {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = Cx::for_testing();
        let conn = runtime
            .block_on(pools[0].acquire(&cx))
            .into_result()
            .unwrap();
        conn.execute_raw(
            "CREATE TABLE gh353_factory_load (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
        )
        .unwrap();
    }
    drop(pools);

    // No original caller survives. Hold admission as another cold pool shape
    // would; ALL hot factory APIs must bypass it throughout the real workload.
    let owner = hold_initializer(&config);
    let steady_activity = write_barrier::begin_write_activity();
    let start = Arc::new(Barrier::new(FACTORY_WRITERS + 1));
    std::thread::scope(|scope| {
        let tasks = (0..FACTORY_WRITERS)
            .map(|writer| {
                let config = config.clone();
                let engine = engine.clone();
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    let runtime = RuntimeBuilder::current_thread().build().unwrap();
                    let cx = Cx::for_testing();
                    let payload = "x".repeat(FACTORY_BODY_BYTES);
                    start.wait();
                    for sequence in 0..FACTORY_WRITES {
                        let _activity = write_barrier::begin_write_activity();
                        let pool = runtime_factory(writer + sequence)(&config).unwrap();
                        assert_eq!(pool.engine_identity_key(), engine);
                        let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
                        let id = writer * FACTORY_WRITES + sequence + 1;
                        conn.execute_raw(&format!(
                            "INSERT INTO gh353_factory_load VALUES ({id}, '{payload}')"
                        ))
                        .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = Cx::for_testing();
        start.wait();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut passes = 0;
        while passes < 32 {
            assert!(Instant::now() < deadline, "maintenance admission starved");
            let Some(_activity) = write_barrier::try_begin_write_activity() else {
                std::thread::yield_now();
                continue;
            };
            let pool = runtime_factory(passes)(&config).unwrap();
            assert_eq!(pool.engine_identity_key(), engine);
            let conn = runtime.block_on(pool.acquire(&cx)).into_result().unwrap();
            conn.query_sync("SELECT COUNT(*) FROM gh353_factory_load", &[])
                .unwrap();
            passes += 1;
        }
        for task in tasks {
            task.join().unwrap();
        }
    });
    assert_eq!(
        RUNTIME_POOL_CONSTRUCTIONS.load(Ordering::Relaxed) - before,
        1
    );
    let pool = get_cached_pool(&config).unwrap();
    assert_factory_payloads(&pool);
    let (failed, drain) =
        write_barrier::acquire_promotion_barrier_draining(Duration::from_millis(2));
    assert!(matches!(drain, DrainOutcome::TimedOut { .. }));
    assert!(write_barrier::try_begin_write_activity().is_none());
    drop(failed);
    drop(steady_activity);
    drop(owner);

    let (promotion, drain) =
        write_barrier::acquire_promotion_barrier_draining(Duration::from_secs(2));
    assert!(!matches!(drain, DrainOutcome::TimedOut { .. }));
    assert!(
        pool.try_recover_from_corruption("GH353 factory retirement regression")
            .is_err()
    );
    assert!(get_cached_pool(&config).is_none());
    let replacement = create_pool(&config).unwrap();
    assert_ne!(replacement.engine_identity_key(), engine);
    assert_factory_payloads(&replacement);
    assert_eq!(
        RUNTIME_POOL_CONSTRUCTIONS.load(Ordering::Relaxed) - before,
        2
    );
    assert!(write_barrier::try_begin_write_activity().is_none());
    drop(promotion);
    assert!(
        RUNTIME_POOL_INITIALIZERS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn memory_compatibility_selection_bypasses_file_initialization_admission() {
    if isolated() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let mut config = factory_config(&temp);
    config.database_url = "sqlite:///:memory:".into();
    let registry = RUNTIME_POOL_INITIALIZERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let owner = RuntimePoolInitialization::try_begin(registry, ":memory:".into()).unwrap();
    let pool = create_pool(&config).unwrap();
    let mut compatible = config.clone();
    compatible.max_connections = 2;
    let reused = get_or_reuse_compatible_memory_pool(&compatible).unwrap();
    assert_eq!(pool.engine_identity_key(), reused.engine_identity_key());
    drop(pool);
    drop(reused);
    assert!(
        get_cached_pool(&config).is_none(),
        "memory session was pinned"
    );
    drop(owner);
    assert!(registry.lock().unwrap().is_empty());
}
