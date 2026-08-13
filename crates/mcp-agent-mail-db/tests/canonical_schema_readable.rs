//! Regression guard for br-zrcxs / frankensqlite bd-lgolw: the comment-prefixed
//! Agent Mail schema batch must round-trip through fsqlite into a `sqlite_master`
//! that CANONICAL SQLite can read.
//!
//! The bug: fsqlite captured the leading `-- section` comment INTO the verbatim
//! `sqlite_master.sql` for each `CREATE TABLE`. fsqlite itself keeps its own
//! schema representation and was unaffected, but stock/canonical SQLite reading
//! the file re-parses `sqlite_master.sql` and failed the WHOLE schema load with
//! `malformed database schema (<table>)` — breaking migrate, fresh-mailbox
//! bootstrap (the quarantine loop), reconstruct/verification, and the resource
//! shape suite. Fixed upstream by `strip_leading_sql_comments` (fsqlite
//! 5015af7f0); this test pins the fix at the CONSUMER boundary so a future
//! fsqlite pin regression is caught in mcp_agent_mail_rust's own suite.

mod common;

use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::{CanonicalDbConn, DbConn};

/// Build a fresh db file carrying the full (comment-prefixed) Agent Mail schema
/// through fsqlite, exactly as runtime bootstrap does.
fn make_schema_db() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("canonical_schema.sqlite3");
    let path = db_path.display().to_string();

    let init_conn = DbConn::open_file(path.clone()).expect("open fsqlite connection");
    init_conn
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_BASE_SQL)
        .expect("apply init PRAGMAs");
    let cx = Cx::for_testing();
    match common::spin_poll(mcp_agent_mail_db::schema::migrate_to_latest_base(&cx, &init_conn)) {
        Outcome::Ok(_) => {}
        other => panic!("schema migration failed: {other:?}"),
    }
    mcp_agent_mail_db::close_db_conn(init_conn, "canonical_schema_readable test");
    (dir, path)
}

#[test]
fn canonical_sqlite_reads_comment_prefixed_schema() {
    let (_dir, path) = make_schema_db();

    // Canonical SQLite must be able to OPEN + read sqlite_master without hitting
    // "malformed database schema". Before the fix this query failed outright.
    let canon = CanonicalDbConn::open_file(path.clone()).expect("open canonical SQLite connection");
    let rows = canon
        .query_sync(
            "SELECT name, sql FROM sqlite_master WHERE type = 'table' AND sql IS NOT NULL \
             ORDER BY name",
            &[],
        )
        .unwrap_or_else(|e| {
            panic!("canonical SQLite failed to read sqlite_master (malformed schema?): {e}")
        });

    assert!(
        rows.len() >= 5,
        "expected the full Agent Mail table set, got {} tables",
        rows.len()
    );

    // Every stored CREATE text must begin at its first real token (`CREATE`),
    // not a leading `-- section` comment (the exact bd-lgolw defect).
    for row in &rows {
        let name = row.get_named::<String>("name").unwrap_or_default();
        let sql = row.get_named::<String>("sql").unwrap_or_default();
        let head = sql.trim_start();
        assert!(
            head.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("create")),
            "sqlite_master.sql for table `{name}` must start at CREATE, not a comment; got: {:?}",
            &head.chars().take(48).collect::<String>()
        );
    }

    // Force canonical SQLite to actually USE the parsed schema for a couple of
    // representative tables (a bare open can lazily defer the schema parse).
    for table in ["projects", "messages", "file_reservations", "idempotency_keys"] {
        canon
            .query_sync(&format!("SELECT COUNT(*) FROM {table}"), &[])
            .unwrap_or_else(|e| {
                panic!("canonical SQLite could not read table `{table}` (malformed schema?): {e}")
            });
    }

    // fsqlite itself must of course still read its own database too.
    let franken = DbConn::open_file(path).expect("re-open fsqlite connection");
    franken
        .query_sync("SELECT COUNT(*) FROM idempotency_keys", &[])
        .expect("fsqlite reads its own schema");
    mcp_agent_mail_db::close_db_conn(franken, "canonical_schema_readable fsqlite re-open");
}
