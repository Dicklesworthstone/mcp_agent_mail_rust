//! Integration tests for pool exhaustion and recovery.
//!
//! These tests verify that the connection pool behaves correctly under
//! constrained configurations:
//! - Minimal pool (`pool_size=1`) creation and operation
//! - Multiple concurrent operations within pool capacity
//! - Sequential operations after initial setup
//! - Small pool serving requests one at a time
//! - Connection reuse after release

#![allow(
    clippy::redundant_clone,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]

mod common;

use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::pool::{DbPool, DbPoolConfig};
use mcp_agent_mail_db::queries;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Run an async closure in its own single-threaded runtime.
fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    common::block_on(f)
}

/// Create a file-backed pool with the given configuration.
fn make_pool(min: usize, max: usize) -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("pool_exhaust_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        min_connections: min,
        max_connections: max,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

/// Helper to unwrap a pool acquire Outcome, panicking on non-Ok variants.
/// Needed because `PooledConnection<DbConn>` does not implement Debug.
macro_rules! unwrap_acquire {
    ($outcome:expr, $msg:expr) => {
        match $outcome {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => panic!("{}: {e:?}", $msg),
            Outcome::Cancelled(r) => panic!("{}: cancelled: {r:?}", $msg),
            Outcome::Panicked(p) => panic!("{}: panicked: {p}", $msg),
        }
    };
}

// =============================================================================
// Test 1: Pool can be created with minimal config (pool_size=1)
// =============================================================================

#[test]
fn pool_minimal_config_single_connection() {
    let (pool, _dir) = make_pool(1, 1);

    // Acquire a single connection and perform a basic operation.
    block_on(|cx| async move {
        let conn = unwrap_acquire!(pool.acquire(&cx).await, "acquire on pool_size=1");

        // Verify the connection is usable by running a simple query.
        let rows = conn
            .query_sync("SELECT 1 AS val", &[])
            .expect("simple query should work");
        assert_eq!(rows.len(), 1, "should return exactly one row");
        drop(conn);
    });
}

#[test]
fn pool_minimal_config_ensure_project_works() {
    let (pool, _dir) = make_pool(1, 1);
    let key = format!("/tmp/pool_min_{}", unique_suffix());

    block_on(|cx| async move {
        match queries::ensure_project(&cx, &pool, &key).await {
            Outcome::Ok(p) => {
                assert!(p.id.is_some(), "project should have an id");
            }
            Outcome::Err(e) => panic!("ensure_project on pool_size=1 failed: {e:?}"),
            Outcome::Cancelled(r) => panic!("ensure_project cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("ensure_project panicked: {p}"),
        }
    });
}

// =============================================================================
// Test 2: Multiple concurrent operations work within pool capacity
// =============================================================================

#[test]
fn pool_concurrent_operations_within_capacity() {
    // Pool with 5 connections, 5 concurrent threads -- should all succeed
    // without contention.
    let (pool, _dir) = make_pool(5, 5);

    // Seed a project first.
    let human_key = format!("/tmp/pool_concurrent_{}", unique_suffix());
    {
        let pool = pool.clone();
        let key = human_key.clone();
        block_on(|cx| async move {
            let _ = queries::ensure_project(&cx, &pool, &key).await;
        });
    }

    let n_threads = 5;
    let barrier = Arc::new(Barrier::new(n_threads));
    let success_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let success = Arc::clone(&success_count);
            let key = human_key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                block_on(|cx| async move {
                    match queries::ensure_project(&cx, &pool, &key).await {
                        Outcome::Ok(_) => {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                        Outcome::Err(e) => {
                            panic!(
                                "concurrent ensure_project within capacity should succeed: {e:?}"
                            );
                        }
                        _ => {}
                    }
                });
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread should not panic");
    }

    assert_eq!(
        success_count.load(Ordering::Relaxed),
        n_threads as u64,
        "all {n_threads} threads should succeed within pool capacity"
    );
}

// =============================================================================
// Test 3: Pool handles sequential operations gracefully after initial setup
// =============================================================================

#[test]
fn pool_sequential_operations_after_setup() {
    let (pool, _dir) = make_pool(2, 4);

    let key = format!("/tmp/pool_sequential_{}", unique_suffix());

    // Perform several sequential operations: ensure_project, register_agent, etc.
    // The pool should handle repeated acquire/release cycles without issues.
    let project_id = {
        let pool = pool.clone();
        let key = key.clone();
        block_on(|cx| async move {
            match queries::ensure_project(&cx, &pool, &key).await {
                Outcome::Ok(p) => p.id.unwrap(),
                Outcome::Err(e) => panic!("ensure_project failed: {e:?}"),
                Outcome::Cancelled(r) => panic!("ensure_project cancelled: {r:?}"),
                Outcome::Panicked(p) => panic!("ensure_project panicked: {p}"),
            }
        })
    };

    // Register an agent.
    let agent_id = {
        let pool = pool.clone();
        block_on(|cx| async move {
            match queries::register_agent(
                &cx,
                &pool,
                project_id,
                "RedLake",
                "test-program",
                "test-model",
                Some("sequential test"),
                None,
                None,
            )
            .await
            {
                Outcome::Ok(a) => a.id.unwrap(),
                Outcome::Err(e) => panic!("register_agent failed: {e:?}"),
                Outcome::Cancelled(r) => panic!("register_agent cancelled: {r:?}"),
                Outcome::Panicked(p) => panic!("register_agent panicked: {p}"),
            }
        })
    };

    // Call ensure_project again (idempotent).
    let project_id_2 = {
        let pool = pool.clone();
        let key = key.clone();
        block_on(|cx| async move {
            match queries::ensure_project(&cx, &pool, &key).await {
                Outcome::Ok(p) => p.id.unwrap(),
                Outcome::Err(e) => panic!("second ensure_project failed: {e:?}"),
                Outcome::Cancelled(r) => panic!("second ensure_project cancelled: {r:?}"),
                Outcome::Panicked(p) => panic!("second ensure_project panicked: {p}"),
            }
        })
    };

    assert_eq!(
        project_id, project_id_2,
        "idempotent ensure_project should return same id"
    );

    // Register another agent on the same project.
    let agent_id_2 = {
        let pool = pool.clone();
        block_on(|cx| async move {
            match queries::register_agent(
                &cx,
                &pool,
                project_id,
                "BluePeak",
                "test-program",
                "test-model",
                Some("another agent"),
                None,
                None,
            )
            .await
            {
                Outcome::Ok(a) => a.id.unwrap(),
                Outcome::Err(e) => panic!("register_agent 2 failed: {e:?}"),
                Outcome::Cancelled(r) => panic!("register_agent 2 cancelled: {r:?}"),
                Outcome::Panicked(p) => panic!("register_agent 2 panicked: {p}"),
            }
        })
    };

    assert_ne!(
        agent_id, agent_id_2,
        "different agents should have different ids"
    );
}

// =============================================================================
// Test 4: Pool with small size still serves requests one at a time
// =============================================================================

#[test]
fn pool_small_size_serializes_requests() {
    // Pool with exactly 1 connection. Multiple requests should queue and
    // succeed sequentially (not deadlock).
    let (pool, _dir) = make_pool(1, 1);

    let key = format!("/tmp/pool_serial_{}", unique_suffix());

    // Perform 10 sequential operations through a pool_size=1 pool.
    for i in 0..10 {
        let pool = pool.clone();
        let key = key.clone();
        block_on(|cx| async move {
            match queries::ensure_project(&cx, &pool, &key).await {
                Outcome::Ok(_) => {}
                Outcome::Err(e) => {
                    panic!("sequential request {i} on pool_size=1 failed: {e:?}");
                }
                _ => panic!("unexpected outcome on request {i}"),
            }
        });
    }
}

#[test]
fn pool_small_size_concurrent_waiters_succeed() {
    // Pool with max_connections=2, but 6 threads competing. Threads should
    // queue on the pool and eventually all succeed (within the 30s timeout).
    let (pool, _dir) = make_pool(1, 2);

    // Seed a project.
    let human_key = format!("/tmp/pool_wait_{}", unique_suffix());
    {
        let pool = pool.clone();
        let key = human_key.clone();
        block_on(|cx| async move {
            let _ = queries::ensure_project(&cx, &pool, &key).await;
        });
    }

    let n_threads = 6;
    let barrier = Arc::new(Barrier::new(n_threads));
    let success_count = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..n_threads)
        .map(|_| {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            let success = Arc::clone(&success_count);
            let key = human_key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                block_on(|cx| async move {
                    match queries::ensure_project(&cx, &pool, &key).await {
                        Outcome::Ok(_) => {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                        Outcome::Err(e) => {
                            panic!("queued request on small pool should eventually succeed: {e:?}");
                        }
                        _ => {}
                    }
                });
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread should not panic");
    }

    assert_eq!(
        success_count.load(Ordering::Relaxed),
        n_threads as u64,
        "all {n_threads} threads should succeed despite small pool (queuing)"
    );
}

// =============================================================================
// Test 5: Connection reuse after release
// =============================================================================

#[test]
fn pool_connection_reuse_after_release() {
    let (pool, _dir) = make_pool(1, 2);

    // Acquire a connection, use it, release it, then acquire again.
    // The pool should reuse the released connection.
    block_on(|cx| async move {
        // First acquire.
        let conn = unwrap_acquire!(pool.acquire(&cx).await, "first acquire");

        // Use the connection.
        conn.execute_raw("SELECT 1").expect("query should work");

        // Release the connection back to the pool.
        drop(conn);

        // Second acquire -- should get a recycled connection (not create new).
        let conn2 = unwrap_acquire!(pool.acquire(&cx).await, "second acquire (reuse)");

        // Verify it still works.
        conn2
            .execute_raw("SELECT 2")
            .expect("reused connection should work");
        drop(conn2);
    });
}

#[test]
fn pool_connection_reuse_across_many_cycles() {
    // Verify that connections can be acquired and released many times without
    // leaking or exhausting the pool.
    let (pool, _dir) = make_pool(1, 1);

    let key = format!("/tmp/pool_reuse_cycle_{}", unique_suffix());

    // Create project once.
    {
        let pool = pool.clone();
        let key = key.clone();
        block_on(|cx| async move {
            let _ = queries::ensure_project(&cx, &pool, &key).await;
        });
    }

    // Acquire and release 50 times through the same pool_size=1 pool.
    for i in 0..50 {
        let pool = pool.clone();
        let key = key.clone();
        block_on(|cx| async move {
            match queries::ensure_project(&cx, &pool, &key).await {
                Outcome::Ok(_) => {}
                Outcome::Err(e) => panic!("cycle {i}: ensure_project failed: {e:?}"),
                _ => panic!("cycle {i}: unexpected outcome"),
            }
        });
    }
}

#[test]
fn pool_stats_reflect_usage() {
    let (pool, _dir) = make_pool(1, 3);

    block_on(|cx| async move {
        // Before any acquisition, pool should have 0 total connections.
        // (Connections are created lazily on first acquire.)
        pool.sample_pool_stats_now();

        // Acquire a connection to trigger creation.
        let conn = unwrap_acquire!(pool.acquire(&cx).await, "acquire for stats");

        // Sample stats with connection held.
        pool.sample_pool_stats_now();

        // The connection is usable.
        conn.execute_raw("SELECT 42").expect("query should work");

        // Release connection.
        drop(conn);

        // After release, acquire again to verify pool is still healthy.
        let conn2 = unwrap_acquire!(pool.acquire(&cx).await, "acquire after release");
        conn2
            .execute_raw("SELECT 43")
            .expect("second query should work");
        drop(conn2);
    });
}

// =============================================================================
// Test: Pool acquisitions are recorded in the global DB metrics surface
// (bead K5 — `am robot metrics` reads `global_metrics().db.snapshot()`).
// =============================================================================

#[test]
fn pool_acquire_records_global_db_metrics() {
    // `global_metrics()` is a process-wide singleton shared across parallel
    // tests, so assert only monotonic lower bounds (counters never decrease),
    // never absolute values.
    let before = mcp_agent_mail_core::global_metrics().db.snapshot();

    // A tiny pool that forces queuing: 1 connection, several sequential acquires.
    let (pool, _dir) = make_pool(1, 1);
    block_on(|cx| async move {
        for _ in 0..3 {
            let conn = unwrap_acquire!(pool.acquire(&cx).await, "acquire for metrics");
            conn.execute_raw("SELECT 1").expect("query should work");
            drop(conn);
        }
        // Refresh the pool gauges into the global metrics surface.
        pool.sample_pool_stats_now();
    });

    let after = mcp_agent_mail_core::global_metrics().db.snapshot();

    // We performed at least one acquire between the two reads, and acquire
    // counters are monotonic, so the total must have advanced.
    assert!(
        after.pool_acquires_total > before.pool_acquires_total,
        "pool acquires must be counted: before={} after={}",
        before.pool_acquires_total,
        after.pool_acquires_total
    );
    // The acquire-latency histogram must have observed at least our acquires.
    assert!(
        after.pool_acquire_latency_us.count > before.pool_acquire_latency_us.count,
        "pool acquire latency histogram must record samples"
    );
    // Utilization is always a valid 0..=100 percentage.
    assert!(
        after.pool_utilization_pct <= 100,
        "pool utilization must be a clamped percentage, got {}",
        after.pool_utilization_pct
    );
}

// =============================================================================
// Test: Periodic maintenance ops (bead K4) run on a real file-backed pool.
// =============================================================================

#[test]
fn maintenance_ops_run_on_file_pool() {
    let (pool, _dir) = make_pool(1, 2);

    // Materialize the DB file + schema and write some rows so VACUUM/ANALYZE
    // have real content to operate on.
    let setup_pool = pool.clone();
    block_on(|cx| async move {
        let conn = unwrap_acquire!(setup_pool.acquire(&cx).await, "acquire to materialize db");
        conn.execute_raw("CREATE TABLE IF NOT EXISTS k4_probe (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create probe table");
        conn.execute_raw("INSERT INTO k4_probe (v) VALUES ('alpha'), ('beta'), ('gamma')")
            .expect("insert probe rows");
        drop(conn);
    });

    // Each K4 maintenance op must succeed against a real file-backed database.
    pool.set_journal_size_limit(64 * 1024 * 1024)
        .expect("set_journal_size_limit should succeed");
    pool.wal_checkpoint_passive()
        .expect("passive checkpoint should succeed");
    pool.analyze().expect("analyze should succeed");
    pool.vacuum().expect("vacuum should succeed");

    // Ops are idempotent: a second pass also succeeds.
    pool.analyze().expect("repeat analyze should succeed");
    pool.vacuum().expect("repeat vacuum should succeed");
}

// =============================================================================
// Test: Verified last-known-healthy snapshot (bead K2) via the pool wrapper.
// =============================================================================

#[test]
fn create_verified_snapshot_records_metadata() {
    let (pool, _dir) = make_pool(1, 2);

    // Materialize + migrate the DB so the full integrity check has real content.
    let setup_pool = pool.clone();
    block_on(|cx| async move {
        let key = format!("/tmp/k2_snap_{}", unique_suffix());
        let _ = queries::ensure_project(&cx, &setup_pool, &key).await;
    });

    let before = mcp_agent_mail_core::global_metrics()
        .db
        .snapshot()
        .snapshot_created_total;

    let meta = pool
        .create_verified_snapshot()
        .expect("snapshot op should succeed")
        .expect("a healthy DB must produce a verified snapshot");
    assert!(
        meta.integrity_verified,
        "snapshot must be integrity-verified"
    );
    assert_eq!(meta.integrity_kind, "integrity_check");
    assert!(meta.created_us > 0);

    // The .bak + metadata sidecar exist next to the primary, and the snapshot is
    // discoverable as the latest verified one.
    let primary = std::path::Path::new(pool.sqlite_path());
    assert!(
        mcp_agent_mail_db::snapshot::snapshot_bak_path(primary).is_file(),
        "verified snapshot .bak must exist"
    );
    assert!(
        mcp_agent_mail_db::snapshot::snapshot_meta_path(primary).is_file(),
        "verified snapshot metadata sidecar must exist"
    );
    assert!(
        mcp_agent_mail_db::snapshot::latest_verified_snapshot(primary).is_some(),
        "the recorded snapshot must be discoverable as latest-verified"
    );

    let after = mcp_agent_mail_core::global_metrics()
        .db
        .snapshot()
        .snapshot_created_total;
    assert!(
        after > before,
        "snapshot_created_total must advance: before={before} after={after}"
    );
}

#[test]
fn maintenance_ops_noop_on_memory_pool() {
    // :memory: databases have no file to checkpoint/vacuum; the ops must be
    // graceful no-ops rather than errors (bead K4).
    let config = DbPoolConfig {
        database_url: "sqlite:///:memory:".to_string(),
        storage_root: None,
        min_connections: 1,
        max_connections: 1,
        acquire_timeout_ms: 5_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create in-memory pool");
    pool.set_journal_size_limit(1024)
        .expect("memory journal_size_limit no-op");
    pool.analyze().expect("memory analyze no-op");
    pool.vacuum().expect("memory vacuum no-op");
}

// =============================================================================
// Test: Warmup with pool_size=1 opens exactly 1 connection
// =============================================================================

#[test]
fn pool_warmup_single_connection() {
    let (pool, _dir) = make_pool(1, 1);

    block_on(|cx| async move {
        let opened = pool
            .warmup(&cx, 1, std::time::Duration::from_secs(10))
            .await;
        assert_eq!(opened, 1, "warmup should open exactly 1 connection");

        // After warmup, a regular acquire should succeed (reusing the warmed connection).
        let conn = unwrap_acquire!(pool.acquire(&cx).await, "acquire after warmup");
        conn.execute_raw("SELECT 1")
            .expect("query should work after warmup");
        drop(conn);
    });
}

// =============================================================================
// Test: Integrity checks on pool with minimal config
// =============================================================================

#[test]
fn pool_integrity_check_on_fresh_db() {
    let (pool, _dir) = make_pool(1, 1);

    // Acquire once to create the database file and run migrations.
    let pool2 = pool.clone();
    block_on(|cx| async move {
        let conn = unwrap_acquire!(pool2.acquire(&cx).await, "initial acquire for integrity");
        drop(conn);
    });

    // Run startup integrity check.
    let result = pool
        .run_startup_integrity_check()
        .expect("integrity check should succeed on fresh db");
    assert!(result.ok, "fresh database should pass integrity check");
}

// =============================================================================
// Test: In-memory pool works with pool_size=1
// =============================================================================

#[test]
fn pool_memory_minimal_config() {
    let config = DbPoolConfig {
        database_url: "sqlite:///:memory:".to_string(),
        storage_root: None,
        min_connections: 1,
        max_connections: 1,
        acquire_timeout_ms: 5_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create in-memory pool");

    block_on(|cx| async move {
        let conn = unwrap_acquire!(pool.acquire(&cx).await, "in-memory acquire");

        // Verify the connection is functional.
        let rows = conn
            .query_sync("SELECT sqlite_version() AS ver", &[])
            .expect("version query should work");
        assert_eq!(rows.len(), 1, "should return one row");
        drop(conn);
    });
}

// =============================================================================
// Test: WAL checkpoint on minimal pool
// =============================================================================

#[test]
fn pool_wal_checkpoint_minimal() {
    let (pool, _dir) = make_pool(1, 1);

    // Acquire to create DB and run migrations.
    let pool2 = pool.clone();
    block_on(|cx| async move {
        let conn = unwrap_acquire!(pool2.acquire(&cx).await, "acquire for checkpoint test");
        // Write some data.
        conn.execute_raw("CREATE TABLE IF NOT EXISTS wal_test (id INTEGER PRIMARY KEY, data TEXT)")
            .expect("create table");
        conn.execute_raw("INSERT INTO wal_test VALUES (1, 'hello')")
            .expect("insert");
        drop(conn);
    });

    // Checkpoint should succeed.
    let frames = pool
        .wal_checkpoint()
        .expect("wal checkpoint on minimal pool should succeed");
    assert!(
        frames <= 1000,
        "reasonable frame count after minimal writes: {frames}"
    );
}
