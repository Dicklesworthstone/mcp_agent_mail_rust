//! Integration tests for schema migration paths (v1 -> v2 -> ... -> latest).
//!
//! Verifies:
//! - Fresh databases reach the correct schema version with all tables/indexes
//! - FTS5 virtual tables and triggers are properly set up
//! - Migration idempotency (re-running produces no errors or duplicate work)
//! - Column existence and type correctness
//! - Roundtrip through pool-based initialization matches direct migration
//! - Legacy TEXT timestamp conversion (v3) with real data
//! - Composite index creation (v4) on existing data
//! - FTS tokenizer upgrade (v5) preserves and improves search
//! - Inbox stats materialization (v6) with trigger validation
//! - Identity FTS (v7) backfill and triggers
//! - Search recipes tables (v8)

#![allow(clippy::redundant_clone, clippy::too_many_lines)]

use asupersync::cx::Cx;
use asupersync::runtime::RuntimeBuilder;
use mcp_agent_mail_db::schema::{
    self, MIGRATIONS_TABLE_NAME, PRAGMA_SETTINGS_SQL, enforce_runtime_fts_cleanup,
    migrate_to_latest, migration_status,
};
use mcp_agent_mail_db::DbConn as SqliteConnection;
use mcp_agent_mail_db::{DbPool, DbPoolConfig};
use sqlmodel_core::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn block_on<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

/// Create a file-backed `SqliteConnection` in a temporary directory.
/// Uses C-backed `SQLite` so tests can query `sqlite_master` for schema verification.
fn open_temp_db() -> (SqliteConnection, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("schema_mig_{}.db", unique_suffix()));
    let conn =
        SqliteConnection::open_file(db_path.display().to_string()).expect("open sqlite connection");
    (conn, dir)
}

/// Create a pool-managed database in a temporary directory.
fn make_pool() -> (DbPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let db_path = dir
        .path()
        .join(format!("schema_pool_{}.db", unique_suffix()));
    let config = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        max_connections: 5,
        min_connections: 1,
        acquire_timeout_ms: 30_000,
        max_lifetime_ms: 3_600_000,
        run_migrations: true,
        warmup_connections: 0,
    };
    let pool = DbPool::new(&config).expect("create pool");
    (pool, dir)
}

// ---------------------------------------------------------------------------
// 1. Fresh database has correct schema version
// ---------------------------------------------------------------------------

#[test]
fn fresh_db_reaches_latest_schema_version() {
    let (conn, _dir) = open_temp_db();

    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Must have applied a meaningful number of migrations (v1 tables + v2 triggers
    // + v3 timestamp fixes + v4 indexes + v5 FTS + v6 inbox_stats + v7 identity FTS + v8 recipes).
    assert!(
        applied.len() > 30,
        "expected 30+ migrations on fresh DB, got {}",
        applied.len()
    );

    // Check that v1 through v8 prefixes are all represented.
    for prefix in &["v1_", "v2_", "v3_", "v4_", "v5_", "v6_", "v7_", "v8_"] {
        assert!(
            applied.iter().any(|id| id.starts_with(prefix)),
            "missing migration with prefix {prefix} in applied list"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. All tables exist after migration
// ---------------------------------------------------------------------------

#[test]
fn all_expected_tables_exist_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
            &[],
        )
        .expect("query sqlite_master for tables");

    let table_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    let expected_tables = [
        "projects",
        "products",
        "product_project_links",
        "agents",
        "messages",
        "message_recipients",
        "file_reservations",
        "agent_links",
        "project_sibling_suggestions",
        "inbox_stats",
        "search_recipes",
        "query_history",
    ];

    for table in &expected_tables {
        assert!(
            table_names.contains(&table.to_string()),
            "missing table '{table}' in {table_names:?}"
        );
    }

    // Verify migration tracking table exists.
    assert!(
        table_names.contains(&MIGRATIONS_TABLE_NAME.to_string()),
        "missing migration tracking table '{MIGRATIONS_TABLE_NAME}'"
    );
}

// ---------------------------------------------------------------------------
// 3. FTS tables are properly set up
// ---------------------------------------------------------------------------

#[test]
fn fts_virtual_tables_absent_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'fts_%' ORDER BY name",
            &[],
        )
        .expect("query FTS tables");

    let fts_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // v11 drops all FTS tables (Tantivy handles search now).
    assert!(
        fts_names.is_empty(),
        "no FTS tables should exist after v11 migration, found: {fts_names:?}"
    );
}

#[test]
fn triggers_after_migration() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name",
            &[],
        )
        .expect("query triggers");

    let trigger_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // FTS triggers should NOT exist (v11 drops them, Tantivy handles search).
    for name in &[
        "messages_ai",
        "messages_ad",
        "messages_au",
        "agents_ai",
        "agents_ad",
        "agents_au",
        "projects_ai",
        "projects_ad",
        "projects_au",
        "fts_messages_ai",
        "fts_messages_ad",
        "fts_messages_au",
    ] {
        assert!(
            !trigger_names.contains(&name.to_string()),
            "FTS trigger '{name}' should not exist after v11 migration, found: {trigger_names:?}"
        );
    }

    // v6 inbox_stats triggers should still exist.
    for name in &[
        "trg_inbox_stats_insert",
        "trg_inbox_stats_mark_read",
        "trg_inbox_stats_ack",
    ] {
        assert!(
            trigger_names.contains(&name.to_string()),
            "missing inbox stats trigger '{name}' in {trigger_names:?}"
        );
    }
}

// NOTE: fts_message_insert_trigger_fires removed — FTS5 triggers dropped
// in v11 migration (br-2tnl.8.4).  Tantivy handles indexing now.

// ---------------------------------------------------------------------------
// 4. Key columns exist with correct types
// ---------------------------------------------------------------------------

#[test]
fn key_columns_exist_with_correct_types() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Helper to get column info for a table.
    let columns_of = |table: &str| -> Vec<(String, String, bool)> {
        let rows = conn
            .query_sync(&format!("PRAGMA table_info({table})"), &[])
            .unwrap_or_else(|_| panic!("PRAGMA table_info({table}) failed"));
        rows.iter()
            .map(|r| {
                let name: String = r.get_named("name").unwrap_or_default();
                let col_type: String = r.get_named("type").unwrap_or_default();
                let notnull: i64 = r.get_named("notnull").unwrap_or(0);
                (name, col_type.to_uppercase(), notnull != 0)
            })
            .collect()
    };

    // projects table
    let cols = columns_of("projects");
    assert!(
        cols.iter().any(|(n, t, _)| n == "id" && t == "INTEGER"),
        "projects.id should be INTEGER"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "slug" && t == "TEXT" && *nn),
        "projects.slug should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "human_key" && t == "TEXT" && *nn),
        "projects.human_key should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "created_at" && t == "INTEGER" && *nn),
        "projects.created_at should be INTEGER NOT NULL"
    );

    // agents table
    let cols = columns_of("agents");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "project_id" && t == "INTEGER" && *nn),
        "agents.project_id should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "name" && t == "TEXT" && *nn),
        "agents.name should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "inception_ts" && t == "INTEGER" && *nn),
        "agents.inception_ts should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "attachments_policy" && t == "TEXT" && *nn),
        "agents.attachments_policy should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "contact_policy" && t == "TEXT" && *nn),
        "agents.contact_policy should be TEXT NOT NULL"
    );

    // messages table
    let cols = columns_of("messages");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "sender_id" && t == "INTEGER" && *nn),
        "messages.sender_id should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "thread_id" && !nn),
        "messages.thread_id should be nullable"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "body_md" && t == "TEXT" && *nn),
        "messages.body_md should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "ack_required" && t == "INTEGER" && *nn),
        "messages.ack_required should be INTEGER NOT NULL"
    );

    // message_recipients table
    let cols = columns_of("message_recipients");
    assert!(
        cols.iter().any(|(n, _, nn)| n == "read_ts" && !nn),
        "message_recipients.read_ts should be nullable"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "ack_ts" && !nn),
        "message_recipients.ack_ts should be nullable"
    );

    // file_reservations table
    let cols = columns_of("file_reservations");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "path_pattern" && t == "TEXT" && *nn),
        "file_reservations.path_pattern should be TEXT NOT NULL"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "released_ts" && !nn),
        "file_reservations.released_ts should be nullable"
    );

    // inbox_stats table (v6)
    let cols = columns_of("inbox_stats");
    assert!(
        cols.iter()
            .any(|(n, t, _)| n == "agent_id" && t == "INTEGER"),
        "inbox_stats.agent_id should be INTEGER (PK)"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "total_count" && t == "INTEGER" && *nn),
        "inbox_stats.total_count should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "unread_count" && t == "INTEGER" && *nn),
        "inbox_stats.unread_count should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "ack_pending_count" && t == "INTEGER" && *nn),
        "inbox_stats.ack_pending_count should be INTEGER NOT NULL"
    );
    assert!(
        cols.iter().any(|(n, _, nn)| n == "last_message_ts" && !nn),
        "inbox_stats.last_message_ts should be nullable"
    );

    // search_recipes table (v8)
    let cols = columns_of("search_recipes");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "name" && t == "TEXT" && *nn),
        "search_recipes.name should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "query_text" && t == "TEXT" && *nn),
        "search_recipes.query_text should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "pinned" && t == "INTEGER" && *nn),
        "search_recipes.pinned should be INTEGER NOT NULL"
    );

    // query_history table (v8)
    let cols = columns_of("query_history");
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "query_text" && t == "TEXT" && *nn),
        "query_history.query_text should be TEXT NOT NULL"
    );
    assert!(
        cols.iter()
            .any(|(n, t, nn)| n == "executed_ts" && t == "INTEGER" && *nn),
        "query_history.executed_ts should be INTEGER NOT NULL"
    );
}

// ---------------------------------------------------------------------------
// 5. Migration idempotency
// ---------------------------------------------------------------------------

#[test]
fn migration_is_idempotent() {
    let (conn, _dir) = open_temp_db();

    // First run: applies all migrations.
    let applied1 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(!applied1.is_empty(), "first run should apply migrations");

    // Second run: no new migrations to apply.
    let applied2 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied2.is_empty(),
        "second run should be no-op, but applied: {applied2:?}"
    );

    // Third run: still idempotent.
    let applied3 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied3.is_empty(),
        "third run should also be no-op, but applied: {applied3:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. All indexes exist after migration
// ---------------------------------------------------------------------------

#[test]
fn all_expected_indexes_exist() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%' ORDER BY name",
            &[],
        )
        .expect("query indexes");

    let index_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // v1 indexes
    let v1_indexes = [
        "idx_projects_slug",
        "idx_projects_human_key",
        "idx_products_uid",
        "idx_products_name",
        "idx_agents_project_name",
        "idx_messages_project_created",
        "idx_messages_project_sender_created",
        "idx_messages_thread_id",
        "idx_messages_importance",
        "idx_messages_created_ts",
        "idx_message_recipients_agent",
        "idx_message_recipients_agent_message",
        "idx_file_reservations_project_released_expires",
        "idx_file_reservations_project_agent_released",
        "idx_file_reservations_expires_ts",
        "idx_agent_links_a_project",
        "idx_agent_links_b_project",
        "idx_agent_links_status",
    ];

    for idx in &v1_indexes {
        assert!(
            index_names.contains(&idx.to_string()),
            "missing v1 index '{idx}' in {index_names:?}"
        );
    }

    // v4 composite indexes
    let v4_indexes = [
        "idx_mr_agent_ack",
        "idx_msg_thread_created",
        "idx_msg_project_importance_created",
        "idx_al_a_agent_status",
        "idx_al_b_agent_status",
    ];

    for idx in &v4_indexes {
        assert!(
            index_names.contains(&idx.to_string()),
            "missing v4 composite index '{idx}' in {index_names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Pool-based initialization matches direct migration
// ---------------------------------------------------------------------------

#[test]
fn pool_initialization_creates_same_schema_as_direct_migration() {
    // Direct migration path.
    let (conn, _dir1) = open_temp_db();
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    // Apply the same runtime cleanup that pool startup performs (drops legacy
    // identity FTS tables: fts_agents, fts_projects and their triggers).
    enforce_runtime_fts_cleanup(&conn).expect("identity fts cleanup");

    let direct_tables: Vec<String> = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
            &[],
        )
        .expect("query tables (direct)")
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();

    // Pool-managed path.
    let (pool, _dir2) = make_pool();
    let pool2 = pool.clone();
    block_on(|cx| async move {
        let conn = pool2.acquire(&cx).await.into_result().unwrap();
        // Just acquiring a connection triggers schema setup.
        let pool_tables: Vec<String> = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
                &[],
            )
            .expect("query tables (pool)")
            .iter()
            .filter_map(|r| r.get_named::<String>("name").ok())
            .collect();

        assert_eq!(
            direct_tables, pool_tables,
            "pool-created schema should match direct migration schema"
        );
    });
}

// ---------------------------------------------------------------------------
// 8. Migration status tracking
// ---------------------------------------------------------------------------

#[test]
fn migration_status_reports_all_applied() {
    let (conn, _dir) = open_temp_db();

    // Apply all migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Get status.
    let statuses = block_on({
        let conn = &conn;
        move |cx| async move { migration_status(&cx, conn).await.into_result().unwrap() }
    });

    // Every migration should be Applied (not Pending).
    let pending: Vec<_> = statuses
        .iter()
        .filter(|(_, s)| matches!(s, sqlmodel_schema::MigrationStatus::Pending))
        .collect();

    assert!(
        pending.is_empty(),
        "all migrations should be applied, but these are pending: {pending:?}"
    );

    // The number of statuses should match the total migration count.
    let all_migrations = schema::schema_migrations();
    assert_eq!(
        statuses.len(),
        all_migrations.len(),
        "status count should match total migrations"
    );
}

// ---------------------------------------------------------------------------
// 9. v3 legacy TEXT timestamp roundtrip with real data
// ---------------------------------------------------------------------------

#[test]
fn v3_text_timestamp_conversion_roundtrip() {
    let (conn, _dir) = open_temp_db();

    conn.execute_raw(PRAGMA_SETTINGS_SQL)
        .expect("apply PRAGMAs");

    // Simulate a legacy Python database with DATETIME columns (TEXT storage).
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at DATETIME NOT NULL)",
        &[],
    )
    .expect("create legacy projects");

    // Insert rows with different timestamp formats.
    let timestamps = [
        ("proj-a", "/a", "2026-02-04 22:13:11.079199"),
        ("proj-b", "/b", "2026-01-01 00:00:00.000000"),
        ("proj-c", "/c", "2026-12-31 23:59:59.999999"),
    ];

    for (slug, key, ts) in &timestamps {
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text(slug.to_string()),
                Value::Text(key.to_string()),
                Value::Text(ts.to_string()),
            ],
        )
        .expect("insert legacy project");
    }

    // Create other required tables with TEXT timestamps.
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL DEFAULT '', inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', UNIQUE(project_id, name))",
        &[],
    )
    .expect("create legacy agents");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("BlueLake".into()),
            Value::Text("cc".into()),
            Value::Text("opus".into()),
            Value::Text("2026-02-05 00:06:44.082288".into()),
            Value::Text("2026-02-05 01:30:00.000000".into()),
        ],
    )
    .expect("insert legacy agent");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL DEFAULT 'normal', ack_required INTEGER NOT NULL DEFAULT 0, created_ts DATETIME NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
        &[],
    )
    .expect("create legacy messages");

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, created_ts) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("Test msg".into()),
            Value::Text("Body".into()),
            Value::Text("2026-06-15 12:30:45.123456".into()),
        ],
    )
    .expect("insert legacy message");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS file_reservations (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL DEFAULT 1, reason TEXT NOT NULL DEFAULT '', created_ts DATETIME NOT NULL, expires_ts DATETIME NOT NULL, released_ts DATETIME)",
        &[],
    )
    .expect("create legacy file_reservations");

    conn.execute_sync(
        "INSERT INTO file_reservations (project_id, agent_id, path_pattern, created_ts, expires_ts) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("*.rs".into()),
            Value::Text("2026-03-01 10:00:00.500000".into()),
            Value::Text("2026-03-01 11:00:00.750000".into()),
        ],
    )
    .expect("insert legacy file reservation");

    // Create products/product_project_links with TEXT timestamps.
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS products (id INTEGER PRIMARY KEY AUTOINCREMENT, product_uid TEXT NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE, created_at DATETIME NOT NULL)",
        &[],
    )
    .expect("create legacy products");
    conn.execute_sync(
        "INSERT INTO products (product_uid, name, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("uid1".into()),
            Value::Text("Prod1".into()),
            Value::Text("2026-04-01 00:00:00.000001".into()),
        ],
    )
    .expect("insert legacy product");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS product_project_links (id INTEGER PRIMARY KEY AUTOINCREMENT, product_id INTEGER NOT NULL, project_id INTEGER NOT NULL, created_at DATETIME NOT NULL, UNIQUE(product_id, project_id))",
        &[],
    )
    .expect("create legacy product_project_links");
    conn.execute_sync(
        "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("2026-05-15 06:30:00.999000".into()),
        ],
    )
    .expect("insert legacy link");

    // Run migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Verify ALL timestamps are now integers (not text).
    let verify_integer_type = |table: &str, col: &str| {
        let rows = conn
            .query_sync(
                &format!("SELECT typeof({col}) as t, {col} as v FROM {table}"),
                &[],
            )
            .unwrap_or_else(|_| panic!("query {table}.{col}"));
        for (i, row) in rows.iter().enumerate() {
            let t: String = row.get_named("t").unwrap_or_default();
            assert_eq!(
                t, "integer",
                "{table}.{col} row {i} should be integer, got '{t}'"
            );
            let v: i64 = row.get_named("v").unwrap_or(0);
            assert!(
                v > 1_700_000_000_000_000,
                "{table}.{col} row {i} should be microseconds since epoch, got {v}"
            );
        }
    };

    verify_integer_type("projects", "created_at");
    verify_integer_type("agents", "inception_ts");
    verify_integer_type("agents", "last_active_ts");
    verify_integer_type("messages", "created_ts");
    verify_integer_type("file_reservations", "created_ts");
    verify_integer_type("file_reservations", "expires_ts");
    verify_integer_type("products", "created_at");
    verify_integer_type("product_project_links", "created_at");

    // Verify the converted values are in the expected range.
    // "2026-02-04 22:13:11.079199" should convert to approximately 1.77e15 microseconds.
    // SQLite strftime('%s') treats timestamps as UTC. We verify that all three
    // project rows have timestamps in the 2026 range (roughly 1.77e15 to 1.80e15).
    let rows = conn
        .query_sync("SELECT created_at FROM projects ORDER BY id", &[])
        .expect("query all projects");
    assert_eq!(rows.len(), 3);
    for (i, row) in rows.iter().enumerate() {
        let created_at: i64 = row.get_named("created_at").unwrap();
        assert!(
            created_at > 1_767_000_000_000_000 && created_at < 1_800_000_000_000_000,
            "project row {i} created_at={created_at} should be in 2026 microsecond range"
        );
    }

    // Verify fractional microseconds are preserved (the .079199 part).
    let rows = conn
        .query_sync("SELECT created_at FROM projects WHERE slug = 'proj-a'", &[])
        .expect("query proj-a");
    let created_at: i64 = rows[0].get_named("created_at").unwrap();
    let fractional = created_at % 1_000_000;
    assert_eq!(
        fractional, 79199,
        "proj-a fractional microseconds should be 079199, got {fractional}"
    );
}

// ---------------------------------------------------------------------------
// 10. v6 inbox_stats triggers fire correctly with real data
// ---------------------------------------------------------------------------

#[test]
fn v6_inbox_stats_triggers_work_with_real_data() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Set up project + agents.
    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("stats-proj".into()),
            Value::Text("/stats".into()),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert project");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("RedFox".into()),
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(1_000_000),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert sender");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("BlueLake".into()),
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(1_000_000),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert recipient");

    // Insert a message with ack_required=1.
    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, created_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("Urgent task".into()),
            Value::Text("Please ack".into()),
            Value::Text("high".into()),
            Value::BigInt(1),
            Value::BigInt(5_000_000),
        ],
    )
    .expect("insert ack-required message");

    // Add recipient (agent_id=2 is BlueLake).
    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[Value::BigInt(1), Value::BigInt(2), Value::Text("to".into())],
    )
    .expect("insert recipient link");

    // Check inbox_stats: should have total=1, unread=1, ack_pending=1.
    let rows = conn
        .query_sync(
            "SELECT total_count, unread_count, ack_pending_count FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("query inbox_stats");
    assert_eq!(rows.len(), 1, "inbox_stats should have a row for agent 2");
    assert_eq!(rows[0].get_named::<i64>("total_count").unwrap_or(0), 1);
    assert_eq!(rows[0].get_named::<i64>("unread_count").unwrap_or(0), 1);
    assert_eq!(
        rows[0].get_named::<i64>("ack_pending_count").unwrap_or(0),
        1
    );

    // Mark as read.
    conn.execute_sync(
        "UPDATE message_recipients SET read_ts = ? WHERE message_id = 1 AND agent_id = 2",
        &[Value::BigInt(6_000_000)],
    )
    .expect("mark read");

    let rows = conn
        .query_sync(
            "SELECT unread_count FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("query unread after mark read");
    assert_eq!(
        rows[0].get_named::<i64>("unread_count").unwrap_or(-1),
        0,
        "unread should decrement to 0 after mark read"
    );

    // Acknowledge.
    conn.execute_sync(
        "UPDATE message_recipients SET ack_ts = ? WHERE message_id = 1 AND agent_id = 2",
        &[Value::BigInt(7_000_000)],
    )
    .expect("acknowledge");

    let rows = conn
        .query_sync(
            "SELECT ack_pending_count FROM inbox_stats WHERE agent_id = 2",
            &[],
        )
        .expect("query ack_pending after ack");
    assert_eq!(
        rows[0].get_named::<i64>("ack_pending_count").unwrap_or(-1),
        0,
        "ack_pending should decrement to 0 after acknowledgement"
    );
}

// NOTE: v7_identity_fts_backfill_from_preexisting_data removed — identity FTS
// tables and triggers dropped by v11 migrations (br-2tnl.8.4).
// Tantivy handles full-text search for agents and projects now.

// NOTE: v5_fts_porter_stemming_and_prefix_search removed — FTS5 decommissioned
// in v11 migration (br-2tnl.8.4).  Tantivy handles stemming/prefix search.

// ---------------------------------------------------------------------------
// 13. Partial migration (incremental upgrade)
// ---------------------------------------------------------------------------

#[test]
fn incremental_migration_from_partial_schema() {
    let (conn, _dir) = open_temp_db();

    conn.execute_raw(PRAGMA_SETTINGS_SQL)
        .expect("apply PRAGMAs");

    // Simulate a DB that already had v1 tables created manually (e.g. older version).
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
        &[],
    )
    .expect("create projects manually");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL DEFAULT '', inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', UNIQUE(project_id, name))",
        &[],
    )
    .expect("create agents manually");

    // Insert some data so we can verify it survives migration.
    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("existing".into()),
            Value::Text("/existing".into()),
            Value::BigInt(42_000_000),
        ],
    )
    .expect("insert existing project");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("OldAgent".into()),
            Value::Text("legacy".into()),
            Value::Text("old-model".into()),
            Value::BigInt(42_000_000),
            Value::BigInt(42_000_000),
        ],
    )
    .expect("insert existing agent");

    // Run migrations -- should create missing tables without disturbing existing data.
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        !applied.is_empty(),
        "should apply some migrations on partial schema"
    );

    // Existing data should still be there.
    let rows = conn
        .query_sync("SELECT slug FROM projects", &[])
        .expect("query projects");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_named::<String>("slug").unwrap_or_default(),
        "existing"
    );

    let rows = conn
        .query_sync("SELECT name FROM agents", &[])
        .expect("query agents");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_named::<String>("name").unwrap_or_default(),
        "OldAgent"
    );

    // New tables should exist.
    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name = 'inbox_stats'",
            &[],
        )
        .expect("check inbox_stats");
    assert_eq!(rows.len(), 1, "inbox_stats should have been created");
}

// ---------------------------------------------------------------------------
// 14. Migration count consistency
// ---------------------------------------------------------------------------

#[test]
fn schema_migrations_count_is_consistent() {
    let migrations = schema::schema_migrations();

    // All migration IDs should be unique.
    let mut ids: Vec<String> = migrations.iter().map(|m| m.id.clone()).collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "migration IDs must be unique; found {} duplicates",
        total - ids.len()
    );

    // Verify that all expected version prefixes (v1 through v8) are present.
    // Note: ordering is not strictly monotonic by version number because
    // schema_migrations() interleaves v2 (drop legacy triggers) before v1
    // (create triggers) due to the split between CREATE_TABLES_SQL and
    // CREATE_FTS_TRIGGERS_SQL processing order.
    let mut version_set = std::collections::BTreeSet::new();
    for m in &migrations {
        if let Some(v_str) = m.id.strip_prefix('v')
            && let Some(num_str) = v_str.split('_').next()
            && let Ok(v) = num_str.parse::<u32>()
        {
            version_set.insert(v);
        }
    }
    for expected_v in 1..=8 {
        assert!(
            version_set.contains(&expected_v),
            "expected migrations with version prefix v{expected_v}_, found only: {version_set:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 15. Data survives full migration roundtrip
// ---------------------------------------------------------------------------

#[test]
fn data_survives_complete_migration_roundtrip() {
    let (conn, _dir) = open_temp_db();

    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Insert a full set of test data.
    conn.execute_sync(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        &[
            Value::Text("roundtrip".into()),
            Value::Text("/roundtrip".into()),
            Value::BigInt(1_000_000),
        ],
    )
    .expect("insert project");

    conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("RedFox".into()),
            Value::Text("claude-code".into()),
            Value::Text("opus".into()),
            Value::Text("roundtrip testing".into()),
            Value::BigInt(2_000_000),
            Value::BigInt(3_000_000),
        ],
    )
    .expect("insert agent");

    conn.execute_sync(
        "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("TKT-001".into()),
            Value::Text("Roundtrip subject".into()),
            Value::Text("Roundtrip body text".into()),
            Value::Text("high".into()),
            Value::BigInt(1),
            Value::BigInt(4_000_000),
        ],
    )
    .expect("insert message");

    conn.execute_sync(
        "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[Value::BigInt(1), Value::BigInt(1), Value::Text("to".into())],
    )
    .expect("insert recipient");

    conn.execute_sync(
        "INSERT INTO file_reservations (project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::BigInt(1),
            Value::Text("src/*.rs".into()),
            Value::BigInt(1),
            Value::Text("editing".into()),
            Value::BigInt(5_000_000),
            Value::BigInt(6_000_000),
        ],
    )
    .expect("insert file reservation");

    // Re-run migrations (should be no-op).
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(applied.is_empty(), "re-running migrations should be no-op");

    // Verify all data is intact.
    let rows = conn
        .query_sync(
            "SELECT slug, human_key, created_at FROM projects WHERE slug = 'roundtrip'",
            &[],
        )
        .expect("query project");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_named::<i64>("created_at").unwrap(), 1_000_000);

    let rows = conn
        .query_sync(
            "SELECT name, task_description FROM agents WHERE name = 'RedFox'",
            &[],
        )
        .expect("query agent");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .get_named::<String>("task_description")
            .unwrap_or_default(),
        "roundtrip testing"
    );

    let rows = conn
        .query_sync(
            "SELECT subject, importance, ack_required FROM messages WHERE thread_id = 'TKT-001'",
            &[],
        )
        .expect("query message");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .get_named::<String>("importance")
            .unwrap_or_default(),
        "high"
    );
    assert_eq!(rows[0].get_named::<i64>("ack_required").unwrap_or(0), 1);

    let rows = conn
        .query_sync("SELECT path_pattern, reason FROM file_reservations", &[])
        .expect("query file reservations");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .get_named::<String>("path_pattern")
            .unwrap_or_default(),
        "src/*.rs"
    );
    assert_eq!(
        rows[0].get_named::<String>("reason").unwrap_or_default(),
        "editing"
    );

    // FTS tables are dropped by v11 migration — no FTS assertions needed.
    // Tantivy handles full-text search now.

    // Inbox stats should have been updated by the recipient insert trigger.
    let rows = conn
        .query_sync(
            "SELECT total_count, unread_count, ack_pending_count FROM inbox_stats WHERE agent_id = 1",
            &[],
        )
        .expect("query inbox_stats");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_named::<i64>("total_count").unwrap_or(0), 1);
    assert_eq!(rows[0].get_named::<i64>("unread_count").unwrap_or(0), 1);
    assert_eq!(
        rows[0].get_named::<i64>("ack_pending_count").unwrap_or(0),
        1
    );
}

/// Test whether `FrankenConnection` corrupts a DB created by `SqliteConnection`.
/// Reproduces the "malformed database schema (`agent_links`) - duplicate column name" error.
#[test]
fn frankenconnection_does_not_corrupt_schema() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_corruption.db");
    let path_str = db_path.display().to_string();

    // 1. Create DB with SqliteConnection (base schema, no FTS5/triggers)
    let conn = SqliteConnection::open_file(&path_str).expect("open sqlite");
    conn.execute_raw(&schema::init_schema_sql_base())
        .expect("init base schema");
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test', '/tmp/test', 0)",
        &[],
    )
    .expect("insert project");
    drop(conn);

    // 2. Verify schema is valid + dump sqlite_master
    let conn = SqliteConnection::open_file(&path_str).expect("reopen sqlite");
    conn.query_sync("SELECT * FROM agent_links", &[])
        .expect("query agent_links before FrankenConnection");

    // 2b. Dump sqlite_master schema SQL BEFORE FrankenConnection
    let schema_before: Vec<String> = conn
        .query_sync(
            "SELECT type, name, sql FROM sqlite_master ORDER BY rowid",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            let typ: String = r.get_named("type").unwrap_or_default();
            let name: String = r.get_named("name").unwrap_or_default();
            let sql: String = r.get_named("sql").unwrap_or_default();
            format!("{typ}: {name} => {}", &sql[..sql.len().min(100)])
        })
        .collect();
    eprintln!(
        "BEFORE FrankenConnection ({} entries):",
        schema_before.len()
    );
    for s in &schema_before {
        eprintln!("  {s}");
    }
    drop(conn);

    // 3. Open with FrankenConnection and do a write
    let fconn = DbConn::open_file(&path_str).expect("open franken");
    fconn
        .execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (2, 'test2', '/tmp/test2', 0)",
            &[],
        )
        .expect("franken insert");
    drop(fconn);

    // 4. Re-verify with SqliteConnection — schema should still be valid
    let conn = SqliteConnection::open_file(&path_str).expect("reopen sqlite after franken");

    // 4a. Dump sqlite_master schema SQL AFTER FrankenConnection
    match conn.query_sync(
        "SELECT type, name, sql FROM sqlite_master ORDER BY rowid",
        &[],
    ) {
        Ok(rows) => {
            eprintln!("AFTER FrankenConnection ({} entries):", rows.len());
            for r in &rows {
                let typ: String = r.get_named("type").unwrap_or_default();
                let name: String = r.get_named("name").unwrap_or_default();
                let sql: String = r.get_named("sql").unwrap_or_default();
                eprintln!("  {typ}: {name} => {}", &sql[..sql.len().min(200)]);
            }
        }
        Err(e) => eprintln!("AFTER: Failed to read sqlite_master: {e}"),
    }

    match conn.query_sync("SELECT * FROM agent_links", &[]) {
        Ok(_) => {} // Schema is fine
        Err(e) => panic!("FrankenConnection corrupted DB schema: {e}"),
    }
    conn.execute_sync("UPDATE projects SET slug = 'updated' WHERE id = 1", &[])
        .expect("update after franken should work");
}

/// Does `FrankenConnection` corrupt even without writing (just open + close)?
#[test]
fn frankenconnection_open_close_no_write() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_nowrite.db");
    let path_str = db_path.display().to_string();

    // Create DB with our schema
    let conn = SqliteConnection::open_file(&path_str).expect("open");
    conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test', '/tmp/test', 0)",
        &[],
    )
    .unwrap();
    drop(conn);

    // Just open and close FrankenConnection without writing
    let fconn = DbConn::open_file(&path_str).expect("franken open");
    drop(fconn);

    // Check if schema is still valid
    let conn = SqliteConnection::open_file(&path_str).expect("reopen");
    match conn.query_sync("SELECT * FROM agent_links", &[]) {
        Ok(_) => eprintln!("open_close_no_write: schema OK"),
        Err(e) => panic!("FrankenConnection corrupted schema just by opening: {e}"),
    }
}

/// Does corruption happen with AUTOINCREMENT specifically?
#[test]
fn frankenconnection_autoincrement_write() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");

    // Test A: Write to table WITHOUT AUTOINCREMENT (manual ID)
    {
        let db_path = dir.path().join("test_no_autoincrement.db");
        let path_str = db_path.display().to_string();
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
        drop(conn);

        let fconn = DbConn::open_file(&path_str).expect("franken open");
        // Insert with explicit id, bypassing sqlite_sequence
        fconn
            .execute_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (999, 'x', '/x', 0)",
                &[],
            )
            .unwrap();
        drop(fconn);

        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        match conn.query_sync("SELECT * FROM agent_links", &[]) {
            Ok(_) => eprintln!("no_autoincrement (explicit id): schema OK"),
            Err(e) => eprintln!("no_autoincrement (explicit id): CORRUPTED: {e}"),
        }
    }

    // Test B: Write to AUTOINCREMENT table (triggers sqlite_sequence update)
    {
        let db_path = dir.path().join("test_autoincrement.db");
        let path_str = db_path.display().to_string();
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
        drop(conn);

        let fconn = DbConn::open_file(&path_str).expect("franken open");
        // Insert without explicit id — triggers AUTOINCREMENT / sqlite_sequence
        fconn
            .execute_sync(
                "INSERT INTO projects (slug, human_key, created_at) VALUES ('y', '/y', 0)",
                &[],
            )
            .unwrap();
        drop(fconn);

        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        match conn.query_sync("SELECT * FROM agent_links", &[]) {
            Ok(_) => eprintln!("autoincrement: schema OK"),
            Err(e) => eprintln!("autoincrement: CORRUPTED: {e}"),
        }
    }

    // Test C: Write to non-AUTOINCREMENT table
    {
        let db_path = dir.path().join("test_no_ai_table.db");
        let path_str = db_path.display().to_string();
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
        // Pre-populate required references
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 't', '/t', 0)",
            &[],
        )
        .unwrap();
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (1, 1, 'GreenCastle', 'test', 'test', '', 0, 0, 'auto', 'auto')",
            &[],
        ).unwrap();
        drop(conn);

        let fconn = DbConn::open_file(&path_str).expect("franken open");
        // message_recipients has no AUTOINCREMENT
        fconn
            .execute_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (1, 1, 'to')",
                &[],
            )
            .unwrap_or_default(); // May fail on FK but we care about corruption
        drop(fconn);

        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        match conn.query_sync("SELECT * FROM agent_links", &[]) {
            Ok(_) => eprintln!("non-AI table write: schema OK"),
            Err(e) => eprintln!("non-AI table write: CORRUPTED: {e}"),
        }
    }
}

/// Does `FrankenConnection` corrupt with a read-only query?
#[test]
fn frankenconnection_read_only_no_corruption() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_readonly.db");
    let path_str = db_path.display().to_string();

    // Create DB with our schema
    let conn = SqliteConnection::open_file(&path_str).expect("open");
    conn.execute_raw(&schema::init_schema_sql_base()).unwrap();
    conn.execute_sync(
        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test', '/tmp/test', 0)",
        &[],
    )
    .unwrap();
    drop(conn);

    // Open FrankenConnection and only do reads
    let fconn = DbConn::open_file(&path_str).expect("franken open");
    let _rows = fconn
        .query_sync("SELECT id, slug FROM projects", &[])
        .unwrap();
    drop(fconn);

    // Check if schema is still valid
    let conn = SqliteConnection::open_file(&path_str).expect("reopen");
    match conn.query_sync("SELECT * FROM agent_links", &[]) {
        Ok(_) => eprintln!("read_only: schema OK"),
        Err(e) => panic!("FrankenConnection corrupted schema with read-only: {e}"),
    }
}

/// Minimal reproduction: does `FrankenConnection` corrupt even a tiny schema?
#[test]
fn frankenconnection_tiny_schema_no_corruption() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("test_tiny.db");
    let path_str = db_path.display().to_string();

    // 1. Create a tiny DB
    let conn = SqliteConnection::open_file(&path_str).expect("open");
    conn.execute_raw("CREATE TABLE t1 (id INTEGER PRIMARY KEY, val TEXT)")
        .unwrap();
    conn.execute_raw("CREATE TABLE t2 (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)")
        .unwrap();
    conn.execute_sync("INSERT INTO t1 VALUES (1, 'hello')", &[])
        .unwrap();
    drop(conn);

    // 2. FrankenConnection writes
    let fconn = DbConn::open_file(&path_str).expect("franken open");
    fconn
        .execute_sync("INSERT INTO t1 VALUES (2, 'world')", &[])
        .unwrap();
    drop(fconn);

    // 3. SqliteConnection verifies
    let conn = SqliteConnection::open_file(&path_str).expect("reopen");
    let rows = conn.query_sync("SELECT * FROM t1", &[]).unwrap();
    assert_eq!(rows.len(), 2, "expected 2 rows");
    conn.query_sync("SELECT * FROM t2", &[])
        .expect("t2 schema should be intact");
}

/// Test with progressively more tables to find corruption threshold.
#[test]
fn frankenconnection_many_tables_corruption_threshold() {
    use mcp_agent_mail_db::DbConn;

    let dir = tempfile::tempdir().expect("tempdir");

    for n_tables in [2, 5, 8, 10, 12] {
        let db_path = dir.path().join(format!("test_{n_tables}.db"));
        let path_str = db_path.display().to_string();

        // Create DB with N tables
        let conn = SqliteConnection::open_file(&path_str).expect("open");
        for i in 0..n_tables {
            let sql = format!(
                "CREATE TABLE table_{i} (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 col_a INTEGER NOT NULL, col_b TEXT NOT NULL DEFAULT '', col_c INTEGER)"
            );
            conn.execute_raw(&sql).unwrap();
        }
        conn.execute_sync("INSERT INTO table_0 (col_a, col_b) VALUES (1, 'test')", &[])
            .unwrap();
        drop(conn);

        // FrankenConnection writes
        let fconn = DbConn::open_file(&path_str).expect("franken open");
        fconn
            .execute_sync(
                "INSERT INTO table_0 (col_a, col_b) VALUES (2, 'world')",
                &[],
            )
            .unwrap();
        drop(fconn);

        // SqliteConnection verifies ALL tables
        let conn = SqliteConnection::open_file(&path_str).expect("reopen");
        for i in 0..n_tables {
            match conn.execute_sync(&format!("SELECT count(*) FROM table_{i}"), &[]) {
                Ok(_) => {}
                Err(e) => panic!("Corruption at {n_tables} tables, table_{i}: {e}"),
            }
        }
        // Also check via sqlite_master
        match conn.query_sync("SELECT count(*) FROM sqlite_master WHERE type='table'", &[]) {
            Ok(_) => eprintln!("OK: {n_tables} tables, no corruption"),
            Err(e) => panic!("sqlite_master corrupted at {n_tables} tables: {e}"),
        }
    }
}

// ===========================================================================
// v10a/v10b migration tests — Doom Loop Fix Test Coverage (br-3h13.16.1)
// ===========================================================================

/// Helper: create a pre-v10 agents table (case-sensitive UNIQUE constraint)
/// and a projects table, then insert agents manually. Returns an open
/// `SqliteConnection` ready for `migrate_to_latest`.
fn setup_pre_v10_db_with_agents(
    agents: &[(i64, &str, i64)], // (project_id, name, explicit_id)
) -> (SqliteConnection, tempfile::TempDir) {
    let (conn, dir) = open_temp_db();

    conn.execute_raw(schema::PRAGMA_SETTINGS_SQL)
        .expect("apply PRAGMAs");

    // Create pre-v10 schema: projects + agents with case-sensitive UNIQUE.
    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS projects (\
            id INTEGER PRIMARY KEY AUTOINCREMENT,\
            slug TEXT NOT NULL UNIQUE,\
            human_key TEXT NOT NULL,\
            created_at INTEGER NOT NULL\
        )",
        &[],
    )
    .expect("create projects");

    conn.execute_sync(
        "CREATE TABLE IF NOT EXISTS agents (\
            id INTEGER PRIMARY KEY AUTOINCREMENT,\
            project_id INTEGER NOT NULL REFERENCES projects(id),\
            name TEXT NOT NULL,\
            program TEXT NOT NULL,\
            model TEXT NOT NULL,\
            task_description TEXT NOT NULL DEFAULT '',\
            inception_ts INTEGER NOT NULL,\
            last_active_ts INTEGER NOT NULL,\
            attachments_policy TEXT NOT NULL DEFAULT 'auto',\
            contact_policy TEXT NOT NULL DEFAULT 'auto',\
            UNIQUE(project_id, name)\
        )",
        &[],
    )
    .expect("create agents");

    // Create index matching pre-v10 schema.
    conn.execute_sync(
        "CREATE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name)",
        &[],
    )
    .expect("create index");

    // Insert projects that are referenced by agents.
    let mut project_ids: Vec<i64> = agents.iter().map(|(pid, _, _)| *pid).collect();
    project_ids.sort_unstable();
    project_ids.dedup();
    for pid in &project_ids {
        conn.execute_sync(
            "INSERT OR IGNORE INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(*pid),
                Value::Text(format!("proj-{pid}")),
                Value::Text(format!("/proj/{pid}")),
                Value::BigInt(1_000_000),
            ],
        )
        .expect("insert project");
    }

    // Insert agents with explicit IDs to control ordering.
    for (project_id, name, agent_id) in agents {
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(*agent_id),
                Value::BigInt(*project_id),
                Value::Text(name.to_string()),
                Value::Text("test-program".into()),
                Value::Text("test-model".into()),
                Value::BigInt(*agent_id * 1_000_000),
                Value::BigInt(*agent_id * 1_000_000),
            ],
        )
        .unwrap_or_else(|e| panic!("insert agent '{name}' (id={agent_id}): {e}"));
    }

    (conn, dir)
}

// ---------------------------------------------------------------------------
// T16.1.1: Test v10a dedup fires on case-duplicate agents and keeps oldest
// (br-3h13.16.1.1)
// ---------------------------------------------------------------------------

#[test]
fn v10a_dedup_case_duplicate_agents_keeps_oldest() {
    // Setup: Two agents with same project_id but different-case names.
    // id=10 "SilverFox" (oldest), id=20 "silverfox" (newer), id=30 "SILVERFOX" (newest).
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[
        (1, "SilverFox", 10),
        (1, "silverfox", 20),
        (1, "SILVERFOX", 30),
    ]);

    // Run all migrations (v10a will dedup).
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Only 1 agent should remain for project_id=1.
    let rows = conn
        .query_sync(
            "SELECT id, name FROM agents WHERE project_id = 1 ORDER BY id",
            &[],
        )
        .expect("query agents after dedup");

    assert_eq!(
        rows.len(),
        1,
        "expected exactly 1 agent after v10a dedup, got {}",
        rows.len()
    );

    // The KEPT agent must be the one with the lowest id (oldest = id 10).
    let kept_id: i64 = rows[0].get_named("id").expect("get id");
    let kept_name: String = rows[0].get_named("name").expect("get name");
    assert_eq!(kept_id, 10, "should keep oldest agent (id=10)");
    assert_eq!(
        kept_name, "SilverFox",
        "should keep the name of the oldest agent"
    );
}

// ---------------------------------------------------------------------------
// T16.1.2: Test v10a migration is safe on empty agents table
// (br-3h13.16.1.2)
// ---------------------------------------------------------------------------

#[test]
fn v10a_safe_on_empty_agents_table() {
    // Setup: No agents at all.
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[]);

    // Run all migrations — should succeed without error.
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(
        applied
            .iter()
            .any(|id| id == "v10a_dedup_agents_case_insensitive"),
        "v10a migration should have been applied"
    );

    // Agents table still exists and is empty.
    let rows = conn
        .query_sync("SELECT COUNT(*) as cnt FROM agents", &[])
        .expect("count agents");
    let count: i64 = rows[0].get_named("cnt").unwrap_or(-1);
    assert_eq!(
        count, 0,
        "agents table should be empty after v10a on empty table"
    );

    // Idempotency: running again also succeeds.
    let applied2 = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });
    assert!(applied2.is_empty(), "re-running migrations should be no-op");
}

// ---------------------------------------------------------------------------
// T16.1.3: Test v10a preserves non-duplicate agents unchanged
// (br-3h13.16.1.3)
// ---------------------------------------------------------------------------

#[test]
fn v10a_preserves_non_duplicate_agents() {
    // Setup: 3 agents with unique names (no case collisions).
    let (conn, _dir) =
        setup_pre_v10_db_with_agents(&[(1, "Alice", 1), (1, "Bob", 2), (2, "Charlie", 3)]);

    // Run all migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // All 3 agents should still exist with unchanged data.
    let rows = conn
        .query_sync(
            "SELECT id, name, project_id, program, model FROM agents ORDER BY id",
            &[],
        )
        .expect("query all agents");

    assert_eq!(rows.len(), 3, "all 3 non-duplicate agents should survive");

    let names: Vec<String> = rows
        .iter()
        .map(|r| r.get_named::<String>("name").unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["Alice", "Bob", "Charlie"]);

    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.get_named::<i64>("id").unwrap_or(0))
        .collect();
    assert_eq!(ids, vec![1, 2, 3], "agent IDs should be unchanged");

    // Verify agent data (program, model) is also unchanged.
    for row in &rows {
        let program: String = row.get_named("program").unwrap_or_default();
        let model: String = row.get_named("model").unwrap_or_default();
        assert_eq!(program, "test-program", "program should be unchanged");
        assert_eq!(model, "test-model", "model should be unchanged");
    }
}

// ---------------------------------------------------------------------------
// T16.1.4: Test v10a cross-project isolation (same name, different projects kept)
// (br-3h13.16.1.4)
// ---------------------------------------------------------------------------

#[test]
fn v10a_cross_project_isolation_same_name_different_projects() {
    // Setup: Same name (different case) but in DIFFERENT projects.
    // These should NOT be deduped because GROUP BY includes project_id.
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[(1, "Alice", 1), (2, "alice", 2)]);

    // Run all migrations.
    block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    // Both agents should still exist (2 rows total).
    let rows = conn
        .query_sync("SELECT id, name, project_id FROM agents ORDER BY id", &[])
        .expect("query agents");

    assert_eq!(
        rows.len(),
        2,
        "cross-project agents with same name should both survive dedup"
    );

    let agent1_id: i64 = rows[0].get_named("id").unwrap_or(0);
    let agent1_proj: i64 = rows[0].get_named("project_id").unwrap_or(0);
    let agent2_id: i64 = rows[1].get_named("id").unwrap_or(0);
    let agent2_proj: i64 = rows[1].get_named("project_id").unwrap_or(0);

    assert_eq!(agent1_id, 1, "agent in project 1 should have id=1");
    assert_eq!(agent1_proj, 1, "first agent should be in project 1");
    assert_eq!(agent2_id, 2, "agent in project 2 should have id=2");
    assert_eq!(agent2_proj, 2, "second agent should be in project 2");
}

// ---------------------------------------------------------------------------
// T16.1.5: Test v10b index creation and uniqueness enforcement
// (br-3h13.16.1.5)
// ---------------------------------------------------------------------------

#[test]
fn v10b_index_creation_and_uniqueness_enforcement() {
    // Setup: Clean agents (no duplicates) so v10a is a no-op and v10b creates the index.
    let (conn, _dir) = setup_pre_v10_db_with_agents(&[
        (1, "Alice", 1),
        (1, "Bob", 2),
        (2, "Alice", 3), // Same name but different project — allowed.
    ]);

    // Run all migrations (v10a dedup + v10b index creation).
    let applied = block_on({
        let conn = &conn;
        move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
    });

    assert!(
        applied
            .iter()
            .any(|id| id == "v10b_idx_agents_project_name_nocase"),
        "v10b migration should have been applied"
    );

    // Verify index exists via PRAGMA index_list.
    let rows = conn
        .query_sync("PRAGMA index_list(agents)", &[])
        .expect("query index_list");

    let index_names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();
    assert!(
        index_names.contains(&"idx_agents_project_name_nocase".to_string()),
        "idx_agents_project_name_nocase should exist in {index_names:?}"
    );

    // Verify the index is UNIQUE (origin = 'u' in PRAGMA index_list).
    let unique_flag: Option<i64> = rows
        .iter()
        .find(|r| {
            r.get_named::<String>("name").unwrap_or_default() == "idx_agents_project_name_nocase"
        })
        .and_then(|r| r.get_named::<i64>("unique").ok());
    assert_eq!(
        unique_flag,
        Some(1),
        "idx_agents_project_name_nocase should be UNIQUE"
    );

    // Verify uniqueness enforcement: inserting a case-duplicate should FAIL.
    let result = conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
         VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(1),
            Value::Text("alice".into()), // "Alice" already exists in project 1
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(99_000_000),
            Value::BigInt(99_000_000),
        ],
    );
    assert!(
        result.is_err(),
        "inserting case-duplicate 'alice' when 'Alice' exists in same project should fail with UNIQUE constraint"
    );

    // Verify cross-project insertion still works: "bob" in project 2 should succeed.
    let result = conn.execute_sync(
        "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) \
         VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Value::BigInt(2),
            Value::Text("bob".into()), // "Bob" only exists in project 1, not project 2
            Value::Text("test".into()),
            Value::Text("model".into()),
            Value::BigInt(100_000_000),
            Value::BigInt(100_000_000),
        ],
    );
    assert!(
        result.is_ok(),
        "inserting 'bob' in project 2 (only exists in project 1) should succeed"
    );

    // Verify final state: 4 agents total (3 original + 1 new cross-project).
    let rows = conn
        .query_sync("SELECT COUNT(*) as cnt FROM agents", &[])
        .expect("count agents");
    let count: i64 = rows[0].get_named("cnt").unwrap_or(0);
    assert_eq!(
        count, 4,
        "should have 4 agents total after cross-project insert"
    );
}
