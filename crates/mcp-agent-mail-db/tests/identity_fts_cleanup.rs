use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use mcp_agent_mail_db::pool::{DbPool, DbPoolConfig};
use mcp_agent_mail_db::schema;
use mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection;
use tempfile::tempdir;

fn count_identity_fts_artifacts(conn: &SqliteConnection) -> i64 {
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS n FROM sqlite_master \
             WHERE (type='table' AND name IN ('fts_agents', 'fts_projects')) \
                OR (type='trigger' AND name IN (\
                    'agents_ai', 'agents_ad', 'agents_au', \
                    'projects_ai', 'projects_ad', 'projects_au'\
                ))",
            &[],
        )
        .expect("query identity FTS artifacts");
    rows.first()
        .and_then(|row| row.get_named::<i64>("n").ok())
        .unwrap_or_default()
}

fn count_message_fts_triggers(conn: &SqliteConnection) -> i64 {
    let rows = conn
        .query_sync(
            "SELECT COUNT(*) AS n FROM sqlite_master \
             WHERE type='trigger' AND name IN ('messages_ai', 'messages_ad', 'messages_au')",
            &[],
        )
        .expect("query message FTS trigger artifacts");
    rows.first()
        .and_then(|row| row.get_named::<i64>("n").ok())
        .unwrap_or_default()
}

fn create_fixture_with_identity_fts(db_path: &std::path::Path) {
    let db_path_str = db_path.display().to_string();
    let conn = SqliteConnection::open_file(db_path_str).expect("open fixture db");
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

    assert!(
        count_identity_fts_artifacts(&conn) > 0,
        "fixture should contain identity FTS artifacts before startup cleanup"
    );
}

#[test]
fn base_mode_cleanup_strips_identity_and_message_fts_artifacts() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("identity_fts_cleanup.db");
    let db_path_str = db_path.display().to_string();

    // Build a fixture DB that has full migrations (including identity and message FTS).
    create_fixture_with_identity_fts(&db_path);

    let conn = SqliteConnection::open_file(db_path_str).expect("reopen db");
    schema::enforce_base_mode_cleanup(&conn).expect("base mode cleanup");

    assert_eq!(
        count_identity_fts_artifacts(&conn),
        0,
        "base mode cleanup must remove identity FTS artifacts to prevent rowid corruption regressions"
    );
    assert_eq!(
        count_message_fts_triggers(&conn),
        0,
        "base mode cleanup must remove message FTS triggers to keep base-mode readers safe"
    );
}

#[test]
fn startup_runtime_keeps_message_fts_triggers() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("identity_fts_cleanup_no_migrations.db");
    let db_path_str = db_path.display().to_string();
    let db_url = format!("sqlite:///{}", db_path.display());
    create_fixture_with_identity_fts(&db_path);

    let config = DbPoolConfig {
        database_url: db_url,
        run_migrations: false,
        ..Default::default()
    };
    let parsed_path = config
        .sqlite_path()
        .expect("parse sqlite path from database_url");
    assert_eq!(
        parsed_path, db_path_str,
        "pool config must resolve to the same fixture DB path"
    );
    let pool = DbPool::new(&config).expect("create pool");

    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let cx = Cx::for_testing();
    rt.block_on(async {
        let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
    });
    drop(pool);

    let conn = SqliteConnection::open_file(parsed_path).expect("reopen db");
    assert_eq!(
        count_message_fts_triggers(&conn),
        3,
        "startup must keep message FTS triggers for runtime connections"
    );
    assert!(
        count_identity_fts_artifacts(&conn) > 0,
        "startup should preserve identity FTS artifacts in runtime mode"
    );
}

#[test]
fn startup_runtime_keeps_identity_fts_artifacts_with_migrations_disabled() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir
        .path()
        .join("identity_fts_cleanup_migrations_disabled.db");
    let db_path_str = db_path.display().to_string();
    let db_url = format!("sqlite:///{}", db_path.display());
    create_fixture_with_identity_fts(&db_path);

    let config = DbPoolConfig {
        database_url: db_url,
        run_migrations: false,
        ..Default::default()
    };
    let parsed_path = config
        .sqlite_path()
        .expect("parse sqlite path from database_url");
    assert_eq!(
        parsed_path, db_path_str,
        "pool config must resolve to the same fixture DB path"
    );
    let pool = DbPool::new(&config).expect("create pool");

    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let cx = Cx::for_testing();
    rt.block_on(async {
        let _conn = pool.acquire(&cx).await.into_result().expect("acquire");
    });
    drop(pool);

    let conn = SqliteConnection::open_file(parsed_path).expect("reopen db");
    assert_eq!(
        count_message_fts_triggers(&conn),
        3,
        "startup must keep message FTS triggers even when migrations are disabled"
    );
    assert!(
        count_identity_fts_artifacts(&conn) > 0,
        "startup should preserve identity FTS artifacts in runtime mode"
    );
}
