//! Database schema creation and migrations
//!
//! Creates all tables, indexes, and FTS5 virtual tables.

use asupersync::{Cx, Outcome};
use sqlmodel_core::{Connection, Error as SqlError};
use sqlmodel_schema::{Migration, MigrationRunner, MigrationStatus};

// Schema creation SQL - no runtime dependencies needed

/// SQL statements for creating the database schema
pub const CREATE_TABLES_SQL: &str = r"
-- Projects table
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT NOT NULL UNIQUE,
    human_key TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_projects_slug ON projects(slug);
CREATE INDEX IF NOT EXISTS idx_projects_human_key ON projects(human_key);

-- Products table
CREATE TABLE IF NOT EXISTS products (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_uid TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_products_uid ON products(product_uid);
CREATE INDEX IF NOT EXISTS idx_products_name ON products(name);

-- Product-Project links (many-to-many)
CREATE TABLE IF NOT EXISTS product_project_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id),
    project_id INTEGER NOT NULL REFERENCES projects(id),
    created_at INTEGER NOT NULL,
    UNIQUE(product_id, project_id)
);

-- Agents table
CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    program TEXT NOT NULL,
    model TEXT NOT NULL,
    task_description TEXT NOT NULL DEFAULT '',
    inception_ts INTEGER NOT NULL,
    last_active_ts INTEGER NOT NULL,
    attachments_policy TEXT NOT NULL DEFAULT 'auto',
    contact_policy TEXT NOT NULL DEFAULT 'auto',
    UNIQUE(project_id, name)
);
CREATE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name);

-- Messages table
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    sender_id INTEGER NOT NULL REFERENCES agents(id),
    thread_id TEXT,
    subject TEXT NOT NULL,
    body_md TEXT NOT NULL,
    importance TEXT NOT NULL DEFAULT 'normal',
    ack_required INTEGER NOT NULL DEFAULT 0,
    created_ts INTEGER NOT NULL,
    attachments TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_messages_project_created ON messages(project_id, created_ts);
CREATE INDEX IF NOT EXISTS idx_messages_project_sender_created ON messages(project_id, sender_id, created_ts);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_importance ON messages(importance);
CREATE INDEX IF NOT EXISTS idx_messages_created_ts ON messages(created_ts);

-- Message recipients (many-to-many)
CREATE TABLE IF NOT EXISTS message_recipients (
    message_id INTEGER NOT NULL REFERENCES messages(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    kind TEXT NOT NULL DEFAULT 'to',
    read_ts INTEGER,
    ack_ts INTEGER,
    PRIMARY KEY(message_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_message_recipients_agent ON message_recipients(agent_id);
CREATE INDEX IF NOT EXISTS idx_message_recipients_agent_message ON message_recipients(agent_id, message_id);

-- File reservations table
CREATE TABLE IF NOT EXISTS file_reservations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    path_pattern TEXT NOT NULL,
    exclusive INTEGER NOT NULL DEFAULT 1,
    reason TEXT NOT NULL DEFAULT '',
    created_ts INTEGER NOT NULL,
    expires_ts INTEGER NOT NULL,
    released_ts INTEGER
);
CREATE INDEX IF NOT EXISTS idx_file_reservations_project_released_expires ON file_reservations(project_id, released_ts, expires_ts);
CREATE INDEX IF NOT EXISTS idx_file_reservations_project_agent_released ON file_reservations(project_id, agent_id, released_ts);
CREATE INDEX IF NOT EXISTS idx_file_reservations_expires_ts ON file_reservations(expires_ts);

-- Agent links (contact relationships)
CREATE TABLE IF NOT EXISTS agent_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    a_project_id INTEGER NOT NULL REFERENCES projects(id),
    a_agent_id INTEGER NOT NULL REFERENCES agents(id),
    b_project_id INTEGER NOT NULL REFERENCES projects(id),
    b_agent_id INTEGER NOT NULL REFERENCES agents(id),
    status TEXT NOT NULL DEFAULT 'pending',
    reason TEXT NOT NULL DEFAULT '',
    created_ts INTEGER NOT NULL,
    updated_ts INTEGER NOT NULL,
    expires_ts INTEGER,
    UNIQUE(a_project_id, a_agent_id, b_project_id, b_agent_id)
);
CREATE INDEX IF NOT EXISTS idx_agent_links_a_project ON agent_links(a_project_id);
CREATE INDEX IF NOT EXISTS idx_agent_links_b_project ON agent_links(b_project_id);
CREATE INDEX IF NOT EXISTS idx_agent_links_status ON agent_links(status);

-- Project sibling suggestions
CREATE TABLE IF NOT EXISTS project_sibling_suggestions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_a_id INTEGER NOT NULL REFERENCES projects(id),
    project_b_id INTEGER NOT NULL REFERENCES projects(id),
    score REAL NOT NULL,
    status TEXT NOT NULL DEFAULT 'suggested',
    rationale TEXT NOT NULL DEFAULT '',
    created_ts INTEGER NOT NULL,
    evaluated_ts INTEGER NOT NULL,
    confirmed_ts INTEGER,
    dismissed_ts INTEGER,
    UNIQUE(project_a_id, project_b_id)
);

-- FTS5 virtual table for message search
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
    message_id UNINDEXED,
    subject,
    body
);
";

/// SQL for FTS triggers
pub const CREATE_FTS_TRIGGERS_SQL: &str = r"
-- Insert trigger for FTS
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO fts_messages(message_id, subject, body)
    VALUES (NEW.id, NEW.subject, NEW.body_md);
END;

-- Delete trigger for FTS
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    DELETE FROM fts_messages WHERE message_id = OLD.id;
END;

-- Update trigger for FTS
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    DELETE FROM fts_messages WHERE message_id = OLD.id;
    INSERT INTO fts_messages(message_id, subject, body)
    VALUES (NEW.id, NEW.subject, NEW.body_md);
END;
";

/// SQL for WAL mode and performance settings.
///
/// Per-connection PRAGMAs matching legacy Python `db.py` event listeners.
///
/// - `journal_mode=WAL`: readers never block writers; writers never block readers
/// - `synchronous=NORMAL`: fsync on commit (not per-statement); safe with WAL
/// - `busy_timeout=60s`: 60 second wait for locks (matches Python `PRAGMA busy_timeout=60000`)
/// - `wal_autocheckpoint=2000`: fewer checkpoints under sustained write bursts
/// - `cache_size=64MB`: large page cache to avoid disk reads for hot data
/// - `mmap_size=512MB`: memory-mapped I/O for sequential scan acceleration
/// - `temp_store=MEMORY`: temp tables and indices stay in RAM (never hit disk)
/// - `threads=4`: allow `SQLite` to parallelize sorting and other internal work
pub const PRAGMA_SETTINGS_SQL: &str = r"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 60000;
PRAGMA wal_autocheckpoint = 2000;
PRAGMA cache_size = -65536;
PRAGMA mmap_size = 536870912;
PRAGMA temp_store = MEMORY;
PRAGMA threads = 4;
";

/// Initialize the database schema
#[must_use]
pub fn init_schema_sql() -> String {
    format!("{PRAGMA_SETTINGS_SQL}\n{CREATE_TABLES_SQL}\n{CREATE_FTS_TRIGGERS_SQL}")
}

/// Schema version for migrations
pub const SCHEMA_VERSION: i32 = 1;

/// Name of the schema migration tracking table.
///
/// Stored in the same `SQLite` database as the rest of Agent Mail data.
pub const MIGRATIONS_TABLE_NAME: &str = "mcp_agent_mail_migrations";

fn extract_ident_after_keyword(stmt: &str, keyword_lc: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    let idx = lower.find(keyword_lc)?;
    let after = stmt[idx + keyword_lc.len()..].trim_start();
    let end = after
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(after.len());
    let ident = after[..end].trim();
    if ident.is_empty() {
        None
    } else {
        Some(ident.to_string())
    }
}

fn derive_migration_id_and_description(stmt: &str) -> Option<(String, String)> {
    const CREATE_TABLE: &str = "create table if not exists ";
    const CREATE_INDEX: &str = "create index if not exists ";
    const CREATE_VIRTUAL_TABLE: &str = "create virtual table if not exists ";
    const CREATE_TRIGGER: &str = "create trigger if not exists ";

    if let Some(name) = extract_ident_after_keyword(stmt, CREATE_TABLE) {
        return Some((
            format!("v1_create_table_{name}"),
            format!("create table {name}"),
        ));
    }
    if let Some(name) = extract_ident_after_keyword(stmt, CREATE_INDEX) {
        return Some((
            format!("v1_create_index_{name}"),
            format!("create index {name}"),
        ));
    }
    if let Some(name) = extract_ident_after_keyword(stmt, CREATE_VIRTUAL_TABLE) {
        return Some((
            format!("v1_create_virtual_table_{name}"),
            format!("create virtual table {name}"),
        ));
    }
    if let Some(name) = extract_ident_after_keyword(stmt, CREATE_TRIGGER) {
        return Some((
            format!("v1_create_trigger_{name}"),
            format!("create trigger {name}"),
        ));
    }

    None
}

fn extract_trigger_statements(sql: &str) -> Vec<&str> {
    let lower = sql.to_ascii_lowercase();
    let mut starts: Vec<usize> = Vec::new();
    let mut pos: usize = 0;
    while let Some(rel) = lower[pos..].find("create trigger if not exists") {
        let start = pos + rel;
        starts.push(start);
        pos = start + 1;
    }

    let mut out: Vec<&str> = Vec::new();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(sql.len());
        let stmt = sql[start..end].trim();
        if !stmt.is_empty() {
            out.push(stmt);
        }
    }
    out
}

/// Return the complete list of schema migrations.
///
/// Migrations are designed so each `up` is a single `SQLite` statement (compatible with
/// `sqlmodel_sqlite::SqliteConnection::execute_sync`, which only executes the first
/// prepared statement). Triggers are included as single `CREATE TRIGGER ... END;` statements.
#[must_use]
pub fn schema_migrations() -> Vec<Migration> {
    let mut migrations: Vec<Migration> = Vec::new();

    for chunk in CREATE_TABLES_SQL.split(';') {
        let stmt = chunk.trim();
        if stmt.is_empty() {
            continue;
        }

        let Some((id, desc)) = derive_migration_id_and_description(stmt) else {
            continue;
        };

        migrations.push(Migration::new(id, desc, stmt.to_string(), String::new()));
    }

    // Drop legacy Python FTS triggers that conflict with the Rust triggers below.
    // The Python schema created triggers named `fts_messages_ai/ad/au` while the Rust
    // schema uses `messages_ai/ad/au`. When both exist, every message INSERT fires two
    // FTS insert triggers, causing constraint failures on the FTS5 rowid.
    for (suffix, desc) in [
        ("ai", "drop legacy fts insert trigger"),
        ("ad", "drop legacy fts delete trigger"),
        ("au", "drop legacy fts update trigger"),
    ] {
        migrations.push(Migration::new(
            format!("v2_drop_legacy_fts_trigger_{suffix}"),
            desc.to_string(),
            format!("DROP TRIGGER IF EXISTS fts_messages_{suffix}"),
            String::new(),
        ));
    }

    for stmt in extract_trigger_statements(CREATE_FTS_TRIGGERS_SQL) {
        let Some((id, desc)) = derive_migration_id_and_description(stmt) else {
            continue;
        };
        migrations.push(Migration::new(id, desc, stmt.to_string(), String::new()));
    }

    // v3: Convert legacy Python TEXT timestamps to INTEGER (i64 microseconds).
    // The Python schema used SQLAlchemy DATETIME columns that store ISO-8601 strings
    // like "2026-02-04 22:13:11.079199", but the Rust port expects i64 microseconds.
    // The conversion: strftime('%s', text) * 1000000 + fractional_micros
    let ts_conversion = |col: &str| -> String {
        format!(
            "CAST(strftime('%s', {col}) AS INTEGER) * 1000000 + \
             CASE WHEN instr({col}, '.') > 0 \
                  THEN CAST(substr({col} || '000000', instr({col}, '.') + 1, 6) AS INTEGER) \
                  ELSE 0 \
             END"
        )
    };

    // projects.created_at
    migrations.push(Migration::new(
        "v3_fix_projects_text_timestamps".to_string(),
        "convert legacy TEXT created_at to INTEGER microseconds in projects".to_string(),
        format!(
            "UPDATE projects SET created_at = ({}) WHERE typeof(created_at) = 'text'",
            ts_conversion("created_at")
        ),
        String::new(),
    ));

    // agents.inception_ts + last_active_ts
    migrations.push(Migration::new(
        "v3_fix_agents_text_timestamps".to_string(),
        "convert legacy TEXT timestamps to INTEGER microseconds in agents".to_string(),
        format!(
            "UPDATE agents SET \
             inception_ts = CASE WHEN typeof(inception_ts) = 'text' THEN ({}) ELSE inception_ts END, \
             last_active_ts = CASE WHEN typeof(last_active_ts) = 'text' THEN ({}) ELSE last_active_ts END \
             WHERE typeof(inception_ts) = 'text' OR typeof(last_active_ts) = 'text'",
            ts_conversion("inception_ts"),
            ts_conversion("last_active_ts")
        ),
        String::new(),
    ));

    // messages.created_ts
    migrations.push(Migration::new(
        "v3_fix_messages_text_timestamps".to_string(),
        "convert legacy TEXT created_ts to INTEGER microseconds in messages".to_string(),
        format!(
            "UPDATE messages SET created_ts = ({}) WHERE typeof(created_ts) = 'text'",
            ts_conversion("created_ts")
        ),
        String::new(),
    ));

    // file_reservations.created_ts + expires_ts
    migrations.push(Migration::new(
        "v3_fix_file_reservations_text_timestamps".to_string(),
        "convert legacy TEXT timestamps to INTEGER microseconds in file_reservations".to_string(),
        format!(
            "UPDATE file_reservations SET \
             created_ts = CASE WHEN typeof(created_ts) = 'text' THEN ({}) ELSE created_ts END, \
             expires_ts = CASE WHEN typeof(expires_ts) = 'text' THEN ({}) ELSE expires_ts END \
             WHERE typeof(created_ts) = 'text' OR typeof(expires_ts) = 'text'",
            ts_conversion("created_ts"),
            ts_conversion("expires_ts")
        ),
        String::new(),
    ));

    migrations
}

#[must_use]
pub fn migration_runner() -> MigrationRunner {
    MigrationRunner::new(schema_migrations()).table_name(MIGRATIONS_TABLE_NAME)
}

pub async fn init_migrations_table<C: Connection>(cx: &Cx, conn: &C) -> Outcome<(), SqlError> {
    // Ensure duplicate inserts are ignored. Under concurrency, multiple connections may
    // attempt to record the same migration id; `ON CONFLICT IGNORE` prevents that from
    // becoming a fatal error during startup.
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS {MIGRATIONS_TABLE_NAME} (
            id TEXT PRIMARY KEY ON CONFLICT IGNORE,
            description TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        )"
    );
    conn.execute(cx, &sql, &[]).await.map(|_| ())
}

pub async fn migration_status<C: Connection>(
    cx: &Cx,
    conn: &C,
) -> Outcome<Vec<(String, MigrationStatus)>, SqlError> {
    match init_migrations_table(cx, conn).await {
        Outcome::Ok(()) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }
    migration_runner().status(cx, conn).await
}

pub async fn migrate_to_latest<C: Connection>(cx: &Cx, conn: &C) -> Outcome<Vec<String>, SqlError> {
    match init_migrations_table(cx, conn).await {
        Outcome::Ok(()) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }
    migration_runner().migrate(cx, conn).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use sqlmodel_sqlite::SqliteConnection;

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

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("migrations_apply.db");
        let conn = SqliteConnection::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        // First run applies all schema migrations.
        let applied = block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
        });
        assert!(
            !applied.is_empty(),
            "fresh DB should apply at least one migration"
        );

        // Second run is a no-op (already applied).
        let applied2 = block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
        });
        assert!(
            applied2.is_empty(),
            "second migrate call should be idempotent"
        );
    }

    #[test]
    fn migrations_preserve_existing_data() {
        use sqlmodel_core::Value;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("migrations_preserve.db");
        let conn = SqliteConnection::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        // Simulate an older DB with only `projects` table.
        conn.execute_raw(PRAGMA_SETTINGS_SQL)
            .expect("apply PRAGMAs");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
            &[],
        )
        .expect("create projects table");
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text("proj".to_string()),
                Value::Text("/abs/path".to_string()),
                Value::BigInt(123),
            ],
        )
        .expect("insert project row");

        // Migrating should not delete existing rows.
        block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
        });

        let rows = conn
            .query_sync("SELECT slug, human_key, created_at FROM projects", &[])
            .expect("query projects");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap_or_default(),
            "proj"
        );
    }

    #[test]
    fn v3_migration_converts_text_timestamps_to_integer() {
        use sqlmodel_core::Value;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("v3_text_ts.db");
        let conn = SqliteConnection::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        conn.execute_raw(PRAGMA_SETTINGS_SQL)
            .expect("apply PRAGMAs");

        // Simulate a legacy Python database with DATETIME timestamps (NUMERIC affinity).
        // Python/SQLAlchemy creates columns as DATETIME which stores ISO-8601 text strings.
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at DATETIME NOT NULL)",
            &[],
        ).expect("create legacy projects table");
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text("legacy-proj".to_string()),
                Value::Text("/data/legacy".to_string()),
                Value::Text("2026-02-04 22:13:11.079199".to_string()),
            ],
        )
        .expect("insert legacy project");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL DEFAULT '', inception_ts DATETIME NOT NULL, last_active_ts DATETIME NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', UNIQUE(project_id, name))",
            &[],
        ).expect("create legacy agents table");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("BlueLake".to_string()),
                Value::Text("claude-code".to_string()),
                Value::Text("opus".to_string()),
                Value::Text("2026-02-05 00:06:44.082288".to_string()),
                Value::Text("2026-02-05 01:30:00.000000".to_string()),
            ],
        ).expect("insert legacy agent");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL DEFAULT 'normal', ack_required INTEGER NOT NULL DEFAULT 0, created_ts DATETIME NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
            &[],
        ).expect("create legacy messages table");
        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, subject, body_md, created_ts) VALUES (?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("Hello".to_string()),
                Value::Text("Test body".to_string()),
                Value::Text("2026-02-04 22:15:00.500000".to_string()),
            ],
        ).expect("insert legacy message");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS file_reservations (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, path_pattern TEXT NOT NULL, exclusive INTEGER NOT NULL DEFAULT 1, reason TEXT NOT NULL DEFAULT '', created_ts DATETIME NOT NULL, expires_ts DATETIME NOT NULL, released_ts DATETIME)",
            &[],
        ).expect("create legacy file_reservations table");
        conn.execute_sync(
            "INSERT INTO file_reservations (project_id, agent_id, path_pattern, created_ts, expires_ts) VALUES (?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("src/**".to_string()),
                Value::Text("2026-02-04 22:20:00.123456".to_string()),
                Value::Text("2026-02-04 23:20:00.654321".to_string()),
            ],
        ).expect("insert legacy file_reservation");

        // Run migrations (v3 should convert TEXT timestamps).
        block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
        });

        // Verify projects.created_at is now INTEGER
        let rows = conn
            .query_sync(
                "SELECT typeof(created_at) as t, created_at FROM projects",
                &[],
            )
            .expect("query projects");
        assert_eq!(rows[0].get_named::<String>("t").unwrap(), "integer");
        let created_at: i64 = rows[0].get_named("created_at").unwrap();
        assert!(
            created_at > 1_700_000_000_000_000,
            "created_at should be microseconds: {created_at}"
        );

        // Verify agents timestamps are now INTEGER
        let rows = conn
            .query_sync(
                "SELECT typeof(inception_ts) as t1, typeof(last_active_ts) as t2 FROM agents",
                &[],
            )
            .expect("query agents");
        assert_eq!(rows[0].get_named::<String>("t1").unwrap(), "integer");
        assert_eq!(rows[0].get_named::<String>("t2").unwrap(), "integer");

        // Verify messages.created_ts is now INTEGER
        let rows = conn
            .query_sync("SELECT typeof(created_ts) as t FROM messages", &[])
            .expect("query messages");
        assert_eq!(rows[0].get_named::<String>("t").unwrap(), "integer");

        // Verify file_reservations timestamps are now INTEGER
        let rows = conn
            .query_sync(
                "SELECT typeof(created_ts) as t1, typeof(expires_ts) as t2 FROM file_reservations",
                &[],
            )
            .expect("query file_reservations");
        assert_eq!(rows[0].get_named::<String>("t1").unwrap(), "integer");
        assert_eq!(rows[0].get_named::<String>("t2").unwrap(), "integer");
    }

    #[test]
    fn corrupted_migrations_table_yields_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("migrations_corrupt.db");
        let conn = SqliteConnection::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        // Create a tracking table with the right name but wrong schema.
        conn.execute_sync(
            &format!("CREATE TABLE {MIGRATIONS_TABLE_NAME} (id INTEGER PRIMARY KEY)"),
            &[],
        )
        .expect("create corrupted migrations table");

        let outcome = block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await }
        });
        assert!(outcome.is_err(), "corrupted migrations table should error");
    }
}
