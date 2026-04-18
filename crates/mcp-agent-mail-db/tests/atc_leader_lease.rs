#![allow(clippy::too_many_lines)]

//! Tests for the ATC leader lease (multi-server concurrency safety).
//!
//! Exercises acquire / renew / release / steal-on-expiry / conflict semantics
//! with REAL SQLite — no mocks.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use mcp_agent_mail_db::queries::{
    LeaseOutcome, release_atc_leader_lease, renew_atc_leader_lease,
    try_acquire_atc_leader_lease,
};
use mcp_agent_mail_db::{DbConn, DbPool, DbPoolConfig, create_pool};

fn setup_real_db(rt: &asupersync::runtime::Runtime, name: &str) -> (Cx, DbPool, tempfile::TempDir) {
    let cx = Cx::for_testing();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join(name);

    let init_conn = DbConn::open_file(db_path.display().to_string()).expect("open DB file");
    init_conn
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
        .expect("apply PRAGMAs");
    let base_sql = mcp_agent_mail_db::schema::init_schema_sql_base();
    init_conn.execute_raw(&base_sql).expect("apply base schema");

    rt.block_on(async {
        match mcp_agent_mail_db::schema::migrate_to_latest_base(&cx, &init_conn).await {
            asupersync::Outcome::Ok(applied) => {
                eprintln!("[SETUP] Applied {} migrations for {name}", applied.len());
            }
            asupersync::Outcome::Err(err) => {
                panic!("migration failed for {name}: {err}");
            }
            other => panic!("migration unexpected outcome: {other:?}"),
        }
    });
    drop(init_conn);

    let cfg = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        min_connections: 1,
        max_connections: 2,
        run_migrations: false,
        warmup_connections: 0,
        ..Default::default()
    };
    let pool = create_pool(&cfg).expect("create pool");
    (cx, pool, dir)
}

const TTL: i64 = 10_000_000; // 10s in micros

#[test]
fn acquire_on_empty_table() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_acquire_empty.db");

    rt.block_on(async {
        let result = try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("acquire");
        assert_eq!(result, LeaseOutcome::Acquired);
    });
}

#[test]
fn acquire_is_idempotent_for_same_instance() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_idempotent.db");

    rt.block_on(async {
        let r1 = try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("first acquire");
        assert_eq!(r1, LeaseOutcome::Acquired);

        let r2 = try_acquire_atc_leader_lease(&cx, &pool, "server-A", 2_000_000, TTL)
            .await
            .into_result()
            .expect("second acquire");
        assert_eq!(r2, LeaseOutcome::Acquired);
    });
}

#[test]
fn second_instance_blocked_while_lease_active() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_blocked.db");

    rt.block_on(async {
        try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("A acquires");

        let result = try_acquire_atc_leader_lease(&cx, &pool, "server-B", 2_000_000, TTL)
            .await
            .into_result()
            .expect("B attempts");

        assert!(
            matches!(result, LeaseOutcome::NotLeader { ref holder, .. } if holder == "server-A"),
            "B must be blocked: {result:?}"
        );
    });
}

#[test]
fn expired_lease_can_be_stolen() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_steal.db");

    rt.block_on(async {
        try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("A acquires");

        // Time advances past A's TTL
        let after_expiry = 1_000_000 + TTL + 1;
        let result = try_acquire_atc_leader_lease(&cx, &pool, "server-B", after_expiry, TTL)
            .await
            .into_result()
            .expect("B steals expired");

        assert_eq!(result, LeaseOutcome::Acquired);

        // A is now blocked
        let a_retry = try_acquire_atc_leader_lease(&cx, &pool, "server-A", after_expiry + 1, TTL)
            .await
            .into_result()
            .expect("A retries");
        assert!(
            matches!(a_retry, LeaseOutcome::NotLeader { ref holder, .. } if holder == "server-B"),
            "A must be blocked after B stole: {a_retry:?}"
        );
    });
}

#[test]
fn renew_succeeds_for_leader() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_renew_ok.db");

    rt.block_on(async {
        try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("acquire");

        let renewed = renew_atc_leader_lease(&cx, &pool, "server-A", 5_000_000, TTL)
            .await
            .into_result()
            .expect("renew");
        assert!(renewed, "leader must be able to renew");
    });
}

#[test]
fn renew_fails_for_non_leader() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_renew_fail.db");

    rt.block_on(async {
        try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("A acquires");

        let renewed = renew_atc_leader_lease(&cx, &pool, "server-B", 2_000_000, TTL)
            .await
            .into_result()
            .expect("B renew attempt");
        assert!(!renewed, "non-leader must not renew");
    });
}

#[test]
fn release_clears_lease() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_release.db");

    rt.block_on(async {
        try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("acquire");

        release_atc_leader_lease(&cx, &pool, "server-A")
            .await
            .into_result()
            .expect("release");

        // Now B can acquire immediately (no expiry needed)
        let result = try_acquire_atc_leader_lease(&cx, &pool, "server-B", 2_000_000, TTL)
            .await
            .into_result()
            .expect("B acquires after A released");
        assert_eq!(result, LeaseOutcome::Acquired);
    });
}

#[test]
fn release_is_noop_for_non_leader() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "lease_release_noop.db");

    rt.block_on(async {
        try_acquire_atc_leader_lease(&cx, &pool, "server-A", 1_000_000, TTL)
            .await
            .into_result()
            .expect("A acquires");

        // B trying to release A's lease — should be a no-op
        release_atc_leader_lease(&cx, &pool, "server-B")
            .await
            .into_result()
            .expect("B release attempt");

        // A should still be leader
        let result = try_acquire_atc_leader_lease(&cx, &pool, "server-A", 2_000_000, TTL)
            .await
            .into_result()
            .expect("A re-acquire");
        assert_eq!(result, LeaseOutcome::Acquired);
    });
}

#[test]
fn lease_outcome_is_leader_helper() {
    assert!(LeaseOutcome::Acquired.is_leader());
    assert!(!LeaseOutcome::NotLeader {
        holder: "x".into(),
        expires_at_micros: 0,
    }
    .is_leader());
}
