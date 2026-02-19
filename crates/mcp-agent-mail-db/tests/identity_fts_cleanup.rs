//! Tests verifying FTS5 artifacts are fully cleaned up after v11 migrations.
//!
//! With the Search V3 decommission (br-2tnl.8.4), v11 migrations drop all
//! FTS5 tables and triggers. These tests verify the migration works correctly
//! and that `enforce_runtime_fts_cleanup` is safe on a clean database.

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use mcp_agent_mail_db::pool::{DbPool, DbPoolConfig};
use mcp_agent_mail_db::schema;
use mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;
use tempfile::tempdir;

fn count_fts_artifacts(conn: &SqliteConnection) -> i64 {
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS n FROM sqlite_master \
             WHERE (type='table' AND name LIKE 'fts_%') \
                OR (type='trigger' AND name IN (\
                    'messages_ai', 'messages_ad', 'messages_au', \
                    'agents_ai', 'agents_ad', 'agents_au', \
                    'projects_ai', 'projects_ad', 'projects_au'\
                ))",
            &[],
        )
        .expect("query FTS artifacts");
    rows.first()
        .and_then(|row| row.get_named::<i64>("n").ok())
        .unwrap_or_default()
}

#[test]
fn v11_migration_drops_all_fts_artifacts() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("v11_fts_cleanup.db");
    let conn =
        SqliteConnection::open_file(db_path.display().to_string()).expect("open fixture db");
    conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
        .expect("apply init pragmas");

    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let cx = Cx::for_testing();
    rt.block_on(async {
        schema::migrate_to_latest(&cx, &conn)
            .await
            .into_result()
            .expect("apply full migrations");
    });

    assert_eq!(
        count_fts_artifacts(&conn),
        0,
        "v11 migrations must drop all FTS tables and triggers"
    );
}

#[test]
fn base_mode_cleanup_is_safe_on_clean_db() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("base_mode_clean.db");
    let conn =
        SqliteConnection::open_file(db_path.display().to_string()).expect("open fixture db");
    conn.execute_raw(schema::PRAGMA_DB_INIT_SQL)
        .expect("apply init pragmas");

    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let cx = Cx::for_testing();
    rt.block_on(async {
        schema::migrate_to_latest(&cx, &conn)
            .await
            .into_result()
            .expect("apply full migrations");
    });

    // enforce_runtime_fts_cleanup should be safe even when FTS is already gone
    schema::enforce_runtime_fts_cleanup(&conn).expect("base mode cleanup on clean db");
    assert_eq!(count_fts_artifacts(&conn), 0);
}

#[test]
fn pool_startup_produces_clean_fts_state() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("pool_startup_clean.db");
    let db_url = format!("sqlite:///{}", db_path.display());

    let config = DbPoolConfig {
        database_url: db_url,
        ..Default::default()
    };
    let pool = DbPool::new(&config).expect("create pool");

    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let cx = Cx::for_testing();
    rt.block_on(async {
        let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
    });
    drop(pool);

    let parsed_path = config
        .sqlite_path()
        .expect("parse sqlite path from database_url");
    let conn = SqliteConnection::open_file(parsed_path).expect("reopen db");
    assert_eq!(
        count_fts_artifacts(&conn),
        0,
        "pool startup should leave no FTS artifacts"
    );
}
