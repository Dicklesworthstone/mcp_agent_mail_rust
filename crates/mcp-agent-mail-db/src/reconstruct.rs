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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

const DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT: usize = 5;

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
    /// Number of duplicate canonical archive files skipped because their
    /// positive frontmatter `id` had already been recovered.
    pub duplicate_canonical_message_files: usize,
    /// Number of distinct logical message ids represented by the skipped
    /// duplicate canonical archive files.
    pub duplicate_canonical_message_ids: usize,
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
    duplicate_canonical_id_set: BTreeSet<i64>,
}

/// Lightweight canonical archive inventory used for drift detection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveMessageInventory {
    /// Number of canonical archive files under `messages/YYYY/MM/*.md`.
    pub canonical_message_files: usize,
    /// Number of unique positive message ids represented by those files.
    pub unique_message_ids: usize,
    /// Number of duplicate canonical archive files skipped by id.
    pub duplicate_canonical_message_files: usize,
    /// Number of distinct ids represented by the duplicate files.
    pub duplicate_canonical_message_ids: usize,
    /// Largest positive canonical message id observed in the archive.
    pub latest_message_id: Option<i64>,
    /// Number of canonical message files that failed JSON frontmatter parsing.
    pub parse_errors: usize,
}

impl ArchiveMessageInventory {
    fn record_message_id(&mut self, message_id: i64, seen_ids: &mut BTreeSet<i64>) {
        self.latest_message_id = Some(
            self.latest_message_id
                .map_or(message_id, |current| current.max(message_id)),
        );
        if seen_ids.insert(message_id) {
            self.unique_message_ids += 1;
        } else {
            self.duplicate_canonical_message_files += 1;
        }
    }
}

impl ReconstructStats {
    fn record_duplicate_canonical_message(&mut self, message_id: i64, file_path: &Path) {
        self.duplicate_canonical_message_files += 1;
        if self.duplicate_canonical_id_set.insert(message_id) {
            self.duplicate_canonical_message_ids += 1;
        }
        if self.duplicate_canonical_message_files <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT {
            self.warnings.push(format!(
                "Duplicate canonical message id {message_id} in {}; keeping the first archive artifact and skipping the duplicate",
                file_path.display()
            ));
        }
    }

    fn finalize_duplicate_warnings(&mut self) {
        if self.duplicate_canonical_message_files <= DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT {
            return;
        }

        let sample_ids = self
            .duplicate_canonical_id_set
            .iter()
            .take(DUPLICATE_CANONICAL_WARNING_SAMPLE_LIMIT)
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.warnings.push(format!(
            "Skipped {} duplicate canonical message file(s) across {} logical message id(s); sample ids: {}",
            self.duplicate_canonical_message_files,
            self.duplicate_canonical_message_ids,
            sample_ids
        ));
    }
}

impl std::fmt::Display for ReconstructStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reconstructed {} projects, {} agents, {} messages ({} recipients), {} parse errors",
            self.projects, self.agents, self.messages, self.recipients, self.parse_errors
        )?;
        if self.duplicate_canonical_message_files > 0 {
            write!(
                f,
                "; skipped {} duplicate canonical file(s) across {} message id(s)",
                self.duplicate_canonical_message_files, self.duplicate_canonical_message_ids
            )?;
        }
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

/// Scan canonical archive message files without writing to SQLite.
#[must_use]
pub fn scan_archive_message_inventory(storage_root: &Path) -> ArchiveMessageInventory {
    let mut inventory = ArchiveMessageInventory::default();
    let projects_dir = storage_root.join("projects");
    if !is_real_directory(&projects_dir) {
        return inventory;
    }

    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return inventory;
    };

    let mut seen_ids = BTreeSet::new();
    let mut duplicate_ids = BTreeSet::new();

    for entry in project_entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        scan_project_archive_message_inventory(
            &path.join("messages"),
            &mut inventory,
            &mut seen_ids,
            &mut duplicate_ids,
        );
    }

    inventory.duplicate_canonical_message_ids = duplicate_ids.len();
    inventory
}

fn scan_project_archive_message_inventory(
    messages_dir: &Path,
    inventory: &mut ArchiveMessageInventory,
    seen_ids: &mut BTreeSet<i64>,
    duplicate_ids: &mut BTreeSet<i64>,
) {
    if !is_real_directory(messages_dir) {
        return;
    }

    let Ok(year_entries) = std::fs::read_dir(messages_dir) else {
        return;
    };

    for year_entry in year_entries.flatten() {
        let year_path = year_entry.path();
        let Ok(year_type) = year_entry.file_type() else {
            continue;
        };
        if !year_type.is_dir() || year_type.is_symlink() {
            continue;
        }
        let Some(year_name) = year_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if year_name.len() != 4 || !year_name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let Ok(month_entries) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month_entry in month_entries.flatten() {
            let month_path = month_entry.path();
            let Ok(month_type) = month_entry.file_type() else {
                continue;
            };
            if !month_type.is_dir() || month_type.is_symlink() {
                continue;
            }
            let Some(month_name) = month_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if month_name.len() != 2 || !month_name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }

            let Ok(file_entries) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for file_entry in file_entries.flatten() {
                let file_path = file_entry.path();
                let Ok(file_type) = file_entry.file_type() else {
                    continue;
                };
                if !file_type.is_file()
                    || file_type.is_symlink()
                    || file_path.extension().is_none_or(|ext| ext != "md")
                {
                    continue;
                }

                inventory.canonical_message_files += 1;
                match scan_archive_message_id(&file_path) {
                    Ok(Some(message_id)) => {
                        let existed = seen_ids.contains(&message_id);
                        inventory.record_message_id(message_id, seen_ids);
                        if existed {
                            duplicate_ids.insert(message_id);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => inventory.parse_errors += 1,
                }
            }
        }
    }
}

fn scan_archive_message_id(file_path: &Path) -> DbResult<Option<i64>> {
    let content = std::fs::read_to_string(file_path)
        .map_err(|e| DbError::Sqlite(format!("read {}: {e}", file_path.display())))?;
    let Some(frontmatter) = extract_json_frontmatter(&content) else {
        return Ok(None);
    };
    let msg: serde_json::Value = serde_json::from_str(frontmatter)
        .map_err(|e| DbError::Sqlite(format!("bad JSON in {}: {e}", file_path.display())))?;
    Ok(msg
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0))
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
            discover_messages(&conn, &messages_dir, pid, slug, &mut agent_ids, &mut stats)?;
        }
    }

    // Rebuild all index b-trees to ensure consistency after bulk inserts.
    conn.execute_raw("REINDEX;")
        .map_err(|e| DbError::Sqlite(format!("reconstruct: REINDEX: {e}")))?;

    // Flush WAL (if any residual) and remove sidecar files so the DB is a
    // single clean file ready for the runtime to open with its own settings.
    let _ = conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);");

    stats.finalize_duplicate_warnings();
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
///
/// Returns `Err` only for unrecoverable DB failures (connection dead, disk full).
/// Individual file parse errors are counted in `stats.parse_errors` and skipped.
fn discover_messages(
    conn: &DbConn,
    messages_dir: &Path,
    project_id: i64,
    project_slug: &str,
    agent_ids: &mut HashMap<(i64, String), i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    // Walk year directories
    let Ok(years) = std::fs::read_dir(messages_dir) else {
        return Ok(());
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
                // Distinguish parse errors (skip file) from DB errors (abort).
                // Probe the connection — if it's dead, propagate the error.
                if conn.execute_raw("SELECT 1").is_err() {
                    return Err(e);
                }
                stats.parse_errors += 1;
                stats.warnings.push(format!(
                    "Failed to reconstruct message from {}: {e}",
                    file_path.display()
                ));
            }
        }
    }
    Ok(())
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
    let recipients_json = encode_recipients_json(&to_names, &cc_names, &bcc_names);

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
        stats.record_duplicate_canonical_message(cid, file_path);
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
             (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                Value::Text(recipients_json.clone()),
                Value::Text(attachments),
            ],
        )
        .map_err(|e| DbError::Sqlite(format!("insert message with id {cid}: {e}")))?;
        cid
    } else {
        conn.execute_sync(
            "INSERT INTO messages \
             (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(project_id),
                Value::BigInt(sender_id),
                thread_id_val,
                Value::Text(subject.to_string()),
                Value::Text(body_md.to_string()),
                Value::Text(importance.to_string()),
                Value::BigInt(i64::from(ack_required)),
                Value::BigInt(created_ts),
                Value::Text(recipients_json.clone()),
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

fn encode_recipients_json(
    to_names: &[String],
    cc_names: &[String],
    bcc_names: &[String],
) -> String {
    serde_json::json!({
        "to": to_names,
        "cc": cc_names,
        "bcc": bcc_names,
    })
    .to_string()
}

fn parse_salvaged_recipients_json(
    recipients_json: Option<String>,
    message_id: i64,
    stats: &mut ReconstructStats,
) -> (String, Vec<String>, Vec<String>, Vec<String>) {
    let empty = (
        encode_recipients_json(&[], &[], &[]),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let Some(recipients_json) = recipients_json.filter(|json| !json.trim().is_empty()) else {
        return empty;
    };

    let parsed: serde_json::Value = match serde_json::from_str(&recipients_json) {
        Ok(parsed) => parsed,
        Err(err) => {
            stats.warnings.push(format!(
                "Salvage message {message_id} has invalid recipients_json; dropping malformed recipient metadata: {err}"
            ));
            return empty;
        }
    };

    let to_names = json_str_array(&parsed, "to");
    let cc_names = json_str_array(&parsed, "cc");
    let bcc_names = json_str_array(&parsed, "bcc");
    (
        encode_recipients_json(&to_names, &cc_names, &bcc_names),
        to_names,
        cc_names,
        bcc_names,
    )
}

fn sync_reconstructed_message_recipients_json(conn: &DbConn, message_id: i64) -> DbResult<()> {
    let rows = conn
        .query_sync(
            "SELECT a.name AS name, mr.kind AS kind \
             FROM message_recipients mr \
             JOIN agents a ON a.id = mr.agent_id \
             WHERE mr.message_id = ? \
             ORDER BY CASE mr.kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 WHEN 'bcc' THEN 2 ELSE 3 END, \
                      a.name COLLATE NOCASE",
            &[Value::BigInt(message_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query recipients_json rows for message {message_id}: {e}"
            ))
        })?;

    let mut to_names = Vec::new();
    let mut cc_names = Vec::new();
    let mut bcc_names = Vec::new();

    for row in rows {
        let name = row.get_named::<String>("name").map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: decode recipient name for message {message_id}: {e}"
            ))
        })?;
        let kind = row.get_named::<String>("kind").map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: decode recipient kind for message {message_id}: {e}"
            ))
        })?;
        match kind.as_str() {
            "cc" => cc_names.push(name),
            "bcc" => bcc_names.push(name),
            _ => to_names.push(name),
        }
    }

    conn.execute_sync(
        "UPDATE messages SET recipients_json = ? WHERE id = ?",
        &[
            Value::Text(encode_recipients_json(&to_names, &cc_names, &bcc_names)),
            Value::BigInt(message_id),
        ],
    )
    .map(|_| ())
    .map_err(|e| {
        DbError::Sqlite(format!(
            "reconstruct salvage: update recipients_json for message {message_id}: {e}"
        ))
    })
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
    if sanitized.is_empty()
        || !sanitized
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
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

fn merge_salvaged_created_at(current_created_at: i64, salvaged_created_at: i64) -> i64 {
    if salvaged_created_at <= 0 {
        current_created_at
    } else if current_created_at <= 0 {
        salvaged_created_at
    } else {
        current_created_at.min(salvaged_created_at)
    }
}

fn merge_salvaged_inception_ts(current_inception_ts: i64, salvaged_inception_ts: i64) -> i64 {
    if salvaged_inception_ts <= 0 {
        current_inception_ts
    } else if current_inception_ts <= 0 {
        salvaged_inception_ts
    } else {
        current_inception_ts.min(salvaged_inception_ts)
    }
}

fn merge_salvaged_last_active_ts(current_last_active_ts: i64, salvaged_last_active_ts: i64) -> i64 {
    if salvaged_last_active_ts <= 0 {
        current_last_active_ts
    } else if current_last_active_ts <= 0 {
        salvaged_last_active_ts
    } else {
        current_last_active_ts.max(salvaged_last_active_ts)
    }
}

fn should_replace_placeholder_text(current: &str, salvaged: &str, placeholder: &str) -> bool {
    let current = current.trim();
    let salvaged = salvaged.trim();
    !salvaged.is_empty()
        && salvaged != placeholder
        && (current.is_empty() || current == placeholder)
}

fn should_replace_default_policy(current: &str, salvaged: &str) -> bool {
    let current = current.trim();
    let salvaged = salvaged.trim();
    !salvaged.is_empty() && salvaged != "auto" && (current.is_empty() || current == "auto")
}

fn synthetic_project_placeholder_human_key(slug: &str) -> String {
    format!("/{slug}")
}

fn placeholder_human_key_for_human_key(human_key: &str) -> Option<String> {
    let trimmed = human_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let basename = Path::new(trimmed).file_name()?.to_str()?.trim();
    if basename.is_empty() {
        return None;
    }
    Some(format!("/{basename}"))
}

fn normalized_project_match_token(value: &str) -> Option<String> {
    let normalized = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect::<String>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn project_basename_token_for_human_key(human_key: &str) -> Option<String> {
    let trimmed = human_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let basename = Path::new(trimmed).file_name()?.to_str()?;
    normalized_project_match_token(basename)
}

fn is_synthetic_project_placeholder(slug: &str, human_key: &str) -> bool {
    let trimmed = human_key.trim();
    trimmed.is_empty() || trimmed == synthetic_project_placeholder_human_key(slug)
}

#[derive(Debug, Clone)]
struct SalvageProjectIdentityRow {
    id: i64,
    slug: String,
    human_key: String,
    created_at: i64,
}

fn reconcile_placeholder_project_duplicates_after_salvage(
    conn: &DbConn,
    project_id_map: &mut HashMap<i64, i64>,
    stats: &mut ReconstructStats,
) -> DbResult<()> {
    let rows = conn
        .query_sync(
            "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
            &[],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query project rows for duplicate reconciliation: {e}"
            ))
        })?;

    let mut placeholder_by_token: HashMap<String, SalvageProjectIdentityRow> = HashMap::new();
    let mut placeholder_counts: HashMap<String, usize> = HashMap::new();
    let mut canonical_counts: HashMap<String, usize> = HashMap::new();
    let mut canonical_rows = Vec::new();

    for row in &rows {
        let Some(id) = row.get_named::<i64>("id").ok().filter(|value| *value > 0) else {
            continue;
        };
        let slug = row.get_named::<String>("slug").unwrap_or_default();
        let human_key = row.get_named::<String>("human_key").unwrap_or_default();
        let created_at = row.get_named::<i64>("created_at").unwrap_or_default();
        let identity = SalvageProjectIdentityRow {
            id,
            slug: slug.clone(),
            human_key: human_key.clone(),
            created_at,
        };
        if is_synthetic_project_placeholder(&slug, &human_key) {
            if let Some(token) = normalized_project_match_token(&slug) {
                *placeholder_counts.entry(token.clone()).or_insert(0) += 1;
                placeholder_by_token.entry(token).or_insert(identity);
            }
        } else if Path::new(human_key.trim()).is_absolute()
            && let Some(token) = project_basename_token_for_human_key(&human_key)
        {
            *canonical_counts.entry(token.clone()).or_insert(0) += 1;
            canonical_rows.push((token, identity));
        }
    }

    canonical_rows.sort_by_key(|(_, row)| row.created_at);

    for (token, canonical) in canonical_rows {
        if placeholder_counts.get(&token).copied().unwrap_or(0) != 1
            || canonical_counts.get(&token).copied().unwrap_or(0) != 1
        {
            continue;
        }
        let Some(placeholder) = placeholder_by_token.get(&token).cloned() else {
            continue;
        };
        if placeholder.id == canonical.id {
            continue;
        }

        for mapped_project_id in project_id_map.values_mut() {
            if *mapped_project_id == canonical.id {
                *mapped_project_id = placeholder.id;
            }
        }

        conn.execute_sync(
            "DELETE FROM projects WHERE id = ?",
            &[Value::BigInt(canonical.id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: remove duplicate canonical project {}: {e}",
                canonical.slug
            ))
        })?;

        let merged_created_at =
            merge_salvaged_created_at(placeholder.created_at, canonical.created_at);
        conn.execute_sync(
            "UPDATE projects SET slug = ?, human_key = ?, created_at = ? WHERE id = ?",
            &[
                Value::Text(canonical.slug.clone()),
                Value::Text(canonical.human_key.clone()),
                Value::BigInt(merged_created_at),
                Value::BigInt(placeholder.id),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: promote placeholder project {} to canonical {}: {e}",
                placeholder.slug, canonical.slug
            ))
        })?;
        if stats.salvaged_projects > 0 {
            stats.salvaged_projects -= 1;
        }

        placeholder_by_token.insert(
            token,
            SalvageProjectIdentityRow {
                id: placeholder.id,
                slug: canonical.slug,
                human_key: canonical.human_key,
                created_at: merged_created_at,
            },
        );
    }

    Ok(())
}

fn enrich_existing_project_from_salvage(
    conn: &DbConn,
    project_id: i64,
    slug: &str,
    salvaged_slug: &str,
    salvaged_human_key: &str,
    salvaged_created_at: i64,
) -> DbResult<()> {
    let existing_rows = conn
        .query_sync(
            "SELECT slug, human_key, created_at FROM projects WHERE id = ? LIMIT 1",
            &[Value::BigInt(project_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query project state for slug {slug}: {e}"
            ))
        })?;
    let Some(existing_row) = existing_rows.first() else {
        return Ok(());
    };

    let current_slug = existing_row
        .get_named::<String>("slug")
        .unwrap_or_else(|_| slug.to_string());
    let current_human_key = existing_row
        .get_named::<String>("human_key")
        .unwrap_or_else(|_| synthetic_project_placeholder_human_key(&current_slug));
    let current_created_at = existing_row
        .get_named::<i64>("created_at")
        .unwrap_or_default();
    let fallback_human_key = synthetic_project_placeholder_human_key(&current_slug);
    let current_is_placeholder =
        current_human_key.trim().is_empty() || current_human_key == fallback_human_key;
    let next_slug = if current_is_placeholder {
        let candidate = salvaged_slug.trim();
        if candidate.is_empty() {
            current_slug.clone()
        } else {
            candidate.to_string()
        }
    } else {
        current_slug.clone()
    };
    let next_human_key = if current_is_placeholder {
        let candidate = salvaged_human_key.trim();
        if Path::new(candidate).is_absolute() {
            candidate.to_string()
        } else {
            current_human_key.clone()
        }
    } else {
        current_human_key.clone()
    };
    let next_created_at = merge_salvaged_created_at(current_created_at, salvaged_created_at);

    if next_slug != current_slug
        || next_human_key != current_human_key
        || next_created_at != current_created_at
    {
        conn.execute_sync(
            "UPDATE projects SET slug = ?, human_key = ?, created_at = ? WHERE id = ?",
            &[
                Value::Text(next_slug),
                Value::Text(next_human_key),
                Value::BigInt(next_created_at),
                Value::BigInt(project_id),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: enrich project metadata for slug {slug}: {e}"
            ))
        })?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enrich_existing_agent_from_salvage(
    conn: &DbConn,
    agent_id: i64,
    name: &str,
    salvaged_program: &str,
    salvaged_model: &str,
    salvaged_task_description: &str,
    salvaged_inception_ts: i64,
    salvaged_last_active_ts: i64,
    salvaged_attachments_policy: &str,
    salvaged_contact_policy: &str,
) -> DbResult<()> {
    let existing_rows = conn
        .query_sync(
            "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy \
             FROM agents WHERE id = ? LIMIT 1",
            &[Value::BigInt(agent_id)],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: query agent state for {name}: {e}"
            ))
        })?;
    let Some(existing_row) = existing_rows.first() else {
        return Ok(());
    };

    let current_program = existing_row
        .get_named::<String>("program")
        .unwrap_or_else(|_| "unknown".to_string());
    let current_model = existing_row
        .get_named::<String>("model")
        .unwrap_or_else(|_| "unknown".to_string());
    let current_task_description = existing_row
        .get_named::<String>("task_description")
        .unwrap_or_default();
    let current_inception_ts = existing_row
        .get_named::<i64>("inception_ts")
        .unwrap_or_default();
    let current_last_active_ts = existing_row
        .get_named::<i64>("last_active_ts")
        .unwrap_or_default();
    let current_attachments_policy = existing_row
        .get_named::<String>("attachments_policy")
        .unwrap_or_else(|_| "auto".to_string());
    let current_contact_policy = existing_row
        .get_named::<String>("contact_policy")
        .unwrap_or_else(|_| "auto".to_string());
    let is_placeholder_agent = current_program.trim() == "unknown"
        && current_model.trim() == "unknown"
        && current_task_description.trim().is_empty()
        && current_attachments_policy.trim() == "auto"
        && current_contact_policy.trim() == "auto";

    let next_program =
        if should_replace_placeholder_text(&current_program, salvaged_program, "unknown") {
            salvaged_program.trim().to_string()
        } else {
            current_program.clone()
        };
    let next_model = if should_replace_placeholder_text(&current_model, salvaged_model, "unknown") {
        salvaged_model.trim().to_string()
    } else {
        current_model.clone()
    };
    let next_task_description = if should_replace_placeholder_text(
        &current_task_description,
        salvaged_task_description,
        "",
    ) {
        salvaged_task_description.trim().to_string()
    } else {
        current_task_description.clone()
    };
    let next_inception_ts =
        merge_salvaged_inception_ts(current_inception_ts, salvaged_inception_ts);
    let next_last_active_ts = if is_placeholder_agent && salvaged_last_active_ts > 0 {
        salvaged_last_active_ts
    } else {
        merge_salvaged_last_active_ts(current_last_active_ts, salvaged_last_active_ts)
    };
    let next_attachments_policy = if should_replace_default_policy(
        &current_attachments_policy,
        salvaged_attachments_policy,
    ) {
        salvaged_attachments_policy.trim().to_string()
    } else {
        current_attachments_policy.clone()
    };
    let next_contact_policy =
        if should_replace_default_policy(&current_contact_policy, salvaged_contact_policy) {
            salvaged_contact_policy.trim().to_string()
        } else {
            current_contact_policy.clone()
        };

    if next_program != current_program
        || next_model != current_model
        || next_task_description != current_task_description
        || next_inception_ts != current_inception_ts
        || next_last_active_ts != current_last_active_ts
        || next_attachments_policy != current_attachments_policy
        || next_contact_policy != current_contact_policy
    {
        conn.execute_sync(
            "UPDATE agents SET \
                 program = ?, \
                 model = ?, \
                 task_description = ?, \
                 inception_ts = ?, \
                 last_active_ts = ?, \
                 attachments_policy = ?, \
                 contact_policy = ? \
             WHERE id = ?",
            &[
                Value::Text(next_program),
                Value::Text(next_model),
                Value::Text(next_task_description),
                Value::BigInt(next_inception_ts),
                Value::BigInt(next_last_active_ts),
                Value::Text(next_attachments_policy),
                Value::Text(next_contact_policy),
                Value::BigInt(agent_id),
            ],
        )
        .map_err(|e| {
            DbError::Sqlite(format!(
                "reconstruct salvage: enrich agent metadata for {name}: {e}"
            ))
        })?;
    }

    Ok(())
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

            let human_key = row
                .get_named::<String>("human_key")
                .unwrap_or_else(|_| synthetic_project_placeholder_human_key(&slug));
            let created_at = row
                .get_named::<i64>("created_at")
                .unwrap_or_else(|_| crate::now_micros());

            if let Ok(target_project_id) =
                query_last_insert_or_existing_id(&target_conn, "projects", "slug", &slug)
            {
                enrich_existing_project_from_salvage(
                    &target_conn,
                    target_project_id,
                    &slug,
                    &slug,
                    &human_key,
                    created_at,
                )?;
                project_id_map.insert(source_project_id, target_project_id);
                continue;
            }
            if let Some(placeholder_human_key) = placeholder_human_key_for_human_key(&human_key)
                && let Ok(target_project_id) = query_last_insert_or_existing_id(
                    &target_conn,
                    "projects",
                    "human_key",
                    &placeholder_human_key,
                )
            {
                enrich_existing_project_from_salvage(
                    &target_conn,
                    target_project_id,
                    &slug,
                    &slug,
                    &human_key,
                    created_at,
                )?;
                project_id_map.insert(source_project_id, target_project_id);
                continue;
            }
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

        reconcile_placeholder_project_duplicates_after_salvage(
            &target_conn,
            &mut project_id_map,
            stats,
        )?;
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

            let salvaged_program = row
                .get_named::<String>("program")
                .unwrap_or_else(|_| "unknown".to_string());
            let salvaged_model = row
                .get_named::<String>("model")
                .unwrap_or_else(|_| "unknown".to_string());
            let salvaged_task_description = row
                .get_named::<String>("task_description")
                .unwrap_or_default();
            let salvaged_inception_ts = row
                .get_named::<i64>("inception_ts")
                .unwrap_or_else(|_| crate::now_micros());
            let salvaged_last_active_ts = row
                .get_named::<i64>("last_active_ts")
                .unwrap_or_else(|_| crate::now_micros());
            let salvaged_attachments_policy = row
                .get_named::<String>("attachments_policy")
                .unwrap_or_else(|_| "auto".to_string());
            let salvaged_contact_policy = row
                .get_named::<String>("contact_policy")
                .unwrap_or_else(|_| "auto".to_string());

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
                        Value::Text(salvaged_program.clone()),
                        Value::Text(salvaged_model.clone()),
                        Value::Text(salvaged_task_description.clone()),
                        Value::BigInt(salvaged_inception_ts),
                        Value::BigInt(salvaged_last_active_ts),
                        Value::Text(salvaged_attachments_policy.clone()),
                        Value::Text(salvaged_contact_policy.clone()),
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
            } else {
                enrich_existing_agent_from_salvage(
                    &target_conn,
                    target_agent_id,
                    &name,
                    &salvaged_program,
                    &salvaged_model,
                    &salvaged_task_description,
                    salvaged_inception_ts,
                    salvaged_last_active_ts,
                    &salvaged_attachments_policy,
                    &salvaged_contact_policy,
                )?;
            }
        }
    }

    let mut reconstructed_recipient_agent_ids: HashMap<(i64, String), i64> = HashMap::new();
    let mut recipient_json_updates = BTreeSet::new();

    if has_messages {
        let message_columns = table_columns(&salvage_conn, "messages")?;
        if let Some(message_select) = build_salvage_select(
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
                "recipients_json",
                "attachments",
            ],
            stats,
            salvage_db_path,
        ) {
            let message_rows = salvage_conn
                .query_sync(
                    &format!("SELECT {message_select} FROM messages ORDER BY id"),
                    &[],
                )
                .map_err(|e| {
                    DbError::Sqlite(format!("reconstruct salvage: query messages: {e}"))
                })?;

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
                let (recipients_json, to_names, cc_names, bcc_names) =
                    parse_salvaged_recipients_json(
                        row.get_named::<String>("recipients_json").ok(),
                        message_id,
                        stats,
                    );

                target_conn
                    .execute_sync(
                        "INSERT OR IGNORE INTO messages \
                         (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, recipients_json, attachments) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                            Value::Text(recipients_json),
                            Value::Text(
                                row.get_named::<String>("attachments")
                                    .unwrap_or_else(|_| "[]".to_string()),
                            ),
                        ],
                    )
                    .map_err(|e| {
                        DbError::Sqlite(format!(
                            "reconstruct salvage: insert message {message_id}: {e}"
                        ))
                    })?;
                stats.salvaged_messages += 1;

                for name in &to_names {
                    let agent_id = ensure_agent_exists(
                        &target_conn,
                        target_project_id,
                        name,
                        &mut reconstructed_recipient_agent_ids,
                    )?;
                    insert_recipient(&target_conn, message_id, agent_id, "to")?;
                    stats.salvaged_recipients += 1;
                    recipient_json_updates.insert(message_id);
                }
                for name in &cc_names {
                    let agent_id = ensure_agent_exists(
                        &target_conn,
                        target_project_id,
                        name,
                        &mut reconstructed_recipient_agent_ids,
                    )?;
                    insert_recipient(&target_conn, message_id, agent_id, "cc")?;
                    stats.salvaged_recipients += 1;
                    recipient_json_updates.insert(message_id);
                }
                for name in &bcc_names {
                    let agent_id = ensure_agent_exists(
                        &target_conn,
                        target_project_id,
                        name,
                        &mut reconstructed_recipient_agent_ids,
                    )?;
                    insert_recipient(&target_conn, message_id, agent_id, "bcc")?;
                    stats.salvaged_recipients += 1;
                    recipient_json_updates.insert(message_id);
                }
            }
        }
    }

    if has_recipients {
        let recipient_columns = table_columns(&salvage_conn, "message_recipients")?;
        if let Some(recipient_select) = build_salvage_select(
            "message_recipients",
            &recipient_columns,
            &["message_id", "agent_id", "kind"],
            &["read_ts", "ack_ts"],
            stats,
            salvage_db_path,
        ) {
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
                recipient_json_updates.insert(message_id);

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
    }

    for message_id in recipient_json_updates {
        sync_reconstructed_message_recipients_json(&target_conn, message_id)?;
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
/// Falls back to a synthetic `/{slug}` placeholder when metadata is missing or
/// malformed. Recovery flows that have a readable salvage database will later
/// replace this placeholder with the canonical path.
fn read_project_human_key(project_path: &Path, slug: &str, stats: &mut ReconstructStats) -> String {
    let metadata_path = project_path.join("project.json");
    let fallback = synthetic_project_placeholder_human_key(slug);

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
        assert_eq!(fm, "{\"id\": 1, \"subject\": \"hello\"}\n");
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
        assert!(body.starts_with("Line 1\n"));
        assert!(body.ends_with("Line 3\n"));
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
            duplicate_canonical_message_files: 0,
            duplicate_canonical_message_ids: 0,
            salvaged_projects: 0,
            salvaged_agents: 0,
            salvaged_messages: 0,
            salvaged_recipients: 0,
            parse_errors: 3,
            warnings: vec![],
            duplicate_canonical_id_set: BTreeSet::new(),
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
            .get_named::<String>("human_key")
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
            .get_named::<String>("human_key")
            .expect("human_key text");
        assert_eq!(human_key, "/test-project");
    }

    #[test]
    fn reconstruct_with_salvage_upgrades_slug_only_archive_project_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let salvage_db_path = tmp.path().join("salvage.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("test-project");
        std::fs::create_dir_all(&project_dir).unwrap();

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
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at) VALUES (100, 'test-project', '/test-project', 1)",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.salvaged_projects, 0);

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<i64>("id").unwrap(),
            1_i64,
            "archive placeholder project id should remain stable"
        );
        assert_eq!(rows[0].get_named::<String>("slug").unwrap(), "test-project");
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/test-project"
        );
        assert_eq!(
            rows[0].get_named::<i64>("created_at").unwrap(),
            1_i64,
            "salvage database should promote project created_at"
        );
    }

    #[test]
    fn reconcile_placeholder_project_duplicates_promotes_archive_project_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("flywheel_connectors".to_string()),
                Value::Text("/flywheel_connectors".to_string()),
                Value::BigInt(10),
            ],
        )
        .unwrap();
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(2),
                Value::Text("users-jemanuel-projects-flywheel-connectors".to_string()),
                Value::Text("/Users/jemanuel/projects/flywheel_connectors".to_string()),
                Value::BigInt(1),
            ],
        )
        .unwrap();

        let mut project_id_map = HashMap::from([(100_i64, 2_i64)]);
        let mut stats = ReconstructStats {
            salvaged_projects: 1,
            ..ReconstructStats::default()
        };

        reconcile_placeholder_project_duplicates_after_salvage(
            &conn,
            &mut project_id_map,
            &mut stats,
        )
        .expect("duplicate reconciliation should succeed");

        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get_named::<i64>("id").unwrap(),
            1_i64,
            "salvage database should promote project id"
        );
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap(),
            "users-jemanuel-projects-flywheel-connectors"
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/Users/jemanuel/projects/flywheel_connectors"
        );
        assert_eq!(
            rows[0].get_named::<i64>("created_at").unwrap(),
            1_i64,
            "salvage database should promote project created_at"
        );
    }

    #[test]
    fn reconcile_placeholder_project_duplicates_skips_ambiguous_basename_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed.db");
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        conn.execute_raw(&schema::init_schema_sql_base()).unwrap();

        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("shared".to_string()),
                Value::Text("/shared".to_string()),
                Value::BigInt(10),
            ],
        )
        .unwrap();
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(2),
                Value::Text("tmp-one-shared".to_string()),
                Value::Text("/tmp/one/shared".to_string()),
                Value::BigInt(1),
            ],
        )
        .unwrap();
        conn.query_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(3),
                Value::Text("var-two-shared".to_string()),
                Value::Text("/var/two/shared".to_string()),
                Value::BigInt(2),
            ],
        )
        .unwrap();

        let mut project_id_map = HashMap::from([(100_i64, 2_i64), (101_i64, 3_i64)]);
        let mut stats = ReconstructStats {
            salvaged_projects: 2,
            ..ReconstructStats::default()
        };

        reconcile_placeholder_project_duplicates_after_salvage(
            &conn,
            &mut project_id_map,
            &mut stats,
        )
        .expect("ambiguous duplicate reconciliation should leave distinct canonical rows alone");

        let rows = conn
            .query_sync(
                "SELECT id, slug, human_key, created_at FROM projects ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].get_named::<i64>("id").unwrap(), 1);
        assert_eq!(
            rows[0].get_named::<String>("slug").unwrap(),
            "shared".to_string()
        );
        assert_eq!(
            rows[0].get_named::<String>("human_key").unwrap(),
            "/shared".to_string()
        );
        assert_eq!(rows[0].get_named::<i64>("created_at").unwrap(), 10);
        assert_eq!(rows[1].get_named::<i64>("id").unwrap(), 2);
        assert_eq!(
            rows[1].get_named::<String>("slug").unwrap(),
            "tmp-one-shared".to_string()
        );
        assert_eq!(
            rows[1].get_named::<String>("human_key").unwrap(),
            "/tmp/one/shared".to_string()
        );
        assert_eq!(rows[1].get_named::<i64>("created_at").unwrap(), 1);
        assert_eq!(rows[2].get_named::<i64>("id").unwrap(), 3);
        assert_eq!(
            rows[2].get_named::<String>("slug").unwrap(),
            "var-two-shared".to_string()
        );
        assert_eq!(
            rows[2].get_named::<String>("human_key").unwrap(),
            "/var/two/shared".to_string()
        );
        assert_eq!(rows[2].get_named::<i64>("created_at").unwrap(), 2);
        assert_eq!(project_id_map.get(&100), Some(&2));
        assert_eq!(project_id_map.get(&101), Some(&3));
        assert_eq!(stats.salvaged_projects, 2);
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
            stats.agents, 1,
            "Alice from profile; Bob and Carol auto-created as placeholders (not counted in stats)"
        );
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.recipients, 2);
        assert_eq!(stats.parse_errors, 0);

        // Verify the message was inserted correctly
        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let rows = conn
            .query_sync(
                "SELECT subject, body_md, thread_id, recipients_json FROM messages LIMIT 1",
                &[],
            )
            .unwrap();
        assert!(!rows.is_empty(), "message should exist in DB");
        let recipients_json = rows[0]
            .get_named::<String>("recipients_json")
            .expect("recipients_json");
        let recipients_value: serde_json::Value =
            serde_json::from_str(&recipients_json).expect("recipients_json parses");
        assert_eq!(recipients_value["to"], serde_json::json!(["Bob"]));
        assert_eq!(recipients_value["cc"], serde_json::json!([]));
        assert_eq!(recipients_value["bcc"], serde_json::json!(["Carol"]));

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
        assert_eq!(stats.duplicate_canonical_message_files, 1);
        assert_eq!(stats.duplicate_canonical_message_ids, 1);
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
            .query_sync(
                "SELECT id, subject, recipients_json FROM messages ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(message_rows.len(), 2);
        assert_eq!(
            message_rows[1]
                .get_named::<String>("subject")
                .expect("subject"),
            "DB-only"
        );
        let db_only_recipients_json = message_rows[1]
            .get_named::<String>("recipients_json")
            .expect("db-only recipients_json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&db_only_recipients_json)
                .expect("db-only recipients_json parses"),
            serde_json::json!({
                "to": ["Carol"],
                "cc": [],
                "bcc": [],
            })
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

    #[test]
    fn reconstruct_with_salvage_rebuilds_recipients_when_recipient_table_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_missing_recipients.db");
        let salvage_db_path = tmp.path().join("salvage_missing_recipients.db");
        let storage_root = tmp.path().join("storage");

        std::fs::create_dir_all(storage_root.join("projects")).unwrap();

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
                    created_ts INTEGER,
                    recipients_json TEXT
                )",
            )
            .unwrap();

        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'test-project', '/test-project', 1)",
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
                "INSERT INTO messages (id, project_id, sender_id, subject, body_md, created_ts, recipients_json)
                 VALUES
                    (2, 100, 10, 'DB-only', 'db body', 2, '{\"to\":[\"Bob\"],\"cc\":\"Carol\",\"bcc\":[]}')",
                &[],
            )
            .unwrap();

        let stats =
            reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
                .expect("salvage merge should succeed");
        assert_eq!(stats.salvaged_projects, 1);
        assert_eq!(stats.salvaged_agents, 3);
        assert_eq!(stats.salvaged_messages, 1);
        assert_eq!(stats.salvaged_recipients, 2);

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let message_rows = conn
            .query_sync("SELECT recipients_json FROM messages WHERE id = 2", &[])
            .unwrap();
        assert_eq!(message_rows.len(), 1);
        let recipients_json = message_rows[0]
            .get_named::<String>("recipients_json")
            .expect("recipients_json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recipients_json)
                .expect("recipients_json parses"),
            serde_json::json!({
                "to": ["Bob"],
                "cc": ["Carol"],
                "bcc": [],
            })
        );

        let recipient_rows = conn
            .query_sync(
                "SELECT a.name AS name, mr.kind AS kind
                 FROM message_recipients mr
                 JOIN agents a ON a.id = mr.agent_id
                 WHERE mr.message_id = 2
                 ORDER BY mr.kind, a.name",
                &[],
            )
            .unwrap();
        assert_eq!(recipient_rows.len(), 2);
        assert_eq!(
            recipient_rows[0]
                .get_named::<String>("kind")
                .expect("first recipient kind"),
            "cc"
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
    fn reconstruct_with_salvage_enriches_fallback_project_and_agent_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("reconstructed_enriched.db");
        let salvage_db_path = tmp.path().join("salvage_enriched.db");
        let storage_root = tmp.path().join("storage");

        let project_dir = storage_root.join("projects").join("orphan-slug");
        let messages_dir = project_dir.join("messages").join("2026").join("02");
        std::fs::create_dir_all(&messages_dir).unwrap();
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
                    name TEXT NOT NULL,
                    program TEXT,
                    model TEXT,
                    task_description TEXT,
                    inception_ts INTEGER,
                    last_active_ts INTEGER,
                    attachments_policy TEXT,
                    contact_policy TEXT
                )",
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO projects (id, slug, human_key, created_at)
                 VALUES (100, 'orphan-slug', '/Users/demo/projects/orphan', 123)",
                &[],
            )
            .unwrap();
        salvage_conn
            .query_sync(
                "INSERT INTO agents
                 (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy)
                 VALUES
                    (10, 100, 'Alice', 'codex-cli', 'gpt-5', 'investigating', 10, 99, 'inline', 'contacts_only'),
                    (11, 100, 'Bob', 'claude-code', 'sonnet', 'reviewing', 20, 120, 'auto', 'open')",
                &[],
            )
            .unwrap();

        reconstruct_from_archive_with_salvage(&db_path, &storage_root, Some(&salvage_db_path))
            .expect("salvage merge should enrich fallback rows");

        let conn = DbConn::open_file(db_path.to_str().unwrap()).unwrap();
        let project_rows = conn
            .query_sync(
                "SELECT human_key, created_at FROM projects WHERE slug = 'orphan-slug'",
                &[],
            )
            .unwrap();
        assert_eq!(project_rows.len(), 1);
        assert_eq!(
            project_rows[0]
                .get_named::<String>("human_key")
                .expect("human_key"),
            "/Users/demo/projects/orphan"
        );
        assert_eq!(
            project_rows[0]
                .get_named::<i64>("created_at")
                .expect("created_at"),
            123
        );

        let alice_rows = conn
            .query_sync(
                "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy
                 FROM agents
                 WHERE name = 'Alice'",
                &[],
            )
            .unwrap();
        assert_eq!(alice_rows.len(), 1);
        let alice = &alice_rows[0];
        assert_eq!(alice.get_named::<String>("program").unwrap(), "codex-cli");
        assert_eq!(alice.get_named::<String>("model").unwrap(), "gpt-5");
        assert_eq!(
            alice.get_named::<String>("task_description").unwrap(),
            "investigating"
        );
        assert_eq!(alice.get_named::<i64>("inception_ts").unwrap(), 10);
        assert_eq!(alice.get_named::<i64>("last_active_ts").unwrap(), 99);
        assert_eq!(
            alice.get_named::<String>("attachments_policy").unwrap(),
            "inline"
        );
        assert_eq!(
            alice.get_named::<String>("contact_policy").unwrap(),
            "contacts_only"
        );

        let bob_rows = conn
            .query_sync(
                "SELECT program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy
                 FROM agents
                 WHERE name = 'Bob'",
                &[],
            )
            .unwrap();
        assert_eq!(bob_rows.len(), 1);
        let bob = &bob_rows[0];
        assert_eq!(bob.get_named::<String>("program").unwrap(), "claude-code");
        assert_eq!(bob.get_named::<String>("model").unwrap(), "sonnet");
        assert_eq!(
            bob.get_named::<String>("task_description").unwrap(),
            "reviewing"
        );
        assert_eq!(bob.get_named::<i64>("inception_ts").unwrap(), 20);
        assert_eq!(bob.get_named::<i64>("last_active_ts").unwrap(), 120);
        assert_eq!(
            bob.get_named::<String>("attachments_policy").unwrap(),
            "auto"
        );
        assert_eq!(bob.get_named::<String>("contact_policy").unwrap(), "open");
    }
}
