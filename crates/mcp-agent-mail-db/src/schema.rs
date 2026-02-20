//! Database schema creation and migrations
//!
//! Creates all tables, indexes, and FTS5 virtual tables.

use asupersync::{Cx, Outcome};
use crate::DbConn;
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
CREATE INDEX IF NOT EXISTS idx_msg_thread_created ON messages(thread_id, created_ts);
CREATE INDEX IF NOT EXISTS idx_msg_project_importance_created ON messages(project_id, importance, created_ts);

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
CREATE INDEX IF NOT EXISTS idx_mr_agent_ack ON message_recipients(agent_id, ack_ts);

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
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_links_pair_unique
    ON agent_links(a_project_id, a_agent_id, b_project_id, b_agent_id);
CREATE INDEX IF NOT EXISTS idx_agent_links_a_project ON agent_links(a_project_id);
CREATE INDEX IF NOT EXISTS idx_agent_links_b_project ON agent_links(b_project_id);
CREATE INDEX IF NOT EXISTS idx_agent_links_status ON agent_links(status);
CREATE INDEX IF NOT EXISTS idx_al_a_agent_status ON agent_links(a_project_id, a_agent_id, status);
CREATE INDEX IF NOT EXISTS idx_al_b_agent_status ON agent_links(b_project_id, b_agent_id, status);

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
-- Porter stemmer: run/running/runs → run. Unicode61: Unicode-aware tokenization.
-- remove_diacritics 2: normalize accented characters. prefix='2,3': fast prefix queries.
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
    message_id UNINDEXED,
    subject,
    body,
    tokenize='porter unicode61 remove_diacritics 2',
    prefix='2,3'
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
/// Legacy-style PRAGMAs matching the Python `db.py` on-connect behavior.
///
/// Note: some PRAGMAs are database-wide (notably `journal_mode`). In the Rust
/// server we apply `journal_mode=WAL` once per sqlite file during pool warmup
/// (see `mcp-agent-mail-db/src/pool.rs`) to avoid high-concurrency races where
/// multiple connections simultaneously attempt WAL/migrations.
///
/// - `journal_mode=WAL`: readers never block writers; writers never block readers
/// - `synchronous=NORMAL`: fsync on commit (not per-statement); safe with WAL
/// - `busy_timeout=60s`: 60 second wait for locks (matches Python `PRAGMA busy_timeout=60000`)
/// - `wal_autocheckpoint=2000`: fewer checkpoints under sustained write bursts
/// - `cache_size`: budget-aware, scales inversely with pool size (see [`build_conn_pragmas`])
/// - `mmap_size=256MB`: memory-mapped I/O for sequential scan acceleration
/// - `temp_store=MEMORY`: temp tables and indices stay in RAM (never hit disk)
/// - `threads=4`: allow `SQLite` to parallelize sorting and other internal work
/// - `journal_size_limit=64MB`: cap WAL file size to prevent unbounded growth
/// - `foreign_keys=OFF`: the statically linked `SQLite` is compiled with
///   `SQLITE_DEFAULT_FOREIGN_KEYS` which enables FK enforcement by default.
///   We must explicitly disable it because: (a) our schema uses `REFERENCES`
///   for documentation only, not for runtime enforcement; (b) FK checks on
///   every INSERT/UPDATE cause cascading failures when orphan data exists
///   (e.g. agents referencing deleted projects); (c) FK enforcement must be
///   the FIRST pragma since it is per-connection and must be set before any
///   DML.
pub const PRAGMA_SETTINGS_SQL: &str = r"
PRAGMA foreign_keys = OFF;
PRAGMA busy_timeout = 60000;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA wal_autocheckpoint = 2000;
PRAGMA cache_size = -8192;
PRAGMA mmap_size = 268435456;
PRAGMA temp_store = MEMORY;
PRAGMA threads = 4;
PRAGMA journal_size_limit = 67108864;
";

/// Database-wide initialization PRAGMAs (applied once per sqlite file).
pub const PRAGMA_DB_INIT_SQL: &str = r"
PRAGMA foreign_keys = OFF;
PRAGMA journal_mode = WAL;
";

/// Base-mode DB init PRAGMAs for files later opened by `FrankenConnection`.
///
/// WAL mode is intentionally avoided here to prevent mixed-runtime corruption
/// and malformed-image scenarios when the server process is terminated abruptly.
pub const PRAGMA_DB_INIT_BASE_SQL: &str = r"
PRAGMA foreign_keys = OFF;
PRAGMA journal_mode = 'DELETE';
";

/// Per-connection PRAGMAs (safe to run on every new connection).
///
/// IMPORTANT: `foreign_keys = OFF` must come first to override the
/// `SQLITE_DEFAULT_FOREIGN_KEYS` compile-time default before any DML.
/// `busy_timeout` comes next so lock waits apply to subsequent PRAGMAs.
pub const PRAGMA_CONN_SETTINGS_SQL: &str = r"
PRAGMA foreign_keys = OFF;
PRAGMA busy_timeout = 60000;
PRAGMA synchronous = NORMAL;
PRAGMA wal_autocheckpoint = 2000;
PRAGMA cache_size = -8192;
PRAGMA mmap_size = 268435456;
PRAGMA temp_store = MEMORY;
PRAGMA threads = 4;
PRAGMA journal_size_limit = 67108864;
";

/// Total memory budget (in KB) for page caches across all pooled connections.
///
/// Default: 512 MB. With 100 connections, each gets ~5 MB of page cache.
/// With 25 connections, each gets ~20 MB. This prevents memory blowup when
/// `max_connections` increases.
const TOTAL_CACHE_BUDGET_KB: usize = 512 * 1024;

/// Build per-connection PRAGMAs with a `cache_size` that respects the total
/// memory budget.
///
/// `max_connections` is the pool's maximum size. The per-connection cache
/// is `TOTAL_CACHE_BUDGET_KB / max_connections`, clamped to \[2 MB, 64 MB\].
///
/// Returns a SQL string suitable for `execute_raw()`.
#[must_use]
pub fn build_conn_pragmas(max_connections: usize) -> String {
    let per_conn_kb = (TOTAL_CACHE_BUDGET_KB
        .checked_div(max_connections)
        .unwrap_or(8192))
    .clamp(2048, 65536);

    format!(
        "\
PRAGMA foreign_keys = OFF;
PRAGMA busy_timeout = 60000;
PRAGMA synchronous = NORMAL;
PRAGMA wal_autocheckpoint = 2000;
PRAGMA cache_size = -{per_conn_kb};
PRAGMA mmap_size = 268435456;
PRAGMA temp_store = MEMORY;
PRAGMA threads = 4;
PRAGMA journal_size_limit = 67108864;
"
    )
}

/// Initialize the full database schema (tables, FTS5 virtual tables, triggers).
///
/// Prefer [`init_schema_sql_base`] for runtime DBs. The full schema keeps
/// legacy FTS objects for explicit export/compatibility paths only.
#[must_use]
pub fn init_schema_sql() -> String {
    format!("{PRAGMA_SETTINGS_SQL}\n{CREATE_TABLES_SQL}\n{CREATE_FTS_TRIGGERS_SQL}")
}

/// Initialize the base database schema without FTS5 virtual tables, triggers, or PRAGMAs.
///
/// Safe for databases that will be opened by `FrankenConnection` (pure-Rust `SQLite`).
/// PRAGMAs are intentionally excluded because:
/// - The pool applies per-connection PRAGMAs separately via [`build_conn_pragmas`]
/// - The pool's init gate applies base-safe DB init pragmas via
///   [`PRAGMA_DB_INIT_BASE_SQL`] before pooled connections open
///
/// Search queries automatically fall back to LIKE-based search when FTS5 tables are absent.
#[must_use]
pub fn init_schema_sql_base() -> String {
    // Strip the trailing FTS5 virtual table definition from CREATE_TABLES_SQL.
    // Everything before the "-- FTS5 virtual table" comment is base DDL.
    let base = CREATE_TABLES_SQL
        .find("-- FTS5 virtual table")
        .map_or(CREATE_TABLES_SQL, |idx| &CREATE_TABLES_SQL[..idx]);
    base.to_string()
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
/// `DbConn::execute_sync`, which only executes the first
/// prepared statement). Triggers are included as single `CREATE TRIGGER ... END;` statements.
#[must_use]
#[allow(clippy::too_many_lines)]
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

    // file_reservations.created_ts + expires_ts + released_ts
    migrations.push(Migration::new(
        "v3_fix_file_reservations_text_timestamps".to_string(),
        "convert legacy TEXT timestamps to INTEGER microseconds in file_reservations".to_string(),
        format!(
            "UPDATE file_reservations SET \
             created_ts = CASE WHEN typeof(created_ts) = 'text' THEN ({}) ELSE created_ts END, \
             expires_ts = CASE WHEN typeof(expires_ts) = 'text' THEN ({}) ELSE expires_ts END, \
             released_ts = CASE WHEN typeof(released_ts) = 'text' THEN ({}) ELSE released_ts END \
             WHERE typeof(created_ts) = 'text' OR typeof(expires_ts) = 'text' OR typeof(released_ts) = 'text'",
            ts_conversion("created_ts"),
            ts_conversion("expires_ts"),
            ts_conversion("released_ts")
        ),
        String::new(),
    ));

    // products.created_at
    migrations.push(Migration::new(
        "v3_fix_products_text_timestamps".to_string(),
        "convert legacy TEXT created_at to INTEGER microseconds in products".to_string(),
        format!(
            "UPDATE products SET created_at = ({}) WHERE typeof(created_at) = 'text'",
            ts_conversion("created_at")
        ),
        String::new(),
    ));

    // product_project_links.created_at
    migrations.push(Migration::new(
        "v3_fix_product_project_links_text_timestamps".to_string(),
        "convert legacy TEXT created_at to INTEGER microseconds in product_project_links"
            .to_string(),
        format!(
            "UPDATE product_project_links SET created_at = ({}) WHERE typeof(created_at) = 'text'",
            ts_conversion("created_at")
        ),
        String::new(),
    ));

    // ── v4: composite indexes for hot-path queries ──────────────────────
    // These cover the most frequent query patterns that previously required
    // full table scans or suboptimal single-column index usage.
    //
    // 1. message_recipients(agent_id, ack_ts) — ack-required / ack-overdue views
    //    Queries: list_unacknowledged_messages, fetch_unacked_for_agent
    migrations.push(Migration::new(
        "v4_idx_mr_agent_ack".to_string(),
        "composite index on message_recipients(agent_id, ack_ts) for ack views".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_mr_agent_ack ON message_recipients(agent_id, ack_ts)"
            .to_string(),
        String::new(),
    ));

    // 2. messages(thread_id, created_ts) — thread retrieval with ordering
    //    Queries: list_thread_messages, summarize_thread
    migrations.push(Migration::new(
        "v4_idx_msg_thread_created".to_string(),
        "composite index on messages(thread_id, created_ts) for thread queries".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_msg_thread_created ON messages(thread_id, created_ts)"
            .to_string(),
        String::new(),
    ));

    // 3. messages(project_id, importance, created_ts) — urgent-unread views
    //    Queries: fetch_inbox (urgent_only=true), views/urgent-unread resource
    migrations.push(Migration::new(
        "v4_idx_msg_project_importance_created".to_string(),
        "composite index on messages(project_id, importance, created_ts) for urgent views"
            .to_string(),
        "CREATE INDEX IF NOT EXISTS idx_msg_project_importance_created ON messages(project_id, importance, created_ts)"
            .to_string(),
        String::new(),
    ));

    // 4. agent_links(a_project_id, a_agent_id, status) — outgoing contact queries
    //    Queries: list_contacts (outgoing), list_approved_contact_ids, is_contact_allowed
    migrations.push(Migration::new(
        "v4_idx_al_a_agent_status".to_string(),
        "composite index on agent_links(a_project_id, a_agent_id, status) for contact queries"
            .to_string(),
        "CREATE INDEX IF NOT EXISTS idx_al_a_agent_status ON agent_links(a_project_id, a_agent_id, status)"
            .to_string(),
        String::new(),
    ));

    // 5. agent_links(b_project_id, b_agent_id, status) — incoming contact queries
    //    Queries: list_contacts (incoming), reverse contact lookups
    migrations.push(Migration::new(
        "v4_idx_al_b_agent_status".to_string(),
        "composite index on agent_links(b_project_id, b_agent_id, status) for reverse contact queries"
            .to_string(),
        "CREATE INDEX IF NOT EXISTS idx_al_b_agent_status ON agent_links(b_project_id, b_agent_id, status)"
            .to_string(),
        String::new(),
    ));

    // 6. ANALYZE to update query planner statistics after new indexes
    migrations.push(Migration::new(
        "v4_analyze_after_indexes".to_string(),
        "run ANALYZE to update query planner statistics for new indexes".to_string(),
        "ANALYZE".to_string(),
        String::new(),
    ));

    // ── v5: FTS5 tokenizer upgrade ──────────────────────────────────────
    // Rebuild FTS table with porter stemmer, unicode61, and prefix indexes.
    // This enables stemming (run/running → run), accent-insensitive search,
    // and fast prefix queries (migrat* → migration, migratable, ...).
    //
    // Step 1: Drop the old FTS table (triggers on `messages` are unaffected;
    // they will resume working once the new table is created in step 2).
    migrations.push(Migration::new(
        "v5_drop_fts_for_tokenizer_rebuild".to_string(),
        "drop old FTS5 table for tokenizer rebuild".to_string(),
        "DROP TABLE IF EXISTS fts_messages".to_string(),
        String::new(),
    ));

    // Step 2: Recreate with porter stemmer + unicode61 + prefix indexes.
    migrations.push(Migration::new(
        "v5_create_fts_with_porter".to_string(),
        "create FTS5 table with porter stemmer, unicode61, and prefix indexes".to_string(),
        "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(\
             message_id UNINDEXED, \
             subject, \
             body, \
             tokenize='porter unicode61 remove_diacritics 2', \
             prefix='2,3'\
         )"
        .to_string(),
        String::new(),
    ));

    // Step 3: Rebuild FTS content from existing messages.
    migrations.push(Migration::new(
        "v5_rebuild_fts_content".to_string(),
        "rebuild FTS5 content from messages table after tokenizer upgrade".to_string(),
        "INSERT INTO fts_messages(message_id, subject, body) \
         SELECT id, subject, body_md FROM messages"
            .to_string(),
        String::new(),
    ));

    // ── v6: Materialized inbox aggregate counters ───────────────────────
    // Maintain per-agent counters (total, unread, ack_pending) via SQLite
    // triggers so that inbox stats are always O(1) instead of scanning
    // message_recipients. Triggers fire within the same transaction as the
    // write, so counters are always consistent.

    // Step 1: Create the inbox_stats table.
    migrations.push(Migration::new(
        "v6_create_inbox_stats".to_string(),
        "create inbox_stats table for materialized aggregate counters".to_string(),
        "CREATE TABLE IF NOT EXISTS inbox_stats (\
             agent_id INTEGER PRIMARY KEY REFERENCES agents(id), \
             total_count INTEGER NOT NULL DEFAULT 0, \
             unread_count INTEGER NOT NULL DEFAULT 0, \
             ack_pending_count INTEGER NOT NULL DEFAULT 0, \
             last_message_ts INTEGER\
         )"
        .to_string(),
        String::new(),
    ));

    // Step 2: Trigger — after INSERT into message_recipients, increment counters.
    migrations.push(Migration::new(
        "v6_trg_inbox_stats_insert".to_string(),
        "trigger to increment inbox_stats on new message recipient".to_string(),
        "CREATE TRIGGER IF NOT EXISTS trg_inbox_stats_insert \
         AFTER INSERT ON message_recipients \
         BEGIN \
             INSERT INTO inbox_stats (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
             VALUES ( \
                 NEW.agent_id, \
                 1, \
                 1, \
                 (SELECT CASE WHEN m.ack_required = 1 THEN 1 ELSE 0 END FROM messages m WHERE m.id = NEW.message_id), \
                 (SELECT m.created_ts FROM messages m WHERE m.id = NEW.message_id) \
             ) \
             ON CONFLICT(agent_id) DO UPDATE SET \
                 total_count = total_count + 1, \
                 unread_count = unread_count + 1, \
                 ack_pending_count = ack_pending_count + \
                     (SELECT CASE WHEN m.ack_required = 1 THEN 1 ELSE 0 END FROM messages m WHERE m.id = NEW.message_id), \
                 last_message_ts = MAX(COALESCE(last_message_ts, 0), \
                     (SELECT m.created_ts FROM messages m WHERE m.id = NEW.message_id)); \
         END"
        .to_string(),
        String::new(),
    ));

    // Step 3: Trigger — after UPDATE of read_ts (mark read), decrement unread.
    migrations.push(Migration::new(
        "v6_trg_inbox_stats_mark_read".to_string(),
        "trigger to decrement unread_count when message marked read".to_string(),
        "CREATE TRIGGER IF NOT EXISTS trg_inbox_stats_mark_read \
         AFTER UPDATE OF read_ts ON message_recipients \
         WHEN OLD.read_ts IS NULL AND NEW.read_ts IS NOT NULL \
         BEGIN \
             UPDATE inbox_stats SET \
                 unread_count = MAX(0, unread_count - 1) \
             WHERE agent_id = NEW.agent_id; \
         END"
        .to_string(),
        String::new(),
    ));

    // Step 4: Trigger — after UPDATE of ack_ts (acknowledge), decrement ack_pending.
    migrations.push(Migration::new(
        "v6_trg_inbox_stats_ack".to_string(),
        "trigger to decrement ack_pending_count when message acknowledged".to_string(),
        "CREATE TRIGGER IF NOT EXISTS trg_inbox_stats_ack \
         AFTER UPDATE OF ack_ts ON message_recipients \
         WHEN OLD.ack_ts IS NULL AND NEW.ack_ts IS NOT NULL \
         BEGIN \
             UPDATE inbox_stats SET \
                 ack_pending_count = MAX(0, ack_pending_count - 1) \
             WHERE agent_id = NEW.agent_id; \
         END"
        .to_string(),
        String::new(),
    ));

    // Step 5: Backfill inbox_stats from existing data.
    migrations.push(Migration::new(
        "v6_backfill_inbox_stats".to_string(),
        "backfill inbox_stats from existing message_recipients data".to_string(),
        "INSERT OR REPLACE INTO inbox_stats (agent_id, total_count, unread_count, ack_pending_count, last_message_ts) \
         SELECT \
             r.agent_id, \
             COUNT(*) AS total_count, \
             SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END) AS unread_count, \
             SUM(CASE WHEN m.ack_required = 1 AND r.ack_ts IS NULL THEN 1 ELSE 0 END) AS ack_pending_count, \
             MAX(m.created_ts) AS last_message_ts \
         FROM message_recipients r \
         JOIN messages m ON m.id = r.message_id \
         GROUP BY r.agent_id"
            .to_string(),
        String::new(),
    ));

    // ── v7: Search corpus FTS for agents + projects ──────────────────────
    // Add lightweight identity indexes without paying write amplification costs
    // on high-churn columns (e.g. `agents.last_active_ts`).

    migrations.push(Migration::new(
        "v7_create_fts_agents".to_string(),
        "create fts_agents for agent identity search".to_string(),
        "CREATE VIRTUAL TABLE IF NOT EXISTS fts_agents USING fts5(\
             agent_id UNINDEXED, \
             project_id UNINDEXED, \
             name, \
             task_description, \
             program UNINDEXED, \
             model UNINDEXED, \
             tokenize='porter unicode61 remove_diacritics 2', \
             prefix='2,3'\
         )"
        .to_string(),
        String::new(),
    ));

    migrations.push(Migration::new(
        "v7_create_fts_projects".to_string(),
        "create fts_projects for project identity search".to_string(),
        "CREATE VIRTUAL TABLE IF NOT EXISTS fts_projects USING fts5(\
             project_id UNINDEXED, \
             slug, \
             human_key, \
             tokenize='porter unicode61 remove_diacritics 2', \
             prefix='2,3'\
         )"
        .to_string(),
        String::new(),
    ));

    // Agents -> fts_agents triggers
    migrations.push(Migration::new(
        "v7_trg_fts_agents_insert".to_string(),
        "trigger to insert fts_agents on new agents".to_string(),
        "CREATE TRIGGER IF NOT EXISTS agents_ai \
         AFTER INSERT ON agents \
	         BEGIN \
	             INSERT INTO fts_agents(rowid, agent_id, project_id, name, task_description, program, model) \
	             VALUES (NEW.id, NEW.id, NEW.project_id, NEW.name, NEW.task_description, NEW.program, NEW.model); \
	         END"
	        .to_string(),
	        String::new(),
	    ));
    migrations.push(Migration::new(
        "v7_trg_fts_agents_delete".to_string(),
        "trigger to delete fts_agents on agent delete".to_string(),
        "CREATE TRIGGER IF NOT EXISTS agents_ad \
	         AFTER DELETE ON agents \
	         BEGIN \
	             DELETE FROM fts_agents WHERE rowid = OLD.id; \
	         END"
        .to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v7_trg_fts_agents_update".to_string(),
        "trigger to update fts_agents when indexed agent fields change".to_string(),
        "CREATE TRIGGER IF NOT EXISTS agents_au \
         AFTER UPDATE OF name, task_description, program, model ON agents \
	         BEGIN \
	             DELETE FROM fts_agents WHERE rowid = OLD.id; \
	             INSERT INTO fts_agents(rowid, agent_id, project_id, name, task_description, program, model) \
	             VALUES (NEW.id, NEW.id, NEW.project_id, NEW.name, NEW.task_description, NEW.program, NEW.model); \
	         END"
	        .to_string(),
	        String::new(),
	    ));

    // Projects -> fts_projects triggers
    migrations.push(Migration::new(
        "v7_trg_fts_projects_insert".to_string(),
        "trigger to insert fts_projects on new projects".to_string(),
        "CREATE TRIGGER IF NOT EXISTS projects_ai \
         AFTER INSERT ON projects \
	         BEGIN \
	             INSERT INTO fts_projects(rowid, project_id, slug, human_key) \
	             VALUES (NEW.id, NEW.id, NEW.slug, NEW.human_key); \
	         END"
        .to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v7_trg_fts_projects_delete".to_string(),
        "trigger to delete fts_projects on project delete".to_string(),
        "CREATE TRIGGER IF NOT EXISTS projects_ad \
	         AFTER DELETE ON projects \
	         BEGIN \
	             DELETE FROM fts_projects WHERE rowid = OLD.id; \
	         END"
        .to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v7_trg_fts_projects_update".to_string(),
        "trigger to update fts_projects when indexed project fields change".to_string(),
        "CREATE TRIGGER IF NOT EXISTS projects_au \
         AFTER UPDATE OF slug, human_key ON projects \
	         BEGIN \
	             DELETE FROM fts_projects WHERE rowid = OLD.id; \
	             INSERT INTO fts_projects(rowid, project_id, slug, human_key) \
	             VALUES (NEW.id, NEW.id, NEW.slug, NEW.human_key); \
	         END"
        .to_string(),
        String::new(),
    ));

    // Backfill agent/project identity indexes from existing rows.
    migrations.push(Migration::new(
	        "v7_backfill_fts_agents".to_string(),
	        "backfill fts_agents from agents".to_string(),
	        "INSERT OR REPLACE INTO fts_agents(rowid, agent_id, project_id, name, task_description, program, model) \
	         SELECT id, id, project_id, name, task_description, program, model FROM agents"
	        .to_string(),
	        String::new(),
	    ));
    migrations.push(Migration::new(
        "v7_backfill_fts_projects".to_string(),
        "backfill fts_projects from projects".to_string(),
        "INSERT OR REPLACE INTO fts_projects(rowid, project_id, slug, human_key) \
	         SELECT id, id, slug, human_key FROM projects"
            .to_string(),
        String::new(),
    ));

    // ── v8: Search recipes and query history ──────────────────────
    migrations.extend(crate::search_recipes::recipe_migrations());

    // ── v9: Persisted tool metrics snapshots ───────────────────────
    //
    // Stores periodic per-tool metric snapshots emitted by the server worker.
    // This enables TUI hydration after restart (tool metrics + analytics).
    migrations.push(Migration::new(
        "v9_create_tool_metrics_snapshots".to_string(),
        "create persisted per-tool metrics snapshot table".to_string(),
        "CREATE TABLE IF NOT EXISTS tool_metrics_snapshots (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             collected_ts INTEGER NOT NULL, \
             tool_name TEXT NOT NULL, \
             calls INTEGER NOT NULL DEFAULT 0, \
             errors INTEGER NOT NULL DEFAULT 0, \
             cluster TEXT NOT NULL DEFAULT '', \
             capabilities_json TEXT NOT NULL DEFAULT '[]', \
             complexity TEXT NOT NULL DEFAULT 'unknown', \
             latency_avg_ms REAL, \
             latency_min_ms REAL, \
             latency_max_ms REAL, \
             latency_p50_ms REAL, \
             latency_p95_ms REAL, \
             latency_p99_ms REAL, \
             latency_is_slow INTEGER NOT NULL DEFAULT 0\
         )"
        .to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v9_idx_tool_metrics_snapshots_tool_ts".to_string(),
        "index tool_metrics_snapshots by tool_name + collected_ts desc".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_tool_metrics_snapshots_tool_ts \
         ON tool_metrics_snapshots(tool_name, collected_ts DESC)"
            .to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v9_idx_tool_metrics_snapshots_collected_ts".to_string(),
        "index tool_metrics_snapshots by collected_ts for retention pruning".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_tool_metrics_snapshots_collected_ts \
         ON tool_metrics_snapshots(collected_ts)"
            .to_string(),
        String::new(),
    ));

    // ── v10: Case-insensitive unique index on agents ────────────────
    //
    // Enforce case-insensitive uniqueness for agent names per project.
    // This prevents "BlueLake" and "bluelake" from coexisting.
    //
    // v10a: Deduplicate any pre-existing case-duplicate agents before
    // creating the UNIQUE index. For each (project_id, LOWER(name)) group
    // with >1 row, keep the one with the lowest id (oldest) and DELETE the rest.
    migrations.push(Migration::new(
        "v10a_dedup_agents_case_insensitive".to_string(),
        "deduplicate case-duplicate agents before creating unique index".to_string(),
        "DELETE FROM agents WHERE id NOT IN (\
             SELECT MIN(id) FROM agents GROUP BY project_id, name COLLATE NOCASE\
         )"
        .to_string(),
        String::new(),
    ));

    // v10b: Now safe to create the UNIQUE index (no case-duplicates remain).
    migrations.push(Migration::new(
        "v10b_idx_agents_project_name_nocase".to_string(),
        "create unique index on agents(project_id, name COLLATE NOCASE)".to_string(),
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_project_name_nocase \
         ON agents(project_id, name COLLATE NOCASE)"
            .to_string(),
        String::new(),
    ));

    // ── v11: Decommission FTS5 message search (Search V3: br-2tnl.8.4) ──
    //
    // ── v11: FTS5 decommission (br-2tnl.8.4) ────────────────────────
    //
    // Tantivy now handles all text search. Drop every FTS5 virtual table
    // and synchronization trigger. Each statement is its own migration
    // because the migration runner executes one statement per migration.
    //
    // Message FTS (created v1, rebuilt v5 with porter stemmer):
    migrations.push(Migration::new(
        "v11_drop_trigger_messages_ai".to_string(),
        "drop FTS5 messages insert trigger".to_string(),
        "DROP TRIGGER IF EXISTS messages_ai".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_messages_ad".to_string(),
        "drop FTS5 messages delete trigger".to_string(),
        "DROP TRIGGER IF EXISTS messages_ad".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_messages_au".to_string(),
        "drop FTS5 messages update trigger".to_string(),
        "DROP TRIGGER IF EXISTS messages_au".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_fts_messages_table".to_string(),
        "drop FTS5 messages virtual table".to_string(),
        "DROP TABLE IF EXISTS fts_messages".to_string(),
        String::new(),
    ));
    // Identity FTS (created v7):
    migrations.push(Migration::new(
        "v11_drop_trigger_agents_ai".to_string(),
        "drop FTS5 agents insert trigger".to_string(),
        "DROP TRIGGER IF EXISTS agents_ai".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_agents_ad".to_string(),
        "drop FTS5 agents delete trigger".to_string(),
        "DROP TRIGGER IF EXISTS agents_ad".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_agents_au".to_string(),
        "drop FTS5 agents update trigger".to_string(),
        "DROP TRIGGER IF EXISTS agents_au".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_fts_agents_table".to_string(),
        "drop FTS5 agents virtual table".to_string(),
        "DROP TABLE IF EXISTS fts_agents".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_projects_ai".to_string(),
        "drop FTS5 projects insert trigger".to_string(),
        "DROP TRIGGER IF EXISTS projects_ai".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_projects_ad".to_string(),
        "drop FTS5 projects delete trigger".to_string(),
        "DROP TRIGGER IF EXISTS projects_ad".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_trigger_projects_au".to_string(),
        "drop FTS5 projects update trigger".to_string(),
        "DROP TRIGGER IF EXISTS projects_au".to_string(),
        String::new(),
    ));
    migrations.push(Migration::new(
        "v11_drop_fts_projects_table".to_string(),
        "drop FTS5 projects virtual table".to_string(),
        "DROP TABLE IF EXISTS fts_projects".to_string(),
        String::new(),
    ));

    migrations
}

/// Returns `true` if a migration creates or manipulates FTS5 virtual tables.
///
/// Trigger DDL is supported by `FrankenConnection`; base mode only excludes
/// FTS virtual table migrations.
fn is_fts_migration(id: &str) -> bool {
    let id_lower = id.to_ascii_lowercase();
    id_lower.contains("fts")
}

/// Migrations that use SQL features unsupported by `FrankenConnection`.
///
/// Includes FTS5 virtual tables, queries with aggregate functions over JOINs,
/// and CREATE INDEX with expressions (COLLATE NOCASE).
fn is_unsupported_by_franken(id: &str) -> bool {
    is_fts_migration(id)
        || matches!(
            id,
            "v6_backfill_inbox_stats"
                | "v10a_dedup_agents_case_insensitive"
                | "v10b_idx_agents_project_name_nocase"
        )
}

/// Base-only trigger cleanup migrations.
///
/// Base mode runs during startup to make DB files safe for later runtime access.
/// Any pre-existing message->FTS triggers can break message inserts in that mode,
/// so base startup drops both legacy
/// Python trigger names and current Rust trigger names.
fn base_trigger_cleanup_migrations() -> Vec<Migration> {
    let cleanup_steps = vec![
        (
            "base_v1_drop_legacy_fts_messages_ai",
            "drop legacy python fts insert trigger for base mode",
            "DROP TRIGGER IF EXISTS fts_messages_ai",
        ),
        (
            "base_v1_drop_legacy_fts_messages_ad",
            "drop legacy python fts delete trigger for base mode",
            "DROP TRIGGER IF EXISTS fts_messages_ad",
        ),
        (
            "base_v1_drop_legacy_fts_messages_au",
            "drop legacy python fts update trigger for base mode",
            "DROP TRIGGER IF EXISTS fts_messages_au",
        ),
        (
            "base_v1_drop_rust_messages_ai",
            "drop rust fts insert trigger for base mode",
            "DROP TRIGGER IF EXISTS messages_ai",
        ),
        (
            "base_v1_drop_rust_messages_ad",
            "drop rust fts delete trigger for base mode",
            "DROP TRIGGER IF EXISTS messages_ad",
        ),
        (
            "base_v1_drop_rust_messages_au",
            "drop rust fts update trigger for base mode",
            "DROP TRIGGER IF EXISTS messages_au",
        ),
        (
            "base_v2_drop_fts_agents_insert_trigger",
            "drop identity fts agent insert trigger for base mode",
            "DROP TRIGGER IF EXISTS agents_ai",
        ),
        (
            "base_v2_drop_fts_agents_delete_trigger",
            "drop identity fts agent delete trigger for base mode",
            "DROP TRIGGER IF EXISTS agents_ad",
        ),
        (
            "base_v2_drop_fts_agents_update_trigger",
            "drop identity fts agent update trigger for base mode",
            "DROP TRIGGER IF EXISTS agents_au",
        ),
        (
            "base_v2_drop_fts_projects_insert_trigger",
            "drop identity fts project insert trigger for base mode",
            "DROP TRIGGER IF EXISTS projects_ai",
        ),
        (
            "base_v2_drop_fts_projects_delete_trigger",
            "drop identity fts project delete trigger for base mode",
            "DROP TRIGGER IF EXISTS projects_ad",
        ),
        (
            "base_v2_drop_fts_projects_update_trigger",
            "drop identity fts project update trigger for base mode",
            "DROP TRIGGER IF EXISTS projects_au",
        ),
        (
            "base_v2_drop_fts_agents_table",
            "drop identity fts agent table for base mode",
            "DROP TABLE IF EXISTS fts_agents",
        ),
        (
            "base_v2_drop_fts_projects_table",
            "drop identity fts project table for base mode",
            "DROP TABLE IF EXISTS fts_projects",
        ),
    ];

    cleanup_steps
        .into_iter()
        .map(|(id, desc, up)| {
            Migration::new(
                id.to_string(),
                desc.to_string(),
                up.to_string(),
                String::new(),
            )
        })
        .collect()
}

/// Re-apply base-mode cleanup statements at startup.
///
/// This is intentionally separate from migration history so servers can recover
/// from DB files that were later touched by full/CLI migrations and reintroduced
/// incompatible FTS identity objects.
#[allow(clippy::result_large_err)]
pub fn enforce_base_mode_cleanup(conn: &DbConn) -> std::result::Result<(), SqlError> {
    for migration in base_trigger_cleanup_migrations() {
        conn.execute_raw(&migration.up)?;
    }
    Ok(())
}

/// Re-apply runtime cleanup for ALL FTS artifacts (messages + identity).
///
/// Since Search V3 decommission (br-2tnl.8.4), Tantivy handles all text search.
/// This drops `fts_messages`, `fts_agents`, `fts_projects` and all their triggers.
#[allow(clippy::result_large_err)]
pub fn enforce_runtime_fts_cleanup(conn: &DbConn) -> std::result::Result<(), SqlError> {
    // Drop all FTS artifacts — same as base mode cleanup
    for migration in base_trigger_cleanup_migrations() {
        conn.execute_raw(&migration.up)?;
    }
    // Also drop fts_messages table itself
    conn.execute_raw("DROP TABLE IF EXISTS fts_messages")?;
    Ok(())
}

/// Migrations excluding FTS5 virtual tables and FTS backfill inserts.
///
/// Safe for databases that will be opened by `FrankenConnection`. The migration
/// runner will still record these IDs in the migrations table.
#[must_use]
pub fn schema_migrations_base() -> Vec<Migration> {
    let mut migrations: Vec<Migration> = schema_migrations()
        .into_iter()
        .filter(|m| !is_unsupported_by_franken(&m.id))
        .collect();
    migrations.extend(base_trigger_cleanup_migrations());
    migrations
}

#[must_use]
pub fn migration_runner() -> MigrationRunner {
    MigrationRunner::new(schema_migrations()).table_name(MIGRATIONS_TABLE_NAME)
}

/// Migration runner that skips FTS5 migrations (safe for `FrankenConnection` DBs).
#[must_use]
pub fn migration_runner_base() -> MigrationRunner {
    MigrationRunner::new(schema_migrations_base()).table_name(MIGRATIONS_TABLE_NAME)
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

/// Run only base migrations (no FTS5 virtual tables).
///
/// Use this when the database will be opened by `FrankenConnection`. FTS5
/// shadow table pages in the file would cause `FrankenConnection::open_file`
/// to fail with unsupported virtual-table behavior.
pub async fn migrate_to_latest_base<C: Connection>(
    cx: &Cx,
    conn: &C,
) -> Outcome<Vec<String>, SqlError> {
    match init_migrations_table(cx, conn).await {
        Outcome::Ok(()) => {}
        Outcome::Err(e) => return Outcome::Err(e),
        Outcome::Cancelled(r) => return Outcome::Cancelled(r),
        Outcome::Panicked(p) => return Outcome::Panicked(p),
    }
    migration_runner_base().migrate(cx, conn).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use crate::DbConn;

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
        let conn = DbConn::open_file(db_path.display().to_string())
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
        let conn = DbConn::open_file(db_path.display().to_string())
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
    fn base_migrations_include_message_fts_trigger_cleanup() {
        use std::collections::HashSet;

        let ids: HashSet<String> = schema_migrations_base().into_iter().map(|m| m.id).collect();
        assert!(ids.contains("base_v1_drop_legacy_fts_messages_ai"));
        assert!(ids.contains("base_v1_drop_legacy_fts_messages_ad"));
        assert!(ids.contains("base_v1_drop_legacy_fts_messages_au"));
        assert!(ids.contains("base_v1_drop_rust_messages_ai"));
        assert!(ids.contains("base_v1_drop_rust_messages_ad"));
        assert!(ids.contains("base_v1_drop_rust_messages_au"));
        assert!(ids.contains("base_v2_drop_fts_agents_insert_trigger"));
        assert!(ids.contains("base_v2_drop_fts_agents_delete_trigger"));
        assert!(ids.contains("base_v2_drop_fts_agents_update_trigger"));
        assert!(ids.contains("base_v2_drop_fts_projects_insert_trigger"));
        assert!(ids.contains("base_v2_drop_fts_projects_delete_trigger"));
        assert!(ids.contains("base_v2_drop_fts_projects_update_trigger"));
        assert!(ids.contains("base_v2_drop_fts_agents_table"));
        assert!(ids.contains("base_v2_drop_fts_projects_table"));

        // FTS table creation must still be excluded from base migrations.
        assert!(!ids.contains("v5_create_fts_with_porter"));
        assert!(!ids.contains("v7_create_fts_agents"));
        assert!(!ids.contains("v7_create_fts_projects"));
    }

    #[test]
    fn base_migrations_drop_existing_message_fts_triggers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("base_drop_fts_triggers.db");
        let conn = DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        conn.execute_raw(PRAGMA_SETTINGS_SQL)
            .expect("apply PRAGMAs");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS messages (\
                id INTEGER PRIMARY KEY AUTOINCREMENT,\
                project_id INTEGER NOT NULL,\
                sender_id INTEGER NOT NULL,\
                thread_id TEXT,\
                subject TEXT NOT NULL,\
                body_md TEXT NOT NULL,\
                importance TEXT NOT NULL DEFAULT 'normal',\
                ack_required INTEGER NOT NULL DEFAULT 0,\
                created_ts INTEGER NOT NULL,\
                attachments_json TEXT NOT NULL DEFAULT ''\
            )",
            &[],
        )
        .expect("create messages table");
        conn.execute_sync(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(message_id UNINDEXED, subject, body)",
            &[],
        )
        .expect("create fts_messages table");

        // Legacy Python trigger names.
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS fts_messages_ai AFTER INSERT ON messages BEGIN \
                 INSERT INTO fts_messages(rowid, message_id, subject, body) \
                 VALUES (NEW.id, NEW.id, NEW.subject, NEW.body_md); \
             END",
            &[],
        )
        .expect("create legacy ai trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS fts_messages_ad AFTER DELETE ON messages BEGIN \
                 DELETE FROM fts_messages WHERE rowid = OLD.id; \
             END",
            &[],
        )
        .expect("create legacy ad trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS fts_messages_au AFTER UPDATE ON messages BEGIN \
                 DELETE FROM fts_messages WHERE rowid = OLD.id; \
                 INSERT INTO fts_messages(rowid, message_id, subject, body) \
                 VALUES (NEW.id, NEW.id, NEW.subject, NEW.body_md); \
             END",
            &[],
        )
        .expect("create legacy au trigger");

        // Current Rust trigger names.
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN \
                 INSERT INTO fts_messages(message_id, subject, body) \
                 VALUES (NEW.id, NEW.subject, NEW.body_md); \
             END",
            &[],
        )
        .expect("create rust ai trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN \
                 DELETE FROM fts_messages WHERE message_id = OLD.id; \
             END",
            &[],
        )
        .expect("create rust ad trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN \
                 DELETE FROM fts_messages WHERE message_id = OLD.id; \
                 INSERT INTO fts_messages(message_id, subject, body) \
                 VALUES (NEW.id, NEW.subject, NEW.body_md); \
             END",
            &[],
        )
        .expect("create rust au trigger");

        block_on({
            let conn = &conn;
            move |cx| async move {
                migrate_to_latest_base(&cx, conn)
                    .await
                    .into_result()
                    .unwrap()
            }
        });

        let rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='trigger' AND name IN (\
                     'fts_messages_ai', 'fts_messages_ad', 'fts_messages_au', \
                     'messages_ai', 'messages_ad', 'messages_au'\
                 )",
                &[],
            )
            .expect("query remaining trigger names");
        assert!(
            rows.is_empty(),
            "base migrations should remove all message->fts triggers"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn enforce_base_mode_cleanup_drops_identity_fts_objects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("base_cleanup_identity_fts.db");
        let conn = DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        conn.execute_raw(PRAGMA_SETTINGS_SQL)
            .expect("apply PRAGMAs");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS projects (\
                id INTEGER PRIMARY KEY AUTOINCREMENT,\
                slug TEXT NOT NULL UNIQUE,\
                human_key TEXT NOT NULL,\
                created_at INTEGER NOT NULL\
            )",
            &[],
        )
        .expect("create projects table");
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS agents (\
                id INTEGER PRIMARY KEY AUTOINCREMENT,\
                project_id INTEGER NOT NULL,\
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
        .expect("create agents table");
        conn.execute_sync(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_agents USING fts5(\
                agent_id UNINDEXED, project_id UNINDEXED, name, task_description, program, model\
            )",
            &[],
        )
        .expect("create fts_agents table");
        conn.execute_sync(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_projects USING fts5(\
                project_id UNINDEXED, slug, human_key\
            )",
            &[],
        )
        .expect("create fts_projects table");

        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS agents_ai AFTER INSERT ON agents BEGIN \
                 INSERT INTO fts_agents(rowid, agent_id, project_id, name, task_description, program, model) \
                 VALUES (NEW.id, NEW.id, NEW.project_id, NEW.name, NEW.task_description, NEW.program, NEW.model); \
             END",
            &[],
        )
        .expect("create agents_ai trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS agents_ad AFTER DELETE ON agents BEGIN \
                 DELETE FROM fts_agents WHERE rowid = OLD.id; \
             END",
            &[],
        )
        .expect("create agents_ad trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS agents_au AFTER UPDATE ON agents BEGIN \
                 DELETE FROM fts_agents WHERE rowid = OLD.id; \
                 INSERT INTO fts_agents(rowid, agent_id, project_id, name, task_description, program, model) \
                 VALUES (NEW.id, NEW.id, NEW.project_id, NEW.name, NEW.task_description, NEW.program, NEW.model); \
             END",
            &[],
        )
        .expect("create agents_au trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS projects_ai AFTER INSERT ON projects BEGIN \
                 INSERT INTO fts_projects(rowid, project_id, slug, human_key) \
                 VALUES (NEW.id, NEW.id, NEW.slug, NEW.human_key); \
             END",
            &[],
        )
        .expect("create projects_ai trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS projects_ad AFTER DELETE ON projects BEGIN \
                 DELETE FROM fts_projects WHERE rowid = OLD.id; \
             END",
            &[],
        )
        .expect("create projects_ad trigger");
        conn.execute_sync(
            "CREATE TRIGGER IF NOT EXISTS projects_au AFTER UPDATE ON projects BEGIN \
                 DELETE FROM fts_projects WHERE rowid = OLD.id; \
                 INSERT INTO fts_projects(rowid, project_id, slug, human_key) \
                 VALUES (NEW.id, NEW.id, NEW.slug, NEW.human_key); \
             END",
            &[],
        )
        .expect("create projects_au trigger");

        enforce_base_mode_cleanup(&conn).expect("base cleanup");

        let trigger_rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='trigger' AND name IN (\
                     'agents_ai', 'agents_ad', 'agents_au',\
                     'projects_ai', 'projects_ad', 'projects_au'\
                 )",
                &[],
            )
            .expect("query trigger names");
        assert!(
            trigger_rows.is_empty(),
            "base cleanup should remove identity FTS triggers"
        );

        let fts_rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='table' AND name IN ('fts_agents', 'fts_projects')",
                &[],
            )
            .expect("query fts table names");
        assert!(
            fts_rows.is_empty(),
            "base cleanup should remove identity FTS tables"
        );
    }

    #[test]
    fn enforce_runtime_fts_cleanup_drops_all_fts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("runtime_cleanup_all_fts.db");
        let conn = DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        conn.execute_raw(PRAGMA_DB_INIT_SQL).expect("apply PRAGMAs");
        let conn_ref = &conn;
        block_on(|cx| async move {
            migrate_to_latest(&cx, conn_ref)
                .await
                .into_result()
                .expect("apply full migrations");
        });

        enforce_runtime_fts_cleanup(&conn).expect("runtime fts cleanup");

        // All message FTS triggers should be dropped (Tantivy handles search now)
        let message_trigger_rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='trigger' AND name IN ('messages_ai', 'messages_ad', 'messages_au')",
                &[],
            )
            .expect("query message trigger names");
        assert!(
            message_trigger_rows.is_empty(),
            "runtime cleanup should remove ALL FTS triggers (Search V3 decommission)"
        );

        // All identity FTS triggers should also be dropped
        let identity_trigger_rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='trigger' AND name IN (\
                     'agents_ai', 'agents_ad', 'agents_au',\
                     'projects_ai', 'projects_ad', 'projects_au'\
                 )",
                &[],
            )
            .expect("query identity trigger names");
        assert!(
            identity_trigger_rows.is_empty(),
            "runtime cleanup should remove identity FTS triggers"
        );

        // All FTS tables should be dropped
        let fts_rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master \
                 WHERE type='table' AND name IN ('fts_messages', 'fts_agents', 'fts_projects')",
                &[],
            )
            .expect("query fts table names");
        assert!(
            fts_rows.is_empty(),
            "runtime cleanup should remove ALL FTS tables"
        );
    }

    #[test]
    fn v3_migration_converts_text_timestamps_to_integer() {
        use sqlmodel_core::Value;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("v3_text_ts.db");
        let conn = DbConn::open_file(db_path.display().to_string())
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
            "INSERT INTO file_reservations (project_id, agent_id, path_pattern, created_ts, expires_ts, released_ts) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("src/**".to_string()),
                Value::Text("2026-02-04 22:20:00.123456".to_string()),
                Value::Text("2026-02-04 23:20:00.654321".to_string()),
                Value::Text("2026-02-04 23:25:00.000000".to_string()),
            ],
        ).expect("insert legacy file_reservation");

        // Create legacy products table with TEXT timestamps.
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS products (id INTEGER PRIMARY KEY AUTOINCREMENT, product_uid TEXT NOT NULL UNIQUE, name TEXT NOT NULL UNIQUE, created_at DATETIME NOT NULL)",
            &[],
        ).expect("create legacy products table");
        conn.execute_sync(
            "INSERT INTO products (product_uid, name, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text("uid-001".to_string()),
                Value::Text("MyProduct".to_string()),
                Value::Text("2026-02-04 22:30:00.999999".to_string()),
            ],
        )
        .expect("insert legacy product");

        // Create legacy product_project_links table with TEXT timestamps.
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS product_project_links (id INTEGER PRIMARY KEY AUTOINCREMENT, product_id INTEGER NOT NULL, project_id INTEGER NOT NULL, created_at DATETIME NOT NULL, UNIQUE(product_id, project_id))",
            &[],
        ).expect("create legacy product_project_links table");
        conn.execute_sync(
            "INSERT INTO product_project_links (product_id, project_id, created_at) VALUES (?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("2026-02-04 22:35:00.500000".to_string()),
            ],
        ).expect("insert legacy product_project_link");

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

        // Verify file_reservations timestamps are now INTEGER (including released_ts)
        let rows = conn
            .query_sync(
                "SELECT typeof(created_ts) as t1, typeof(expires_ts) as t2, typeof(released_ts) as t3 FROM file_reservations",
                &[],
            )
            .expect("query file_reservations");
        assert_eq!(rows[0].get_named::<String>("t1").unwrap(), "integer");
        assert_eq!(rows[0].get_named::<String>("t2").unwrap(), "integer");
        assert_eq!(rows[0].get_named::<String>("t3").unwrap(), "integer");

        // Verify products.created_at is now INTEGER
        let rows = conn
            .query_sync(
                "SELECT typeof(created_at) as t, created_at FROM products",
                &[],
            )
            .expect("query products");
        assert_eq!(rows[0].get_named::<String>("t").unwrap(), "integer");
        let products_created: i64 = rows[0].get_named("created_at").unwrap();
        assert!(
            products_created > 1_700_000_000_000_000,
            "products.created_at should be microseconds: {products_created}"
        );

        // Verify product_project_links.created_at is now INTEGER
        let rows = conn
            .query_sync(
                "SELECT typeof(created_at) as t, created_at FROM product_project_links",
                &[],
            )
            .expect("query product_project_links");
        assert_eq!(rows[0].get_named::<String>("t").unwrap(), "integer");
        let link_created: i64 = rows[0].get_named("created_at").unwrap();
        assert!(
            link_created > 1_700_000_000_000_000,
            "product_project_links.created_at should be microseconds: {link_created}"
        );
    }

    #[test]
    fn v4_migration_creates_composite_indexes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("v4_indexes.db");
        let conn = DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        // Apply all migrations.
        block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
        });

        // Query sqlite_master for v4 indexes.
        let rows = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%' ORDER BY name",
                &[],
            )
            .expect("query indexes");

        let index_names: Vec<String> = rows
            .iter()
            .map(|r| r.get_named::<String>("name").unwrap())
            .collect();

        // v4 composite indexes must exist.
        assert!(
            index_names.contains(&"idx_mr_agent_ack".to_string()),
            "missing idx_mr_agent_ack in {index_names:?}"
        );
        assert!(
            index_names.contains(&"idx_msg_thread_created".to_string()),
            "missing idx_msg_thread_created in {index_names:?}"
        );
        assert!(
            index_names.contains(&"idx_msg_project_importance_created".to_string()),
            "missing idx_msg_project_importance_created in {index_names:?}"
        );
        assert!(
            index_names.contains(&"idx_al_a_agent_status".to_string()),
            "missing idx_al_a_agent_status in {index_names:?}"
        );
        assert!(
            index_names.contains(&"idx_al_b_agent_status".to_string()),
            "missing idx_al_b_agent_status in {index_names:?}"
        );
    }

    #[test]
    fn v4_indexes_applied_to_existing_db() {
        use sqlmodel_core::Value;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("v4_existing.db");
        let conn = DbConn::open_file(db_path.display().to_string())
            .expect("open sqlite connection");

        conn.execute_raw(PRAGMA_SETTINGS_SQL)
            .expect("apply PRAGMAs");

        // Create minimal schema (pre-v4) with some data.
        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS projects (id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, human_key TEXT NOT NULL, created_at INTEGER NOT NULL)",
            &[],
        ).expect("create projects table");
        conn.execute_sync(
            "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text("test".to_string()),
                Value::Text("/test".to_string()),
                Value::BigInt(100),
            ],
        )
        .expect("insert project");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, name TEXT NOT NULL, program TEXT NOT NULL, model TEXT NOT NULL, task_description TEXT NOT NULL DEFAULT '', inception_ts INTEGER NOT NULL, last_active_ts INTEGER NOT NULL, attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto', UNIQUE(project_id, name))",
            &[],
        ).expect("create agents table");
        conn.execute_sync(
            "INSERT INTO agents (project_id, name, program, model, inception_ts, last_active_ts) VALUES (?, ?, ?, ?, ?, ?)",
            &[Value::BigInt(1), Value::Text("BlueLake".to_string()), Value::Text("cc".to_string()), Value::Text("opus".to_string()), Value::BigInt(100), Value::BigInt(100)],
        ).expect("insert agent");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL, sender_id INTEGER NOT NULL, thread_id TEXT, subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL DEFAULT 'normal', ack_required INTEGER NOT NULL DEFAULT 0, created_ts INTEGER NOT NULL, attachments TEXT NOT NULL DEFAULT '[]')",
            &[],
        ).expect("create messages table");
        conn.execute_sync(
            "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, created_ts) VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[Value::BigInt(1), Value::BigInt(1), Value::Text("t1".to_string()), Value::Text("Hi".to_string()), Value::Text("body".to_string()), Value::Text("urgent".to_string()), Value::BigInt(200)],
        ).expect("insert message");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS message_recipients (message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT 'to', read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id))",
            &[],
        ).expect("create message_recipients table");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("to".to_string()),
            ],
        )
        .expect("insert recipient");

        conn.execute_sync(
            "CREATE TABLE IF NOT EXISTS agent_links (id INTEGER PRIMARY KEY AUTOINCREMENT, a_project_id INTEGER NOT NULL, a_agent_id INTEGER NOT NULL, b_project_id INTEGER NOT NULL, b_agent_id INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', reason TEXT NOT NULL DEFAULT '', created_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL, expires_ts INTEGER, UNIQUE(a_project_id, a_agent_id, b_project_id, b_agent_id))",
            &[],
        ).expect("create agent_links table");

        // Now run migrations — v4 should create indexes on existing tables.
        let applied = block_on({
            let conn = &conn;
            move |cx| async move { migrate_to_latest(&cx, conn).await.into_result().unwrap() }
        });

        // v4 indexes should be among applied migrations.
        assert!(
            applied.iter().any(|id| id == "v4_idx_mr_agent_ack"),
            "v4_idx_mr_agent_ack should be applied: {applied:?}"
        );

        // Verify queries using the new indexes work with data.
        let rows = conn
            .query_sync(
                "SELECT agent_id FROM message_recipients WHERE agent_id = 1 AND ack_ts IS NULL",
                &[],
            )
            .expect("query using idx_mr_agent_ack");
        assert_eq!(rows.len(), 1);

        let rows = conn
            .query_sync(
                "SELECT id FROM messages WHERE thread_id = 't1' ORDER BY created_ts ASC",
                &[],
            )
            .expect("query using idx_msg_thread_created");
        assert_eq!(rows.len(), 1);

        let rows = conn
            .query_sync(
                "SELECT id FROM messages WHERE project_id = 1 AND importance = 'urgent' ORDER BY created_ts DESC",
                &[],
            )
            .expect("query using idx_msg_project_importance_created");
        assert_eq!(rows.len(), 1);
    }

    // NOTE: v5_fts_porter_stemming_and_prefix removed — FTS5 decommissioned
    // in Search V3 cutover (br-2tnl.8.4).  Tantivy handles stemming/prefix.

    // NOTE: v7_fts_agents_and_projects_backfill_and_triggers_work removed —
    // identity FTS tables and triggers dropped by v11 migrations (br-2tnl.8.4).
    // Tantivy handles full-text search for agents and projects now.

    #[test]
    fn schema_migrations_include_tool_metrics_snapshot_table() {
        let ids: std::collections::HashSet<String> =
            schema_migrations().into_iter().map(|m| m.id).collect();
        assert!(ids.contains("v9_create_tool_metrics_snapshots"));
        assert!(ids.contains("v9_idx_tool_metrics_snapshots_tool_ts"));
        assert!(ids.contains("v9_idx_tool_metrics_snapshots_collected_ts"));
    }

    #[test]
    fn corrupted_migrations_table_yields_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("migrations_corrupt.db");
        let conn = DbConn::open_file(db_path.display().to_string())
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

    // ── br-3h13.17.3: SQL schema extraction tests (JadeCave) ──────────

    #[test]
    fn extract_ident_create_table() {
        let result = extract_ident_after_keyword(
            "CREATE TABLE IF NOT EXISTS foo (id INT)",
            "create table if not exists ",
        );
        assert_eq!(result, Some("foo".to_string()));
    }

    #[test]
    fn extract_ident_create_index() {
        let result = extract_ident_after_keyword(
            "CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages (ts)",
            "create index if not exists ",
        );
        assert_eq!(result, Some("idx_messages_ts".to_string()));
    }

    #[test]
    fn extract_ident_create_trigger() {
        let result = extract_ident_after_keyword(
            "CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN ... END",
            "create trigger if not exists ",
        );
        assert_eq!(result, Some("messages_ai".to_string()));
    }

    #[test]
    fn extract_ident_create_virtual_table() {
        let result = extract_ident_after_keyword(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(subject, body_md)",
            "create virtual table if not exists ",
        );
        assert_eq!(result, Some("fts_messages".to_string()));
    }

    #[test]
    fn extract_ident_keyword_not_found() {
        let result =
            extract_ident_after_keyword("SELECT * FROM foo", "create table if not exists ");
        assert_eq!(result, None);
    }

    #[test]
    fn extract_ident_empty_sql() {
        assert_eq!(extract_ident_after_keyword("", "create table "), None);
    }

    #[test]
    fn extract_ident_keyword_at_end() {
        // Keyword found but nothing after it
        let result = extract_ident_after_keyword(
            "CREATE TABLE IF NOT EXISTS ",
            "create table if not exists ",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn extract_ident_case_insensitive() {
        let result = extract_ident_after_keyword(
            "create table if not exists MyTable (id INT)",
            "create table if not exists ",
        );
        assert_eq!(result, Some("MyTable".to_string()));
    }

    #[test]
    fn extract_ident_multiple_spaces() {
        let result = extract_ident_after_keyword(
            "CREATE TABLE IF NOT EXISTS    spaced_table  (id INT)",
            "create table if not exists ",
        );
        assert_eq!(result, Some("spaced_table".to_string()));
    }

    #[test]
    fn extract_ident_underscore_name() {
        let result = extract_ident_after_keyword(
            "CREATE TABLE IF NOT EXISTS _private_table (id INT)",
            "create table if not exists ",
        );
        assert_eq!(result, Some("_private_table".to_string()));
    }

    #[test]
    fn extract_trigger_statements_single() {
        let sql = "CREATE TRIGGER IF NOT EXISTS trg_ai AFTER INSERT ON t BEGIN SELECT 1; END;";
        let stmts = extract_trigger_statements(sql);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("trg_ai"));
    }

    #[test]
    fn extract_trigger_statements_multiple() {
        let sql = "\
            CREATE TRIGGER IF NOT EXISTS trg_ai AFTER INSERT ON t BEGIN SELECT 1; END;\n\
            CREATE TRIGGER IF NOT EXISTS trg_ad AFTER DELETE ON t BEGIN SELECT 2; END;";
        let stmts = extract_trigger_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("trg_ai"));
        assert!(stmts[1].contains("trg_ad"));
    }

    #[test]
    fn extract_trigger_statements_empty() {
        assert!(extract_trigger_statements("").is_empty());
    }

    #[test]
    fn extract_trigger_statements_no_triggers() {
        let sql = "CREATE TABLE foo (id INT); CREATE INDEX idx ON foo (id);";
        assert!(extract_trigger_statements(sql).is_empty());
    }

    #[test]
    fn extract_trigger_statements_mixed_with_non_trigger() {
        let sql = "\
            CREATE TABLE foo (id INT);\n\
            CREATE TRIGGER IF NOT EXISTS trg_ai AFTER INSERT ON foo BEGIN INSERT INTO bar VALUES (NEW.id); END;\n\
            CREATE INDEX idx ON foo (id);";
        let stmts = extract_trigger_statements(sql);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].starts_with("CREATE TRIGGER"));
    }

    #[test]
    fn derive_migration_id_table() {
        let result = derive_migration_id_and_description(
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY)",
        );
        assert_eq!(
            result,
            Some((
                "v1_create_table_messages".to_string(),
                "create table messages".to_string()
            ))
        );
    }

    #[test]
    fn derive_migration_id_index() {
        let result = derive_migration_id_and_description(
            "CREATE INDEX IF NOT EXISTS idx_ts ON messages (ts)",
        );
        assert_eq!(
            result,
            Some((
                "v1_create_index_idx_ts".to_string(),
                "create index idx_ts".to_string()
            ))
        );
    }

    #[test]
    fn derive_migration_id_unknown_returns_none() {
        assert_eq!(derive_migration_id_and_description("SELECT 1"), None);
        assert_eq!(derive_migration_id_and_description(""), None);
    }

    // ── br-3h13.17.3 addendum: additional edge case (RubyPrairie) ──────

    #[test]
    fn extract_ident_stops_at_parenthesis() {
        // No space between identifier and parenthesis
        let sql = "CREATE TABLE IF NOT EXISTS tbl(id INT)";
        assert_eq!(
            extract_ident_after_keyword(sql, "create table if not exists "),
            Some("tbl".into())
        );
    }
}
