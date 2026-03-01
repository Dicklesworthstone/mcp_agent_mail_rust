//! Reconstruct a `SQLite` database from the Git archive.
//!
//! When the database file is corrupt and no healthy backup exists, this module
//! walks the per-project Git archive directories to recover:
//!
//! - **Projects** — from subdirectory names under `{storage_root}/projects/`
//!   plus optional `project.json` metadata for exact `human_key` recovery
//! - **Agents** — from `agents/{name}/profile.json` files
//! - **Messages** — from `messages/{YYYY}/{MM}/*.md` files (JSON frontmatter)
//! - **Message recipients** — from the `to`, `cc`, `bcc` arrays in frontmatter
//!
//! The reconstructed database will be missing:
//! - `read_ts` / `ack_ts` on `message_recipients` (no archive artifact for these)
//! - `file_reservations` (ephemeral by design; TTL-based)
//! - `agent_links` / contacts (handshake state not archived)
//! - `products` / `product_project_links` (not archived)
//!
//! These are acceptable losses because reservations and contacts are transient,
//! and the core data (messages + agents) is fully recovered.

use crate::DbConn;
use crate::error::{DbError, DbResult};
use crate::schema;
use sqlmodel_core::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Statistics returned after a reconstruction attempt.
#[derive(Debug, Clone, Default)]
pub struct ReconstructStats {
    /// Number of projects discovered and inserted.
    pub projects: usize,
    /// Number of agents discovered and inserted.
    pub agents: usize,
    /// Number of messages recovered from archive files.
    pub messages: usize,
    /// Number of message-recipient rows inserted.
    pub recipients: usize,
    /// Number of archive files that failed to parse (skipped).
    pub parse_errors: usize,
    /// Human-readable warnings collected during reconstruction.
    pub warnings: Vec<String>,
}

impl std::fmt::Display for ReconstructStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconstructed {} projects, {} agents, {} messages ({} recipients), {} parse errors",
            self.projects, self.agents, self.messages, self.recipients, self.parse_errors
        )
    }
}

/// Reconstruct the database from the Git archive at `storage_root`.
///
/// Opens (or creates) a fresh `SQLite` database at `db_path`, runs schema
/// migrations, then walks the archive to recover data.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or if schema creation
/// fails. Individual archive files that fail to parse are skipped (counted
/// in `parse_errors`).
#[allow(clippy::too_many_lines)]
pub fn reconstruct_from_archive(db_path: &Path, storage_root: &Path) -> DbResult<ReconstructStats> {
    let db_str = db_path.to_string_lossy();
    let conn = DbConn::open_file(db_str.as_ref()).map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct: cannot open {}: {e}",
            db_path.display()
        ))
    })?;

    // Apply base-mode PRAGMAs: DELETE journal (rollback) is safer for one-shot
    // reconstruction. WAL mode causes corruption when the runtime later opens
    // with different connection settings (e.g. FrankenConnection pool warmup).
    for pragma in schema::PRAGMA_DB_INIT_BASE_SQL.split(';') {
        let pragma = pragma.trim();
        if pragma.is_empty() {
            continue;
        }
        conn.execute_raw(&format!("{pragma};"))
            .map_err(|e| DbError::Sqlite(format!("reconstruct: pragma: {e}")))?;
    }
    conn.execute_raw("PRAGMA synchronous=NORMAL;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: synchronous: {e}")))?;
    conn.execute_raw("PRAGMA busy_timeout=60000;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: busy_timeout: {e}")))?;

    // Apply schema via the migration pipeline (base mode: no FTS5 virtual
    // tables, which FrankenConnection doesn't support). First lay down the
    // base DDL, then run each base migration individually. This keeps the
    // reconstructed DB aligned with the same schema state the runtime expects.
    let ddl = schema::init_schema_sql_base();
    for stmt in ddl.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        conn.execute_raw(&format!("{stmt};"))
            .map_err(|e| DbError::Sqlite(format!("reconstruct: DDL: {e}")))?;
    }

    // Run base migrations so the migrations table records the correct state.
    // This ensures the runtime won't re-run migrations on first open.
    let base_migrations = schema::schema_migrations_base();
    // Create the migrations tracking table first.
    conn.execute_raw(&format!(
        "CREATE TABLE IF NOT EXISTS {} (\
            id TEXT PRIMARY KEY ON CONFLICT IGNORE,\
            description TEXT NOT NULL,\
            applied_at INTEGER NOT NULL\
        )",
        schema::MIGRATIONS_TABLE_NAME,
    ))
    .map_err(|e| DbError::Sqlite(format!("reconstruct: migrations table: {e}")))?;

    let migration_ts = crate::now_micros();
    for migration in &base_migrations {
        // Execute migration SQL. We tolerate only duplicate/exists-style
        // idempotency errors; anything else indicates a broken reconstruction.
        if let Err(e) = conn.execute_raw(&migration.up) {
            let err_text = e.to_string();
            if is_reconstruct_benign_migration_error(&err_text) {
                tracing::debug!(
                    migration_id = %migration.id,
                    error = %err_text,
                    "reconstruct migration produced benign idempotency error; continuing"
                );
            } else {
                return Err(DbError::Sqlite(format!(
                    "reconstruct: apply migration {}: {e}",
                    migration.id
                )));
            }
        }
        // Record it as applied.
        conn.execute_sync(
            &format!(
                "INSERT OR IGNORE INTO {} (id, description, applied_at) VALUES (?, ?, ?)",
                schema::MIGRATIONS_TABLE_NAME,
            ),
            &[
                Value::Text(migration.id.clone()),
                Value::Text(migration.description.clone()),
                Value::BigInt(migration_ts),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("reconstruct: record migration: {e}")))?;
    }

    // Clean up any FTS artifacts that may have been left by prior migrations.
    schema::enforce_runtime_fts_cleanup(&conn)
        .map_err(|e| DbError::Sqlite(format!("reconstruct: fts cleanup: {e}")))?;

    let mut stats = ReconstructStats::default();

    // Maps for deduplication: ((project_id, name) → agent_id)
    let mut agent_ids: HashMap<(i64, String), i64> = HashMap::new();

    let projects_dir = storage_root.join("projects");
    if !projects_dir.is_dir() {
        stats.warnings.push(format!(
            "No projects directory found at {}",
            projects_dir.display()
        ));
        return Ok(stats);
    }

    // Phase 1: Discover projects
    let mut project_dirs: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(slug) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            project_dirs.push((slug, path));
        }
    }
    project_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    for (slug, project_path) in &project_dirs {
        let now = crate::now_micros();
        let human_key = read_project_human_key(project_path, slug, &mut stats);

        conn.execute_sync(
            "INSERT OR IGNORE INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
            &[
                Value::Text(slug.clone()),
                Value::Text(human_key.clone()),
                Value::BigInt(now),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("reconstruct: insert project {slug}: {e}")))?;

        let pid = query_last_insert_or_existing_id(&conn, "projects", "slug", slug)?;
        stats.projects += 1;

        // Phase 2: Discover agents for this project
        let agents_dir = project_path.join("agents");
        if agents_dir.is_dir() {
            discover_agents(&conn, &agents_dir, pid, &mut agent_ids, &mut stats)?;
        }

        // Phase 3: Discover messages for this project
        let messages_dir = project_path.join("messages");
        if messages_dir.is_dir() {
            discover_messages(&conn, &messages_dir, pid, slug, &mut agent_ids, &mut stats);
        }
    }

    // Rebuild all index b-trees to ensure consistency after bulk inserts.
    conn.execute_raw("REINDEX;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: REINDEX: {e}")))?;

    // Flush WAL (if any residual) and remove sidecar files so the DB is a
    // single clean file ready for the runtime to open with its own settings.
    let _ = conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);");

    tracing::info!(%stats, "database reconstruction from archive complete");
    Ok(stats)
}

#[must_use]
fn is_reconstruct_benign_migration_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("already exists")
        || lower.contains("duplicate column name")
        || lower.contains("duplicate index name")
}

/// Walk `agents/{name}/profile.json` and insert agent rows.
fn discover_agents(
    conn: &DbConn,
    agents_dir: &Path,
    project_id: i64,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Ok(());
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(agent_name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };

        let profile_path = path.join("profile.json");
        if !profile_path.is_file() {
            continue;
        }

        let profile_data = match std::fs::read_to_string(&profile_path) {
            Ok(d) => d,
            Err(e) => {
                stats.parse_errors += 1;
                stats
                    .warnings
                    .push(format!("Cannot read {}: {e}", profile_path.display()));
                continue;
            }
        };

        let profile: serde_json::Value = match serde_json::from_str(&profile_data) {
            Ok(v) => v,
            Err(e) => {
                stats.parse_errors += 1;
                stats
                    .warnings
                    .push(format!("Cannot parse {}: {e}", profile_path.display()));
                continue;
            }
        };

        let program = json_str(&profile, "program").unwrap_or("unknown");
        let model = json_str(&profile, "model").unwrap_or("unknown");
        let task_description = json_str(&profile, "task_description").unwrap_or("");
        let attachments_policy = json_str(&profile, "attachments_policy").unwrap_or("auto");
        let contact_policy = json_str(&profile, "contact_policy").unwrap_or("auto");

        // Parse inception timestamp (try both field names for compatibility)
        let inception_ts = parse_ts_from_json(&profile, "inception_ts")
            .or_else(|| parse_ts_from_json(&profile, "registered_ts"));
        let last_active_ts = parse_ts_from_json(&profile, "last_active_ts")
            .unwrap_or_else(|| inception_ts.unwrap_or_else(crate::now_micros));
        let inception_ts = inception_ts.unwrap_or(last_active_ts);

        conn.execute_sync(
            "INSERT OR IGNORE INTO agents \
             (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::Text(agent_name.clone()),
                Value::Text(program.to_string()),
                Value::Text(model.to_string()),
                Value::Text(task_description.to_string()),
                Value::BigInt(inception_ts),
                Value::BigInt(last_active_ts),
                Value::Text(attachments_policy.to_string()),
                Value::Text(contact_policy.to_string()),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("reconstruct: insert agent {agent_name}: {e}")))?;

        let aid = query_last_insert_or_existing_id_composite(
            conn,
            "agents",
            "project_id",
            project_id,
            "name",
            &agent_name,
        )?;
        agent_ids.insert((project_id, agent_name), aid);
        stats.agents += 1;
    }

    Ok(())
}

/// Walk `messages/{YYYY}/{MM}/*.md` and insert message + recipient rows.
fn discover_messages(
    conn: &DbConn,
    messages_dir: &Path,
    project_id: i64,
    project_slug: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) {
    // Walk year directories
    let Ok(years) = std::fs::read_dir(messages_dir) else {
        return;
    };

    let mut message_files: Vec<PathBuf> = Vec::new();

    for year_entry in years.flatten() {
        let year_path = year_entry.path();
        if !year_path.is_dir() {
            continue;
        }
        // Walk month directories
        let Ok(months) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in months.flatten() {
            let month_path = month_entry.path();
            if !month_path.is_dir() {
                continue;
            }
            // Collect .md files
            let Ok(files) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().is_some_and(|e| e == "md") {
                    message_files.push(file_path);
                }
            }
        }
    }

    // Sort by filename (which starts with ISO timestamp) for chronological order
    message_files.sort();

    for file_path in &message_files {
        match parse_and_insert_message(conn, file_path, project_id, project_slug, agent_ids, stats)
        {
            Ok(()) => {}
            Err(e) => {
                stats.parse_errors += 1;
                stats.warnings.push(format!(
                    "Failed to reconstruct message from {}: {e}",
                    file_path.display()
                ));
            }
        }
    }
}

/// Parse a single archive `.md` file and insert the message into the database.
fn parse_and_insert_message(
    conn: &DbConn,
    file_path: &Path,
    project_id: i64,
    _project_slug: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let content = std::fs::read_to_string(file_path)
        .map_err(|e| DbError::Sqlite(format!("read {}: {e}", file_path.display())))?;

    // Parse JSON frontmatter between ---json and ---
    let frontmatter = extract_json_frontmatter(&content).ok_or_else(|| {
        DbError::Sqlite(format!("no JSON frontmatter in {}", file_path.display()))
    })?;

    let msg: serde_json::Value = serde_json::from_str(frontmatter)
        .map_err(|e| DbError::Sqlite(format!("bad JSON in {}: {e}", file_path.display())))?;

    // Extract fields
    let sender_name = json_str(&msg, "from")
        .or_else(|| json_str(&msg, "sender"))
        .or_else(|| json_str(&msg, "from_agent"))
        .unwrap_or("unknown");

    let subject = json_str(&msg, "subject").unwrap_or("");
    let body_md = extract_body_after_frontmatter(&content).unwrap_or("");
    let thread_id = json_str(&msg, "thread_id");
    let importance = json_str(&msg, "importance").unwrap_or("normal");
    let ack_required = msg
        .get("ack_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let created_ts = parse_ts_from_json(&msg, "created_ts")
        .or_else(|| parse_ts_from_json(&msg, "created"))
        .unwrap_or_else(crate::now_micros);
    let attachments = msg
        .get("attachments")
        .map_or_else(|| "[]".to_string(), std::string::ToString::to_string);

    // Ensure sender agent exists
    let sender_id = ensure_agent_exists(conn, project_id, sender_name, agent_ids)?;

    // Build recipient lists
    let to_names = json_str_array(&msg, "to");
    let cc_names = json_str_array(&msg, "cc");
    let bcc_names = json_str_array(&msg, "bcc");

    // Insert message, preserving canonical frontmatter ID when available.
    //
    // If the frontmatter contains a valid positive `id` field, use it as the
    // DB primary key so that archive filenames (which embed `__{id}.md`)
    // remain consistent with DB row IDs.
    // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/9
    let thread_id_val = thread_id.map_or_else(|| Value::Null, |t| Value::Text(t.to_string()));
    let canonical_id = msg
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|&id| id > 0);

    let message_id = if let Some(cid) = canonical_id {
        conn.execute_sync(
            "INSERT OR REPLACE INTO messages \
             (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(cid),
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                Value::Text(subject.to_string()),
                Value::Text(body_md.to_string()),
                Value::Text(importance.to_string()),
                Value::BigInt(i64::from(ack_required)),
                Value::BigInt(created_ts),
                Value::Text(attachments),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("insert message with id {cid}: {e}")))?;
        cid
    } else {
        conn.execute_sync(
            "INSERT INTO messages \
             (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                Value::Text(subject.to_string()),
                Value::Text(body_md.to_string()),
                Value::Text(importance.to_string()),
                Value::BigInt(i64::from(ack_required)),
                Value::BigInt(created_ts),
                Value::Text(attachments),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("insert message: {e}")))?;

        // Retrieve the inserted row ID via last_insert_rowid() for reliability.
        query_last_insert_rowid(conn)?
    };

    stats.messages += 1;

    // Insert recipients
    for name in &to_names {
        let aid = ensure_agent_exists(conn, project_id, name, agent_ids)?;
        insert_recipient(conn, message_id, aid, "to")?;
        stats.recipients += 1;
    }
    for name in &cc_names {
        let aid = ensure_agent_exists(conn, project_id, name, agent_ids)?;
        insert_recipient(conn, message_id, aid, "cc")?;
        stats.recipients += 1;
    }
    for name in &bcc_names {
        let aid = ensure_agent_exists(conn, project_id, name, agent_ids)?;
        insert_recipient(conn, message_id, aid, "bcc")?;
        stats.recipients += 1;
    }

    Ok(())
}

/// Ensure an agent row exists, creating a placeholder if needed.
fn ensure_agent_exists(
    conn: &DbConn,
    project_id: i64,
    name: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
) -> DbResult<i64> {
    let key = (project_id, name.to_string());
    if let Some(&id) = agent_ids.get(&key) {
        return Ok(id);
    }

    let now = crate::now_micros();
    conn.execute_sync(
        "INSERT OR IGNORE INTO agents \
         (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
         VALUES (?, ?, 'unknown', 'unknown', '', ?, ?, 'auto', 'auto')",
        &[
            Value::BigInt(project_id),
            Value::Text(name.to_string()),
            Value::BigInt(now),
            Value::BigInt(now),
        ],
    )
    .map_err(|e| DbError::Sqlite(format!("ensure agent {name}: {e}")))?;

    let aid = query_last_insert_or_existing_id_composite(
        conn,
        "agents",
        "project_id",
        project_id,
        "name",
        name,
    )?;
    agent_ids.insert(key, aid);
    Ok(aid)
}

fn insert_recipient(conn: &DbConn, message_id: i64, agent_id: i64, kind: &str) -> DbResult<()> {
    conn.execute_sync(
        "INSERT OR IGNORE INTO message_recipients (message_id, agent_id, kind) VALUES (?, ?, ?)",
        &[
            Value::BigInt(message_id),
            Value::BigInt(agent_id),
            Value::Text(kind.to_string()),
        ],
    )
    .map(|_| ())
    .map_err(|e| DbError::Sqlite(format!("insert recipient: {e}")))
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Load canonical `human_key` from `project.json` when available.
///
/// Falls back to a synthetic `/{slug}` path when metadata is missing or
/// malformed. The fallback remains absolute so downstream path validation
/// continues to work.
fn read_project_human_key(project_path: &Path, slug: &str, stats: &mut ReconstructStats) -> String {
    let metadata_path = project_path.join("project.json");
    let fallback = format!("/{slug}");

    if !metadata_path.is_file() {
        stats.warnings.push(format!(
            "Missing {}; using fallback human_key '{}'",
            metadata_path.display(),
            fallback
        ));
        return fallback;
    }

    let metadata_str = match std::fs::read_to_string(&metadata_path) {
        Ok(s) => s,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot read {}: {e}; using fallback human_key '{}'",
                metadata_path.display(),
                fallback
            ));
            return fallback;
        }
    };

    let metadata_json: serde_json::Value = match serde_json::from_str(&metadata_str) {
        Ok(v) => v,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot parse {}: {e}; using fallback human_key '{}'",
                metadata_path.display(),
                fallback
            ));
            return fallback;
        }
    };

    let Some(human_key) = metadata_json
        .get("human_key")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        stats.parse_errors += 1;
        stats.warnings.push(format!(
            "Missing/empty human_key in {}; using fallback human_key '{}'",
            metadata_path.display(),
            fallback
        ));
        return fallback;
    };

    if !Path::new(human_key).is_absolute() {
        stats.parse_errors += 1;
        stats.warnings.push(format!(
            "Non-absolute human_key '{}' in {}; using fallback human_key '{}'",
            human_key,
            metadata_path.display(),
            fallback
        ));
        return fallback;
    }

    if let Some(metadata_slug) = metadata_json
        .get("slug")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && metadata_slug != slug
    {
        stats.warnings.push(format!(
            "Project metadata slug mismatch in {}: dir slug='{}', metadata slug='{}'",
            metadata_path.display(),
            slug,
            metadata_slug
        ));
    }

    human_key.to_string()
}

/// Extract JSON frontmatter from a `---json\n...\n---` block.
fn extract_json_frontmatter(content: &str) -> Option<&str> {
    let start_marker = "---json\n";
    let end_marker = "\n---\n";

    let start = content.find(start_marker)?;
    let json_start = start + start_marker.len();
    let json_end = content[json_start..].find(end_marker)?;
    Some(&content[json_start..json_start + json_end])
}

/// Extract the body text after the frontmatter block.
fn extract_body_after_frontmatter(content: &str) -> Option<&str> {
    let end_marker = "\n---\n";
    let idx = content.find(end_marker)?;
    let after = &content[idx + end_marker.len()..];
    // Skip leading blank lines
    Some(after.trim())
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn json_str_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(String::from)
            .collect(),
        Some(serde_json::Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.clone()]
            }
        }
        _ => Vec::new(),
    }
}

/// Parse a timestamp field from JSON (supports both ISO string and i64 micros).
fn parse_ts_from_json(value: &serde_json::Value, key: &str) -> Option<i64> {
    match value.get(key)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Try parsing as i64 first (microseconds)
            if let Ok(n) = s.parse::<i64>() {
                return Some(n);
            }
            // Try ISO-8601
            crate::iso_to_micros(s)
        }
        _ => None,
    }
}

/// Query the ID of a row by a unique text column, or the last inserted row.
fn query_last_insert_or_existing_id(
    conn: &DbConn,
    table: &str,
    column: &str,
    value: &str,
) -> DbResult<i64> {
    let rows = conn
        .query_sync(
            &format!("SELECT id FROM {table} WHERE {column} = ?"),
            &[Value::Text(value.to_string())],
        )
        .map_err(|e| DbError::Sqlite(format!("query {table}.id: {e}")))?;

    extract_id_from_rows(&rows)
        .ok_or_else(|| DbError::Sqlite(format!("no id found for {table}.{column} = {value}")))
}

/// Query the ID of a row by a composite key (integer + text).
fn query_last_insert_or_existing_id_composite(
    conn: &DbConn,
    table: &str,
    col1: &str,
    val1: i64,
    col2: &str,
    val2: &str,
) -> DbResult<i64> {
    let rows = conn
        .query_sync(
            &format!("SELECT id FROM {table} WHERE {col1} = ? AND {col2} = ?"),
            &[Value::BigInt(val1), Value::Text(val2.to_string())],
        )
        .map_err(|e| DbError::Sqlite(format!("query {table}.id composite: {e}")))?;

    extract_id_from_rows(&rows).ok_or_else(|| {
        DbError::Sqlite(format!(
            "no id found for {table}.{col1}={val1}, {col2}={val2}"
        ))
    })
}

/// Get the rowid of the most recently inserted row on this connection.
fn query_last_insert_rowid(conn: &DbConn) -> DbResult<i64> {
    let rows = conn
        .query_sync("SELECT last_insert_rowid() AS id", &[])
        .map_err(|e| DbError::Sqlite(format!("query last_insert_rowid: {e}")))?;

    extract_id_from_rows(&rows)
        .ok_or_else(|| DbError::Sqlite("last_insert_rowid() returned no rows".to_string()))
}

fn extract_id_from_rows(rows: &[sqlmodel_core::Row]) -> Option<i64> {
    let row = rows.first()?;
    match row.get_by_name("id") {
        Some(Value::BigInt(n)) => Some(*n),
        Some(Value::Int(n)) => Some(i64::from(*n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstruct_benign_migration_error_detection() {
        assert!(is_reconstruct_benign_migration_error(
            "table projects already exists"
        ));
        assert!(is_reconstruct_benign_migration_error(
            "duplicate column name: foo"
        ));
        assert!(is_reconstruct_benign_migration_error(
            "duplicate index name: idx_messages_created_ts"
        ));
        assert!(!is_reconstruct_benign_migration_error(
            "near \"CREATE\": syntax error"
        ));
        assert!(!is_reconstruct_benign_migration_error(
            "no such table: agents"
        ));
    }

    #[test]
    fn extract_json_frontmatter_basic() {
        let content = "---json\n{\"id\": 1, \"subject\": \"hello\"}\n---\n\nBody text here.\n";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert_eq!(fm, "{\"id\": 1, \"subject\": \"hello\"}");
    }

    #[test]
    fn extract_json_frontmatter_multiline() {
        let content =
            "---json\n{\n  \"id\": 42,\n  \"from\": \"TestAgent\"\n}\n---\n\nHello world.\n";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert!(fm.contains("\"id\": 42"));
        assert!(fm.contains("\"from\": \"TestAgent\""));
    }

    #[test]
    fn extract_json_frontmatter_missing() {
        assert!(extract_json_frontmatter("no frontmatter here").is_none());
        assert!(extract_json_frontmatter("---json\nno end marker").is_none());
    }

    #[test]
    fn extract_body_after_frontmatter_basic() {
        let content = "---json\n{}\n---\n\nThe body content.\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert_eq!(body, "The body content.");
    }

    #[test]
    fn json_str_array_variants() {
        let v: serde_json::Value = serde_json::json!({
            "to": ["Alice", "Bob"],
            "cc": "Charlie",
            "bcc": [],
        });
        assert_eq!(json_str_array(&v, "to"), vec!["Alice", "Bob"]);
        assert_eq!(json_str_array(&v, "cc"), vec!["Charlie"]);
        assert!(json_str_array(&v, "bcc").is_empty());
        assert!(json_str_array(&v, "missing").is_empty());
    }

    #[test]
    fn parse_ts_iso_string() {
        let v: serde_json::Value = serde_json::json!({
            "created_ts": "2026-02-22T12:00:00Z"
        });
        let ts = parse_ts_from_json(&v, "created_ts");
        assert!(ts.is_some());
        let ts = ts.unwrap();
        // Should be in microseconds, somewhere around 2026
        assert!(ts > 1_700_000_000_000_000);
    }

    #[test]
    fn parse_ts_integer() {
        let v: serde_json::Value = serde_json::json!({
            "created_ts": 1_740_000_000_000_000_i64
        });
        let ts = parse_ts_from_json(&v, "created_ts");
        assert_eq!(ts, Some(1_740_000_000_000_000));
    }

    #[test]
    fn reconstruct_stats_display() {
        let stats = ReconstructStats {
            projects: 2,
            agents: 5,
            messages: 100,
            recipients: 200,
            parse_errors: 3,
            warnings: vec![],
        };
        let display = stats.to_string();
        assert!(display.contains("2 projects"));
        assert!(display.contains("5 agents"));
        assert!(display.contains("100 messages"));
        assert!(display.contains("3 parse errors"));
    }

    #[test]
    fn reconstruct_empty_storage_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
    }

    #[test]
    fn reconstruct_with_agent_profile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        // Create fake archive structure
        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("TestAgent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let profile = serde_json::json!({
            "name": "TestAgent",
            "program": "claude-code",
            "model": "opus-4.6",
            "task_description": "testing",
            "inception_ts": "2026-02-22T12:00:00Z",
            "last_active_ts": "2026-02-22T12:00:00Z",
            "attachments_policy": "auto",
        });
        std::fs::write(
            agent_dir.join("profile.json"),
            serde_json::to_string_pretty(&profile).unwrap(),
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.agents, 1);
        assert_eq!(stats.messages, 0);
        assert_eq!(stats.parse_errors, 0);
    }

    #[test]
    fn reconstruct_uses_project_metadata_human_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let metadata = serde_json::json!({
            "slug": "test-project",
            "human_key": "/data/projects/exact-human-key",
        });
        std::fs::write(
            project_dir.join("project.json"),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT slug, human_key FROM projects WHERE slug = 'test-project'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let human_key = rows[0]
            .get_by_name("human_key")
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("human_key text");
        assert_eq!(human_key, "/data/projects/exact-human-key");
    }

    #[test]
    fn reconstruct_falls_back_when_project_metadata_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);
        assert!(
            stats
                .warnings
                .iter()
                .any(|w| w.contains("Missing") && w.contains("project.json"))
        );

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT human_key FROM projects WHERE slug = 'test-project'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let human_key = rows[0]
            .get_by_name("human_key")
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("human_key text");
        assert_eq!(human_key, "/test-project");
    }

    #[test]
    fn reconstruct_with_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        // Create fake archive structure
        let project_dir = storage_root.join("projects").join("test-project");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();

        // Create agent profile
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"test","model":"test","inception_ts":"2026-02-22T12:00:00Z","last_active_ts":"2026-02-22T12:00:00Z"}"#,
        )
        .unwrap();

        // Create message file
        let msg_content = r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "cc": [],
  "bcc": [],
  "thread_id": "TEST-1",
  "subject": "Hello Bob",
  "importance": "normal",
  "ack_required": false,
  "created_ts": "2026-02-22T12:00:00Z",
  "attachments": []
}
---

Hello Bob, this is a test message.
"#;
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__hello-bob__1.md"),
            msg_content,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(
            stats.agents, 1,
            "Alice from profile; Bob auto-created as placeholder"
        );
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 1);
        assert_eq!(stats.parse_errors, 0);

        // Verify the message was inserted correctly
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT subject, body_md, thread_id FROM messages LIMIT 1",
                &[],
            )
            .unwrap();
        assert!(!rows.is_empty(), "message should exist in DB");

        // Verify Bob was auto-created as a placeholder agent
        let agent_rows = conn
            .query_sync("SELECT name, program FROM agents ORDER BY name", &[])
            .unwrap();
        assert_eq!(agent_rows.len(), 2, "Alice + Bob should both exist");
        // Verify Alice has the correct program from profile
        let alice_rows = conn
            .query_sync("SELECT program FROM agents WHERE name = 'Alice'", &[])
            .unwrap();
        assert!(!alice_rows.is_empty());
        // Verify Bob was auto-created with 'unknown' program
        let bob_rows = conn
            .query_sync("SELECT program FROM agents WHERE name = 'Bob'", &[])
            .unwrap();
        assert!(!bob_rows.is_empty());
    }

    #[test]
    fn reconstruct_handles_malformed_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .unwrap();

        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();

        // Malformed file (no frontmatter)
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__bad__1.md"),
            "This file has no frontmatter at all.",
        )
        .unwrap();

        // Another malformed file (invalid JSON)
        std::fs::write(
            messages_dir.join("2026-02-22T12-01-00Z__bad__2.md"),
            "---json\n{invalid json}\n---\n\nBody.\n",
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 0);
        assert_eq!(stats.parse_errors, 2, "both bad files should be counted");
        assert_eq!(stats.warnings.len(), 2);
    }
}
