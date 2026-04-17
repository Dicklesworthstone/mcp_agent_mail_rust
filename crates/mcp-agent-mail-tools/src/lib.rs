//! MCP tools and resources implementation for MCP Agent Mail
//!
//! This crate provides implementations for all 34 MCP tools:
//! - Infrastructure cluster (4 tools)
//! - Identity cluster (3 tools)
//! - Messaging cluster (5 tools)
//! - Contact cluster (4 tools)
//! - File reservation cluster (4 tools)
//! - Search cluster (2 tools)
//! - Workflow macro cluster (4 tools)
//! - Product bus cluster (5 tools)
//! - Build slot cluster (3 tools)
//!
//! And 20+ MCP resources for read-only data access.

#![forbid(unsafe_code)]
#![allow(
    clippy::needless_pass_by_value,
    clippy::needless_borrows_for_generic_args,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::needless_borrow,
    clippy::manual_ignore_case_cmp
)]

pub mod build_slots;
pub mod contacts;
pub mod identity;
pub mod llm;
pub mod macros;
pub mod messaging;
pub mod metrics;
pub mod products;
pub mod reservation_index;
pub mod reservations;
pub mod resources;
pub mod search;

// Re-export tool handlers for server registration
pub use build_slots::*;
pub use contacts::*;
pub use identity::*;
pub use macros::*;
pub use messaging::*;
pub use metrics::{
    LatencySnapshot, MetricsSnapshotEntry, record_call, record_call_idx, record_error,
    record_error_idx, record_latency, record_latency_idx, reset_tool_latencies, reset_tool_metrics,
    slow_tools, tool_index, tool_meta, tool_metrics_snapshot, tool_metrics_snapshot_full,
};
pub use products::*;
pub use reservations::*;
pub use resources::*;
pub use search::*;

pub mod tool_util {
    use fastmcp::McpErrorCode;
    use fastmcp::prelude::*;
    use mcp_agent_mail_core::Config;
    use mcp_agent_mail_db::{DbError, DbPool, DbPoolConfig, create_pool, get_or_create_pool};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(crate) const MALFORMED_ATTACHMENTS_SENTINEL: &str = "[malformed-attachments-json]";
    pub(crate) const MALFORMED_RECIPIENTS_SENTINEL: &str = "[malformed-recipients-json]";

    #[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
    pub(crate) struct ParsedRecipients {
        #[serde(default)]
        pub(crate) to: Vec<String>,
        #[serde(default)]
        pub(crate) cc: Vec<String>,
        #[serde(default)]
        pub(crate) bcc: Vec<String>,
    }

    fn malformed_attachments_payload() -> Vec<serde_json::Value> {
        vec![json!({
            "name": MALFORMED_ATTACHMENTS_SENTINEL,
            "media_type": null,
            "path": null,
            "bytes": null,
        })]
    }

    fn malformed_recipients_payload() -> serde_json::Value {
        json!({
            "to": [MALFORMED_RECIPIENTS_SENTINEL],
            "cc": [],
            "bcc": [],
        })
    }

    fn is_valid_recipients_payload(value: &serde_json::Value) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };

        ["to", "cc", "bcc"].iter().all(|key| {
            object.get(*key).is_none_or(|entries| {
                entries
                    .as_array()
                    .is_some_and(|items| items.iter().all(serde_json::Value::is_string))
            })
        })
    }

    pub(crate) fn parse_attachment_metadata_json(input: &str) -> Vec<serde_json::Value> {
        match serde_json::from_str::<Vec<serde_json::Value>>(input) {
            Ok(attachments) => attachments,
            Err(_) if input.trim().is_empty() => Vec::new(),
            Err(_) => malformed_attachments_payload(),
        }
    }

    pub(crate) fn parse_recipients_json_value(input: &str) -> serde_json::Value {
        match serde_json::from_str::<serde_json::Value>(input) {
            Ok(value) if is_valid_recipients_payload(&value) => value,
            Ok(_) | Err(_) if input.trim().is_empty() => json!({}),
            Ok(_) | Err(_) => malformed_recipients_payload(),
        }
    }

    pub(crate) fn parse_recipients_lists(input: &str) -> ParsedRecipients {
        serde_json::from_value(parse_recipients_json_value(input)).unwrap_or_else(|_| {
            ParsedRecipients {
                to: vec![MALFORMED_RECIPIENTS_SENTINEL.to_string()],
                cc: Vec::new(),
                bcc: Vec::new(),
            }
        })
    }

    fn legacy_error_payload(
        error_type: &str,
        message: &str,
        recoverable: bool,
        data: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "error": {
                "type": error_type,
                "message": message,
                "recoverable": recoverable,
                "data": data,
            }
        })
    }

    #[must_use]
    pub fn legacy_mcp_error(
        code: McpErrorCode,
        error_type: &str,
        message: impl Into<String>,
        recoverable: bool,
        data: serde_json::Value,
    ) -> McpError {
        let message = message.into();
        McpError::with_data(
            code,
            message.clone(),
            legacy_error_payload(error_type, &message, recoverable, data),
        )
    }

    #[must_use]
    pub fn legacy_tool_error(
        error_type: &str,
        message: impl Into<String>,
        recoverable: bool,
        data: serde_json::Value,
    ) -> McpError {
        legacy_mcp_error(
            McpErrorCode::ToolExecutionError,
            error_type,
            message,
            recoverable,
            data,
        )
    }

    fn is_retryable_post_commit_visibility_probe(message: &str) -> bool {
        message.contains("not visible after commit")
    }

    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn db_error_to_mcp_error(e: DbError) -> McpError {
        match e {
            DbError::InvalidArgument { field, message } => legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Invalid argument value: {field}: {message}. Check that all parameters have valid values."
                ),
                true,
                json!({
                    "field": field,
                    "error_detail": message,
                }),
            ),
            DbError::NotFound { entity, identifier } => legacy_tool_error(
                "NOT_FOUND",
                format!("{entity} not found: {identifier}"),
                true,
                json!({
                    "entity": entity,
                    "identifier": identifier,
                }),
            ),
            DbError::Duplicate { entity, identifier } => legacy_tool_error(
                "INVALID_ARGUMENT",
                format!("{entity} already exists: {identifier}"),
                true,
                json!({
                    "entity": entity,
                    "identifier": identifier,
                }),
            ),
            DbError::Sqlite(ref message)
            | DbError::Schema(ref message)
            | DbError::Pool(ref message)
                if e.is_corruption() =>
            {
                // Attempt automatic recovery before reporting error.
                if let Ok(pool) = get_db_pool()
                    && matches!(pool.try_recover_from_corruption(message), Ok(true))
                {
                    return legacy_tool_error(
                        "DATABASE_RECOVERED",
                        "Database corruption was detected and the runtime was automatically recovered. \
                         Please retry your operation.",
                        true,
                        json!({ "error_detail": message, "recovered": true }),
                    );
                }
                let message = message.clone();
                legacy_tool_error(
                    "DATABASE_CORRUPTION",
                    format!(
                        "Database corruption detected: {message}. \
                         Run 'am doctor repair' or 'am doctor reconstruct' to recover."
                    ),
                    false,
                    json!({ "error_detail": message }),
                )
            }
            DbError::Sqlite(ref message)
            | DbError::Schema(ref message)
            | DbError::Pool(ref message)
                if mcp_agent_mail_db::is_lock_error(message) =>
            {
                let message = message.clone();
                legacy_tool_error(
                    "RESOURCE_BUSY",
                    "Resource is temporarily busy. Wait a moment and try again.",
                    true,
                    json!({ "error_detail": message }),
                )
            }
            DbError::Pool(message) => legacy_tool_error(
                "DATABASE_POOL_EXHAUSTED",
                "Database connection pool exhausted. Reduce concurrency or increase pool settings.",
                true,
                json!({ "error_detail": message }),
            ),
            DbError::Sqlite(message) | DbError::Schema(message) => legacy_tool_error(
                "DATABASE_ERROR",
                "A database error occurred. This may be a transient issue - try again.",
                true,
                json!({ "error_detail": message }),
            ),
            DbError::Serialization(message) => {
                // Python-parity hint selection based on error content
                let hint = if message.contains("got an unexpected keyword argument") {
                    " Check parameter names for typos."
                } else if message.contains("missing") && message.contains("required") {
                    " Ensure all required parameters are provided."
                } else if message.contains("NoneType") {
                    " A required value was None/null."
                } else {
                    ""
                };
                legacy_tool_error(
                    "TYPE_ERROR",
                    format!("Argument type mismatch: {message}.{hint}"),
                    true,
                    json!({ "error_detail": message }),
                )
            }
            DbError::Internal(message) if is_retryable_post_commit_visibility_probe(&message) => {
                legacy_tool_error(
                    "RESOURCE_BUSY",
                    "Resource is temporarily busy. Wait a moment and try again.",
                    true,
                    json!({ "error_detail": message }),
                )
            }
            DbError::Internal(message) => legacy_tool_error(
                "UNHANDLED_EXCEPTION",
                format!("Unexpected error (DbError): {message}"),
                false,
                json!({ "error_detail": message }),
            ),
            DbError::PoolExhausted {
                message,
                pool_size,
                max_overflow,
            } => legacy_tool_error(
                "DATABASE_POOL_EXHAUSTED",
                "Database connection pool exhausted. Reduce concurrency or increase pool settings.",
                true,
                json!({
                    "error_detail": message,
                    "pool_size": pool_size,
                    "max_overflow": max_overflow,
                }),
            ),
            DbError::ResourceBusy(message) => legacy_tool_error(
                "RESOURCE_BUSY",
                "Resource is temporarily busy. Wait a moment and try again.",
                true,
                json!({ "error_detail": message }),
            ),
            DbError::CircuitBreakerOpen {
                message,
                failures,
                reset_after_secs,
            } => legacy_tool_error(
                "RESOURCE_BUSY",
                format!(
                    "Circuit breaker open: {message}. Database experiencing sustained failures. \
                     Wait {reset_after_secs:.0}s before retrying."
                ),
                true,
                json!({
                    "error_detail": message,
                    "failures": failures,
                    "reset_after_secs": reset_after_secs,
                }),
            ),
            DbError::IntegrityCorruption { message, details } => legacy_tool_error(
                "DATABASE_CORRUPTION",
                format!(
                    "Database integrity check failed: {message}. \
                     The database may be corrupted; consider restoring from backup."
                ),
                false,
                json!({
                    "error_detail": message,
                    "corruption_details": details,
                }),
            ),
        }
    }

    pub fn db_outcome_to_mcp_result<T>(out: Outcome<T, DbError>) -> McpResult<T> {
        match out {
            Outcome::Ok(v) => Ok(v),
            Outcome::Err(e) => Err(db_error_to_mcp_error(e)),
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(p) => Err(McpError::internal_error(format!(
                "Internal panic: {}",
                p.message()
            ))),
        }
    }

    pub fn get_db_pool() -> McpResult<DbPool> {
        let cfg = DbPoolConfig::from_env();
        get_or_create_pool(&cfg).map_err(|e| McpError::internal_error(e.to_string()))
    }

    fn read_pool_setup_error_to_mcp_error(message: String) -> McpError {
        let db_error = if mcp_agent_mail_db::is_lock_error(&message) {
            DbError::ResourceBusy(message)
        } else {
            DbError::Sqlite(message)
        };
        db_error_to_mcp_error(db_error)
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct ReadReconcileInventory {
        projects: usize,
        agents: usize,
        messages: usize,
        max_message_id: i64,
        project_identities: BTreeSet<mcp_agent_mail_db::MailboxProjectIdentity>,
    }

    fn query_read_db_inventory(
        conn: &mcp_agent_mail_db::DbConn,
    ) -> Result<ReadReconcileInventory, String> {
        let tables = conn
            .query_sync(
                "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('projects','agents','messages')",
                &[],
            )
            .map_err(|err| err.to_string())?;
        let present: BTreeSet<String> = tables
            .iter()
            .filter_map(|row| row.get_named::<String>("name").ok())
            .collect();

        let projects = if present.contains("projects") {
            let rows = conn
                .query_sync("SELECT COUNT(*) AS project_count FROM projects", &[])
                .map_err(|err| err.to_string())?;
            rows.first()
                .and_then(|row| row.get_named::<i64>("project_count").ok())
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0)
        } else {
            0
        };
        let agents = if present.contains("agents") {
            let rows = conn
                .query_sync("SELECT COUNT(*) AS agent_count FROM agents", &[])
                .map_err(|err| err.to_string())?;
            rows.first()
                .and_then(|row| row.get_named::<i64>("agent_count").ok())
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(0)
        } else {
            0
        };
        let (messages, max_message_id) = if present.contains("messages") {
            let rows = conn
                .query_sync(
                    "SELECT COUNT(*) AS message_count, COALESCE(MAX(id), 0) AS max_id FROM messages",
                    &[],
                )
                .map_err(|err| err.to_string())?;
            let Some(row) = rows.first() else {
                return Err("no rows returned from read message inventory query".to_string());
            };
            (
                row.get_named::<i64>("message_count")
                    .ok()
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(0),
                row.get_named::<i64>("max_id").unwrap_or(0),
            )
        } else {
            (0, 0)
        };
        let project_identities = if present.contains("projects") {
            mcp_agent_mail_db::collect_db_project_identities(conn).map_err(|err| err.to_string())?
        } else {
            BTreeSet::new()
        };

        Ok(ReadReconcileInventory {
            projects,
            agents,
            messages,
            max_message_id,
            project_identities,
        })
    }

    fn read_archive_inventory_has_state(storage_root: &Path) -> bool {
        let archive = mcp_agent_mail_db::scan_archive_message_inventory(storage_root);
        archive.projects > 0 || archive.agents > 0 || archive.unique_message_ids > 0
    }

    pub(crate) fn archive_storage_root_is_authoritative_for_sqlite_path(
        storage_root: &Path,
        sqlite_path: &Path,
    ) -> bool {
        !mcp_agent_mail_core::config::is_default_storage_root(storage_root)
            || sqlite_path.starts_with(storage_root)
    }

    fn read_archive_is_ahead(
        storage_root: &Path,
        sqlite_path: &Path,
        conn: &mcp_agent_mail_db::DbConn,
    ) -> Result<bool, String> {
        if !archive_storage_root_is_authoritative_for_sqlite_path(storage_root, sqlite_path) {
            return Ok(false);
        }

        let archive = mcp_agent_mail_db::scan_archive_message_inventory(storage_root);
        if archive.projects == 0 && archive.agents == 0 && archive.unique_message_ids == 0 {
            return Ok(false);
        }

        let db_inventory = query_read_db_inventory(conn)?;
        let archive_message_count = archive.unique_message_ids;
        let archive_max_id = archive.latest_message_id.unwrap_or(0);
        let missing_archive_projects = mcp_agent_mail_db::archive_missing_project_identities(
            &archive,
            &db_inventory.project_identities,
        );

        let archive_metadata_ahead =
            mcp_agent_mail_db::pool::archive_metadata_advantage_is_decisive(
                archive.projects,
                archive.agents,
                archive_message_count,
                archive.latest_message_id,
                db_inventory.projects,
                db_inventory.agents,
                db_inventory.messages,
                db_inventory.max_message_id,
                &missing_archive_projects,
            );

        Ok(archive_message_count > db_inventory.messages
            || archive_max_id > db_inventory.max_message_id
            || archive_metadata_ahead)
    }

    static READ_SNAPSHOT_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct ToolReadPool {
        pool: mcp_agent_mail_db::DbPool,
        _snapshot_dir: Option<ReadSnapshotDirGuard>,
    }

    impl ToolReadPool {
        const fn live(pool: mcp_agent_mail_db::DbPool) -> Self {
            Self {
                pool,
                _snapshot_dir: None,
            }
        }
    }

    impl std::ops::Deref for ToolReadPool {
        type Target = mcp_agent_mail_db::DbPool;

        fn deref(&self) -> &Self::Target {
            &self.pool
        }
    }

    struct ReadSnapshotDirGuard {
        path: PathBuf,
    }

    impl ReadSnapshotDirGuard {
        fn new(prefix: &str) -> std::io::Result<Self> {
            let base = std::env::temp_dir();
            let pid = std::process::id();
            for _ in 0..32 {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let counter = READ_SNAPSHOT_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = base.join(format!("{prefix}{pid}_{nanos}_{counter}"));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Ok(Self { path }),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "failed to allocate unique read snapshot directory",
            ))
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for ReadSnapshotDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Check whether the live `SQLite` database is suspect (`DegradedReadOnly` or
    /// worse) according to a fast mailbox verdict. Returns `true` when read
    /// surfaces should fall back to archive snapshots instead of the
    /// potentially corrupt live file.
    fn live_db_is_suspect(database_url: &str, storage_root: &Path, sqlite_path: &Path) -> bool {
        if !archive_storage_root_is_authoritative_for_sqlite_path(storage_root, sqlite_path) {
            return false;
        }

        let verdict = mcp_agent_mail_db::compute_mailbox_verdict(
            database_url,
            storage_root,
            &mcp_agent_mail_db::VerdictOptions::fast(),
        );
        let durability = mcp_agent_mail_db::DurabilityState::from_mailbox_state(verdict.state);
        let prefer_archive =
            mcp_agent_mail_db::verdict_prefers_archive_snapshot_reads_for_primary_read_surface(
                &verdict,
                sqlite_path,
            );
        if prefer_archive && durability.allows_reads() {
            // DegradedReadOnly — reads should come from archive snapshots.
            tracing::info!(
                verdict_state = %verdict.state,
                durability_state = %durability,
                "live SQLite is suspect; read surfaces will prefer archive snapshots"
            );
            true
        } else if prefer_archive && !durability.allows_reads() {
            // Corrupt / Recovering — reads are fully blocked on the live path,
            // so we should also try archive snapshots as a last resort.
            tracing::warn!(
                verdict_state = %verdict.state,
                durability_state = %durability,
                "live SQLite is corrupt/recovering; read surfaces will attempt archive snapshot fallback"
            );
            true
        } else {
            false
        }
    }

    fn open_read_db_pool() -> Result<Option<ToolReadPool>, String> {
        let config = Config::from_env();
        if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url) {
            return Ok(None);
        }

        let sqlite_path =
            mcp_agent_mail_db::pool::resolve_mailbox_sqlite_path(&config.database_url)
                .map_err(|err| err.to_string())?
                .canonical_path;
        if sqlite_path == ":memory:" {
            return Ok(None);
        }

        let resolved_path = PathBuf::from(&sqlite_path);
        let archive_has_state = archive_storage_root_is_authoritative_for_sqlite_path(
            &config.storage_root,
            &resolved_path,
        ) && read_archive_inventory_has_state(&config.storage_root);

        // When the durability verdict says the live DB is suspect or worse,
        // force archive-snapshot reads even if the archive isn't strictly
        // "ahead" of the DB by row count.
        let durability_forces_snapshot = archive_has_state
            && live_db_is_suspect(&config.database_url, &config.storage_root, &resolved_path);

        let use_archive_snapshot = if durability_forces_snapshot {
            true
        } else {
            match mcp_agent_mail_db::DbConn::open_file(&sqlite_path) {
                Ok(conn) => {
                    let archive_ahead =
                        read_archive_is_ahead(&config.storage_root, &resolved_path, &conn);
                    drop(conn);
                    match archive_ahead {
                        Ok(true) => true,
                        Err(error) if archive_has_state => {
                            tracing::warn!(
                                source = %resolved_path.display(),
                                storage_root = %config.storage_root.display(),
                                error = %error,
                                "using archive-backed tool snapshot because the live sqlite inventory probe failed"
                            );
                            true
                        }
                        Ok(false) | Err(_) => false,
                    }
                }
                Err(error) if archive_has_state => {
                    tracing::warn!(
                        source = %resolved_path.display(),
                        storage_root = %config.storage_root.display(),
                        error = %error,
                        "using archive-backed tool snapshot because the live sqlite source could not be opened"
                    );
                    true
                }
                Err(_) => false,
            }
        };

        if !use_archive_snapshot {
            return Ok(None);
        }

        let snapshot_dir = ReadSnapshotDirGuard::new("agent-mail-tool-snapshot-")
            .map_err(|err| err.to_string())?;
        let snapshot_db = snapshot_dir.path().join("mailbox.sqlite3");
        if resolved_path.exists() {
            mcp_agent_mail_db::reconstruct_from_archive_with_salvage(
                &snapshot_db,
                &config.storage_root,
                Some(resolved_path.as_path()),
            )
            .map_err(|err| err.to_string())?;
        } else {
            mcp_agent_mail_db::reconstruct_from_archive(&snapshot_db, &config.storage_root)
                .map_err(|err| err.to_string())?;
        }
        let pool = create_pool(&mcp_agent_mail_db::DbPoolConfig {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(&snapshot_db),
            storage_root: Some(config.storage_root),
            ..Default::default()
        })
        .map_err(|err| err.to_string())?;
        Ok(Some(ToolReadPool {
            pool,
            _snapshot_dir: Some(snapshot_dir),
        }))
    }

    pub fn get_read_db_pool() -> McpResult<ToolReadPool> {
        match open_read_db_pool() {
            Ok(Some(pool)) => Ok(pool),
            Ok(None) => get_db_pool().map(ToolReadPool::live),
            Err(error) => Err(read_pool_setup_error_to_mcp_error(error)),
        }
    }

    /// Placeholder patterns that indicate unconfigured hooks/settings.
    const PLACEHOLDER_PATTERNS: &[&str] = &[
        "YOUR_PROJECT_PATH",
        "YOUR_PROJECT_KEY",
        "YOUR_PROJECT",
        "PLACEHOLDER",
        "<PROJECT>",
        "{PROJECT}",
        "$PROJECT",
    ];

    /// Compute similarity ratio between two strings (0.0 to 1.0).
    ///
    /// Mimics Python's `difflib.SequenceMatcher.ratio()` which returns
    /// `2.0 * matching_chars / total_chars`.
    fn similarity_score(a: &str, b: &str) -> f64 {
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        let total = a_bytes.len() + b_bytes.len();
        if total == 0 {
            return 1.0;
        }
        // LCS-based matching count (same algorithm as SequenceMatcher)
        let m = a_bytes.len();
        let n = b_bytes.len();
        // Use DP for LCS length
        let mut prev = vec![0usize; n + 1];
        let mut curr = vec![0usize; n + 1];
        for i in 1..=m {
            for j in 1..=n {
                curr[j] =
                    if a_bytes[i - 1].to_ascii_lowercase() == b_bytes[j - 1].to_ascii_lowercase() {
                        prev[j - 1] + 1
                    } else {
                        prev[j].max(curr[j - 1])
                    };
            }
            std::mem::swap(&mut prev, &mut curr);
            curr.fill(0);
        }
        #[allow(clippy::cast_precision_loss)]
        let lcs_len = prev[n] as f64;
        let Ok(total_u32) = u32::try_from(total) else {
            return 0.0;
        };
        2.0 * lcs_len / f64::from(total_u32)
    }

    /// Find projects with similar slugs/names.
    async fn find_similar_projects(
        ctx: &McpContext,
        pool: &DbPool,
        identifier: &str,
        limit: usize,
        min_score: f64,
    ) -> Vec<(String, String, f64)> {
        let slug = mcp_agent_mail_core::slugify(identifier);
        let out = mcp_agent_mail_db::queries::list_projects(ctx.cx(), pool).await;
        let asupersync::Outcome::Ok(projects) = out else {
            return Vec::new();
        };
        let mut suggestions: Vec<(String, String, f64)> = Vec::new();
        for p in &projects {
            let slug_score = similarity_score(&slug, &p.slug);
            let key_score = if p.human_key.is_empty() {
                0.0
            } else {
                similarity_score(identifier, &p.human_key)
            };
            let best = slug_score.max(key_score);
            if best >= min_score {
                suggestions.push((p.slug.clone(), p.human_key.clone(), best));
            }
        }
        suggestions.sort_by(|a, b| {
            b.2.total_cmp(&a.2)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        suggestions.truncate(limit);
        suggestions
    }

    #[allow(clippy::too_many_lines)]
    pub async fn resolve_project(
        ctx: &McpContext,
        pool: &DbPool,
        project_key: &str,
    ) -> McpResult<mcp_agent_mail_db::ProjectRow> {
        // 1. Empty/whitespace check
        if project_key.is_empty() || project_key.trim().is_empty() {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                "Project identifier cannot be empty. Provide a project path like '/data/projects/myproject' or a slug like 'myproject'.",
                true,
                json!({"parameter": "project_key", "provided": format!("{project_key:?}")}),
            ));
        }

        let raw_identifier = project_key.trim();

        // 2. Placeholder detection
        let identifier_upper = raw_identifier.to_ascii_uppercase();
        for pattern in PLACEHOLDER_PATTERNS {
            if identifier_upper.contains(pattern) || identifier_upper == *pattern {
                return Err(legacy_tool_error(
                    "CONFIGURATION_ERROR",
                    format!(
                        "Detected placeholder value '{raw_identifier}' instead of a real project path. \
                         This typically means a hook or integration script hasn't been configured yet. \
                         Replace placeholder values in your .claude/settings.json or environment variables \
                         with actual project paths like '/Users/you/projects/myproject'."
                    ),
                    true,
                    json!({
                        "parameter": "project_key",
                        "provided": raw_identifier,
                        "detected_placeholder": pattern,
                        "fix_hint": "Update AGENT_MAIL_PROJECT or project_key in your configuration",
                    }),
                ));
            }
        }

        // Check read cache first (slug lookups only; ensure_project always hits DB)
        let is_absolute = std::path::Path::new(raw_identifier).is_absolute();
        if !is_absolute
            && let Some(cached) = mcp_agent_mail_db::read_cache().get_project(raw_identifier)
        {
            return Ok(cached);
        }
        let out = if is_absolute {
            mcp_agent_mail_db::queries::ensure_project(ctx.cx(), pool, raw_identifier).await
        } else {
            mcp_agent_mail_db::queries::get_project_by_slug(ctx.cx(), pool, raw_identifier).await
        };

        match db_outcome_to_mcp_result(out) {
            Ok(project) => {
                // Populate cache on miss
                mcp_agent_mail_db::read_cache().put_project(&project);
                Ok(project)
            }
            Err(e) => {
                // Only enhance NOT_FOUND errors with fuzzy suggestions
                let is_not_found = e
                    .data
                    .as_ref()
                    .and_then(|d| d["error"]["type"].as_str())
                    .is_some_and(|t| t == "NOT_FOUND");

                if !is_not_found {
                    return Err(e);
                }

                // 3/4. NOT_FOUND: try fuzzy suggestions
                let slug = mcp_agent_mail_core::slugify(raw_identifier);
                let suggestions = find_similar_projects(ctx, pool, raw_identifier, 5, 0.4).await;

                if suggestions.is_empty() {
                    Err(legacy_tool_error(
                        "NOT_FOUND",
                        format!(
                            "Project '{raw_identifier}' not found and no similar projects exist. \
                             Use ensure_project to create a new project first. \
                             Example: ensure_project(human_key='/path/to/your/project')"
                        ),
                        true,
                        json!({"identifier": raw_identifier, "slug_searched": slug}),
                    ))
                } else {
                    let suggestion_text = suggestions
                        .iter()
                        .take(3)
                        .map(|s| format!("'{}'", s.0))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let suggestions_data: Vec<serde_json::Value> = suggestions
                        .iter()
                        .map(|s| {
                            json!({
                                "slug": s.0,
                                "human_key": s.1,
                                "score": (s.2 * 100.0).round() / 100.0,
                            })
                        })
                        .collect();
                    Err(legacy_tool_error(
                        "NOT_FOUND",
                        format!(
                            "Project '{raw_identifier}' not found. Did you mean: {suggestion_text}? \
                             Use ensure_project to create a new project, or check spelling."
                        ),
                        true,
                        json!({
                            "identifier": raw_identifier,
                            "slug_searched": slug,
                            "suggestions": suggestions_data,
                        }),
                    ))
                }
            }
        }
    }

    /// Agent placeholder patterns that indicate unconfigured hooks/settings.
    const AGENT_PLACEHOLDER_PATTERNS: &[&str] = &[
        "YOUR_AGENT",
        "YOUR_AGENT_NAME",
        "AGENT_NAME",
        "PLACEHOLDER",
        "<AGENT>",
        "{AGENT}",
        "$AGENT",
    ];

    /// Find agents with similar names in a project.
    async fn find_similar_agents(
        ctx: &McpContext,
        pool: &DbPool,
        project_id: i64,
        name: &str,
        limit: usize,
        min_score: f64,
    ) -> Vec<(String, f64)> {
        let out = mcp_agent_mail_db::queries::list_agents(ctx.cx(), pool, project_id).await;
        let asupersync::Outcome::Ok(agents) = out else {
            return Vec::new();
        };
        let mut suggestions: Vec<(String, f64)> = Vec::new();
        for a in &agents {
            let score = similarity_score(name, &a.name);
            if score >= min_score {
                suggestions.push((a.name.clone(), score));
            }
        }
        suggestions.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        suggestions.truncate(limit);
        suggestions
    }

    /// List agent names in a project (up to `limit`).
    async fn list_project_agent_names(
        ctx: &McpContext,
        pool: &DbPool,
        project_id: i64,
        limit: usize,
    ) -> (Vec<String>, usize) {
        let out = mcp_agent_mail_db::queries::list_agents(ctx.cx(), pool, project_id).await;
        let asupersync::Outcome::Ok(agents) = out else {
            return (Vec::new(), 0);
        };
        let total = agents.len();
        let names: Vec<String> = agents.into_iter().take(limit).map(|a| a.name).collect();
        (names, total)
    }

    #[allow(clippy::too_many_lines)]
    pub async fn resolve_agent(
        ctx: &McpContext,
        pool: &DbPool,
        project_id: i64,
        agent_name: &str,
        project_slug: &str,
        project_human_key: &str,
    ) -> McpResult<mcp_agent_mail_db::AgentRow> {
        // 1. Empty/whitespace check
        if agent_name.is_empty() || agent_name.trim().is_empty() {
            return Err(legacy_tool_error(
                "INVALID_ARGUMENT",
                format!(
                    "Agent name cannot be empty. Provide a valid agent name for project '{project_human_key}'."
                ),
                true,
                json!({"parameter": "agent_name", "provided": format!("{agent_name:?}"), "project": project_slug}),
            ));
        }

        let name_raw = agent_name.trim();
        // Normalize name if it follows the adj+noun pattern, otherwise keep as-is.
        let name_norm = mcp_agent_mail_core::models::normalize_agent_name(name_raw)
            .unwrap_or_else(|| name_raw.to_string());
        let name = &name_norm;

        // 2. Agent placeholder detection
        let name_upper = name.to_ascii_uppercase();
        for pattern in AGENT_PLACEHOLDER_PATTERNS {
            if name_upper.contains(pattern) || name_upper == *pattern {
                return Err(legacy_tool_error(
                    "CONFIGURATION_ERROR",
                    format!(
                        "Detected placeholder value '{name}' instead of a real agent name. \
                         This typically means a hook or integration script hasn't been configured yet. \
                         Replace placeholder values with your actual agent name (e.g., 'BlueMountain')."
                    ),
                    true,
                    json!({
                        "parameter": "agent_name",
                        "provided": name,
                        "detected_placeholder": pattern,
                        "fix_hint": "Update AGENT_MAIL_AGENT or agent_name in your configuration",
                    }),
                ));
            }
        }

        // Check read cache first
        if let Some(cached) = mcp_agent_mail_db::read_cache().get_agent(project_id, name) {
            return Ok(cached);
        }
        let out = mcp_agent_mail_db::queries::get_agent(ctx.cx(), pool, project_id, name).await;

        match db_outcome_to_mcp_result(out) {
            Ok(agent) => {
                // Populate cache on miss
                mcp_agent_mail_db::read_cache().put_agent(&agent);
                Ok(agent)
            }
            Err(e) => {
                // Only enhance NOT_FOUND errors with suggestions
                let is_not_found = e
                    .data
                    .as_ref()
                    .and_then(|d| d["error"]["type"].as_str())
                    .is_some_and(|t| t == "NOT_FOUND");

                if !is_not_found {
                    return Err(e);
                }

                // Check for common agent name mistakes
                let mistake = mcp_agent_mail_core::detect_agent_name_mistake(name);
                let mistake_hint = mistake
                    .as_ref()
                    .map(|(_, msg)| format!("\n\nHINT: {msg}"))
                    .unwrap_or_default();
                let mistake_type = mistake.as_ref().map(|(t, _)| *t);

                let suggestions = find_similar_agents(ctx, pool, project_id, name, 5, 0.4).await;
                let (available_agents, total_agents) =
                    list_project_agent_names(ctx, pool, project_id, 10).await;

                let error_type = mistake_type.unwrap_or("NOT_FOUND");

                if !suggestions.is_empty() {
                    // 3. Agent not found WITH suggestions
                    let suggestion_text = suggestions
                        .iter()
                        .take(3)
                        .map(|s| format!("'{}'", s.0))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let suggestions_data: Vec<serde_json::Value> = suggestions
                        .iter()
                        .map(|s| json!({"name": s.0, "score": (s.1 * 100.0).round() / 100.0}))
                        .collect();
                    Err(legacy_tool_error(
                        error_type,
                        format!(
                            "Agent '{name}' not found in project '{project_human_key}'. \
                             Did you mean: {suggestion_text}? \
                             Agent names are case-insensitive but must match exactly.{mistake_hint}"
                        ),
                        true,
                        json!({
                            "agent_name": name,
                            "project": project_slug,
                            "suggestions": suggestions_data,
                            "available_agents": available_agents,
                            "mistake_type": mistake_type,
                        }),
                    ))
                } else if !available_agents.is_empty() {
                    // 4. Agent not found, agents exist but no match
                    let agents_list = available_agents
                        .iter()
                        .take(5)
                        .map(|a| format!("'{a}'"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let more_text = if total_agents > 5 {
                        format!(" and {} more", total_agents - 5)
                    } else {
                        String::new()
                    };
                    Err(legacy_tool_error(
                        error_type,
                        format!(
                            "Agent '{name}' not found in project '{project_human_key}'. \
                             Available agents: {agents_list}{more_text}. \
                             Use register_agent to create a new agent identity.{mistake_hint}"
                        ),
                        true,
                        json!({
                            "agent_name": name,
                            "project": project_slug,
                            "available_agents": available_agents,
                            "mistake_type": mistake_type,
                        }),
                    ))
                } else {
                    // 5. No agents in project
                    Err(legacy_tool_error(
                        error_type,
                        format!(
                            "Agent '{name}' not found. Project '{project_human_key}' has no registered agents yet. \
                             Use register_agent to create an agent identity first \
                             (omit 'name' to auto-generate a valid one). \
                             Example: register_agent(project_key='{project_slug}', \
                             program='claude-code', model='opus-4'){mistake_hint}"
                        ),
                        true,
                        json!({
                            "agent_name": name,
                            "project": project_slug,
                            "available_agents": Vec::<String>::new(),
                            "mistake_type": mistake_type,
                        }),
                    ))
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::{Mutex, OnceLock};

        static READ_POOL_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

        #[test]
        fn legacy_tool_error_sets_payload_shape() {
            let err = legacy_tool_error(
                "NOT_FOUND",
                "Project 'x' not found",
                true,
                json!({"entity":"Project","identifier":"x"}),
            );
            assert_eq!(err.code, McpErrorCode::ToolExecutionError);
            assert_eq!(err.message, "Project 'x' not found");
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "NOT_FOUND");
            assert_eq!(data["error"]["message"], "Project 'x' not found");
            assert_eq!(data["error"]["recoverable"], true);
            assert_eq!(data["error"]["data"]["entity"], "Project");
        }

        #[test]
        fn db_error_to_mcp_error_maps_not_found() {
            let err = db_error_to_mcp_error(DbError::not_found("Agent", "BlueLake"));
            assert_eq!(err.code, McpErrorCode::ToolExecutionError);
            assert!(err.message.contains("Agent not found"));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "NOT_FOUND");
            assert_eq!(data["error"]["recoverable"], true);
            assert_eq!(data["error"]["data"]["entity"], "Agent");
        }

        #[test]
        fn db_error_to_mcp_error_maps_duplicate() {
            let err = db_error_to_mcp_error(DbError::duplicate("Agent", "BlueLake"));
            assert_eq!(err.code, McpErrorCode::ToolExecutionError);
            assert!(err.message.contains("already exists"));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
            assert_eq!(data["error"]["recoverable"], true);
            assert_eq!(data["error"]["data"]["entity"], "Agent");
            assert_eq!(data["error"]["data"]["identifier"], "BlueLake");
        }

        #[test]
        fn parse_attachment_metadata_json_surfaces_malformed_payloads() {
            assert!(parse_attachment_metadata_json("").is_empty());
            assert_eq!(
                parse_attachment_metadata_json("{not-json")[0]["name"],
                MALFORMED_ATTACHMENTS_SENTINEL
            );
        }

        #[test]
        fn parse_recipients_lists_surfaces_malformed_payloads() {
            assert_eq!(parse_recipients_lists(""), ParsedRecipients::default());
            assert_eq!(
                parse_recipients_lists(r#"{"to":"BlueLake"}"#).to,
                vec![MALFORMED_RECIPIENTS_SENTINEL.to_string()]
            );
            assert_eq!(
                parse_recipients_lists("{not-json").to,
                vec![MALFORMED_RECIPIENTS_SENTINEL.to_string()]
            );
        }

        #[test]
        fn db_error_to_mcp_error_maps_invalid_argument() {
            let err =
                db_error_to_mcp_error(DbError::invalid("agent_name", "must be adjective+noun"));
            assert_eq!(err.code, McpErrorCode::ToolExecutionError);
            assert!(err.message.contains("agent_name"));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "INVALID_ARGUMENT");
            assert_eq!(data["error"]["recoverable"], true);
        }

        #[test]
        fn db_error_to_mcp_error_maps_pool_error() {
            let err = db_error_to_mcp_error(DbError::Pool("timeout".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_POOL_EXHAUSTED");
            assert_eq!(data["error"]["recoverable"], true);
        }

        #[test]
        fn open_read_db_pool_ignores_unrelated_default_archive_overlap() {
            let _guard = READ_POOL_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("custom.sqlite3");
            let database_url = format!("sqlite:///{}", db_path.display());
            let xdg_data_home = temp.path().join("xdg");
            let xdg_data_home_text = xdg_data_home.to_string_lossy().into_owned();

            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[
                    ("DATABASE_URL", database_url.as_str()),
                    ("XDG_DATA_HOME", xdg_data_home_text.as_str()),
                ],
                || {
                    Config::reset_cached();
                    let storage_root = Config::from_env().storage_root;
                    let project_dir = storage_root.join("projects").join("ahead-project");
                    let agent_dir = project_dir.join("agents").join("Alice");
                    let message_dir = project_dir.join("messages").join("2026").join("04");
                    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
                    std::fs::create_dir_all(&message_dir).expect("create message dir");
                    std::fs::write(
                        project_dir.join("project.json"),
                        r#"{"slug":"ahead-project","human_key":"/ahead-project"}"#,
                    )
                    .expect("write project metadata");
                    std::fs::write(agent_dir.join("profile.json"), "{}")
                        .expect("write agent profile");
                    std::fs::write(
                        message_dir.join("2026-04-01T12-00-00Z__archive-only__7.md"),
                        "---json\n{\"id\":7,\"from\":\"Alice\",\"to\":[],\"subject\":\"Archive only\"}\n---\nbody\n",
                    )
                    .expect("write canonical message");

                    let conn =
                        mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref())
                            .expect("open db");
                    conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                        .expect("init schema");
                    conn.query_sync(
                        "INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'ahead-project', '/ahead-project', 0)",
                        &[],
                    )
                    .expect("insert overlapping project");
                    drop(conn);

                    let pool = open_read_db_pool().expect("open read db pool");
                    assert!(
                        pool.is_none(),
                        "default global archive should not force shared tool read snapshots for an external custom DB"
                    );
                },
            );
        }

        #[test]
        fn db_error_to_mcp_error_maps_pool_corruption() {
            let err =
                db_error_to_mcp_error(DbError::Pool("database disk image is malformed".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_CORRUPTION");
            assert_eq!(data["error"]["recoverable"], false);
        }

        #[test]
        fn db_error_to_mcp_error_maps_pool_exhausted() {
            let err = db_error_to_mcp_error(DbError::PoolExhausted {
                message: "all connections in use".into(),
                pool_size: 10,
                max_overflow: 5,
            });
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_POOL_EXHAUSTED");
            assert_eq!(data["error"]["data"]["pool_size"], 10);
            assert_eq!(data["error"]["data"]["max_overflow"], 5);
        }

        #[test]
        fn db_error_to_mcp_error_maps_sqlite() {
            let err = db_error_to_mcp_error(DbError::Sqlite("constraint violation".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_ERROR");
            assert_eq!(data["error"]["recoverable"], true);
        }

        #[test]
        fn db_error_to_mcp_error_maps_sqlite_lock_as_resource_busy() {
            let err = db_error_to_mcp_error(DbError::Sqlite("database is locked".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["recoverable"], true);
        }

        #[test]
        fn db_error_to_mcp_error_maps_schema() {
            let err = db_error_to_mcp_error(DbError::Schema("migration v4 failed".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_ERROR");
        }

        #[test]
        fn db_error_to_mcp_error_maps_schema_corruption() {
            let err =
                db_error_to_mcp_error(DbError::Schema("database disk image is malformed".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_CORRUPTION");
            assert_eq!(data["error"]["recoverable"], false);
        }

        #[test]
        fn db_error_to_mcp_error_maps_serialization() {
            let err = db_error_to_mcp_error(DbError::Serialization("invalid JSON".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "TYPE_ERROR");
            assert!(
                data["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("type mismatch")
            );
        }

        #[test]
        fn type_error_hint_unexpected_keyword() {
            let err = db_error_to_mcp_error(DbError::Serialization(
                "foo() got an unexpected keyword argument 'bar'".into(),
            ));
            let data = err.data.expect("expected data payload");
            let msg = data["error"]["message"].as_str().unwrap();
            assert!(
                msg.ends_with("Check parameter names for typos."),
                "expected typo hint, got: {msg}"
            );
        }

        #[test]
        fn type_error_hint_missing_required() {
            let err = db_error_to_mcp_error(DbError::Serialization(
                "missing 1 required positional argument: 'x'".into(),
            ));
            let data = err.data.expect("expected data payload");
            let msg = data["error"]["message"].as_str().unwrap();
            assert!(
                msg.ends_with("Ensure all required parameters are provided."),
                "expected required-params hint, got: {msg}"
            );
        }

        #[test]
        fn type_error_hint_nonetype() {
            let err = db_error_to_mcp_error(DbError::Serialization(
                "unsupported operand type(s) for +: 'NoneType' and 'int'".into(),
            ));
            let data = err.data.expect("expected data payload");
            let msg = data["error"]["message"].as_str().unwrap();
            assert!(
                msg.ends_with("A required value was None/null."),
                "expected NoneType hint, got: {msg}"
            );
        }

        #[test]
        fn type_error_no_hint_generic() {
            let err = db_error_to_mcp_error(DbError::Serialization("invalid JSON".into()));
            let data = err.data.expect("expected data payload");
            let msg = data["error"]["message"].as_str().unwrap();
            assert_eq!(msg, "Argument type mismatch: invalid JSON.");
        }

        #[test]
        fn db_error_to_mcp_error_maps_resource_busy() {
            let err = db_error_to_mcp_error(DbError::ResourceBusy("SQLITE_BUSY".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["recoverable"], true);
        }

        #[test]
        fn db_error_to_mcp_error_maps_circuit_breaker() {
            let err = db_error_to_mcp_error(DbError::CircuitBreakerOpen {
                message: "sustained failures".into(),
                failures: 5,
                reset_after_secs: 30.0,
            });
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["data"]["failures"], 5);
            assert!(data["error"]["message"].as_str().unwrap().contains("30"));
        }

        #[test]
        fn db_error_to_mcp_error_maps_integrity_corruption() {
            let err = db_error_to_mcp_error(DbError::IntegrityCorruption {
                message: "page checksum mismatch".into(),
                details: vec!["page 42".into(), "page 99".into()],
            });
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "DATABASE_CORRUPTION");
            assert_eq!(data["error"]["recoverable"], false);
            assert!(
                data["error"]["data"]["corruption_details"]
                    .as_array()
                    .unwrap()
                    .len()
                    == 2
            );
        }

        #[test]
        fn db_error_to_mcp_error_maps_internal() {
            let err = db_error_to_mcp_error(DbError::Internal("unexpected state".into()));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "UNHANDLED_EXCEPTION");
            assert_eq!(data["error"]["recoverable"], false);
        }

        #[test]
        fn db_error_to_mcp_error_maps_post_commit_visibility_probe_as_resource_busy() {
            let err = db_error_to_mcp_error(DbError::Internal(
                "agent row not visible after commit for 1:BlueLake".into(),
            ));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["recoverable"], true);
        }

        #[test]
        fn db_error_to_mcp_error_maps_post_commit_recipient_visibility_probe_as_resource_busy() {
            let err = db_error_to_mcp_error(DbError::Internal(
                "message recipient rows not visible after commit for message_id=42: expected=1 actual=0".into(),
            ));
            let data = err.data.expect("expected data payload");
            assert_eq!(data["error"]["type"], "RESOURCE_BUSY");
            assert_eq!(data["error"]["recoverable"], true);
        }

        // -------------------------------------------------------------------
        // similarity_score
        // -------------------------------------------------------------------

        #[test]
        fn similarity_identical_strings() {
            let score = similarity_score("hello", "hello");
            assert!((score - 1.0).abs() < f64::EPSILON);
        }

        #[test]
        fn similarity_empty_strings() {
            let score = similarity_score("", "");
            assert!((score - 1.0).abs() < f64::EPSILON);
        }

        #[test]
        fn similarity_one_empty() {
            let score = similarity_score("hello", "");
            assert!((score - 0.0).abs() < f64::EPSILON);
        }

        #[test]
        fn similarity_case_insensitive() {
            let score = similarity_score("Hello", "hello");
            assert!((score - 1.0).abs() < f64::EPSILON);
        }

        #[test]
        fn similarity_similar_strings() {
            let score = similarity_score("myproject", "my-project");
            // Should be reasonably high (> 0.8)
            assert!(score > 0.8);
        }

        #[test]
        fn similarity_dissimilar_strings() {
            let score = similarity_score("abcdef", "xyz123");
            assert!(score < 0.3);
        }

        #[test]
        fn similarity_partial_overlap() {
            let score = similarity_score("backend", "backend-api");
            // Should be moderately high
            assert!(score > 0.6);
        }

        #[test]
        fn similarity_is_symmetric() {
            let s1 = similarity_score("project-a", "project-b");
            let s2 = similarity_score("project-b", "project-a");
            assert!((s1 - s2).abs() < f64::EPSILON);
        }

        // -------------------------------------------------------------------
        // placeholder detection
        // -------------------------------------------------------------------

        #[test]
        fn placeholder_your_project_detected() {
            for pattern in PLACEHOLDER_PATTERNS {
                let upper = pattern.to_string();
                // Direct match
                assert!(
                    upper.to_ascii_uppercase().contains(pattern)
                        || upper.to_ascii_uppercase() == *pattern,
                    "pattern {pattern} should match itself"
                );
            }
        }

        #[test]
        fn placeholder_case_insensitive() {
            let identifier = "your_project";
            let upper = identifier.to_ascii_uppercase();
            assert!(
                PLACEHOLDER_PATTERNS
                    .iter()
                    .any(|p| upper.contains(p) || upper == *p),
                "your_project should match YOUR_PROJECT pattern"
            );
        }

        #[test]
        fn placeholder_substring_match() {
            let identifier = "prefix_YOUR_PROJECT_suffix";
            let upper = identifier.to_ascii_uppercase();
            assert!(
                PLACEHOLDER_PATTERNS
                    .iter()
                    .any(|p| upper.contains(p) || upper == *p),
                "should detect YOUR_PROJECT as substring"
            );
        }

        #[test]
        fn placeholder_real_path_not_detected() {
            let real_paths = [
                "/data/projects/backend",
                "my-cool-project",
                "data-projects-api",
            ];
            for path in real_paths {
                let upper = path.to_ascii_uppercase();
                assert!(
                    !PLACEHOLDER_PATTERNS
                        .iter()
                        .any(|p| upper.contains(p) || upper == *p),
                    "real path '{path}' should not be flagged as placeholder"
                );
            }
        }

        // -------------------------------------------------------------------
        // agent placeholder detection
        // -------------------------------------------------------------------

        #[test]
        fn agent_placeholder_your_agent_detected() {
            for pattern in AGENT_PLACEHOLDER_PATTERNS {
                let upper = pattern.to_ascii_uppercase();
                assert!(
                    upper.contains(pattern) || upper == *pattern,
                    "pattern {pattern} should match itself"
                );
            }
        }

        #[test]
        fn agent_placeholder_case_insensitive() {
            let name = "your_agent";
            let upper = name.to_ascii_uppercase();
            assert!(
                AGENT_PLACEHOLDER_PATTERNS
                    .iter()
                    .any(|p| upper.contains(p) || upper == *p),
                "your_agent should match YOUR_AGENT pattern"
            );
        }

        #[test]
        fn agent_placeholder_real_names_not_detected() {
            let real_names = ["BlueLake", "GreenCastle", "RedFox"];
            for name in real_names {
                let upper = name.to_ascii_uppercase();
                assert!(
                    !AGENT_PLACEHOLDER_PATTERNS
                        .iter()
                        .any(|p| upper.contains(p) || upper == *p),
                    "real name '{name}' should not be flagged as placeholder"
                );
            }
        }

        #[test]
        fn agent_placeholder_patterns_match_python() {
            // Python's exact 7 patterns
            let expected = [
                "YOUR_AGENT",
                "YOUR_AGENT_NAME",
                "AGENT_NAME",
                "PLACEHOLDER",
                "<AGENT>",
                "{AGENT}",
                "$AGENT",
            ];
            assert_eq!(AGENT_PLACEHOLDER_PATTERNS.len(), expected.len());
            for (i, p) in AGENT_PLACEHOLDER_PATTERNS.iter().enumerate() {
                assert_eq!(*p, expected[i], "pattern at index {i} differs");
            }
        }
    }
}

/// Returns true when two glob/literal patterns overlap under Agent Mail semantics.
#[must_use]
pub fn patterns_overlap(left: &str, right: &str) -> bool {
    let left = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(left);
    let right = mcp_agent_mail_core::pattern_overlap::CompiledPattern::cached(right);
    left.overlaps(&right)
}

/// Tool cluster identifiers for grouping and RBAC
pub mod clusters {
    pub const INFRASTRUCTURE: &str = "infrastructure";
    pub const IDENTITY: &str = "identity";
    pub const MESSAGING: &str = "messaging";
    pub const CONTACT: &str = "contact";
    pub const FILE_RESERVATIONS: &str = "file_reservations";
    pub const SEARCH: &str = "search";
    pub const WORKFLOW_MACROS: &str = "workflow_macros";
    pub const PRODUCT_BUS: &str = "product_bus";
    pub const BUILD_SLOTS: &str = "build_slots";
}

/// Tool name → cluster mapping used for filtering and tooling metadata.
pub const TOOL_CLUSTER_MAP: &[(&str, &str)] = &[
    // Infrastructure
    ("health_check", clusters::INFRASTRUCTURE),
    ("ensure_project", clusters::INFRASTRUCTURE),
    ("install_precommit_guard", clusters::INFRASTRUCTURE),
    ("uninstall_precommit_guard", clusters::INFRASTRUCTURE),
    // Identity
    ("register_agent", clusters::IDENTITY),
    ("create_agent_identity", clusters::IDENTITY),
    ("whois", clusters::IDENTITY),
    ("resolve_pane_identity", clusters::IDENTITY),
    ("cleanup_pane_identities", clusters::IDENTITY),
    ("list_agents", clusters::IDENTITY),
    // Messaging
    ("send_message", clusters::MESSAGING),
    ("reply_message", clusters::MESSAGING),
    ("fetch_inbox", clusters::MESSAGING),
    ("mark_message_read", clusters::MESSAGING),
    ("acknowledge_message", clusters::MESSAGING),
    // Contact
    ("request_contact", clusters::CONTACT),
    ("respond_contact", clusters::CONTACT),
    ("list_contacts", clusters::CONTACT),
    ("set_contact_policy", clusters::CONTACT),
    // File reservations
    ("file_reservation_paths", clusters::FILE_RESERVATIONS),
    ("release_file_reservations", clusters::FILE_RESERVATIONS),
    ("renew_file_reservations", clusters::FILE_RESERVATIONS),
    (
        "force_release_file_reservation",
        clusters::FILE_RESERVATIONS,
    ),
    // Search
    ("search_messages", clusters::SEARCH),
    ("summarize_thread", clusters::SEARCH),
    // Workflow macros
    ("macro_start_session", clusters::WORKFLOW_MACROS),
    ("macro_prepare_thread", clusters::WORKFLOW_MACROS),
    ("macro_file_reservation_cycle", clusters::WORKFLOW_MACROS),
    ("macro_contact_handshake", clusters::WORKFLOW_MACROS),
    // Product bus
    ("ensure_product", clusters::PRODUCT_BUS),
    ("products_link", clusters::PRODUCT_BUS),
    ("search_messages_product", clusters::PRODUCT_BUS),
    ("fetch_inbox_product", clusters::PRODUCT_BUS),
    ("summarize_thread_product", clusters::PRODUCT_BUS),
    // Build slots
    ("acquire_build_slot", clusters::BUILD_SLOTS),
    ("renew_build_slot", clusters::BUILD_SLOTS),
    ("release_build_slot", clusters::BUILD_SLOTS),
];

#[must_use]
pub fn tool_cluster(tool_name: &str) -> Option<&'static str> {
    TOOL_CLUSTER_MAP
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, cluster)| *cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- tool_cluster tests --

    #[test]
    fn tool_cluster_known_tools() {
        assert_eq!(tool_cluster("health_check"), Some(clusters::INFRASTRUCTURE));
        assert_eq!(tool_cluster("register_agent"), Some(clusters::IDENTITY));
        assert_eq!(
            tool_cluster("resolve_pane_identity"),
            Some(clusters::IDENTITY)
        );
        assert_eq!(
            tool_cluster("cleanup_pane_identities"),
            Some(clusters::IDENTITY)
        );
        assert_eq!(tool_cluster("send_message"), Some(clusters::MESSAGING));
        assert_eq!(tool_cluster("request_contact"), Some(clusters::CONTACT));
        assert_eq!(
            tool_cluster("file_reservation_paths"),
            Some(clusters::FILE_RESERVATIONS)
        );
        assert_eq!(tool_cluster("search_messages"), Some(clusters::SEARCH));
        assert_eq!(
            tool_cluster("macro_start_session"),
            Some(clusters::WORKFLOW_MACROS)
        );
        assert_eq!(tool_cluster("ensure_product"), Some(clusters::PRODUCT_BUS));
        assert_eq!(
            tool_cluster("acquire_build_slot"),
            Some(clusters::BUILD_SLOTS)
        );
    }

    #[test]
    fn tool_cluster_unknown_tool_returns_none() {
        assert_eq!(tool_cluster("nonexistent_tool"), None);
        assert_eq!(tool_cluster(""), None);
        assert_eq!(tool_cluster("HEALTH_CHECK"), None); // case-sensitive
    }

    #[test]
    fn tool_cluster_all_entries_resolve() {
        for (name, cluster) in TOOL_CLUSTER_MAP {
            assert_eq!(
                tool_cluster(name),
                Some(*cluster),
                "tool_cluster({name}) should match TOOL_CLUSTER_MAP"
            );
        }
    }

    // -- patterns_overlap tests --

    #[test]
    fn patterns_overlap_identical() {
        assert!(patterns_overlap("src/*.rs", "src/*.rs"));
    }

    #[test]
    fn patterns_overlap_literal_match() {
        assert!(patterns_overlap("README.md", "README.md"));
    }

    #[test]
    fn patterns_overlap_disjoint() {
        assert!(!patterns_overlap("src/*.rs", "tests/*.py"));
    }

    #[test]
    fn patterns_overlap_glob_subsumes() {
        assert!(patterns_overlap("src/**", "src/main.rs"));
    }

    #[test]
    fn patterns_overlap_star_overlap() {
        assert!(patterns_overlap("*.rs", "lib.rs"));
    }

    #[test]
    fn patterns_overlap_empty_patterns() {
        // An empty pattern normalizes to the root directory, which overlaps with everything
        assert!(patterns_overlap("", "src/main.rs"));
    }

    // -- cluster constants test --

    #[test]
    fn cluster_constants_are_distinct() {
        let all = [
            clusters::INFRASTRUCTURE,
            clusters::IDENTITY,
            clusters::MESSAGING,
            clusters::CONTACT,
            clusters::FILE_RESERVATIONS,
            clusters::SEARCH,
            clusters::WORKFLOW_MACROS,
            clusters::PRODUCT_BUS,
            clusters::BUILD_SLOTS,
        ];
        let unique: std::collections::HashSet<&str> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len(), "all cluster names must be unique");
    }
}
