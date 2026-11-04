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
//! fsqlite pin regression is caught in `mcp_agent_mail_rust`'s own suite.
//!
//! GH#372 also protects the reverse boundary: an existing canonical legacy
//! mailbox must retain its complete payloads through native startup migrations
//! and subsequent native writes when the database engine is upgraded.

use crate::common;

use asupersync::{Cx, Outcome};
use mcp_agent_mail_db::pool::SyncQuery;
use mcp_agent_mail_db::{CanonicalDbConn, DbConn};
use sqlmodel_core::Value;

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
    match common::spin_poll(mcp_agent_mail_db::schema::migrate_to_latest_base(
        &cx, &init_conn,
    )) {
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
            head.get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case("create")),
            "sqlite_master.sql for table `{name}` must start at CREATE, not a comment; got: {:?}",
            head.chars().take(48).collect::<String>()
        );
    }

    // Force canonical SQLite to actually USE the parsed schema for a couple of
    // representative tables (a bare open can lazily defer the schema parse).
    for table in [
        "projects",
        "messages",
        "file_reservations",
        "idempotency_keys",
    ] {
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

// Python-era on-disk tables, deliberately without a Rust migration ledger or
// the later messages.recipients_json/topic/archive_metadata_json columns.
// Seed these with canonical SQLite, not the engine under test: a round trip
// which creates and reads both sides with the new engine cannot protect the
// existing-mailbox boundary when the pinned FrankenSQLite version changes.
const LEGACY_MAILBOX_SCHEMA: &str = "
    CREATE TABLE projects (
        id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE,
        human_key TEXT NOT NULL, created_at DATETIME NOT NULL
    );
    CREATE TABLE agents (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        project_id INTEGER NOT NULL REFERENCES projects(id), name TEXT NOT NULL,
        program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL,
        inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL,
        attachments_policy TEXT NOT NULL DEFAULT 'auto',
        contact_policy TEXT NOT NULL DEFAULT 'auto',
        reaper_exempt INTEGER NOT NULL DEFAULT 0, registration_token TEXT,
        UNIQUE(project_id, name)
    );
    CREATE TABLE messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        project_id INTEGER NOT NULL REFERENCES projects(id),
        sender_id INTEGER NOT NULL REFERENCES agents(id), thread_id TEXT,
        subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL,
        ack_required INTEGER NOT NULL, created_ts DATETIME NOT NULL,
        attachments TEXT NOT NULL DEFAULT '[]'
    );
    CREATE TABLE message_recipients (
        message_id INTEGER NOT NULL REFERENCES messages(id),
        agent_id INTEGER NOT NULL REFERENCES agents(id), kind TEXT NOT NULL,
        read_ts DATETIME, ack_ts DATETIME,
        PRIMARY KEY(message_id, agent_id, kind)
    );
    CREATE TABLE file_reservations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        project_id INTEGER NOT NULL REFERENCES projects(id),
        agent_id INTEGER NOT NULL REFERENCES agents(id), path_pattern TEXT NOT NULL,
        exclusive INTEGER NOT NULL, reason TEXT, created_ts DATETIME NOT NULL,
        expires_ts DATETIME NOT NULL, released_ts DATETIME
    );
";

fn seed_canonical_legacy_mailbox(conn: &CanonicalDbConn) {
    conn.execute_raw(LEGACY_MAILBOX_SCHEMA)
        .expect("canonical legacy schema");
    conn.execute_raw(
        "INSERT INTO projects VALUES (7, 'kept', '/kept', '2026-02-24 15:30:00.123456');
         INSERT INTO agents (id, project_id, name, program, model, task_description,
                             inception_ts, last_active_ts)
         VALUES (11, 7, 'BlueLake', 'legacy', 'model', 'sender',
                 '2026-02-24 15:30:00.123456', '2026-02-24 15:30:00.123456'),
                (19, 7, 'RedStone', 'legacy', 'model', 'recipient',
                 '2026-02-24 15:30:00.123456', '2026-02-24 15:30:00.123456'),
                (23, 7, 'GreenHill', 'legacy', 'model', 'copy',
                 '2026-02-24 15:30:00.123456', '2026-02-24 15:30:00.123456');",
    )
    .expect("canonical project and agents");
    // Overflow payload pages and non-ASCII text must survive, as must attachment
    // metadata byte-for-byte. An empty body/attachment fixture would miss both.
    let body = "# Retained mail\nCafé — 日本語 🦀; 'quoted' and \\path\n".repeat(256);
    let attachments = r#"[{"type":"image","path":"attachments/kept.png","sha256":"deadbeef","width":32,"height":16}]"#;
    conn.execute_sync(
        "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md,
                               importance, ack_required, created_ts, attachments)
         VALUES (500, 7, 11, 'kept-thread', 'Retained subject', ?1, 'high', 1,
                 '2026-02-24 15:30:00.123456', ?2)",
        &[Value::Text(body), Value::Text(attachments.to_string())],
    )
    .expect("canonical message with overflow body and attachment");
    conn.execute_raw(
        "INSERT INTO messages VALUES
            (900, 7, 19, NULL, 'Second retained message', '', 'normal', 0,
             '2026-02-24 15:30:00.123456', '[]');
         INSERT INTO message_recipients VALUES
            (500, 19, 'to', '2026-02-24 15:32:00.123456', '2026-02-24 15:33:00.123456'),
            (500, 23, 'cc', NULL, NULL), (900, 11, 'bcc', NULL, NULL);
         INSERT INTO file_reservations VALUES
            (77, 7, 11, 'src/**', 1, 'retained lease',
             '2026-02-24 15:30:00.123456', '2026-02-24 16:30:00.123456', NULL);",
    )
    .expect("canonical recipients, second message and reservation");
}

/// Rows as values, with every SQLite INTEGER widened to `BigInt`. Canonical
/// SQLite reports an integer that fits in 32 bits as `Int` while `FrankenSQLite`
/// reports `BigInt`, so comparing one engine's rows with the other's must
/// compare integer values, not the adapter's choice of variant.
fn query_values(conn: &impl SyncQuery, sql: &str) -> Vec<Vec<Value>> {
    conn.query_sync(sql, &[])
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .into_iter()
        .map(|row| {
            row.values()
                .map(|value| match value {
                    // Canonical SQLite uses Int for i32-range INTEGERs;
                    // FrankenSQLite always uses BigInt. Compare the exact SQL
                    // integer without coercing text, floats, blobs or NULL.
                    Value::TinyInt(n) => Value::BigInt(i64::from(*n)),
                    Value::SmallInt(n) => Value::BigInt(i64::from(*n)),
                    Value::Int(n) => Value::BigInt(i64::from(*n)),
                    other => other.clone(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn cross_engine_payload_comparison_preserves_values_and_sql_types() {
    let dir = tempfile::tempdir().expect("integer comparison fixture");
    let path = dir.path().join("values.sqlite3").display().to_string();
    let canonical = CanonicalDbConn::open_file(&path).expect("canonical value writer");
    canonical
        .execute_raw(
            "CREATE TABLE payloads (value);
             INSERT INTO payloads VALUES
                (7), (8), ('7'), (7.0), (x'37'), (NULL),
                (-2147483648), (2147483647), (-2147483649), (2147483648),
                (-9223372036854775808), (9223372036854775807);",
        )
        .expect("seed integer bounds and distinct SQL types");
    let sql = "SELECT value FROM payloads ORDER BY rowid";
    let raw = canonical
        .query_sync(sql, &[])
        .expect("raw canonical values");
    assert_eq!(raw[0].values().next(), Some(&Value::Int(7)));
    let original = query_values(&canonical, sql);
    assert_eq!(
        original,
        vec![
            vec![Value::BigInt(7)],
            vec![Value::BigInt(8)],
            vec![Value::Text("7".into())],
            vec![Value::Double(7.0)],
            vec![Value::Bytes(vec![b'7'])],
            vec![Value::Null],
            vec![Value::BigInt(i64::from(i32::MIN))],
            vec![Value::BigInt(i64::from(i32::MAX))],
            vec![Value::BigInt(i64::from(i32::MIN) - 1)],
            vec![Value::BigInt(i64::from(i32::MAX) + 1)],
            vec![Value::BigInt(i64::MIN)],
            vec![Value::BigInt(i64::MAX)],
        ]
    );
    for distinct in &original[1..6] {
        assert_ne!(
            original[0], *distinct,
            "SQL type/value must remain distinct"
        );
    }
    drop(canonical);

    let native = DbConn::open_file(&path).expect("native value reader");
    let raw = native.query_sync(sql, &[]).expect("raw native values");
    assert_eq!(raw[0].values().next(), Some(&Value::BigInt(7)));
    assert_eq!(query_values(&native, sql), original);
    native.close_sync().expect("close native value reader");
}

/// Original payloads and relational keys, excluding timestamps that the
/// documented v3 migration intentionally converts from TEXT to microseconds.
fn retained_legacy_payloads(conn: &impl SyncQuery) -> Vec<Vec<Vec<Value>>> {
    [
        "SELECT id, slug, human_key FROM projects ORDER BY id",
        "SELECT id, project_id, name, program, model, task_description,
                attachments_policy, contact_policy FROM agents ORDER BY id",
        "SELECT id, project_id, sender_id, thread_id, subject, body_md, importance,
                ack_required, attachments FROM messages WHERE id <= 900 ORDER BY id",
        "SELECT message_id, agent_id, kind FROM message_recipients
         WHERE message_id <= 900 ORDER BY message_id, agent_id, kind",
        "SELECT id, project_id, agent_id, path_pattern, exclusive, reason
         FROM file_reservations ORDER BY id",
    ]
    .into_iter()
    .map(|sql| query_values(conn, sql))
    .collect()
}

fn assert_legacy_timestamps(conn: &impl SyncQuery) {
    const CREATED: i64 = 1_771_947_000_123_456;
    for (sql, expected) in [
        (
            "SELECT created_at FROM projects ORDER BY id",
            vec![vec![Value::BigInt(CREATED)]],
        ),
        (
            "SELECT inception_ts, last_active_ts FROM agents ORDER BY id",
            vec![vec![Value::BigInt(CREATED), Value::BigInt(CREATED)]; 3],
        ),
        (
            "SELECT created_ts FROM messages WHERE id <= 900 ORDER BY id",
            vec![vec![Value::BigInt(CREATED)]; 2],
        ),
        (
            "SELECT read_ts, ack_ts FROM message_recipients
             WHERE message_id <= 900 ORDER BY message_id, agent_id, kind",
            vec![
                vec![
                    Value::BigInt(CREATED + 120_000_000),
                    Value::BigInt(CREATED + 180_000_000),
                ],
                vec![Value::Null, Value::Null],
                vec![Value::Null, Value::Null],
            ],
        ),
        (
            "SELECT created_ts, expires_ts, released_ts FROM file_reservations ORDER BY id",
            vec![vec![
                Value::BigInt(CREATED),
                Value::BigInt(CREATED + 3_600_000_000),
                Value::Null,
            ]],
        ),
    ] {
        assert_eq!(query_values(conn, sql), expected, "{sql}");
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the sequential engine ownership phases visible together.
fn canonical_legacy_mailbox_survives_native_upgrade_and_new_writes() {
    let dir = tempfile::tempdir().expect("legacy mailbox directory");
    let path = dir.path().join("legacy.sqlite3").display().to_string();
    let canonical = CanonicalDbConn::open_file(&path).expect("canonical legacy writer");
    seed_canonical_legacy_mailbox(&canonical);
    let original = retained_legacy_payloads(&canonical);
    drop(canonical);

    // Match startup's native base pass followed by its deferred canonical
    // migrations. Close each owner before the other engine opens the file.
    let native = DbConn::open_file(&path).expect("new engine opens existing mailbox");
    native
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
        .expect("runtime migration PRAGMAs");
    let applied = common::block_on({
        let native = &native;
        move |cx| async move {
            mcp_agent_mail_db::schema::migrate_to_latest_base(&cx, native)
                .await
                .into_result()
                .expect("native legacy base migration")
        }
    });
    assert!(!applied.is_empty(), "fixture must exercise real migrations");
    assert_eq!(retained_legacy_payloads(&native), original);
    assert_legacy_timestamps(&native);
    native.close_sync().expect("close native migration owner");

    let canonical = CanonicalDbConn::open_file(&path).expect("canonical deferred migration owner");
    canonical
        .execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_SQL)
        .expect("canonical migration PRAGMAs");
    let applied = common::block_on({
        let canonical = &canonical;
        move |cx| async move {
            mcp_agent_mail_db::schema::migrate_to_latest(&cx, canonical)
                .await
                .into_result()
                .expect("deferred legacy migrations")
        }
    });
    assert!(
        applied
            .iter()
            .any(|id| id == "v15_add_recipients_json_to_messages")
    );
    assert_eq!(retained_legacy_payloads(&canonical), original);
    canonical
        .execute_raw("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("publish canonical migrations before runtime reopen");
    drop(canonical);

    let native = DbConn::open_file(&path).expect("native upgraded mailbox writer");
    assert_eq!(retained_legacy_payloads(&native), original);
    assert_legacy_timestamps(&native);
    native
        .execute_raw(
            "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md,
                                   importance, ack_required, created_ts)
             VALUES (7, 11, 'kept-thread', 'After upgrade', 'new body', 'normal', 0,
                     1771947001000000);
             INSERT INTO message_recipients (message_id, agent_id, kind)
             VALUES (901, 19, 'to');",
        )
        .expect("new message and delivery after upgrade");
    native
        .close_sync()
        .expect("close native upgraded mailbox writer");

    let canonical = CanonicalDbConn::open_file(&path).expect("independent final verifier");
    assert_eq!(retained_legacy_payloads(&canonical), original);
    assert_legacy_timestamps(&canonical);
    assert_eq!(
        query_values(
            &canonical,
            "SELECT id, subject, body_md FROM messages WHERE id > 900"
        ),
        vec![vec![
            Value::BigInt(901),
            Value::Text("After upgrade".into()),
            Value::Text("new body".into())
        ]],
        "a new write must preserve the existing message-ID floor"
    );
    assert_eq!(
        query_values(
            &canonical,
            "SELECT recipients_json FROM messages WHERE id <= 900 ORDER BY id"
        ),
        vec![vec![Value::Text("{}".into())]; 2]
    );
    assert_eq!(
        query_values(
            &canonical,
            "SELECT message_id, agent_id, kind, read_ts, ack_ts
             FROM message_recipients WHERE message_id > 900"
        ),
        vec![vec![
            Value::BigInt(901),
            Value::BigInt(19),
            Value::Text("to".into()),
            Value::Null,
            Value::Null,
        ]]
    );
    assert_eq!(
        query_values(&canonical, "PRAGMA integrity_check"),
        vec![vec![Value::Text("ok".into())]]
    );
    assert_eq!(
        query_values(&canonical, "PRAGMA foreign_key_check"),
        [] as [Vec<Value>; 0]
    );
}
