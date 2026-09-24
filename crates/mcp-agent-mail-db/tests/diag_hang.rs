//! Diagnostic tests for br-2em1l: verify the spin-loop `block_on` fix.
//!
//! These tests confirm that `pool.acquire()` works correctly when driven by
//! the `common::block_on` spin-loop executor, fixing the hang caused by the
//! runtime's `thread::park()` mechanism.

use crate::common;

use asupersync::Outcome;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn make_pool_no_migrations() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir
        .path()
        .join(format!("diag_nomig_{}.db", unique_suffix()));

    // Pre-init schema synchronously.
    let init_conn =
        mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open");
    init_conn
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
        .expect("pragmas");
    let init_sql = mcp_agent_mail_db::schema::init_schema_sql_base();
    init_conn.execute_raw(&init_sql).expect("schema");
    drop(init_conn);

    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 5_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: false,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

fn make_pool_with_migrations() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join(format!("diag_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        storage_root: Some(db_path.parent().unwrap().join("storage")),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 5_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

#[test]
fn pool_acquire_no_migrations() {
    let (pool, _dir) = make_pool_no_migrations();
    common::block_on(|cx| async move {
        match pool.acquire(&cx).await {
            Outcome::Ok(conn) => drop(conn),
            Outcome::Err(e) => panic!("pool acquire error: {e:?}"),
            Outcome::Cancelled(r) => panic!("pool acquire cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("pool acquire panicked: {p:?}"),
        }
    });
}

#[test]
fn pool_acquire_with_migrations() {
    let (pool, _dir) = make_pool_with_migrations();
    common::block_on(|cx| async move {
        match pool.acquire(&cx).await {
            Outcome::Ok(conn) => drop(conn),
            Outcome::Err(e) => panic!("pool acquire error: {e:?}"),
            Outcome::Cancelled(r) => panic!("pool acquire cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("pool acquire panicked: {p:?}"),
        }
    });
}

#[test]
fn pool_acquire_then_query() {
    let (pool, _dir) = make_pool_no_migrations();
    common::block_on(|cx| async move {
        let conn = match pool.acquire(&cx).await {
            Outcome::Ok(c) => c,
            Outcome::Err(e) => panic!("pool acquire failed: {e:?}"),
            Outcome::Cancelled(r) => panic!("pool acquire cancelled: {r:?}"),
            Outcome::Panicked(p) => panic!("pool acquire panicked: {p:?}"),
        };
        // Run a simple query to verify the connection works.
        let rows = conn
            .query_sync("SELECT 1 AS val", &[])
            .expect("simple query");
        assert_eq!(rows.len(), 1);
        drop(conn);
    });
}
