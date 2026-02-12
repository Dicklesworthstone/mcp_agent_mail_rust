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

#[test]
fn startup_cleanup_strips_identity_fts_from_preexisting_db() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("identity_fts_cleanup.db");
    let db_path_str = db_path.display().to_string();
    let db_url = format!("sqlite:///{}", db_path.display());

    // Build a fixture DB that has full migrations (including identity FTS tables/triggers).
    {
        let conn = SqliteConnection::open_file(db_path_str.clone()).expect("open fixture db");
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

    // Opening a pooled runtime connection should run startup cleanup.
    let config = DbPoolConfig {
        database_url: db_url,
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
        count_identity_fts_artifacts(&conn),
        0,
        "startup cleanup must remove identity FTS artifacts to prevent rowid corruption regressions"
    );
}
