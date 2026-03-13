//! Reconstruct a `SQLite` database from the Git archive.
//!
//! When the database file is corrupt and no healthy backup exists, this module
//! walks the per-project Git archive directories to recover:
//!
//! - **Projects** — from subdirectory names under `{storage_root}/projects/`
//!   plus optional `project.json` metadata for exact `human_key` recovery
//! - **Agents** — from `agents/{name}/profile.json` files
//! - **File reservations** — from `file_reservations/*.json` artifacts
//! - **Messages** — from `messages/{YYYY}/{MM}/*.md` files (JSON frontmatter)
//! - **Message recipients** — from the `to`, `cc`, `bcc` arrays in frontmatter
//!
//! The reconstructed database will be missing:
//! - `read_ts` / `ack_ts` on `message_recipients` (no archive artifact for these)
//! - `agent_links` / contacts (handshake state not archived)
//! - `products` / `product_project_links` (not archived)
//!
//! These are acceptable losses because contacts are transient, and the core
//! archive-backed data is fully recovered.

use crate::error::{DbError, DbResult};
use crate::schema;
use sqlmodel_core::Value;
use sqlmodel_sqlite::SqliteConnection as DbConn;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

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
    /// Number of projects recovered only from a salvaged database.
    pub salvaged_projects: usize,
    /// Number of agents recovered only from a salvaged database.
    pub salvaged_agents: usize,
    /// Number of messages recovered only from a salvaged database.
    pub salvaged_messages: usize,
    /// Number of recipient rows inserted or state rows updated from a salvaged database.
    pub salvaged_recipients: usize,
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
        )?;
        if self.salvaged_projects > 0
            || self.salvaged_agents > 0
            || self.salvaged_messages > 0
            || self.salvaged_recipients > 0
        {
            write!(
                f,
                "; salvaged {} projects, {} agents, {} messages ({} recipients/state updates)",
                self.salvaged_projects,
                self.salvaged_agents,
                self.salvaged_messages,
                self.salvaged_recipients
            )?;
        }
        Ok(())
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
    let mut stats = ReconstructStats::default();
    if !is_real_directory(storage_root) {
        stats.warnings.push(format!(
            "Storage root {} is missing or not a real directory",
            storage_root.display()
        ));
        return Ok(stats);
    }

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
    // This mirrors `schema::enforce_runtime_fts_cleanup`, but uses canonical
    // SQLite so reconstruction is not coupled to runtime connection type.
    let cleanup_sql = [
        "DROP TRIGGER IF EXISTS fts_messages_ai",
        "DROP TRIGGER IF EXISTS fts_messages_ad",
        "DROP TRIGGER IF EXISTS fts_messages_au",
        "DROP TRIGGER IF EXISTS messages_ai",
        "DROP TRIGGER IF EXISTS messages_ad",
        "DROP TRIGGER IF EXISTS messages_au",
        "DROP TRIGGER IF EXISTS agents_ai",
        "DROP TRIGGER IF EXISTS agents_ad",
        "DROP TRIGGER IF EXISTS agents_au",
        "DROP TRIGGER IF EXISTS projects_ai",
        "DROP TRIGGER IF EXISTS projects_ad",
        "DROP TRIGGER IF EXISTS projects_au",
        "DROP TABLE IF EXISTS fts_agents",
        "DROP TABLE IF EXISTS fts_projects",
        "DROP TABLE IF EXISTS fts_messages",
    ];
    for stmt in cleanup_sql {
        conn.execute_raw(stmt)
            .map_err(|e| DbError::Sqlite(format!("reconstruct: fts cleanup ({stmt}): {e}")))?;
    }

    // Maps for deduplication: ((project_id, name) → agent_id)
    let mut agent_ids: HashMap<(i64, String), i64> = HashMap::new();

    let projects_dir = storage_root.join("projects");
    if !is_real_directory(&projects_dir) {
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
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
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
        if is_real_directory(&agents_dir) {
            discover_agents(&conn, &agents_dir, pid, &mut agent_ids, &mut stats)?;
        }

        // Phase 2b: Recover archived file reservations so robot/status reads can
        // rebuild the same project-scoped lease view from the archive alone.
        let reservations_dir = project_path.join("file_reservations");
        if is_real_directory(&reservations_dir) {
            discover_file_reservations(&conn, &reservations_dir, pid, &mut agent_ids, &mut stats)?;
        }

        // Phase 3: Discover messages for this project
        let messages_dir = project_path.join("messages");
        if is_real_directory(&messages_dir) {
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

/// Reconstruct the database from the Git archive and then best-effort merge
/// any additional durable state from a salvaged `SQLite` database.
///
/// This is intended for doctor/recovery flows where the primary database file
/// was corrupt, but `sqlite3 .recover` or similar tooling could still extract
/// additional rows that never made it into the Git archive.
pub fn reconstruct_from_archive_with_salvage(
    db_path: &Path,
    storage_root: &Path,
    salvage_db_path: Option<&Path>,
) -> DbResult<ReconstructStats> {
    let mut stats = reconstruct_from_archive(db_path, storage_root)?;
    if let Some(salvage_db_path) = salvage_db_path.filter(|path| is_real_file(path)) {
        merge_salvaged_database(db_path, salvage_db_path, &mut stats)?;
    }
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
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(agent_name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };

        let profile_path = path.join("profile.json");
        if !is_real_file(&profile_path) {
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
        let Ok(year_type) = year_entry.file_type() else {
            continue;
        };
        if !year_type.is_dir() || year_type.is_symlink() {
            continue;
        }
        // Walk month directories
        let Ok(months) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in months.flatten() {
            let month_path = month_entry.path();
            let Ok(month_type) = month_entry.file_type() else {
                continue;
            };
            if !month_type.is_dir() || month_type.is_symlink() {
                continue;
            }
            // Collect .md files
            let Ok(files) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                let Ok(file_type) = file_entry.file_type() else {
                    continue;
                };
                if file_type.is_file()
                    && !file_type.is_symlink()
                    && file_path.extension().is_some_and(|e| e == "md")
                {
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
#[allow(clippy::too_many_lines)]
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
    let sender_name = normalized_archive_agent_name(
        json_str(&msg, "from")
            .or_else(|| json_str(&msg, "sender"))
            .or_else(|| json_str(&msg, "from_agent")),
    )
    .unwrap_or_else(|| "unknown".to_string());

    let subject = json_str(&msg, "subject").unwrap_or("");
    let body_md = extract_body_after_frontmatter(&content).unwrap_or("");
    let raw_thread_id = json_str(&msg, "thread_id");
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
    let sender_id = ensure_agent_exists(conn, project_id, &sender_name, agent_ids)?;

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
    let canonical_id = msg
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|&id| id > 0);

    if let Some(cid) = canonical_id
        && message_id_exists(conn, cid)?
    {
        stats.warnings.push(format!(
            "Duplicate canonical message id {cid} in {}; keeping the first archive artifact and skipping the duplicate",
            file_path.display()
        ));
        return Ok(());
    }

    let thread_id = raw_thread_id.and_then(|raw| {
        let normalized = sanitize_reconstructed_thread_id(raw);
        if normalized.as_deref() != Some(raw) {
            stats.warnings.push(format!(
                "Sanitized invalid thread_id {:?} in {} during reconstruction",
                raw,
                file_path.display()
            ));
        }
        normalized
    });
    let thread_id_val = thread_id
        .as_deref()
        .map_or_else(|| Value::Null, |t| Value::Text(t.to_string()));

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

struct ArchivedFileReservation {
    reservation_id: Option<i64>,
    agent_name: String,
    path_pattern: String,
    exclusive: bool,
    reason: String,
    created_ts: i64,
    expires_ts: i64,
    released_ts: Option<i64>,
}

fn reservation_artifact_paths(reservations_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(reservations_dir) else {
        return Vec::new();
    };

    let mut reservation_files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file()
            && !file_type.is_symlink()
            && path.extension().is_some_and(|ext| ext == "json")
        {
            reservation_files.push(path);
        }
    }
    reservation_files.sort();
    reservation_files
}

fn parse_archived_file_reservation(
    file_path: &Path,
    stats: &mut ReconstructStats,
) -> Option<ArchivedFileReservation> {
    let reservation_data = match std::fs::read_to_string(file_path) {
        Ok(data) => data,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot read reservation artifact {}: {e}",
                file_path.display()
            ));
            return None;
        }
    };

    let reservation: serde_json::Value = match serde_json::from_str(&reservation_data) {
        Ok(value) => value,
        Err(e) => {
            stats.parse_errors += 1;
            stats.warnings.push(format!(
                "Cannot parse reservation artifact {}: {e}",
                file_path.display()
            ));
            return None;
        }
    };

    let Some(path_pattern) = json_str(&reservation, "path_pattern")
        .or_else(|| json_str(&reservation, "path"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        stats.parse_errors += 1;
        stats.warnings.push(format!(
            "Reservation artifact {} is missing path_pattern/path",
            file_path.display()
        ));
        return None;
    };

    let agent_name = normalized_archive_agent_name(json_str(&reservation, "agent"))
        .unwrap_or_else(|| "unknown".to_string());
    let exclusive = reservation
        .get("exclusive")
        .and_then(|value| value.as_bool().or_else(|| value.as_i64().map(|n| n != 0)))
        .unwrap_or(true);
    let reason = json_str(&reservation, "reason").unwrap_or("").to_string();
    let created_ts =
        parse_ts_from_json(&reservation, "created_ts").unwrap_or_else(crate::now_micros);
    let expires_ts = parse_ts_from_json(&reservation, "expires_ts").unwrap_or(created_ts);
    let released_ts = parse_ts_from_json(&reservation, "released_ts");
    let reservation_id = reservation
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0);

    Some(ArchivedFileReservation {
        reservation_id,
        agent_name,
        path_pattern,
        exclusive,
        reason,
        created_ts,
        expires_ts,
        released_ts,
    })
}

fn insert_archived_file_reservation(
    conn: &DbConn,
    project_id: i64,
    reservation: &ArchivedFileReservation,
    file_path: &Path,
    agent_ids: &mut HashMap<(i64, String), i64>,
) -> DbResult<()> {
    let agent_id = ensure_agent_exists(conn, project_id, &reservation.agent_name, agent_ids)?;

    if let Some(id) = reservation.reservation_id {
        conn.execute_sync(
            "INSERT OR REPLACE INTO file_reservations \
             (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(id),
                Value::BigInt(project_id),
                Value::BigInt(agent_id),
                Value::Text(reservation.path_pattern.clone()),
                Value::BigInt(i64::from(reservation.exclusive)),
                Value::Text(reservation.reason.clone()),
                Value::BigInt(reservation.created_ts),
                Value::BigInt(reservation.expires_ts),
                reservation.released_ts.map_or(Value::Null, Value::BigInt),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: insert file reservation {}: {e}",
                file_path.display()
            ))
        })?;
    } else {
        conn.execute_sync(
            "INSERT INTO file_reservations \
             (project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(agent_id),
                Value::Text(reservation.path_pattern.clone()),
                Value::BigInt(i64::from(reservation.exclusive)),
                Value::Text(reservation.reason.clone()),
                Value::BigInt(reservation.created_ts),
                Value::BigInt(reservation.expires_ts),
                reservation.released_ts.map_or(Value::Null, Value::BigInt),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct: insert file reservation {}: {e}",
                file_path.display()
            ))
        })?;
    }

    Ok(())
}

fn discover_file_reservations(
    conn: &DbConn,
    reservations_dir: &Path,
    project_id: i64,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    for file_path in reservation_artifact_paths(reservations_dir) {
        let Some(reservation) = parse_archived_file_reservation(&file_path, stats) else {
            continue;
        };
        insert_archived_file_reservation(conn, project_id, &reservation, &file_path, agent_ids)?;
    }

    Ok(())
}

fn sanitize_reconstructed_thread_id(raw: &str) -> Option<String> {
    let sanitized: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .take(128)
        .collect();
    if sanitized.is_empty() || !sanitized.as_bytes()[0].is_ascii_alphanumeric() {
        None
    } else {
        Some(sanitized)
    }
}

fn message_id_exists(conn: &DbConn, message_id: i64) -> DbResult<bool> {
    let rows = conn
        .query_sync(
            "SELECT 1 AS exists_flag FROM messages WHERE id = ? LIMIT 1",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| DbError::Sqlite(format!("check message {message_id} existence: {e}")))?;
    Ok(!rows.is_empty())
}

fn table_exists(conn: &DbConn, table: &str) -> DbResult<bool> {
    let rows = conn
        .query_sync(
            "SELECT 1 AS exists_flag FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            &[Value::Text(table.to_string())],
        )
        .map_err(|e| DbError::Sqlite(format!("check table {table} existence: {e}")))?;
    Ok(!rows.is_empty())
}

fn table_columns(conn: &DbConn, table: &str) -> DbResult<HashSet<String>> {
    let rows = conn
        .query_sync(&format!("PRAGMA table_info({table})"), &[])
        .map_err(|e| DbError::Sqlite(format!("inspect columns for {table}: {e}")))?;
    let mut columns = HashSet::new();
    for row in &rows {
        if let Ok(name) = row.get_named::<String>("name") {
            columns.insert(name);
        }
    }
    Ok(columns)
}

fn build_salvage_select(
    table: &str,
    columns: &HashSet<String>,
    required: &[&str],
    optional: &[&str],
    stats: &mut ReconstructStats,
    salvage_db_path: &Path,
) -> Option<String> {
    let missing_required: Vec<&str> = required
        .iter()
        .copied()
        .filter(|column| !columns.contains(*column))
        .collect();
    if !missing_required.is_empty() {
        stats.warnings.push(format!(
            "Salvage database {} table {table} missing required column(s): {}",
            salvage_db_path.display(),
            missing_required.join(", ")
        ));
        return None;
    }

    let mut selected = required
        .iter()
        .map(|column| (*column).to_string())
        .collect::<Vec<_>>();
    selected.extend(
        optional
            .iter()
            .copied()
            .filter(|column| columns.contains(*column))
            .map(str::to_string),
    );
    Some(selected.join(", "))
}

#[allow(clippy::too_many_lines)]
fn merge_salvaged_database(
    target_db_path: &Path,
    salvage_db_path: &Path,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let target_conn =
        DbConn::open_file(target_db_path.to_string_lossy().as_ref()).map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: cannot open target {}: {e}",
                target_db_path.display()
            ))
        })?;
    let salvage_conn =
        DbConn::open_file(salvage_db_path.to_string_lossy().as_ref()).map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: cannot open salvage {}: {e}",
                salvage_db_path.display()
            ))
        })?;

    let has_projects = table_exists(&salvage_conn, "projects")?;
    let has_agents = table_exists(&salvage_conn, "agents")?;
    let has_messages = table_exists(&salvage_conn, "messages")?;
    let has_recipients = table_exists(&salvage_conn, "message_recipients")?;

    if !(has_projects || has_agents || has_messages || has_recipients) {
        stats.warnings.push(format!(
            "Salvage database {} contained none of the expected mail tables",
            salvage_db_path.display()
        ));
        return Ok(());
    }

    let mut project_id_map: HashMap<i64, i64> = HashMap::new();
    let mut agent_id_map: HashMap<i64, i64> = HashMap::new();

    if has_projects {
        let project_columns = table_columns(&salvage_conn, "projects")?;
        let Some(project_select) = build_salvage_select(
            "projects",
            &project_columns,
            &["id", "slug"],
            &["human_key", "created_at"],
            stats,
            salvage_db_path,
        ) else {
            return Ok(());
        };
        let project_rows = salvage_conn
            .query_sync(
                &format!("SELECT {project_select} FROM projects ORDER BY id"),
                &[],
            )
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query projects: {e}")))?;

        for row in &project_rows {
            let Some(source_project_id) =
                row.get_named::<i64>("id").ok().filter(|value| *value > 0)
            else {
                continue;
            };
            let slug = row
                .get_named::<String>("slug")
                .unwrap_or_default()
                .trim()
                .to_string();
            if slug.is_empty() {
                stats.warnings.push(format!(
                    "Salvage database {} had a project row with empty slug; skipping",
                    salvage_db_path.display()
                ));
                continue;
            }

            if let Ok(target_project_id) =
                query_last_insert_or_existing_id(&target_conn, "projects", "slug", &slug)
            {
                project_id_map.insert(source_project_id, target_project_id);
                continue;
            }

            let human_key = row
                .get_named::<String>("human_key")
                .unwrap_or_else(|_| format!("/{slug}"));
            let created_at = row
                .get_named::<i64>("created_at")
                .unwrap_or_else(|_| crate::now_micros());
            target_conn
                .execute_sync(
                    "INSERT OR IGNORE INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
                    &[
                        Value::Text(slug.clone()),
                        Value::Text(human_key),
                        Value::BigInt(created_at),
                    ],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: insert project {slug}: {e}"))
                })?;
            let target_project_id =
                query_last_insert_or_existing_id(&target_conn, "projects", "slug", &slug)?;
            project_id_map.insert(source_project_id, target_project_id);
            stats.salvaged_projects += 1;
        }
    }

    if has_agents {
        let agent_columns = table_columns(&salvage_conn, "agents")?;
        let Some(agent_select) = build_salvage_select(
            "agents",
            &agent_columns,
            &["id", "project_id", "name"],
            &[
                "program",
                "model",
                "task_description",
                "inception_ts",
                "last_active_ts",
                "attachments_policy",
                "contact_policy",
            ],
            stats,
            salvage_db_path,
        ) else {
            return Ok(());
        };
        let agent_rows = salvage_conn
            .query_sync(
                &format!("SELECT {agent_select} FROM agents ORDER BY id"),
                &[],
            )
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query agents: {e}")))?;

        for row in &agent_rows {
            let Some(source_agent_id) = row.get_named::<i64>("id").ok().filter(|value| *value > 0)
            else {
                continue;
            };
            let Some(source_project_id) = row
                .get_named::<i64>("project_id")
                .ok()
                .filter(|value| *value > 0)
            else {
                continue;
            };
            let Some(&target_project_id) = project_id_map.get(&source_project_id) else {
                stats.warnings.push(format!(
                    "Salvage agent {source_agent_id} referenced missing project id {source_project_id}; skipping"
                ));
                continue;
            };

            let name = row
                .get_named::<String>("name")
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                stats.warnings.push(format!(
                    "Salvage database {} had an agent row with empty name; skipping",
                    salvage_db_path.display()
                ));
                continue;
            }

            let existed = query_last_insert_or_existing_id_composite(
                &target_conn,
                "agents",
                "project_id",
                target_project_id,
                "name",
                &name,
            )
            .ok();

            target_conn
                .execute_sync(
                    "INSERT OR IGNORE INTO agents \
                     (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    &[
                        Value::BigInt(target_project_id),
                        Value::Text(name.clone()),
                        Value::Text(
                            row.get_named::<String>("program")
                                .unwrap_or_else(|_| "unknown".to_string()),
                        ),
                        Value::Text(
                            row.get_named::<String>("model")
                                .unwrap_or_else(|_| "unknown".to_string()),
                        ),
                        Value::Text(
                            row.get_named::<String>("task_description")
                                .unwrap_or_default(),
                        ),
                        Value::BigInt(
                            row.get_named::<i64>("inception_ts")
                                .unwrap_or_else(|_| crate::now_micros()),
                        ),
                        Value::BigInt(
                            row.get_named::<i64>("last_active_ts")
                                .unwrap_or_else(|_| crate::now_micros()),
                        ),
                        Value::Text(
                            row.get_named::<String>("attachments_policy")
                                .unwrap_or_else(|_| "auto".to_string()),
                        ),
                        Value::Text(
                            row.get_named::<String>("contact_policy")
                                .unwrap_or_else(|_| "auto".to_string()),
                        ),
                    ],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: insert agent {name}: {e}"))
                })?;

            let target_agent_id = query_last_insert_or_existing_id_composite(
                &target_conn,
                "agents",
                "project_id",
                target_project_id,
                "name",
                &name,
            )?;
            agent_id_map.insert(source_agent_id, target_agent_id);
            if existed.is_none() {
                stats.salvaged_agents += 1;
            }
        }
    }

    if has_messages {
        let message_columns = table_columns(&salvage_conn, "messages")?;
        let Some(message_select) = build_salvage_select(
            "messages",
            &message_columns,
            &["id", "project_id", "sender_id"],
            &[
                "thread_id",
                "subject",
                "body_md",
                "importance",
                "ack_required",
                "created_ts",
                "attachments",
            ],
            stats,
            salvage_db_path,
        ) else {
            return Ok(());
        };
        let message_rows = salvage_conn
            .query_sync(
                &format!("SELECT {message_select} FROM messages ORDER BY id"),
                &[],
            )
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query messages: {e}")))?;

        for row in &message_rows {
            let Some(message_id) = row.get_named::<i64>("id").ok().filter(|value| *value > 0)
            else {
                continue;
            };
            if message_id_exists(&target_conn, message_id)? {
                continue;
            }

            let Some(source_project_id) = row
                .get_named::<i64>("project_id")
                .ok()
                .filter(|value| *value > 0)
            else {
                continue;
            };
            let Some(&target_project_id) = project_id_map.get(&source_project_id) else {
                stats.warnings.push(format!(
                    "Salvage message {message_id} referenced missing project id {source_project_id}; skipping"
                ));
                continue;
            };

            let Some(source_sender_id) = row
                .get_named::<i64>("sender_id")
                .ok()
                .filter(|value| *value > 0)
            else {
                continue;
            };
            let Some(&target_sender_id) = agent_id_map.get(&source_sender_id) else {
                stats.warnings.push(format!(
                    "Salvage message {message_id} referenced missing sender id {source_sender_id}; skipping"
                ));
                continue;
            };

            let thread_id = row
                .get_named::<String>("thread_id")
                .ok()
                .and_then(|raw| sanitize_reconstructed_thread_id(&raw));
            let thread_value = thread_id.map_or(Value::Null, Value::Text);

            target_conn
                .execute_sync(
                    "INSERT OR IGNORE INTO messages \
                     (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    &[
                        Value::BigInt(message_id),
                        Value::BigInt(target_project_id),
                        Value::BigInt(target_sender_id),
                        thread_value,
                        Value::Text(row.get_named::<String>("subject").unwrap_or_default()),
                        Value::Text(row.get_named::<String>("body_md").unwrap_or_default()),
                        Value::Text(
                            row.get_named::<String>("importance")
                                .unwrap_or_else(|_| "normal".to_string()),
                        ),
                        Value::BigInt(i64::from(
                            row.get_named::<i64>("ack_required").unwrap_or(0) != 0,
                        )),
                        Value::BigInt(
                            row.get_named::<i64>("created_ts")
                                .unwrap_or_else(|_| crate::now_micros()),
                        ),
                        Value::Text(
                            row.get_named::<String>("attachments")
                                .unwrap_or_else(|_| "[]".to_string()),
                        ),
                    ],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: insert message {message_id}: {e}"))
                })?;
            stats.salvaged_messages += 1;
        }
    }

    if has_recipients {
        let recipient_columns = table_columns(&salvage_conn, "message_recipients")?;
        let Some(recipient_select) = build_salvage_select(
            "message_recipients",
            &recipient_columns,
            &["message_id", "agent_id", "kind"],
            &["read_ts", "ack_ts"],
            stats,
            salvage_db_path,
        ) else {
            return Ok(());
        };
        let recipient_rows = salvage_conn
            .query_sync(
                &format!(
                    "SELECT {recipient_select} FROM message_recipients ORDER BY message_id, agent_id, kind"
                ),
                &[],
            )
            .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query recipients: {e}")))?;

        for row in &recipient_rows {
            let Some(message_id) = row
                .get_named::<i64>("message_id")
                .ok()
                .filter(|value| *value > 0)
            else {
                continue;
            };
            if !message_id_exists(&target_conn, message_id)? {
                continue;
            }

            let Some(source_agent_id) = row
                .get_named::<i64>("agent_id")
                .ok()
                .filter(|value| *value > 0)
            else {
                continue;
            };
            let Some(&target_agent_id) = agent_id_map.get(&source_agent_id) else {
                continue;
            };
            let kind = row
                .get_named::<String>("kind")
                .unwrap_or_else(|_| "to".to_string());
            let read_ts = row.get_named::<i64>("read_ts").ok();
            let ack_ts = row.get_named::<i64>("ack_ts").ok();

            let existing_rows = target_conn
                .query_sync(
                    "SELECT read_ts, ack_ts FROM message_recipients \
                     WHERE message_id = ? AND agent_id = ? AND kind = ? LIMIT 1",
                    &[
                        Value::BigInt(message_id),
                        Value::BigInt(target_agent_id),
                        Value::Text(kind.clone()),
                    ],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!(
                        "reconstruct salvage: query recipient state for message {message_id}: {e}"
                    ))
                })?;

            if existing_rows.is_empty() {
                target_conn
                    .execute_sync(
                        "INSERT OR IGNORE INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) \
                         VALUES (?, ?, ?, ?, ?)",
                        &[
                            Value::BigInt(message_id),
                            Value::BigInt(target_agent_id),
                            Value::Text(kind),
                            read_ts.map_or(Value::Null, Value::BigInt),
                            ack_ts.map_or(Value::Null, Value::BigInt),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: insert recipient for message {message_id}: {e}"
                        ))
                    })?;
                stats.salvaged_recipients += 1;
                continue;
            }

            let existing_row = &existing_rows[0];
            let current_read_ts = existing_row.get_named::<i64>("read_ts").ok();
            let current_ack_ts = existing_row.get_named::<i64>("ack_ts").ok();
            if current_read_ts.is_none() && read_ts.is_some()
                || current_ack_ts.is_none() && ack_ts.is_some()
            {
                target_conn
                    .execute_sync(
                        "UPDATE message_recipients SET \
                             read_ts = COALESCE(read_ts, ?), \
                             ack_ts = COALESCE(ack_ts, ?) \
                         WHERE message_id = ? AND agent_id = ? AND kind = ?",
                        &[
                            read_ts.map_or(Value::Null, Value::BigInt),
                            ack_ts.map_or(Value::Null, Value::BigInt),
                            Value::BigInt(message_id),
                            Value::BigInt(target_agent_id),
                            Value::Text(kind),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: update recipient state for message {message_id}: {e}"
                        ))
                    })?;
                stats.salvaged_recipients += 1;
            }
        }
    }

    target_conn
        .execute_raw("REINDEX;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: REINDEX: {e}")))?;
    let _ = target_conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);");

    Ok(())
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

    if !is_real_file(&metadata_path) {
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

fn frontmatter_bounds(content: &str) -> Option<(usize, usize, usize)> {
    let start = content.find("---json")?;
    let after_start = &content[start..];
    let json_start = if after_start.starts_with("---json\r\n") {
        start + "---json\r\n".len()
    } else if after_start.starts_with("---json\n") {
        start + "---json\n".len()
    } else {
        return None;
    };

    let mut search_from = json_start;
    while let Some(relative) = content[search_from..].find("---") {
        let marker_start = search_from + relative;
        if marker_start == 0 || !content[..marker_start].ends_with('\n') {
            search_from = marker_start + 3;
            continue;
        }

        let after_marker = marker_start + 3;
        if after_marker == content.len() {
            return Some((json_start, marker_start, after_marker));
        }
        if content[after_marker..].starts_with("\r\n") {
            return Some((json_start, marker_start, after_marker + 2));
        }
        if content[after_marker..].starts_with('\n') {
            return Some((json_start, marker_start, after_marker + 1));
        }

        search_from = marker_start + 3;
    }

    None
}

/// Extract JSON frontmatter from a `---json\n...\n---` block.
fn extract_json_frontmatter(content: &str) -> Option<&str> {
    let (json_start, json_end, _) = frontmatter_bounds(content)?;
    Some(&content[json_start..json_end])
}

/// Extract the body text after the frontmatter block.
///
/// Only strips leading blank lines; trailing whitespace is preserved
/// so reconstructed bodies match the original archive content.
fn extract_body_after_frontmatter(content: &str) -> Option<&str> {
    let (_, _, body_start) = frontmatter_bounds(content)?;
    let after = &content[body_start..];
    // Skip leading blank lines only — preserve trailing whitespace
    Some(after.trim_start_matches(['\n', '\r']))
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn normalized_archive_agent_name(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn json_str_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::String(s)) => {
            normalized_archive_agent_name(Some(s)).into_iter().collect()
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
            &format!("SELECT id FROM {table} WHERE {col1} = ? AND {col2} = ? COLLATE NOCASE"),
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
    fn extract_json_frontmatter_accepts_crlf_delimiters() {
        let content = "---json\r\n{\"id\": 7}\r\n---\r\n\r\nBody\r\n";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert_eq!(fm, "{\"id\": 7}\r\n");
    }

    #[test]
    fn extract_json_frontmatter_accepts_eof_after_closing_marker() {
        let content = "---json\n{\"id\": 9}\n---";
        let fm = extract_json_frontmatter(content).expect("should extract");
        assert_eq!(fm, "{\"id\": 9}\n");
        let body = extract_body_after_frontmatter(content).expect("should extract body");
        assert_eq!(body, "");
    }

    #[test]
    fn extract_body_after_frontmatter_basic() {
        let content = "---json\n{}\n---\n\nThe body content.\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        // Trailing newline is preserved (no .trim() on body)
        assert_eq!(body, "The body content.\n");
    }

    #[test]
    fn extract_body_after_frontmatter_preserves_trailing_whitespace() {
        let content = "---json\n{}\n---\n\nLine 1\n  indented\n\nLine 3\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert_eq!(body, "Line 1\n  indented\n\nLine 3\n");
    }

    #[test]
    fn extract_body_after_frontmatter_preserves_code_block() {
        let content =
            "---json\n{}\n---\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert!(body.starts_with("```rust\n"));
        assert!(body.ends_with("```\n"));
    }

    #[test]
    fn extract_body_after_frontmatter_strips_leading_blank_lines() {
        let content = "---json\n{}\n---\n\n\n\nBody after blanks.\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert_eq!(body, "Body after blanks.\n");
    }

    #[test]
    fn extract_body_after_frontmatter_preserves_leading_spaces() {
        let content = "---json\n{}\n---\n\n    indented body\n";
        let body = extract_body_after_frontmatter(content).expect("should extract");
        assert_eq!(body, "    indented body\n");
    }

    #[test]
    fn json_str_array_variants() {
        let v: serde_json::Value = serde_json::json!({
            "to": ["Alice", " Bob ", "   "],
            "cc": " Charlie ",
            "bcc": [],
        });
        assert_eq!(json_str_array(&v, "to"), vec!["Alice", "Bob"]);
        assert_eq!(json_str_array(&v, "cc"), vec!["Charlie"]);
        assert!(json_str_array(&v, "bcc").is_empty());
        assert!(json_str_array(&v, "missing").is_empty());
    }

    #[test]
    fn normalized_archive_agent_name_rejects_blank_values() {
        assert_eq!(
            normalized_archive_agent_name(Some(" Alice ")),
            Some("Alice".to_string())
        );
        assert_eq!(normalized_archive_agent_name(Some("   ")), None);
        assert_eq!(normalized_archive_agent_name(None), None);
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
            salvaged_projects: 0,
            salvaged_agents: 0,
            salvaged_messages: 0,
            salvaged_recipients: 0,
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
    fn query_last_insert_or_existing_id_composite_matches_case_insensitively() {
        let conn = DbConn::open_memory().expect("open in-memory db");
        conn.execute_raw(
            "CREATE TABLE agents (\
                id INTEGER PRIMARY KEY,\
                project_id INTEGER NOT NULL,\
                name TEXT NOT NULL\
            )",
        )
        .expect("create agents table");
        conn.query_sync(
            "INSERT INTO agents (project_id, name) VALUES (1, 'BlueLake')",
            &[],
        )
        .expect("insert agent");

        let id = query_last_insert_or_existing_id_composite(
            &conn,
            "agents",
            "project_id",
            1,
            "name",
            "bluelake",
        )
        .expect("find agent id case-insensitively");

        assert_eq!(id, 1);
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

    #[cfg(unix)]
    #[test]
    fn reconstruct_skips_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");
        let real_project = tmp.path().join("outside-project");
        let real_agent = real_project.join("agents").join("Ghost");
        let real_messages = real_project.join("messages").join("2026").join("03");
        let linked_project = storage_root.join("projects").join("linked-project");

        std::fs::create_dir_all(&real_agent).unwrap();
        std::fs::create_dir_all(&real_messages).unwrap();
        std::fs::create_dir_all(linked_project.parent().unwrap()).unwrap();
        std::fs::write(real_agent.join("profile.json"), "{}").unwrap();
        std::fs::write(
            real_messages.join("note.md"),
            "---json\n{\"from\":\"Ghost\",\"to\":[],\"subject\":\"hi\"}\n---\nbody\n",
        )
        .unwrap();
        symlink(&real_project, &linked_project).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
    }

    #[cfg(unix)]
    #[test]
    fn reconstruct_warns_on_symlinked_storage_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let real_storage = tmp.path().join("real-storage");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(real_storage.join("projects")).unwrap();
        symlink(&real_storage, &storage_root).unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.projects, 0);
        assert_eq!(stats.agents, 0);
        assert_eq!(stats.messages, 0);
        assert!(
            !db_path.exists(),
            "symlinked storage roots should not create a reconstructed database file"
        );
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("not a real directory")),
            "expected symlinked storage root warning, got {:?}",
            stats.warnings
        );
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
    #[allow(clippy::too_many_lines)]
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
  "bcc": ["Carol"],
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
            stats.agents, 2,
            "Alice from profile; Bob and Carol auto-created as placeholders"
        );
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 2);
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
        assert_eq!(
            agent_rows.len(),
            3,
            "Alice, Bob, and Carol should all exist"
        );
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
        let carol_rows = conn
            .query_sync("SELECT program FROM agents WHERE name = 'Carol'", &[])
            .unwrap();
        assert!(!carol_rows.is_empty());

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.kind AS kind
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 ORDER BY mr.kind, a.name",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 2);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("kind")
                .expect("first recipient kind"),
            "bcc"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("first recipient name"),
            "Carol"
        );
        assert_eq!(
            recipient_rows[1]
                .get_named::<String>("kind")
                .expect("second recipient kind"),
            "to"
        );
        assert_eq!(
            recipient_rows[1]
                .get_named::<String>("name")
                .expect("second recipient name"),
            "Bob"
        );
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

    #[test]
    fn reconstruct_skips_duplicate_canonical_message_id_without_merging_recipients() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("dup-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"dup-project","human_key":"/dup-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();

        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__first__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

first body
"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-01-00Z__second__7.md"),
            r#"---json
{
  "id": 7,
  "from": "Alice",
  "to": ["Carol"],
  "subject": "Second copy",
  "importance": "urgent",
  "created_ts": "2026-02-22T12:01:00Z"
}
---

second body
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1, "duplicate canonical id must be skipped");
        assert_eq!(
            stats.recipients, 1,
            "duplicate recipient rows must not merge"
        );
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("Duplicate canonical message id 7")),
            "expected duplicate-id warning, got {:?}",
            stats.warnings
        );

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let subject_rows = conn
            .query_sync("SELECT subject FROM messages WHERE id = 7", &[])
            .unwrap();
        assert_eq!(subject_rows.len(), 1);
        assert_eq!(
            subject_rows[0]
                .get_named::<String>("subject")
                .expect("subject"),
            "First copy"
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name \
                 FROM message_recipients mr \
                 JOIN agents a ON a.id = mr.agent_id \
                 WHERE mr.message_id = 7 \
                 ORDER BY a.name",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            "Bob"
        );
    }

    #[test]
    fn reconstruct_sanitizes_invalid_thread_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("thread-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"thread-project","human_key":"/thread-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__thread__9.md"),
            r#"---json
{
  "id": 9,
  "from": "Alice",
  "to": ["Bob"],
  "thread_id": "  !!br:123??  ",
  "subject": "Thread sanitize",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

thread body
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert!(
            stats
                .warnings
                .iter()
                .any(|warning| warning.contains("Sanitized invalid thread_id")),
            "expected thread-id warning, got {:?}",
            stats.warnings
        );

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync("SELECT thread_id FROM messages WHERE id = 9", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<String>("thread_id").expect("thread_id"),
            "br123"
        );
    }

    #[test]
    fn reconstruct_trims_sender_and_recipient_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("trim-project");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"trim-project","human_key":"/trim-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__trim__1.md"),
            r#"---json
{
  "id": 1,
  "from": "   ",
  "to": [" Bob ", "   "],
  "cc": " Carol ",
  "subject": "Trim names",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

body
"#,
        )
        .unwrap();

        let stats = reconstruct_from_archive(&db_path, &storage_root).expect("should succeed");
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 2);

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let agent_rows = conn
            .query_sync("SELECT name FROM agents ORDER BY name", &[])
            .unwrap();
        let names: Vec<String> = agent_rows
            .iter()
            .map(|row| row.get_named::<String>("name").expect("name"))
            .collect();
        assert_eq!(names, vec!["Bob", "Carol", "unknown"]);

        let sender_rows = conn
            .query_sync(
                "SELECT a.name AS name \
                 FROM messages m JOIN agents a ON a.id = m.sender_id \
                 WHERE m.id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(
            sender_rows[0].get_named::<String>("name").expect("sender"),
            "unknown"
        );
    }

    #[test]
    fn reconstruct_recovers_file_reservations_from_archive() {
        let storage_root = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = storage_root
            .path()
            .join("projects")
            .join("reservation-project");
        let agents_dir = project_dir.join("agents").join("CoralMarsh");
        let reservations_dir = project_dir.join("file_reservations");
        std::fs::create_dir_all(&agents_dir).expect("create agents dir");
        std::fs::create_dir_all(&reservations_dir).expect("create reservations dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"reservation-project","human_key":"/reservation-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agents_dir.join("profile.json"),
            r#"{
                "name": "CoralMarsh",
                "program": "codex-cli",
                "model": "gpt-5",
                "task_description": "reservation snapshot",
                "inception_ts": "2026-03-13T21:21:02Z",
                "last_active_ts": "2026-03-13T21:21:02Z"
            }"#,
        )
        .expect("write agent profile");
        let reservation_json = r#"{
            "id": 904,
            "project": "/reservation-project",
            "agent": "CoralMarsh",
            "path_pattern": "crates/mcp-agent-mail-cli/src/robot.rs",
            "exclusive": true,
            "reason": "br-q0e0u",
            "created_ts": "2026-03-13T21:36:47.221175Z",
            "expires_ts": "2026-03-13T23:36:47.221175Z"
        }"#;
        std::fs::write(reservations_dir.join("id-904.json"), reservation_json)
            .expect("write canonical reservation artifact");
        std::fs::write(
            reservations_dir.join("bb1d1d9f8a400a6c3e5732b41fc1f253986e4077.json"),
            reservation_json,
        )
        .expect("write mirrored reservation artifact");
        std::fs::write(
            reservations_dir.join("id-905.json"),
            r#"{
                "id": 905,
                "project": "/reservation-project",
                "agent": "BlueLake",
                "path": "crates/mcp-agent-mail-db/src/reconstruct.rs",
                "exclusive": false,
                "reason": "python-compat",
                "created_ts": "2026-03-13T21:40:00Z",
                "expires_ts": "2026-03-13T23:40:00Z"
            }"#,
        )
        .expect("write python-format reservation artifact");

        let db_path = db_dir.path().join("reconstruct_reservations.sqlite3");
        reconstruct_from_archive(&db_path, storage_root.path()).expect("reconstruct");

        let conn = DbConn::open_file(db_path.display().to_string()).expect("open db");
        let rows = conn
            .query_sync(
                "SELECT fr.id, a.name AS agent_name, fr.path_pattern, fr.exclusive, fr.reason
                 FROM file_reservations fr
                 JOIN agents a ON a.id = fr.agent_id
                 ORDER BY fr.id ASC",
                &[],
            )
            .expect("query reservations");

        assert_eq!(rows.len(), 2, "reconstruction should recover both formats");
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 904);
        assert_eq!(
            rows[0].get_named::<String>("agent_name").unwrap(),
            "CoralMarsh"
        );
        assert_eq!(
            rows[0].get_named::<String>("path_pattern").unwrap(),
            "crates/mcp-agent-mail-cli/src/robot.rs"
        );
        assert_eq!(rows[0].get_named::<i64>("exclusive").unwrap(), 1);
        assert_eq!(rows[0].get_named::<String>("reason").unwrap(), "br-q0e0u");
        assert_eq!(rows[1].get_named::<i64>("id").unwrap(), 905);
        assert_eq!(
            rows[1].get_named::<String>("agent_name").unwrap(),
            "BlueLake"
        );
        assert_eq!(
            rows[1].get_named::<String>("path_pattern").unwrap(),
            "crates/mcp-agent-mail-db/src/reconstruct.rs"
        );
        assert_eq!(rows[1].get_named::<i64>("exclusive").unwrap(), 0);
        assert_eq!(
            rows[1].get_named::<String>("reason").unwrap(),
            "python-compat"
        );
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn reconstruct_with_salvage_merges_db_only_rows_and_recipient_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let salvage_db_path = tmp.path().join("salvage.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&messages_dir).unwrap();
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"test-project","human_key":"/test-project","created_at":0}"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"name":"Alice","program":"coder","model":"test","inception_ts":"2026-02-22T00:00:00Z","last_active_ts":"2026-02-22T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            messages_dir.join("2026-02-22T12-00-00Z__archive__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "Archive copy",
  "importance": "normal",
  "created_ts": "2026-02-22T12:00:00Z"
}
---

archive body
"#,
        )
        .unwrap();

        let salvage_conn = DbConn::open_file(salvage_db_path.to_str().unwrap()).unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    human_key TEXT,
                    created_at INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE agents (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    name TEXT NOT NULL
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    project_id INTEGER NOT NULL,
                    sender_id INTEGER NOT NULL,
                    subject TEXT,
                    body_md TEXT,
                    created_ts INTEGER
                )",
            )
            .unwrap();
        salvage_conn
            .execute_raw(
                "CREATE TABLE message_recipients (
                    message_id INTEGER NOT NULL,
                    agent_id INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    read_ts INTEGER,
                    ack_ts INTEGER
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents (id, project_id, name) VALUES
                    (10, 100, 'Alice'),
                    (11, 100, 'Bob'),
                    (12, 100, 'Carol')",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts)
                 VALUES (2, 100, 10, 'DB-only', 'db body', 2)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
                 VALUES
                    (1, 11, 'to', 123, 456),
                    (2, 12, 'to', NULL, NULL)",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.salvaged_projects, 0);
        assert_eq!(stats.salvaged_agents, 1);
        assert_eq!(stats.salvaged_messages, 1);
        assert_eq!(stats.salvaged_recipients, 2);

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync("SELECT id, subject FROM messages ORDER BY id", &[])
            .unwrap();
        assert_eq!(message_rows.len(), 2);
        assert_eq!(
            message_rows[1]
                .get_named::<String>("subject")
                .expect("subject"),
            "DB-only"
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.read_ts AS read_ts, mr.ack_ts AS ack_ts
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 1);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            "Bob"
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<i64>("read_ts")
                .expect("read_ts"),
            123
        );
        assert_eq!(
            recipient_rows[0]
                .get_named::<i64>("ack_ts")
                .expect("ack_ts"),
            456
        );

        let carol_rows = conn
            .query_sync(
                "SELECT a.name AS name
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 2",
                &[],
            )
            .unwrap();
        assert_eq!(carol_rows.len(), 1);
        assert_eq!(
            carol_rows[0]
                .get_named::<String>("name")
                .expect("recipient name"),
            "Carol"
        );
    }
}
