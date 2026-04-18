#![allow(clippy::too_many_lines)]

//! Tests for ATC rollup snapshot / restore lifecycle.
//!
//! Exercises snapshot capture, JSON round-trip, restore upsert, and
//! idempotent restore semantics with REAL SQLite — no mocks.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use mcp_agent_mail_db::queries::{restore_atc_rollups, snapshot_atc_rollups};
use mcp_agent_mail_db::{DbConn, DbPool, DbPoolConfig, create_pool};
use sqlmodel_core::Value;

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

fn insert_rollup(pool: &DbPool, stratum_key: &str, total: i64, correct: i64) {
    let conn = DbConn::open_file(pool.sqlite_path()).expect("open for insert");
    conn.execute_sync(
        "INSERT INTO atc_experience_rollups \
         (stratum_key, subsystem, effect_kind, risk_tier, \
          total_count, resolved_count, censored_count, expired_count, \
          correct_count, incorrect_count, total_regret, total_loss, \
          ewma_loss, ewma_weight, delay_sum_micros, delay_count, delay_max_micros, \
          last_updated_ts) \
         VALUES (?, 'test', 'send', 0, ?, 0, 0, 0, ?, 0, 0.0, 0.0, 0.0, 0.0, 0, 0, 0, 1000000)",
        &[
            Value::Text(stratum_key.to_string()),
            Value::BigInt(total),
            Value::BigInt(correct),
        ],
    )
    .expect("insert rollup");
}

#[test]
fn snapshot_empty_table_returns_zero_rows() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_empty.db");

    rt.block_on(async {
        let snap = snapshot_atc_rollups(&cx, &pool, 1_000_000)
            .await
            .into_result()
            .expect("snapshot");
        assert_eq!(snap.rollup_rows, 0);
        assert!(!snap.payload_sha256.is_empty());
        assert_eq!(snap.captured_ts_micros, 1_000_000);
    });
}

#[test]
fn snapshot_captures_all_rows() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_rows.db");

    insert_rollup(&pool, "stratum-a", 10, 8);
    insert_rollup(&pool, "stratum-b", 20, 15);

    rt.block_on(async {
        let snap = snapshot_atc_rollups(&cx, &pool, 2_000_000)
            .await
            .into_result()
            .expect("snapshot");
        assert_eq!(snap.rollup_rows, 2);
        assert!(snap.payload.contains("stratum-a"));
        assert!(snap.payload.contains("stratum-b"));
    });
}

#[test]
fn restore_round_trips_through_snapshot() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_roundtrip.db");

    insert_rollup(&pool, "stratum-x", 100, 90);
    insert_rollup(&pool, "stratum-y", 200, 180);

    rt.block_on(async {
        let snap = snapshot_atc_rollups(&cx, &pool, 3_000_000)
            .await
            .into_result()
            .expect("snapshot");

        // Clear all rollups
        let conn = DbConn::open_file(pool.sqlite_path()).expect("open");
        conn.execute_sync("DELETE FROM atc_experience_rollups", &[])
            .expect("clear");
        drop(conn);

        // Restore from snapshot
        let restored = restore_atc_rollups(&cx, &pool, &snap.payload, 4_000_000)
            .await
            .into_result()
            .expect("restore");
        assert_eq!(restored, 2);

        // Verify data
        let conn = DbConn::open_file(pool.sqlite_path()).expect("open");
        let rows = conn
            .query_sync(
                "SELECT stratum_key, total_count, correct_count \
                 FROM atc_experience_rollups ORDER BY stratum_key",
                &[],
            )
            .expect("query");
        assert_eq!(rows.len(), 2);
        let key_x = rows[0].get_named::<String>("stratum_key").unwrap();
        let total_x = rows[0].get_named::<i64>("total_count").unwrap();
        let key_y = rows[1].get_named::<String>("stratum_key").unwrap();
        let total_y = rows[1].get_named::<i64>("total_count").unwrap();
        assert_eq!(key_x, "stratum-x");
        assert_eq!(total_x, 100);
        assert_eq!(key_y, "stratum-y");
        assert_eq!(total_y, 200);
    });
}

#[test]
fn restore_is_idempotent() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_idempotent.db");

    insert_rollup(&pool, "stratum-z", 50, 40);

    rt.block_on(async {
        let snap = snapshot_atc_rollups(&cx, &pool, 5_000_000)
            .await
            .into_result()
            .expect("snapshot");

        // Clear and restore twice
        let conn = DbConn::open_file(pool.sqlite_path()).expect("open");
        conn.execute_sync("DELETE FROM atc_experience_rollups", &[])
            .expect("clear");
        drop(conn);

        let r1 = restore_atc_rollups(&cx, &pool, &snap.payload, 6_000_000)
            .await
            .into_result()
            .expect("restore 1");
        let r2 = restore_atc_rollups(&cx, &pool, &snap.payload, 7_000_000)
            .await
            .into_result()
            .expect("restore 2");
        assert_eq!(r1, 1);
        assert_eq!(r2, 1);

        // Still only 1 row
        let conn = DbConn::open_file(pool.sqlite_path()).expect("open");
        let rows = conn
            .query_sync("SELECT stratum_key FROM atc_experience_rollups", &[])
            .expect("query");
        assert_eq!(rows.len(), 1);
    });
}

#[test]
fn snapshot_records_metadata_in_snapshots_table() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_meta.db");

    insert_rollup(&pool, "key-1", 5, 3);

    rt.block_on(async {
        let snap = snapshot_atc_rollups(&cx, &pool, 8_000_000)
            .await
            .into_result()
            .expect("snapshot");

        let conn = DbConn::open_file(pool.sqlite_path()).expect("open");
        let rows = conn
            .query_sync(
                "SELECT captured_ts, rollup_rows, payload_sha256 \
                 FROM atc_rollup_snapshots ORDER BY snapshot_id DESC LIMIT 1",
                &[],
            )
            .expect("query snapshots");
        assert_eq!(rows.len(), 1);
        let captured_ts = rows[0].get_named::<i64>("captured_ts").unwrap();
        let rollup_rows = rows[0].get_named::<i64>("rollup_rows").unwrap();
        let sha = rows[0].get_named::<String>("payload_sha256").unwrap();
        assert_eq!(captured_ts, 8_000_000);
        assert_eq!(rollup_rows, 1);
        assert_eq!(sha, snap.payload_sha256);
    });
}

#[test]
fn sha256_is_deterministic() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_sha.db");

    insert_rollup(&pool, "determ-key", 42, 30);

    rt.block_on(async {
        let snap1 = snapshot_atc_rollups(&cx, &pool, 9_000_000)
            .await
            .into_result()
            .expect("snapshot 1");
        let snap2 = snapshot_atc_rollups(&cx, &pool, 10_000_000)
            .await
            .into_result()
            .expect("snapshot 2");
        // Same rollup data => same payload => same SHA256
        assert_eq!(snap1.payload_sha256, snap2.payload_sha256);
        assert_eq!(snap1.payload, snap2.payload);
    });
}

#[test]
fn restore_rejects_invalid_json() {
    let rt = RuntimeBuilder::current_thread().build().expect("runtime");
    let (cx, pool, _dir) = setup_real_db(&rt, "snap_bad_json.db");

    rt.block_on(async {
        let result = restore_atc_rollups(&cx, &pool, "not-valid-json", 11_000_000)
            .await
            .into_result();
        assert!(result.is_err(), "should reject invalid JSON");
    });
}
