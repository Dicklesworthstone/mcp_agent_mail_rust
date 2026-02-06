//! Database schema creation and migrations
//!
//! Creates all tables, indexes, and FTS5 virtual tables.

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
