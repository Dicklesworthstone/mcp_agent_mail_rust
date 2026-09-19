//! GH326: a native export must be usable as a strictly verified SQLite backup.
//!
//! These tests intentionally cross the real runtime/export/canonical boundary.
//! A quick-check pass, equal counts, or a tolerated collation disagreement is
//! not a substitute for a strict full check of the produced artifact.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcp_agent_mail_db::pool::SyncQuery;
use mcp_agent_mail_db::{CanonicalDbConn, DbConn, DbPool, DbPoolConfig};
use sqlmodel_core::Value;

const MAILBOX_SCHEMA: &str = "
    CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT);
    CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT);
    CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER,
                           subject TEXT, body_md TEXT);
    CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT);
    CREATE UNIQUE INDEX idx_agents_project_name_nocase
        ON agents(project_id, name COLLATE NOCASE);
    INSERT INTO projects VALUES (1, 'nocase-fixture', '/nocase-fixture');
    INSERT INTO agents VALUES
        (1, 1, 'Z'), (2, 1, '['), (3, 1, 'codex_823_cache'),
        (4, 1, '_'), (5, 1, '`'), (6, 1, '{');
    INSERT INTO messages VALUES (1, 1, 3, 'retained subject', 'retained body');
    INSERT INTO message_recipients VALUES (1, 1, 'to');
";

fn retained_fixture() -> PathBuf {
    tempfile::tempdir()
        .expect("create private native fixture")
        .keep()
        .canonicalize()
        .expect("canonical fixture path")
}

fn seed_native_mailbox(root: &Path) -> PathBuf {
    let primary = root.join("mailbox.sqlite3");
    let writer = DbConn::open_file(primary.to_str().expect("fixture UTF-8"))
        .expect("open real runtime writer");
    writer
        .execute_raw(MAILBOX_SCHEMA)
        .expect("seed native mailbox");
    mcp_agent_mail_db::close_db_conn(writer, "GH326 native mailbox fixture");
    primary
}

fn sql_compare(conn: &impl SyncQuery, left: &str, right: &str) -> i64 {
    let rows = conn
        .query_sync(
            "SELECT CASE WHEN ?1 COLLATE NOCASE < ?2 THEN -1 \
             WHEN ?1 COLLATE NOCASE > ?2 THEN 1 ELSE 0 END AS ordering",
            &[
                Value::Text(left.to_string()),
                Value::Text(right.to_string()),
            ],
        )
        .expect("execute real NOCASE comparison");
    assert_eq!(rows.len(), 1);
    rows[0]
        .get_named::<i64>("ordering")
        .expect("comparison result")
}

fn agent_rows(conn: &impl SyncQuery) -> Vec<(i64, String)> {
    conn.query_sync("SELECT id, name FROM agents NOT INDEXED ORDER BY id", &[])
        .expect("read table rows independently of the index")
        .into_iter()
        .map(|row| {
            (
                row.get_named::<i64>("id").expect("agent id"),
                row.get_named::<String>("name").expect("agent name"),
            )
        })
        .collect()
}

fn assert_strict_snapshot(path: &Path) {
    let conn = CanonicalDbConn::open_file(path.to_str().expect("snapshot UTF-8"))
        .expect("open only the private output with canonical SQLite");
    let rows = conn
        .query_sync("PRAGMA integrity_check(1000000)", &[])
        .expect("execute strict canonical full integrity check");
    let details = mcp_agent_mail_db::integrity::extract_check_details(
        &rows,
        mcp_agent_mail_db::CheckKind::Full,
    );
    assert_eq!(details, ["ok"], "export must not need a NOCASE exception");
    assert_eq!(
        agent_rows(&conn),
        vec![
            (1, "Z".to_string()),
            (2, "[".to_string()),
            (3, "codex_823_cache".to_string()),
            (4, "_".to_string()),
            (5, "`".to_string()),
            (6, "{".to_string()),
        ]
    );
    let bodies = conn
        .query_sync("SELECT subject, body_md FROM messages WHERE id = 1", &[])
        .expect("read retained message");
    assert_eq!(
        bodies[0].get_named::<String>("subject").unwrap(),
        "retained subject"
    );
    assert_eq!(
        bodies[0].get_named::<String>("body_md").unwrap(),
        "retained body"
    );
}

#[test]
fn runtime_nocase_sql_matches_canonical_boundaries() {
    let runtime = DbConn::open_memory().expect("runtime comparison fixture");
    let canonical = CanonicalDbConn::open_memory().expect("canonical comparison oracle");
    let values = [
        "",
        "A",
        "a",
        "Z",
        "z",
        "[",
        "\\",
        "]",
        "^",
        "_",
        "`",
        "{",
        "codex_823_cache",
        "CODEX_823_CACHE",
        "Ä",
        "ä",
        "\0",
        "A\0x",
        "a\0y",
        "a\0longer",
        "a\0",
        "a",
    ];
    for left in values {
        for right in values {
            assert_eq!(
                sql_compare(&runtime, left, right),
                sql_compare(&canonical, left, right),
                "runtime/canonical NOCASE disagreement for {left:?} versus {right:?}"
            );
        }
    }
}

#[test]
fn runtime_vacuum_into_has_canonical_nocase_index_order() {
    let root = retained_fixture();
    let primary = seed_native_mailbox(&root);
    let before = std::fs::read(&primary).expect("source bytes before export");
    let export = root.join("export.sqlite3");
    let reader = mcp_agent_mail_db::pool::open_guarded_read_only_sqlite_file(
        &primary,
        "GH326 native export test",
    )
    .expect("open guarded runtime source");
    reader
        .execute_raw("PRAGMA query_only = OFF")
        .expect("enable private export");
    let literal = export.to_str().expect("export UTF-8").replace('\'', "''");
    reader
        .execute_raw(&format!("VACUUM INTO '{literal}'"))
        .expect("native export");
    drop(reader);
    assert_strict_snapshot(&export);
    assert_eq!(
        std::fs::read(primary).unwrap(),
        before,
        "export must not rewrite the source"
    );
}

#[test]
fn public_proactive_backup_publishes_a_strict_nocase_snapshot() {
    let root = retained_fixture();
    let primary = seed_native_mailbox(&root);
    let before = std::fs::read(&primary).expect("source bytes before backup");
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", primary.display()),
        storage_root: Some(root.join("archive")),
        run_migrations: false,
        ..DbPoolConfig::default()
    };
    let pool = DbPool::new_without_startup_init(&config).expect("construct native backup pool");
    let backup = pool
        .create_proactive_backup(Duration::ZERO)
        .expect("public backup producer must not reject its own NOCASE export")
        .expect("a new backup must actually be published");
    assert_eq!(backup, primary.with_file_name("mailbox.sqlite3.bak"));
    assert!(
        mcp_agent_mail_db::pool::sqlite_recovery_candidate_passes_full_integrity_check(&backup)
            .expect("strict recovery-candidate probe")
    );
    assert_strict_snapshot(&backup);
    assert_eq!(std::fs::read(&primary).unwrap(), before);
    assert!(
        pool.create_proactive_backup(Duration::from_secs(3600))
            .expect("reuse the verified recent backup")
            .is_none()
    );
}

#[test]
fn nocase_unique_index_still_rejects_case_only_duplicate_names() {
    let root = retained_fixture();
    let primary = seed_native_mailbox(&root);
    let conn = DbConn::open_file(primary.to_str().unwrap()).expect("reopen runtime mailbox");
    let before = agent_rows(&conn);
    assert!(
        conn.execute_raw("INSERT INTO agents VALUES (7, 1, 'CODEX_823_CACHE')")
            .is_err(),
        "fixing punctuation order must not weaken case-insensitive uniqueness"
    );
    assert_eq!(agent_rows(&conn), before);
    mcp_agent_mail_db::close_db_conn(conn, "GH326 uniqueness fixture");
}
